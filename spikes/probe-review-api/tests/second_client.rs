//! Процесс второго клиента `probe-review --second-client gl-create-draft` против `forge-fake`:
//! хост и токены — окружением, как у процесса сценария `gitlab_ce`, ID черновика — в stdout,
//! очищенный след — в `second-client.json`, токена нет ни в аргументах, ни в следе.

use std::path::Path;
use std::process::{Command, Output};

use probe_review_api::fake::FakeServer;
use probe_review_api::fake::gitlab::{Access, FakeDiff, GitlabFake};
use probe_review_api::live::provision::{ENV_HOST, ENV_TOKENS};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const REPO: &str = "probe-author/shellio-review-sandbox";
const AUTHOR: &str = "probe-author";
const REVIEWER: &str = "probe-reviewer";
const AUTHOR_TOKEN: &str = "sentinel-author-token-5c1e";
const REVIEWER_TOKEN: &str = "sentinel-reviewer-token-9d2b";
const IID: u64 = 4;
const DIFF: &str = "@@ -1,8 +1,7 @@\n line 01\n line 02\n-line 03\n line 04\n-line 05\n\
+line 05 changed\n line 06\n line 07\n line 08\n@@ -22,6 +21,8 @@\n line 22\n line 23\n line 24\n\
+line 24a\n+line 24b\n line 25\n line 26\n line 27\n";

fn fake(visibility_private: bool) -> Result<FakeServer<GitlabFake>, String> {
    let mut fake = GitlabFake::new(REPO);
    if !visibility_private {
        fake.set_visibility("public");
    }
    fake.add_user(AUTHOR, AUTHOR_TOKEN, Some(Access::Developer));
    fake.add_user(REVIEWER, REVIEWER_TOKEN, Some(Access::Developer));
    fake.add_mr(
        IID,
        AUTHOR,
        "1111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222",
        vec![FakeDiff::modified("probe-review/fixture.txt", DIFF)],
    );
    fake.add_reviewer(IID, REVIEWER)?;
    FakeServer::start(fake)
}

fn host_file(dir: &Path, url: &str) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let path = dir.join("gitlab-ce.json");
    let host = json!({
        "schema": 1,
        "url": url,
        "image": "gitlab/gitlab-ce@sha256:0000",
        "tag": "19.6.1-ce.0",
        "version": "19.6.1",
        "revision": null,
        "enterprise": false,
        "project": REPO,
        "default_branch": "main",
        "author": {"id": 1, "username": AUTHOR, "name": "Probe Author"},
        "reviewer": {"id": 2, "username": REVIEWER, "name": "Probe Reviewer"},
        "root_token": "rails_runner",
    });
    std::fs::write(&path, serde_json::to_vec(&host)?)?;
    Ok(path)
}

fn second_client(host: &Path, out: &Path, column: &str) -> std::io::Result<Output> {
    Command::new(env!("CARGO_BIN_EXE_probe-review"))
        .args([
            "--second-client",
            "gl-create-draft",
            "--column",
            column,
            "--mr",
            &IID.to_string(),
            "--line",
            "24",
            "--out",
        ])
        .arg(out)
        .env(ENV_HOST, host)
        .env(
            ENV_TOKENS,
            json!({"author": AUTHOR_TOKEN, "reviewer": REVIEWER_TOKEN}).to_string(),
        )
        .output()
}

#[test]
fn second_client_process_creates_a_reviewer_draft_through_its_guard() -> TestResult {
    let server = fake(true)?;
    let dir = tempfile::tempdir()?;
    let host = host_file(dir.path(), server.base())?;
    let output = second_client(&host, dir.path(), "gitlab_ce")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout} {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: Value = serde_json::from_str(stdout.trim())?;
    let id = reply["id"].as_str().ok_or("нет id")?.to_owned();
    let drafts = server.with(|fake| fake.drafts(IID));
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].id.to_string(), id);
    assert_eq!(drafts[0].note, "черновик второго клиента");

    let record = std::fs::read_to_string(dir.path().join("second-client.json"))?;
    let value: Value = serde_json::from_str(&record)?;
    assert_eq!(value["reply"], reply);
    let trace = value["trace"].as_array().ok_or("нет следа")?;
    assert!(
        trace.iter().all(|entry| entry["actor"] == "second-client"),
        "{record}"
    );
    let address = server.base().trim_start_matches("http://");
    for raw in [REVIEWER_TOKEN, AUTHOR_TOKEN, REPO, REVIEWER, address] {
        assert!(!record.contains(raw), "{raw} очищен: {record}");
    }
    Ok(())
}

#[test]
fn second_client_refuses_to_write_to_a_public_project() -> TestResult {
    let server = fake(false)?;
    let dir = tempfile::tempdir()?;
    let host = host_file(dir.path(), server.base())?;
    let output = second_client(&host, dir.path(), "gitlab_ce")?;
    assert_eq!(output.status.code(), Some(3));
    let reply: Value = serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim())?;
    assert!(reply["error"].is_string(), "{reply}");
    assert!(server.with(|fake| fake.drafts(IID)).is_empty());
    Ok(())
}

#[test]
fn second_client_outside_gitlab_ce_is_an_execution_error() -> TestResult {
    let dir = tempfile::tempdir()?;
    let host = host_file(dir.path(), "http://127.0.0.1:1")?;
    let output = second_client(&host, dir.path(), "gitlab_cloud")?;
    assert_eq!(output.status.code(), Some(3));
    Ok(())
}
