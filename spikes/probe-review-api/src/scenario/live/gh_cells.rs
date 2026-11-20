//! Клетки строк GitHub в живом столбце `github_cloud`: обвязка общая с GitLab
//! ([`run_live_cell`]), сценарии строк. Ожидания — в отчёте, «Живые клетки GitHub».
//!
//! Оракул живой клетки не видит полей `forge-fake` (`origin`): записи пробы берутся из следа
//! ревьюера в окне отправки, состояние — из чтения автором reviews PR и их комментариев.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use serde_json::{Value, json};

use super::Verdict;
use super::github_adapter;
use super::gl_cells::{LiveRoles, Observed, Poll, WRITE_UNOBSERVED, run_live_cell, stand};
use crate::fake::{GraphqlKind, graphql_root};
use crate::github::{
    ApiError, Capabilities, GitHub, PullSnapshot, RemoteComment, RemoteReview, ReviewEvent,
    ViewedState, is_rate_limited,
};
use crate::journal::{ItemState, Journal, Operation, ProjectId};
use crate::live::guard::Guarded;
use crate::live::lifecycle::Cell;
use crate::model::anchor::Side;
use crate::model::{Provider, ReviewSession, ReviewTarget};
use crate::results::ResultFile;
use crate::scenario::Scenario;
use crate::scenario::fake::{Checks, Spot, add_drafts, dump_in};
use crate::scrub::Scrubber;
use crate::submit::{Delivery, Report, SubmitError, plan_github_local, report, run_github};
use crate::transport::{Call, Method, Trace, Transport};

/// Ключ записи Create review в следе.
pub const GH_CREATE: &str = "POST reviews";

const LIVE_PROJECT: &str = "live-github";
const ACTOR_REVIEWER: &str = "reviewer";
const SUMMARY: &str = "Итог ревью";
/// Черновики `gh-local-create`: строка 05 после правки (новая сторона, 4) и добавленная 24a
/// (новая сторона, 24).
const LOCAL_DRAFTS: [(Spot, &str); 2] = [
    (Spot::Line(Side::New, 4), "первый"),
    (Spot::Line(Side::New, 24), "второй"),
];
const LOCAL_EVENTS: [ReviewEvent; 3] = [
    ReviewEvent::Comment,
    ReviewEvent::Approve,
    ReviewEvent::RequestChanges,
];

/// Окружение клетки GitHub: сторожа ролей над общим следом.
pub struct GhCx<'a, T> {
    pub author: &'a Guarded<T>,
    pub reviewer: &'a Guarded<T>,
    pub trace: &'a Trace,
    /// Хост для `ReviewTarget.host`.
    pub host_url: String,
    /// Логин ревьюера: аккаунт сессии журнала и автор его reviews.
    pub reviewer_login: String,
    pub state_dir: &'a Path,
    pub poll: Poll,
}

impl<T: Transport> GhCx<'_, T> {
    fn review(&self, number: u64) -> ReviewTarget {
        ReviewTarget {
            provider: Provider::GitHub,
            host: self.host_url.clone(),
            repo: self.reviewer.target().repo.clone(),
            number,
        }
    }
}

/// Строки GitHub, у которых есть тело живой клетки. Остальные до открытия ветки и PR дают ошибку
/// «не реализован»: клетка без тела не пишет в песочницу.
pub fn implemented(scenario: Scenario) -> bool {
    matches!(scenario, Scenario::GhLocalCreate | Scenario::GhViewed)
}

/// Клетка строки GitHub: ветки и PR от автора, тело сценария, уборка в любом случае, итог.
pub fn run_gh_cell<T: Transport>(
    scenario: Scenario,
    cx: &GhCx<'_, T>,
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
            Scenario::GhLocalCreate => gh_local_create(cx, cell),
            Scenario::GhViewed => gh_viewed(cx, cell),
            other => Err(Verdict::Error(format!(
                "сценарий {} в живом столбце GitHub не реализован",
                other.name()
            ))),
        },
    );
}

/// Ключ записи GitHub в следе: хвост пути после `pulls/<номер>/` с числовыми ID, заменёнными на
/// `{id}`, или имя мутации GraphQL. Чтения (`GET`, запросы GraphQL) — `None`; запись вне PR —
/// `МЕТОД путь`, чтобы точное сравнение записей её не пропустило.
pub fn gh_write_key(method: Method, path: &str, body: Option<&Value>) -> Option<String> {
    if !method.is_write() {
        return None;
    }
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    if path.ends_with("/graphql") {
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
        .split_once("/pulls/")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, tail)| tail);
    match tail {
        Some(tail) => {
            let tail: Vec<&str> = tail
                .split('/')
                .map(|part| {
                    if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) {
                        "{id}"
                    } else {
                        part
                    }
                })
                .collect();
            Some(format!("{method} {}", tail.join("/")))
        }
        None => Some(format!("{method} {path}")),
    }
}

/// Чтение клетки: лимит — `not_run` «лимит», отказ транспорта и 5xx — ошибка исполнения, прочий
/// отказ хоста — `fail` (наблюдение).
pub fn gh_read_or_fail(what: &str, err: &ApiError) -> Verdict {
    match err {
        ApiError::RateLimited { status, detail } => {
            Verdict::NotRun(format!("лимит: {what}: код {status}: {detail}"))
        }
        ApiError::NotSent(_) | ApiError::Transport(_) => Verdict::Error(format!("{what}: {err}")),
        ApiError::Status { status, .. } if *status >= 500 => {
            Verdict::Error(format!("{what}: {err}"))
        }
        _ => Verdict::Fail(format!("{what}: {err}")),
    }
}

/// Отказ исполнителя отправки GitHub: чтение шага 1 судится как чтение клетки, остальное —
/// ошибка исполнения.
pub fn gh_submit_verdict(err: &SubmitError) -> Verdict {
    match err {
        SubmitError::GitHub(ApiError::Unexpected(reason)) => Verdict::NotRun(format!(
            "{PR_NOT_READY}: отправка, шаг 1: снимок PR: {reason}"
        )),
        SubmitError::GitHub(api) => gh_read_or_fail("отправка, шаг 1: снимок PR", api),
        other => Verdict::Error(format!("отправка: {other}")),
    }
}

/// Причина `not_run` клетки GitHub, чей новый PR не отдал файлы diff в срок.
pub const PR_NOT_READY: &str = "PR не готов за 120 с";

/// Итог отправки, который оракул записей не судит, как [`super::gl_cells::delivery_verdict`]:
/// `unknown` или `in_flight` — [`WRITE_UNOBSERVED`]; `failed` после явного отказа по лимиту
/// (429 или 403 с признаком лимита) на последнюю запись роли — `not_run` «лимит»; `failed`
/// после записи, которая не ушла, — ошибка исполнения; остальной `failed` судят проверки.
pub fn gh_delivery_verdict(done: &Report, calls: &[Call], actor: &str) -> Option<Verdict> {
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
        let key = gh_write_key(
            call.request.method,
            call.request.path(),
            call.request.body_json().as_ref(),
        )?;
        (call.actor == actor).then_some((call, key))
    })?;
    match &last.outcome {
        Ok(response) => {
            let message = response
                .body_json()
                .ok()
                .and_then(|body| {
                    body.get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_default();
            is_rate_limited(response, &message)
                .then(|| Verdict::NotRun(format!("лимит: код {} на запись {key}", response.status)))
        }
        Err(failure) if !failure.may_have_been_sent() => {
            Some(Verdict::Error(format!("запись {key} не ушла: {failure}")))
        }
        Err(_) => None,
    }
}

/// Снимок PR после асинхронного расчёта diff: ответ без файлов (или `Unexpected`) повторяется до
/// крайнего срока, затем `not_run` [`PR_NOT_READY`]. Другой отказ — по [`gh_read_or_fail`].
pub fn await_pull<T: Transport>(github: &GitHub<T>, poll: &Poll) -> Result<PullSnapshot, Verdict> {
    let start = Instant::now();
    let mut tries = 0u32;
    loop {
        tries += 1;
        let last = match github.pull() {
            Ok(snapshot) if snapshot.files.is_empty() => "в diff PR нет файлов".to_owned(),
            Ok(snapshot) => return Ok(snapshot),
            Err(ApiError::Unexpected(reason)) => reason,
            Err(err) => return Err(gh_read_or_fail("снимок PR", &err)),
        };
        if start.elapsed() >= poll.deadline {
            return Err(Verdict::NotRun(format!(
                "{PR_NOT_READY}: попыток {tries}, последний ответ: {last}"
            )));
        }
        std::thread::sleep(poll.pause);
    }
}

fn comment_json(comment: &RemoteComment) -> Value {
    json!({
        "id": comment.id.as_str(),
        "review_id": comment.review_id.as_ref().map(|id| id.as_str()),
        "user": comment.user,
        "body": comment.body,
        "path": comment.path,
        "line": comment.line,
        "side": comment.side,
        "start_line": comment.start_line,
        "start_side": comment.start_side,
        "commit_id": comment.commit_id,
        "in_reply_to": comment.in_reply_to.as_ref().map(|id| id.as_str()),
    })
}

/// Review и его комментарии по чтению автора.
pub type ReviewRead = (RemoteReview, Vec<RemoteComment>);

/// Reviews PR и их комментарии, прочитанные автором (`PENDING` ревьюера автору не виден).
fn pr_state<T: Transport>(author: &GitHub<T>) -> Result<(Vec<ReviewRead>, Value), Verdict> {
    let reviews = author
        .reviews()
        .map_err(|err| gh_read_or_fail("reviews PR автором", &err))?;
    let mut state = Vec::new();
    for review in reviews {
        let comments = author
            .review_comments(&review.id)
            .map_err(|err| gh_read_or_fail("комментарии review автором", &err))?;
        state.push((review, comments));
    }
    let value = json!({
        "reviews": state.iter().map(|(review, comments)| json!({
            "id": review.id.as_str(),
            "user": review.user,
            "state": review.state.name(),
            "body": review.body,
            "commit_id": review.commit_id,
            "comments": comments.iter().map(comment_json).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    });
    Ok((state, value))
}

/// Отправка ветви «только локальные черновики» в своём журнале.
struct LocalSent {
    delivery: Option<Verdict>,
    window: Vec<Call>,
    done: Report,
    recovered: usize,
    journal: Value,
}

fn submit_local<T: Transport, R: Transport>(
    cx: &GhCx<'_, T>,
    reviewer: &GitHub<R>,
    review: &ReviewTarget,
    snapshot: &PullSnapshot,
    caps: &Capabilities,
    event: ReviewEvent,
) -> Result<LocalSent, Verdict> {
    let path = cx.state_dir.join(format!(
        "journal-gh-local-create-{}.db",
        event.name().to_ascii_lowercase()
    ));
    let project = ProjectId::new(LIVE_PROJECT);
    let start = cx.trace.len();
    let (submission, sent) = {
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
        add_drafts(&scope, id, &snapshot.files, &LOCAL_DRAFTS)
            .map_err(|err| stand("черновики", &err))?;
        let submission =
            plan_github_local(&scope, id, event, SUMMARY).map_err(|err| stand("план", &err))?;
        let sent = run_github(&scope, submission.id, reviewer, caps)
            .map_err(|err| gh_submit_verdict(&err))?;
        (submission.id, sent)
    };
    let calls = cx.trace.calls();
    let window = calls.get(start..).unwrap_or_default().to_vec();
    let delivery = gh_delivery_verdict(&sent, &window, ACTOR_REVIEWER);
    let journal = Journal::open(&path).map_err(|err| stand("журнал после переоткрытия", &err))?;
    let scope = journal.scope(&project);
    let done = report(&scope, submission).map_err(|err| stand("итог отправки", &err))?;
    let dump = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
    Ok(LocalSent {
        delivery,
        window,
        done,
        recovered: journal.recovered().len(),
        journal: dump,
    })
}

/// Записи ветви `gh-local-create`: тела и коды ответов записей ревьюера в окне её отправки.
pub struct LocalBranch {
    pub event: ReviewEvent,
    /// Ключ, тело и ответ (код, ID review) каждой записи ревьюера.
    pub writes: Vec<(String, Value, Option<u16>, Option<String>)>,
    pub delivery: Delivery,
    pub items: Vec<(Operation, ItemState, Option<String>)>,
    pub recovered: usize,
}

fn local_branch(event: ReviewEvent, sent: &LocalSent) -> LocalBranch {
    let writes = sent
        .window
        .iter()
        .filter(|call| call.actor == ACTOR_REVIEWER)
        .filter_map(|call| {
            let body = call.request.body_json();
            let key = gh_write_key(call.request.method, call.request.path(), body.as_ref())?;
            let response = call.outcome.as_ref().ok();
            let id = response
                .filter(|response| response.is_success())
                .and_then(|response| response.body_json().ok())
                .and_then(|value| value.get("id").map(id_text));
            Some((
                key,
                body.unwrap_or(Value::Null),
                response.map(|response| response.status),
                id,
            ))
        })
        .collect();
    LocalBranch {
        event,
        writes,
        delivery: sent.done.delivery,
        items: sent
            .done
            .items
            .iter()
            .map(|item| {
                (
                    item.operation,
                    item.state,
                    item.remote_id.as_ref().map(|id| id.as_str().to_owned()),
                )
            })
            .collect(),
        recovered: sent.recovered,
    }
}

fn id_text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

/// Факты `gh-local-create` для оракула.
pub struct LocalFacts<'a> {
    pub head: &'a str,
    pub path: &'a str,
    pub reviewer: &'a str,
    pub branches: &'a [LocalBranch],
    /// Reviews PR и их комментарии по чтению автора после всех ветвей.
    pub after: &'a [ReviewRead],
}

/// Оракул `gh-local-create` на живом хосте (ожидание — в отчёте).
pub fn local_create_checks(facts: &LocalFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    let wanted_comments: Vec<(Option<&str>, Option<u64>, Option<&str>)> = LOCAL_DRAFTS
        .iter()
        .map(|(spot, _)| match spot {
            Spot::Line(_, line) => (Some(facts.path), Some(u64::from(*line)), Some("RIGHT")),
            _ => (None, None, None),
        })
        .collect();
    let mut review_ids = Vec::new();
    for branch in facts.branches {
        let name = branch.event.name().to_ascii_lowercase();
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (key, ..) in &branch.writes {
            *counts.entry(key.as_str()).or_insert(0) += 1;
        }
        checks.eq(
            &format!("{name}: записи ревьюера"),
            counts,
            [(GH_CREATE, 1)].into_iter().collect(),
        );
        if let [(_, body, status, id)] = &branch.writes[..] {
            checks.check(
                &format!("{name}: Create review принят"),
                status.is_some_and(|status| (200..300).contains(&status)),
                format!("{status:?}"),
            );
            checks.eq(
                &format!("{name}: event Create review"),
                body["event"].as_str(),
                Some(branch.event.name()),
            );
            checks.check(
                &format!("{name}: body Create review непустой"),
                body["body"].as_str().is_some_and(|b| !b.trim().is_empty()),
                body["body"].to_string(),
            );
            checks.eq(
                &format!("{name}: commit_id Create review"),
                body["commit_id"].as_str(),
                Some(facts.head),
            );
            let comments: Vec<(Option<&str>, Option<u64>, Option<&str>)> = body["comments"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .map(|c| (c["path"].as_str(), c["line"].as_u64(), c["side"].as_str()))
                        .collect()
                })
                .unwrap_or_default();
            checks.eq(
                &format!("{name}: comments[] Create review"),
                comments,
                wanted_comments.clone(),
            );
            review_ids.push(id.clone());
            checks.eq(
                &format!("{name}: элементы журнала"),
                branch.items.clone(),
                vec![(
                    Operation::GitHubCreateReview,
                    ItemState::Confirmed,
                    id.clone(),
                )],
            );
        }
        checks.eq(
            &format!("{name}: итог в журнале"),
            branch.delivery,
            Delivery::Full,
        );
        checks.eq(
            &format!("{name}: восстановленные элементы"),
            branch.recovered,
            0,
        );
    }
    let own: Vec<&ReviewRead> = facts
        .after
        .iter()
        .filter(|(review, _)| review.user.eq_ignore_ascii_case(facts.reviewer))
        .collect();
    checks.eq(
        "автор: reviews ревьюера (ID, состояние, commit_id)",
        own.iter()
            .map(|(review, _)| {
                (
                    Some(review.id.as_str().to_owned()),
                    review.state.name(),
                    review.commit_id.clone(),
                )
            })
            .collect::<Vec<_>>(),
        facts
            .branches
            .iter()
            .zip(review_ids.iter().chain(std::iter::repeat(&None)))
            .map(|(branch, id)| {
                (
                    id.clone(),
                    branch.event.state().name(),
                    facts.head.to_owned(),
                )
            })
            .collect::<Vec<_>>(),
    );
    let wanted: Vec<(String, Option<u32>, Option<String>, String)> = LOCAL_DRAFTS
        .iter()
        .filter_map(|(spot, body)| match spot {
            Spot::Line(_, line) => Some((
                facts.path.to_owned(),
                Some(*line),
                Some("RIGHT".to_owned()),
                (*body).to_owned(),
            )),
            _ => None,
        })
        .collect();
    for (review, comments) in &own {
        let mut seen: Vec<(String, Option<u32>, Option<String>, String)> = comments
            .iter()
            .map(|c| (c.path.clone(), c.line, c.side.clone(), c.body.clone()))
            .collect();
        seen.sort_by_key(|c| c.1);
        checks.eq(
            &format!("автор: комментарии review {}", review.state.name()),
            seen,
            wanted.clone(),
        );
    }
    checks
}

fn gh_local_create<T: Transport>(cx: &GhCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = github_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = github_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_pull(&reviewer, &cx.poll)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| gh_read_or_fail("возможности ревьюера", &err))?;
    let (_, before) = pr_state(&author)?;
    let mut branches = Vec::new();
    let mut journals = serde_json::Map::new();
    for event in LOCAL_EVENTS {
        let sent = submit_local(cx, &reviewer, &review, &snapshot, &caps, event)?;
        if let Some(verdict) = sent.delivery {
            return Err(verdict);
        }
        journals.insert(event.name().to_ascii_lowercase(), sent.journal.clone());
        branches.push(local_branch(event, &sent));
    }
    let (after_state, after) = pr_state(&author)?;
    let path = snapshot
        .files
        .first()
        .map(|file| file.new_path().to_owned())
        .unwrap_or_default();
    let checks = local_create_checks(&LocalFacts {
        head: snapshot.revision.head_sha().as_str(),
        path: &path,
        reviewer: &cx.reviewer_login,
        branches: &branches,
        after: &after_state,
    });
    Ok(Observed {
        checks,
        notes: Vec::new(),
        before,
        after,
        journal: Value::Object(journals),
        tier: caps.plan,
        verdict: None,
    })
}

/// Ключи записей `gh-viewed`.
pub const GH_MARK: &str = "graphql markFileAsViewed";
pub const GH_UNMARK: &str = "graphql unmarkFileAsViewed";

/// Отказ записи клетки, который проверки не судят: лимит — `not_run` «лимит», не ушедший запрос —
/// ошибка исполнения, итог неизвестен — [`WRITE_UNOBSERVED`]. Явный отказ хоста — `None`, его
/// судят проверки.
pub fn gh_write_verdict(key: &str, err: &ApiError) -> Option<Verdict> {
    match err {
        ApiError::RateLimited { status, .. } => Some(Verdict::NotRun(format!(
            "лимит: код {status} на запись {key}"
        ))),
        ApiError::NotSent(_) => Some(Verdict::Error(format!("запись {key} не ушла: {err}"))),
        _ if err.write_fate() == crate::github::WriteFate::Unknown => {
            Some(Verdict::NotRun(format!("{WRITE_UNOBSERVED}: {key}: {err}")))
        }
        _ => None,
    }
}

/// Состояние просмотра файла `path` по чтению роли.
fn viewed_state<T: Transport>(
    github: &GitHub<T>,
    pull: &crate::model::ProviderId,
    path: &str,
    who: &str,
) -> Result<Option<ViewedState>, Verdict> {
    github
        .viewed_files(pull)
        .map(|files| {
            files
                .into_iter()
                .find(|(file, _)| file == path)
                .map(|(_, state)| state)
        })
        .map_err(|err| gh_read_or_fail(&format!("просмотр файлов: {who}"), &err))
}

fn viewed_name(state: Option<ViewedState>) -> Value {
    json!(state.map(|state| format!("{state:?}").to_ascii_uppercase()))
}

/// Факты `gh-viewed` для оракула.
pub struct ViewedFacts<'a> {
    pub pull_node: &'a str,
    pub path: &'a str,
    /// Ключ, тело и код ответа каждой записи ревьюера в окне клетки.
    pub writes: &'a [(String, Value, Option<u16>)],
    /// Чтение ревьюером после `markFileAsViewed` и после `unmarkFileAsViewed`.
    pub after_mark: Option<ViewedState>,
    pub after_unmark: Option<ViewedState>,
    /// Сессии и отправки журнала после переоткрытия.
    pub sessions: usize,
    pub submissions: usize,
}

/// Оракул `gh-viewed` на живом хосте (ожидание — в отчёте). `viewerViewedState` принадлежит
/// токену, поэтому судит только чтение ревьюера.
pub fn viewed_checks(facts: &ViewedFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (key, ..) in facts.writes {
        *counts.entry(key.as_str()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера",
        counts,
        [(GH_MARK, 1), (GH_UNMARK, 1)].into_iter().collect(),
    );
    for (key, body, status) in facts.writes {
        checks.check(
            &format!("{key}: принят"),
            status.is_some_and(|status| (200..300).contains(&status)),
            format!("{status:?}"),
        );
        checks.eq(
            &format!("{key}: вход"),
            (
                body.pointer("/variables/input/pullRequestId")
                    .and_then(Value::as_str),
                body.pointer("/variables/input/path")
                    .and_then(Value::as_str),
            ),
            (Some(facts.pull_node), Some(facts.path)),
        );
    }
    checks.eq(
        "ревьюер после markFileAsViewed",
        facts.after_mark,
        Some(ViewedState::Viewed),
    );
    checks.eq(
        "ревьюер после unmarkFileAsViewed",
        facts.after_unmark,
        Some(ViewedState::Unviewed),
    );
    checks.eq(
        "журнал: сессия без отправок",
        (facts.sessions, facts.submissions),
        (1, 0),
    );
    checks
}

fn gh_viewed<T: Transport>(cx: &GhCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = github_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = github_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_pull(&reviewer, &cx.poll)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| gh_read_or_fail("возможности ревьюера", &err))?;
    let path = snapshot
        .files
        .first()
        .map(|file| file.new_path().to_owned())
        .ok_or_else(|| Verdict::Error("в diff PR нет файлов".to_owned()))?;
    let pull = &snapshot.node_id;
    let journal_path = cx.state_dir.join("journal-gh-viewed.db");
    let project = ProjectId::new(LIVE_PROJECT);
    {
        let journal = Journal::open(&journal_path).map_err(|err| stand("журнал", &err))?;
        let session = ReviewSession::new(
            &cx.reviewer_login,
            review.clone(),
            snapshot.revision.clone(),
        );
        journal
            .scope(&project)
            .create_session(&session)
            .map_err(|err| stand("сессия", &err))?;
    }
    let mut notes = Vec::new();
    let author_state =
        |notes: &mut Vec<String>, when: &str| match viewed_state(&author, pull, &path, "автор")
        {
            Ok(state) => notes.push(format!(
                "наблюдение: viewerViewedState автора {when}: {}",
                viewed_name(state)
            )),
            Err(verdict) => notes.push(format!(
                "наблюдение: чтение просмотра автором {when}: {verdict:?}"
            )),
        };
    let state_before = viewed_state(&reviewer, pull, &path, "ревьюер")?;
    let before =
        json!({"reviewer": {"path": path, "viewerViewedState": viewed_name(state_before)}});
    let dump = || -> Result<(Value, usize, usize), Verdict> {
        let journal =
            Journal::open(&journal_path).map_err(|err| stand("журнал после переоткрытия", &err))?;
        let scope = journal.scope(&project);
        let sessions = scope.sessions().map_err(|err| stand("журнал", &err))?;
        let mut submissions = 0;
        for (id, _) in &sessions {
            submissions += scope
                .submissions(*id)
                .map_err(|err| stand("журнал", &err))?
                .len();
        }
        let value = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
        Ok((value, sessions.len(), submissions))
    };
    if !caps.viewed() {
        let (journal, ..) = dump()?;
        return Ok(Observed {
            checks: Checks::default(),
            notes,
            after: before.clone(),
            before,
            journal,
            tier: caps.plan,
            verdict: Some(Verdict::Absent(
                "markFileAsViewed и unmarkFileAsViewed нет в схеме по introspection".to_owned(),
            )),
        });
    }
    let start = cx.trace.len();
    let mut after_mark = None;
    let mut after_unmark = None;
    for (key, viewed) in [(GH_MARK, true), (GH_UNMARK, false)] {
        match reviewer.set_viewed(&caps, pull, &path, viewed) {
            Ok(()) => {}
            Err(err) => match gh_write_verdict(key, &err) {
                Some(verdict) => return Err(verdict),
                None => {
                    notes.push(format!("отказ хоста на {key}: {err}"));
                    break;
                }
            },
        }
        let state = viewed_state(&reviewer, pull, &path, "ревьюер")?;
        author_state(
            &mut notes,
            if viewed {
                "после отметки"
            } else {
                "после снятия"
            },
        );
        if viewed {
            after_mark = state;
        } else {
            after_unmark = state;
        }
    }
    let calls = cx.trace.calls();
    let writes: Vec<(String, Value, Option<u16>)> = calls
        .get(start..)
        .unwrap_or_default()
        .iter()
        .filter(|call| call.actor == ACTOR_REVIEWER)
        .filter_map(|call| {
            let body = call.request.body_json();
            let key = gh_write_key(call.request.method, call.request.path(), body.as_ref())?;
            Some((
                key,
                body.unwrap_or(Value::Null),
                call.outcome.as_ref().ok().map(|response| response.status),
            ))
        })
        .collect();
    let (journal, sessions, submissions) = dump()?;
    let checks = viewed_checks(&ViewedFacts {
        pull_node: pull.as_str(),
        path: &path,
        writes: &writes,
        after_mark,
        after_unmark,
        sessions,
        submissions,
    });
    let after = json!({"reviewer": {
        "path": path,
        "after_mark": viewed_name(after_mark),
        "after_unmark": viewed_name(after_unmark),
    }});
    Ok(Observed {
        checks,
        notes,
        before,
        after,
        journal,
        tier: caps.plan,
        verdict: None,
    })
}

#[cfg(test)]
mod tests;
