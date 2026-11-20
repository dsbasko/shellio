//! Клетки строк GitLab в живых столбцах: общая обвязка (ветки и MR клетки, сторож, уборка, итог
//! в `result.json`) и сценарии строк. Ожидания — в отчёте, «Живые клетки GitLab».
//!
//! Оракул живой клетки не видит полей `forge-fake` (`from_draft`, `origin`): записи пробы берутся
//! из следа ревьюера в окне отправки, состояние — из чтения автором заметок MR и ревьюером своих
//! черновиков. Системные заметки MR (назначение ревьюера, новый коммит) в оракул не входят.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::step_verdict;
use super::{INJECTION_UNAVAILABLE, MR_HEAD_STALE, MR_NOT_READY, classify_read, mentions_tier};
use super::{Verdict, gitlab_adapter, guard_errors, live_repo, read_or_fail, record_cleanup};
use crate::fake::{GraphqlKind, graphql_root};
use crate::gitlab::{
    ApiError, Capabilities, GitLab, KnownDraft, MergeSnapshot, RemoteDraft, RemoteNote,
    RemotePosition, ReviewerState, capabilities,
};
use crate::journal::{ItemState, Journal, Operation, ProjectId, SessionId};
use crate::live::guard::Guarded;
use crate::live::lifecycle::{Cell, CellNames, Fixture, LEDGER_FILE, Ledger, Roles, run_cell};
use crate::model::anchor::{Anchor, LinePoint, Side};
use crate::model::revision::{
    AnchorFate, HeadCheck, HeadPath, Revision, Sha, StaleReason, TripleOrigin, triple_origin,
};
use crate::model::{DraftComment, DraftId, Provider, ProviderId, ReviewSession, ReviewTarget};
use crate::results::{Column, ResultFile};
use crate::scenario::Scenario;
use crate::scenario::fake::{Checks, Spot, add_drafts, dump_in, states};
use crate::scrub::Scrubber;
use crate::submit::reconcile::{Verdict as Reconciled, normalize_body};
use crate::submit::{
    Delivery, GitLabPlan, Report, SubmitError, plan_gitlab, reconcile_gitlab, report, run_gitlab,
};
use crate::transport::{Call, Failure, Method, Request, Response, Trace, Transport};

/// Ключи записей GitLab в следе, как ключи журнала `forge-fake`.
pub const DRAFT_CREATE: &str = "POST draft_notes";
pub const DRAFT_PUBLISH: &str = "PUT draft_notes/{id}/publish";
pub const BULK_PUBLISH: &str = "POST draft_notes/bulk_publish";
pub const NOTE: &str = "POST notes";
pub const APPROVE: &str = "POST approve";
pub const UNAPPROVE: &str = "POST unapprove";
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
    /// Смена head клетки (`gl-approve-sha`, `head-change`).
    pub head: &'a dyn HeadMover,
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
    let roles = LiveRoles {
        author: cx.author,
        reviewer: cx.reviewer,
        trace: cx.trace,
    };
    run_live_cell(
        scenario,
        &roles,
        out,
        scrubber,
        result,
        |cell| match scenario {
            Scenario::GlDraftPublish => gl_draft_publish(cx, cell),
            Scenario::GlForeignDraft => gl_foreign_draft(cx, cell),
            Scenario::GlRequestChanges => gl_request_changes(cx, cell),
            Scenario::GlAnchors => gl_anchors(cx, cell),
            Scenario::GlApproveSha => gl_approve_sha(cx, cell),
            Scenario::HeadChange => gl_head_change(cx, cell),
            Scenario::Partial => gl_partial(cx, cell),
            Scenario::ReconcileDeleted => gl_reconcile_deleted(cx, cell),
            Scenario::ReconcileAmbiguous => gl_reconcile_ambiguous(cx, cell),
            Scenario::JournalCrash => gl_journal_crash(cx, cell),
            Scenario::ReconcilePaged => gl_reconcile_paged(cx, cell),
            other => Err(Verdict::Error(format!(
                "сценарий {} в живом столбце GitLab не реализован",
                other.name()
            ))),
        },
    );
}

/// Сторожа ролей живой клетки над общим следом.
pub struct LiveRoles<'a, T> {
    pub author: &'a Guarded<T>,
    pub reviewer: &'a Guarded<T>,
    pub trace: &'a Trace,
}

/// Обвязка живой клетки любого провайдера: ветки и PR/MR от автора, тело, уборка в любом случае,
/// след и итог в `result.json`.
pub fn run_live_cell<T: Transport>(
    scenario: Scenario,
    roles: &LiveRoles<'_, T>,
    out: &Path,
    scrubber: &Scrubber,
    result: &mut ResultFile,
    body: impl FnOnce(&mut Cell) -> Result<Observed, Verdict>,
) {
    let verdict = cell_verdict(scenario, roles, out, scrubber, result, body);
    result.trace = roles
        .trace
        .entries(scrubber)
        .iter()
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect();
    result.calls = roles
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
    live: &LiveRoles<'_, T>,
    out: &Path,
    scrubber: &Scrubber,
    result: &mut ResultFile,
    body: impl FnOnce(&mut Cell) -> Result<Observed, Verdict>,
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
    let mut ledger = match Ledger::create_for(&out.join(LEDGER_FILE), live.author.target()) {
        Ok(ledger) => ledger,
        Err(err) => return Some(Verdict::Error(err)),
    };
    let (author_repo, reviewer_repo) = match (live_repo(live.author), live_repo(live.reviewer)) {
        (Ok(author), Ok(reviewer)) => (author, reviewer),
        (Err(err), _) | (_, Err(err)) => return Some(Verdict::Error(err)),
    };
    let roles = Roles {
        author: author_repo.as_ref(),
        reviewer: Some(reviewer_repo.as_ref()),
    };
    let title = format!("probe-review {}", scenario.name());
    let run = run_cell(&roles, &mut ledger, &names, &title, |cell| Ok(body(cell)));
    record_cleanup(result, &run.cleanup, scrubber);
    let errors = guard_errors(&[("author", live.author), ("reviewer", live.reviewer)]);
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
pub(super) fn record_observed(
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
        (Method::Post, Some("approve")) => APPROVE,
        (Method::Post, Some("unapprove")) => UNAPPROVE,
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

pub(super) fn stand(what: &str, err: &dyn std::fmt::Display) -> Verdict {
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
    /// Операции и состояния элементов после переоткрытия.
    states: Vec<(Operation, ItemState)>,
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

fn submit_drafts<T: Transport, R: Transport>(
    cx: &GlCx<'_, T>,
    reviewer: &GitLab<R>,
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
    let item_states = states(&scope, submission).map_err(|err| stand("состояния", &err))?;
    let facts = JournalFacts {
        full_confirmed: done.delivery == Delivery::Full
            && done
                .items
                .iter()
                .all(|item| item.state == ItemState::Confirmed),
        states: format!("{item_states:?}"),
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
        states: item_states,
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

/// Второй клиент ревьюера: создаёт черновик на новой строке MR и отдаёт его ID или удаляет
/// черновик по ID. Отказ хоста — `not_run` «второй клиент не создал черновик» или «… не удалил
/// черновик», отказ транспорта, 5xx и сторожа — ошибка исполнения.
pub trait SecondClient {
    fn create_draft(&self, iid: u64, line: u32) -> Result<String, Verdict>;
    fn delete_draft(&self, iid: u64, draft: &str) -> Result<String, Verdict>;
    /// `count` заметок MR с другими текстами; ответ — ID последней.
    fn plant_notes(&self, iid: u64, count: u32) -> Result<String, Verdict>;
}

/// Операция процесса второго клиента.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecondOp {
    /// `gl-create-draft --line <строка>`.
    CreateDraft { line: u32 },
    /// `gl-delete-draft --draft <ID>`.
    DeleteDraft { draft: String },
    /// `gl-plant-notes --count <число>`.
    PlantNotes { count: u32 },
    /// `gh-create-pending --count <число>`: `PENDING` ревьюера на PR `--mr`.
    GhCreatePending { count: u32 },
}

impl SecondOp {
    pub fn name(&self) -> &'static str {
        match self {
            SecondOp::CreateDraft { .. } => "gl-create-draft",
            SecondOp::DeleteDraft { .. } => "gl-delete-draft",
            SecondOp::PlantNotes { .. } => "gl-plant-notes",
            SecondOp::GhCreatePending { .. } => "gh-create-pending",
        }
    }

    /// Провайдер операции: операция другого провайдера в столбце — ошибка исполнения до чтения
    /// предусловий.
    pub fn provider(&self) -> Provider {
        match self {
            SecondOp::CreateDraft { .. }
            | SecondOp::DeleteDraft { .. }
            | SecondOp::PlantNotes { .. } => Provider::GitLab,
            SecondOp::GhCreatePending { .. } => Provider::GitHub,
        }
    }
}

/// Второй клиент отдельным процессом `probe-review --second-client`: хост и токен ревьюера он
/// берёт из унаследованного окружения `run.sh`, токена в аргументах нет.
pub struct ProcessClient {
    pub exe: PathBuf,
    pub column: Column,
    /// Каталог процесса сценария: туда второй клиент пишет свой очищенный след.
    pub out: PathBuf,
}

impl ProcessClient {
    fn run(&self, iid: u64, op: &SecondOp) -> Result<String, Verdict> {
        let (flag, value) = match op {
            SecondOp::CreateDraft { line } => ("--line", line.to_string()),
            SecondOp::DeleteDraft { draft } => ("--draft", draft.clone()),
            SecondOp::PlantNotes { count } | SecondOp::GhCreatePending { count } => {
                ("--count", count.to_string())
            }
        };
        let output = Command::new(&self.exe)
            .args(["--second-client", op.name(), "--column"])
            .arg(self.column.name())
            .arg("--mr")
            .arg(iid.to_string())
            .arg(flag)
            .arg(value)
            .arg("--out")
            .arg(&self.out)
            .stdin(Stdio::null())
            .output()
            .map_err(|err| Verdict::Error(format!("второй клиент не запущен: {}", err.kind())))?;
        second_reply(output.status.code(), &output.stdout, &output.stderr)
    }
}

impl SecondClient for ProcessClient {
    fn create_draft(&self, iid: u64, line: u32) -> Result<String, Verdict> {
        self.run(iid, &SecondOp::CreateDraft { line })
    }

    fn delete_draft(&self, iid: u64, draft: &str) -> Result<String, Verdict> {
        self.run(
            iid,
            &SecondOp::DeleteDraft {
                draft: draft.to_owned(),
            },
        )
    }

    fn plant_notes(&self, iid: u64, count: u32) -> Result<String, Verdict> {
        self.run(iid, &SecondOp::PlantNotes { count })
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

/// Удаление черновика ревьюера вторым клиентом по ID через своего сторожа: ответ — тот же ID.
/// Отказ его сторожа — ошибка исполнения.
pub fn foreign_delete<T: Transport>(
    guard: &Guarded<T>,
    review: &ReviewTarget,
    draft: &str,
) -> Result<String, Verdict> {
    let deleted = gitlab_adapter(guard, review)
        .map_err(|err| stand("второй клиент", &err))
        .and_then(|gitlab| {
            gitlab
                .delete_draft(&KnownDraft::recorded(ProviderId::new(draft)))
                .map_err(|err| match classify_read(&err) {
                    Ok(observed) => {
                        Verdict::NotRun(format!("второй клиент не удалил черновик: {observed}"))
                    }
                    Err(stand) => Verdict::Error(format!("второй клиент: удаление: {stand}")),
                })
        })
        .map(|()| draft.to_owned());
    let errors = guard_errors(&[(ACTOR_SECOND, guard)]);
    if !errors.is_empty() {
        return Err(Verdict::Error(errors.join("; ")));
    }
    if let Some(err) = guard.refusal() {
        return Err(Verdict::Error(format!("второй клиент: сторож: {err}")));
    }
    deleted
}

/// Текст заметок второго клиента `reconcile-paged`: «другая заметка N».
pub const PLANTED_NOTE: &str = "другая заметка";
/// Число заметок второго клиента `reconcile-paged`: одна полная страница по 100.
pub const PLANTED_COUNT: u32 = 100;

/// Заметки MR вторым клиентом через своего сторожа: `count` штук с текстами «другая заметка N»,
/// ответ — ID последней. Отказ хоста — `not_run`, 429 — `not_run` «лимит», отказ транспорта, 5xx
/// и его сторожа — ошибка исполнения.
pub fn foreign_notes<T: Transport>(
    guard: &Guarded<T>,
    review: &ReviewTarget,
    count: u32,
) -> Result<String, Verdict> {
    let planted = gitlab_adapter(guard, review)
        .map_err(|err| stand("второй клиент", &err))
        .and_then(|gitlab| {
            let mut last = None;
            for index in 0..count {
                let note = gitlab
                    .summary_note(&format!("{PLANTED_NOTE} {index}"))
                    .map_err(|err| match &err {
                        ApiError::RateLimited { status, .. } => Verdict::NotRun(format!(
                            "лимит: код {status} на заметку второго клиента {index}"
                        )),
                        _ => match classify_read(&err) {
                            Ok(observed) => Verdict::NotRun(format!(
                                "второй клиент не опубликовал заметки: заметка {index}: {observed}"
                            )),
                            Err(stand) => {
                                Verdict::Error(format!("второй клиент: заметка {index}: {stand}"))
                            }
                        },
                    })?;
                last = Some(note.id.as_str().to_owned());
            }
            last.ok_or_else(|| Verdict::Error("второй клиент: число заметок 0".to_owned()))
        });
    let errors = guard_errors(&[(ACTOR_SECOND, guard)]);
    if !errors.is_empty() {
        return Err(Verdict::Error(errors.join("; ")));
    }
    if let Some(err) = guard.refusal() {
        return Err(Verdict::Error(format!("второй клиент: сторож: {err}")));
    }
    planted
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
    let refusal = |what: &str, err: &ApiError| match classify_read(err) {
        Ok(text) => Ok(format!("отказ: {what}: {text}")),
        Err(stand) => Err(Verdict::Error(format!(
            "наблюдение токеном автора: {what}: {stand}"
        ))),
    };
    let outcome = match author.capabilities() {
        Err(err) => refusal("возможности автора", &err)?,
        Ok(caps) => match author.request_changes(&caps) {
            Ok(state) => format!("принят, состояние {}", state.graphql()),
            Err(err) => refusal("вызов", &err)?,
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

/// Смена head клетки: живая — коммит фикстуры автором ([`Cell::move_head`]), тест — `push_head`
/// `forge-fake`. Возвращает SHA нового head.
pub trait HeadMover {
    fn move_head(&self, cell: &mut Cell) -> Result<String, Verdict>;
}

/// Смена head живой клетки: коммит `Fixture::Moved` (меняется строка 27) в ветку клетки автором
/// через его сторожа.
pub struct AuthorCommit<'a, T> {
    pub author: &'a Guarded<T>,
}

impl<T: Transport> HeadMover for AuthorCommit<'_, T> {
    fn move_head(&self, cell: &mut Cell) -> Result<String, Verdict> {
        let repo = live_repo(self.author).map_err(Verdict::Error)?;
        cell.move_head(repo.as_ref(), Fixture::Moved)
            .map_err(|err| step_verdict(&err))
    }
}

/// Логины одобривших MR по чтению автором.
fn approvers<T: Transport>(author: &GitLab<T>, what: &str) -> Result<Vec<String>, Verdict> {
    author
        .approvals()
        .map(|approval| approval.approved_by)
        .map_err(|err| read_or_fail(what, &err))
}

fn with_approvers(mut value: Value, approvers: &[String]) -> Value {
    value["approved_by"] = json!(approvers);
    value
}

fn approved(approvers: &[String], login: &str) -> bool {
    approvers
        .iter()
        .any(|name| name.eq_ignore_ascii_case(login))
}

/// `sha` тела и код ответа `approve`.
pub type ApproveCall = (Option<String>, Option<u16>);

/// `sha` тела и код ответа каждого `approve` ревьюера в вызовах окна.
pub fn approve_calls(calls: &[Call]) -> Vec<ApproveCall> {
    calls
        .iter()
        .filter(|call| call.actor == ACTOR_REVIEWER)
        .filter_map(|call| {
            let body = call.request.body_json();
            let key = write_key(call.request.method, call.request.path(), body.as_ref())?;
            (key == APPROVE).then(|| {
                (
                    body.as_ref()
                        .and_then(|body| body.get("sha"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    call.outcome.as_ref().ok().map(|response| response.status),
                )
            })
        })
        .collect()
}

/// Итог первого `approve` исполнителем: элемент `failed` или `unknown` с кодом 401 или 403 у
/// последнего `approve` ревьюера в окне или с признаком тарифа в тексте — `absent` (исполнитель
/// свёл `ApiError` к строке, код берётся из следа); затем итог, который оракул записей не судит;
/// другой отказ — `fail`; `confirmed` судят проверки (`None`).
pub fn approve_outcome(
    done: &Report,
    window: &[Call],
    delivery: Option<&Verdict>,
) -> Option<Verdict> {
    let Some(item) = done
        .items
        .iter()
        .find(|item| item.operation == Operation::GitLabApprove)
    else {
        return Some(Verdict::Error(
            "в отправке нет элемента gitlab.approve".to_owned(),
        ));
    };
    let detail = item.detail.as_deref().unwrap_or("без подробностей");
    if matches!(item.state, ItemState::Failed | ItemState::Unknown) {
        if let Some(status @ (401 | 403)) = approve_calls(window).last().and_then(|call| call.1) {
            return Some(Verdict::Absent(format!(
                "approve: отказ по правам или тарифу, код {status}: {detail}"
            )));
        }
        if mentions_tier(detail) {
            return Some(Verdict::Absent(format!(
                "approve: признак тарифа: {detail}"
            )));
        }
    }
    if let Some(verdict) = delivery {
        return Some(verdict.clone());
    }
    match item.state {
        ItemState::Confirmed => None,
        ItemState::Failed => Some(Verdict::Fail(format!("approve с актуальным sha: {detail}"))),
        other => Some(Verdict::Error(format!(
            "элемент gitlab.approve в {other:?}"
        ))),
    }
}

/// Наблюдённое `gl-approve-sha` для оракула.
#[derive(Clone, Debug)]
pub struct ApproveFacts<'a> {
    /// Записи ревьюера за весь ход тела.
    pub writes: &'a [Write],
    /// `sha` и код каждого `approve` ревьюера.
    pub approves: &'a [ApproveCall],
    /// Head ревизии сессий (до смены head).
    pub old_head: &'a str,
    pub reviewer: &'a str,
    /// Одобрившие по чтению автора после первого `approve` и в конце.
    pub after_first: &'a [String],
    pub at_end: &'a [String],
    /// Исполнитель на прежней ревизии: остановился на шаге 1 (`HeadChanged`) и его итог.
    pub stopped: (bool, String),
    /// Прямой `approve` с прежним `sha`.
    pub stale: &'a Result<(), ApiError>,
    /// Элементы отправки на прежней ревизии в `planned`, в `in_flight` не входили.
    pub stale_planned: bool,
    pub journal: &'a JournalFacts,
}

/// Оракул `gl-approve-sha` на живом хосте (ожидание — в отчёте).
pub fn approve_sha_checks(facts: &ApproveFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in facts.writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера",
        counts,
        [(APPROVE.to_owned(), 2), (UNAPPROVE.to_owned(), 1)]
            .into_iter()
            .collect(),
    );
    let class = |status: Option<u16>| match status {
        Some(200..=299) => "2xx".to_owned(),
        Some(status) => status.to_string(),
        None => "нет ответа".to_owned(),
    };
    checks.eq(
        "approve: sha и коды",
        facts
            .approves
            .iter()
            .map(|(sha, status)| (sha.clone(), class(*status)))
            .collect::<Vec<_>>(),
        vec![
            (Some(facts.old_head.to_owned()), "2xx".to_owned()),
            (Some(facts.old_head.to_owned()), "409".to_owned()),
        ],
    );
    checks.check(
        "автор: ревьюер в одобривших после первого approve",
        approved(facts.after_first, facts.reviewer),
        format!("{:?}", facts.after_first),
    );
    checks.check(
        "исполнитель на прежней ревизии останавливается на шаге 1",
        facts.stopped.0,
        facts.stopped.1.clone(),
    );
    checks.check(
        "approve с устаревшим sha — 409",
        matches!(facts.stale, Err(ApiError::ShaRejected(_))),
        format!("{:?}", facts.stale),
    );
    checks.check(
        "автор: ревьюера нет в одобривших в конце",
        !approved(facts.at_end, facts.reviewer),
        format!("{:?}", facts.at_end),
    );
    checks.check(
        "журнал: approve confirmed, итог «полностью»",
        facts.journal.full_confirmed,
        facts.journal.states.clone(),
    );
    checks.check(
        "журнал: отправка на прежней ревизии в planned, в in_flight не входила",
        facts.stale_planned,
        "",
    );
    checks.eq("восстановленные элементы", facts.journal.recovered, 0);
    checks
}

/// Отправка `approve` исполнителем на прежней ревизии `snapshot` в журнале клетки: итог
/// исполнителя, признак «элементы в `planned`, в `in_flight` не входили», сессия и дамп журнала.
fn stale_submit<T: Transport>(
    cx: &GlCx<'_, T>,
    reviewer: &GitLab<&Guarded<T>>,
    review: &ReviewTarget,
    snapshot: &MergeSnapshot,
    caps: &Capabilities,
    journal: &str,
) -> Result<((bool, String), bool, ReviewSession, Value), Verdict> {
    let path = cx.state_dir.join(journal);
    let project = ProjectId::new(LIVE_PROJECT);
    let session = ReviewSession::new(
        &cx.reviewer_login,
        review.clone(),
        snapshot.revision.clone(),
    );
    let (submission, stopped) = {
        let journal = Journal::open(&path).map_err(|err| stand("журнал", &err))?;
        let scope = journal.scope(&project);
        let id = scope
            .create_session(&session)
            .map_err(|err| stand("сессия", &err))?;
        let plan = GitLabPlan {
            approve: true,
            ..GitLabPlan::default()
        };
        let submission = plan_gitlab(&scope, id, &plan).map_err(|err| stand("план", &err))?;
        let run = run_gitlab(&scope, submission.id, reviewer, caps);
        let stopped = match &run {
            Err(SubmitError::HeadChanged { .. }) => true,
            Err(other) => return Err(submit_verdict(other)),
            Ok(_) => false,
        };
        (submission.id, (stopped, format!("{run:?}")))
    };
    let journal = Journal::open(&path).map_err(|err| stand("журнал после переоткрытия", &err))?;
    let scope = journal.scope(&project);
    let planned = scope
        .submission(submission)
        .map_err(|err| stand("отправка в журнале", &err))?
        .items
        .iter()
        .all(|item| item.state == ItemState::Planned && item.in_flight_at.is_none());
    let dump = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
    Ok((stopped, planned, session, dump))
}

fn gl_approve_sha<T: Transport>(cx: &GlCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    const JOURNAL: &str = "journal-gl-approve-sha.db";
    let review = cx.review(cell.number);
    let reviewer = gitlab_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = gitlab_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_snapshot(&reviewer, &cx.poll, None, MR_NOT_READY)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| read_or_fail("возможности ревьюера", &err))?;
    let (_, _, before) = mr_state(&author, &reviewer)?;
    let before = with_approvers(before, &approvers(&author, "одобрения до")?);
    let start = cx.trace.len();
    let sent = submit_drafts(
        cx,
        &reviewer,
        Sending {
            review: &review,
            snapshot: &snapshot,
            caps: &caps,
            journal: JOURNAL,
            drafts: &[],
            plan: GitLabPlan {
                approve: true,
                ..GitLabPlan::default()
            },
        },
    )?;
    let after_first = approvers(&author, "одобрения после первого approve")?;
    let mut notes = vec![format!(
        "MR клетки: версия {}",
        snapshot.version_id.as_str()
    )];
    // Журнал — дамп после последней записи в него: после `stale_submit` в нём вторая отправка.
    let early =
        |verdict: Verdict, notes: Vec<String>, journal: Value| -> Result<Observed, Verdict> {
            let (_, _, after) = mr_state(&author, &reviewer)?;
            let at_end = approvers(&author, "одобрения после")?;
            Ok(Observed {
                notes,
                before: before.clone(),
                after: with_approvers(after, &at_end),
                journal,
                tier: caps.plan.clone(),
                verdict: Some(verdict),
                ..Observed::default()
            })
        };
    if let Some(verdict) = approve_outcome(&sent.done, &sent.window, sent.delivery.as_ref()) {
        return early(verdict, notes, sent.journal.clone());
    }
    if let Err(err) = reviewer.unapprove() {
        let verdict = match classify_read(&err) {
            Ok(text) => Verdict::NotRun(format!("одобрение не снято: {text}")),
            Err(text) => Verdict::Error(format!("снятие одобрения: {text}")),
        };
        return early(verdict, notes, sent.journal.clone());
    }
    let unapproved = approvers(&author, "одобрения после unapprove")?;
    if approved(&unapproved, &cx.reviewer_login) {
        return early(
            Verdict::NotRun("одобрение не снято по чтению автора".to_owned()),
            notes,
            sent.journal.clone(),
        );
    }
    let new_head = cx.head.move_head(cell)?;
    await_snapshot(&reviewer, &cx.poll, Some(&new_head), MR_HEAD_STALE)?;
    let (stopped, stale_planned, stale_session, journal) =
        stale_submit(cx, &reviewer, &review, &snapshot, &caps, JOURNAL)?;
    let stale = reviewer.approve(&stale_session).map(|_| ());
    if let Err(err) = &stale {
        if let ApiError::RateLimited { status, .. } = err {
            notes.push(format!("смена head: новый head {new_head}"));
            return early(
                Verdict::NotRun(format!(
                    "лимит: код {status} на запись {APPROVE} с прежним sha"
                )),
                notes,
                journal,
            );
        }
        if let Err(text) = classify_read(err) {
            return Err(Verdict::Error(format!("approve с прежним sha: {text}")));
        }
    }
    let (_, _, after) = mr_state(&author, &reviewer)?;
    let at_end = approvers(&author, "одобрения после")?;
    let calls = cx.trace.calls();
    let window = calls.get(start..).unwrap_or_default();
    let writes = writes_of(window, ACTOR_REVIEWER);
    let approves = approve_calls(window);
    let checks = approve_sha_checks(&ApproveFacts {
        writes: &writes,
        approves: &approves,
        old_head: snapshot.revision.head_sha().as_str(),
        reviewer: &cx.reviewer_login,
        after_first: &after_first,
        at_end: &at_end,
        stopped,
        stale: &stale,
        stale_planned,
        journal: &sent.facts,
    });
    notes.push(format!("смена head: новый head {new_head}"));
    Ok(Observed {
        checks,
        notes,
        before,
        after: with_approvers(after, &at_end),
        journal,
        tier: caps.plan,
        verdict: None,
    })
}

/// Черновики S `head-change`: строка 05 после правки (новая 4, первый блок) и строка 27 (новая 28,
/// контекст второго блока), которую меняет смена head.
pub const HEAD_S_DRAFTS: [(Spot, &str); 2] = [
    (Spot::Line(Side::New, 4), "сохранится"),
    (Spot::Line(Side::New, 28), "перепривязать"),
];
/// Черновик T `head-change`: добавленная 24a (новая 24), между v1 и v2 не меняется.
pub const HEAD_T_DRAFTS: [(Spot, &str); 1] = [(Spot::Line(Side::New, 24), "остаться")];
/// Явный якорь v2 устаревшего черновика S: «line 27 moved» (новая 28).
const HEAD_REBIND_LINE: u32 = 28;

/// Ревизия тройки позиции: `held` (v1), `fresh` (v2), `mixed` или `foreign`.
pub fn position_origin(held: &Revision, fresh: &Revision, position: &Value) -> &'static str {
    let field = |name: &str| position.get(name).and_then(Value::as_str).map(Sha::parse);
    let triple = match (field("base_sha"), field("start_sha"), field("head_sha")) {
        (Some(Ok(base)), Some(Ok(start)), Some(Ok(head))) => Revision::new(base, start, head),
        _ => return "foreign",
    };
    match triple_origin(held, fresh, &triple) {
        TripleOrigin::Held => "held",
        TripleOrigin::Fresh => "fresh",
        TripleOrigin::Mixed { .. } => "mixed",
        TripleOrigin::Foreign => "foreign",
    }
}

/// Наблюдённое `head-change` для оракула.
#[derive(Clone, Debug)]
pub struct HeadFacts<'a> {
    /// Исполнитель S на прежней ревизии остановился на шаге 1 со свежей ревизией и его итог.
    pub stopped: (bool, String),
    /// Записи ревьюера до выбора пути (ход исполнителя S на прежней ревизии).
    pub blocked_writes: &'a [Write],
    /// Записи ревьюера за весь ход после смены head.
    pub writes: &'a [Write],
    /// Коды ответов ревьюеру за весь ход после смены head.
    pub statuses: &'a [u16],
    pub fates: &'a [(DraftId, AnchorFate)],
    /// ID черновиков S: сохраняемый и устаревающий.
    pub s_ids: &'a [DraftId],
    /// Элементы второй отправки S.
    pub planned: &'a [(Operation, Vec<DraftId>)],
    /// Путь «остаться»: отказ хоста на создание или публикацию черновика T.
    pub stay: &'a Option<String>,
    /// Ревизия тройки каждой исходящей позиции по тексту черновика.
    pub origins: &'a BTreeMap<String, &'static str>,
    pub drafts_after: usize,
    /// Тексты заметок автора с позицией.
    pub positioned: &'a BTreeSet<String>,
    /// Журнал после переоткрытия: head сессии S, head и признак действующего у черновиков S,
    /// head сессии T.
    pub heads: &'a (String, Vec<(String, bool)>, String),
    pub old_head: &'a str,
    pub new_head: &'a str,
    pub journal: &'a JournalFacts,
}

/// Оракул `head-change` на живом хосте (ожидание — в отчёте).
pub fn head_change_checks(facts: &HeadFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    checks.check(
        "исполнитель S на прежней ревизии останавливается на шаге 1 со свежей ревизией",
        facts.stopped.0,
        facts.stopped.1.clone(),
    );
    checks.eq("записи до выбора пути", facts.blocked_writes.len(), 0);
    let [kept, stale] = facts.s_ids else {
        checks.check("черновики S", false, format!("{:?}", facts.s_ids));
        return checks;
    };
    checks.eq(
        "классификация якорей",
        facts.fates.to_vec(),
        vec![
            (*kept, AnchorFate::Kept),
            (*stale, AnchorFate::Stale(StaleReason::LinesChanged)),
        ],
    );
    checks.eq(
        "после перепривязки в отправку входят два черновика",
        facts.planned.to_vec(),
        vec![
            (Operation::GitLabCreateDraftNote, vec![*kept]),
            (Operation::GitLabCreateDraftNote, vec![*stale]),
            (Operation::GitLabPublishDraftNote, vec![*kept]),
            (Operation::GitLabPublishDraftNote, vec![*stale]),
        ],
    );
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in facts.writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера",
        counts,
        [(DRAFT_CREATE.to_owned(), 3), (DRAFT_PUBLISH.to_owned(), 3)]
            .into_iter()
            .collect(),
    );
    checks.check(
        "ответов 400 нет",
        !facts.statuses.contains(&400),
        format!("{:?}", facts.statuses),
    );
    checks.check(
        "путь «остаться»: черновик T создан и опубликован на v1",
        facts.stay.is_none(),
        facts.stay.clone().unwrap_or_default(),
    );
    let bodies =
        |drafts: &[(Spot, &'static str)]| drafts.iter().map(|(_, body)| *body).collect::<Vec<_>>();
    let wanted: BTreeMap<String, &'static str> = bodies(&HEAD_T_DRAFTS)
        .into_iter()
        .map(|body| (body.to_owned(), "held"))
        .chain(
            bodies(&HEAD_S_DRAFTS)
                .into_iter()
                .map(|body| (body.to_owned(), "fresh")),
        )
        .collect();
    checks.eq(
        "тройки исходящих позиций",
        facts.origins.clone(),
        wanted.clone(),
    );
    checks.eq("черновиков ревьюера после отправки", facts.drafts_after, 0);
    checks.eq(
        "автор: заметки с позициями черновиков",
        facts.positioned.clone(),
        wanted.into_keys().collect::<BTreeSet<_>>(),
    );
    checks.eq(
        "журнал: S на v2 с обоими черновиками, T на v1",
        facts.heads.clone(),
        (
            facts.new_head.to_owned(),
            vec![(facts.new_head.to_owned(), true); 2],
            facts.old_head.to_owned(),
        ),
    );
    checks.check(
        "журнал: отправка S — все элементы confirmed, итог «полностью»",
        facts.journal.full_confirmed,
        facts.journal.states.clone(),
    );
    checks.eq("восстановленные элементы", facts.journal.recovered, 0);
    checks
}

/// Путь «остаться»: черновик T создаётся и публикуется адаптером на прежней ревизии. Отказ хоста —
/// `Ok(Some(текст))`, его судит оракул; лимит — `not_run`, транспорт и 5xx — ошибка исполнения.
fn stay_on_held<T: Transport>(
    reviewer: &GitLab<&Guarded<T>>,
    session: &ReviewSession,
    draft: &DraftComment,
) -> Result<Option<String>, Verdict> {
    let refused = |what: &str, err: ApiError| -> Result<Option<String>, Verdict> {
        if let ApiError::RateLimited { status, .. } = err {
            return Err(Verdict::NotRun(format!(
                "лимит: код {status} на запись {what} пути «остаться»"
            )));
        }
        match classify_read(&err) {
            Ok(text) => Ok(Some(format!("{what}: {text}"))),
            Err(text) => Err(Verdict::Error(format!("путь «остаться», {what}: {text}"))),
        }
    };
    let known = match reviewer.create_draft(session, draft, false) {
        Ok((known, _)) => known,
        Err(err) => return refused(DRAFT_CREATE, err),
    };
    match reviewer.publish_draft(&known) {
        Ok(()) => Ok(None),
        Err(err) => refused(DRAFT_PUBLISH, err),
    }
}

fn gl_head_change<T: Transport>(cx: &GlCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = gitlab_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = gitlab_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let held = await_snapshot(&reviewer, &cx.poll, None, MR_NOT_READY)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| read_or_fail("возможности ревьюера", &err))?;
    let (_, _, before) = mr_state(&author, &reviewer)?;
    let path = cx.state_dir.join("journal-head-change.db");
    let project = ProjectId::new(LIVE_PROJECT);
    let mut notes = vec![format!("MR клетки: версия {}", held.version_id.as_str())];
    let journal = Journal::open(&path).map_err(|err| stand("журнал", &err))?;
    let scope = journal.scope(&project);
    let session_of = |drafts: &[(Spot, &str)]| -> Result<(SessionId, Vec<DraftId>), Verdict> {
        let session = ReviewSession::new(&cx.reviewer_login, review.clone(), held.revision.clone());
        let id = scope
            .create_session(&session)
            .map_err(|err| stand("сессия", &err))?;
        let ids =
            add_drafts(&scope, id, &held.files, drafts).map_err(|err| stand("черновики", &err))?;
        Ok((id, ids))
    };
    let (s_id, s_ids) = session_of(&HEAD_S_DRAFTS)?;
    let (t_id, t_ids) = session_of(&HEAD_T_DRAFTS)?;
    let new_head = cx.head.move_head(cell)?;
    let fresh = await_snapshot(&reviewer, &cx.poll, Some(&new_head), MR_HEAD_STALE)?;
    notes.push(format!(
        "смена head: новый head {new_head}, версия {}",
        fresh.version_id.as_str()
    ));
    let start = cx.trace.len();
    let blocked =
        plan_gitlab(&scope, s_id, &GitLabPlan::default()).map_err(|err| stand("план", &err))?;
    let stopped = match run_gitlab(&scope, blocked.id, &reviewer, &caps) {
        Err(SubmitError::HeadChanged { fresh: seen }) => (
            *seen == fresh.revision,
            format!("HeadChanged до {}", seen.head_sha().as_str()),
        ),
        Err(other) => return Err(submit_verdict(&other)),
        Ok(done) => (
            false,
            format!("исполнитель не остановился: {:?}", done.delivery),
        ),
    };
    let blocked_writes = writes_of(
        cx.trace.calls().get(start..).unwrap_or_default(),
        ACTOR_REVIEWER,
    );
    scope
        .close_submission(blocked.id)
        .map_err(|err| stand("закрытие отправки", &err))?;
    let mut session = scope.session(s_id).map_err(|err| stand("сессия S", &err))?;
    let mut drafts = scope
        .drafts(s_id)
        .map_err(|err| stand("черновики S", &err))?;
    let HeadCheck::Changed(change) =
        session.check_head(&fresh.revision, &held.files, &fresh.files, &drafts)
    else {
        return Err(Verdict::Error("смена head не замечена моделью".to_owned()));
    };
    let fates = change.fates().to_vec();
    let file = fresh
        .files
        .first()
        .ok_or_else(|| Verdict::Error(DIFF_EMPTY.to_owned()))?;
    let rebound = LinePoint::at(file, Side::New, HEAD_REBIND_LINE)
        .and_then(|point| Anchor::line(file, point))
        .map_err(|err| stand("явный якорь v2", &err))?;
    let rebind = fates
        .iter()
        .filter(|(id, fate)| s_ids.get(1) == Some(id) && matches!(fate, AnchorFate::Stale(_)))
        .map(|(id, _)| (*id, rebound.clone()))
        .collect();
    session
        .apply_head(&change, HeadPath::Rebind(rebind), &mut drafts, &fresh.files)
        .map_err(|err| stand("перепривязка", &err))?;
    scope
        .save_head(s_id, &session, &drafts)
        .map_err(|err| stand("сохранение перепривязки", &err))?;
    let t_session = scope.session(t_id).map_err(|err| stand("сессия T", &err))?;
    let t_draft = t_ids
        .first()
        .ok_or_else(|| Verdict::Error("нет черновика T".to_owned()))
        .and_then(|id| scope.draft(*id).map_err(|err| stand("черновик T", &err)));
    let stay = stay_on_held(&reviewer, &t_session, &t_draft?);
    let moved =
        plan_gitlab(&scope, s_id, &GitLabPlan::default()).map_err(|err| stand("план", &err))?;
    let planned: Vec<(Operation, Vec<DraftId>)> = moved
        .items
        .iter()
        .map(|item| (item.operation, item.drafts.clone()))
        .collect();
    let s_start = cx.trace.len();
    let sent = match &stay {
        Err(_) => None,
        Ok(_) => Some(
            run_gitlab(&scope, moved.id, &reviewer, &caps).map_err(|err| submit_verdict(&err))?,
        ),
    };
    drop(scope);
    drop(journal);
    let calls = cx.trace.calls();
    let delivery = sent.as_ref().and_then(|sent| {
        delivery_verdict(
            sent,
            calls.get(s_start..).unwrap_or_default(),
            ACTOR_REVIEWER,
        )
    });
    let journal = Journal::open(&path).map_err(|err| stand("журнал после переоткрытия", &err))?;
    let scope = journal.scope(&project);
    let dump = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
    let (notes_after, drafts_after, after) = mr_state(&author, &reviewer)?;
    let early = |verdict: Verdict, notes: Vec<String>| Observed {
        notes,
        before: before.clone(),
        after: after.clone(),
        journal: dump.clone(),
        tier: caps.plan.clone(),
        verdict: Some(verdict),
        ..Observed::default()
    };
    let stay = match stay {
        Err(verdict) => return Ok(early(verdict, notes)),
        Ok(stay) => stay,
    };
    if let Some(verdict) = delivery {
        return Ok(early(verdict, notes));
    }
    let done = report(&scope, moved.id).map_err(|err| stand("итог отправки", &err))?;
    let s_drafts = scope
        .drafts(s_id)
        .map_err(|err| stand("черновики S", &err))?;
    let head_of = |id| -> Result<String, Verdict> {
        Ok(scope
            .session(id)
            .map_err(|err| stand("сессия", &err))?
            .revision()
            .head_sha()
            .as_str()
            .to_owned())
    };
    let heads = (
        head_of(s_id)?,
        s_drafts
            .iter()
            .map(|draft| {
                (
                    draft.revision().head_sha().as_str().to_owned(),
                    draft.is_current(),
                )
            })
            .collect(),
        head_of(t_id)?,
    );
    let facts = JournalFacts {
        full_confirmed: done.delivery == Delivery::Full
            && done
                .items
                .iter()
                .all(|item| item.state == ItemState::Confirmed),
        states: format!(
            "{:?}",
            states(&scope, moved.id).map_err(|err| stand("состояния", &err))?
        ),
        remote_ids: s_drafts.iter().all(|draft| draft.remote_id.is_some()),
        recovered: journal.recovered().len(),
    };
    let window = calls.get(start..).unwrap_or_default();
    let reviewer_calls = window.iter().filter(|call| call.actor == ACTOR_REVIEWER);
    let statuses: Vec<u16> = reviewer_calls
        .filter_map(|call| call.outcome.as_ref().ok().map(|response| response.status))
        .collect();
    let origins: BTreeMap<String, &'static str> = created_positions(window)
        .into_iter()
        .map(|(body, position)| {
            let origin = position_origin(&held.revision, &fresh.revision, &position);
            (body, origin)
        })
        .collect();
    let positioned: BTreeSet<String> = notes_after
        .iter()
        .filter(|note| !note.system)
        .filter_map(|note| {
            let position = note.position.as_ref()?;
            notes.push(format!(
                "наблюдение: заметка «{}» вернулась с тройкой {}",
                note.body,
                position_origin(&held.revision, &fresh.revision, &position_json(position))
            ));
            Some(note.body.clone())
        })
        .collect();
    let checks = head_change_checks(&HeadFacts {
        stopped,
        blocked_writes: &blocked_writes,
        writes: &writes_of(window, ACTOR_REVIEWER),
        statuses: &statuses,
        fates: &fates,
        s_ids: &s_ids,
        planned: &planned,
        stay: &stay,
        origins: &origins,
        drafts_after: drafts_after.len(),
        positioned: &positioned,
        heads: &heads,
        old_head: held.revision.head_sha().as_str(),
        new_head: &new_head,
        journal: &facts,
    });
    Ok(Observed {
        checks,
        notes,
        before,
        after,
        journal: dump,
        tier: caps.plan,
        verdict: None,
    })
}

/// Что делает клиентская инъекция на первой публикации черновика ревьюера.
pub enum OnPublish<'a> {
    /// `partial`: перед публикацией второй клиент удаляет второй созданный в окне черновик,
    /// запрос уходит.
    DeleteSecond {
        second: &'a dyn SecondClient,
        iid: u64,
        trace: &'a Trace,
        start: usize,
    },
    /// `reconcile-deleted`: запрос не отправляется, клиенту отдаётся таймаут (запрос мог дойти,
    /// элемент — `unknown`).
    Drop,
    /// `reconcile-ambiguous`, `reconcile-paged`: запрос уходит, ответ 2xx отбрасывается, клиенту
    /// отдаётся таймаут. Ответ не 2xx возвращается как есть, инъекция не срабатывает.
    LoseResponse,
}

/// Сработавшая инъекция: итог действия — ID, удалённый вторым клиентом, или ID в пути не
/// отправленной публикации.
#[derive(Debug)]
pub struct Fired {
    pub outcome: Result<String, Verdict>,
}

/// Транспорт ревьюера с клиентской инъекцией на первой публикации черновика (`DRAFT_PUBLISH`):
/// срабатывает один раз, остальные запросы идут как есть. Стоит над сторожем, поэтому не
/// отправленный запрос не попадает в след: его фиксирует [`Fired`].
pub struct PublishHook<'a, R> {
    pub inner: R,
    pub action: OnPublish<'a>,
    /// Номер публикации с нуля, на которой срабатывает инъекция.
    pub at: usize,
    seen: std::cell::Cell<usize>,
    pub fired: RefCell<Option<Fired>>,
}

impl<'a, R> PublishHook<'a, R> {
    pub fn new(inner: R, action: OnPublish<'a>, at: usize) -> PublishHook<'a, R> {
        PublishHook {
            inner,
            action,
            at,
            seen: std::cell::Cell::new(0),
            fired: RefCell::new(None),
        }
    }
}

/// ID черновика в пути публикации.
fn path_draft(request: &Request) -> Option<String> {
    request
        .path()
        .split_once("/merge_requests/")
        .and_then(|(_, rest)| rest.split_once('/'))
        .and_then(|(_, tail)| publish_id(tail.split_once('?').map_or(tail, |(t, _)| t)))
        .map(str::to_owned)
}

impl<R: Transport> Transport for PublishHook<'_, R> {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let body = request.body_json();
        let publish = write_key(request.method, request.path(), body.as_ref()).as_deref()
            == Some(DRAFT_PUBLISH)
            && self.fired.borrow().is_none();
        if !publish {
            return self.inner.send(request);
        }
        let index = self.seen.get();
        self.seen.set(index + 1);
        if index != self.at {
            return self.inner.send(request);
        }
        let draft = || {
            path_draft(request).ok_or_else(|| Verdict::Error("в пути публикации нет ID".to_owned()))
        };
        match &self.action {
            OnPublish::Drop => {
                *self.fired.borrow_mut() = Some(Fired { outcome: draft() });
                Err(Failure::timeout(
                    "инъекция пробы: запрос не отправлен, клиенту отдан таймаут",
                ))
            }
            OnPublish::LoseResponse => {
                let response = self.inner.send(request);
                if !matches!(&response, Ok(reply) if reply.is_success()) {
                    return response;
                }
                *self.fired.borrow_mut() = Some(Fired { outcome: draft() });
                Err(Failure::timeout(
                    "инъекция пробы: ответ получен и отброшен, клиенту отдан таймаут",
                ))
            }
            OnPublish::DeleteSecond {
                second,
                iid,
                trace,
                start,
            } => {
                let created: Vec<String> = writes_of(
                    trace.calls().get(*start..).unwrap_or_default(),
                    ACTOR_REVIEWER,
                )
                .into_iter()
                .filter_map(|write| write.created)
                .collect();
                let outcome = match created.get(1) {
                    Some(id) => second.delete_draft(*iid, id),
                    None => Err(Verdict::Error(format!(
                        "перед первой публикацией создано черновиков {}",
                        created.len()
                    ))),
                };
                *self.fired.borrow_mut() = Some(Fired { outcome });
                self.inner.send(request)
            }
        }
    }
}

/// Адаптер ревьюера над транспортом с инъекцией и теми же адресами цели сторожа.
fn hooked<'a, 'h, T: Transport>(
    cx: &GlCx<'a, T>,
    review: &ReviewTarget,
    action: OnPublish<'h>,
    at: usize,
) -> Result<GitLab<PublishHook<'h, &'a Guarded<T>>>, Verdict> {
    GitLab::new(
        PublishHook::new(cx.reviewer, action, at),
        cx.reviewer.target().gitlab_endpoints(),
        review,
    )
    .map_err(|err| stand("адаптер", &err))
}

/// Черновики `partial`: новая 4 («line 05 changed»), 24 (24a) и 25 (24b); второй удаляет второй
/// клиент перед первой публикацией.
pub const PARTIAL_DRAFTS: [(Spot, &str); 3] = [
    (Spot::Line(Side::New, 4), "первый"),
    (Spot::Line(Side::New, 24), "второй"),
    (Spot::Line(Side::New, 25), "третий"),
];

/// Наблюдённое `partial` для оракула.
#[derive(Clone, Debug)]
pub struct PartialFacts<'a> {
    pub writes: &'a [Write],
    /// ID, который второй клиент удалил перед первой публикацией.
    pub deleted: Option<&'a str>,
    /// Публикации ревьюера в окне: ID в пути и код ответа.
    pub publishes: &'a [(Option<String>, Option<u16>)],
    pub states: &'a [(Operation, ItemState)],
    pub closed: bool,
    pub delivery: Delivery,
    pub notes_after: &'a [RemoteNote],
    pub drafts_after: &'a [RemoteDraft],
    pub recovered: usize,
}

/// Оракул `partial` на живом хосте (ожидание — в отчёте). Код отказа второй публикации — только
/// наблюдение против гипотезы 404, условие — отказ хоста 4xx, кроме 429.
pub fn partial_checks(facts: &PartialFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in facts.writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера",
        counts,
        [(DRAFT_CREATE.to_owned(), 3), (DRAFT_PUBLISH.to_owned(), 2)]
            .into_iter()
            .collect(),
    );
    let created: Vec<&str> = facts
        .writes
        .iter()
        .filter_map(|write| write.created.as_deref())
        .collect();
    checks.eq(
        "второй клиент удалил второй созданный черновик до первой публикации",
        facts.deleted,
        created.get(1).copied(),
    );
    checks.eq(
        "публикации по ID первого и второго созданных",
        facts
            .publishes
            .iter()
            .map(|(id, _)| id.as_deref())
            .collect::<Vec<_>>(),
        created.iter().take(2).map(|id| Some(*id)).collect(),
    );
    let codes: Vec<Option<u16>> = facts.publishes.iter().map(|(_, code)| *code).collect();
    checks.check(
        "первая публикация 2xx, вторая — отказ хоста 4xx, кроме 429",
        matches!(codes[..], [Some(first), Some(second)]
            if (200..300).contains(&first) && (400..500).contains(&second) && second != 429),
        format!("{codes:?}"),
    );
    checks.eq(
        "журнал: состояния",
        facts.states.to_vec(),
        vec![
            (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
            (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
            (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
            (Operation::GitLabPublishDraftNote, ItemState::Confirmed),
            (Operation::GitLabPublishDraftNote, ItemState::Failed),
            (Operation::GitLabPublishDraftNote, ItemState::Planned),
        ],
    );
    checks.eq(
        "журнал: отправка закрыта, итог «частично»",
        (facts.closed, facts.delivery),
        (true, Delivery::Partial),
    );
    checks.eq(
        "автор: одна заметка из первого черновика",
        facts
            .notes_after
            .iter()
            .filter(|note| !note.system)
            .map(|note| {
                (
                    note.position
                        .as_ref()
                        .and_then(|position| position.new_line),
                    note.body.clone(),
                )
            })
            .collect::<Vec<_>>(),
        vec![(Some(4), "первый".to_owned())],
    );
    checks.eq(
        "черновики ревьюера после отправки — третий",
        facts
            .drafts_after
            .iter()
            .map(|draft| (draft.id.as_str().to_owned(), draft.note.clone()))
            .collect::<Vec<_>>(),
        created
            .get(2)
            .map(|id| ((*id).to_owned(), "третий".to_owned()))
            .into_iter()
            .collect::<Vec<_>>(),
    );
    checks.eq("восстановленные элементы", facts.recovered, 0);
    checks
}

/// Публикации роли в вызовах окна: ID в пути и код ответа.
fn publishes_of(calls: &[Call], actor: &str) -> Vec<(Option<String>, Option<u16>)> {
    calls
        .iter()
        .filter(|call| call.actor == actor)
        .filter(|call| {
            write_key(
                call.request.method,
                call.request.path(),
                call.request.body_json().as_ref(),
            )
            .as_deref()
                == Some(DRAFT_PUBLISH)
        })
        .map(|call| {
            let draft = call
                .request
                .path()
                .split_once("/merge_requests/")
                .and_then(|(_, rest)| rest.split_once('/'))
                .and_then(|(_, tail)| publish_id(tail))
                .map(str::to_owned);
            (draft, call.status())
        })
        .collect()
}

fn gl_partial<T: Transport>(cx: &GlCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = gitlab_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = gitlab_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_snapshot(&reviewer, &cx.poll, None, MR_NOT_READY)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| read_or_fail("возможности ревьюера", &err))?;
    let (_, _, before) = mr_state(&author, &reviewer)?;
    let hook = hooked(
        cx,
        &review,
        OnPublish::DeleteSecond {
            second: cx.second,
            iid: cell.number,
            trace: cx.trace,
            start: cx.trace.len(),
        },
        0,
    )?;
    let sent = submit_drafts(
        cx,
        &hook,
        Sending {
            review: &review,
            snapshot: &snapshot,
            caps: &caps,
            journal: "journal-partial.db",
            drafts: &PARTIAL_DRAFTS,
            plan: GitLabPlan::default(),
        },
    )?;
    let fired = hook.transport().fired.borrow_mut().take();
    let (notes_after, drafts_after, after) = mr_state(&author, &reviewer)?;
    let mut notes = vec![format!(
        "MR клетки: версия {}",
        snapshot.version_id.as_str()
    )];
    let early = |verdict: Verdict, notes: Vec<String>| Observed {
        notes,
        before: before.clone(),
        after: after.clone(),
        journal: sent.journal.clone(),
        tier: caps.plan.clone(),
        verdict: Some(verdict),
        ..Observed::default()
    };
    let deleted = match fired.map(|fired| fired.outcome) {
        Some(Err(verdict)) => return Ok(early(verdict, notes)),
        Some(Ok(id)) => Some(id),
        None => None,
    };
    if let Some(verdict) = sent.delivery.clone() {
        return Ok(early(verdict, notes));
    }
    let publishes = publishes_of(&sent.window, ACTOR_REVIEWER);
    if let Some(deleted) = &deleted {
        notes.push(format!(
            "второй клиент удалил черновик {deleted} перед первой публикацией, след — {SECOND_FILE}"
        ));
    }
    if let Some((_, code)) = publishes.get(1) {
        notes.push(format!(
            "наблюдение: код публикации удалённого черновика {code:?}, гипотеза 404"
        ));
    }
    let checks = partial_checks(&PartialFacts {
        writes: &sent.writes,
        deleted: deleted.as_deref(),
        publishes: &publishes,
        states: &sent.states,
        closed: sent.done.closed,
        delivery: sent.done.delivery,
        notes_after: &notes_after,
        drafts_after: &drafts_after,
        recovered: sent.facts.recovered,
    });
    Ok(Observed {
        checks,
        notes,
        before,
        after,
        journal: sent.journal,
        tier: caps.plan,
        verdict: None,
    })
}

/// Черновик `reconcile-deleted`: новая 4 («line 05 changed»).
pub const DELETED_DRAFTS: [(Spot, &str); 1] = [(Spot::Line(Side::New, 4), "удалят")];

/// Наблюдённое `reconcile-deleted` для оракула.
#[derive(Clone, Debug)]
pub struct DeletedFacts<'a> {
    /// Записи ревьюера от отправки до конца сверки.
    pub writes: &'a [Write],
    /// ID в пути публикации, которую инъекция не отправила.
    pub dropped: Option<&'a str>,
    /// Итоги сверки.
    pub reconciled: &'a [Reconciled],
    /// Элемент публикации после переоткрытия: состояние и кандидат.
    pub item: Option<(ItemState, Option<String>)>,
    pub delivery: Delivery,
    pub notes_after: &'a [RemoteNote],
    pub drafts_after: &'a [RemoteDraft],
    pub recovered: usize,
}

/// Оракул `reconcile-deleted` на живом хосте (ожидание — в отчёте).
pub fn deleted_checks(facts: &DeletedFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in facts.writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера: создание, публикация не отправлена, повторов нет",
        counts,
        [(DRAFT_CREATE.to_owned(), 1)].into_iter().collect(),
    );
    let created = facts
        .writes
        .iter()
        .find_map(|write| write.created.as_deref());
    checks.check(
        "инъекция: публикация созданного черновика не отправлена",
        facts.dropped.is_some() && facts.dropped == created,
        format!("не отправлена {:?}, создан {created:?}", facts.dropped),
    );
    checks.check(
        "сверка: «не выяснено» без кандидата",
        matches!(facts.reconciled, [Reconciled::Unresolved { .. }]),
        format!("{:?}", facts.reconciled),
    );
    checks.eq(
        "журнал: публикация unknown без кандидата, итог «частично»",
        (facts.item.clone(), facts.delivery),
        (Some((ItemState::Unknown, None)), Delivery::Partial),
    );
    checks.eq(
        "сервер: ни черновика, ни заметки",
        (
            facts.drafts_after.len(),
            facts.notes_after.iter().filter(|note| !note.system).count(),
        ),
        (0, 0),
    );
    checks.eq("восстановленные элементы", facts.recovered, 0);
    checks
}

/// Ключ чтения GitLab для причины без пути репозитория: хвост после `merge_requests/<iid>/` с
/// числовыми ID, заменёнными на `{id}`, или `graphql`.
pub fn read_key(method: Method, path: &str) -> String {
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    if path.ends_with("/api/graphql") {
        return format!("{method} graphql");
    }
    let tail = path
        .split_once("/merge_requests/")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, tail)| tail);
    match tail {
        Some(tail) => {
            let segments: Vec<&str> = tail
                .split('/')
                .map(|segment| {
                    if !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_digit()) {
                        "{id}"
                    } else {
                        segment
                    }
                })
                .collect();
            format!("{method} {}", segments.join("/"))
        }
        None => format!("{method} вне MR"),
    }
}

/// Итог «не выяснено» сверки из-за неудачного чтения: 429 на последнем вызове роли в окне сверки —
/// `not_run` «лимит» по общему правилу строк GitLab, иначе ошибка исполнения (сверка не
/// различает отказ транспорта, 5xx и лимит, различает след).
pub fn unread_verdict(detail: &str, window: &[Call], actor: &str) -> Verdict {
    match window.iter().rev().find(|call| call.actor == actor) {
        Some(call) if call.status() == Some(429) => Verdict::NotRun(format!(
            "лимит: код 429 на чтение сверки {}",
            read_key(call.request.method, call.request.path())
        )),
        _ => Verdict::Error(format!("сверка: {detail}")),
    }
}

fn gl_reconcile_deleted<T: Transport>(
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
    let (_, _, before) = mr_state(&author, &reviewer)?;
    let path = cx.state_dir.join("journal-reconcile-deleted.db");
    let project = ProjectId::new(LIVE_PROJECT);
    let mut notes = vec![format!(
        "MR клетки: версия {}",
        snapshot.version_id.as_str()
    )];
    let start = cx.trace.len();
    let hook = hooked(cx, &review, OnPublish::Drop, 0)?;
    let (submission, lost, sent, deleted, reconciled) = {
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
        add_drafts(&scope, id, &snapshot.files, &DELETED_DRAFTS)
            .map_err(|err| stand("черновики", &err))?;
        let submission =
            plan_gitlab(&scope, id, &GitLabPlan::default()).map_err(|err| stand("план", &err))?;
        let sent =
            run_gitlab(&scope, submission.id, &hook, &caps).map_err(|err| submit_verdict(&err))?;
        let lost = sent
            .items
            .iter()
            .find(|item| item.operation == Operation::GitLabPublishDraftNote)
            .map(|item| item.id);
        let fired = hook.transport().fired.borrow_mut().take();
        let (deleted, reconciled) = match fired.map(|fired| fired.outcome) {
            Some(Ok(draft)) => {
                let deleted = cx.second.delete_draft(cell.number, &draft);
                let reconciled = match &deleted {
                    Ok(_) => {
                        let at = cx.trace.len();
                        let verdicts = reconcile_gitlab(&scope, submission.id, &reviewer)
                            .map_err(|err| stand("сверка", &err))?;
                        (verdicts, at)
                    }
                    Err(_) => (Vec::new(), cx.trace.len()),
                };
                (Some(deleted), reconciled)
            }
            Some(Err(verdict)) => return Err(verdict),
            None => (None, (Vec::new(), cx.trace.len())),
        };
        (submission.id, lost, sent, deleted, reconciled)
    };
    let (reconciled, reconcile_start) = reconciled;
    let calls = cx.trace.calls();
    let window = calls.get(start..).unwrap_or_default();
    let reconcile_window = calls.get(reconcile_start..).unwrap_or_default();
    let journal = Journal::open(&path).map_err(|err| stand("журнал после переоткрытия", &err))?;
    let scope = journal.scope(&project);
    let dump = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
    let done = report(&scope, submission).map_err(|err| stand("итог отправки", &err))?;
    let item = match lost {
        Some(lost) => {
            let item = scope.item(lost).map_err(|err| stand("элемент", &err))?;
            Some((
                item.state,
                item.candidate
                    .map(|candidate| candidate.as_str().to_owned()),
            ))
        }
        None => None,
    };
    let (notes_after, drafts_after, after) = mr_state(&author, &reviewer)?;
    let early = |verdict: Verdict, notes: Vec<String>| Observed {
        notes,
        before: before.clone(),
        after: after.clone(),
        journal: dump.clone(),
        tier: caps.plan.clone(),
        verdict: Some(verdict),
        ..Observed::default()
    };
    let dropped = match deleted {
        Some(Err(verdict)) => return Ok(early(verdict, notes)),
        Some(Ok(draft)) => Some(draft),
        // Инъекция не сработала: публикации не было, итог создания — по общему правилу.
        None => {
            if let Some(verdict) = delivery_verdict(&sent, window, ACTOR_REVIEWER) {
                return Ok(early(verdict, notes));
            }
            None
        }
    };
    if let [Reconciled::Unresolved { detail }] = &reconciled[..]
        && detail.starts_with("чтение ")
    {
        return Ok(early(
            unread_verdict(detail, reconcile_window, ACTOR_REVIEWER),
            notes,
        ));
    }
    if let Some(draft) = &dropped {
        notes.push(format!(
            "инъекция: публикация черновика {draft} не отправлена, клиенту отдан таймаут; \
второй клиент удалил черновик, след — {SECOND_FILE}"
        ));
    }
    let checks = deleted_checks(&DeletedFacts {
        writes: &writes_of(window, ACTOR_REVIEWER),
        dropped: dropped.as_deref(),
        reconciled: &reconciled,
        item,
        delivery: done.delivery,
        notes_after: &notes_after,
        drafts_after: &drafts_after,
        recovered: journal.recovered().len(),
    });
    Ok(Observed {
        checks,
        notes,
        before,
        after,
        journal: dump,
        tier: caps.plan,
        verdict: None,
    })
}

/// Черновики `reconcile-ambiguous`: одинаковый текст на новой строке 4 («line 05 changed»).
pub const AMBIGUOUS_NOTE: &str = "одинаковый";
pub const AMBIGUOUS_DRAFTS: [(Spot, &str); 2] = [
    (Spot::Line(Side::New, 4), AMBIGUOUS_NOTE),
    (Spot::Line(Side::New, 4), AMBIGUOUS_NOTE),
];

/// Наблюдённое `reconcile-ambiguous` для оракула.
#[derive(Clone, Debug)]
pub struct AmbiguousFacts<'a> {
    /// Записи ревьюера от отправки до конца сверки.
    pub writes: &'a [Write],
    /// ID в пути публикации, ответ на которую отброшен.
    pub lost: Option<&'a str>,
    pub reconciled: &'a [Reconciled],
    pub states: &'a [(Operation, ItemState)],
    /// Элемент второй публикации после переоткрытия: состояние и кандидат.
    pub item: Option<(ItemState, Option<String>)>,
    pub delivery: Delivery,
    pub notes_after: &'a [RemoteNote],
    pub drafts_after: &'a [RemoteDraft],
    pub recovered: usize,
}

/// Заметки автора с текстом `body` на новой строке `line`.
fn notes_at<'n>(notes: &'n [RemoteNote], line: u32, body: &str) -> Vec<&'n RemoteNote> {
    notes
        .iter()
        .filter(|note| {
            !note.system
                && note.body == body
                && note
                    .position
                    .as_ref()
                    .and_then(|position| position.new_line)
                    == Some(line)
        })
        .collect()
}

/// Оракул `reconcile-ambiguous` на живом хосте (ожидание — в отчёте).
pub fn ambiguous_checks(facts: &AmbiguousFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in facts.writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера: создания 2, публикации 2, повторов нет",
        counts,
        [(DRAFT_CREATE.to_owned(), 2), (DRAFT_PUBLISH.to_owned(), 2)]
            .into_iter()
            .collect(),
    );
    let created: Vec<&str> = facts
        .writes
        .iter()
        .filter_map(|write| write.created.as_deref())
        .collect();
    let published: Vec<&str> = facts
        .writes
        .iter()
        .filter(|write| write.key == DRAFT_PUBLISH)
        .filter_map(|write| write.draft.as_deref())
        .collect();
    checks.eq(
        "публикации по ID из ответов на создание",
        published,
        created.clone(),
    );
    checks.eq(
        "инъекция: отброшен ответ на вторую публикацию",
        facts.lost,
        created.get(1).copied(),
    );
    let same: BTreeSet<&str> = notes_at(facts.notes_after, 4, AMBIGUOUS_NOTE)
        .iter()
        .map(|note| note.id.as_str())
        .collect();
    checks.check(
        "сверка: «неоднозначно», кандидаты — обе одинаковые заметки",
        matches!(facts.reconciled, [Reconciled::Ambiguous { candidates }]
            if candidates.len() == 2
                && candidates.iter().map(ProviderId::as_str).collect::<BTreeSet<_>>() == same),
        format!("{:?}, заметки {same:?}", facts.reconciled),
    );
    checks.eq(
        "журнал: состояния",
        facts.states.to_vec(),
        vec![
            (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
            (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
            (Operation::GitLabPublishDraftNote, ItemState::Confirmed),
            (Operation::GitLabPublishDraftNote, ItemState::Unknown),
        ],
    );
    checks.eq(
        "журнал: вторая публикация unknown без кандидата, итог «частично»",
        (facts.item.clone(), facts.delivery),
        (Some((ItemState::Unknown, None)), Delivery::Partial),
    );
    checks.eq(
        "автор: две одинаковые заметки на новой строке 4 и других нет",
        facts
            .notes_after
            .iter()
            .filter(|note| !note.system)
            .map(|note| {
                (
                    note.body.clone(),
                    note.position
                        .as_ref()
                        .and_then(|position| position.new_line),
                )
            })
            .collect::<Vec<_>>(),
        vec![(AMBIGUOUS_NOTE.to_owned(), Some(4)); 2],
    );
    checks.eq(
        "черновиков ревьюера после отправки",
        facts.drafts_after.len(),
        0,
    );
    checks.eq("восстановленные элементы", facts.recovered, 0);
    checks
}

/// Неудачное чтение сверки («не выяснено» с текстом «чтение … не удалось») — по [`unread_verdict`].
fn unread_of(reconciled: &[Reconciled], window: &[Call]) -> Option<Verdict> {
    reconciled.iter().find_map(|verdict| match verdict {
        Reconciled::Unresolved { detail } if detail.starts_with("чтение ") => {
            Some(unread_verdict(detail, window, ACTOR_REVIEWER))
        }
        _ => None,
    })
}

/// Состояние и кандидат элемента журнала.
fn item_facts(
    scope: &crate::journal::Scope<'_>,
    item: Option<crate::journal::ItemId>,
) -> Result<Option<(ItemState, Option<String>)>, Verdict> {
    match item {
        Some(id) => {
            let item = scope.item(id).map_err(|err| stand("элемент", &err))?;
            Ok(Some((
                item.state,
                item.candidate
                    .map(|candidate| candidate.as_str().to_owned()),
            )))
        }
        None => Ok(None),
    }
}

fn gl_reconcile_ambiguous<T: Transport>(
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
    let (_, _, before) = mr_state(&author, &reviewer)?;
    let path = cx.state_dir.join("journal-reconcile-ambiguous.db");
    let project = ProjectId::new(LIVE_PROJECT);
    let mut notes = vec![format!(
        "MR клетки: версия {}",
        snapshot.version_id.as_str()
    )];
    let start = cx.trace.len();
    let hook = hooked(cx, &review, OnPublish::LoseResponse, 1)?;
    let (submission, lost, sent, fired, reconciled, reconcile_start) = {
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
        add_drafts(&scope, id, &snapshot.files, &AMBIGUOUS_DRAFTS)
            .map_err(|err| stand("черновики", &err))?;
        let submission =
            plan_gitlab(&scope, id, &GitLabPlan::default()).map_err(|err| stand("план", &err))?;
        let sent =
            run_gitlab(&scope, submission.id, &hook, &caps).map_err(|err| submit_verdict(&err))?;
        let lost = sent
            .items
            .iter()
            .filter(|item| item.operation == Operation::GitLabPublishDraftNote)
            .nth(1)
            .map(|item| item.id);
        let fired = match hook.transport().fired.borrow_mut().take() {
            Some(Fired { outcome: Ok(draft) }) => Some(draft),
            Some(Fired {
                outcome: Err(verdict),
            }) => return Err(verdict),
            None => None,
        };
        let at = cx.trace.len();
        let reconciled = match fired {
            Some(_) => reconcile_gitlab(&scope, submission.id, &reviewer)
                .map_err(|err| stand("сверка", &err))?,
            None => Vec::new(),
        };
        (submission.id, lost, sent, fired, reconciled, at)
    };
    let calls = cx.trace.calls();
    let window = calls.get(start..).unwrap_or_default();
    let reconcile_window = calls.get(reconcile_start..).unwrap_or_default();
    let journal = Journal::open(&path).map_err(|err| stand("журнал после переоткрытия", &err))?;
    let scope = journal.scope(&project);
    let dump = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
    let done = report(&scope, submission).map_err(|err| stand("итог отправки", &err))?;
    let item_states = states(&scope, submission).map_err(|err| stand("состояния", &err))?;
    let item = item_facts(&scope, lost)?;
    let (notes_after, drafts_after, after) = mr_state(&author, &reviewer)?;
    let early = |verdict: Verdict, notes: Vec<String>| Observed {
        notes,
        before: before.clone(),
        after: after.clone(),
        journal: dump.clone(),
        tier: caps.plan.clone(),
        verdict: Some(verdict),
        ..Observed::default()
    };
    // Инъекция не сработала (вторая публикация не получила 2xx): итог по общему правилу, прочий
    // отказ хоста судят проверки.
    if fired.is_none()
        && let Some(verdict) = delivery_verdict(&sent, window, ACTOR_REVIEWER)
    {
        return Ok(early(verdict, notes));
    }
    if let Some(lost_draft) = &fired {
        let same = notes_at(&notes_after, 4, AMBIGUOUS_NOTE).len();
        if same < 2 {
            return Ok(early(
                Verdict::NotRun(format!(
                    "публикация с потерянным ответом не наблюдена чтением автора: одинаковых заметок {same}"
                )),
                notes,
            ));
        }
        if let Some(verdict) = unread_of(&reconciled, reconcile_window) {
            return Ok(early(verdict, notes));
        }
        notes.push(format!(
            "инъекция: ответ на публикацию черновика {lost_draft} получен и отброшен, клиенту отдан таймаут"
        ));
    }
    let checks = ambiguous_checks(&AmbiguousFacts {
        writes: &writes_of(window, ACTOR_REVIEWER),
        lost: fired.as_deref(),
        reconciled: &reconciled,
        states: &item_states,
        item,
        delivery: done.delivery,
        notes_after: &notes_after,
        drafts_after: &drafts_after,
        recovered: journal.recovered().len(),
    });
    Ok(Observed {
        checks,
        notes,
        before,
        after,
        journal: dump,
        tier: caps.plan,
        verdict: None,
    })
}

/// Черновик `journal-crash` при сбое до запроса: новая строка 4 («line 05 changed»).
pub const CRASH_BEFORE: [(Spot, &str); 1] = [(Spot::Line(Side::New, 4), "до запроса")];
/// Черновик `journal-crash` при сбое после доставки: новая строка 24 (24a).
pub const CRASH_AFTER: [(Spot, &str); 1] = [(Spot::Line(Side::New, 24), "после доставки")];

/// Транспорт ревьюера `journal-crash`: после доставки создания черновика с ответом 2xx поток
/// исполнителя падает — процесс умер между принятой записью и записью ответа в журнал. Стоит над
/// сторожем, поэтому вызов уже в следе. Ответ не 2xx возвращается как есть, сбоя нет.
pub struct CrashAfterCreate<R> {
    pub inner: R,
    pub fired: std::cell::Cell<bool>,
}

impl<R: Transport> Transport for CrashAfterCreate<R> {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let response = self.inner.send(request);
        let body = request.body_json();
        let create = write_key(request.method, request.path(), body.as_ref()).as_deref()
            == Some(DRAFT_CREATE);
        if create && !self.fired.get() && matches!(&response, Ok(reply) if reply.is_success()) {
            self.fired.set(true);
            std::panic::resume_unwind(Box::new("инъекция пробы: сбой после доставки записи"));
        }
        response
    }
}

/// Наблюдённое `journal-crash` для оракула: фаза до запроса и фаза после доставки.
#[derive(Clone, Debug)]
pub struct CrashFacts<'a> {
    pub before_writes: &'a [Write],
    /// Восстановлен ровно элемент, переведённый в `in_flight`: итог и подробности.
    pub before_recovered: (bool, String),
    pub before_reconciled: &'a [Reconciled],
    pub before_item: Option<(ItemState, Option<String>)>,
    pub crashed: bool,
    pub after_writes: &'a [Write],
    pub after_recovered: (bool, String),
    pub after_reconciled: &'a [Reconciled],
    pub after_item: Option<(ItemState, Option<String>)>,
    pub drafts_after: &'a [RemoteDraft],
    pub notes_after: &'a [RemoteNote],
}

/// Оракул `journal-crash` на живом хосте (ожидание — в отчёте).
pub fn crash_checks(facts: &CrashFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    checks.eq(
        "до запроса: записей ревьюера нет",
        facts
            .before_writes
            .iter()
            .map(|write| write.key.clone())
            .collect::<Vec<_>>(),
        Vec::<String>::new(),
    );
    checks.check(
        "до запроса: восстановлен элемент in_flight",
        facts.before_recovered.0,
        facts.before_recovered.1.clone(),
    );
    checks.check(
        "до запроса: сверка «не выяснено»",
        matches!(facts.before_reconciled, [Reconciled::Unresolved { .. }]),
        format!("{:?}", facts.before_reconciled),
    );
    checks.eq(
        "до запроса: элемент unknown без кандидата",
        facts.before_item.clone(),
        Some((ItemState::Unknown, None)),
    );
    checks.check("после доставки: поток исполнителя упал", facts.crashed, "");
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in facts.after_writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    checks.eq(
        "после доставки: записи ревьюера — одно создание, повтора нет",
        counts,
        [(DRAFT_CREATE.to_owned(), 1)].into_iter().collect(),
    );
    let created = facts
        .after_writes
        .iter()
        .find_map(|write| write.created.clone());
    checks.check(
        "после доставки: восстановлен элемент создания",
        facts.after_recovered.0,
        facts.after_recovered.1.clone(),
    );
    checks.check(
        "после доставки: сверка даёт кандидата — записанный черновик",
        matches!(facts.after_reconciled, [Reconciled::Candidate { remote, .. }]
            if Some(remote.as_str()) == created.as_deref()),
        format!("{:?}, создан {created:?}", facts.after_reconciled),
    );
    checks.eq(
        "после доставки: элемент unknown с кандидатом, а не confirmed",
        facts.after_item.clone(),
        created.clone().map(|id| (ItemState::Unknown, Some(id))),
    );
    checks.eq(
        "черновики ревьюера — записанный после доставки",
        facts
            .drafts_after
            .iter()
            .map(|draft| (draft.id.as_str().to_owned(), draft.note.clone()))
            .collect::<Vec<_>>(),
        created
            .map(|id| (id, CRASH_AFTER[0].1.to_owned()))
            .into_iter()
            .collect::<Vec<_>>(),
    );
    checks.eq(
        "автор: заметок нет",
        facts.notes_after.iter().filter(|note| !note.system).count(),
        0,
    );
    checks
}

/// Сессия с черновиками и план без сводки в журнале `path`: ID отправки и её первого элемента.
fn crash_plan<T: Transport>(
    cx: &GlCx<'_, T>,
    review: &ReviewTarget,
    snapshot: &MergeSnapshot,
    path: &Path,
    drafts: &[(Spot, &str)],
) -> Result<(crate::journal::SubmissionId, crate::journal::ItemId), Verdict> {
    let journal = Journal::open(path).map_err(|err| stand("журнал", &err))?;
    let scope = journal.scope(&ProjectId::new(LIVE_PROJECT));
    let session = ReviewSession::new(
        &cx.reviewer_login,
        review.clone(),
        snapshot.revision.clone(),
    );
    let id = scope
        .create_session(&session)
        .map_err(|err| stand("сессия", &err))?;
    add_drafts(&scope, id, &snapshot.files, drafts).map_err(|err| stand("черновики", &err))?;
    let submission =
        plan_gitlab(&scope, id, &GitLabPlan::default()).map_err(|err| stand("план", &err))?;
    let first = submission
        .items
        .first()
        .map(|item| item.id)
        .ok_or_else(|| Verdict::Error("в плане нет элементов".to_owned()))?;
    Ok((submission.id, first))
}

/// Переоткрытый журнал после сбоя: восстановленные, сверка, элемент, дамп и окно сверки.
struct Reopened {
    recovered: (bool, String),
    reconciled: Vec<Reconciled>,
    item: Option<(ItemState, Option<String>)>,
    dump: Value,
    reconcile_start: usize,
}

fn reopen_and_reconcile<T: Transport>(
    cx: &GlCx<'_, T>,
    reviewer: &GitLab<&Guarded<T>>,
    path: &Path,
    submission: crate::journal::SubmissionId,
    first: crate::journal::ItemId,
) -> Result<Reopened, Verdict> {
    let project = ProjectId::new(LIVE_PROJECT);
    let journal = Journal::open(path).map_err(|err| stand("журнал после сбоя", &err))?;
    let recovered = journal.recovered().to_vec();
    let scope = journal.scope(&project);
    let reconcile_start = cx.trace.len();
    let reconciled =
        reconcile_gitlab(&scope, submission, reviewer).map_err(|err| stand("сверка", &err))?;
    let item = item_facts(&scope, Some(first))?;
    let dump = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
    Ok(Reopened {
        recovered: (
            recovered == [first],
            format!("{recovered:?}, ждали {first:?}"),
        ),
        reconciled,
        item,
        dump,
        reconcile_start,
    })
}

fn gl_journal_crash<T: Transport>(cx: &GlCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = gitlab_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = gitlab_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_snapshot(&reviewer, &cx.poll, None, MR_NOT_READY)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| read_or_fail("возможности ревьюера", &err))?;
    let notes = vec![format!(
        "MR клетки: версия {}",
        snapshot.version_id.as_str()
    )];
    let (_, _, initial) = mr_state(&author, &reviewer)?;
    // Сбой до запроса: элемент переведён в in_flight, журнал закрыт без запроса.
    let before_path = cx.state_dir.join("journal-journal-crash-before-request.db");
    let before_start = cx.trace.len();
    let (before_submission, before_first) =
        crash_plan(cx, &review, &snapshot, &before_path, &CRASH_BEFORE)?;
    {
        let journal = Journal::open(&before_path).map_err(|err| stand("журнал", &err))?;
        journal
            .scope(&ProjectId::new(LIVE_PROJECT))
            .begin(before_first)
            .map_err(|err| stand("in_flight", &err))?;
    }
    let before =
        reopen_and_reconcile(cx, &reviewer, &before_path, before_submission, before_first)?;
    let calls = cx.trace.calls();
    let before_writes = writes_of(
        calls.get(before_start..).unwrap_or_default(),
        ACTOR_REVIEWER,
    );
    let (_, _, between) = mr_state(&author, &reviewer)?;
    let early_before = |verdict: Verdict| Observed {
        notes: notes.clone(),
        before: json!({ "before-request": initial }),
        after: json!({ "before-request": between }),
        journal: json!({ "before-request": before.dump }),
        tier: caps.plan.clone(),
        verdict: Some(verdict),
        ..Observed::default()
    };
    let before_window = calls.get(before.reconcile_start..).unwrap_or_default();
    if let Some(verdict) = unread_of(&before.reconciled, before_window) {
        return Ok(early_before(verdict));
    }
    // Сбой после принятой записи: поток исполнителя падает после доставки создания с ответом 2xx.
    let after_path = cx.state_dir.join("journal-journal-crash-after-delivery.db");
    let (after_submission, after_first) =
        crash_plan(cx, &review, &snapshot, &after_path, &CRASH_AFTER)?;
    let after_start = cx.trace.len();
    let crash = GitLab::new(
        CrashAfterCreate {
            inner: cx.reviewer,
            fired: std::cell::Cell::new(false),
        },
        cx.reviewer.target().gitlab_endpoints(),
        &review,
    )
    .map_err(|err| stand("адаптер", &err))?;
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let journal = Journal::open(&after_path).map_err(|err| stand("журнал", &err))?;
        let scope = journal.scope(&ProjectId::new(LIVE_PROJECT));
        run_gitlab(&scope, after_submission, &crash, &caps).map_err(|err| submit_verdict(&err))
    }));
    let crashed = crash.transport().fired.get();
    let sent = match ran {
        Ok(Ok(sent)) => Some(sent),
        Ok(Err(verdict)) => return Err(verdict),
        Err(_) if crashed => None,
        Err(_) => return Err(Verdict::Error("исполнитель упал не по инъекции".to_owned())),
    };
    let (_, drafts_mid, _) = mr_state(&author, &reviewer)?;
    let after = reopen_and_reconcile(cx, &reviewer, &after_path, after_submission, after_first)?;
    let calls = cx.trace.calls();
    let after_window = calls.get(after_start..).unwrap_or_default();
    let (notes_after, drafts_after, last) = mr_state(&author, &reviewer)?;
    let early = |verdict: Verdict| Observed {
        notes: notes.clone(),
        before: json!({ "before-request": initial, "after-delivery": between }),
        after: json!({ "before-request": between, "after-delivery": last }),
        journal: json!({ "before-request": before.dump, "after-delivery": after.dump }),
        tier: caps.plan.clone(),
        verdict: Some(verdict),
        ..Observed::default()
    };
    // Инъекция не сработала (создание не получило 2xx): итог по общему правилу, прочий отказ
    // хоста судят проверки.
    if let Some(sent) = &sent
        && let Some(verdict) = delivery_verdict(sent, after_window, ACTOR_REVIEWER)
    {
        return Ok(early(verdict));
    }
    let mut notes = notes.clone();
    if crashed {
        let written = drafts_mid
            .iter()
            .filter(|draft| draft.note == CRASH_AFTER[0].1)
            .count();
        if written != 1 {
            return Ok(early(Verdict::NotRun(format!(
                "запись после доставки не наблюдена чтением ревьюера: черновиков «{}» {written}",
                CRASH_AFTER[0].1
            ))));
        }
        let after_reconcile = calls.get(after.reconcile_start..).unwrap_or_default();
        if let Some(verdict) = unread_of(&after.reconciled, after_reconcile) {
            return Ok(early(verdict));
        }
        notes.push(
            "инъекция: поток исполнителя упал после ответа 2xx на создание черновика, \
ответ в журнал не записан"
                .to_owned(),
        );
    }
    let checks = crash_checks(&CrashFacts {
        before_writes: &before_writes,
        before_recovered: before.recovered.clone(),
        before_reconciled: &before.reconciled,
        before_item: before.item.clone(),
        crashed,
        after_writes: &writes_of(after_window, ACTOR_REVIEWER),
        after_recovered: after.recovered.clone(),
        after_reconciled: &after.reconciled,
        after_item: after.item.clone(),
        drafts_after: &drafts_after,
        notes_after: &notes_after,
    });
    Ok(Observed {
        checks,
        notes,
        before: json!({ "before-request": initial, "after-delivery": between }),
        after: json!({ "before-request": between, "after-delivery": last }),
        journal: json!({ "before-request": before.dump, "after-delivery": after.dump }),
        tier: caps.plan,
        verdict: None,
    })
}

/// Черновик `reconcile-paged`: текст с хвостовыми пробелами и `\r\n` на новой строке 4
/// («line 05 changed»).
pub const PAGED_BODY: &str = "текст ревью  \r\n";
pub const PAGED_LINE: u32 = 4;
pub const PAGED_DRAFTS: [(Spot, &str); 1] = [(Spot::Line(Side::New, PAGED_LINE), PAGED_BODY)];
/// Размер страницы списков адаптера GitLab.
const PAGE_SIZE: usize = 100;

/// Наблюдённое `reconcile-paged` для оракула.
#[derive(Clone, Debug)]
pub struct PagedFacts<'a> {
    /// Записи ревьюера от отправки до конца сверки.
    pub writes: &'a [Write],
    /// ID в пути публикации, ответ на которую отброшен.
    pub lost: Option<&'a str>,
    /// Ответы на чтение второй страницы тредов ревьюером в окне сверки.
    pub second_pages: &'a [PageRead],
    pub reconciled: &'a [Reconciled],
    pub states: &'a [(Operation, ItemState)],
    /// Элемент публикации после переоткрытия: состояние и кандидат.
    pub item: Option<(ItemState, Option<String>)>,
    pub delivery: Delivery,
    pub notes_after: &'a [RemoteNote],
    pub drafts_after: &'a [RemoteDraft],
    pub recovered: usize,
}

/// Ответ на чтение страницы тредов: число тредов и ID их заметок.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageRead {
    pub threads: usize,
    pub notes: BTreeSet<String>,
}

/// Не системные заметки автора на новой строке `line`, текст которых после нормализации сверки
/// совпадает с `body`.
fn normalized_at<'n>(notes: &'n [RemoteNote], line: u32, body: &str) -> Vec<&'n RemoteNote> {
    let wanted = normalize_body(body);
    notes
        .iter()
        .filter(|note| {
            !note.system
                && normalize_body(&note.body) == wanted
                && note
                    .position
                    .as_ref()
                    .and_then(|position| position.new_line)
                    == Some(line)
        })
        .collect()
}

/// Ответы на чтение страницы `page` тредов MR (`GET …/discussions?…&page=N`) участником `actor`.
pub fn discussion_pages(calls: &[Call], actor: &str, page: u32) -> Vec<PageRead> {
    let wanted = format!("page={page}");
    calls
        .iter()
        .filter(|call| call.actor == actor && call.request.method == Method::Get)
        .filter(|call| {
            call.request
                .path()
                .split_once('?')
                .is_some_and(|(path, query)| {
                    path.ends_with("/discussions") && query.split('&').any(|pair| pair == wanted)
                })
        })
        .filter_map(|call| call.outcome.as_ref().ok())
        .filter(|response| response.is_success())
        .filter_map(|response| response.body_json().ok())
        .filter_map(|value| value.as_array().cloned())
        .map(|threads| PageRead {
            threads: threads.len(),
            notes: threads
                .iter()
                .filter_map(|thread| thread.get("notes").and_then(Value::as_array))
                .flatten()
                .filter_map(|note| note.get("id"))
                .map(id_text)
                .collect(),
        })
        .collect()
}

/// Как хост сохранил текст черновика `reconcile-paged`.
pub fn body_form(seen: Option<&str>) -> &'static str {
    match seen {
        None => "не прочитан",
        Some(text) if text == PAGED_BODY => "как есть",
        Some(text) if text == normalize_body(PAGED_BODY) => {
            "нормализован (без \\r и хвостовых пробелов)"
        }
        Some(_) => "изменён иначе",
    }
}

/// Оракул `reconcile-paged` на живом хосте (ожидание — в отчёте).
pub fn paged_checks(facts: &PagedFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for write in facts.writes {
        *counts.entry(write.key.clone()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера: создание 1, публикация 1, повторов нет",
        counts,
        [(DRAFT_CREATE.to_owned(), 1), (DRAFT_PUBLISH.to_owned(), 1)]
            .into_iter()
            .collect(),
    );
    let created: Vec<&str> = facts
        .writes
        .iter()
        .filter_map(|write| write.created.as_deref())
        .collect();
    let published: Vec<&str> = facts
        .writes
        .iter()
        .filter(|write| write.key == DRAFT_PUBLISH)
        .filter_map(|write| write.draft.as_deref())
        .collect();
    checks.eq(
        "публикация по ID из ответа на создание",
        published,
        created.clone(),
    );
    checks.eq(
        "инъекция: отброшен ответ на публикацию",
        facts.lost,
        created.first().copied(),
    );
    checks.check(
        "сверка читает вторую страницу тредов",
        !facts.second_pages.is_empty(),
        format!("{:?}", facts.second_pages),
    );
    let ours: Vec<&str> = normalized_at(facts.notes_after, PAGED_LINE, PAGED_BODY)
        .iter()
        .map(|note| note.id.as_str())
        .collect();
    let single = match ours[..] {
        [id] => Some(id.to_owned()),
        _ => None,
    };
    checks.check(
        "сверка: кандидат — заметка автора с текстом черновика",
        matches!(facts.reconciled, [Reconciled::Candidate { remote, .. }]
            if single.as_deref() == Some(remote.as_str())),
        format!("{:?}, заметки {ours:?}", facts.reconciled),
    );
    checks.eq(
        "журнал: состояния",
        facts.states.to_vec(),
        vec![
            (Operation::GitLabCreateDraftNote, ItemState::Confirmed),
            (Operation::GitLabPublishDraftNote, ItemState::Unknown),
        ],
    );
    checks.eq(
        "журнал: публикация unknown с кандидатом, итог «частично»",
        (facts.item.clone(), facts.delivery),
        (Some((ItemState::Unknown, single)), Delivery::Partial),
    );
    let visible: Vec<&RemoteNote> = facts.notes_after.iter().filter(|n| !n.system).collect();
    let planted = visible
        .iter()
        .filter(|note| note.position.is_none() && note.body.starts_with(PLANTED_NOTE))
        .count();
    checks.eq(
        "автор: 100 заметок второго клиента и одна своя, других нет",
        (planted, ours.len(), visible.len()),
        (PLANTED_COUNT as usize, 1, PLANTED_COUNT as usize + 1),
    );
    checks.eq(
        "черновиков ревьюера после отправки",
        facts.drafts_after.len(),
        0,
    );
    checks.eq("восстановленные элементы", facts.recovered, 0);
    checks
}

fn gl_reconcile_paged<T: Transport>(
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
    let planted = cx.second.plant_notes(cell.number, PLANTED_COUNT)?;
    let (_, _, before) = mr_state(&author, &reviewer)?;
    let path = cx.state_dir.join("journal-reconcile-paged.db");
    let project = ProjectId::new(LIVE_PROJECT);
    let mut notes = vec![
        format!("MR клетки: версия {}", snapshot.version_id.as_str()),
        format!("второй клиент: заметок {PLANTED_COUNT}, последняя {planted}"),
    ];
    let start = cx.trace.len();
    let hook = hooked(cx, &review, OnPublish::LoseResponse, 0)?;
    let (submission, lost, sent, fired, reconciled, reconcile_start) = {
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
        add_drafts(&scope, id, &snapshot.files, &PAGED_DRAFTS)
            .map_err(|err| stand("черновики", &err))?;
        let submission =
            plan_gitlab(&scope, id, &GitLabPlan::default()).map_err(|err| stand("план", &err))?;
        let sent =
            run_gitlab(&scope, submission.id, &hook, &caps).map_err(|err| submit_verdict(&err))?;
        let lost = sent
            .items
            .iter()
            .find(|item| item.operation == Operation::GitLabPublishDraftNote)
            .map(|item| item.id);
        let fired = match hook.transport().fired.borrow_mut().take() {
            Some(Fired { outcome: Ok(draft) }) => Some(draft),
            Some(Fired {
                outcome: Err(verdict),
            }) => return Err(verdict),
            None => None,
        };
        let at = cx.trace.len();
        let reconciled = match fired {
            Some(_) => reconcile_gitlab(&scope, submission.id, &reviewer)
                .map_err(|err| stand("сверка", &err))?,
            None => Vec::new(),
        };
        (submission.id, lost, sent, fired, reconciled, at)
    };
    let calls = cx.trace.calls();
    let window = calls.get(start..).unwrap_or_default();
    let reconcile_window = calls.get(reconcile_start..).unwrap_or_default();
    let journal = Journal::open(&path).map_err(|err| stand("журнал после переоткрытия", &err))?;
    let scope = journal.scope(&project);
    let dump = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
    let done = report(&scope, submission).map_err(|err| stand("итог отправки", &err))?;
    let item_states = states(&scope, submission).map_err(|err| stand("состояния", &err))?;
    let item = item_facts(&scope, lost)?;
    let (notes_after, drafts_after, after) = mr_state(&author, &reviewer)?;
    let early = |verdict: Verdict, notes: Vec<String>| Observed {
        notes,
        before: before.clone(),
        after: after.clone(),
        journal: dump.clone(),
        tier: caps.plan.clone(),
        verdict: Some(verdict),
        ..Observed::default()
    };
    // Инъекция не сработала (публикация не получила 2xx): итог по общему правилу, прочий отказ
    // хоста судят проверки.
    if fired.is_none()
        && let Some(verdict) = delivery_verdict(&sent, window, ACTOR_REVIEWER)
    {
        return Ok(early(verdict, notes));
    }
    let second_pages = discussion_pages(reconcile_window, ACTOR_REVIEWER, 2);
    if let Some(lost_draft) = &fired {
        let ours = normalized_at(&notes_after, PAGED_LINE, PAGED_BODY);
        let Some(own) = ours.first() else {
            return Ok(early(
                Verdict::NotRun(
                    "публикация с потерянным ответом не наблюдена чтением автора: заметки с текстом черновика на новой строке 4 нет"
                        .to_owned(),
                ),
                notes,
            ));
        };
        if let Some(verdict) = unread_of(&reconciled, reconcile_window) {
            return Ok(early(verdict, notes));
        }
        let created_body = window
            .iter()
            .filter(|call| call.actor == ACTOR_REVIEWER)
            .find(|call| {
                write_key(
                    call.request.method,
                    call.request.path(),
                    call.request.body_json().as_ref(),
                )
                .as_deref()
                    == Some(DRAFT_CREATE)
            })
            .and_then(|call| call.outcome.as_ref().ok())
            .and_then(|response| response.body_json().ok())
            .and_then(|value| value.get("note").and_then(Value::as_str).map(str::to_owned));
        notes.push(format!(
            "текст черновика: в ответе на создание — {}, в заметке автора — {}",
            body_form(created_body.as_deref()),
            body_form(Some(&own.body))
        ));
        notes.push(format!(
            "вторая страница тредов в сверке: {}",
            if second_pages.is_empty() {
                "не прочитана".to_owned()
            } else {
                second_pages
                    .iter()
                    .map(|page| {
                        format!(
                            "тредов {}, заметка автора {}",
                            page.threads,
                            if page.notes.contains(own.id.as_str()) {
                                "есть"
                            } else {
                                "нет"
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            }
        ));
        if !second_pages.is_empty()
            && !second_pages
                .iter()
                .any(|page| page.threads < PAGE_SIZE && page.notes.contains(own.id.as_str()))
        {
            let last = notes.last().cloned().unwrap_or_default();
            return Ok(early(
                Verdict::NotRun(format!(
                    "кандидат не на неполной второй странице: {}",
                    last.trim_start_matches("вторая страница тредов в сверке: ")
                )),
                notes,
            ));
        }
        notes.push(format!(
            "инъекция: ответ на публикацию черновика {lost_draft} получен и отброшен, клиенту отдан таймаут"
        ));
    }
    let checks = paged_checks(&PagedFacts {
        writes: &writes_of(window, ACTOR_REVIEWER),
        lost: fired.as_deref(),
        second_pages: &second_pages,
        reconciled: &reconciled,
        states: &item_states,
        item,
        delivery: done.delivery,
        notes_after: &notes_after,
        drafts_after: &drafts_after,
        recovered: journal.recovered().len(),
    });
    Ok(Observed {
        checks,
        notes,
        before,
        after,
        journal: dump,
        tier: caps.plan,
        verdict: None,
    })
}

#[cfg(test)]
mod tests;
