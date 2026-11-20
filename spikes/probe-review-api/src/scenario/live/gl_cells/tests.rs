use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::fake::gitlab::{self as glf, Access, FakeDiff, GitlabFake};
use crate::fake::{FakeServer, Fault, Reply as FakeReply};
use crate::gitlab::Endpoints;
use crate::journal::{Operation, SubmissionId, SubmitItem};
use crate::live::guard::{GuardTarget, WriteScope};
use crate::live::lifecycle::{Created, SWEPT_FILE};
use crate::live::repo::Account;
use crate::model::ProviderId;
use crate::results::{Column, Execution, Status};
use crate::transport::http::{Credential, HttpConfig, HttpTransport, Secret};
use crate::transport::scripted::{Reply, Scripted};
use crate::transport::{Failure, Request, Response, Traced};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const ROOT: &str = "http://127.0.0.1:55001";
const MR: &str = "/api/v4/projects/probe-author%2Fshellio-review-sandbox/merge_requests/1";
const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";
const NEW_HEAD: &str = "3333333333333333333333333333333333333333";

fn review() -> ReviewTarget {
    ReviewTarget {
        provider: Provider::GitLab,
        host: ROOT.to_owned(),
        repo: "probe-author/shellio-review-sandbox".to_owned(),
        number: 1,
    }
}

fn mr_json(head: Option<&str>) -> Value {
    let refs = head.map_or(
        Value::Null,
        |head| json!({"base_sha": BASE, "start_sha": BASE, "head_sha": head}),
    );
    json!({"id": 9, "iid": 1, "author": {"username": "a"}, "reviewers": [], "diff_refs": refs})
}

fn version_json(id: u64, head: &str) -> Value {
    json!({"id": id, "base_commit_sha": BASE, "start_commit_sha": BASE, "head_commit_sha": head})
}

fn page(items: Value) -> Reply {
    Reply::Respond(Response::json(200, &items).with_header("x-next-page", ""))
}

fn ready(scripted: Scripted, head: &str) -> Scripted {
    ready_with(
        scripted,
        head,
        json!([{
            "old_path": "f.txt", "new_path": "f.txt",
            "diff": "@@ -1,2 +1,2 @@\n a\n-b\n+c\n",
        }]),
    )
}

fn ready_with(scripted: Scripted, head: &str, diffs: Value) -> Scripted {
    let mut detail = version_json(5, head);
    detail["diffs"] = diffs;
    scripted
        .expect(Method::Get, MR, Reply::json(200, &mr_json(Some(head))))
        .expect(
            Method::Get,
            format!("{MR}/versions"),
            page(json!([version_json(5, head)])),
        )
        .expect(
            Method::Get,
            format!("{MR}/versions/5"),
            Reply::json(200, &detail),
        )
}

const NOW: Poll = Poll {
    deadline: Duration::ZERO,
    pause: Duration::ZERO,
};
const RETRY: Poll = Poll {
    deadline: Duration::from_secs(60),
    pause: Duration::ZERO,
};

#[test]
fn snapshot_waits_for_diff_refs_and_the_new_head() -> TestResult {
    let scripted = ready(
        Scripted::new().expect(Method::Get, MR, Reply::json(200, &mr_json(None))),
        HEAD,
    );
    let gitlab = GitLab::new(&scripted, Endpoints::instance(ROOT), &review())?;
    let snapshot =
        await_snapshot(&gitlab, &RETRY, None, MR_NOT_READY).map_err(|v| format!("{v:?}"))?;
    assert_eq!(snapshot.revision.head_sha().as_str(), HEAD);
    assert_eq!(snapshot.version_id, ProviderId::new("5"));
    scripted.finish()?;

    let scripted = ready(ready(Scripted::new(), HEAD), NEW_HEAD);
    let gitlab = GitLab::new(&scripted, Endpoints::instance(ROOT), &review())?;
    let snapshot = await_snapshot(&gitlab, &RETRY, Some(NEW_HEAD), MR_NOT_READY)
        .map_err(|v| format!("{v:?}"))?;
    assert_eq!(snapshot.revision.head_sha().as_str(), NEW_HEAD);
    scripted.finish()?;
    Ok(())
}

#[test]
fn snapshot_past_the_deadline_is_not_run_and_a_refusal_is_not_retried() -> TestResult {
    let scripted = Scripted::new().expect(Method::Get, MR, Reply::json(200, &mr_json(None)));
    let gitlab = GitLab::new(&scripted, Endpoints::instance(ROOT), &review())?;
    match await_snapshot(&gitlab, &NOW, None, MR_NOT_READY) {
        Err(Verdict::NotRun(reason)) => {
            assert!(reason.starts_with(MR_NOT_READY), "{reason}");
            assert!(reason.contains("попыток 1"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    scripted.finish()?;

    let scripted = Scripted::new().expect(Method::Get, MR, Reply::status(502));
    let gitlab = GitLab::new(&scripted, Endpoints::instance(ROOT), &review())?;
    assert!(matches!(
        await_snapshot(&gitlab, &RETRY, None, MR_NOT_READY),
        Err(Verdict::Error(_))
    ));
    scripted.finish()?;
    Ok(())
}

#[test]
fn writes_are_keyed_by_kind_and_unknown_writes_keep_their_path() {
    let mr = format!("{MR}/");
    let key = |method, path: &str, body: Option<Value>| write_key(method, path, body.as_ref());
    assert_eq!(key(Method::Get, &format!("{mr}draft_notes"), None), None);
    assert_eq!(
        key(Method::Post, &format!("{mr}draft_notes"), None).as_deref(),
        Some(DRAFT_CREATE)
    );
    assert_eq!(
        key(Method::Put, &format!("{mr}draft_notes/41/publish"), None).as_deref(),
        Some(DRAFT_PUBLISH)
    );
    assert_eq!(
        key(Method::Post, &format!("{mr}draft_notes/bulk_publish"), None).as_deref(),
        Some(BULK_PUBLISH)
    );
    assert_eq!(
        key(Method::Post, &format!("{mr}notes?x=1"), None).as_deref(),
        Some(NOTE)
    );
    assert_eq!(
        key(Method::Post, &format!("{mr}approve"), None).as_deref(),
        Some(APPROVE)
    );
    assert_eq!(
        key(Method::Post, &format!("{mr}unapprove"), None).as_deref(),
        Some(UNAPPROVE)
    );
    assert_eq!(
        key(Method::Delete, &format!("{mr}draft_notes/41"), None),
        Some(format!("DELETE {mr}draft_notes/41"))
    );
    let query = json!({"query": "query { currentUser { username } }"});
    assert_eq!(key(Method::Post, "/api/graphql", Some(query)), None);
    let mutation =
        json!({"query": "mutation { mergeRequestRequestChanges(input: {}) { errors } }"});
    assert_eq!(
        key(Method::Post, "/api/graphql", Some(mutation)).as_deref(),
        Some("graphql mergeRequestRequestChanges")
    );
}

#[test]
fn writes_of_takes_the_actor_and_ids_from_responses_and_paths() {
    let call = |seq, actor, request: Request, response: Response| Call {
        seq,
        actor,
        request,
        outcome: Ok(response),
        duration: Duration::ZERO,
    };
    let url = |tail: &str| format!("{ROOT}{MR}/{tail}");
    let calls = [
        call(
            1,
            "reviewer",
            Request::new(Method::Post, url("draft_notes")),
            Response::json(201, &json!({"id": 41})),
        ),
        call(
            2,
            "author",
            Request::new(Method::Post, url("notes")),
            Response::json(201, &json!({"id": 7})),
        ),
        call(
            3,
            "reviewer",
            Request::new(Method::Put, url("draft_notes/41/publish")),
            Response::new(204),
        ),
        call(
            4,
            "reviewer",
            Request::get(url("draft_notes")),
            Response::json(200, &json!([])),
        ),
    ];
    let writes = writes_of(&calls, "reviewer");
    assert_eq!(
        writes,
        vec![
            Write {
                seq: 1,
                key: DRAFT_CREATE.to_owned(),
                draft: None,
                created: Some("41".to_owned()),
            },
            Write {
                seq: 3,
                key: DRAFT_PUBLISH.to_owned(),
                draft: Some("41".to_owned()),
                created: None,
            },
        ]
    );
}

fn position(new_line: u32) -> RemotePosition {
    RemotePosition {
        base_sha: BASE.to_owned(),
        start_sha: BASE.to_owned(),
        head_sha: HEAD.to_owned(),
        old_path: "f.txt".to_owned(),
        new_path: "f.txt".to_owned(),
        old_line: None,
        new_line: Some(new_line),
        line_range: None,
    }
}

fn note(id: u64, body: &str, system: bool, position: Option<RemotePosition>) -> RemoteNote {
    RemoteNote {
        id: ProviderId::new(id.to_string()),
        author: "r".to_owned(),
        body: body.to_owned(),
        system,
        resolvable: position.is_some(),
        resolved: None,
        created_at: None,
        position,
    }
}

fn write(seq: u64, key: &str, draft: Option<&str>, created: Option<&str>) -> Write {
    Write {
        seq,
        key: key.to_owned(),
        draft: draft.map(str::to_owned),
        created: created.map(str::to_owned),
    }
}

fn published() -> Vec<Write> {
    vec![
        write(1, DRAFT_CREATE, None, Some("41")),
        write(2, DRAFT_CREATE, None, Some("42")),
        write(3, DRAFT_PUBLISH, Some("41"), None),
        write(4, DRAFT_PUBLISH, Some("42"), None),
        write(5, NOTE, None, None),
    ]
}

fn facts() -> JournalFacts {
    JournalFacts {
        full_confirmed: true,
        states: String::new(),
        remote_ids: true,
        recovered: 0,
    }
}

const EXPECTED: [(u32, &str); 2] = [(4, "первый"), (24, "второй")];

#[test]
fn draft_publish_passes_with_system_notes_on_the_mr() {
    let notes = [
        note(1, "requested review from @r", true, None),
        note(2, "первый", false, Some(position(4))),
        note(3, "второй", false, Some(position(24))),
        note(4, "Итог ревью", false, None),
        note(5, "added 1 commit", true, None),
    ];
    let checks = draft_publish_checks(&published(), &[], &notes, &EXPECTED, "Итог ревью", &facts());
    assert_eq!(checks.verdict().0, Status::Pass, "{:?}", checks.to_json());
}

#[test]
fn draft_publish_fails_on_bulk_publish_left_drafts_and_early_summary() {
    let notes = [
        note(2, "первый", false, Some(position(4))),
        note(3, "второй", false, Some(position(24))),
        note(4, "Итог ревью", false, None),
    ];
    let mut writes = published();
    writes.push(write(6, BULK_PUBLISH, None, None));
    let checks = draft_publish_checks(&writes, &[], &notes, &EXPECTED, "Итог ревью", &facts());
    assert_eq!(checks.verdict().0, Status::Fail);

    let mut writes = published();
    writes[4].seq = 0;
    let checks = draft_publish_checks(&writes, &[], &notes, &EXPECTED, "Итог ревью", &facts());
    assert_eq!(checks.verdict().0, Status::Fail, "сводка до публикаций");

    let left = RemoteDraft {
        id: ProviderId::new("43"),
        author_id: None,
        note: "чужой".to_owned(),
        discussion_id: None,
        resolve_discussion: false,
        line_code: None,
        position: None,
    };
    let checks = draft_publish_checks(
        &published(),
        &[left],
        &notes,
        &EXPECTED,
        "Итог ревью",
        &facts(),
    );
    assert_eq!(checks.verdict().0, Status::Fail);

    let mut writes = published();
    writes[3].draft = Some("99".to_owned());
    let checks = draft_publish_checks(&writes, &[], &notes, &EXPECTED, "Итог ревью", &facts());
    assert_eq!(checks.verdict().0, Status::Fail, "публикация чужого ID");

    let partial = JournalFacts {
        full_confirmed: false,
        ..facts()
    };
    let checks = draft_publish_checks(&published(), &[], &notes, &EXPECTED, "Итог ревью", &partial);
    assert_eq!(checks.verdict().0, Status::Fail);
}

#[test]
fn snapshot_with_an_empty_diff_is_not_ready() -> TestResult {
    let empty = json!([{"old_path": "f.txt", "new_path": "f.txt", "diff": ""}]);
    let scripted = ready(ready_with(Scripted::new(), HEAD, empty.clone()), HEAD);
    let gitlab = GitLab::new(&scripted, Endpoints::instance(ROOT), &review())?;
    let snapshot =
        await_snapshot(&gitlab, &RETRY, None, MR_NOT_READY).map_err(|v| format!("{v:?}"))?;
    assert_eq!(snapshot.files.len(), 1);
    scripted.finish()?;

    let scripted = ready_with(Scripted::new(), HEAD, json!([]));
    let gitlab = GitLab::new(&scripted, Endpoints::instance(ROOT), &review())?;
    match await_snapshot(&gitlab, &NOW, None, MR_NOT_READY) {
        Err(Verdict::NotRun(reason)) => {
            assert!(reason.starts_with(MR_NOT_READY), "{reason}");
            assert!(reason.ends_with(DIFF_EMPTY), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    scripted.finish()?;
    Ok(())
}

fn item(seq: u32, operation: Operation, state: ItemState) -> SubmitItem {
    SubmitItem {
        id: crate::journal::ItemId(u64::from(seq)),
        submission: SubmissionId(1),
        seq,
        operation,
        drafts: Vec::new(),
        state,
        remote_id: None,
        candidate: None,
        detail: Some("подробности".to_owned()),
        in_flight_at: None,
    }
}

fn sent(states: &[ItemState]) -> Report {
    let items: Vec<SubmitItem> = states
        .iter()
        .zip(1..)
        .map(|(state, seq)| item(seq, Operation::GitLabPublishDraftNote, *state))
        .collect();
    Report {
        submission: SubmissionId(1),
        delivery: Delivery::Partial,
        closed: false,
        unresolved: items
            .iter()
            .filter(|item| matches!(item.state, ItemState::Unknown | ItemState::InFlight))
            .count(),
        items,
    }
}

fn publish_call(actor: &'static str, outcome: Result<Response, Failure>) -> Call {
    Call {
        seq: 7,
        actor,
        request: Request::new(Method::Put, format!("{ROOT}{MR}/draft_notes/41/publish")),
        outcome,
        duration: Duration::ZERO,
    }
}

#[test]
fn unknown_rate_limited_and_unsent_writes_are_not_judged_by_the_oracle() {
    use ItemState::{Confirmed, Failed, Unknown};
    let refused = [publish_call("reviewer", Ok(Response::new(403)))];
    assert_eq!(
        delivery_verdict(&sent(&[Confirmed, Confirmed]), &refused, "reviewer"),
        None
    );
    match delivery_verdict(&sent(&[Confirmed, Unknown]), &[], "reviewer") {
        Some(Verdict::NotRun(reason)) => {
            assert!(reason.starts_with(WRITE_UNOBSERVED), "{reason}");
            assert!(reason.contains("подробности"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        delivery_verdict(&sent(&[Confirmed, Failed]), &refused, "reviewer"),
        None,
        "отказ хоста судят проверки клетки"
    );
    let limited = [
        publish_call("reviewer", Ok(Response::new(429))),
        publish_call("author", Ok(Response::new(200))),
    ];
    assert!(matches!(
        delivery_verdict(&sent(&[Failed]), &limited, "reviewer"),
        Some(Verdict::NotRun(reason)) if reason == format!("лимит: код 429 на запись {DRAFT_PUBLISH}")
    ));
    let unsent = [publish_call("reviewer", Err(Failure::not_sent("отказ")))];
    assert!(matches!(
        delivery_verdict(&sent(&[Failed]), &unsent, "reviewer"),
        Some(Verdict::Error(_))
    ));
    let lost = [publish_call("reviewer", Err(Failure::timeout("срок")))];
    assert_eq!(delivery_verdict(&sent(&[Failed]), &lost, "reviewer"), None);
}

#[test]
fn submit_errors_split_into_reads_and_stand_errors() {
    let status = |status| {
        SubmitError::GitLab(ApiError::Status {
            status,
            message: String::new(),
        })
    };
    assert!(matches!(submit_verdict(&status(403)), Verdict::Fail(_)));
    assert!(matches!(submit_verdict(&status(502)), Verdict::Error(_)));
    assert!(matches!(
        submit_verdict(&SubmitError::Empty),
        Verdict::Error(_)
    ));
    assert!(matches!(
        submit_verdict(&SubmitError::GitLab(ApiError::Unexpected("у MR нет diff_refs".to_owned()))),
        Verdict::NotRun(reason) if reason.starts_with(MR_NOT_READY)
    ));
}

// --- обвязка клетки над сторожем ---

const REPO: &str = "probe-author/shellio-review-sandbox";
const PROJECT: &str = "/api/v4/projects/probe-author%2Fshellio-review-sandbox";
const RUN_ID: &str = "20261120-101010-77";

fn project(visibility: &str) -> Reply {
    Reply::json(
        200,
        &json!({"path_with_namespace": REPO, "visibility": visibility, "default_branch": "main"}),
    )
}

fn branch_reply() -> Reply {
    Reply::json(200, &json!({"name": "x", "commit": {"id": "abc"}}))
}

struct Harness {
    _root: tempfile::TempDir,
    out: std::path::PathBuf,
    state: std::path::PathBuf,
}

impl Harness {
    fn new() -> Result<Harness, Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let out = root.path().join(RUN_ID).join("gl-draft-publish-gitlab_ce");
        let state = root.path().join("state");
        std::fs::create_dir_all(&out)?;
        std::fs::create_dir_all(&state)?;
        Ok(Harness {
            _root: root,
            out,
            state,
        })
    }

    fn run(&self, author: &Scripted, reviewer: &Scripted) -> ResultFile {
        let trace = Trace::new();
        let target = GuardTarget::gitlab(&Endpoints::instance(ROOT), REPO);
        let guard = |scripted, actor| {
            Guarded::new(
                Traced::new(scripted, trace.clone(), actor),
                target.clone(),
                WriteScope::Loopback,
            )
        };
        let author = guard(author, "author");
        let reviewer = guard(reviewer, "reviewer");
        let cx = GlCx {
            author: &author,
            reviewer: &reviewer,
            trace: &trace,
            host_url: ROOT.to_owned(),
            reviewer_login: "probe-reviewer".to_owned(),
            state_dir: &self.state,
            poll: NOW,
            second: &NoSecond,
            head: &NoHead,
        };
        let mut scrubber = Scrubber::new();
        scrubber.add_repo(REPO);
        let mut result = ResultFile::new("s", "b", "gl-draft-publish", Column::GitlabCe);
        run_gl_cell(
            Scenario::GlDraftPublish,
            &cx,
            &self.out,
            &scrubber,
            &mut result,
        );
        result
    }

    fn ledger(&self) -> Result<Vec<Created>, String> {
        Ledger::load(&self.out.join(LEDGER_FILE))
    }
}

fn target_entry() -> Created {
    Created::Target {
        rest: format!("{ROOT}/api/v4"),
        repo: REPO.to_owned(),
    }
}

#[test]
fn denied_repository_read_by_the_author_guard_is_not_run() -> TestResult {
    let harness = Harness::new()?;
    let author = Scripted::new().expect(
        Method::Get,
        PROJECT,
        Reply::json(404, &json!({"message": "404 Project Not Found"})),
    );
    let reviewer = Scripted::new();
    let result = harness.run(&author, &reviewer);
    assert!(result.execution.is_ok(), "{:?}", result.execution);
    assert_eq!(result.status, Status::NotRun);
    assert!(
        result
            .reason
            .starts_with("сторож автора: токен без прав или нет репозитория: код 404"),
        "{}",
        result.reason
    );
    assert_eq!(harness.ledger()?, [target_entry()], "журнал начат целью");
    assert!(harness.out.join(SWEPT_FILE).exists(), "убирать нечего");
    assert_eq!(result.trace.len(), 1);
    assert!(result.leftovers.is_empty());
    author.finish()?;
    reviewer.finish()?;
    Ok(())
}

#[test]
fn guard_refusal_of_the_reviewer_outranks_the_open_error() -> TestResult {
    let harness = Harness::new()?;
    let author = Scripted::new()
        .expect(Method::Get, PROJECT, project("private"))
        .expect(
            Method::Get,
            format!("{PROJECT}/repository/branches/main"),
            branch_reply(),
        );
    let reviewer = Scripted::new().expect(Method::Get, PROJECT, project("public"));
    let result = harness.run(&author, &reviewer);
    match &result.execution {
        Execution::Error { reason } => assert!(
            reason.starts_with("reviewer: сторож записи: репозиторий не приватный"),
            "{reason}"
        ),
        Execution::Ok => panic!("ожидалась ошибка исполнения"),
    }
    assert_eq!(harness.ledger()?, [target_entry()]);
    author.finish()?;
    reviewer.finish()?;
    Ok(())
}

#[test]
fn refused_branch_is_not_run_and_cleanup_reaches_the_result() -> TestResult {
    let harness = Harness::new()?;
    let base = format!("probe/{RUN_ID}-gl-draft-publish-base");
    let encoded = format!("{PROJECT}/repository/branches/probe%2F{RUN_ID}-gl-draft-publish-base");
    let author = Scripted::new()
        .expect(Method::Get, PROJECT, project("private"))
        .expect(
            Method::Get,
            format!("{PROJECT}/repository/branches/main"),
            branch_reply(),
        )
        .expect(
            Method::Post,
            format!("{PROJECT}/repository/branches"),
            Reply::json(403, &json!({"message": "403 Forbidden"})),
        )
        .expect(
            Method::Get,
            format!("{PROJECT}/merge_requests"),
            Reply::json(200, &json!([])),
        )
        .expect(
            Method::Delete,
            encoded.clone(),
            Reply::json(500, &json!({"message": "500 Internal Server Error"})),
        )
        .expect(Method::Get, encoded, branch_reply())
        .expect(
            Method::Get,
            format!("{PROJECT}/merge_requests"),
            Reply::json(200, &json!([])),
        );
    let reviewer = Scripted::new()
        .expect(Method::Get, PROJECT, project("private"))
        .expect(
            Method::Get,
            "/api/v4/user",
            Reply::json(
                200,
                &json!({"username": "probe-reviewer", "id": 2, "name": "Probe Reviewer"}),
            ),
        );
    let result = harness.run(&author, &reviewer);
    assert!(result.execution.is_ok(), "{:?}", result.execution);
    assert_eq!(result.status, Status::NotRun);
    assert!(
        result.reason.starts_with("токен без прав: ветка цели"),
        "{}",
        result.reason
    );
    assert_eq!(
        harness.ledger()?,
        [target_entry(), Created::Branch { name: base.clone() }],
        "run-id веток — имя каталога прогона"
    );
    let delivered = author.finish()?;
    let created = delivered
        .iter()
        .find(|request| request.method == Method::Post)
        .and_then(Request::body_json)
        .ok_or("нет запроса создания ветки")?;
    assert_eq!(created["branch"], json!(base));
    assert_eq!(result.leftovers, [format!("ветка {base}")]);
    assert!(
        result
            .notes
            .iter()
            .any(|note| note.starts_with(&format!("уборка: удаление ветки {base}: код 500"))),
        "{:?}",
        result.notes
    );
    let text = format!("{:?} {:?}", result.notes, result.trace);
    assert!(
        !result.trace.is_empty() && text.contains("/repository/branches"),
        "{text}"
    );
    for raw in [
        REPO,
        "probe-author%2Fshellio-review-sandbox",
        "probe-author",
    ] {
        assert!(!text.contains(raw), "{raw} очищен: {text}");
    }
    assert!(
        !harness.out.join(SWEPT_FILE).exists(),
        "не убранное не отмечается"
    );
    reviewer.finish()?;
    Ok(())
}

/// Второй клиент, которого тест не ждёт.
struct NoSecond;

struct NoHead;

impl HeadMover for NoHead {
    fn move_head(&self, _: &mut Cell) -> Result<String, Verdict> {
        Err(Verdict::Error(
            "смена head в этом тесте не нужна".to_owned(),
        ))
    }
}

impl SecondClient for NoSecond {
    fn create_draft(&self, _: u64, _: u32) -> Result<String, Verdict> {
        Err(Verdict::Error(
            "второй клиент в этом тесте не нужен".to_owned(),
        ))
    }
}

// --- тело клетки против forge-fake ---

const FAKE_AUTHOR: &str = "probe-author";
const FAKE_REVIEWER: &str = "probe-reviewer";
const FAKE_IID: u64 = 4;
const AUTHOR_TOKEN: &str = "fake-author-token";
const REVIEWER_TOKEN: &str = "fake-reviewer-token";
/// Diff `Fixture::Head` против `Fixture::Base`: строка 03 удалена, 05 изменена, после 24 добавлены
/// две; черновики клетки — на новых строках 4 («line 05 changed») и 24 («line 24a»).
const FIXTURE_DIFF: &str = "@@ -1,8 +1,7 @@\n line 01\n line 02\n-line 03\n line 04\n-line 05\n\
+line 05 changed\n line 06\n line 07\n line 08\n@@ -22,6 +21,8 @@\n line 22\n line 23\n line 24\n\
+line 24a\n+line 24b\n line 25\n line 26\n line 27\n";

fn fake_gitlab() -> Result<FakeServer<GitlabFake>, String> {
    let mut fake = GitlabFake::new(REPO);
    fake.add_user(FAKE_AUTHOR, AUTHOR_TOKEN, Some(Access::Developer));
    fake.add_user(FAKE_REVIEWER, REVIEWER_TOKEN, Some(Access::Developer));
    fake.add_mr(
        FAKE_IID,
        FAKE_AUTHOR,
        BASE,
        HEAD,
        vec![FakeDiff::modified("probe-review/fixture.txt", FIXTURE_DIFF)],
    );
    fake.add_reviewer(FAKE_IID, FAKE_REVIEWER)?;
    FakeServer::start(fake)
}

/// Второй клиент в процессе теста: свой сторож с токеном ревьюера против `forge-fake`, тот же
/// [`foreign_draft`], что у процесса `--second-client`.
struct InProcess<'a> {
    server: &'a FakeServer<GitlabFake>,
}

impl SecondClient for InProcess<'_> {
    fn create_draft(&self, iid: u64, line: u32) -> Result<String, Verdict> {
        let credential = Credential::private_token(Secret::new(REVIEWER_TOKEN));
        let guard = Guarded::new(
            HttpTransport::new(HttpConfig::default(), Some(credential)),
            GuardTarget::gitlab(&Endpoints::instance(self.server.base()), REPO),
            WriteScope::Loopback,
        );
        let review = ReviewTarget {
            provider: Provider::GitLab,
            host: self.server.base().to_owned(),
            repo: REPO.to_owned(),
            number: iid,
        };
        foreign_draft(&guard, &review, FAKE_REVIEWER, line)
    }
}

/// Тело клетки строки над сторожами (`WriteScope::Loopback`) против `forge-fake`, клетка собрана
/// вручную, как после `open_cell`.
fn cell_body(
    server: &FakeServer<GitlabFake>,
    scenario: Scenario,
) -> Result<Result<Observed, Verdict>, Box<dyn std::error::Error>> {
    cell_body_with(server, scenario, &FakePush { server, push: true })
}

/// Diff `Fixture::Moved` против `Fixture::Base`: как [`FIXTURE_DIFF`], и строка 27 изменена.
const MOVED_DIFF: &str = "@@ -1,8 +1,7 @@\n line 01\n line 02\n-line 03\n line 04\n-line 05\n\
+line 05 changed\n line 06\n line 07\n line 08\n@@ -22,6 +21,8 @@\n line 22\n line 23\n line 24\n\
+line 24a\n+line 24b\n line 25\n line 26\n-line 27\n+line 27 moved\n";

/// Смена head в `forge-fake`: новая версия MR с head `NEW_HEAD`; без `push` head не сдвигается,
/// а новый SHA всё равно отдаётся, как коммит, который MR ещё не учёл.
struct FakePush<'a> {
    server: &'a FakeServer<GitlabFake>,
    push: bool,
}

impl HeadMover for FakePush<'_> {
    fn move_head(&self, cell: &mut Cell) -> Result<String, Verdict> {
        if self.push {
            self.server
                .with(|fake| {
                    fake.push_head(
                        FAKE_IID,
                        NEW_HEAD,
                        vec![FakeDiff::modified("probe-review/fixture.txt", MOVED_DIFF)],
                    )
                })
                .map_err(Verdict::Error)?;
        }
        cell.head_commit = NEW_HEAD.to_owned();
        Ok(NEW_HEAD.to_owned())
    }
}

fn cell_body_with(
    server: &FakeServer<GitlabFake>,
    scenario: Scenario,
    mover: &dyn HeadMover,
) -> Result<Result<Observed, Verdict>, Box<dyn std::error::Error>> {
    let state = tempfile::tempdir()?;
    let trace = Trace::new();
    let target = GuardTarget::gitlab(&Endpoints::instance(server.base()), REPO);
    let guard = |token: &str, actor| {
        let credential = Credential::private_token(Secret::new(token));
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
    let second = InProcess { server };
    let cx = GlCx {
        author: &author,
        reviewer: &reviewer,
        trace: &trace,
        host_url: server.base().to_owned(),
        reviewer_login: FAKE_REVIEWER.to_owned(),
        state_dir: state.path(),
        poll: NOW,
        second: &second,
        head: mover,
    };
    let mut cell = Cell {
        names: CellNames::new(RUN_ID, scenario.name())?,
        default_branch: "main".to_owned(),
        number: FAKE_IID,
        base_commit: BASE.to_owned(),
        head_commit: HEAD.to_owned(),
        reviewer: Some(Account {
            login: FAKE_REVIEWER.to_owned(),
            id: 2,
            name: None,
        }),
    };
    let observed = match scenario {
        Scenario::GlDraftPublish => gl_draft_publish(&cx, &mut cell),
        Scenario::GlForeignDraft => gl_foreign_draft(&cx, &mut cell),
        Scenario::GlRequestChanges => gl_request_changes(&cx, &mut cell),
        Scenario::GlAnchors => gl_anchors(&cx, &mut cell),
        Scenario::GlApproveSha => gl_approve_sha(&cx, &mut cell),
        other => return Err(format!("тело {} в тесте не собрано", other.name()).into()),
    };
    let errors = guard_errors(&[("author", &author), ("reviewer", &reviewer)]);
    assert!(errors.is_empty(), "{errors:?}");
    assert!(reviewer.refusal().is_none() && author.refusal().is_none());
    Ok(observed)
}

fn publish_body(
    server: &FakeServer<GitlabFake>,
) -> Result<Result<Observed, Verdict>, Box<dyn std::error::Error>> {
    cell_body(server, Scenario::GlDraftPublish)
}

fn bulk_publishes(server: &FakeServer<GitlabFake>) -> usize {
    server
        .log()
        .iter()
        .filter(|call| call.key.ends_with("bulk_publish"))
        .count()
}

#[test]
fn draft_publish_body_passes_against_the_fake() -> TestResult {
    let server = fake_gitlab()?;
    let observed = publish_body(&server)?.map_err(|verdict| format!("{verdict:?}"))?;
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
        observed.after["notes"].as_array().map(|notes| notes
            .iter()
            .filter(|note| !note["position"].is_null())
            .count()),
        Some(2)
    );
    assert!(server.with(|fake| fake.drafts(FAKE_IID)).is_empty());
    assert_eq!(bulk_publishes(&server), 0);
    Ok(())
}

#[test]
fn refused_publish_fails_the_body_against_the_fake() -> TestResult {
    let server = fake_gitlab()?;
    server.inject(Fault::new(glf::PUBLISH, FakeReply::Status(403)).skip(1));
    let observed = publish_body(&server)?.map_err(|verdict| format!("{verdict:?}"))?;
    assert_eq!(
        observed.checks.verdict().0,
        Status::Fail,
        "{:?}",
        observed.checks.to_json()
    );
    assert_eq!(server.with(|fake| fake.drafts(FAKE_IID)).len(), 1);
    assert_eq!(bulk_publishes(&server), 0);
    Ok(())
}

#[test]
fn rate_limited_publish_is_not_run_against_the_fake() -> TestResult {
    let server = fake_gitlab()?;
    server.inject(Fault::new(glf::PUBLISH, FakeReply::RateLimited));
    match publish_body(&server)? {
        Err(Verdict::NotRun(reason)) => {
            assert_eq!(reason, format!("лимит: код 429 на запись {DRAFT_PUBLISH}"));
        }
        other => panic!("{other:?}"),
    }
    Ok(())
}

#[test]
fn observed_pass_reaches_the_result_scrubbed() -> TestResult {
    let server = fake_gitlab()?;
    let observed = publish_body(&server)?.map_err(|verdict| format!("{verdict:?}"))?;
    let mut scrubber = Scrubber::new();
    scrubber
        .add_repo(REPO)
        .add_login(FAKE_AUTHOR, crate::scrub::LABEL_AUTHOR)
        .add_login(FAKE_REVIEWER, crate::scrub::LABEL_REVIEWER)
        .add_gitlab_ce(server.base());
    let mut result = ResultFile::new("s", "b", "gl-draft-publish", Column::GitlabCe);
    assert_eq!(record_observed(&observed, &scrubber, &mut result), None);
    assert!(result.execution.is_ok(), "{:?}", result.execution);
    assert_eq!(
        result.status,
        Status::Pass,
        "{}: {:?}",
        result.reason,
        result.notes
    );
    for (what, value) in [
        ("до", &result.server_before),
        ("после", &result.server_after),
        ("журнал", &result.journal),
    ] {
        assert!(
            value.as_ref().is_some_and(|value| !value.is_null()),
            "{what}"
        );
    }
    let text = serde_json::to_string(&result)?;
    let address = server.base().trim_start_matches("http://");
    for raw in [REPO, FAKE_AUTHOR, FAKE_REVIEWER, address] {
        assert!(!text.contains(raw), "{raw} очищен: {text}");
    }

    let mut result = ResultFile::new("s", "b", "gl-draft-publish", Column::GitlabCe);
    let empty = Observed {
        after: Value::Null,
        ..Observed::default()
    };
    assert!(matches!(
        record_observed(&empty, &scrubber, &mut result),
        Some(Verdict::Error(reason)) if reason.contains("снимок сервера после")
    ));
    Ok(())
}

#[test]
fn foreign_draft_body_passes_against_the_fake() -> TestResult {
    let server = fake_gitlab()?;
    let observed =
        cell_body(&server, Scenario::GlForeignDraft)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(
        status,
        Status::Pass,
        "{reason}: {:?}",
        observed.checks.to_json()
    );
    let drafts = server.with(|fake| fake.drafts(FAKE_IID));
    assert_eq!(
        drafts
            .iter()
            .map(|draft| draft.note.as_str())
            .collect::<Vec<_>>(),
        vec![FOREIGN_NOTE]
    );
    assert_eq!(server.with(|fake| fake.notes(FAKE_IID)).len(), 1);
    assert_eq!(bulk_publishes(&server), 0);
    assert!(
        observed
            .notes
            .iter()
            .any(|note| note.starts_with("второй клиент: черновик ")),
        "{:?}",
        observed.notes
    );
    Ok(())
}

#[test]
fn second_client_refusal_is_not_run_and_its_transport_failure_is_an_error() -> TestResult {
    let server = fake_gitlab()?;
    server.inject(Fault::new(glf::DRAFTS, FakeReply::Status(403)));
    match cell_body(&server, Scenario::GlForeignDraft)? {
        Err(Verdict::NotRun(reason)) => {
            assert!(
                reason.starts_with("второй клиент не создал черновик"),
                "{reason}"
            );
        }
        other => panic!("{other:?}"),
    }
    let server = fake_gitlab()?;
    server.inject(Fault::new(glf::DRAFTS, FakeReply::Status(502)));
    assert!(matches!(
        cell_body(&server, Scenario::GlForeignDraft)?,
        Err(Verdict::Error(reason)) if reason.starts_with("второй клиент")
    ));
    Ok(())
}

#[test]
fn foreign_draft_checks_catch_a_published_foreign_draft() {
    let writes = vec![
        write(1, DRAFT_CREATE, None, Some("2")),
        write(2, DRAFT_PUBLISH, Some("2"), None),
        write(3, DRAFT_PUBLISH, Some("1"), None),
    ];
    let notes = vec![
        note(10, "свой", false, Some(position(4))),
        note(11, FOREIGN_NOTE, false, Some(position(24))),
    ];
    let mut ids = BTreeSet::new();
    ids.insert("1".to_owned());
    let checks = foreign_draft_checks(&writes, "1", &[], &notes, (4, "свой"), &facts(), &ids);
    let failed: Vec<String> = checks
        .to_json()
        .into_iter()
        .filter(|line| line.starts_with("НЕТ "))
        .collect();
    assert_eq!(checks.verdict().0, Status::Fail);
    for what in [
        "записи ревьюера",
        "ID второго клиента не опубликован",
        "черновики ревьюера после отправки",
        "автор не видит текста чужого черновика",
        "ID второго клиента нет в журнале",
    ] {
        assert!(
            failed
                .iter()
                .any(|line| line.starts_with(&format!("НЕТ {what}:"))),
            "{what}: {failed:?}"
        );
    }
}

#[test]
fn second_client_reply_forms() {
    assert_eq!(
        second_reply(Some(0), b"{\"id\":\"7\"}\n", b""),
        Ok("7".to_owned())
    );
    assert_eq!(
        second_reply(Some(0), b"{\"not_run\":\"x\"}", b""),
        Err(Verdict::NotRun("x".to_owned()))
    );
    assert_eq!(
        second_reply(Some(3), b"{\"error\":\"y\"}", b""),
        Err(Verdict::Error("второй клиент: y".to_owned()))
    );
    for (code, out) in [
        (Some(0), &b"{\"error\":\"y\"}"[..]),
        (Some(3), &b"{\"id\":\"7\"}"[..]),
        (None, &b""[..]),
        (Some(0), &b"{\"id\":\"\"}"[..]),
    ] {
        assert!(
            matches!(second_reply(code, out, b"stderr"), Err(Verdict::Error(_))),
            "{code:?} {out:?}"
        );
    }
}

// --- gl-request-changes ---

fn request_changes_fake(mode: glf::RequestChangesMode) -> Result<FakeServer<GitlabFake>, String> {
    let server = fake_gitlab()?;
    server.with(|fake| fake.set_request_changes(mode));
    Ok(server)
}

fn mutation_calls(server: &FakeServer<GitlabFake>) -> usize {
    server
        .log()
        .iter()
        .filter(|call| call.key == glf::REQUEST_CHANGES)
        .count()
}

fn request_changes_body(
    server: &FakeServer<GitlabFake>,
) -> Result<Observed, Box<dyn std::error::Error>> {
    cell_body(server, Scenario::GlRequestChanges)?.map_err(|verdict| format!("{verdict:?}").into())
}

#[test]
fn request_changes_body_passes_against_the_fake() -> TestResult {
    let server = request_changes_fake(glf::RequestChangesMode::Available)?;
    let observed = request_changes_body(&server)?;
    assert_eq!(observed.verdict, None);
    let (status, reason) = observed.checks.verdict();
    assert_eq!(
        status,
        Status::Pass,
        "{reason}: {:?}",
        observed.checks.to_json()
    );
    assert_eq!(
        server.with(|fake| fake.reviewer_state(FAKE_IID, FAKE_REVIEWER)),
        Some(glf::ReviewerState::RequestedChanges)
    );
    assert_eq!(observed.after["reviewer_state"], "REQUESTED_CHANGES");
    assert_ne!(observed.before["reviewer_state"], "REQUESTED_CHANGES");
    assert!(server.with(|fake| fake.notes(FAKE_IID)).is_empty());
    // Вызов ревьюера и вызов-наблюдение автора.
    assert_eq!(mutation_calls(&server), 2);
    let injections = observed
        .notes
        .iter()
        .filter(|note| note.ends_with(&format!("not_run «{INJECTION_UNAVAILABLE}»")))
        .count();
    assert_eq!(injections, 2, "{:?}", observed.notes);
    assert!(
        observed.notes.iter().any(|note| note.starts_with(
            "наблюдение: mergeRequestRequestChanges токеном автора (не ревьюер MR): отказ: вызов: "
        )),
        "{:?}",
        observed.notes
    );
    assert!(!observed.journal.is_null());
    Ok(())
}

#[test]
fn request_changes_absent_from_the_schema_is_absent_without_a_call() -> TestResult {
    let server = request_changes_fake(glf::RequestChangesMode::Absent)?;
    let observed = request_changes_body(&server)?;
    assert!(
        matches!(&observed.verdict, Some(Verdict::Absent(reason)) if reason.contains("нет в схеме")),
        "{:?}",
        observed.verdict
    );
    assert_eq!(mutation_calls(&server), 0);
    assert_eq!(
        observed
            .notes
            .iter()
            .filter(|note| note.contains(INJECTION_UNAVAILABLE))
            .count(),
        1,
        "{:?}",
        observed.notes
    );
    let mut scrubber = Scrubber::new();
    scrubber.add_gitlab_ce(server.base());
    let mut result = ResultFile::new("s", "b", "gl-request-changes", Column::GitlabCe);
    result.host.version = Some("GitLab 19.6.1 (CE)".to_owned());
    let verdict = record_observed(&observed, &scrubber, &mut result);
    assert!(matches!(verdict, Some(Verdict::Absent(_))), "{verdict:?}");
    assert!(result.server_after.is_some() && result.journal.is_some());
    Ok(())
}

#[test]
fn refused_request_changes_fails_and_an_execution_error_is_not_observed() -> TestResult {
    for mode in [
        glf::RequestChangesMode::Forbidden,
        glf::RequestChangesMode::PayloadError,
    ] {
        let server = request_changes_fake(mode)?;
        let observed = request_changes_body(&server)?;
        assert!(
            matches!(&observed.verdict, Some(Verdict::Fail(reason))
                if reason.starts_with("mergeRequestRequestChanges: ")),
            "{mode:?}: {:?}",
            observed.verdict
        );
        assert_ne!(
            server.with(|fake| fake.reviewer_state(FAKE_IID, FAKE_REVIEWER)),
            Some(glf::ReviewerState::RequestedChanges)
        );
        assert!(server.with(|fake| fake.notes(FAKE_IID)).is_empty());
    }
    let server = request_changes_fake(glf::RequestChangesMode::ExecutionError)?;
    let observed = request_changes_body(&server)?;
    assert!(
        matches!(&observed.verdict, Some(Verdict::NotRun(reason)) if reason.starts_with(WRITE_UNOBSERVED)),
        "{:?}",
        observed.verdict
    );
    Ok(())
}

fn caps_with(mutations: &[&str]) -> Capabilities {
    Capabilities {
        username: FAKE_REVIEWER.to_owned(),
        host: crate::gitlab::HostVersion {
            version: "19.6.1".to_owned(),
            revision: None,
            enterprise: Some(false),
            source: "metadata",
        },
        plan: None,
        plan_source: None,
        visibility: "private".to_owned(),
        access_level: Some(30),
        mutations: mutations.iter().map(|name| (*name).to_owned()).collect(),
        introspection: crate::gitlab::capabilities::FORM_TYPE,
    }
}

fn request_changes_report(state: ItemState, detail: &str) -> Report {
    let mut item = item(1, Operation::GitLabRequestChanges, state);
    item.detail = Some(detail.to_owned());
    Report {
        submission: SubmissionId(1),
        delivery: Delivery::Partial,
        closed: true,
        unresolved: usize::from(state == ItemState::Unknown),
        items: vec![item],
    }
}

#[test]
fn request_changes_outcome_puts_schema_and_tier_before_the_delivery_rule() {
    let caps = caps_with(&[capabilities::REQUEST_CHANGES]);
    let unobserved = Verdict::NotRun(WRITE_UNOBSERVED.to_owned());
    let tier = "GraphQL: This feature requires GitLab Premium";
    for state in [ItemState::Failed, ItemState::Unknown] {
        let delivery = (state == ItemState::Unknown).then_some(&unobserved);
        assert!(matches!(
            request_changes_outcome(&caps, &request_changes_report(state, tier), delivery),
            Some(Verdict::Absent(reason)) if reason.contains("признак тарифа")
        ));
    }
    let schema = ApiError::Absent(capabilities::REQUEST_CHANGES).to_string();
    assert!(matches!(
        request_changes_outcome(&caps, &request_changes_report(ItemState::Failed, &schema), None),
        Some(Verdict::Absent(reason)) if reason.contains("нет в схеме")
    ));
    assert!(matches!(
        request_changes_outcome(
            &caps_with(&[]),
            &request_changes_report(ItemState::Failed, "x"),
            None
        ),
        Some(Verdict::Absent(_))
    ));
    assert_eq!(
        request_changes_outcome(
            &caps,
            &request_changes_report(ItemState::Unknown, "502"),
            Some(&unobserved)
        ),
        Some(unobserved.clone())
    );
    let limit =
        Verdict::NotRun("лимит: код 429 на запись graphql mergeRequestRequestChanges".to_owned());
    assert_eq!(
        request_changes_outcome(
            &caps,
            &request_changes_report(ItemState::Failed, "Retry later"),
            Some(&limit)
        ),
        Some(limit.clone())
    );
    assert_eq!(
        request_changes_outcome(
            &caps,
            &request_changes_report(ItemState::Failed, "payload.errors: Reviewer not found"),
            None
        ),
        Some(Verdict::Fail(
            "mergeRequestRequestChanges: payload.errors: Reviewer not found".to_owned()
        ))
    );
    assert_eq!(
        request_changes_outcome(
            &caps,
            &request_changes_report(ItemState::Confirmed, ""),
            None
        ),
        None
    );
    assert!(matches!(
        request_changes_outcome(&caps, &sent(&[ItemState::Confirmed]), None),
        Some(Verdict::Error(_))
    ));
}

#[test]
fn request_changes_checks_count_writes_and_read_the_state_by_the_author() {
    let journal = facts();
    let writes = vec![write(5, REQUEST_CHANGES, None, None)];
    let pass = request_changes_checks(
        &writes,
        true,
        Some(ReviewerState::RequestedChanges),
        &journal,
    );
    assert_eq!(pass.verdict().0, Status::Pass, "{:?}", pass.to_json());
    let substituted = vec![
        write(5, REQUEST_CHANGES, None, None),
        write(6, NOTE, None, None),
    ];
    let fail = request_changes_checks(&substituted, true, Some(ReviewerState::Reviewed), &journal);
    assert_eq!(fail.verdict().0, Status::Fail);
    let failed: Vec<String> = fail
        .to_json()
        .into_iter()
        .filter(|line| line.starts_with("НЕТ"))
        .collect();
    assert_eq!(failed.len(), 2, "{failed:?}");
}

// --- gl-anchors ---

#[test]
fn anchors_body_passes_against_the_fake_and_observes_both_ranges() -> TestResult {
    let server = fake_gitlab()?;
    let observed =
        cell_body(&server, Scenario::GlAnchors)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(
        status,
        Status::Pass,
        "{reason}: {:?}",
        observed.checks.to_json()
    );
    assert!(server.with(|fake| fake.drafts(FAKE_IID)).is_empty());
    assert_eq!(server.with(|fake| fake.notes(FAKE_IID)).len(), 5);
    let ranges: Vec<&String> = observed
        .notes
        .iter()
        .filter(|note| note.starts_with("наблюдение line_range"))
        .collect();
    assert_eq!(ranges.len(), 2, "{:?}", observed.notes);
    // forge-fake хранит line_range как прислано: обе стороны совпадают.
    assert!(
        ranges
            .iter()
            .all(|note| note.ends_with("line_code совпадает, type совпадает")),
        "{ranges:?}"
    );
    let path_sha = sha1_smol::Sha1::from("probe-review/fixture.txt")
        .digest()
        .to_string();
    let deleted = ranges
        .iter()
        .find(|note| note.contains("«диапазон 03–04»"))
        .ok_or("нет диапазона 03–04")?;
    assert!(deleted.contains(&format!("{path_sha}_3_3")), "{deleted}");
    assert!(deleted.contains(&format!("{path_sha}_4_3")), "{deleted}");
    assert_eq!(bulk_publishes(&server), 0);
    Ok(())
}

fn anchor_note(body: &str, range: Option<Value>) -> RemoteNote {
    let mut position = position(24);
    position.line_range = range;
    note(9, body, false, Some(position))
}

#[test]
fn range_observations_report_missing_and_diverging_ends() {
    let sent = json!({"line_range": {
        "start": {"line_code": "abc_24_23", "type": null},
        "end": {"line_code": "abc_25_24", "type": "new"},
    }});
    let created = vec![
        ("диапазон".to_owned(), sent.clone()),
        ("строка".to_owned(), json!({"new_line": 4})),
    ];
    let returned = json!({
        "start": {"line_code": "abc_24_23", "type": "old"},
        "end": {"line_code": "abc_24_24", "type": "new"},
    });
    let diverging = range_observations(&created, &[anchor_note("диапазон", Some(returned))]);
    assert_eq!(diverging.len(), 1, "{diverging:?}");
    assert!(
        diverging[0].ends_with("line_code расходится, type расходится"),
        "{diverging:?}"
    );
    let missing = range_observations(&created, &[anchor_note("диапазон", None)]);
    assert!(
        missing[0].ends_with("GitLab не вернул line_range"),
        "{missing:?}"
    );
}

#[test]
fn anchors_checks_catch_a_wrong_triple_and_a_lost_note() {
    let created: Vec<(String, Value)> = ANCHOR_DRAFTS
        .iter()
        .map(|(_, body, _, _)| {
            (
                (*body).to_owned(),
                json!({"position_type": "text", "base_sha": BASE, "start_sha": BASE, "head_sha": HEAD}),
            )
        })
        .collect();
    let writes: Vec<Write> = (1..=5)
        .map(|id| write(id, DRAFT_CREATE, None, Some(&id.to_string())))
        .chain((1..=5).map(|id| write(10 + id, DRAFT_PUBLISH, Some(&id.to_string()), None)))
        .collect();
    let notes: Vec<RemoteNote> = ANCHOR_DRAFTS
        .iter()
        .map(|(_, body, old, new)| {
            let mut position = position(1);
            position.old_line = *old;
            position.new_line = *new;
            note(1, body, false, Some(position))
        })
        .collect();
    let triple = [BASE, BASE, HEAD];
    let pass = anchors_checks(&writes, &created, triple, &[], &notes, &facts());
    assert_eq!(pass.verdict().0, Status::Pass, "{:?}", pass.to_json());
    let stale = anchors_checks(
        &writes,
        &created,
        [BASE, BASE, NEW_HEAD],
        &[],
        &notes,
        &facts(),
    );
    assert_eq!(stale.verdict().0, Status::Fail);
    let lost = anchors_checks(
        &writes,
        &created,
        triple,
        &[],
        notes.get(1..).unwrap_or_default(),
        &facts(),
    );
    assert_eq!(lost.verdict().0, Status::Fail);
}

#[test]
fn author_observation_refusal_is_a_note_and_its_stand_error_is_an_error() -> TestResult {
    let scripted = Scripted::new().expect(Method::Get, "/api/v4/user", Reply::status(403));
    let gitlab = GitLab::new(&scripted, Endpoints::instance(ROOT), &review())?;
    let note = author_request_changes(&gitlab).map_err(|verdict| format!("{verdict:?}"))?;
    assert!(
        note.starts_with(
            "наблюдение: mergeRequestRequestChanges токеном автора (не ревьюер MR): отказ: \
             возможности автора: "
        ),
        "{note}"
    );
    scripted.finish()?;

    let scripted = Scripted::new().expect(Method::Get, "/api/v4/user", Reply::status(502));
    let gitlab = GitLab::new(&scripted, Endpoints::instance(ROOT), &review())?;
    assert!(matches!(
        author_request_changes(&gitlab),
        Err(Verdict::Error(reason)) if reason.starts_with("наблюдение токеном автора")
    ));
    scripted.finish()?;
    Ok(())
}

// --- gl-approve-sha ---

#[test]
fn approve_sha_body_passes_against_the_fake() -> TestResult {
    let server = fake_gitlab()?;
    let observed =
        cell_body(&server, Scenario::GlApproveSha)?.map_err(|verdict| format!("{verdict:?}"))?;
    let (status, reason) = observed.checks.verdict();
    assert_eq!(
        status,
        Status::Pass,
        "{reason}: {:?}",
        observed.checks.to_json()
    );
    assert!(observed.verdict.is_none());
    assert_eq!(observed.before["approved_by"], json!([]));
    assert_eq!(observed.after["approved_by"], json!([]));
    assert!(!observed.journal.is_null());
    assert!(server.with(|fake| fake.approved_by(FAKE_IID)).is_empty());
    let approves: Vec<ApproveCall> = server
        .log()
        .iter()
        .filter(|call| call.key == glf::APPROVE)
        .map(|call| (call.body["sha"].as_str().map(str::to_owned), call.status))
        .collect();
    assert_eq!(
        approves,
        vec![
            (Some(HEAD.to_owned()), Some(201)),
            (Some(HEAD.to_owned()), Some(409)),
        ]
    );
    assert!(
        observed
            .notes
            .iter()
            .any(|note| note.ends_with(&format!("новый head {NEW_HEAD}"))),
        "{:?}",
        observed.notes
    );
    Ok(())
}

#[test]
fn refused_first_approve_is_absent_with_artifacts() -> TestResult {
    let server = fake_gitlab()?;
    server.inject(Fault::new(glf::APPROVE, FakeReply::Status(403)));
    let observed =
        cell_body(&server, Scenario::GlApproveSha)?.map_err(|verdict| format!("{verdict:?}"))?;
    assert!(
        matches!(&observed.verdict, Some(Verdict::Absent(reason)) if reason.contains("код 403")),
        "{:?}",
        observed.verdict
    );
    for (what, value) in [
        ("до", &observed.before),
        ("после", &observed.after),
        ("журнал", &observed.journal),
    ] {
        assert!(!value.is_null(), "{what}");
    }
    let unapproves = server
        .log()
        .iter()
        .filter(|call| call.key.ends_with("/unapprove"))
        .count();
    assert_eq!(unapproves, 0, "после отказа клетка дальше не идёт");
    Ok(())
}

#[test]
fn head_the_mr_did_not_take_is_not_run() -> TestResult {
    let server = fake_gitlab()?;
    let verdict = match cell_body_with(
        &server,
        Scenario::GlApproveSha,
        &FakePush {
            server: &server,
            push: false,
        },
    )? {
        Ok(observed) => return Err(format!("ожидался not_run: {:?}", observed.checks).into()),
        Err(verdict) => verdict,
    };
    assert!(
        matches!(&verdict, Verdict::NotRun(reason) if reason.starts_with(MR_HEAD_STALE)
            && reason.contains(&format!("head {HEAD}"))),
        "{verdict:?}"
    );
    Ok(())
}

fn approve_facts_pass() -> (Vec<Write>, Vec<ApproveCall>) {
    (
        vec![
            write(1, APPROVE, None, None),
            write(2, UNAPPROVE, None, None),
            write(3, APPROVE, None, None),
        ],
        vec![
            (Some(HEAD.to_owned()), Some(201)),
            (Some(HEAD.to_owned()), Some(409)),
        ],
    )
}

#[test]
fn approve_sha_checks_fail_on_an_accepted_or_refused_stale_sha() {
    let (writes, approves) = approve_facts_pass();
    let reviewer = vec!["Probe-Reviewer".to_owned()];
    let journal = facts();
    let rejected = Err(ApiError::ShaRejected("SHA does not match".to_owned()));
    let base = ApproveFacts {
        writes: &writes,
        approves: &approves,
        old_head: HEAD,
        reviewer: "probe-reviewer",
        after_first: &reviewer,
        at_end: &[],
        stopped: (true, "HeadChanged".to_owned()),
        stale: &rejected,
        stale_planned: true,
        journal: &journal,
    };
    let (status, reason) = approve_sha_checks(&base).verdict();
    assert_eq!(status, Status::Pass, "{reason}");

    let accepted = Ok(());
    let stale_accepted = [
        (Some(HEAD.to_owned()), Some(201)),
        (Some(HEAD.to_owned()), Some(201)),
    ];
    let checks = approve_sha_checks(&ApproveFacts {
        approves: &stale_accepted,
        stale: &accepted,
        at_end: &reviewer,
        ..base.clone()
    });
    let (status, failed) = checks.verdict();
    assert_eq!(status, Status::Fail);
    for name in [
        "approve: sha и коды",
        "approve с устаревшим sha — 409",
        "ревьюера нет в одобривших в конце",
    ] {
        assert!(failed.contains(name), "{name}: {failed}");
    }

    let unauthorized = Err(ApiError::Status {
        status: 401,
        message: "401 Unauthorized".to_owned(),
    });
    let refused = [
        (Some(HEAD.to_owned()), Some(201)),
        (Some(HEAD.to_owned()), Some(401)),
    ];
    let checks = approve_sha_checks(&ApproveFacts {
        approves: &refused,
        stale: &unauthorized,
        ..base.clone()
    });
    assert_eq!(checks.verdict().0, Status::Fail);

    let checks = approve_sha_checks(&ApproveFacts {
        stopped: (false, "Ok".to_owned()),
        stale_planned: false,
        ..base.clone()
    });
    assert_eq!(checks.verdict().0, Status::Fail);
}
