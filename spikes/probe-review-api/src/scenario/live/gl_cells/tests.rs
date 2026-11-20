use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::gitlab::Endpoints;
use crate::journal::{Operation, SubmissionId, SubmitItem};
use crate::live::guard::{GuardTarget, WriteScope};
use crate::live::lifecycle::{Created, SWEPT_FILE};
use crate::model::ProviderId;
use crate::results::{Column, Execution, Status};
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
