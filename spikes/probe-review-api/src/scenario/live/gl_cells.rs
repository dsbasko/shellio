//! Клетки строк GitLab в живых столбцах: общая обвязка (ветки и MR клетки, сторож, уборка, итог
//! в `result.json`) и сценарии строк. Ожидания — в отчёте, «Живые клетки GitLab».
//!
//! Оракул живой клетки не видит полей `forge-fake` (`from_draft`, `origin`): записи пробы берутся
//! из следа ревьюера в окне отправки, состояние — из чтения автором заметок MR и ревьюером своих
//! черновиков. Системные заметки MR (назначение ревьюера, новый коммит) в оракул не входят.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{MR_NOT_READY, step_verdict};
use super::{Verdict, gitlab_adapter, guard_errors, live_repo, read_or_fail, record_cleanup};
use crate::fake::{GraphqlKind, graphql_root};
use crate::gitlab::{ApiError, GitLab, MergeSnapshot, RemoteDraft, RemoteNote, RemotePosition};
use crate::journal::{ItemState, Journal, ProjectId};
use crate::live::guard::Guarded;
use crate::live::lifecycle::{Cell, CellNames, LEDGER_FILE, Ledger, Roles, run_cell};
use crate::model::anchor::Side;
use crate::model::{Provider, ReviewSession, ReviewTarget};
use crate::results::ResultFile;
use crate::scenario::Scenario;
use crate::scenario::fake::{Checks, Spot, add_drafts, dump_in, states};
use crate::scrub::Scrubber;
use crate::submit::{Delivery, GitLabPlan, Report, SubmitError, plan_gitlab, report, run_gitlab};
use crate::transport::{Call, Method, Trace, Transport};

/// Ключи записей GitLab в следе, как ключи журнала `forge-fake`.
pub const DRAFT_CREATE: &str = "POST draft_notes";
pub const DRAFT_PUBLISH: &str = "PUT draft_notes/{id}/publish";
pub const BULK_PUBLISH: &str = "POST draft_notes/bulk_publish";
pub const NOTE: &str = "POST notes";

/// Проект журнала живой клетки.
const LIVE_PROJECT: &str = "live-gitlab";
const ACTOR_REVIEWER: &str = "reviewer";
const SUMMARY: &str = "Итог ревью";
/// Черновики `gl-draft-publish`: строка 05 после правки (новая сторона, 4) и добавленная 24a
/// (новая сторона, 24).
const PUBLISH_DRAFTS: [(Spot, &str); 2] = [
    (Spot::Line(Side::New, 4), "первый"),
    (Spot::Line(Side::New, 24), "второй"),
];

/// Последний ответ опроса MR, у версии которого diff ещё без файлов.
pub const DIFF_EMPTY: &str = "в diff версии нет файлов";
/// Причина `not_run` живой клетки, чья запись осталась `unknown`: естественный сбой засчитывается,
/// только если серверная запись наблюдена чтением.
pub const WRITE_UNOBSERVED: &str =
    "естественный сбой записи на живом хосте, серверная запись не наблюдена чтением";

/// Опрос MR: крайний срок и пауза между попытками. Тесты дают нулевую паузу.
#[derive(Clone, Copy, Debug)]
pub struct Poll {
    pub deadline: Duration,
    pub pause: Duration,
}

impl Poll {
    pub const LIVE: Poll = Poll {
        deadline: Duration::from_secs(120),
        pause: Duration::from_secs(1),
    };
}

/// Окружение клетки GitLab: сторожа ролей над общим следом.
pub struct GlCx<'a, T> {
    pub author: &'a Guarded<T>,
    pub reviewer: &'a Guarded<T>,
    pub trace: &'a Trace,
    /// Корень инсталляции для `ReviewTarget.host`.
    pub host_url: String,
    /// Логин ревьюера: аккаунт сессии журнала.
    pub reviewer_login: String,
    pub state_dir: &'a Path,
    pub poll: Poll,
}

impl<T: Transport> GlCx<'_, T> {
    fn review(&self, number: u64) -> ReviewTarget {
        ReviewTarget {
            provider: Provider::GitLab,
            host: self.host_url.clone(),
            repo: self.reviewer.target().repo.clone(),
            number,
        }
    }
}

/// Наблюдённое телом клетки.
#[derive(Debug, Default)]
pub struct Observed {
    pub checks: Checks,
    pub notes: Vec<String>,
    pub before: Value,
    pub after: Value,
    pub journal: Value,
    pub tier: Option<String>,
}

/// Клетка строки GitLab: ветки и MR от автора, тело сценария, уборка в любом случае, итог.
/// `out` — каталог процесса `<прогон>/<сценарий>-<столбец>`: имя прогона — run-id веток.
pub fn run_gl_cell<T: Transport>(
    scenario: Scenario,
    cx: &GlCx<'_, T>,
    out: &Path,
    scrubber: &Scrubber,
    result: &mut ResultFile,
) {
    let verdict = cell_verdict(scenario, cx, out, scrubber, result);
    result.trace = cx
        .trace
        .entries(scrubber)
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    result.calls = cx
        .trace
        .counts(scrubber)
        .into_iter()
        .map(|(key, count)| (key, count as u64))
        .collect();
    let verdict = match verdict {
        Some(verdict) => verdict,
        None if result.trace.is_empty() => Verdict::Error("нет следа вызовов".to_owned()),
        None => return,
    };
    verdict.apply(result, scrubber);
}

/// Итог клетки или `None`, если итог записан проверками тела.
fn cell_verdict<T: Transport>(
    scenario: Scenario,
    cx: &GlCx<'_, T>,
    out: &Path,
    scrubber: &Scrubber,
    result: &mut ResultFile,
) -> Option<Verdict> {
    let run_id = match out
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
    {
        Some(run_id) => run_id,
        None => return Some(Verdict::Error("у каталога процесса нет прогона".to_owned())),
    };
    let names = match CellNames::new(run_id, scenario.name()) {
        Ok(names) => names,
        Err(err) => return Some(Verdict::Error(err)),
    };
    let mut ledger = match Ledger::create_for(&out.join(LEDGER_FILE), cx.author.target()) {
        Ok(ledger) => ledger,
        Err(err) => return Some(Verdict::Error(err)),
    };
    let (author_repo, reviewer_repo) = match (live_repo(cx.author), live_repo(cx.reviewer)) {
        (Ok(author), Ok(reviewer)) => (author, reviewer),
        (Err(err), _) | (_, Err(err)) => return Some(Verdict::Error(err)),
    };
    let roles = Roles {
        author: author_repo.as_ref(),
        reviewer: Some(reviewer_repo.as_ref()),
    };
    let title = format!("probe-review {}", scenario.name());
    let run = run_cell(&roles, &mut ledger, &names, &title, |cell| {
        Ok(match scenario {
            Scenario::GlDraftPublish => gl_draft_publish(cx, cell),
            other => Err(Verdict::Error(format!(
                "сценарий {} в живом столбце GitLab не реализован",
                other.name()
            ))),
        })
    });
    record_cleanup(result, &run.cleanup, scrubber);
    let errors = guard_errors(&[("author", cx.author), ("reviewer", cx.reviewer)]);
    if !errors.is_empty() {
        return Some(Verdict::Error(errors.join("; ")));
    }
    if let Some(err) = &run.open_error {
        return Some(step_verdict(err));
    }
    let observed = match run.outcome {
        Err(text) => return Some(Verdict::Error(text)),
        Ok(Err(verdict)) => return Some(verdict),
        Ok(Ok(observed)) => observed,
    };
    record_observed(&observed, scrubber, result)
}

/// Наблюдённое телом — в `result.json` через очиститель: итог проверок (`None`) или ошибка
/// исполнения, если нет обязательного артефакта.
fn record_observed(
    observed: &Observed,
    scrubber: &Scrubber,
    result: &mut ResultFile,
) -> Option<Verdict> {
    let missing: Vec<&str> = [
        (observed.before.is_null(), "снимок сервера до"),
        (observed.after.is_null(), "снимок сервера после"),
        (observed.journal.is_null(), "журнал после переоткрытия"),
    ]
    .into_iter()
    .filter_map(|(empty, what)| empty.then_some(what))
    .collect();
    result.server_before = Some(scrubber.scrub_json(&observed.before));
    result.server_after = Some(scrubber.scrub_json(&observed.after));
    result.journal = Some(scrubber.scrub_json(&observed.journal));
    if observed.tier.is_some() {
        result.host.tier = observed.tier.clone();
    }
    result.notes.extend(
        observed
            .notes
            .iter()
            .chain(observed.checks.to_json().iter())
            .map(|line| scrubber.scrub_text(line)),
    );
    if !missing.is_empty() {
        return Some(Verdict::Error(format!(
            "нет обязательных артефактов: {}",
            missing.join(", ")
        )));
    }
    let (status, reason) = observed.checks.verdict();
    result.status = status;
    result.reason = scrubber.scrub_text(&reason);
    None
}

/// Снимок MR после асинхронного заполнения `diff_refs`, версии и её diff: ответ без них (адаптер
/// даёт `Unexpected`), с другим head или без файлов в diff версии повторяется до крайнего срока,
/// затем `not_run` с причиной `late`. Другой отказ чтения — по [`read_or_fail`].
pub fn await_snapshot<T: Transport>(
    gitlab: &GitLab<T>,
    poll: &Poll,
    head: Option<&str>,
    late: &str,
) -> Result<MergeSnapshot, Verdict> {
    let start = Instant::now();
    let mut tries = 0u32;
    loop {
        tries += 1;
        let last = match gitlab.merge_request() {
            Ok(snapshot)
                if head.is_some_and(|head| snapshot.revision.head_sha().as_str() != head) =>
            {
                format!("head {}", snapshot.revision.head_sha().as_str())
            }
            Ok(snapshot) if snapshot.files.is_empty() => DIFF_EMPTY.to_owned(),
            Ok(snapshot) => return Ok(snapshot),
            Err(ApiError::Unexpected(reason)) => reason,
            Err(err) => return Err(read_or_fail("снимок MR", &err)),
        };
        if start.elapsed() >= poll.deadline {
            return Err(Verdict::NotRun(format!(
                "{late}: попыток {tries}, последний ответ: {last}"
            )));
        }
        std::thread::sleep(poll.pause);
    }
}

/// Запись в следе: ключ, ID черновика в пути публикации, ID из ответа на создание черновика.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    pub seq: u64,
    pub key: String,
    pub draft: Option<String>,
    pub created: Option<String>,
}

/// Ключ записи GitLab: REST по хвосту пути после `merge_requests/<iid>/`, GraphQL — по имени
/// мутации. Чтения (`GET`, запросы GraphQL) — `None`; неизвестная запись — `МЕТОД путь`, чтобы
/// точное сравнение записей её не пропустило.
pub fn write_key(method: Method, path: &str, body: Option<&Value>) -> Option<String> {
    if !method.is_write() {
        return None;
    }
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    if path.ends_with("/api/graphql") {
        let query = body
            .and_then(|body| body.get("query"))
            .and_then(Value::as_str);
        return match query.and_then(graphql_root) {
            Some((GraphqlKind::Query, _)) => None,
            Some((GraphqlKind::Mutation, name)) => Some(format!("graphql {name}")),
            None => Some("graphql без разбора".to_owned()),
        };
    }
    let tail = path
        .split_once("/merge_requests/")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, tail)| tail);
    let key = match (method, tail) {
        (Method::Post, Some("draft_notes")) => DRAFT_CREATE,
        (Method::Post, Some("draft_notes/bulk_publish")) => BULK_PUBLISH,
        (Method::Put, Some(tail)) if publish_id(tail).is_some() => DRAFT_PUBLISH,
        (Method::Post, Some("notes")) => NOTE,
        _ => return Some(format!("{method} {path}")),
    };
    Some(key.to_owned())
}

fn publish_id(tail: &str) -> Option<&str> {
    tail.strip_prefix("draft_notes/")?
        .strip_suffix("/publish")
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

/// Записи роли в вызовах окна.
pub fn writes_of(calls: &[Call], actor: &str) -> Vec<Write> {
    calls
        .iter()
        .filter(|call| call.actor == actor)
        .filter_map(|call| {
            let body = call.request.body_json();
            let key = write_key(call.request.method, call.request.path(), body.as_ref())?;
            let draft = call
                .request
                .path()
                .split_once("/merge_requests/")
                .and_then(|(_, rest)| rest.split_once('/'))
                .and_then(|(_, tail)| publish_id(tail))
                .map(str::to_owned);
            let created = (key == DRAFT_CREATE)
                .then(|| call.outcome.as_ref().ok())
                .flatten()
                .filter(|response| response.is_success())
                .and_then(|response| response.body_json().ok())
                .and_then(|value| value.get("id").map(id_text));
            Some(Write {
                seq: call.seq,
                key,
                draft,
                created,
            })
        })
        .collect()
}

fn id_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

/// Журнал отправки после переоткрытия.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalFacts {
    pub full_confirmed: bool,
    pub states: String,
    pub remote_ids: bool,
    pub recovered: usize,
}

/// Отказ исполнителя отправки. Отказ хоста на запись исполнитель пишет в элемент (`failed` или
/// `unknown`) и возвращает итог; `Err` дают журнал, отказ до записи и чтение шага 1 — его отказ
/// судится как чтение клетки (неожиданный ответ — MR не готов, как в опросе), остальное — ошибка
/// исполнения.
pub fn submit_verdict(err: &SubmitError) -> Verdict {
    match err {
        // Тот же ответ опрос MR считает неготовым, а не отказом хоста.
        SubmitError::GitLab(ApiError::Unexpected(reason)) => Verdict::NotRun(format!(
            "{MR_NOT_READY}: отправка, шаг 1: снимок MR: {reason}"
        )),
        SubmitError::GitLab(api) => read_or_fail("отправка, шаг 1: снимок MR", api),
        other => Verdict::Error(format!("отправка: {other}")),
    }
}

/// Итог отправки, который оракул записей не судит: элемент `unknown` или `in_flight` —
/// [`WRITE_UNOBSERVED`]; `failed` после ответа 429 на последнюю запись роли — `not_run` «лимит»
/// с ключом записи (не с путём: в пути репозиторий)
/// (запись не принята, повтора нет); `failed` после записи, которая не ушла, — ошибка исполнения.
/// Остальной `failed` — отказ хоста, его судят проверки клетки.
pub fn delivery_verdict(done: &Report, calls: &[Call], actor: &str) -> Option<Verdict> {
    if done.unresolved > 0 {
        let details: Vec<String> = done
            .items
            .iter()
            .filter(|item| matches!(item.state, ItemState::Unknown | ItemState::InFlight))
            .map(|item| {
                format!(
                    "{} {:?}: {}",
                    item.operation.name(),
                    item.state,
                    item.detail.as_deref().unwrap_or("без подробностей")
                )
            })
            .collect();
        return Some(Verdict::NotRun(format!(
            "{WRITE_UNOBSERVED}: {}",
            details.join("; ")
        )));
    }
    if !done
        .items
        .iter()
        .any(|item| item.state == ItemState::Failed)
    {
        return None;
    }
    let (last, key) = calls.iter().rev().find_map(|call| {
        let key = write_key(
            call.request.method,
            call.request.path(),
            call.request.body_json().as_ref(),
        )?;
        (call.actor == actor).then_some((call, key))
    })?;
    match &last.outcome {
        Ok(response) if response.status == 429 => {
            Some(Verdict::NotRun(format!("лимит: код 429 на запись {key}")))
        }
        Err(failure) if !failure.may_have_been_sent() => {
            Some(Verdict::Error(format!("запись {key} не ушла: {failure}")))
        }
        _ => None,
    }
}

/// Оракул `gl-draft-publish` на живом хосте (ожидание — в отчёте).
pub fn draft_publish_checks(
    writes: &[Write],
    drafts_after: &[RemoteDraft],
    notes_after: &[RemoteNote],
    expected: &[(u32, &str)],
    summary: &str,
    journal: &JournalFacts,
) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    let expected_counts: BTreeMap<String, usize> = [
        (DRAFT_CREATE.to_owned(), expected.len()),
        (DRAFT_PUBLISH.to_owned(), expected.len()),
        (NOTE.to_owned(), 1),
    ]
    .into_iter()
    .collect();
    checks.eq("записи ревьюера", counts, expected_counts);
    let created: BTreeSet<&str> = writes
        .iter()
        .filter_map(|write| write.created.as_deref())
        .collect();
    let published: BTreeSet<&str> = writes
        .iter()
        .filter(|write| write.key == DRAFT_PUBLISH)
        .filter_map(|write| write.draft.as_deref())
        .collect();
    checks.eq(
        "публикации по ID из ответов на создание",
        (published.len(), published == created),
        (expected.len(), true),
    );
    let last_publish = writes
        .iter()
        .filter(|write| write.key == DRAFT_PUBLISH)
        .map(|write| write.seq)
        .max();
    let note_seq = writes
        .iter()
        .find(|write| write.key == NOTE)
        .map(|write| write.seq);
    checks.check(
        "сводка после последней публикации",
        matches!((last_publish, note_seq), (Some(p), Some(n)) if n > p),
        format!("публикация {last_publish:?}, сводка {note_seq:?}"),
    );
    checks.eq("черновиков ревьюера после отправки", drafts_after.len(), 0);
    let user_notes: Vec<&RemoteNote> = notes_after.iter().filter(|note| !note.system).collect();
    let positioned: BTreeSet<(Option<u32>, String)> = user_notes
        .iter()
        .filter_map(|note| {
            note.position
                .as_ref()
                .map(|position| (position.new_line, note.body.clone()))
        })
        .collect();
    let wanted: BTreeSet<(Option<u32>, String)> = expected
        .iter()
        .map(|(line, body)| (Some(*line), (*body).to_owned()))
        .collect();
    checks.eq("автор: заметки с позициями черновиков", positioned, wanted);
    checks.eq(
        "автор: заметки без позиции",
        user_notes
            .iter()
            .filter(|note| note.position.is_none())
            .map(|note| note.body.clone())
            .collect::<Vec<_>>(),
        vec![summary.to_owned()],
    );
    checks.check(
        "журнал: все элементы confirmed, итог «полностью»",
        journal.full_confirmed,
        journal.states.clone(),
    );
    checks.check("журнал: у черновиков ID GitLab", journal.remote_ids, "");
    checks.eq("восстановленные элементы", journal.recovered, 0);
    checks
}

fn position_json(position: &RemotePosition) -> Value {
    json!({
        "base_sha": position.base_sha,
        "start_sha": position.start_sha,
        "head_sha": position.head_sha,
        "old_path": position.old_path,
        "new_path": position.new_path,
        "old_line": position.old_line,
        "new_line": position.new_line,
        "line_range": position.line_range,
    })
}

/// Состояние MR: заметки тредов, прочитанные автором, и черновики ревьюера. Заметки читаются
/// через Discussions API, как в сверке: Notes API не документирует `position` у заметок MR и
/// отсылает за комментариями к строкам к тредам.
fn mr_state<T: Transport>(
    author: &GitLab<T>,
    reviewer: &GitLab<T>,
) -> Result<(Vec<RemoteNote>, Vec<RemoteDraft>, Value), Verdict> {
    let discussions = author
        .discussions()
        .map_err(|err| read_or_fail("треды MR автором", &err))?;
    let drafts = reviewer
        .drafts()
        .map_err(|err| read_or_fail("черновики ревьюера", &err))?;
    let value = json!({
        "notes": discussions.iter().flat_map(|discussion| discussion.notes.iter().map(|note| json!({
            "discussion": discussion.id.as_str(),
            "id": note.id.as_str(),
            "author": note.author,
            "body": note.body,
            "system": note.system,
            "position": note.position.as_ref().map(position_json),
        }))).collect::<Vec<_>>(),
        "drafts": drafts.iter().map(|draft| json!({
            "id": draft.id.as_str(),
            "note": draft.note,
            "line_code": draft.line_code,
            "position": draft.position.as_ref().map(position_json),
        })).collect::<Vec<_>>(),
    });
    let notes = discussions
        .into_iter()
        .flat_map(|discussion| discussion.notes)
        .collect();
    Ok((notes, drafts, value))
}

fn stand(what: &str, err: &dyn std::fmt::Display) -> Verdict {
    Verdict::Error(format!("{what}: {err}"))
}

fn gl_draft_publish<T: Transport>(cx: &GlCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = gitlab_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = gitlab_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_snapshot(&reviewer, &cx.poll, None, MR_NOT_READY)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| read_or_fail("возможности ревьюера", &err))?;
    let (_, _, before) = mr_state(&author, &reviewer)?;
    let path = cx.state_dir.join("journal-gl-draft-publish.db");
    let project = ProjectId::new(LIVE_PROJECT);
    let start = cx.trace.len();
    let submission = {
        let journal = Journal::open(&path).map_err(|err| stand("журнал", &err))?;
        let scope = journal.scope(&project);
        let session = ReviewSession::new(
            &cx.reviewer_login,
            review.clone(),
            snapshot.revision.clone(),
        );
        let id = scope
            .create_session(&session)
            .map_err(|err| stand("сессия", &err))?;
        add_drafts(&scope, id, &snapshot.files, &PUBLISH_DRAFTS)
            .map_err(|err| stand("черновики", &err))?;
        let plan = GitLabPlan {
            summary: SUMMARY.to_owned(),
            ..GitLabPlan::default()
        };
        let submission = plan_gitlab(&scope, id, &plan).map_err(|err| stand("план", &err))?;
        let sent = run_gitlab(&scope, submission.id, &reviewer, &caps)
            .map_err(|err| submit_verdict(&err))?;
        (submission.id, sent)
    };
    let (submission, sent) = submission;
    let calls = cx.trace.calls();
    let window = calls.get(start..).unwrap_or_default();
    if let Some(verdict) = delivery_verdict(&sent, window, ACTOR_REVIEWER) {
        return Err(verdict);
    }
    let writes = writes_of(window, ACTOR_REVIEWER);
    let (notes, drafts, after) = mr_state(&author, &reviewer)?;
    let journal = Journal::open(&path).map_err(|err| stand("журнал после переоткрытия", &err))?;
    let scope = journal.scope(&project);
    let done = report(&scope, submission).map_err(|err| stand("итог отправки", &err))?;
    let session = scope
        .submission(submission)
        .map_err(|err| stand("отправка в журнале", &err))?
        .session;
    let facts = JournalFacts {
        full_confirmed: done.delivery == Delivery::Full
            && done
                .items
                .iter()
                .all(|item| item.state == ItemState::Confirmed),
        states: format!(
            "{:?}",
            states(&scope, submission).map_err(|err| stand("состояния", &err))?
        ),
        remote_ids: scope
            .drafts(session)
            .map_err(|err| stand("черновики журнала", &err))?
            .iter()
            .all(|draft| draft.remote_id.is_some()),
        recovered: journal.recovered().len(),
    };
    let expected: Vec<(u32, &str)> = PUBLISH_DRAFTS
        .iter()
        .filter_map(|(spot, body)| match spot {
            Spot::Line(Side::New, line) => Some((*line, *body)),
            _ => None,
        })
        .collect();
    let checks = draft_publish_checks(&writes, &drafts, &notes, &expected, SUMMARY, &facts);
    Ok(Observed {
        checks,
        notes: vec![format!(
            "MR клетки: версия {}",
            snapshot.version_id.as_str()
        )],
        before,
        after,
        journal: dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?,
        tier: caps.plan,
    })
}

#[cfg(test)]
mod tests;
