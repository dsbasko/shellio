//! Тесты живого контура против встроенного транспорта и подставной команды `security`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::github::GitHubRepo;
use super::gitlab::GitLabRepo;
use super::guard::{GuardError, GuardTarget, Guarded, WriteScope, is_loopback};
use super::keychain::Keychain;
use super::lifecycle::{
    CellNames, Cleanup, Created, FIXTURE_PATH, Fixture, Ledger, Roles, cleanup, resume_cleanup,
    run_cell, run_prefix,
};
use super::repo::{LiveRepo, base64};
use super::sandbox::{Sandbox, SandboxError};
use super::{Preflight, credential, preflight};
use crate::results::Column;
use crate::scrub::Scrubber;
use crate::transport::http::{AuthScheme, Secret};
use crate::transport::scripted::{Reply, Scripted};
use crate::transport::{Method, Request, Transport};
use crate::{github, gitlab};

const GH: &str = "/repos/owner/sandbox";
const GL: &str = "/api/v4/projects/group%2Fsandbox";

fn gh_target() -> GuardTarget {
    GuardTarget::github(
        &github::Endpoints::new("https://gh.test", "https://gh.test/graphql"),
        "owner/sandbox",
    )
}

fn gl_target() -> GuardTarget {
    GuardTarget::gitlab(
        &gitlab::Endpoints::instance("https://gl.test"),
        "group/sandbox",
    )
}

fn listed(repo: &str) -> WriteScope {
    WriteScope::Listed(vec![repo.to_owned()])
}

fn gh_repo(private: Value) -> Reply {
    Reply::json(
        200,
        &json!({"full_name": "owner/sandbox", "private": private, "default_branch": "main"}),
    )
}

fn gl_project(visibility: &str) -> Reply {
    Reply::json(
        200,
        &json!({
            "path_with_namespace": "group/sandbox",
            "visibility": visibility,
            "default_branch": "main",
        }),
    )
}

fn not_found() -> Reply {
    Reply::json(404, &json!({"message": "404 Not Found"}))
}

fn names() -> CellNames {
    CellNames {
        base: "probe/run1-cell-base".to_owned(),
        head: "probe/run1-cell".to_owned(),
    }
}

fn body(request: &Request) -> Value {
    request.body_json().unwrap_or(Value::Null)
}

fn ledger_in(dir: &Path) -> Result<Ledger, String> {
    Ledger::create(&dir.join(super::lifecycle::LEDGER_FILE))
}

// --- сторож записи ---

#[test]
fn guard_opens_for_listed_private_repo_and_scopes_writes() -> Result<(), String> {
    let scripted = Scripted::new()
        .expect(Method::Get, GH, gh_repo(json!(true)))
        .expect(
            Method::Post,
            format!("{GH}/pulls"),
            Reply::json(201, &json!({})),
        )
        .expect(Method::Post, "/graphql", Reply::json(200, &json!({})));
    let guard = Guarded::new(&scripted, gh_target(), listed("Owner/Sandbox"));
    let info = guard.check().map_err(|e| e.to_string())?;
    assert_eq!(info.default_branch, "main");
    assert!(
        guard
            .send(&Request::new(
                Method::Post,
                format!("https://gh.test{GH}/pulls")
            ))
            .is_ok()
    );
    let foreign = guard.send(&Request::new(
        Method::Post,
        "https://gh.test/repos/owner/other/pulls",
    ));
    assert!(foreign.is_err_and(|failure| !failure.may_have_been_sent()));
    assert!(
        guard
            .send(&Request::new(Method::Post, "https://gh.test/graphql"))
            .is_ok()
    );
    assert_eq!(guard.refused_writes(), ["POST /repos/owner/other/pulls"]);
    scripted.finish()?;
    Ok(())
}

#[test]
fn guard_refuses_public_repo_after_one_read() -> Result<(), String> {
    let scripted = Scripted::new().expect(Method::Get, GH, gh_repo(json!(false)));
    let guard = Guarded::new(&scripted, gh_target(), listed("owner/sandbox"));
    let write = guard.send(&Request::new(
        Method::Post,
        format!("https://gh.test{GH}/git/refs"),
    ));
    assert!(write.is_err_and(|failure| !failure.may_have_been_sent()));
    assert_eq!(
        guard.refusal(),
        Some(GuardError::NotPrivate("private: false".to_owned()))
    );
    assert!(guard.check().is_err());
    assert_eq!(
        guard.refused_writes(),
        ["POST /repos/owner/sandbox/git/refs"]
    );
    let delivered = scripted.delivered();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].method, Method::Get);
    scripted.finish()?;
    Ok(())
}

#[test]
fn guard_refuses_unlisted_repo_without_requests() -> Result<(), String> {
    let scripted = Scripted::new();
    let guard = Guarded::new(&scripted, gh_target(), listed("owner/other"));
    assert_eq!(guard.check(), Err(GuardError::NotListed));
    let write = guard.send(&Request::new(
        Method::Post,
        format!("https://gh.test{GH}/pulls"),
    ));
    assert!(write.is_err());
    assert!(scripted.requests().is_empty());
    let empty = Guarded::new(&scripted, gh_target(), WriteScope::Listed(Vec::new()));
    assert_eq!(empty.check(), Err(GuardError::NotListed));
    assert!(scripted.requests().is_empty());
    Ok(())
}

#[test]
fn guard_fails_closed_on_unconfirmed_privacy_and_names() -> Result<(), String> {
    let cases: [(GuardTarget, Reply, GuardError); 6] = [
        (
            gh_target(),
            Reply::json(
                200,
                &json!({"full_name": "owner/sandbox", "default_branch": "main"}),
            ),
            GuardError::NotPrivate("private не подтверждён".to_owned()),
        ),
        (
            gh_target(),
            gh_repo(json!("true")),
            GuardError::NotPrivate("private не подтверждён".to_owned()),
        ),
        (
            gl_target(),
            gl_project("internal"),
            GuardError::NotPrivate("visibility: internal".to_owned()),
        ),
        (
            gl_target(),
            Reply::json(
                200,
                &json!({"path_with_namespace": "group/other", "visibility": "private", "default_branch": "main"}),
            ),
            GuardError::Mismatch,
        ),
        (
            gl_target(),
            Reply::json(
                200,
                &json!({"path_with_namespace": "group/sandbox", "visibility": "private", "default_branch": null}),
            ),
            GuardError::NoDefaultBranch,
        ),
        (gl_target(), not_found(), GuardError::Denied(404)),
    ];
    for (target, reply, expected) in cases {
        let scripted = Scripted::new().then(reply);
        let repo = target.repo.clone();
        let guard = Guarded::new(&scripted, target, listed(&repo));
        assert_eq!(guard.check(), Err(expected));
        assert_eq!(scripted.delivered().len(), 1);
        scripted.finish()?;
    }
    Ok(())
}

#[test]
fn loopback_scope_checks_address_before_any_request() -> Result<(), String> {
    let local = GuardTarget::gitlab(
        &gitlab::Endpoints::instance("http://127.0.0.1:8929"),
        "root/shellio-review-sandbox",
    );
    let scripted = Scripted::new().expect(
        Method::Get,
        "/api/v4/projects/root%2Fshellio-review-sandbox",
        Reply::json(
            200,
            &json!({
                "path_with_namespace": "root/shellio-review-sandbox",
                "visibility": "private",
                "default_branch": "main",
            }),
        ),
    );
    let guard = Guarded::new(&scripted, local, WriteScope::Loopback);
    assert!(guard.check().is_ok());
    scripted.finish()?;

    let remote = GuardTarget::gitlab(
        &gitlab::Endpoints::instance("http://localhost:8929"),
        "root/shellio-review-sandbox",
    );
    let silent = Scripted::new();
    let guard = Guarded::new(&silent, remote, WriteScope::Loopback);
    assert_eq!(guard.check(), Err(GuardError::NotLoopback));
    assert!(silent.requests().is_empty());

    for (url, expected) in [
        ("http://127.0.0.1:8929/api/v4", true),
        ("https://127.0.0.1/api/graphql", true),
        ("http://127.0.0.1", true),
        ("http://127.0.0.10:80/api", false),
        ("http://127.0.0.1.example.com/api", false),
        ("http://user@127.0.0.1:80/api", false),
        ("http://127.0.0.1:/api", false),
        ("https://gitlab.com/api/v4", false),
        ("127.0.0.1:8929", false),
    ] {
        assert_eq!(is_loopback(url), expected, "{url}");
    }
    Ok(())
}

#[test]
fn first_write_runs_the_check_itself() -> Result<(), String> {
    let scripted = Scripted::new()
        .expect(Method::Get, GL, gl_project("private"))
        .expect(
            Method::Post,
            format!("{GL}/repository/branches"),
            Reply::json(201, &json!({})),
        );
    let guard = Guarded::new(&scripted, gl_target(), listed("group/sandbox"));
    let write = Request::new(
        Method::Post,
        format!("https://gl.test{GL}/repository/branches"),
    );
    assert!(guard.send(&write).is_ok());
    assert!(guard.refused_writes().is_empty());
    scripted.finish()?;
    Ok(())
}

// --- жизненный цикл GitHub ---

fn gh_open_author(s: Scripted) -> Scripted {
    s.expect(Method::Get, GH, gh_repo(json!(true)))
        .expect(
            Method::Get,
            format!("{GH}/git/ref/heads/main"),
            Reply::json(200, &json!({"object": {"sha": "d0"}})),
        )
        .expect(
            Method::Post,
            format!("{GH}/git/refs"),
            Reply::json(201, &json!({"ref": "refs/heads/probe/run1-cell-base"})),
        )
        .expect(
            Method::Put,
            format!("{GH}/contents/probe-review/fixture.txt"),
            Reply::json(
                201,
                &json!({"content": {"sha": "blob1"}, "commit": {"sha": "b1"}}),
            ),
        )
        .expect(
            Method::Post,
            format!("{GH}/git/refs"),
            Reply::json(201, &json!({"ref": "refs/heads/probe/run1-cell"})),
        )
        .expect(
            Method::Get,
            format!("{GH}/contents/probe-review/fixture.txt?ref=probe%2Frun1-cell"),
            Reply::json(200, &json!({"sha": "blob1"})),
        )
        .expect(
            Method::Put,
            format!("{GH}/contents/probe-review/fixture.txt"),
            Reply::json(
                200,
                &json!({"content": {"sha": "blob2"}, "commit": {"sha": "h1"}}),
            ),
        )
        .expect(
            Method::Post,
            format!("{GH}/pulls"),
            Reply::json(201, &json!({"number": 7, "state": "open"})),
        )
}

fn gh_cleanup_author(s: Scripted, pull_open: bool) -> Scripted {
    let s = s
        .expect(
            Method::Get,
            format!(
                "{GH}/pulls?state=open&head=owner%3Aprobe%2Frun1-cell-base&per_page=100&page=1"
            ),
            Reply::json(200, &json!([])),
        )
        .expect(
            Method::Get,
            format!("{GH}/pulls?state=open&head=owner%3Aprobe%2Frun1-cell&per_page=100&page=1"),
            Reply::json(200, &json!([{"number": 7}])),
        )
        .expect(
            Method::Get,
            format!("{GH}/pulls/7"),
            Reply::json(
                200,
                &json!({"state": if pull_open { "open" } else { "closed" }}),
            ),
        );
    let s = if pull_open {
        s.expect(
            Method::Patch,
            format!("{GH}/pulls/7"),
            Reply::json(200, &json!({"state": "closed"})),
        )
    } else {
        s
    };
    s.expect(
        Method::Delete,
        format!("{GH}/git/refs/heads/probe/run1-cell"),
        Reply::status(204),
    )
    .expect(
        Method::Delete,
        format!("{GH}/git/refs/heads/probe/run1-cell-base"),
        Reply::status(204),
    )
    .expect(
        Method::Get,
        format!("{GH}/pulls/7"),
        Reply::json(200, &json!({"state": "closed"})),
    )
    .expect(
        Method::Get,
        format!("{GH}/git/ref/heads/probe/run1-cell-base"),
        not_found(),
    )
    .expect(
        Method::Get,
        format!("{GH}/git/ref/heads/probe/run1-cell"),
        not_found(),
    )
    .expect(
        Method::Get,
        format!("{GH}/pulls?state=open&head=owner%3Aprobe%2Frun1-cell-base&per_page=100&page=1"),
        Reply::json(200, &json!([])),
    )
    .expect(
        Method::Get,
        format!("{GH}/pulls?state=open&head=owner%3Aprobe%2Frun1-cell&per_page=100&page=1"),
        Reply::json(200, &json!([])),
    )
}

#[test]
fn github_cell_opens_moves_head_and_cleans_up_with_reviewer() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let author_t = gh_open_author(Scripted::new())
        .expect(
            Method::Get,
            format!("{GH}/contents/probe-review/fixture.txt?ref=probe%2Frun1-cell"),
            Reply::json(200, &json!({"sha": "blob2"})),
        )
        .expect(
            Method::Put,
            format!("{GH}/contents/probe-review/fixture.txt"),
            Reply::json(
                200,
                &json!({"content": {"sha": "blob3"}, "commit": {"sha": "h2"}}),
            ),
        );
    let author_t = gh_cleanup_author(author_t, true);
    let reviewer_t = Scripted::new()
        .expect(Method::Get, GH, gh_repo(json!(true)))
        .expect(
            Method::Get,
            "/user",
            Reply::json(200, &json!({"login": "rev", "id": 42})),
        )
        .expect(
            Method::Get,
            format!("{GH}/pulls/7/reviews?per_page=100&page=1"),
            Reply::json(
                200,
                &json!([{"id": 55, "state": "PENDING"}, {"id": 56, "state": "COMMENTED"}]),
            ),
        )
        .expect(
            Method::Delete,
            format!("{GH}/pulls/7/reviews/55"),
            Reply::json(200, &json!({"id": 55})),
        )
        .expect(
            Method::Get,
            format!("{GH}/pulls/7/reviews?per_page=100&page=1"),
            Reply::json(200, &json!([{"id": 56, "state": "COMMENTED"}])),
        );
    let author_g = Guarded::new(&author_t, gh_target(), listed("owner/sandbox"));
    let reviewer_g = Guarded::new(&reviewer_t, gh_target(), listed("owner/sandbox"));
    let author = GitHubRepo::new(&author_g)?;
    let reviewer = GitHubRepo::new(&reviewer_g)?;
    let roles = Roles {
        author: &author,
        reviewer: Some(&reviewer),
    };
    let mut ledger = ledger_in(dir.path())?;
    let run = run_cell(&roles, &mut ledger, &names(), "probe run1 cell", |cell| {
        assert_eq!(cell.number, 7);
        assert_eq!(cell.base_commit, "b1");
        assert_eq!(cell.head_commit, "h1");
        assert_eq!(cell.default_branch, "main");
        assert_eq!(cell.reviewer.as_ref().map(|r| r.id), Some(42));
        cell.move_head(&author, Fixture::Moved)
            .map_err(|e| e.to_string())
    });
    assert_eq!(run.outcome, Ok("h2".to_owned()));
    assert_eq!(
        run.cleanup,
        Cleanup {
            removed: vec![
                "черновик 55 в PR #7 удалён".to_owned(),
                "PR #7 закрыт".to_owned(),
                "ветка probe/run1-cell удалена".to_owned(),
                "ветка probe/run1-cell-base удалена".to_owned(),
            ],
            errors: Vec::new(),
            leftovers: Vec::new(),
        }
    );
    assert_eq!(
        Ledger::load(ledger.path())?,
        [
            Created::Branch {
                name: "probe/run1-cell-base".to_owned()
            },
            Created::Branch {
                name: "probe/run1-cell".to_owned()
            },
            Created::Pull { number: 7 },
        ]
    );
    let requests = author_t.finish()?;
    reviewer_t.finish()?;
    assert_eq!(
        body(&requests[2]),
        json!({"ref": "refs/heads/probe/run1-cell-base", "sha": "d0"})
    );
    assert_eq!(
        body(&requests[3]),
        json!({
            "message": "probe-review: fixture base",
            "content": base64(Fixture::Base.content().as_bytes()),
            "branch": "probe/run1-cell-base",
        })
    );
    assert_eq!(
        body(&requests[4]),
        json!({"ref": "refs/heads/probe/run1-cell", "sha": "b1"})
    );
    assert_eq!(body(&requests[6])["sha"], json!("blob1"));
    assert_eq!(body(&requests[6])["branch"], json!("probe/run1-cell"));
    assert_eq!(
        body(&requests[7]),
        json!({
            "title": "probe run1 cell",
            "head": "probe/run1-cell",
            "base": "probe/run1-cell-base",
            "body": super::github::PULL_BODY,
        })
    );
    assert_eq!(body(&requests[9])["sha"], json!("blob2"));
    assert_eq!(
        body(&requests[9])["content"],
        json!(base64(Fixture::Moved.content().as_bytes()))
    );
    assert!(author_g.refused_writes().is_empty());
    Ok(())
}

#[test]
fn panic_in_cell_body_still_cleans_up() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let author_t = gh_cleanup_author(gh_open_author(Scripted::new()), true);
    let author_g = Guarded::new(&author_t, gh_target(), listed("owner/sandbox"));
    let author = GitHubRepo::new(&author_g)?;
    let roles = Roles {
        author: &author,
        reviewer: None,
    };
    let mut ledger = ledger_in(dir.path())?;
    let run: super::lifecycle::CellRun<()> = run_cell(&roles, &mut ledger, &names(), "t", |_| {
        panic!("сбой сценария")
    });
    assert_eq!(
        run.outcome,
        Err("паника сценария: сбой сценария".to_owned())
    );
    assert!(run.cleanup.leftovers.is_empty(), "{:?}", run.cleanup);
    author_t.finish()?;
    Ok(())
}

#[test]
fn failed_open_cleans_up_what_was_created() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let author_t = Scripted::new()
        .expect(Method::Get, GH, gh_repo(json!(true)))
        .expect(
            Method::Get,
            format!("{GH}/git/ref/heads/main"),
            Reply::json(200, &json!({"object": {"sha": "d0"}})),
        )
        .expect(
            Method::Post,
            format!("{GH}/git/refs"),
            Reply::json(201, &json!({})),
        )
        .expect(
            Method::Put,
            format!("{GH}/contents/probe-review/fixture.txt"),
            Reply::json(500, &json!({"message": "boom"})),
        )
        .expect(
            Method::Get,
            format!(
                "{GH}/pulls?state=open&head=owner%3Aprobe%2Frun1-cell-base&per_page=100&page=1"
            ),
            Reply::json(200, &json!([])),
        )
        .expect(
            Method::Delete,
            format!("{GH}/git/refs/heads/probe/run1-cell-base"),
            Reply::status(204),
        )
        .expect(
            Method::Get,
            format!("{GH}/git/ref/heads/probe/run1-cell-base"),
            not_found(),
        )
        .expect(
            Method::Get,
            format!(
                "{GH}/pulls?state=open&head=owner%3Aprobe%2Frun1-cell-base&per_page=100&page=1"
            ),
            Reply::json(200, &json!([])),
        );
    let author_g = Guarded::new(&author_t, gh_target(), listed("owner/sandbox"));
    let author = GitHubRepo::new(&author_g)?;
    let roles = Roles {
        author: &author,
        reviewer: None,
    };
    let mut ledger = ledger_in(dir.path())?;
    let run = run_cell(&roles, &mut ledger, &names(), "t", |_| Ok(()));
    let outcome = run.outcome.err().ok_or("ожидалась ошибка открытия")?;
    assert!(outcome.starts_with("коммит основы: код 500"), "{outcome}");
    assert_eq!(run.cleanup.removed, ["ветка probe/run1-cell-base удалена"]);
    assert!(run.cleanup.leftovers.is_empty());
    author_t.finish()?;
    Ok(())
}

#[test]
fn guard_refusal_stops_the_cell_before_any_write() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let author_t = Scripted::new().expect(Method::Get, GH, gh_repo(json!(false)));
    let author_g = Guarded::new(&author_t, gh_target(), listed("owner/sandbox"));
    let author = GitHubRepo::new(&author_g)?;
    let roles = Roles {
        author: &author,
        reviewer: None,
    };
    let mut ledger = ledger_in(dir.path())?;
    let run = run_cell(&roles, &mut ledger, &names(), "t", |_| Ok(()));
    assert_eq!(
        run.outcome,
        Err("сторож автора: сторож записи: репозиторий не приватный (private: false)".to_owned())
    );
    assert_eq!(run.cleanup, Cleanup::default());
    assert!(ledger.entries().is_empty());
    assert!(author_t.delivered().iter().all(|r| !r.method.is_write()));
    author_t.finish()?;
    Ok(())
}

#[test]
fn cleanup_continues_past_failed_step_and_reports_leftovers() -> Result<(), String> {
    let author_t = Scripted::new()
        .expect(Method::Get, GH, gh_repo(json!(true)))
        .expect(
            Method::Get,
            format!(
                "{GH}/pulls?state=open&head=owner%3Aprobe%2Frun1-cell-base&per_page=100&page=1"
            ),
            Reply::json(200, &json!([])),
        )
        .expect(
            Method::Get,
            format!("{GH}/pulls?state=open&head=owner%3Aprobe%2Frun1-cell&per_page=100&page=1"),
            Reply::timeout(),
        )
        .expect(
            Method::Get,
            format!("{GH}/pulls/7"),
            Reply::json(200, &json!({"state": "closed"})),
        )
        .expect(
            Method::Delete,
            format!("{GH}/git/refs/heads/probe/run1-cell"),
            Reply::json(500, &json!({"message": "boom"})),
        )
        .expect(
            Method::Delete,
            format!("{GH}/git/refs/heads/probe/run1-cell-base"),
            Reply::json(422, &json!({"message": "Reference does not exist"})),
        )
        .expect(
            Method::Get,
            format!("{GH}/pulls/7"),
            Reply::json(200, &json!({"state": "closed"})),
        )
        .expect(
            Method::Get,
            format!("{GH}/git/ref/heads/probe/run1-cell-base"),
            not_found(),
        )
        .expect(
            Method::Get,
            format!("{GH}/git/ref/heads/probe/run1-cell"),
            Reply::json(200, &json!({"object": {"sha": "h1"}})),
        )
        .expect(
            Method::Get,
            format!(
                "{GH}/pulls?state=open&head=owner%3Aprobe%2Frun1-cell-base&per_page=100&page=1"
            ),
            Reply::json(200, &json!([])),
        )
        .expect(
            Method::Get,
            format!("{GH}/pulls?state=open&head=owner%3Aprobe%2Frun1-cell&per_page=100&page=1"),
            Reply::json(200, &json!([{"number": 7}])),
        );
    let author_g = Guarded::new(&author_t, gh_target(), listed("owner/sandbox"));
    let author = GitHubRepo::new(&author_g)?;
    let entries = [
        Created::Branch {
            name: "probe/run1-cell-base".to_owned(),
        },
        Created::Branch {
            name: "probe/run1-cell".to_owned(),
        },
        Created::Pull { number: 7 },
    ];
    let report = cleanup(
        &Roles {
            author: &author,
            reviewer: None,
        },
        &entries,
    );
    assert_eq!(report.leftovers, ["ветка probe/run1-cell"]);
    assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
    assert!(report.errors[0].starts_with("поиск PR из ветки probe/run1-cell: итог неизвестен"));
    assert!(report.errors[1].starts_with("удаление ветки probe/run1-cell: код 500"));
    assert!(report.removed.is_empty());
    author_t.finish()?;
    Ok(())
}

#[test]
fn cleanup_refused_by_guard_lists_everything_as_leftover() -> Result<(), String> {
    let author_t = Scripted::new().expect(Method::Get, GH, gh_repo(json!(false)));
    let author_g = Guarded::new(&author_t, gh_target(), listed("owner/sandbox"));
    let author = GitHubRepo::new(&author_g)?;
    let report = cleanup(
        &Roles {
            author: &author,
            reviewer: None,
        },
        &[
            Created::Branch {
                name: "probe/run1-cell".to_owned(),
            },
            Created::Pull { number: 3 },
        ],
    );
    assert_eq!(report.leftovers.len(), 2);
    assert!(report.leftovers[0].starts_with("PR #3: не убран: сторож записи"));
    assert!(report.leftovers[1].starts_with("ветка probe/run1-cell: не убрана"));
    author_t.finish()?;
    Ok(())
}

// --- жизненный цикл GitLab ---

fn gl_open_author(s: Scripted, mr: Reply) -> Scripted {
    s.expect(Method::Get, GL, gl_project("private"))
        .expect(
            Method::Get,
            format!("{GL}/repository/branches/main"),
            Reply::json(200, &json!({"commit": {"id": "d0"}})),
        )
        .expect(
            Method::Post,
            format!("{GL}/repository/branches"),
            Reply::json(201, &json!({"name": "probe/run1-cell-base"})),
        )
        .expect(
            Method::Post,
            format!("{GL}/repository/commits"),
            Reply::json(201, &json!({"id": "b1"})),
        )
        .expect(
            Method::Post,
            format!("{GL}/repository/branches"),
            Reply::json(201, &json!({"name": "probe/run1-cell"})),
        )
        .expect(
            Method::Post,
            format!("{GL}/repository/commits"),
            Reply::json(201, &json!({"id": "h1"})),
        )
        .expect(Method::Post, format!("{GL}/merge_requests"), mr)
}

fn gl_cleanup_author(s: Scripted, iid: u64) -> Scripted {
    s.expect(
        Method::Get,
        format!(
            "{GL}/merge_requests?state=opened&source_branch=probe%2Frun1-cell-base&per_page=100&page=1"
        ),
        Reply::json(200, &json!([])),
    )
    .expect(
        Method::Get,
        format!(
            "{GL}/merge_requests?state=opened&source_branch=probe%2Frun1-cell&per_page=100&page=1"
        ),
        Reply::json(200, &json!([{"iid": iid}])),
    )
}

fn gl_close_and_delete(s: Scripted, iid: u64) -> Scripted {
    s.expect(
        Method::Get,
        format!("{GL}/merge_requests/{iid}"),
        Reply::json(200, &json!({"state": "opened"})),
    )
    .expect(
        Method::Put,
        format!("{GL}/merge_requests/{iid}"),
        Reply::json(200, &json!({"state": "closed"})),
    )
    .expect(
        Method::Delete,
        format!("{GL}/repository/branches/probe%2Frun1-cell"),
        Reply::status(204),
    )
    .expect(
        Method::Delete,
        format!("{GL}/repository/branches/probe%2Frun1-cell-base"),
        Reply::status(204),
    )
    .expect(
        Method::Get,
        format!("{GL}/merge_requests/{iid}"),
        Reply::json(200, &json!({"state": "closed"})),
    )
    .expect(
        Method::Get,
        format!("{GL}/repository/branches/probe%2Frun1-cell-base"),
        not_found(),
    )
    .expect(
        Method::Get,
        format!("{GL}/repository/branches/probe%2Frun1-cell"),
        not_found(),
    )
    .expect(
        Method::Get,
        gl_search("probe%2Frun1-cell-base"),
        Reply::json(200, &json!([])),
    )
    .expect(
        Method::Get,
        gl_search("probe%2Frun1-cell"),
        Reply::json(200, &json!([])),
    )
}

fn gl_search(branch: &str) -> String {
    format!("{GL}/merge_requests?state=opened&source_branch={branch}&per_page=100&page=1")
}

#[test]
fn gitlab_cell_assigns_reviewer_and_removes_reviewer_drafts() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let author_t = gl_open_author(
        Scripted::new(),
        Reply::json(201, &json!({"iid": 3, "state": "opened"})),
    );
    let author_t = gl_close_and_delete(gl_cleanup_author(author_t, 3), 3);
    let reviewer_t = Scripted::new()
        .expect(Method::Get, GL, gl_project("private"))
        .expect(
            Method::Get,
            "/api/v4/user",
            Reply::json(200, &json!({"username": "rev", "id": 42})),
        )
        .expect(
            Method::Get,
            format!("{GL}/merge_requests/3/draft_notes?per_page=100&page=1"),
            Reply::json(200, &json!([{"id": 91}, {"id": 92}])),
        )
        .expect(
            Method::Delete,
            format!("{GL}/merge_requests/3/draft_notes/91"),
            Reply::status(204),
        )
        .expect(
            Method::Delete,
            format!("{GL}/merge_requests/3/draft_notes/92"),
            Reply::status(204),
        )
        .expect(
            Method::Get,
            format!("{GL}/merge_requests/3/draft_notes?per_page=100&page=1"),
            Reply::json(200, &json!([])),
        );
    let author_g = Guarded::new(&author_t, gl_target(), listed("group/sandbox"));
    let reviewer_g = Guarded::new(&reviewer_t, gl_target(), listed("group/sandbox"));
    let author = GitLabRepo::new(&author_g)?;
    let reviewer = GitLabRepo::new(&reviewer_g)?;
    let roles = Roles {
        author: &author,
        reviewer: Some(&reviewer),
    };
    let mut ledger = ledger_in(dir.path())?;
    let run = run_cell(&roles, &mut ledger, &names(), "probe run1 cell", |cell| {
        Ok(cell.number)
    });
    assert_eq!(run.outcome, Ok(3));
    assert_eq!(
        run.cleanup.removed,
        [
            "черновик 91 в MR #3 удалён",
            "черновик 92 в MR #3 удалён",
            "MR #3 закрыт",
            "ветка probe/run1-cell удалена",
            "ветка probe/run1-cell-base удалена",
        ]
    );
    assert!(run.cleanup.leftovers.is_empty() && run.cleanup.errors.is_empty());
    let requests = author_t.finish()?;
    reviewer_t.finish()?;
    assert_eq!(
        body(&requests[2]),
        json!({"branch": "probe/run1-cell-base", "ref": "d0"})
    );
    assert_eq!(
        body(&requests[3]),
        json!({
            "branch": "probe/run1-cell-base",
            "commit_message": "probe-review: fixture base",
            "actions": [{
                "action": "create",
                "file_path": FIXTURE_PATH,
                "content": Fixture::Base.content(),
            }],
        })
    );
    assert_eq!(body(&requests[5])["actions"][0]["action"], json!("update"));
    assert_eq!(
        body(&requests[6]),
        json!({
            "source_branch": "probe/run1-cell",
            "target_branch": "probe/run1-cell-base",
            "title": "probe run1 cell",
            "description": super::gitlab::MR_DESCRIPTION,
            "reviewer_ids": [42],
        })
    );
    Ok(())
}

#[test]
fn lost_mr_response_is_found_by_source_branch_and_closed() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let author_t = gl_open_author(Scripted::new(), Reply::timeout());
    let author_t = gl_close_and_delete(gl_cleanup_author(author_t, 5), 5);
    let author_g = Guarded::new(&author_t, gl_target(), listed("group/sandbox"));
    let author = GitLabRepo::new(&author_g)?;
    let roles = Roles {
        author: &author,
        reviewer: None,
    };
    let mut ledger = ledger_in(dir.path())?;
    let run = run_cell(&roles, &mut ledger, &names(), "t", |_| Ok(()));
    let outcome = run.outcome.err().ok_or("ожидалась ошибка создания MR")?;
    assert!(
        outcome.starts_with("создание PR/MR: итог неизвестен"),
        "{outcome}"
    );
    assert!(run.cleanup.removed.contains(&"MR #5 закрыт".to_owned()));
    assert!(run.cleanup.leftovers.is_empty(), "{:?}", run.cleanup);
    assert_eq!(ledger.entries().len(), 2);
    author_t.finish()?;
    Ok(())
}

fn gl_delete_and_confirm_branches(s: Scripted) -> Scripted {
    s.expect(
        Method::Delete,
        format!("{GL}/repository/branches/probe%2Frun1-cell"),
        Reply::status(204),
    )
    .expect(
        Method::Delete,
        format!("{GL}/repository/branches/probe%2Frun1-cell-base"),
        Reply::status(204),
    )
    .expect(
        Method::Get,
        format!("{GL}/repository/branches/probe%2Frun1-cell-base"),
        not_found(),
    )
    .expect(
        Method::Get,
        format!("{GL}/repository/branches/probe%2Frun1-cell"),
        not_found(),
    )
}

#[test]
fn lost_mr_response_and_failed_search_leave_the_mr_in_leftovers() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let author_t = gl_open_author(Scripted::new(), Reply::timeout())
        .expect(
            Method::Get,
            gl_search("probe%2Frun1-cell-base"),
            Reply::json(200, &json!([])),
        )
        .expect(
            Method::Get,
            gl_search("probe%2Frun1-cell"),
            Reply::json(500, &json!({"message": "boom"})),
        );
    let author_t = gl_delete_and_confirm_branches(author_t)
        .expect(
            Method::Get,
            gl_search("probe%2Frun1-cell-base"),
            Reply::json(200, &json!([])),
        )
        .expect(
            Method::Get,
            gl_search("probe%2Frun1-cell"),
            Reply::json(200, &json!([{"iid": 5}])),
        );
    let author_g = Guarded::new(&author_t, gl_target(), listed("group/sandbox"));
    let author = GitLabRepo::new(&author_g)?;
    let roles = Roles {
        author: &author,
        reviewer: None,
    };
    let mut ledger = ledger_in(dir.path())?;
    let run = run_cell(&roles, &mut ledger, &names(), "t", |_| Ok(()));
    let outcome = run.outcome.err().ok_or("ожидалась ошибка создания MR")?;
    assert!(
        outcome.starts_with("создание PR/MR: итог неизвестен"),
        "{outcome}"
    );
    assert_eq!(
        run.cleanup,
        Cleanup {
            removed: vec![
                "ветка probe/run1-cell удалена".to_owned(),
                "ветка probe/run1-cell-base удалена".to_owned(),
            ],
            errors: vec!["поиск MR из ветки probe/run1-cell: код 500: boom".to_owned()],
            leftovers: vec![
                "MR #5 открыт: найден повторным поиском из ветки probe/run1-cell".to_owned()
            ],
        }
    );
    author_t.finish()?;

    let author_t = Scripted::new()
        .expect(Method::Get, GL, gl_project("private"))
        .expect(
            Method::Get,
            gl_search("probe%2Frun1-cell-base"),
            Reply::json(200, &json!([])),
        )
        .expect(
            Method::Get,
            gl_search("probe%2Frun1-cell"),
            Reply::timeout(),
        );
    let author_t = gl_delete_and_confirm_branches(author_t)
        .expect(
            Method::Get,
            gl_search("probe%2Frun1-cell-base"),
            Reply::json(200, &json!([])),
        )
        .expect(
            Method::Get,
            gl_search("probe%2Frun1-cell"),
            Reply::timeout(),
        );
    let author_g = Guarded::new(&author_t, gl_target(), listed("group/sandbox"));
    let author = GitLabRepo::new(&author_g)?;
    let report = cleanup(
        &Roles {
            author: &author,
            reviewer: None,
        },
        ledger.entries(),
    );
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert_eq!(report.leftovers.len(), 1, "{report:?}");
    assert!(
        report.leftovers[0]
            .starts_with("поиск MR из ветки probe/run1-cell: не подтверждено: итог неизвестен"),
        "{report:?}"
    );
    author_t.finish()?;
    Ok(())
}

#[test]
fn resume_cleanup_reads_ledger_and_skips_torn_last_line() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let mut ledger = ledger_in(dir.path())?;
    ledger.record(Created::Branch {
        name: "probe/run1-cell-base".to_owned(),
    })?;
    ledger.record(Created::Branch {
        name: "probe/run1-cell".to_owned(),
    })?;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(ledger.path())
        .map_err(|e| e.to_string())?;
    std::io::Write::write_all(&mut file, b"{\"kind\":\"pu").map_err(|e| e.to_string())?;
    assert!(
        Ledger::create(ledger.path()).is_err(),
        "журнал не перезаписывается"
    );
    let author_t = gl_close_and_delete(
        gl_cleanup_author(
            Scripted::new().expect(Method::Get, GL, gl_project("private")),
            8,
        ),
        8,
    );
    let author_g = Guarded::new(&author_t, gl_target(), listed("group/sandbox"));
    let author = GitLabRepo::new(&author_g)?;
    let report = resume_cleanup(
        &Roles {
            author: &author,
            reviewer: None,
        },
        ledger.path(),
    )?;
    assert!(report.leftovers.is_empty(), "{report:?}");
    assert!(report.removed.contains(&"MR #8 закрыт".to_owned()));
    author_t.finish()?;

    let broken = dir.path().join("broken.jsonl");
    std::fs::write(&broken, "nope\n{\"kind\":\"pull\",\"number\":1}\n")
        .map_err(|e| e.to_string())?;
    assert!(Ledger::load(&broken).is_err());
    Ok(())
}

// --- имена, фикстура, base64 ---

#[test]
fn cell_names_share_the_run_prefix() -> Result<(), String> {
    let names = CellNames::new("20261120-164039-1234", "gh-local-create")?;
    assert_eq!(names.head, "probe/20261120-164039-1234-gh-local-create");
    assert_eq!(
        names.base,
        "probe/20261120-164039-1234-gh-local-create-base"
    );
    let prefix = run_prefix("20261120-164039-1234");
    assert!(names.head.starts_with(&prefix) && names.base.starts_with(&prefix));
    for (run, cell) in [
        ("", "c"),
        ("r", ""),
        ("r/x", "c"),
        ("r", "a b"),
        ("-r", "c"),
        ("r", "c..d"),
    ] {
        assert!(CellNames::new(run, cell).is_err(), "{run:?} {cell:?}");
    }
    Ok(())
}

#[test]
fn fixture_versions_remove_change_add_and_move() {
    let base = Fixture::Base.content();
    let head = Fixture::Head.content();
    let moved = Fixture::Moved.content();
    assert_eq!(base.lines().count(), 30);
    assert!(base.contains("line 03\n") && !head.contains("line 03\n"));
    assert!(head.contains("line 05 changed\n") && !base.contains("changed"));
    assert!(head.contains("line 24\nline 24a\nline 24b\nline 25\n"));
    assert!(moved.contains("line 27 moved\n") && !head.contains("moved"));
    assert_eq!(
        head.replace("line 27\n", "line 27 moved\n"),
        moved,
        "смена head меняет одну строку"
    );
}

#[test]
fn base64_matches_rfc_4648_vectors() {
    for (input, expected) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foob", "Zm9vYg=="),
        ("fooba", "Zm9vYmE="),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(input.as_bytes()), expected, "{input}");
    }
}

// --- предусловия облачного столбца ---

fn stub_security(dir: &Path) -> Result<PathBuf, String> {
    let path = dir.join("security");
    std::fs::write(
        &path,
        "#!/bin/sh\ncase \"$5\" in\n  github-author) printf 'ghp_authortoken\\n' ;;\n  *) exit 44 ;;\nesac\n",
    )
    .map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    Ok(path)
}

#[test]
fn preflight_maps_missing_sandbox_host_and_tokens() -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let keychain = Keychain::with_program(&stub_security(dir.path())?, Duration::from_secs(10));
    assert!(matches!(
        preflight(Column::GithubCloud, Err(SandboxError::Missing), &keychain),
        Preflight::NotRun(reason) if reason == "нет sandbox.toml"
    ));
    assert!(matches!(
        preflight(
            Column::GithubCloud,
            Err(SandboxError::Parse {
                line: 2,
                reason: "x".to_owned()
            }),
            &keychain
        ),
        Preflight::Error(_)
    ));
    let only_gitlab =
        Sandbox::parse("[gitlab_cloud]\nrepo = \"g/p\"\n").map_err(|e| e.to_string())?;
    assert!(matches!(
        preflight(Column::GithubCloud, Ok(only_gitlab), &keychain),
        Preflight::NotRun(reason) if reason == "нет хоста github_cloud в sandbox.toml"
    ));
    let sandbox =
        Sandbox::parse("[github_cloud]\nrepo = \"owner/sandbox\"\n").map_err(|e| e.to_string())?;
    let Preflight::Ready(host) = preflight(Column::GithubCloud, Ok(sandbox), &keychain) else {
        return Err("ожидалась готовность хоста".to_owned());
    };
    assert_eq!(host.target.rest, "https://api.github.com");
    assert_eq!(host.target.repo, "owner/sandbox");
    assert_eq!(host.scope, listed("owner/sandbox"));
    assert_eq!(
        host.accounts.author().map(Secret::expose),
        Ok("ghp_authortoken")
    );
    assert_eq!(
        host.accounts.reviewer().err().as_deref(),
        Some("нет ревьюера: нет токена github-reviewer")
    );
    assert!(!format!("{host:?}").contains("ghp_authortoken"));
    let mut scrubber = Scrubber::new();
    host.accounts.register(&mut scrubber);
    assert_eq!(
        scrubber.scrub_text("Bearer ghp_authortoken"),
        "Bearer <token>"
    );
    Ok(())
}

#[test]
fn credential_scheme_follows_the_column() {
    let secret = Secret::new("t");
    assert_eq!(
        credential(Column::GithubCloud, secret.clone()).scheme,
        AuthScheme::Bearer
    );
    assert_eq!(
        credential(Column::GitlabCloud, secret.clone()).scheme,
        AuthScheme::PrivateToken
    );
    assert_eq!(
        credential(Column::GitlabCe, secret).scheme,
        AuthScheme::PrivateToken
    );
}

#[test]
fn live_repos_reject_wrong_provider() {
    let scripted = Scripted::new();
    let gh = Guarded::new(&scripted, gh_target(), listed("owner/sandbox"));
    let gl = Guarded::new(&scripted, gl_target(), listed("group/sandbox"));
    assert!(GitLabRepo::new(&gh).is_err());
    assert!(GitHubRepo::new(&gl).is_err());
    let repo: &dyn LiveRepo = &GitHubRepo::new(&gh).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(repo.pull_word(), "PR");
}

#[test]
fn stale_ledgers_skip_own_swept_other_columns_and_gitlab_ce() -> Result<(), String> {
    use super::lifecycle::{LEDGER_FILE, SWEPT_FILE, stale_ledgers};
    let root = tempfile::tempdir().map_err(|e| e.to_string())?;
    let make = |run: &str, proc: &str, swept: bool| -> Result<PathBuf, String> {
        let dir = root.path().join(run).join(proc);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(LEDGER_FILE), "").map_err(|e| e.to_string())?;
        if swept {
            std::fs::write(dir.join(SWEPT_FILE), "").map_err(|e| e.to_string())?;
        }
        Ok(dir)
    };
    let stale = make("r1", "gh-viewed-github_cloud", false)?;
    make("r1", "gh-anchors-github_cloud", true)?;
    make("r1", "gl-anchors-gitlab_cloud", false)?;
    let ce = make("r1", "gl-anchors-gitlab_ce", false)?;
    let own = make("r2", "gh-viewed-github_cloud", false)?;
    let same_run = make("r2", "gh-anchors-github_cloud", false)?;
    assert_eq!(
        stale_ledgers(root.path(), &own, Column::GithubCloud),
        vec![stale.join(LEDGER_FILE), same_run.join(LEDGER_FILE)]
    );
    assert!(stale_ledgers(root.path(), &ce, Column::GitlabCe).is_empty());
    assert!(stale_ledgers(&root.path().join("нет"), &own, Column::GithubCloud).is_empty());
    Ok(())
}
