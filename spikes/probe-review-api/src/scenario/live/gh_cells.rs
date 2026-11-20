//! Клетки строк GitHub в живом столбце `github_cloud`: обвязка общая с GitLab
//! ([`run_live_cell`]), сценарии строк. Ожидания — в отчёте, «Живые клетки GitHub».
//!
//! Оракул живой клетки не видит полей `forge-fake` (`origin`): записи пробы берутся из следа
//! ревьюера в окне отправки, состояние — из чтения автором reviews PR и их комментариев.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use serde_json::{Value, json};

use super::INJECTION_UNAVAILABLE;
use super::Verdict;
use super::github_adapter;
use super::gl_cells::{LiveRoles, Observed, Poll, WRITE_UNOBSERVED, run_live_cell, stand};
use crate::fake::{GraphqlKind, graphql_root};
use crate::github::{
    ApiError, Capabilities, GitHub, PullSnapshot, RemoteComment, RemoteReview, RemoteThread,
    ReviewEvent, ReviewState, ViewedState, is_rate_limited,
};
use crate::journal::{ItemState, Journal, Operation, ProjectId};
use crate::live::guard::Guarded;
use crate::live::lifecycle::Cell;
use crate::model::anchor::Side;
use crate::model::{Provider, ProviderId, ReviewSession, ReviewTarget};
use crate::results::ResultFile;
use crate::scenario::Scenario;
use crate::scenario::fake::{Checks, Spot, add_drafts, dump_in};
use crate::scrub::Scrubber;
use crate::submit::{
    Delivery, Report, SubmitError, plan_github_local, plan_github_pending, report, run_github,
};
use crate::transport::{Call, Failure, Method, Request, Response, Trace, Transport};

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
    matches!(
        scenario,
        Scenario::GhLocalCreate
            | Scenario::GhPendingCycle
            | Scenario::GhAnchors
            | Scenario::GhViewed
    )
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
            Scenario::GhPendingCycle => gh_pending_cycle(cx, cell),
            Scenario::GhAnchors => gh_anchors(cx, cell),
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
    pr_state_by(author, "автором")
}

/// Reviews PR и их комментарии, видимые токену роли `who` (`PENDING` — только своему).
fn pr_state_by<T: Transport>(
    github: &GitHub<T>,
    who: &str,
) -> Result<(Vec<ReviewRead>, Value), Verdict> {
    let reviews = github
        .reviews()
        .map_err(|err| gh_read_or_fail(&format!("reviews PR {who}"), &err))?;
    let mut state = Vec::new();
    for review in reviews {
        let comments = github
            .review_comments(&review.id)
            .map_err(|err| gh_read_or_fail(&format!("комментарии review {who}"), &err))?;
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

/// Ключи записей `gh-pending-cycle`.
pub const GH_ADD_THREAD: &str = "graphql addPullRequestReviewThread";
pub const GH_UPDATE_COMMENT: &str = "graphql updatePullRequestReviewComment";
pub const GH_SUBMIT: &str = "POST reviews/{id}/events";
/// Черновики `gh-pending-cycle` — те же строки, что у `gh-local-create`.
const PENDING_DRAFTS: [(Spot, &str); 2] = LOCAL_DRAFTS;
const PENDING_UPDATED: &str = "первый, исправлен";

/// Транспорт ревьюера `gh-pending-cycle` с клиентской инъекцией: первый Submit
/// ([`GH_SUBMIT`]) не отправляется, исполнителю отдаётся отказ `not_sent`, остальные запросы идут
/// как есть. Стоит над сторожем, поэтому не отправленный запрос в след не попадает: ID review из
/// его пути фиксирует [`SubmitDrop::dropped`].
pub struct SubmitDrop<R> {
    inner: R,
    dropped: RefCell<Option<String>>,
}

impl<R> SubmitDrop<R> {
    pub fn new(inner: R) -> SubmitDrop<R> {
        SubmitDrop {
            inner,
            dropped: RefCell::new(None),
        }
    }

    /// ID review в пути не отправленного Submit, если инъекция сработала.
    pub fn dropped(&self) -> Option<String> {
        self.dropped.borrow().clone()
    }
}

/// ID review в пути `…/reviews/{id}/events`.
fn submitted_review(path: &str) -> Option<String> {
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    path.rsplit_once("/reviews/")
        .and_then(|(_, rest)| rest.strip_suffix("/events"))
        .map(str::to_owned)
}

impl<R: Transport> Transport for SubmitDrop<R> {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let body = request.body_json();
        let submit = gh_write_key(request.method, request.path(), body.as_ref()).as_deref()
            == Some(GH_SUBMIT);
        if !submit || self.dropped.borrow().is_some() {
            return self.inner.send(request);
        }
        *self.dropped.borrow_mut() = Some(submitted_review(request.path()).unwrap_or_default());
        Err(Failure::not_sent(
            "инъекция пробы: Submit не отправлен, клиенту отдан отказ «запрос не ушёл»",
        ))
    }
}

/// Отказ исполнителя ветви «серверный `PENDING`» без записи в элемент. Чтение шага 1 (последний
/// вызов роли в окне — снимок PR или его файлы) — [`gh_submit_verdict`]. Чтение `PENDING`
/// (последний вызов — путь `…/reviews…`): неожиданный ответ — `fail` с текстом адаптера, прочий
/// отказ — [`gh_read_or_fail`]. Остановка до записи — `fail`: это наблюдение о сохранении ID
/// комментариев, а не неготовность PR.
pub fn gh_pending_submit_verdict(err: &SubmitError, window: &[Call], actor: &str) -> Verdict {
    let pending_read = window
        .iter()
        .rev()
        .find(|call| call.actor == actor)
        .is_some_and(|call| call.request.path().contains("/reviews"));
    match err {
        SubmitError::GitHub(ApiError::Unexpected(reason)) if pending_read => {
            Verdict::Fail(format!("отправка: чтение PENDING: {reason}"))
        }
        SubmitError::GitHub(api) if pending_read => {
            gh_read_or_fail("отправка: чтение PENDING", api)
        }
        SubmitError::Refused(reason) => {
            Verdict::Fail(format!("отправка остановлена до записи: {reason}"))
        }
        other => gh_submit_verdict(other),
    }
}

/// Запись роли в окне клетки: ключ, путь, тело, код и тело ответа 2xx.
#[derive(Clone, Debug)]
pub struct WriteCall {
    pub key: String,
    pub path: String,
    pub body: Value,
    pub status: Option<u16>,
    pub response: Option<Value>,
}

/// Записи роли `actor` в окне по ключу [`gh_write_key`].
pub fn write_calls(window: &[Call], actor: &str) -> Vec<WriteCall> {
    window
        .iter()
        .filter(|call| call.actor == actor)
        .filter_map(|call| {
            let body = call.request.body_json();
            let key = gh_write_key(call.request.method, call.request.path(), body.as_ref())?;
            let response = call.outcome.as_ref().ok();
            Some(WriteCall {
                key,
                path: call.request.path().to_owned(),
                body: body.unwrap_or(Value::Null),
                status: response.map(|response| response.status),
                response: response
                    .filter(|response| response.is_success())
                    .and_then(|response| response.body_json().ok()),
            })
        })
        .collect()
}

/// Факты `gh-pending-cycle` для оракула.
pub struct PendingFacts<'a> {
    pub head: &'a str,
    pub path: &'a str,
    pub reviewer: &'a str,
    /// Записи ревьюера в окне обеих отправок.
    pub writes: &'a [WriteCall],
    /// ID review в пути не отправленного Submit.
    pub dropped: Option<&'a str>,
    /// Чтения между отправками: ревьюер (свой `PENDING`) и автор.
    pub reviewer_between: &'a [ReviewRead],
    pub author_between: &'a [ReviewRead],
    /// Чтение автора после второй отправки.
    pub after: &'a [ReviewRead],
    pub first: &'a Report,
    pub second: Option<&'a Report>,
    pub recovered: usize,
}

fn item_states(report: &Report) -> Vec<(Operation, ItemState)> {
    report
        .items
        .iter()
        .map(|item| (item.operation, item.state))
        .collect()
}

fn lines_and_bodies(comments: &[RemoteComment]) -> Vec<(Option<u32>, String)> {
    let mut seen: Vec<(Option<u32>, String)> = comments
        .iter()
        .map(|comment| (comment.line, comment.body.clone()))
        .collect();
    seen.sort();
    seen
}

/// Оракул `gh-pending-cycle` на живом хосте (ожидание — в отчёте).
pub fn pending_cycle_checks(facts: &PendingFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for write in facts.writes {
        *counts.entry(write.key.as_str()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера",
        counts,
        [
            (GH_CREATE, 1),
            (GH_ADD_THREAD, 2),
            (GH_UPDATE_COMMENT, 1),
            (GH_SUBMIT, 1),
        ]
        .into_iter()
        .collect(),
    );
    for (index, write) in facts.writes.iter().enumerate() {
        checks.check(
            &format!("запись {index} {}: принята", write.key),
            write
                .status
                .is_some_and(|status| (200..300).contains(&status)),
            format!("{:?}", write.status),
        );
    }
    let of = |key: &'static str| facts.writes.iter().filter(move |write| write.key == key);
    let create = of(GH_CREATE).next();
    let created = create.and_then(|write| write.response.as_ref());
    let created_id = created.and_then(|value| value.get("id")).map(id_text);
    let created_node = created
        .and_then(|value| value.get("node_id"))
        .and_then(Value::as_str);
    if let Some(create) = create {
        checks.check(
            "создание PENDING без event и comments",
            create.body.get("event").is_none_or(Value::is_null)
                && create.body.get("comments").is_none_or(Value::is_null),
            create.body.to_string(),
        );
        checks.eq(
            "commit_id создания PENDING",
            create.body["commit_id"].as_str(),
            Some(facts.head),
        );
    }
    checks.eq(
        "addPullRequestReviewThread в созданный review",
        of(GH_ADD_THREAD)
            .map(|write| {
                write
                    .body
                    .pointer("/variables/input/pullRequestReviewId")
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>(),
        vec![created_node; 2],
    );
    let own_between: Vec<&ReviewRead> = facts
        .reviewer_between
        .iter()
        .filter(|(review, _)| review.user.eq_ignore_ascii_case(facts.reviewer))
        .collect();
    checks.eq(
        "ревьюер между отправками: свои reviews (ID, состояние)",
        own_between
            .iter()
            .map(|(review, _)| (Some(review.id.as_str().to_owned()), review.state.name()))
            .collect::<Vec<_>>(),
        vec![(created_id.clone(), ReviewState::Pending.name())],
    );
    let between_comments: &[RemoteComment] = own_between
        .first()
        .map_or(&[], |(_, comments)| comments.as_slice());
    let wanted: Vec<(Option<u32>, String)> = PENDING_DRAFTS
        .iter()
        .filter_map(|(spot, body)| match spot {
            Spot::Line(_, line) => Some((Some(*line), (*body).to_owned())),
            _ => None,
        })
        .collect();
    checks.eq(
        "ревьюер между отправками: комментарии PENDING (строка, текст)",
        lines_and_bodies(between_comments),
        wanted.clone(),
    );
    let first_line = wanted.first().and_then(|(line, _)| *line);
    let first_before = between_comments
        .iter()
        .find(|comment| comment.line == first_line && comment.path == facts.path);
    checks.eq(
        "updatePullRequestReviewComment по node ID первого комментария",
        of(GH_UPDATE_COMMENT)
            .map(|write| {
                write
                    .body
                    .pointer("/variables/input/pullRequestReviewCommentId")
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>(),
        vec![first_before.map(|comment| comment.node_id.as_str())],
    );
    checks.eq(
        "инъекция: не отправленный Submit созданного review",
        facts.dropped.map(str::to_owned),
        created_id.clone(),
    );
    for submit in of(GH_SUBMIT) {
        checks.eq(
            "Submit созданного review",
            submitted_review(&submit.path),
            created_id.clone(),
        );
        checks.check(
            "Submit с event COMMENT и body, без comments[]",
            submit.body["event"].as_str() == Some(ReviewEvent::Comment.name())
                && submit.body["body"]
                    .as_str()
                    .is_some_and(|body| !body.trim().is_empty())
                && submit.body.get("comments").is_none(),
            submit.body.to_string(),
        );
    }
    checks.eq(
        "автор между отправками: reviews ревьюера",
        facts
            .author_between
            .iter()
            .filter(|(review, _)| review.user.eq_ignore_ascii_case(facts.reviewer))
            .map(|(review, _)| review.id.as_str().to_owned())
            .collect::<Vec<_>>(),
        Vec::<String>::new(),
    );
    let own_after: Vec<&ReviewRead> = facts
        .after
        .iter()
        .filter(|(review, _)| review.user.eq_ignore_ascii_case(facts.reviewer))
        .collect();
    checks.eq(
        "автор после: reviews ревьюера (ID, состояние)",
        own_after
            .iter()
            .map(|(review, _)| (Some(review.id.as_str().to_owned()), review.state.name()))
            .collect::<Vec<_>>(),
        vec![(created_id, ReviewState::Commented.name())],
    );
    let after_comments: &[RemoteComment] = own_after
        .first()
        .map_or(&[], |(_, comments)| comments.as_slice());
    let mut wanted_after = wanted;
    if let Some(first) = wanted_after.first_mut() {
        first.1 = PENDING_UPDATED.to_owned();
    }
    checks.eq(
        "автор после: комментарии review (строка, текст)",
        lines_and_bodies(after_comments),
        wanted_after,
    );
    let first_after = after_comments
        .iter()
        .find(|comment| comment.line == first_line && comment.path == facts.path);
    checks.eq(
        "первый комментарий: прежние ID и node ID",
        first_after.map(|comment| (comment.id.as_str(), comment.node_id.as_str())),
        first_before.map(|comment| (comment.id.as_str(), comment.node_id.as_str())),
    );
    checks.eq(
        "первая отправка: закрыта, итог",
        (facts.first.closed, facts.first.delivery),
        (true, Delivery::NotSent),
    );
    checks.eq(
        "первая отправка: элементы",
        item_states(facts.first),
        vec![
            (Operation::GitHubCreatePending, ItemState::Confirmed),
            (Operation::GitHubAddPendingComment, ItemState::Confirmed),
            (Operation::GitHubAddPendingComment, ItemState::Confirmed),
            (Operation::GitHubSubmitReview, ItemState::Failed),
        ],
    );
    checks.eq(
        "вторая отправка: элементы, итог",
        facts
            .second
            .map(|report| (item_states(report), report.delivery)),
        Some((
            vec![
                (Operation::GitHubUpdatePendingComment, ItemState::Confirmed),
                (Operation::GitHubSubmitReview, ItemState::Confirmed),
            ],
            Delivery::Full,
        )),
    );
    checks.eq("восстановленные элементы", facts.recovered, 0);
    checks
}

fn gh_pending_cycle<T: Transport>(cx: &GhCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = GitHub::new(
        SubmitDrop::new(cx.reviewer),
        cx.reviewer.target().github_endpoints(),
        &review,
    )
    .map_err(|err| stand("адаптер", &err))?;
    let author = github_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_pull(&reviewer, &cx.poll)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| gh_read_or_fail("возможности ревьюера", &err))?;
    let (_, before) = pr_state(&author)?;
    let path = snapshot
        .files
        .first()
        .map(|file| file.new_path().to_owned())
        .ok_or_else(|| Verdict::Error("в diff PR нет файлов".to_owned()))?;
    let journal_path = cx.state_dir.join("journal-gh-pending-cycle.db");
    let project = ProjectId::new(LIVE_PROJECT);
    let mut notes = vec![format!(
        "ветвь «первый Submit получает 422»: not_run «{INJECTION_UNAVAILABLE}»"
    )];
    let window = |from: usize| cx.trace.calls().get(from..).unwrap_or_default().to_vec();
    let dump = || -> Result<Value, Verdict> {
        let journal =
            Journal::open(&journal_path).map_err(|err| stand("журнал после переоткрытия", &err))?;
        dump_in(&journal, &project).map_err(|err| stand("журнал", &err))
    };
    let start = cx.trace.len();
    let (first, second, between) = {
        let journal = Journal::open(&journal_path).map_err(|err| stand("журнал", &err))?;
        let scope = journal.scope(&project);
        let session = ReviewSession::new(
            &cx.reviewer_login,
            review.clone(),
            snapshot.revision.clone(),
        );
        let session = scope
            .create_session(&session)
            .map_err(|err| stand("сессия", &err))?;
        let ids = add_drafts(&scope, session, &snapshot.files, &PENDING_DRAFTS)
            .map_err(|err| stand("черновики", &err))?;
        if !caps.server_pending() {
            drop(scope);
            drop(journal);
            let journal = dump()?;
            return Ok(Observed {
                checks: Checks::default(),
                notes,
                after: before.clone(),
                before,
                journal,
                tier: caps.plan,
                verdict: Some(Verdict::Absent(
                    "addPullRequestReviewThread и updatePullRequestReviewComment нет в схеме по \
                     introspection"
                        .to_owned(),
                )),
            });
        }
        let first = plan_github_pending(&scope, session, ReviewEvent::Comment, SUMMARY, None, &[])
            .map_err(|err| stand("план", &err))?;
        let sent = run_github(&scope, first.id, &reviewer, &caps)
            .map_err(|err| gh_pending_submit_verdict(&err, &window(start), ACTOR_REVIEWER))?;
        if let Some(verdict) = gh_delivery_verdict(&sent, &window(start), ACTOR_REVIEWER) {
            return Err(verdict);
        }
        let between = (pr_state_by(&reviewer, "ревьюером")?, pr_state(&author)?);
        let mut second = None;
        if let Some(id) = reviewer.transport().dropped() {
            notes.push(format!(
                "инъекция: Submit review {id} не отправлен, клиенту отдан отказ «запрос не ушёл»"
            ));
            let first_draft = *ids
                .first()
                .ok_or_else(|| Verdict::Error("нет черновика".to_owned()))?;
            scope
                .update_draft_body(first_draft, PENDING_UPDATED)
                .map_err(|err| stand("изменение черновика", &err))?;
            let plan = plan_github_pending(
                &scope,
                session,
                ReviewEvent::Comment,
                SUMMARY,
                None,
                &[first_draft],
            )
            .map_err(|err| stand("план", &err))?;
            let from = cx.trace.len();
            let sent = run_github(&scope, plan.id, &reviewer, &caps)
                .map_err(|err| gh_pending_submit_verdict(&err, &window(from), ACTOR_REVIEWER))?;
            if let Some(verdict) = gh_delivery_verdict(&sent, &window(from), ACTOR_REVIEWER) {
                return Err(verdict);
            }
            second = Some(plan.id);
        } else {
            notes.push(
                "инъекция не сработала: Submit не дошёл до обёртки, вторая отправка не шла"
                    .to_owned(),
            );
        }
        (first.id, second, between)
    };
    let ((reviewer_between, reviewer_between_value), (author_between, author_between_value)) =
        between;
    let (after_state, after_value) = pr_state(&author)?;
    let writes = write_calls(&window(start), ACTOR_REVIEWER);
    let (first_report, second_report, journal, recovered) = {
        let journal =
            Journal::open(&journal_path).map_err(|err| stand("журнал после переоткрытия", &err))?;
        let scope = journal.scope(&project);
        let first = report(&scope, first).map_err(|err| stand("итог отправки", &err))?;
        let second = second
            .map(|id| report(&scope, id))
            .transpose()
            .map_err(|err| stand("итог отправки", &err))?;
        let value = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
        (first, second, value, journal.recovered().len())
    };
    let dropped = reviewer.transport().dropped();
    let checks = pending_cycle_checks(&PendingFacts {
        head: snapshot.revision.head_sha().as_str(),
        path: &path,
        reviewer: &cx.reviewer_login,
        writes: &writes,
        dropped: dropped.as_deref(),
        reviewer_between: &reviewer_between,
        author_between: &author_between,
        after: &after_state,
        first: &first_report,
        second: second_report.as_ref(),
        recovered,
    });
    Ok(Observed {
        checks,
        notes,
        before,
        after: json!({
            "between": {"reviewer": reviewer_between_value, "author": author_between_value},
            "author": after_value,
        }),
        journal,
        tier: caps.plan,
        verdict: None,
    })
}

/// Ключи записей `gh-anchors`.
pub const GH_REPLY: &str = "POST comments/{id}/replies";
pub const GH_RESOLVE: &str = "graphql resolveReviewThread";
/// Черновики `gh-anchors`: удалённая 03 (старая сторона, 3), добавленная 24a (новая, 24) и диапазон
/// 24a–24b на новой стороне (24–25). Первый — корень треда для ответа и resolve.
const ANCHOR_DRAFTS: [(Spot, &str); 3] = [
    (Spot::Line(Side::Old, 3), "удалённая 03"),
    (Spot::Line(Side::New, 24), "добавленная 24a"),
    (
        Spot::Range(Side::New, 24, Side::New, 25),
        "диапазон 24a–24b",
    ),
];
/// Якорь каждого черновика по фикстуре: `line`, `side`, `start_line`, `start_side`.
const ANCHOR_FIELDS: [(u32, &str, Option<u32>, Option<&str>); 3] = [
    (3, "LEFT", None, None),
    (24, "RIGHT", None, None),
    (25, "RIGHT", Some(24), Some("RIGHT")),
];
const ANCHOR_REPLY: &str = "ответ в треде";
/// Причина `not_run`, если тред корня не найден в чтении тредов автором.
pub const THREAD_UNOBSERVED: &str = "тред корня не наблюдён чтением тредов автором";

/// ID комментария в пути `…/comments/{id}/replies`.
fn replied_root(path: &str) -> Option<String> {
    let path = path.split_once('?').map_or(path, |(path, _)| path);
    path.rsplit_once("/comments/")
        .and_then(|(_, rest)| rest.strip_suffix("/replies"))
        .map(str::to_owned)
}

/// Якорь комментария: путь, `line`, `side`, `start_line`, `start_side`.
type AnchorRow = (
    Option<String>,
    Option<u64>,
    Option<String>,
    Option<u64>,
    Option<String>,
);

fn wanted_anchors(path: &str) -> Vec<AnchorRow> {
    ANCHOR_FIELDS
        .iter()
        .map(|(line, side, start_line, start_side)| {
            (
                Some(path.to_owned()),
                Some(u64::from(*line)),
                Some((*side).to_owned()),
                start_line.map(u64::from),
                start_side.map(str::to_owned),
            )
        })
        .collect()
}

/// Факты `gh-anchors` для оракула.
pub struct AnchorFacts<'a> {
    pub head: &'a str,
    pub path: &'a str,
    pub reviewer: &'a str,
    /// Записи ревьюера в окне от отправки до resolve.
    pub writes: &'a [WriteCall],
    /// REST ID корня треда по чтению автора.
    pub root: Option<&'a str>,
    /// Node ID треда корня по чтению тредов автором.
    pub thread: Option<&'a str>,
    /// `isResolved` треда корня в чтении автора до resolve и после.
    pub resolved_before: Option<bool>,
    pub resolved_after: Option<bool>,
    /// Reviews PR и их комментарии по чтению автора после resolve.
    pub after: &'a [ReviewRead],
    pub report: &'a Report,
    pub recovered: usize,
}

/// Оракул `gh-anchors` на живом хосте (ожидание — в отчёте).
pub fn gh_anchors_checks(facts: &AnchorFacts<'_>) -> Checks {
    let mut checks = Checks::default();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for write in facts.writes {
        *counts.entry(write.key.as_str()).or_insert(0) += 1;
    }
    checks.eq(
        "записи ревьюера",
        counts,
        [(GH_CREATE, 1), (GH_REPLY, 1), (GH_RESOLVE, 1)]
            .into_iter()
            .collect(),
    );
    for (index, write) in facts.writes.iter().enumerate() {
        checks.check(
            &format!("запись {index} {}: принята", write.key),
            write
                .status
                .is_some_and(|status| (200..300).contains(&status)),
            format!("{:?}", write.status),
        );
    }
    let of = |key: &'static str| facts.writes.iter().filter(move |write| write.key == key);
    let create = of(GH_CREATE).next();
    let created_id = create
        .and_then(|write| write.response.as_ref())
        .and_then(|value| value.get("id"))
        .map(id_text);
    if let Some(create) = create {
        checks.eq(
            "event Create review",
            create.body["event"].as_str(),
            Some(ReviewEvent::Comment.name()),
        );
        checks.eq(
            "commit_id Create review",
            create.body["commit_id"].as_str(),
            Some(facts.head),
        );
        let sent: Vec<AnchorRow> = create.body["comments"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|c| {
                        (
                            c["path"].as_str().map(str::to_owned),
                            c["line"].as_u64(),
                            c["side"].as_str().map(str::to_owned),
                            c["start_line"].as_u64(),
                            c["start_side"].as_str().map(str::to_owned),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        checks.eq(
            "якоря в comments[] Create review",
            sent,
            wanted_anchors(facts.path),
        );
    }
    checks.eq(
        "ответ на REST ID корня",
        of(GH_REPLY)
            .map(|write| replied_root(&write.path))
            .collect::<Vec<_>>(),
        vec![facts.root.map(str::to_owned)],
    );
    checks.eq(
        "resolveReviewThread по node ID треда корня",
        of(GH_RESOLVE)
            .map(|write| {
                write
                    .body
                    .pointer("/variables/input/threadId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>(),
        vec![facts.thread.map(str::to_owned)],
    );
    let own: Vec<&ReviewRead> = facts
        .after
        .iter()
        .filter(|(review, _)| review.user.eq_ignore_ascii_case(facts.reviewer))
        .collect();
    let created = own
        .iter()
        .find(|(review, _)| Some(review.id.as_str()) == created_id.as_deref());
    checks.eq(
        "автор: review Create (состояние, commit_id)",
        created.map(|(review, _)| (review.state.name(), review.commit_id.as_str())),
        Some((ReviewState::Commented.name(), facts.head)),
    );
    let mut seen: Vec<(String, AnchorRow)> = created
        .map(|(_, comments)| comments.as_slice())
        .unwrap_or_default()
        .iter()
        .filter(|comment| comment.in_reply_to.is_none())
        .map(|comment| {
            (
                comment.body.clone(),
                (
                    Some(comment.path.clone()),
                    comment.line.map(u64::from),
                    comment.side.clone(),
                    comment.start_line.map(u64::from),
                    comment.start_side.clone(),
                ),
            )
        })
        .collect();
    seen.sort_by(|a, b| a.0.cmp(&b.0));
    let mut wanted: Vec<(String, AnchorRow)> = ANCHOR_DRAFTS
        .iter()
        .map(|(_, body)| (*body).to_owned())
        .zip(wanted_anchors(facts.path))
        .collect();
    wanted.sort_by(|a, b| a.0.cmp(&b.0));
    checks.eq(
        "автор: комментарии review Create (текст, якорь)",
        seen,
        wanted,
    );
    checks.eq(
        "автор: ответы в reviews ревьюера (in_reply_to, текст)",
        own.iter()
            .flat_map(|(_, comments)| comments.iter())
            .filter_map(|comment| {
                comment
                    .in_reply_to
                    .as_ref()
                    .map(|parent| (parent.as_str().to_owned(), comment.body.clone()))
            })
            .collect::<Vec<_>>(),
        facts
            .root
            .map(|root| (root.to_owned(), ANCHOR_REPLY.to_owned()))
            .into_iter()
            .collect::<Vec<_>>(),
    );
    checks.eq(
        "тред корня до resolve не разрешён",
        facts.resolved_before,
        Some(false),
    );
    checks.eq(
        "тред корня после resolve разрешён",
        facts.resolved_after,
        Some(true),
    );
    checks.eq(
        "элементы журнала",
        facts
            .report
            .items
            .iter()
            .map(|item| {
                (
                    item.operation,
                    item.state,
                    item.remote_id.as_ref().map(|id| id.as_str().to_owned()),
                )
            })
            .collect::<Vec<_>>(),
        vec![(
            Operation::GitHubCreateReview,
            ItemState::Confirmed,
            created_id,
        )],
    );
    checks.eq("итог в журнале", facts.report.delivery, Delivery::Full);
    checks.eq("восстановленные элементы", facts.recovered, 0);
    checks
}

fn threads_json(threads: &[RemoteThread]) -> Value {
    json!(
        threads
            .iter()
            .map(|thread| json!({
                "id": thread.node_id.as_str(),
                "isResolved": thread.is_resolved,
                "root": thread.root.as_ref().map(ProviderId::as_str),
                "root_full_database_id": thread.root_database_id,
                "comments": thread.comments,
            }))
            .collect::<Vec<_>>()
    )
}

/// Запись ответа или resolve: `Ok(true)` — дальше, `Ok(false)` — явный отказ хоста (заметка,
/// дальнейшие записи не идут), `Err` — итог без проверок. Ответ 2xx, не прошедший проверку
/// адаптера, — заметка: запись дошла до хоста, итог судит чтение автора.
fn anchor_write(
    key: &str,
    outcome: Result<(), ApiError>,
    notes: &mut Vec<String>,
) -> Result<bool, Verdict> {
    match outcome {
        Ok(()) => Ok(true),
        Err(ApiError::Unexpected(reason)) => {
            notes.push(format!(
                "{key}: ответ 2xx не прошёл проверку адаптера: {reason}"
            ));
            Ok(true)
        }
        Err(err) => match gh_write_verdict(key, &err) {
            Some(verdict) => Err(verdict),
            None => {
                notes.push(format!("отказ хоста на {key}: {err}"));
                Ok(false)
            }
        },
    }
}

fn gh_anchors<T: Transport>(cx: &GhCx<'_, T>, cell: &mut Cell) -> Result<Observed, Verdict> {
    let review = cx.review(cell.number);
    let reviewer = github_adapter(cx.reviewer, &review).map_err(|err| stand("адаптер", &err))?;
    let author = github_adapter(cx.author, &review).map_err(|err| stand("адаптер", &err))?;
    let snapshot = await_pull(&reviewer, &cx.poll)?;
    let caps = reviewer
        .capabilities()
        .map_err(|err| gh_read_or_fail("возможности ревьюера", &err))?;
    let (_, before) = pr_state(&author)?;
    let path = snapshot
        .files
        .first()
        .map(|file| file.new_path().to_owned())
        .ok_or_else(|| Verdict::Error("в diff PR нет файлов".to_owned()))?;
    let pull = &snapshot.node_id;
    let journal_path = cx.state_dir.join("journal-gh-anchors.db");
    let project = ProjectId::new(LIVE_PROJECT);
    let window = |from: usize| cx.trace.calls().get(from..).unwrap_or_default().to_vec();
    let mut notes = Vec::new();
    let start = cx.trace.len();
    let submission = {
        let journal = Journal::open(&journal_path).map_err(|err| stand("журнал", &err))?;
        let scope = journal.scope(&project);
        let session = ReviewSession::new(
            &cx.reviewer_login,
            review.clone(),
            snapshot.revision.clone(),
        );
        let session = scope
            .create_session(&session)
            .map_err(|err| stand("сессия", &err))?;
        add_drafts(&scope, session, &snapshot.files, &ANCHOR_DRAFTS)
            .map_err(|err| stand("черновики", &err))?;
        if !caps.resolve() {
            let dump = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
            return Ok(Observed {
                checks: Checks::default(),
                notes,
                after: before.clone(),
                before,
                journal: dump,
                tier: caps.plan,
                verdict: Some(Verdict::Absent(
                    "resolveReviewThread и unresolveReviewThread нет в схеме по introspection"
                        .to_owned(),
                )),
            });
        }
        let submission = plan_github_local(&scope, session, ReviewEvent::Comment, SUMMARY)
            .map_err(|err| stand("план", &err))?;
        let sent = run_github(&scope, submission.id, &reviewer, &caps)
            .map_err(|err| gh_submit_verdict(&err))?;
        if let Some(verdict) = gh_delivery_verdict(&sent, &window(start), ACTOR_REVIEWER) {
            return Err(verdict);
        }
        submission.id
    };
    let created = write_calls(&window(start), ACTOR_REVIEWER)
        .into_iter()
        .find(|write| write.key == GH_CREATE)
        .and_then(|write| write.response)
        .and_then(|value| value.get("id").map(id_text));
    let root_body = ANCHOR_DRAFTS[0].1;
    let root = match &created {
        Some(created) => {
            let comments = author
                .review_comments(&ProviderId::new(created.as_str()))
                .map_err(|err| gh_read_or_fail("комментарии review автором", &err))?;
            let root = comments
                .into_iter()
                .find(|comment| comment.body == root_body && comment.in_reply_to.is_none());
            if root.is_none() {
                notes.push(format!(
                    "корень треда «{root_body}» не найден в чтении автора: ответ и resolve не шли"
                ));
            }
            root
        }
        None => {
            notes.push("Create review не принят: ответ и resolve не шли".to_owned());
            None
        }
    };
    let mut thread: Option<RemoteThread> = None;
    let mut threads_before = Value::Null;
    let mut unobserved = None;
    if let Some(root) = &root {
        let replied = reviewer.reply(&root.id, ANCHOR_REPLY).map(|_| ());
        if anchor_write(GH_REPLY, replied, &mut notes)? {
            let threads = author
                .review_threads(pull)
                .map_err(|err| gh_read_or_fail("треды PR автором", &err))?;
            threads_before = threads_json(&threads);
            thread = threads
                .into_iter()
                .find(|thread| thread.root.as_ref() == Some(&root.node_id));
            match &thread {
                Some(found) => {
                    notes.push(format!(
                        "наблюдение: тред корня найден по node ID, comments.totalCount {}, \
                         fullDatabaseId корня {:?} (REST ID {})",
                        found.comments,
                        found.root_database_id,
                        root.id.as_str()
                    ));
                    let resolved = reviewer.set_thread_resolved(&found.node_id, true);
                    anchor_write(GH_RESOLVE, resolved, &mut notes)?;
                }
                None => {
                    unobserved = Some(format!(
                        "{THREAD_UNOBSERVED}: node ID корня {} нет среди корней тредов, resolve \
                         не шёл",
                        root.node_id.as_str()
                    ));
                }
            }
        }
    }
    let threads_after = match &thread {
        Some(_) => Some(
            author
                .review_threads(pull)
                .map_err(|err| gh_read_or_fail("треды PR автором после resolve", &err))?,
        ),
        None => None,
    };
    let root_node = root.as_ref().map(|root| &root.node_id);
    let resolved_after = threads_after.as_ref().and_then(|threads| {
        threads
            .iter()
            .find(|candidate| candidate.root.as_ref() == root_node)
            .map(|candidate| candidate.is_resolved)
    });
    if let Some(threads) = &threads_after {
        let others: Vec<bool> = threads
            .iter()
            .filter(|candidate| candidate.root.as_ref() != root_node)
            .map(|candidate| candidate.is_resolved)
            .collect();
        notes.push(format!(
            "наблюдение: isResolved остальных тредов после resolve: {others:?}"
        ));
    }
    let (after_state, after_value) = pr_state(&author)?;
    for (review, comments) in &after_state {
        for comment in comments
            .iter()
            .filter(|comment| comment.in_reply_to.is_some())
        {
            notes.push(format!(
                "наблюдение: ответ в review {} ({}{}), line {:?}, side {:?}",
                review.id.as_str(),
                review.state.name(),
                if Some(review.id.as_str()) == created.as_deref() {
                    ", тот же, что Create review"
                } else {
                    ", отдельный review"
                },
                comment.line,
                comment.side
            ));
        }
    }
    let writes = write_calls(&window(start), ACTOR_REVIEWER);
    let (done, journal, recovered) = {
        let journal =
            Journal::open(&journal_path).map_err(|err| stand("журнал после переоткрытия", &err))?;
        let scope = journal.scope(&project);
        let done = report(&scope, submission).map_err(|err| stand("итог отправки", &err))?;
        let value = dump_in(&journal, &project).map_err(|err| stand("журнал", &err))?;
        (done, value, journal.recovered().len())
    };
    let checks = gh_anchors_checks(&AnchorFacts {
        head: snapshot.revision.head_sha().as_str(),
        path: &path,
        reviewer: &cx.reviewer_login,
        writes: &writes,
        root: root.as_ref().map(|root| root.id.as_str()),
        thread: thread.as_ref().map(|thread| thread.node_id.as_str()),
        resolved_before: thread.as_ref().map(|thread| thread.is_resolved),
        resolved_after,
        after: &after_state,
        report: &done,
        recovered,
    });
    Ok(Observed {
        checks,
        notes,
        before,
        after: json!({
            "author": after_value,
            "threads_before_resolve": threads_before,
            "threads_after_resolve": threads_after.as_deref().map(threads_json),
        }),
        journal,
        tier: caps.plan,
        verdict: unobserved.map(Verdict::NotRun),
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
