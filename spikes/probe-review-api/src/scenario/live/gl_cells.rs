//! Клетки строк GitLab в живых столбцах: общая обвязка (ветки и MR клетки, сторож, уборка, итог
//! в `result.json`) и сценарии строк. Ожидания — в отчёте, «Живые клетки GitLab».
//!
//! Оракул живой клетки не видит полей `forge-fake` (`from_draft`, `origin`): записи пробы берутся
//! из следа ревьюера в окне отправки, состояние — из чтения автором заметок MR и ревьюером своих
//! черновиков. Системные заметки MR (назначение ревьюера, новый коммит) в оракул не входят.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{INJECTION_UNAVAILABLE, MR_NOT_READY, classify_read, mentions_tier, step_verdict};
use super::{Verdict, gitlab_adapter, guard_errors, live_repo, read_or_fail, record_cleanup};
use crate::fake::{GraphqlKind, graphql_root};
use crate::gitlab::{
    ApiError, Capabilities, GitLab, MergeSnapshot, RemoteDraft, RemoteNote, RemotePosition,
    ReviewerState, capabilities,
};
use crate::journal::{ItemState, Journal, Operation, ProjectId};
use crate::live::guard::Guarded;
use crate::live::lifecycle::{Cell, CellNames, LEDGER_FILE, Ledger, Roles, run_cell};
use crate::model::anchor::Side;
use crate::model::{DraftComment, DraftId, Provider, ReviewSession, ReviewTarget};
use crate::results::{Column, ResultFile};
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
pub const REQUEST_CHANGES: &str = "graphql mergeRequestRequestChanges";

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

/// Черновик второго клиента `gl-foreign-draft`: текст и новая строка 24 (добавленная 24a).
pub const FOREIGN_NOTE: &str = "черновик второго клиента";
pub const FOREIGN_LINE: u32 = 24;
/// Свой черновик `gl-foreign-draft`: новая строка 4 («line 05 changed»).
const OWN_DRAFTS: [(Spot, &str); 1] = [(Spot::Line(Side::New, 4), "свой")];
/// Роль следа второго клиента: не `reviewer`, иначе его запись вошла бы в записи пробы.
pub const ACTOR_SECOND: &str = "second-client";
/// Очищенный след второго клиента в каталоге процесса сценария.
pub const SECOND_FILE: &str = "second-client.json";

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
    /// Второй клиент ревьюера (`gl-foreign-draft`).
    pub second: &'a dyn SecondClient,
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
    /// Итог по отказу хоста вместо итога проверок; артефакты пишутся так же.
    pub verdict: Option<Verdict>,
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
            Scenario::GlForeignDraft => gl_foreign_draft(cx, cell),
            Scenario::GlRequestChanges => gl_request_changes(cx, cell),
            Scenario::GlAnchors => gl_anchors(cx, cell),
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
    if let Some(verdict) = &observed.verdict {
        return Some(verdict.clone());
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

/// Отправка исполнителем в журнале клетки: записи ревьюера в окне отправки, итог журнала после
/// переоткрытия и его дамп. Итог, который оракул записей не судит, — по [`delivery_verdict`].
struct Submitted {
    /// Итог отправки, который оракул записей не судит ([`delivery_verdict`]).
    delivery: Option<Verdict>,
    /// Итог журнала после переоткрытия.
    done: Report,
    /// Вызовы окна отправки.
    window: Vec<Call>,
    writes: Vec<Write>,
    facts: JournalFacts,
    /// Удалённые ID черновиков и элементов журнала.
    remote_ids: BTreeSet<String>,
    journal: Value,
}

struct Sending<'s> {
    review: &'s ReviewTarget,
    snapshot: &'s MergeSnapshot,
    caps: &'s Capabilities,
    journal: &'s str,
    drafts: &'s [(Spot, &'s str)],
    plan: GitLabPlan,
}

fn submit_drafts<T: Transport>(
    cx: &GlCx<'_, T>,
    reviewer: &GitLab<&Guarded<T>>,
    sending: Sending<'_>,
) -> Result<Submitted, Verdict> {
    let path = cx.state_dir.join(sending.journal);
    let project = ProjectId::new(LIVE_PROJECT);
    let start = cx.trace.len();
    let (submission, sent) = {
        let journal = Journal::open(&path).map_err(|err| stand("журнал", &err))?;
        let scope = journal.scope(&project);
        let session = ReviewSession::new(
            &cx.reviewer_login,
            sending.review.clone(),
            sending.snapshot.revision.clone(),
        );
        let id = scope
            .create_session(&session)
            .map_err(|err| stand("сессия", &err))?;
        add_drafts(&scope, id, &sending.snapshot.files, sending.drafts)
            .map_err(|err| stand("черновики", &err))?;
        let submission =
            plan_gitlab(&scope, id, &sending.plan).map_err(|err| stand("план", &err))?;
        let sent = run_gitlab(&scope, submission.id, reviewer, sending.caps)
            .map_err(|err| submit_verdict(&err))?;
        (submission.id, sent)
    };
    let calls = cx.trace.calls();
    let window = calls.get(start..).unwrap_or_default();
    let delivery = delivery_verdict(&sent, window, ACTOR_REVIEWER);
    let writes = writes_of(window, ACTOR_REVIEWER);
    let window = window.to_vec();
    let journal = Journal::open(&path).map_err(|err| stand("журнал после переоткрытия", &err))?;
    let scope = journal.scope(&project);
    let done = report(&scope, submission).map_err(|err| stand("итог отправки", &err))?;
    let session = scope
        .submission(submission)
        .map_err(|err| stand("отправка в журнале", &err))?
        .session;
    let drafts = scope
        .drafts(session)
        .map_err(|err| stand("черновики журнала", &err))?;
    let remote_ids = drafts
        .iter()
        .filter_map(|draft| draft.remote_id.as_ref())
        .chain(done.items.iter().filter_map(|item| item.remote_id.as_ref()))
        .map(|id| id.as_str().to_owned())
        .collect();
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
        remote_ids: drafts.iter().all(|draft| draft.remote_id.is_some()),
        recovered: journal.recovered().len(),
    };
    let journal_dump = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
    Ok(Submitted {
        delivery,
        done,
        window,
        writes,
        facts,
        remote_ids,
        journal: journal_dump,
    })
}

/// Отправка, итог которой судят проверки клетки: итог, который оракул записей не судит, — сразу.
fn delivered(sent: Submitted) -> Result<Submitted, Verdict> {
    match sent.delivery {
        Some(verdict) => Err(verdict),
        None => Ok(sent),
    }
}

fn new_lines<'s>(drafts: &[(Spot, &'s str)]) -> Vec<(u32, &'s str)> {
    drafts
        .iter()
        .filter_map(|(spot, body)| match spot {
            Spot::Line(Side::New, line) => Some((*line, *body)),
            _ => None,
        })
        .collect()
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
    let sent = delivered(submit_drafts(
        cx,
        &reviewer,
        Sending {
            review: &review,
            snapshot: &snapshot,
            caps: &caps,
            journal: "journal-gl-draft-publish.db",
            drafts: &PUBLISH_DRAFTS,
            plan: GitLabPlan {
                summary: SUMMARY.to_owned(),
                ..GitLabPlan::default()
            },
        },
    )?)?;
    let (notes, drafts, after) = mr_state(&author, &reviewer)?;
    let expected = new_lines(&PUBLISH_DRAFTS);
    let checks = draft_publish_checks(
        &sent.writes,
        &drafts,
        &notes,
        &expected,
        SUMMARY,
        &sent.facts,
    );
    Ok(Observed {
        checks,
        notes: vec![format!(
            "MR клетки: версия {}",
            snapshot.version_id.as_str()
        )],
        before,
        after,
        journal: sent.journal,
        tier: caps.plan,
        verdict: None,
    })
}

/// Второй клиент ревьюера: создаёт черновик на новой строке MR и отдаёт его ID. Отказ хоста —
/// `not_run` «второй клиент не создал черновик», отказ транспорта, 5xx и сторожа — ошибка
/// исполнения.
pub trait SecondClient {
    fn create_draft(&self, iid: u64, line: u32) -> Result<String, Verdict>;
}

/// Второй клиент отдельным процессом `probe-review --second-client`: хост и токен ревьюера он
/// берёт из унаследованного окружения `run.sh`, токена в аргументах нет.
pub struct ProcessClient {
    pub exe: PathBuf,
    pub column: Column,
    /// Каталог процесса сценария: туда второй клиент пишет свой очищенный след.
    pub out: PathBuf,
}

impl SecondClient for ProcessClient {
    fn create_draft(&self, iid: u64, line: u32) -> Result<String, Verdict> {
        let output = Command::new(&self.exe)
            .args(["--second-client", "gl-create-draft", "--column"])
            .arg(self.column.name())
            .arg("--mr")
            .arg(iid.to_string())
            .arg("--line")
            .arg(line.to_string())
            .arg("--out")
            .arg(&self.out)
            .stdin(Stdio::null())
            .output()
            .map_err(|err| Verdict::Error(format!("второй клиент не запущен: {}", err.kind())))?;
        second_reply(output.status.code(), &output.stdout, &output.stderr)
    }
}

/// Ответ процесса второго клиента — строка JSON в stdout: код 0 с `id` или `not_run`, ошибка
/// исполнения — `error`. Другая форма — ошибка исполнения.
pub fn second_reply(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Result<String, Verdict> {
    let value: Option<Value> = serde_json::from_slice(stdout).ok();
    let text = |key: &str| {
        value
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match (code, text("id"), text("not_run"), text("error")) {
        (Some(0), Some(id), None, None) if !id.is_empty() => Ok(id),
        (Some(0), None, Some(reason), None) => Err(Verdict::NotRun(reason)),
        (Some(code), None, None, Some(err)) if code != 0 => {
            Err(Verdict::Error(format!("второй клиент: {err}")))
        }
        _ => Err(Verdict::Error(format!(
            "второй клиент: код {code:?}, ответ «{}», stderr «{}»",
            String::from_utf8_lossy(stdout).trim(),
            String::from_utf8_lossy(stderr).trim()
        ))),
    }
}

/// Черновик второго клиента: свой снимок MR, позиция на новой строке `line` из своей ревизии,
/// запись через своего сторожа. Отказ его сторожа — ошибка исполнения.
pub fn foreign_draft<T: Transport>(
    guard: &Guarded<T>,
    review: &ReviewTarget,
    login: &str,
    line: u32,
) -> Result<String, Verdict> {
    let refused = |what: &str, err: &ApiError| match classify_read(err) {
        Ok(observed) => Verdict::NotRun(format!(
            "второй клиент не создал черновик: {what}: {observed}"
        )),
        Err(stand) => Verdict::Error(format!("второй клиент: {what}: {stand}")),
    };
    let created = (|| {
        let gitlab = gitlab_adapter(guard, review).map_err(|err| stand("второй клиент", &err))?;
        let snapshot = gitlab
            .merge_request()
            .map_err(|err| refused("снимок MR", &err))?;
        let file = snapshot.files.first().ok_or_else(|| {
            Verdict::NotRun(format!("второй клиент не создал черновик: {DIFF_EMPTY}"))
        })?;
        let session = ReviewSession::new(login, review.clone(), snapshot.revision.clone());
        let placement = Spot::Line(Side::New, line)
            .placement(file)
            .map_err(|err| stand("второй клиент: якорь", &err))?;
        let draft = DraftComment::new(DraftId(0), &session, placement, FOREIGN_NOTE);
        gitlab
            .create_draft(&session, &draft, false)
            .map(|(known, _)| known.id().as_str().to_owned())
            .map_err(|err| refused("черновик", &err))
    })();
    let errors = guard_errors(&[(ACTOR_SECOND, guard)]);
    if !errors.is_empty() {
        return Err(Verdict::Error(errors.join("; ")));
    }
    if let Some(err) = guard.refusal() {
        return Err(Verdict::Error(format!("второй клиент: сторож: {err}")));
    }
    created
}

/// Оракул `gl-foreign-draft` на живом хосте (ожидание — в отчёте).
pub fn foreign_draft_checks(
    writes: &[Write],
    foreign: &str,
    drafts_after: &[RemoteDraft],
    notes_after: &[RemoteNote],
    own: (u32, &str),
    journal: &JournalFacts,
    journal_ids: &BTreeSet<String>,
) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера",
        counts,
        [(DRAFT_CREATE.to_owned(), 1), (DRAFT_PUBLISH.to_owned(), 1)]
            .into_iter()
            .collect(),
    );
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
        "публикации по ID из ответа на создание",
        (published.len(), published == created),
        (1, true),
    );
    checks.check(
        "ID второго клиента не опубликован",
        !published.contains(foreign),
        format!("опубликованы {published:?}, второй клиент {foreign}"),
    );
    checks.eq(
        "черновики ревьюера после отправки",
        drafts_after
            .iter()
            .map(|draft| (draft.id.as_str().to_owned(), draft.note.clone()))
            .collect::<Vec<_>>(),
        vec![(foreign.to_owned(), FOREIGN_NOTE.to_owned())],
    );
    let user_notes: Vec<&RemoteNote> = notes_after.iter().filter(|note| !note.system).collect();
    checks.eq(
        "автор: заметки",
        user_notes
            .iter()
            .map(|note| {
                (
                    note.position
                        .as_ref()
                        .and_then(|position| position.new_line),
                    note.body.clone(),
                )
            })
            .collect::<Vec<_>>(),
        vec![(Some(own.0), own.1.to_owned())],
    );
    checks.check(
        "автор не видит текста чужого черновика",
        !user_notes.iter().any(|note| note.body == FOREIGN_NOTE),
        "",
    );
    checks.check(
        "журнал: все элементы confirmed, итог «полностью»",
        journal.full_confirmed,
        journal.states.clone(),
    );
    checks.check("журнал: у черновиков ID GitLab", journal.remote_ids, "");
    checks.check(
        "ID второго клиента нет в журнале",
        !journal_ids.contains(foreign),
        format!("{journal_ids:?}"),
    );
    checks.eq("восстановленные элементы", journal.recovered, 0);
    checks
}

fn gl_foreign_draft<T: Transport>(cx: &GlCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = gitlab_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = gitlab_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_snapshot(&reviewer, &cx.poll, None, MR_NOT_READY)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| read_or_fail("возможности ревьюера", &err))?;
    let foreign = cx.second.create_draft(cell.number, FOREIGN_LINE)?;
    let (_, _, before) = mr_state(&author, &reviewer)?;
    let sent = delivered(submit_drafts(
        cx,
        &reviewer,
        Sending {
            review: &review,
            snapshot: &snapshot,
            caps: &caps,
            journal: "journal-gl-foreign-draft.db",
            drafts: &OWN_DRAFTS,
            plan: GitLabPlan::default(),
        },
    )?)?;
    let (notes, drafts, after) = mr_state(&author, &reviewer)?;
    let own = new_lines(&OWN_DRAFTS)
        .first()
        .copied()
        .ok_or_else(|| Verdict::Error("свой черновик не на новой стороне".to_owned()))?;
    let checks = foreign_draft_checks(
        &sent.writes,
        &foreign,
        &drafts,
        &notes,
        own,
        &sent.facts,
        &sent.remote_ids,
    );
    Ok(Observed {
        checks,
        notes: vec![
            format!("MR клетки: версия {}", snapshot.version_id.as_str()),
            format!("второй клиент: черновик {foreign}, след — {SECOND_FILE}"),
        ],
        before,
        after,
        journal: sent.journal,
        tier: caps.plan,
        verdict: None,
    })
}

/// Итог `gl-request-changes` по элементу отправки: отказ при мутации, которой нет в схеме, или
/// с признаком тарифа — `absent`; затем итог, который оракул записей не судит (`delivery`);
/// другой отказ — `fail` с текстом; `confirmed` судят проверки (`None`).
pub fn request_changes_outcome(
    caps: &Capabilities,
    done: &Report,
    delivery: Option<&Verdict>,
) -> Option<Verdict> {
    let Some(item) = done
        .items
        .iter()
        .find(|item| item.operation == Operation::GitLabRequestChanges)
    else {
        return Some(Verdict::Error(
            "в отправке нет элемента gitlab.request_changes".to_owned(),
        ));
    };
    let detail = item.detail.as_deref().unwrap_or("без подробностей");
    if matches!(item.state, ItemState::Failed | ItemState::Unknown) {
        let schema = ApiError::Absent(capabilities::REQUEST_CHANGES).to_string();
        if !caps.request_changes() || detail == schema {
            return Some(Verdict::Absent(format!(
                "{}: нет в схеме: {detail}",
                capabilities::REQUEST_CHANGES
            )));
        }
        if mentions_tier(detail) {
            return Some(Verdict::Absent(format!(
                "{}: признак тарифа: {detail}",
                capabilities::REQUEST_CHANGES
            )));
        }
    }
    if let Some(verdict) = delivery {
        return Some(verdict.clone());
    }
    match item.state {
        ItemState::Confirmed => None,
        ItemState::Failed => Some(Verdict::Fail(format!(
            "{}: {detail}",
            capabilities::REQUEST_CHANGES
        ))),
        other => Some(Verdict::Error(format!(
            "элемент gitlab.request_changes в {other:?}"
        ))),
    }
}

/// Оракул `gl-request-changes` на живом хосте (ожидание — в отчёте).
pub fn request_changes_checks(
    writes: &[Write],
    schema: bool,
    state_after: Option<ReviewerState>,
    journal: &JournalFacts,
) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    let expected: BTreeMap<String, usize> = schema
        .then(|| (REQUEST_CHANGES.to_owned(), 1))
        .into_iter()
        .collect();
    checks.eq("записи ревьюера", counts, expected);
    checks.eq(
        "состояние ревьюера по чтению автора",
        state_after.map(ReviewerState::graphql),
        Some(ReviewerState::RequestedChanges.graphql()),
    );
    checks.check(
        "журнал: все элементы confirmed, итог «полностью»",
        journal.full_confirmed,
        journal.states.clone(),
    );
    checks.eq("восстановленные элементы", journal.recovered, 0);
    checks
}

fn with_reviewer_state(mut value: Value, state: Option<ReviewerState>) -> Value {
    value["reviewer_state"] = json!(state.map(ReviewerState::graphql));
    value
}

/// Наблюдение `gl-request-changes`: тот же вызов токеном автора (не ревьюер MR). Итог клетки не
/// меняет; отказ транспорта и 5xx — ошибка исполнения, как у чтений.
fn author_request_changes<T: Transport>(author: &GitLab<T>) -> Result<String, Verdict> {
    let caps = author
        .capabilities()
        .map_err(|err| read_or_fail("возможности автора", &err))?;
    let outcome = match author.request_changes(&caps) {
        Ok(state) => format!("принят, состояние {}", state.graphql()),
        Err(err) => match classify_read(&err) {
            Ok(text) => format!("отказ: {text}"),
            Err(stand) => {
                return Err(Verdict::Error(format!(
                    "наблюдение токеном автора: {stand}"
                )));
            }
        },
    };
    Ok(format!(
        "наблюдение: {} токеном автора (не ревьюер MR): {outcome}",
        capabilities::REQUEST_CHANGES
    ))
}

fn gl_request_changes<T: Transport>(
    cx: &GlCx<'_, T>,
    cell: &mut Cell,
) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = gitlab_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = gitlab_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_snapshot(&reviewer, &cx.poll, None, MR_NOT_READY)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| read_or_fail("возможности ревьюера", &err))?;
    let state = |what: &str| {
        author
            .reviewer_state(&cx.reviewer_login)
            .map_err(|err| read_or_fail(what, &err))
    };
    let (_, _, before) = mr_state(&author, &reviewer)?;
    let before = with_reviewer_state(before, state("состояние ревьюера до")?);
    let sent = submit_drafts(
        cx,
        &reviewer,
        Sending {
            review: &review,
            snapshot: &snapshot,
            caps: &caps,
            journal: "journal-gl-request-changes.db",
            drafts: &[],
            plan: GitLabPlan {
                request_changes: true,
                ..GitLabPlan::default()
            },
        },
    )?;
    let (_, _, after) = mr_state(&author, &reviewer)?;
    let state_after = state("состояние ревьюера после")?;
    let after = with_reviewer_state(after, state_after);
    let verdict = request_changes_outcome(&caps, &sent.done, sent.delivery.as_ref());
    let checks = request_changes_checks(
        &sent.writes,
        caps.request_changes(),
        state_after,
        &sent.facts,
    );
    let mut notes = vec![format!(
        "MR клетки: версия {}",
        snapshot.version_id.as_str()
    )];
    if caps.request_changes() {
        notes.push(format!(
            "ветвь «мутации нет в схеме»: not_run «{INJECTION_UNAVAILABLE}»"
        ));
    }
    notes.push(format!(
        "ветвь «ошибка верхнего уровня при HTTP 200»: not_run «{INJECTION_UNAVAILABLE}»"
    ));
    notes.push(author_request_changes(&author)?);
    Ok(Observed {
        checks,
        notes,
        before,
        after,
        journal: sent.journal,
        tier: caps.plan,
        verdict,
    })
}

/// Черновики `gl-anchors` и ожидаемые `old_line`, `new_line` их заметок по фикстуре клетки: у
/// диапазона — строки конца.
const ANCHOR_DRAFTS: [(Spot, &str, Option<u32>, Option<u32>); 5] = [
    (Spot::Line(Side::Old, 3), "удалённая 03", Some(3), None),
    (Spot::Line(Side::New, 24), "добавленная 24a", None, Some(24)),
    (
        Spot::Line(Side::New, 1),
        "неизменённая 01",
        Some(1),
        Some(1),
    ),
    (
        Spot::Range(Side::Old, 3, Side::New, 3),
        "диапазон 03–04",
        Some(4),
        Some(3),
    ),
    (
        Spot::Range(Side::New, 23, Side::New, 24),
        "диапазон 24–24a",
        None,
        Some(24),
    ),
];

/// Позиции тел `POST …/draft_notes` ревьюера в вызовах окна по тексту черновика.
pub fn created_positions(calls: &[Call]) -> Vec<(String, Value)> {
    calls
        .iter()
        .filter(|call| call.actor == ACTOR_REVIEWER)
        .filter_map(|call| {
            let body = call.request.body_json()?;
            let key = write_key(call.request.method, call.request.path(), Some(&body))?;
            (key == DRAFT_CREATE).then(|| {
                (
                    body.get("note")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    body.get("position").cloned().unwrap_or(Value::Null),
                )
            })
        })
        .collect()
}

/// Оракул `gl-anchors` на живом хосте (ожидание — в отчёте).
pub fn anchors_checks(
    writes: &[Write],
    created: &[(String, Value)],
    triple: [&str; 3],
    drafts_after: &[RemoteDraft],
    notes_after: &[RemoteNote],
    journal: &JournalFacts,
) -> Checks {
    let mut checks = Checks::default();
    let count = ANCHOR_DRAFTS.len();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера",
        counts,
        [
            (DRAFT_CREATE.to_owned(), count),
            (DRAFT_PUBLISH.to_owned(), count),
        ]
        .into_iter()
        .collect(),
    );
    let ids: BTreeSet<&str> = writes
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
        (published.len(), published == ids),
        (count, true),
    );
    let field = |position: &Value, name: &str| {
        position
            .get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    let sent: Vec<_> = created
        .iter()
        .map(|(_, position)| {
            (
                field(position, "position_type"),
                [
                    field(position, "base_sha"),
                    field(position, "start_sha"),
                    field(position, "head_sha"),
                ],
            )
        })
        .collect();
    let wanted = (
        Some("text".to_owned()),
        triple.map(|sha| Some(sha.to_owned())),
    );
    checks.eq(
        "позиции созданий: тип и тройка diff_refs",
        sent,
        vec![wanted; count],
    );
    checks.eq("черновиков ревьюера после отправки", drafts_after.len(), 0);
    let stored: BTreeSet<(String, Option<u32>, Option<u32>, String)> = notes_after
        .iter()
        .filter(|note| !note.system)
        .filter_map(|note| {
            note.position.as_ref().map(|position| {
                (
                    note.body.clone(),
                    position.old_line,
                    position.new_line,
                    format!(
                        "{}/{}/{}",
                        position.base_sha, position.start_sha, position.head_sha
                    ),
                )
            })
        })
        .collect();
    let triple = triple.join("/");
    let expected: BTreeSet<(String, Option<u32>, Option<u32>, String)> = ANCHOR_DRAFTS
        .iter()
        .map(|(_, body, old, new)| ((*body).to_owned(), *old, *new, triple.clone()))
        .collect();
    checks.eq("автор: заметки с позициями", stored, expected);
    checks.check(
        "журнал: все элементы confirmed, итог «полностью»",
        journal.full_confirmed,
        journal.states.clone(),
    );
    checks.check("журнал: у черновиков ID GitLab", journal.remote_ids, "");
    checks.eq("восстановленные элементы", journal.recovered, 0);
    checks
}

/// Концы `line_range`: `line_code` и `type` начала и конца.
fn range_ends(range: Option<&Value>) -> Option<Value> {
    let range = range.filter(|range| !range.is_null())?;
    let end = |name: &str| {
        json!({
            "line_code": range.pointer(&format!("/{name}/line_code")),
            "type": range.pointer(&format!("/{name}/type")),
        })
    };
    Some(json!({"start": end("start"), "end": end("end")}))
}

/// Наблюдения `gl-anchors`: концы `line_range` каждого диапазона, отправленные адаптером, против
/// возвращённых в позиции заметки автора. Итог клетки не меняют.
pub fn range_observations(created: &[(String, Value)], notes_after: &[RemoteNote]) -> Vec<String> {
    created
        .iter()
        .filter_map(|(body, position)| {
            let sent = range_ends(position.get("line_range"))?;
            let returned = notes_after
                .iter()
                .find(|note| !note.system && &note.body == body)
                .and_then(|note| note.position.as_ref())
                .and_then(|position| range_ends(position.line_range.as_ref()));
            let line = match returned {
                None => format!(
                    "наблюдение line_range «{body}»: отправлено {sent}, GitLab не вернул line_range"
                ),
                Some(returned) => {
                    let same = |what: &str| {
                        ["start", "end"]
                            .iter()
                            .all(|end| sent[end][what] == returned[end][what])
                    };
                    let verdict = |ok: bool| {
                        if ok {
                            "совпадает"
                        } else {
                            "расходится"
                        }
                    };
                    format!(
                        "наблюдение line_range «{body}»: отправлено {sent}, GitLab вернул \
                         {returned}; line_code {}, type {}",
                        verdict(same("line_code")),
                        verdict(same("type"))
                    )
                }
            };
            Some(line)
        })
        .collect()
}

fn gl_anchors<T: Transport>(cx: &GlCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = gitlab_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = gitlab_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_snapshot(&reviewer, &cx.poll, None, MR_NOT_READY)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| read_or_fail("возможности ревьюера", &err))?;
    let (_, _, before) = mr_state(&author, &reviewer)?;
    let drafts: Vec<(Spot, &str)> = ANCHOR_DRAFTS
        .iter()
        .map(|(spot, body, _, _)| (*spot, *body))
        .collect();
    let sent = delivered(submit_drafts(
        cx,
        &reviewer,
        Sending {
            review: &review,
            snapshot: &snapshot,
            caps: &caps,
            journal: "journal-gl-anchors.db",
            drafts: &drafts,
            plan: GitLabPlan::default(),
        },
    )?)?;
    let (notes, remaining, after) = mr_state(&author, &reviewer)?;
    let created = created_positions(&sent.window);
    let revision = &snapshot.revision;
    let checks = anchors_checks(
        &sent.writes,
        &created,
        [
            revision.base_sha().as_str(),
            revision.start_sha().as_str(),
            revision.head_sha().as_str(),
        ],
        &remaining,
        &notes,
        &sent.facts,
    );
    let mut observed_notes = vec![format!(
        "MR клетки: версия {}",
        snapshot.version_id.as_str()
    )];
    observed_notes.extend(range_observations(&created, &notes));
    Ok(Observed {
        checks,
        notes: observed_notes,
        before,
        after,
        journal: sent.journal,
        tier: caps.plan,
        verdict: None,
    })
}

#[cfg(test)]
mod tests;
