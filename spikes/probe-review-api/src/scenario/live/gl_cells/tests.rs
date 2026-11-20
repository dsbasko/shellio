use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::gitlab::Endpoints;
use crate::model::ProviderId;
use crate::results::Status;
use crate::transport::scripted::{Reply, Scripted};
use crate::transport::{Request, Response};

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
    let mut detail = version_json(5, head);
    detail["diffs"] = json!([{
        "old_path": "f.txt", "new_path": "f.txt",
        "diff": "@@ -1,2 +1,2 @@\n a\n-b\n+c\n",
    }]);
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
