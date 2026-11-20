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
    for scenario in [
        Scenario::GhPendingCycle,
        Scenario::GhPendingForeign,
        Scenario::GhAnchors,
        Scenario::GhViewed,
        Scenario::ReconcileForeign,
        Scenario::Capabilities,
    ] {
        assert!(!implemented(scenario), "{}", scenario.name());
    }
}
