use std::time::Duration;

use super::*;
use crate::fake::github::{self as ghf, FakeCommit, FakeFile, GithubFake, Permission};
use crate::fake::{FakeServer, Fault, Reply as FakeReply};
use crate::github::Endpoints;
use crate::live::guard::{GuardTarget, WriteScope};
use crate::live::lifecycle::CellNames;
use crate::live::repo::Account;
use crate::results::Status;
use crate::scenario::live::guard_errors;
use crate::transport::Traced;
use crate::transport::http::{Credential, HttpConfig, HttpTransport, Secret};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const OWNER: &str = "probe-author";
const NAME: &str = "shellio-review-sandbox";
const REPO: &str = "probe-author/shellio-review-sandbox";
const FAKE_AUTHOR: &str = "probe-author";
const FAKE_REVIEWER: &str = "probe-reviewer";
const AUTHOR_TOKEN: &str = "fake-author-token";
const REVIEWER_TOKEN: &str = "fake-reviewer-token";
const NUMBER: u64 = 5;
const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";
const PATH: &str = "probe-review/fixture.txt";
/// Ключ Create review в журнале `forge-fake`.
const FAKE_CREATE: &str = "POST /repos/{owner}/{repo}/pulls/{number}/reviews";
const RUN_ID: &str = "20261121-010000-1";
const NOW: Poll = Poll {
    deadline: Duration::ZERO,
    pause: Duration::ZERO,
};
/// Diff `Fixture::Head` против `Fixture::Base`, как у клеток GitLab.
const FIXTURE_DIFF: &str = "@@ -1,8 +1,7 @@\n line 01\n line 02\n-line 03\n line 04\n-line 05\n\
+line 05 changed\n line 06\n line 07\n line 08\n@@ -22,6 +21,8 @@\n line 22\n line 23\n line 24\n\
+line 24a\n+line 24b\n line 25\n line 26\n line 27\n";

fn fake_github(files: bool) -> Result<FakeServer<GithubFake>, String> {
    fake_github_with(files, |_| {})
}

fn fake_github_with(
    files: bool,
    edit: impl FnOnce(&mut GithubFake),
) -> Result<FakeServer<GithubFake>, String> {
    let mut fake = GithubFake::new(OWNER, NAME);
    fake.add_user(FAKE_AUTHOR, AUTHOR_TOKEN, Permission::Write);
    fake.add_user(FAKE_REVIEWER, REVIEWER_TOKEN, Permission::Write);
    fake.add_pull(
        NUMBER,
        FAKE_AUTHOR,
        BASE,
        FakeCommit {
            sha: HEAD.to_owned(),
            files: if files {
                vec![FakeFile::modified(PATH, FIXTURE_DIFF)]
            } else {
                Vec::new()
            },
        },
    );
    edit(&mut fake);
    FakeServer::start(fake)
}

/// Ответ на чтение тредов GraphQL `reviewThreads` у роли автора в тестах `gh-anchors`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ThreadsReply {
    Keep,
    /// Все треды убраны: чтение, которое тред корня не наблюдает.
    Strip,
    /// Ответ на чтение номер `at` (с нуля) — ошибка GraphQL `undefinedField` при HTTP 200.
    GraphqlError {
        at: usize,
    },
    /// У тредов ответа номер `at` нет `isResolved`.
    NoResolved {
        at: usize,
    },
    /// Ответ номер `at` заменён кодом HTTP.
    Status {
        at: usize,
        code: u16,
    },
}

/// Транспорт теста: подменяет ответы на чтение тредов по [`ThreadsReply`]. След пишет ответ
/// хоста как есть.
struct AuthorThreads<T> {
    inner: T,
    reply: ThreadsReply,
    reads: std::sync::atomic::AtomicUsize,
}

impl<T: Transport> Transport for AuthorThreads<T> {
    fn send(&self, request: &Request) -> Result<Response, Failure> {
        let response = self.inner.send(request)?;
        let Ok(mut body) = response.body_json() else {
            return Ok(response);
        };
        if body.pointer("/data/node/reviewThreads").is_none() {
            return Ok(response);
        }
        let read = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        match self.reply {
            ThreadsReply::Keep => Ok(response),
            ThreadsReply::Strip => {
                if let Some(nodes) = body.pointer_mut("/data/node/reviewThreads/nodes") {
                    *nodes = json!([]);
                }
                Ok(Response::json(response.status, &body))
            }
            ThreadsReply::GraphqlError { at } if at == read => Ok(Response::json(
                200,
                &json!({"errors": [{
                    "message": "Field 'isResolved' doesn't exist on type 'PullRequestReviewThread'",
                    "extensions": {"code": "undefinedField"},
                }]}),
            )),
            ThreadsReply::NoResolved { at } if at == read => {
                if let Some(nodes) = body
                    .pointer_mut("/data/node/reviewThreads/nodes")
                    .and_then(Value::as_array_mut)
                {
                    for node in nodes {
                        if let Some(node) = node.as_object_mut() {
                            node.remove("isResolved");
                        }
                    }
                }
                Ok(Response::json(response.status, &body))
            }
            ThreadsReply::Status { at, code } if at == read => Ok(Response::json(
                code,
                &json!({"message": "подменённый ответ"}),
            )),
            _ => Ok(response),
        }
    }
}

/// Тело клетки строки над сторожами (`WriteScope::Loopback`) против `forge-fake`, клетка собрана
/// вручную, как после `open_cell`.
/// Второй клиент в процессе теста: та же операция, что у процесса `--second-client`.
struct InProcess {
    guard: Guarded<Traced<HttpTransport>>,
}

impl GhSecond for InProcess {
    fn create_pending(&self, number: u64, count: u32) -> Result<String, Verdict> {
        let review = ReviewTarget {
            provider: Provider::GitHub,
            host: "github.com".to_owned(),
            repo: REPO.to_owned(),
            number,
        };
        foreign_pending(&self.guard, &review, FAKE_REVIEWER, count)
    }
}

fn cell_body(
    server: &FakeServer<GithubFake>,
    scenario: Scenario,
) -> Result<Result<Observed, Verdict>, Box<dyn std::error::Error>> {
    cell_body_with(server, scenario, ThreadsReply::Keep)
}

fn cell_body_with(
    server: &FakeServer<GithubFake>,
    scenario: Scenario,
    author_threads: ThreadsReply,
) -> Result<Result<Observed, Verdict>, Box<dyn std::error::Error>> {
    let state = tempfile::tempdir()?;
    let trace = Trace::new();
    let target = GuardTarget::github(&Endpoints::single(server.base()), REPO);
    let guard = |token: &str, actor, reply| {
        let credential = Credential::bearer(Secret::new(token));
        Guarded::new(
            AuthorThreads {
                inner: Traced::new(
                    HttpTransport::new(HttpConfig::default(), Some(credential)),
                    trace.clone(),
                    actor,
                ),
                reply,
                reads: std::sync::atomic::AtomicUsize::new(0),
            },
            target.clone(),
            WriteScope::Loopback,
        )
    };
    let author = guard(AUTHOR_TOKEN, "author", author_threads);
    let reviewer = guard(REVIEWER_TOKEN, "reviewer", ThreadsReply::Keep);
    let second = InProcess {
        guard: second_guard(server, REVIEWER_TOKEN, &Trace::new()),
    };
    let cx = GhCx {
        author: &author,
        reviewer: &reviewer,
        trace: &trace,
        host_url: "github.com".to_owned(),
        reviewer_login: FAKE_REVIEWER.to_owned(),
        state_dir: state.path(),
        poll: NOW,
        second: &second,
    };
    let mut cell = Cell {
        names: CellNames::new(RUN_ID, scenario.name())?,
        default_branch: "main".to_owned(),
        number: NUMBER,
        base_commit: BASE.to_owned(),
        head_commit: HEAD.to_owned(),
        reviewer: Some(Account {
            login: FAKE_REVIEWER.to_owned(),
            id: 2,
            name: None,
        }),
    };
    let observed = match scenario {
        Scenario::GhLocalCreate => gh_local_create(&cx, &mut cell),
        Scenario::GhPendingCycle => gh_pending_cycle(&cx, &mut cell),
        Scenario::GhPendingForeign => gh_pending_foreign(&cx, &mut cell),
        Scenario::GhAnchors => gh_anchors(&cx, &mut cell),
        Scenario::GhViewed => gh_viewed(&cx, &mut cell),
        other => return Err(format!("тело {} в тесте не собрано", other.name()).into()),
    };
    let errors = guard_errors(&[("author", &author), ("reviewer", &reviewer)]);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(reviewer.refusal().is_none() && author.refusal().is_none());
    Ok(observed)
}

#[test]
fn local_create_body_passes_against_the_fake() -> TestResult {
    let server = fake_github(true)?;
    let observed =
        cell_body(&server, Scenario::GhLocalCreate)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(
        status,
        Status::Pass,
        "{reason}: {:?}",
        observed.checks.to_json()
    );
    for (what, value) in [
        ("до", &observed.before),
        ("после", &observed.after),
        ("журнал", &observed.journal),
    ] {
        assert!(!value.is_null(), "{what}");
    }
    assert_eq!(
        observed.journal.as_object().map(serde_json::Map::len),
        Some(3)
    );
    let states: Vec<ghf::ReviewState> = server
        .with(|fake| fake.reviews(NUMBER))
        .iter()
        .map(|review| review.state)
        .collect();
    assert_eq!(
        states,
        vec![
            ghf::ReviewState::Commented,
            ghf::ReviewState::Approved,
            ghf::ReviewState::ChangesRequested
        ]
    );
    assert_eq!(server.with(|fake| fake.comments(NUMBER)).len(), 6);
    assert_eq!(
        observed.after["reviews"].as_array().map(Vec::len),
        Some(3),
        "{}",
        observed.after
    );
    Ok(())
}

#[test]
fn refused_create_review_fails_the_body_against_the_fake() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_CREATE, FakeReply::Status(422)).skip(1));
    let observed =
        cell_body(&server, Scenario::GhLocalCreate)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(status, Status::Fail, "{reason}");
    assert!(reason.contains("approve"), "{reason}");
    Ok(())
}

#[test]
fn rate_limited_create_review_is_not_run_against_the_fake() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_CREATE, FakeReply::Status(429)));
    let verdict = cell_body(&server, Scenario::GhLocalCreate)?
        .err()
        .ok_or("ожидался итог без проверок")?;
    assert_eq!(
        verdict,
        Verdict::NotRun("лимит: код 429 на запись POST reviews".to_owned())
    );
    assert!(server.with(|fake| fake.reviews(NUMBER)).is_empty());
    Ok(())
}

#[test]
fn pull_without_files_is_not_run() -> TestResult {
    let server = fake_github(false)?;
    let verdict = cell_body(&server, Scenario::GhLocalCreate)?
        .err()
        .ok_or("ожидался not_run")?;
    match verdict {
        Verdict::NotRun(reason) => assert!(reason.starts_with(PR_NOT_READY), "{reason}"),
        other => return Err(format!("{other:?}").into()),
    }
    assert!(server.with(|fake| fake.reviews(NUMBER)).is_empty());
    Ok(())
}

#[test]
fn write_keys_hide_ids_and_name_mutations() {
    let key = |method, path: &str, body: Option<Value>| gh_write_key(method, path, body.as_ref());
    assert_eq!(
        key(Method::Post, "/repos/o/r/pulls/5/reviews", None).as_deref(),
        Some(GH_CREATE)
    );
    assert_eq!(
        key(Method::Post, "/repos/o/r/pulls/5/reviews/123/events", None).as_deref(),
        Some("POST reviews/{id}/events")
    );
    assert_eq!(key(Method::Get, "/repos/o/r/pulls/5/reviews", None), None);
    assert_eq!(
        key(
            Method::Post,
            "/graphql",
            Some(json!({"query": "mutation { markFileAsViewed(input: {}) { clientMutationId } }"}))
        )
        .as_deref(),
        Some("graphql markFileAsViewed")
    );
    assert_eq!(
        key(
            Method::Post,
            "/graphql",
            Some(json!({"query": "query { viewer { login } }"}))
        ),
        None
    );
    assert_eq!(
        key(Method::Post, "/repos/o/r/git/refs", None).as_deref(),
        Some("POST /repos/o/r/git/refs")
    );
}

#[test]
fn only_rows_with_a_body_open_a_live_cell() {
    assert!(implemented(Scenario::GhLocalCreate));
    assert!(implemented(Scenario::GhViewed));
    assert!(implemented(Scenario::GhPendingCycle));
    assert!(implemented(Scenario::GhAnchors));
    assert!(implemented(Scenario::GhPendingForeign));
    for scenario in [Scenario::ReconcileForeign, Scenario::Capabilities] {
        assert!(!implemented(scenario), "{}", scenario.name());
    }
}

/// Ключи мутаций просмотра в журнале `forge-fake`.
const FAKE_MARK: &str = "graphql markFileAsViewed";
const FAKE_UNMARK: &str = "graphql unmarkFileAsViewed";

#[test]
fn viewed_body_passes_against_the_fake() -> TestResult {
    let server = fake_github(true)?;
    let observed =
        cell_body(&server, Scenario::GhViewed)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(
        status,
        Status::Pass,
        "{reason}: {:?}",
        observed.checks.to_json()
    );
    assert_eq!(observed.verdict, None);
    for (what, value) in [
        ("до", &observed.before),
        ("после", &observed.after),
        ("журнал", &observed.journal),
    ] {
        assert!(!value.is_null(), "{what}");
    }
    assert_eq!(observed.before["reviewer"]["viewerViewedState"], "UNVIEWED");
    assert_eq!(observed.after["reviewer"]["after_mark"], "VIEWED");
    assert_eq!(observed.after["reviewer"]["after_unmark"], "UNVIEWED");
    assert!(
        server
            .with(|fake| fake.viewed(NUMBER, FAKE_REVIEWER))
            .is_empty()
    );
    let notes = observed.notes.join("\n");
    assert!(
        notes.contains("автора после отметки: \"UNVIEWED\""),
        "{notes}"
    );
    assert!(server.with(|fake| fake.reviews(NUMBER)).is_empty());
    Ok(())
}

#[test]
fn rate_limited_mark_is_not_run_against_the_fake() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_MARK, FakeReply::Status(429)));
    let verdict = cell_body(&server, Scenario::GhViewed)?
        .err()
        .ok_or("ожидался итог без проверок")?;
    assert_eq!(
        verdict,
        Verdict::NotRun(format!("лимит: код 429 на запись {GH_MARK}"))
    );
    assert!(
        server
            .with(|fake| fake.viewed(NUMBER, FAKE_REVIEWER))
            .is_empty()
    );
    Ok(())
}

#[test]
fn unmark_lost_on_the_server_is_not_run_and_refused_unmark_fails() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_UNMARK, FakeReply::Status(502)));
    let verdict = cell_body(&server, Scenario::GhViewed)?
        .err()
        .ok_or("ожидался итог без проверок")?;
    match verdict {
        Verdict::NotRun(reason) => assert!(reason.starts_with(WRITE_UNOBSERVED), "{reason}"),
        other => return Err(format!("{other:?}").into()),
    }
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_UNMARK, FakeReply::Status(403)));
    let observed =
        cell_body(&server, Scenario::GhViewed)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(status, Status::Fail, "{reason}");
    assert!(reason.contains("unmarkFileAsViewed"), "{reason}");
    assert_eq!(
        server
            .with(|fake| fake.viewed(NUMBER, FAKE_REVIEWER))
            .get(PATH)
            .copied(),
        Some(ghf::Viewed::Viewed)
    );
    Ok(())
}

#[test]
fn viewed_without_mutations_is_absent_without_writes() -> TestResult {
    let server = fake_github_with(true, |fake| {
        fake.remove_mutation("markFileAsViewed");
        fake.remove_mutation("unmarkFileAsViewed");
    })?;
    let observed =
        cell_body(&server, Scenario::GhViewed)?.map_err(|verdict| format!("{verdict:?}"))?;
    match &observed.verdict {
        Some(Verdict::Absent(reason)) => assert!(reason.contains("markFileAsViewed"), "{reason}"),
        other => return Err(format!("{other:?}").into()),
    }
    assert!(!observed.journal.is_null() && !observed.before.is_null());
    assert!(
        crate::scenario::fake::writes(&server.log()).is_empty(),
        "{:?}",
        crate::scenario::fake::writes(&server.log())
    );
    Ok(())
}

#[test]
fn viewed_oracle_catches_a_mark_that_did_not_stick() {
    let writes = vec![
        (
            GH_MARK.to_owned(),
            json!({"variables": {"input": {"pullRequestId": "PR_1", "path": PATH}}}),
            Some(200),
        ),
        (
            GH_UNMARK.to_owned(),
            json!({"variables": {"input": {"pullRequestId": "PR_1", "path": PATH}}}),
            Some(200),
        ),
    ];
    let facts = |after_mark, after_unmark, pull_node| ViewedFacts {
        pull_node,
        path: PATH,
        writes: &writes,
        after_mark,
        after_unmark,
        sessions: 1,
        submissions: 0,
    };
    let pass = viewed_checks(&facts(
        Some(ViewedState::Viewed),
        Some(ViewedState::Unviewed),
        "PR_1",
    ));
    assert_eq!(pass.verdict().0, Status::Pass);
    let (status, reason) = viewed_checks(&facts(
        Some(ViewedState::Viewed),
        Some(ViewedState::Viewed),
        "PR_1",
    ))
    .verdict();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("unmarkFileAsViewed"), "{reason}");
    let (status, reason) = viewed_checks(&facts(
        Some(ViewedState::Unviewed),
        Some(ViewedState::Unviewed),
        "PR_1",
    ))
    .verdict();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("после markFileAsViewed"), "{reason}");
    let (status, reason) = viewed_checks(&facts(
        Some(ViewedState::Viewed),
        Some(ViewedState::Unviewed),
        "PR_2",
    ))
    .verdict();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("вход"), "{reason}");
}

#[test]
fn write_verdicts_split_limit_unsent_unknown_and_refusal() {
    let limit = ApiError::RateLimited {
        status: 403,
        detail: "secondary rate limit".to_owned(),
    };
    assert_eq!(
        gh_write_verdict(GH_MARK, &limit),
        Some(Verdict::NotRun(format!(
            "лимит: код 403 на запись {GH_MARK}"
        )))
    );
    let refused = ApiError::Status {
        status: 403,
        message: "Resource not accessible".to_owned(),
        errors: Vec::new(),
    };
    assert_eq!(gh_write_verdict(GH_MARK, &refused), None);
    let unknown = ApiError::Status {
        status: 502,
        message: "bad gateway".to_owned(),
        errors: Vec::new(),
    };
    assert!(matches!(
        gh_write_verdict(GH_MARK, &unknown),
        Some(Verdict::NotRun(reason)) if reason.starts_with(WRITE_UNOBSERVED)
    ));
    let unsent = ApiError::NotSent(Failure::not_sent("соединение не установлено"));
    assert!(matches!(
        gh_write_verdict(GH_MARK, &unsent),
        Some(Verdict::Error(reason)) if reason.starts_with(&format!("запись {GH_MARK} не ушла"))
    ));
}

/// Ключи `forge-fake` для `gh-pending-cycle`.
const FAKE_SUBMIT: &str = "POST /repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/events";
const FAKE_ADD_THREAD: &str = "graphql addPullRequestReviewThread";
const FAKE_REVIEW: &str = "GET /repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}";

fn fake_writes(server: &FakeServer<GithubFake>, key: &str) -> usize {
    crate::scenario::fake::writes(&server.log())
        .get(key)
        .copied()
        .unwrap_or(0)
}

#[test]
fn pending_cycle_body_passes_against_the_fake() -> TestResult {
    let server = fake_github(true)?;
    let observed =
        cell_body(&server, Scenario::GhPendingCycle)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(
        status,
        Status::Pass,
        "{reason}: {:?}",
        observed.checks.to_json()
    );
    assert_eq!(observed.verdict, None);
    for (what, value) in [
        ("до", &observed.before),
        ("между", &observed.after["between"]),
        ("после", &observed.after["author"]),
        ("журнал", &observed.journal),
    ] {
        assert!(!value.is_null(), "{what}");
    }
    // След не доказывает, что первый Submit не ушёл: до сервера дошёл ровно один.
    assert_eq!(fake_writes(&server, FAKE_SUBMIT), 1);
    let reviews = server.with(|fake| fake.reviews(NUMBER));
    assert_eq!(
        reviews
            .iter()
            .map(|review| review.state)
            .collect::<Vec<_>>(),
        vec![ghf::ReviewState::Commented]
    );
    let bodies: Vec<String> = server
        .with(|fake| fake.comments(NUMBER))
        .iter()
        .map(|comment| comment.body.clone())
        .collect();
    assert_eq!(
        bodies,
        vec![PENDING_UPDATED.to_owned(), "второй".to_owned()]
    );
    let notes = observed.notes.join("\n");
    assert!(notes.contains(INJECTION_UNAVAILABLE), "{notes}");
    assert!(
        notes.contains(&format!(
            "инъекция: Submit review {} не отправлен",
            reviews[0].id
        )),
        "{notes}"
    );
    assert_eq!(
        observed.after["between"]["author"]["reviews"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
    Ok(())
}

#[test]
fn refused_thread_skips_the_second_submission_and_fails() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_ADD_THREAD, FakeReply::Status(422)).skip(1));
    let observed =
        cell_body(&server, Scenario::GhPendingCycle)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(status, Status::Fail, "{reason}");
    assert!(!observed.journal.is_null() && !observed.after["author"].is_null());
    let notes = observed.notes.join("\n");
    assert!(notes.contains("инъекция не сработала"), "{notes}");
    assert_eq!(fake_writes(&server, FAKE_SUBMIT), 0);
    assert_eq!(
        server
            .with(|fake| fake.reviews(NUMBER))
            .iter()
            .map(|review| review.state)
            .collect::<Vec<_>>(),
        vec![ghf::ReviewState::Pending]
    );
    Ok(())
}

#[test]
fn rate_limited_second_submit_is_not_run() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_SUBMIT, FakeReply::Status(429)));
    let verdict = cell_body(&server, Scenario::GhPendingCycle)?
        .err()
        .ok_or("ожидался итог без проверок")?;
    assert_eq!(
        verdict,
        Verdict::NotRun(format!("лимит: код 429 на запись {GH_SUBMIT}"))
    );
    Ok(())
}

#[test]
fn refused_pending_read_on_resume_fails_with_its_label() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_REVIEW, FakeReply::Status(404)));
    let verdict = cell_body(&server, Scenario::GhPendingCycle)?
        .err()
        .ok_or("ожидался итог без проверок")?;
    match verdict {
        Verdict::Fail(reason) => assert!(reason.contains("чтение PENDING"), "{reason}"),
        other => return Err(format!("{other:?}").into()),
    }
    Ok(())
}

#[test]
fn pending_without_mutations_is_absent_without_writes() -> TestResult {
    let server = fake_github_with(true, |fake| {
        fake.remove_mutation("addPullRequestReviewThread");
    })?;
    let observed =
        cell_body(&server, Scenario::GhPendingCycle)?.map_err(|verdict| format!("{verdict:?}"))?;
    match &observed.verdict {
        Some(Verdict::Absent(reason)) => {
            assert!(reason.contains("addPullRequestReviewThread"), "{reason}");
        }
        other => return Err(format!("{other:?}").into()),
    }
    assert!(!observed.journal.is_null() && !observed.before.is_null());
    assert!(crate::scenario::fake::writes(&server.log()).is_empty());
    Ok(())
}

fn call(actor: &'static str, method: Method, path: &str) -> Call {
    Call {
        seq: 0,
        actor,
        request: Request::new(method, format!("http://127.0.0.1:1{path}")),
        outcome: Ok(Response::new(200)),
        duration: Duration::ZERO,
    }
}

#[test]
fn pending_submit_verdict_splits_pull_reads_from_pending_reads() {
    let unexpected = SubmitError::GitHub(ApiError::Unexpected("свой комментарий пропал".into()));
    let pull = [
        call("reviewer", Method::Get, "/repos/o/r/pulls/5/files"),
        call("author", Method::Get, "/repos/o/r/pulls/5/reviews"),
    ];
    match gh_pending_submit_verdict(&unexpected, &pull, "reviewer") {
        Verdict::NotRun(reason) => assert!(reason.starts_with(PR_NOT_READY), "{reason}"),
        other => panic!("{other:?}"),
    }
    let pending = [call(
        "reviewer",
        Method::Get,
        "/repos/o/r/pulls/5/reviews/7/comments",
    )];
    assert_eq!(
        gh_pending_submit_verdict(&unexpected, &pending, "reviewer"),
        Verdict::Fail("отправка: чтение PENDING: свой комментарий пропал".to_owned())
    );
    let limit = SubmitError::GitHub(ApiError::RateLimited {
        status: 429,
        detail: "rate limit".to_owned(),
    });
    assert!(matches!(
        gh_pending_submit_verdict(&limit, &pending, "reviewer"),
        Verdict::NotRun(reason) if reason.starts_with("лимит: отправка: чтение PENDING")
    ));
    let transport = SubmitError::GitHub(ApiError::Transport(Failure::timeout("срок")));
    assert!(matches!(
        gh_pending_submit_verdict(&transport, &pending, "reviewer"),
        Verdict::Error(_)
    ));
    let refused = SubmitError::Refused("в PENDING чужие комментарии, не принятые явно: 9".into());
    assert!(matches!(
        gh_pending_submit_verdict(&refused, &pull, "reviewer"),
        Verdict::Fail(reason) if reason.contains("чужие комментарии")
    ));
    assert!(matches!(
        gh_pending_submit_verdict(&SubmitError::Empty, &pending, "reviewer"),
        Verdict::Error(_)
    ));
}

#[test]
fn submit_drop_fires_once_on_submit_only() -> TestResult {
    use crate::transport::scripted::{Reply, Scripted};
    let scripted = Scripted::new()
        .expect(
            Method::Post,
            "/repos/o/r/pulls/5/reviews",
            Reply::status(200),
        )
        .expect(
            Method::Post,
            "/repos/o/r/pulls/5/reviews/7/events",
            Reply::status(200),
        );
    let hook = SubmitDrop::new(&scripted);
    let send = |path: &str| {
        hook.send(&Request::new(
            Method::Post,
            format!("http://127.0.0.1:1{path}"),
        ))
    };
    assert!(send("/repos/o/r/pulls/5/reviews").is_ok());
    let dropped = send("/repos/o/r/pulls/5/reviews/7/events")
        .err()
        .ok_or("первый Submit ушёл")?;
    assert!(!dropped.may_have_been_sent());
    assert_eq!(hook.dropped().as_deref(), Some("7"));
    assert!(send("/repos/o/r/pulls/5/reviews/7/events").is_ok());
    scripted.finish()?;
    Ok(())
}

fn remote_review(id: &str, state: ReviewState) -> RemoteReview {
    RemoteReview {
        id: crate::model::ProviderId::new(id),
        node_id: crate::model::ProviderId::new(format!("PRR_{id}")),
        user: FAKE_REVIEWER.to_owned(),
        state,
        body: String::new(),
        commit_id: HEAD.to_owned(),
    }
}

fn remote_comment(id: &str, node: &str, line: u32, body: &str) -> RemoteComment {
    RemoteComment {
        id: crate::model::ProviderId::new(id),
        node_id: crate::model::ProviderId::new(node),
        review_id: Some(crate::model::ProviderId::new("7")),
        user: FAKE_REVIEWER.to_owned(),
        body: body.to_owned(),
        path: PATH.to_owned(),
        line: Some(line),
        side: Some("RIGHT".to_owned()),
        start_line: None,
        start_side: None,
        commit_id: HEAD.to_owned(),
        in_reply_to: None,
    }
}

fn submit_report(delivery: Delivery, closed: bool, items: &[(Operation, ItemState)]) -> Report {
    use crate::journal::{ItemId, SubmissionId, SubmitItem};
    Report {
        submission: SubmissionId(1),
        delivery,
        closed,
        unresolved: 0,
        items: items
            .iter()
            .zip(1u32..)
            .map(|((operation, state), seq)| SubmitItem {
                id: ItemId(u64::from(seq)),
                submission: SubmissionId(1),
                seq,
                operation: *operation,
                drafts: Vec::new(),
                state: *state,
                remote_id: None,
                candidate: None,
                detail: None,
                in_flight_at: None,
            })
            .collect(),
    }
}

/// Факты проходящего хода `gh-pending-cycle`: review 7 (`PRR_7`), комментарии 11 и 12.
struct PendingData {
    writes: Vec<WriteCall>,
    reviewer_between: Vec<ReviewRead>,
    author_between: Vec<ReviewRead>,
    after: Vec<ReviewRead>,
    first: Report,
    second: Report,
}

impl PendingData {
    fn passing() -> PendingData {
        let write = |key: &str, path: &str, body: Value, response: Option<Value>| WriteCall {
            key: key.to_owned(),
            path: path.to_owned(),
            body,
            status: Some(200),
            response,
        };
        let thread = json!({"variables": {"input": {"pullRequestReviewId": "PRR_7"}}});
        PendingData {
            writes: vec![
                write(
                    GH_CREATE,
                    "/repos/o/r/pulls/5/reviews",
                    json!({"commit_id": HEAD}),
                    Some(json!({"id": 7, "node_id": "PRR_7"})),
                ),
                write(GH_ADD_THREAD, "/graphql", thread.clone(), None),
                write(GH_ADD_THREAD, "/graphql", thread, None),
                write(
                    GH_UPDATE_COMMENT,
                    "/graphql",
                    json!({"variables": {"input": {"pullRequestReviewCommentId": "PRRC_11"}}}),
                    None,
                ),
                write(
                    GH_SUBMIT,
                    "/repos/o/r/pulls/5/reviews/7/events",
                    json!({"event": "COMMENT", "body": SUMMARY}),
                    None,
                ),
            ],
            reviewer_between: vec![(
                remote_review("7", ReviewState::Pending),
                vec![
                    remote_comment("11", "PRRC_11", 4, "первый"),
                    remote_comment("12", "PRRC_12", 24, "второй"),
                ],
            )],
            author_between: Vec::new(),
            after: vec![(
                remote_review("7", ReviewState::Commented),
                vec![
                    remote_comment("11", "PRRC_11", 4, PENDING_UPDATED),
                    remote_comment("12", "PRRC_12", 24, "второй"),
                ],
            )],
            first: submit_report(
                Delivery::NotSent,
                true,
                &[
                    (Operation::GitHubCreatePending, ItemState::Confirmed),
                    (Operation::GitHubAddPendingComment, ItemState::Confirmed),
                    (Operation::GitHubAddPendingComment, ItemState::Confirmed),
                    (Operation::GitHubSubmitReview, ItemState::Failed),
                ],
            ),
            second: submit_report(
                Delivery::Full,
                true,
                &[
                    (Operation::GitHubUpdatePendingComment, ItemState::Confirmed),
                    (Operation::GitHubSubmitReview, ItemState::Confirmed),
                ],
            ),
        }
    }

    fn checks(&self) -> (Status, String) {
        pending_cycle_checks(&PendingFacts {
            head: HEAD,
            path: PATH,
            reviewer: FAKE_REVIEWER,
            writes: &self.writes,
            dropped: Some("7"),
            reviewer_between: &self.reviewer_between,
            author_between: &self.author_between,
            after: &self.after,
            first: &self.first,
            second: Some(&self.second),
            recovered: 0,
        })
        .verdict()
    }
}

#[test]
fn pending_cycle_oracle_catches_its_own_claims() {
    let (status, reason) = PendingData::passing().checks();
    assert_eq!(status, Status::Pass, "{reason}");

    let mut data = PendingData::passing();
    if let Some(submit) = data.writes.last_mut() {
        submit.body["comments"] = json!([{"path": PATH, "line": 4, "body": "лишний"}]);
    }
    let (status, reason) = data.checks();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("без comments[]"), "{reason}");

    for (id, node) in [("99", "PRRC_11"), ("11", "PRRC_99")] {
        let mut data = PendingData::passing();
        data.after[0].1[0] = remote_comment(id, node, 4, PENDING_UPDATED);
        let (status, reason) = data.checks();
        assert_eq!(status, Status::Fail, "{id} {node}");
        assert!(reason.contains("прежние ID и node ID"), "{reason}");
    }

    let mut data = PendingData::passing();
    data.writes[3].body =
        json!({"variables": {"input": {"pullRequestReviewCommentId": "PRRC_12"}}});
    let (status, reason) = data.checks();
    assert_eq!(status, Status::Fail);
    assert!(
        reason.contains("по node ID первого комментария"),
        "{reason}"
    );

    let mut data = PendingData::passing();
    data.author_between = vec![(remote_review("7", ReviewState::Pending), Vec::new())];
    let (status, reason) = data.checks();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("автор между отправками"), "{reason}");
}

/// Ключи `forge-fake` для `gh-anchors`.
const FAKE_REPLY: &str = "POST /repos/{owner}/{repo}/pulls/{number}/comments/{comment_id}/replies";
const FAKE_RESOLVE: &str = "graphql resolveReviewThread";

#[test]
fn anchors_body_passes_against_the_fake() -> TestResult {
    let server = fake_github(true)?;
    let observed =
        cell_body(&server, Scenario::GhAnchors)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(
        status,
        Status::Pass,
        "{reason}: {:?}",
        observed.checks.to_json()
    );
    assert_eq!(observed.verdict, None);
    for (what, value) in [
        ("до", &observed.before),
        ("после", &observed.after["author"]),
        (
            "треды до resolve",
            &observed.after["threads_before_resolve"],
        ),
        (
            "треды после resolve",
            &observed.after["threads_after_resolve"],
        ),
        ("журнал", &observed.journal),
    ] {
        assert!(!value.is_null(), "{what}");
    }
    let comments = server.with(|fake| fake.comments(NUMBER));
    let anchors: Vec<(String, u32, ghf::DiffSide, Option<u32>)> = comments
        .iter()
        .map(|c| (c.body.clone(), c.line, c.side, c.start_line))
        .collect();
    assert_eq!(
        anchors,
        vec![
            ("удалённая 03".to_owned(), 3, ghf::DiffSide::Left, None),
            ("добавленная 24a".to_owned(), 24, ghf::DiffSide::Right, None),
            (
                "диапазон 24a–24b".to_owned(),
                25,
                ghf::DiffSide::Right,
                Some(24)
            ),
            (ANCHOR_REPLY.to_owned(), 3, ghf::DiffSide::Left, None),
        ]
    );
    let resolved: Vec<bool> = server
        .with(|fake| fake.threads(NUMBER))
        .iter()
        .map(|thread| thread.resolved)
        .collect();
    assert_eq!(resolved, vec![true, false, false]);
    assert_eq!(server.with(|fake| fake.reviews(NUMBER)).len(), 2);
    let notes = observed.notes.join("\n");
    assert!(notes.contains("отдельный review"), "{notes}");
    assert!(notes.contains("comments.totalCount 2"), "{notes}");
    Ok(())
}

#[test]
fn refused_reply_fails_with_artifacts_and_skips_resolve() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_REPLY, FakeReply::Status(422)));
    let observed =
        cell_body(&server, Scenario::GhAnchors)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(status, Status::Fail, "{reason}");
    assert!(reason.contains("ответы в reviews ревьюера"), "{reason}");
    assert!(!observed.journal.is_null() && !observed.after["author"].is_null());
    let notes = observed.notes.join("\n");
    assert!(
        notes.contains(&format!("отказ хоста на {GH_REPLY}")),
        "{notes}"
    );
    assert_eq!(fake_writes(&server, FAKE_RESOLVE), 0);
    Ok(())
}

#[test]
fn rate_limited_resolve_is_not_run() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_RESOLVE, FakeReply::Status(429)));
    let verdict = cell_body(&server, Scenario::GhAnchors)?
        .err()
        .ok_or("ожидался итог без проверок")?;
    assert_eq!(
        verdict,
        Verdict::NotRun(format!("лимит: код 429 на запись {GH_RESOLVE}"))
    );
    assert!(
        server
            .with(|fake| fake.threads(NUMBER))
            .iter()
            .all(|thread| !thread.resolved)
    );
    Ok(())
}

#[test]
fn anchors_without_resolve_is_absent_without_writes() -> TestResult {
    let server = fake_github_with(true, |fake| {
        fake.remove_mutation("resolveReviewThread");
    })?;
    let observed =
        cell_body(&server, Scenario::GhAnchors)?.map_err(|verdict| format!("{verdict:?}"))?;
    match &observed.verdict {
        Some(Verdict::Absent(reason)) => {
            assert!(reason.contains("resolveReviewThread"), "{reason}")
        }
        other => return Err(format!("{other:?}").into()),
    }
    assert!(!observed.journal.is_null() && !observed.before.is_null());
    assert!(crate::scenario::fake::writes(&server.log()).is_empty());
    Ok(())
}

#[test]
fn unobserved_root_thread_is_not_run_with_artifacts() -> TestResult {
    let server = fake_github(true)?;
    let observed = cell_body_with(&server, Scenario::GhAnchors, ThreadsReply::Strip)?
        .map_err(|verdict| format!("{verdict:?}"))?;
    match &observed.verdict {
        Some(Verdict::NotRun(reason)) => assert!(reason.starts_with(THREAD_UNOBSERVED), "{reason}"),
        other => return Err(format!("{other:?}").into()),
    }
    assert!(!observed.journal.is_null() && !observed.after["author"].is_null());
    assert_eq!(fake_writes(&server, FAKE_RESOLVE), 0);
    assert_eq!(fake_writes(&server, FAKE_REPLY), 1);
    Ok(())
}

#[test]
fn schema_error_on_the_first_thread_read_is_not_run_with_artifacts() -> TestResult {
    let server = fake_github(true)?;
    let observed = cell_body_with(
        &server,
        Scenario::GhAnchors,
        ThreadsReply::GraphqlError { at: 0 },
    )?
    .map_err(|verdict| format!("{verdict:?}"))?;
    match &observed.verdict {
        Some(Verdict::NotRun(reason)) => {
            assert!(reason.starts_with(THREAD_UNOBSERVED), "{reason}");
            assert!(reason.contains("ошибка схемы или формы ответа"), "{reason}");
            assert!(reason.ends_with("resolve не шёл"), "{reason}");
        }
        other => return Err(format!("{other:?}").into()),
    }
    assert!(!observed.journal.is_null() && !observed.before.is_null());
    assert!(!observed.after["author"].is_null());
    assert_eq!(fake_writes(&server, FAKE_REPLY), 1);
    assert_eq!(fake_writes(&server, FAKE_RESOLVE), 0);
    Ok(())
}

#[test]
fn form_error_on_the_thread_read_after_resolve_is_not_run_with_artifacts() -> TestResult {
    let server = fake_github(true)?;
    let observed = cell_body_with(
        &server,
        Scenario::GhAnchors,
        ThreadsReply::NoResolved { at: 1 },
    )?
    .map_err(|verdict| format!("{verdict:?}"))?;
    match &observed.verdict {
        Some(Verdict::NotRun(reason)) => {
            assert!(reason.starts_with(THREAD_UNOBSERVED), "{reason}");
            assert!(reason.contains("после resolve"), "{reason}");
            assert!(reason.contains("isResolved"), "{reason}");
            assert!(!reason.contains("не шёл"), "{reason}");
        }
        other => return Err(format!("{other:?}").into()),
    }
    assert!(!observed.journal.is_null() && !observed.after["author"].is_null());
    assert!(!observed.after["threads_before_resolve"].is_null());
    assert!(observed.after["threads_after_resolve"].is_null());
    assert_eq!(fake_writes(&server, FAKE_RESOLVE), 1);
    Ok(())
}

#[test]
fn limit_and_server_error_on_the_thread_read_keep_the_read_rule() -> TestResult {
    let server = fake_github(true)?;
    let limited = cell_body_with(
        &server,
        Scenario::GhAnchors,
        ThreadsReply::Status { at: 0, code: 429 },
    )?
    .err()
    .ok_or("ожидался итог без проверок")?;
    assert!(
        matches!(&limited, Verdict::NotRun(reason) if reason.starts_with("лимит: треды PR автором")),
        "{limited:?}"
    );
    let server = fake_github(true)?;
    let failed = cell_body_with(
        &server,
        Scenario::GhAnchors,
        ThreadsReply::Status { at: 1, code: 502 },
    )?
    .err()
    .ok_or("ожидался итог без проверок")?;
    assert!(
        matches!(&failed, Verdict::Error(reason) if reason.starts_with("треды PR автором после resolve")),
        "{failed:?}"
    );
    Ok(())
}

#[test]
fn threads_unread_splits_schema_and_form_from_other_failures() {
    let unexpected = ApiError::Unexpected("тред без isResolved".to_owned());
    assert!(matches!(
        threads_unread("треды", &unexpected),
        Ok(reason) if reason.starts_with(THREAD_UNOBSERVED) && reason.contains("тред без isResolved")
    ));
    let limit = ApiError::RateLimited {
        status: 200,
        detail: "API rate limit exceeded".to_owned(),
    };
    assert!(matches!(
        threads_unread("треды", &limit),
        Err(Verdict::NotRun(reason)) if reason.starts_with("лимит: треды")
    ));
    let server = ApiError::Status {
        status: 502,
        message: "bad gateway".to_owned(),
        errors: Vec::new(),
    };
    assert!(matches!(
        threads_unread("треды", &server),
        Err(Verdict::Error(_))
    ));
    let transport = ApiError::Transport(Failure::not_sent("обрыв"));
    assert!(matches!(
        threads_unread("треды", &transport),
        Err(Verdict::Error(_))
    ));
}

#[test]
fn anchor_write_notes_unexpected_and_splits_the_other_outcomes() {
    let mut notes = Vec::new();
    assert_eq!(anchor_write(GH_REPLY, Ok(()), &mut notes), Ok(true));
    assert!(notes.is_empty());
    let unexpected = Err(ApiError::Unexpected("ответ без id".to_owned()));
    assert_eq!(anchor_write(GH_REPLY, unexpected, &mut notes), Ok(true));
    assert!(
        notes[0].contains("ответ 2xx не прошёл проверку адаптера: ответ без id"),
        "{notes:?}"
    );
    let limit = Err(ApiError::RateLimited {
        status: 429,
        detail: "too many".to_owned(),
    });
    assert_eq!(
        anchor_write(GH_RESOLVE, limit, &mut notes),
        Err(Verdict::NotRun(format!(
            "лимит: код 429 на запись {GH_RESOLVE}"
        )))
    );
    let unsent = Err(ApiError::NotSent(Failure::not_sent(
        "соединение не установлено",
    )));
    assert!(matches!(
        anchor_write(GH_REPLY, unsent, &mut notes),
        Err(Verdict::Error(reason)) if reason.starts_with(&format!("запись {GH_REPLY} не ушла"))
    ));
    let unknown = Err(ApiError::Status {
        status: 502,
        message: "bad gateway".to_owned(),
        errors: Vec::new(),
    });
    assert!(matches!(
        anchor_write(GH_RESOLVE, unknown, &mut notes),
        Err(Verdict::NotRun(reason)) if reason.starts_with(WRITE_UNOBSERVED)
    ));
    assert_eq!(notes.len(), 1, "{notes:?}");
    let refused = Err(ApiError::Status {
        status: 422,
        message: "Validation Failed".to_owned(),
        errors: Vec::new(),
    });
    assert_eq!(anchor_write(GH_REPLY, refused, &mut notes), Ok(false));
    assert!(
        notes[1].starts_with(&format!("отказ хоста на {GH_REPLY}")),
        "{notes:?}"
    );
}

/// Факты проходящего хода `gh-anchors`: review 7, корень 11 в треде `PRRT_1`, ответ 14.
struct AnchorData {
    writes: Vec<WriteCall>,
    after: Vec<ReviewRead>,
    report: Report,
    thread: &'static str,
    resolved_after: Option<bool>,
}

impl AnchorData {
    fn passing() -> AnchorData {
        let write = |key: &str, path: &str, body: Value, response: Option<Value>| WriteCall {
            key: key.to_owned(),
            path: path.to_owned(),
            body,
            status: Some(201),
            response,
        };
        let anchored = |id: &str, line, side: &str, start: Option<u32>, body: &str| RemoteComment {
            line: Some(line),
            side: Some(side.to_owned()),
            start_line: start,
            start_side: start.map(|_| "RIGHT".to_owned()),
            ..remote_comment(id, &format!("PRRC_{id}"), line, body)
        };
        let mut reply = anchored("14", 3, "LEFT", None, ANCHOR_REPLY);
        reply.review_id = Some(crate::model::ProviderId::new("8"));
        reply.in_reply_to = Some(crate::model::ProviderId::new("11"));
        let mut report = submit_report(
            Delivery::Full,
            true,
            &[(Operation::GitHubCreateReview, ItemState::Confirmed)],
        );
        report.items[0].remote_id = Some(crate::model::ProviderId::new("7"));
        AnchorData {
            writes: vec![
                write(
                    GH_CREATE,
                    "/repos/o/r/pulls/5/reviews",
                    json!({"event": "COMMENT", "commit_id": HEAD, "comments": [
                        {"path": PATH, "line": 3, "side": "LEFT"},
                        {"path": PATH, "line": 24, "side": "RIGHT"},
                        {"path": PATH, "line": 25, "side": "RIGHT", "start_line": 24, "start_side": "RIGHT"},
                    ]}),
                    Some(json!({"id": 7})),
                ),
                write(
                    GH_REPLY,
                    "/repos/o/r/pulls/5/comments/11/replies",
                    json!({"body": ANCHOR_REPLY}),
                    None,
                ),
                write(
                    GH_RESOLVE,
                    "/graphql",
                    json!({"variables": {"input": {"threadId": "PRRT_1"}}}),
                    None,
                ),
            ],
            after: vec![
                (
                    remote_review("7", ReviewState::Commented),
                    vec![
                        anchored("11", 3, "LEFT", None, "удалённая 03"),
                        anchored("12", 24, "RIGHT", None, "добавленная 24a"),
                        anchored("13", 25, "RIGHT", Some(24), "диапазон 24a–24b"),
                    ],
                ),
                (remote_review("8", ReviewState::Commented), vec![reply]),
            ],
            report,
            thread: "PRRT_1",
            resolved_after: Some(true),
        }
    }

    fn checks(&self) -> (Status, String) {
        gh_anchors_checks(&AnchorFacts {
            head: HEAD,
            path: PATH,
            reviewer: FAKE_REVIEWER,
            writes: &self.writes,
            root: Some("11"),
            thread: Some(self.thread),
            resolved_before: Some(false),
            resolved_after: self.resolved_after,
            after: &self.after,
            report: &self.report,
            recovered: 0,
        })
        .verdict()
    }
}

#[test]
fn anchors_oracle_catches_anchor_reply_and_resolve_errors() {
    let (status, reason) = AnchorData::passing().checks();
    assert_eq!(status, Status::Pass, "{reason}");

    let mut data = AnchorData::passing();
    data.writes[0].body["comments"][0]["side"] = json!("RIGHT");
    let (status, reason) = data.checks();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("якоря в comments[]"), "{reason}");

    let mut data = AnchorData::passing();
    data.after[0].1[2].start_line = None;
    let (status, reason) = data.checks();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("комментарии review Create"), "{reason}");

    let mut data = AnchorData::passing();
    data.writes[1].path = "/repos/o/r/pulls/5/comments/12/replies".to_owned();
    let (status, reason) = data.checks();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("ответ на REST ID корня"), "{reason}");

    let mut data = AnchorData::passing();
    data.after[1].1[0].in_reply_to = Some(crate::model::ProviderId::new("12"));
    let (status, reason) = data.checks();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("ответы в reviews ревьюера"), "{reason}");

    let mut data = AnchorData::passing();
    data.thread = "PRRT_2";
    let (status, reason) = data.checks();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("по node ID треда корня"), "{reason}");

    let mut data = AnchorData::passing();
    data.resolved_after = Some(false);
    let (status, reason) = data.checks();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("после resolve разрешён"), "{reason}");

    let mut data = AnchorData::passing();
    data.writes.remove(1);
    let (status, reason) = data.checks();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("записи ревьюера"), "{reason}");
}

#[test]
fn replied_root_reads_the_comment_id() {
    assert_eq!(
        replied_root("/repos/o/r/pulls/5/comments/11/replies").as_deref(),
        Some("11")
    );
    assert_eq!(replied_root("/repos/o/r/pulls/5/comments/11"), None);
}

// --- второй клиент GitHub: PENDING ревьюера одним Create review без event ---

fn second_guard(
    server: &FakeServer<GithubFake>,
    token: &str,
    trace: &Trace,
) -> Guarded<Traced<HttpTransport>> {
    Guarded::new(
        Traced::new(
            HttpTransport::new(
                HttpConfig::default(),
                Some(Credential::bearer(Secret::new(token))),
            ),
            trace.clone(),
            ACTOR_SECOND,
        ),
        GuardTarget::github(&Endpoints::single(server.base()), REPO),
        WriteScope::Loopback,
    )
}

fn second_pending(
    server: &FakeServer<GithubFake>,
    count: u32,
) -> (Result<String, Verdict>, Trace, Scrubber) {
    let trace = Trace::new();
    let author = second_guard(server, AUTHOR_TOKEN, &trace);
    let reviewer = second_guard(server, REVIEWER_TOKEN, &trace);
    let mut scrubber = Scrubber::new();
    scrubber
        .add_token(AUTHOR_TOKEN)
        .add_token(REVIEWER_TOKEN)
        .add_repo(REPO);
    let done = crate::scenario::live::second_cloud_op(
        crate::results::Column::GithubCloud,
        &author,
        &reviewer,
        NUMBER,
        &super::super::SecondOp::GhCreatePending { count },
        &mut scrubber,
    );
    if !matches!(done, Err(Verdict::Error(_))) {
        assert!(author.refused_writes().is_empty() && reviewer.refused_writes().is_empty());
    }
    (done, trace, scrubber)
}

#[test]
fn second_client_creates_the_reviewer_pending_with_all_comments_in_one_request() -> TestResult {
    let server = fake_github(true)?;
    let (done, trace, scrubber) = second_pending(&server, 101);
    let id = done.map_err(|v| format!("{v:?}"))?;
    let reviews = server.with(|f| f.reviews(NUMBER));
    assert_eq!(
        reviews
            .iter()
            .map(|r| (r.id.to_string(), r.user.as_str(), r.state, r.body.as_str()))
            .collect::<Vec<_>>(),
        vec![(
            id.clone(),
            FAKE_REVIEWER,
            ghf::ReviewState::Pending,
            FOREIGN_PENDING_BODY
        )]
    );
    let comments = server.with(|f| f.comments(NUMBER));
    assert_eq!(comments.len(), 101);
    for (index, comment) in comments.iter().enumerate() {
        let index = u32::try_from(index)?;
        assert_eq!(comment.body, foreign_comment_body(index));
        assert_eq!(comment.line, FOREIGN_PENDING_LINES[index as usize % 2]);
        assert_eq!(comment.review_id.to_string(), id);
    }
    let calls = trace.calls();
    let writes: Vec<&Call> = calls
        .iter()
        .filter(|c| c.request.method.is_write())
        .collect();
    assert_eq!(writes.len(), 1, "один Create review");
    let body = writes[0].request.body_json().ok_or("тело Create review")?;
    assert!(body.get("event").is_none(), "без event: {body}");
    assert_eq!(body["comments"].as_array().map(Vec::len), Some(101));
    assert!(calls.iter().all(|c| c.actor == ACTOR_SECOND));
    let text = serde_json::to_string(&trace.entries(&scrubber))?;
    for secret in [
        FAKE_AUTHOR,
        FAKE_REVIEWER,
        REPO,
        AUTHOR_TOKEN,
        REVIEWER_TOKEN,
    ] {
        assert!(!text.contains(secret), "{secret} в следе");
    }
    Ok(())
}

#[test]
fn second_client_pending_refusals_split_not_run_limit_and_error() -> TestResult {
    for (code, expect) in [
        (403, "второй клиент не создал PENDING"),
        (422, "второй клиент не создал PENDING"),
        (429, "лимит"),
    ] {
        let server = fake_github(true)?;
        server.inject(Fault::new(FAKE_CREATE, FakeReply::Status(code)));
        let (done, _, _) = second_pending(&server, 3);
        assert!(
            matches!(&done, Err(Verdict::NotRun(reason)) if reason.contains(expect)),
            "{code}: {done:?}"
        );
        assert!(server.with(|f| f.reviews(NUMBER)).is_empty());
    }
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_CREATE, FakeReply::Status(502)));
    let (done, _, _) = second_pending(&server, 3);
    assert!(matches!(&done, Err(Verdict::Error(_))), "{done:?}");
    Ok(())
}

#[test]
fn second_client_meets_an_existing_pending_with_the_hosts_refusal() -> TestResult {
    let server = fake_github(true)?;
    let (first, _, _) = second_pending(&server, 2);
    first.map_err(|v| format!("{v:?}"))?;
    let (again, _, _) = second_pending(&server, 2);
    assert!(
        matches!(&again, Err(Verdict::NotRun(reason))
            if reason.contains("второй клиент не создал PENDING") && reason.contains("422")),
        "{again:?}"
    );
    assert_eq!(server.with(|f| f.reviews(NUMBER)).len(), 1);
    Ok(())
}

#[test]
fn second_client_refuses_a_public_repository_without_writes() -> TestResult {
    let server = fake_github_with(true, |f| f.set_private(false))?;
    let (done, trace, _) = second_pending(&server, 2);
    assert!(matches!(&done, Err(Verdict::Error(_))), "{done:?}");
    assert!(server.with(|f| f.reviews(NUMBER)).is_empty());
    assert!(
        trace
            .calls()
            .iter()
            .all(|call| !call.request.method.is_write())
    );
    Ok(())
}

// --- gh-pending-foreign: тело против forge-fake ---

#[test]
fn pending_foreign_body_passes_against_the_fake() -> TestResult {
    let server = fake_github(true)?;
    let observed = cell_body(&server, Scenario::GhPendingForeign)?.map_err(|v| format!("{v:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(status, Status::Pass, "{reason}; {:?}", observed.notes);
    assert!(observed.verdict.is_none());
    assert!(
        observed
            .notes
            .iter()
            .any(|n| n.contains("гипотеза 422") && n.contains("422")),
        "{:?}",
        observed.notes
    );
    let reviews = server.with(|f| f.reviews(NUMBER));
    assert_eq!(reviews.len(), 1);
    assert_eq!(reviews[0].state, ghf::ReviewState::Commented);
    assert_eq!(server.with(|f| f.comments(NUMBER)).len(), 102);
    assert!(observed.journal.is_object() || observed.journal.is_array());
    Ok(())
}

#[test]
fn pending_foreign_second_client_refusal_is_not_run_before_writes() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_CREATE, FakeReply::Status(403)));
    let done = cell_body(&server, Scenario::GhPendingForeign)?;
    assert!(
        matches!(&done, Err(Verdict::NotRun(reason)) if reason.contains("второй клиент не создал PENDING")),
        "{:?}",
        done.as_ref().err()
    );
    assert!(server.with(|f| f.reviews(NUMBER)).is_empty());
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_CREATE, FakeReply::Status(429)));
    let done = cell_body(&server, Scenario::GhPendingForeign)?;
    assert!(
        matches!(&done, Err(Verdict::NotRun(reason)) if reason.contains("лимит")),
        "{:?}",
        done.as_ref().err()
    );
    Ok(())
}

#[test]
fn pending_foreign_submit_limit_is_not_run() -> TestResult {
    let server = fake_github(true)?;
    server.inject(Fault::new(FAKE_SUBMIT, FakeReply::Status(429)));
    let done = cell_body(&server, Scenario::GhPendingForeign)?;
    assert!(
        matches!(&done, Err(Verdict::NotRun(reason)) if reason.contains("лимит")),
        "{:?}",
        done.as_ref().err()
    );
    Ok(())
}

#[test]
fn pending_foreign_without_server_pending_is_absent_without_writes() -> TestResult {
    let server = fake_github_with(true, |f| f.remove_mutation("addPullRequestReviewThread"))?;
    let observed = cell_body(&server, Scenario::GhPendingForeign)?.map_err(|v| format!("{v:?}"))?;
    assert!(
        matches!(observed.verdict, Some(Verdict::Absent(_))),
        "{:?}",
        observed.verdict
    );
    assert!(
        server.with(|f| f.reviews(NUMBER)).is_empty(),
        "второй клиент не запускался"
    );
    Ok(())
}

#[test]
fn pending_foreign_checks_catch_a_partial_read_and_an_overwrite() {
    let report = submit_report(Delivery::Full, true, &[]);
    let facts = ForeignFacts {
        planted: "9",
        found: None,
        before_adopt: &[],
        after_adopt: &[],
        after: &[],
        reviewer: FAKE_REVIEWER,
        first: &report,
        first_closed: true,
        second: None,
        foreign_in_spec: None,
    };
    let (status, reason) = pending_foreign_checks(&facts).verdict();
    assert_eq!(status, Status::Fail);
    assert!(reason.contains("PENDING существует"), "{reason}");
    let write = WriteCall {
        key: GH_CREATE.to_owned(),
        path: "/repos/o/r/pulls/5/reviews".to_owned(),
        body: json!({}),
        status: Some(200),
        response: None,
    };
    let facts = ForeignFacts {
        before_adopt: std::slice::from_ref(&write),
        ..facts
    };
    let (_, reason) = pending_foreign_checks(&facts).verdict();
    assert!(reason.contains("записи до принятия"), "{reason}");
}
