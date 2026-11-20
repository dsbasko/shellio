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

/// Тело клетки строки над сторожами (`WriteScope::Loopback`) против `forge-fake`, клетка собрана
/// вручную, как после `open_cell`.
fn cell_body(
    server: &FakeServer<GithubFake>,
    scenario: Scenario,
) -> Result<Result<Observed, Verdict>, Box<dyn std::error::Error>> {
    let state = tempfile::tempdir()?;
    let trace = Trace::new();
    let target = GuardTarget::github(&Endpoints::single(server.base()), REPO);
    let guard = |token: &str, actor| {
        let credential = Credential::bearer(Secret::new(token));
        Guarded::new(
            Traced::new(
                HttpTransport::new(HttpConfig::default(), Some(credential)),
                trace.clone(),
                actor,
            ),
            target.clone(),
            WriteScope::Loopback,
        )
    };
    let author = guard(AUTHOR_TOKEN, "author");
    let reviewer = guard(REVIEWER_TOKEN, "reviewer");
    let cx = GhCx {
        author: &author,
        reviewer: &reviewer,
        trace: &trace,
        host_url: "github.com".to_owned(),
        reviewer_login: FAKE_REVIEWER.to_owned(),
        state_dir: state.path(),
        poll: NOW,
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
    for scenario in [
        Scenario::GhPendingForeign,
        Scenario::GhAnchors,
        Scenario::ReconcileForeign,
        Scenario::Capabilities,
    ] {
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
