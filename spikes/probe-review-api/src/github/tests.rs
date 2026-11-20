use serde_json::{Value, json};

use super::*;
use crate::fake::FakeServer;
use crate::fake::github::{
    CommentSpec, DiffSide, FakeCommit, FakeFile, GithubFake, Permission, ReviewState as FakeState,
    Viewed,
};
use crate::model::anchor::{Anchor, LinePoint, Side};
use crate::model::{DraftId, Placement};
use crate::transport::http::{Credential, HttpConfig, HttpTransport, Secret};
use crate::transport::scripted::{Reply, Scripted};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const API: &str = "https://api.test";
const BASE: &str = "1111111111111111111111111111111111111111";
const HEAD: &str = "2222222222222222222222222222222222222222";
const NEW_HEAD: &str = "3333333333333333333333333333333333333333";
const PATH: &str = "src/a.rs";
const PATCH: &str = "@@ -1,4 +1,4 @@\n ctx1\n-old2\n+new2\n ctx3\n ctx4\n@@ -10,2 +10,3 @@\n ctx10\n+add11\n ctx11\n";
const PULL: &str = "/repos/o/r/pulls/7";
const REVIEWS: &str = "/repos/o/r/pulls/7/reviews";
const REVIEWS_PAGE_1: &str = "/repos/o/r/pulls/7/reviews?per_page=100&page=1";
const REVIEWS_PAGE_2: &str = "/repos/o/r/pulls/7/reviews?per_page=100&page=2";

fn target() -> ReviewTarget {
    ReviewTarget {
        provider: Provider::GitHub,
        host: "github.com".to_owned(),
        repo: "o/r".to_owned(),
        number: 7,
    }
}

fn revision(head: &str) -> Result<Revision, ModelError> {
    let base = Sha::parse(BASE)?;
    Ok(Revision::new(base.clone(), base, Sha::parse(head)?))
}

fn session_on(head: &str) -> Result<ReviewSession, ModelError> {
    Ok(ReviewSession::new("bob", target(), revision(head)?))
}

fn file() -> Result<FileDiff, ModelError> {
    FileDiff::parse(PATH, PATH, PATCH)
}

/// Три черновика на строках: добавленная (новая сторона), удалённая (старая), диапазон.
fn drafts(session: &ReviewSession) -> Result<Vec<DraftComment>, ModelError> {
    let file = file()?;
    let added = Anchor::line(&file, LinePoint::at(&file, Side::New, 2)?)?;
    let removed = Anchor::line(&file, LinePoint::at(&file, Side::Old, 2)?)?;
    let range = Anchor::range(
        &file,
        LinePoint::at(&file, Side::New, 1)?,
        LinePoint::at(&file, Side::New, 3)?,
    )?;
    Ok(vec![
        DraftComment::new(DraftId(1), session, Placement::Line(added), "added"),
        DraftComment::new(DraftId(2), session, Placement::Line(removed), "removed"),
        DraftComment::new(DraftId(3), session, Placement::Line(range), "range"),
    ])
}

fn caps() -> Capabilities {
    Capabilities {
        login: "bob".to_owned(),
        plan: None,
        token_scopes: None,
        api: ApiVersion {
            requested: API_VERSION.to_owned(),
            supported: Some(vec![API_VERSION.to_owned()]),
        },
        repo_private: true,
        can_push: true,
        mutations: capabilities::MUTATIONS
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
    }
}

/// Итог проверки `PENDING` для тестов, где само чтение не проверяется.
fn clear() -> NoPending {
    NoPending { _checked: () }
}

fn adapter(scripted: &Scripted) -> Result<GitHub<&Scripted>, ApiError> {
    GitHub::new(scripted, Endpoints::single(API), &target())
}

fn review(id: u64, user: &str, state: &str, commit: &str) -> Value {
    json!({
        "id": id,
        "node_id": format!("PRR_{id}"),
        "user": {"login": user},
        "body": "",
        "state": state,
        "commit_id": commit,
    })
}

fn rest_comment(id: u64, review: u64, line: u32, body: &str) -> Value {
    json!({
        "id": id,
        "node_id": format!("PRRC_{id}"),
        "pull_request_review_id": review,
        "user": {"login": "bob"},
        "body": body,
        "path": PATH,
        "line": line,
        "side": "RIGHT",
        "start_line": null,
        "start_side": null,
        "commit_id": HEAD,
    })
}

fn thread_reply(thread: &str, id: &str, database_id: u64, body: &str, review: &str) -> Reply {
    Reply::json(
        200,
        &json!({"data": {"addPullRequestReviewThread": {"thread": {
            "id": thread,
            "comments": {"nodes": [{
                "id": id,
                "databaseId": database_id,
                "body": body,
                "pullRequestReview": {"id": review},
            }]},
        }}}}),
    )
}

fn update_reply(id: &str, database_id: u64, body: &str, review: &str) -> Reply {
    Reply::json(
        200,
        &json!({"data": {"updatePullRequestReviewComment": {"pullRequestReviewComment": {
            "id": id,
            "databaseId": database_id,
            "body": body,
            "pullRequestReview": {"id": review},
        }}}}),
    )
}

fn empty_page() -> Reply {
    Reply::json(200, &json!([]))
}

/// Ссылка на следующую страницу ведёт на чужой хост: адаптер должен строить адрес сам.
fn page_with_next(items: Value, path: &str) -> Reply {
    Reply::Respond(Response::json(200, &items).with_header(
        "link",
        format!(
            "<https://elsewhere.test{path}?per_page=100&page=2>; rel=\"next\", \
                 <https://elsewhere.test{path}?per_page=100&page=2>; rel=\"last\""
        ),
    ))
}

fn bodies(requests: &[Request]) -> Vec<Value> {
    requests
        .iter()
        .map(|request| request.body_json().unwrap_or(Value::Null))
        .collect()
}

#[test]
fn capabilities_are_read_from_the_host() -> TestResult {
    let mut fields: Vec<Value> = capabilities::MUTATIONS
        .iter()
        .map(|name| json!({"name": name}))
        .collect();
    fields.push(json!({"name": "createIssue"}));
    let scripted = Scripted::new()
        .expect(
            Method::Get,
            "/user",
            Reply::Respond(
                Response::json(200, &json!({"login": "bob", "plan": {"name": "pro"}}))
                    .with_header("x-oauth-scopes", "repo, read:org"),
            ),
        )
        .expect(
            Method::Get,
            "/versions",
            Reply::json(200, &json!([API_VERSION])),
        )
        .expect(
            Method::Get,
            "/repos/o/r",
            Reply::json(
                200,
                &json!({"private": true, "permissions": {"push": true}}),
            ),
        )
        .expect(
            Method::Post,
            "/graphql",
            Reply::json(200, &json!({"data": {"__type": {"fields": fields}}})),
        );
    let caps = adapter(&scripted)?.capabilities()?;
    assert_eq!(caps.login, "bob");
    assert_eq!(caps.plan.as_deref(), Some("pro"));
    assert_eq!(
        caps.token_scopes,
        Some(vec!["repo".to_owned(), "read:org".to_owned()])
    );
    assert_eq!(caps.api.requested, API_VERSION);
    assert_eq!(caps.api.supported, Some(vec![API_VERSION.to_owned()]));
    assert!(caps.repo_private && caps.can_push);
    assert_eq!(caps.mutations, capabilities::MUTATIONS.to_vec());
    assert!(caps.server_pending() && caps.viewed() && caps.resolve());
    let requests = scripted.finish()?;
    for request in &requests[..3] {
        assert!(request.url.starts_with(API));
        assert!(
            request
                .headers
                .contains(&("x-github-api-version".to_owned(), API_VERSION.to_owned()))
        );
        assert!(
            request
                .headers
                .contains(&("accept".to_owned(), ACCEPT.to_owned()))
        );
    }
    assert_eq!(requests[3].url, format!("{API}/graphql"));
    assert_eq!(
        requests[3].body_json(),
        Some(json!({
            "query": capabilities::INTROSPECT,
            "variables": {"name": "Mutation"},
        }))
    );
    Ok(())
}

#[test]
fn missing_versions_and_mutations_are_recorded_and_gate_branches() -> TestResult {
    let scripted = Scripted::new()
        .expect(
            Method::Get,
            "/user",
            Reply::json(200, &json!({"login": "bob"})),
        )
        .expect(
            Method::Get,
            "/versions",
            Reply::json(404, &json!({"message": "Not Found"})),
        )
        .expect(
            Method::Get,
            "/repos/o/r",
            Reply::json(200, &json!({"private": false})),
        )
        .expect(
            Method::Post,
            "/graphql",
            Reply::json(
                200,
                &json!({"data": {"__type": {"fields": [
                    {"name": "addPullRequestReviewThread"},
                    {"name": "markFileAsViewed"},
                ]}}}),
            ),
        );
    let caps = adapter(&scripted)?.capabilities()?;
    assert_eq!(caps.api.supported, None);
    assert_eq!(caps.plan, None);
    assert_eq!(caps.token_scopes, None);
    assert!(!caps.repo_private && !caps.can_push);
    assert!(!caps.server_pending() && !caps.viewed() && !caps.resolve());
    assert_eq!(caps.to_json()["branches"]["server_pending"], false);
    scripted.finish()?;

    let idle = Scripted::new();
    let github = adapter(&idle)?;
    let session = session_on(HEAD)?;
    let absent = github.start_pending(&session, &caps, &clear());
    assert!(matches!(absent, Err(ApiError::Absent(_))), "{absent:?}");
    let viewed = github.set_viewed(&caps, &ProviderId::new("PR_7"), PATH, true);
    assert!(matches!(viewed, Err(ApiError::Absent(_))), "{viewed:?}");
    assert!(idle.finish()?.is_empty());
    Ok(())
}

#[test]
fn local_branch_is_one_create_review_with_comments_and_event() -> TestResult {
    let session = session_on(HEAD)?;
    let drafts = drafts(&session)?;
    let scripted = Scripted::new()
        .expect(Method::Get, REVIEWS_PAGE_1, empty_page())
        .expect(
            Method::Post,
            REVIEWS,
            Reply::json(200, &review(501, "bob", "CHANGES_REQUESTED", HEAD)),
        );
    let github = adapter(&scripted)?;
    let clear = github.check_no_pending(&caps())?;
    let submitted = github.create_review(
        &session,
        &clear,
        &drafts,
        ReviewEvent::RequestChanges,
        "Итог",
    )?;
    assert_eq!(submitted.id, ProviderId::new("501"));
    assert_eq!(submitted.node_id, ProviderId::new("PRR_501"));
    assert_eq!(submitted.state, ReviewState::ChangesRequested);
    let requests = scripted.finish()?;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests.iter().filter(|r| r.method.is_write()).count(),
        1,
        "одна запись"
    );
    assert_eq!(
        requests[1].body_json(),
        Some(json!({
            "commit_id": HEAD,
            "body": "Итог",
            "event": "REQUEST_CHANGES",
            "comments": [
                {"path": PATH, "line": 2, "side": "RIGHT", "body": "added"},
                {"path": PATH, "line": 2, "side": "LEFT", "body": "removed"},
                {"path": PATH, "line": 3, "side": "RIGHT", "start_line": 1,
                 "start_side": "RIGHT", "body": "range"},
            ],
        }))
    );
    Ok(())
}

#[test]
fn local_branch_each_event_and_optional_body() -> TestResult {
    let session = session_on(HEAD)?;
    let cases = [
        (
            ReviewEvent::Comment,
            "Итог",
            "COMMENTED",
            json!({"commit_id": HEAD, "body": "Итог", "event": "COMMENT"}),
        ),
        (
            ReviewEvent::Approve,
            "",
            "APPROVED",
            json!({"commit_id": HEAD, "event": "APPROVE"}),
        ),
    ];
    for (event, body, state, expected) in cases {
        let scripted = Scripted::new().expect(
            Method::Post,
            REVIEWS,
            Reply::json(200, &review(502, "bob", state, HEAD)),
        );
        let submitted = adapter(&scripted)?.create_review(&session, &clear(), &[], event, body)?;
        assert_eq!(submitted.state, event.state());
        let requests = scripted.finish()?;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body_json(), Some(expected));
    }
    Ok(())
}

#[test]
fn local_branch_refuses_bad_input_before_any_request() -> TestResult {
    let session = session_on(HEAD)?;
    let file = file()?;
    let anchor = Anchor::line(&file, LinePoint::at(&file, Side::New, 2)?)?;
    let reply = DraftComment::new(
        DraftId(4),
        &session,
        Placement::Reply {
            thread: ProviderId::new("PRRT_1"),
        },
        "reply",
    );
    let other = DraftComment::new(
        DraftId(5),
        &session_on(NEW_HEAD)?,
        Placement::Line(anchor),
        "other revision",
    );
    let scripted = Scripted::new();
    let github = adapter(&scripted)?;
    let blank = github.create_review(&session, &clear(), &[], ReviewEvent::Comment, " ");
    assert!(matches!(blank, Err(ApiError::Invalid(_))), "{blank:?}");
    let blank = github.create_review(&session, &clear(), &[], ReviewEvent::RequestChanges, "");
    assert!(matches!(blank, Err(ApiError::Invalid(_))), "{blank:?}");
    let replied = github.create_review(&session, &clear(), &[reply], ReviewEvent::Approve, "");
    assert_eq!(replied, Err(ApiError::Model(ModelError::NotAnchored)));
    let mixed = github.create_review(&session, &clear(), &[other], ReviewEvent::Approve, "");
    assert!(
        matches!(
            mixed,
            Err(ApiError::Model(ModelError::RevisionMismatch { .. }))
        ),
        "{mixed:?}"
    );
    for err in [blank, replied, mixed] {
        assert_eq!(
            err.err().map(|e| e.write_fate()),
            Some(WriteFate::NotAccepted)
        );
    }
    assert!(scripted.finish()?.is_empty());
    Ok(())
}

#[test]
fn pending_check_refuses_existing_pending_without_writing() -> TestResult {
    let scripted = Scripted::new()
        .expect(
            Method::Get,
            REVIEWS_PAGE_1,
            Reply::json(200, &json!([review(77, "bob", "PENDING", HEAD)])),
        )
        .expect(
            Method::Get,
            "/repos/o/r/pulls/7/reviews/77/comments?per_page=100&page=1",
            Reply::json(200, &json!([rest_comment(78, 77, 2, "из браузера")])),
        );
    let refused = adapter(&scripted)?.check_no_pending(&caps());
    let Err(ApiError::PendingExists(existing)) = &refused else {
        panic!("ожидался PendingExists: {refused:?}");
    };
    assert_eq!(existing.id(), &ProviderId::new("77"));
    assert_eq!(existing.comments.len(), 1);
    assert_eq!(existing.comments[0].body, "из браузера");
    assert_eq!(
        refused.err().map(|e| e.write_fate()),
        Some(WriteFate::NotAccepted)
    );
    let requests = scripted.finish()?;
    assert!(requests.iter().all(|request| !request.method.is_write()));
    Ok(())
}

#[test]
fn failed_pending_check_is_a_read_error_and_sends_no_write() -> TestResult {
    for reply in [
        Reply::timeout(),
        Reply::status(502),
        Reply::json(200, &json!({})),
    ] {
        let scripted = Scripted::new().expect(Method::Get, REVIEWS_PAGE_1, reply);
        let checked = adapter(&scripted)?.check_no_pending(&caps());
        assert!(
            matches!(
                checked,
                Err(ApiError::Transport(_) | ApiError::Status { .. } | ApiError::Unexpected(_))
            ),
            "{checked:?}"
        );
        let requests = scripted.finish()?;
        assert_eq!(requests.len(), 1);
        assert!(requests.iter().all(|request| !request.method.is_write()));
    }
    Ok(())
}

#[test]
fn local_branch_unexpected_answer_is_unknown() -> TestResult {
    let session = session_on(HEAD)?;
    let cases = [
        Reply::json(200, &review(503, "bob", "PENDING", HEAD)),
        Reply::json(200, &review(503, "bob", "COMMENTED", NEW_HEAD)),
        Reply::Respond(Response::new(200).with_body("<html>")),
        Reply::json(200, &json!({"state": "COMMENTED"})),
    ];
    for reply in cases {
        let scripted = Scripted::new().expect(Method::Post, REVIEWS, reply);
        let result =
            adapter(&scripted)?.create_review(&session, &clear(), &[], ReviewEvent::Comment, "x");
        assert!(matches!(result, Err(ApiError::Unexpected(_))), "{result:?}");
        assert_eq!(
            result.err().map(|e| e.write_fate()),
            Some(WriteFate::Unknown)
        );
        scripted.finish()?;
    }
    Ok(())
}

#[test]
fn server_pending_cycle_keeps_ids_and_submits_without_comments() -> TestResult {
    let session = session_on(HEAD)?;
    let drafts = drafts(&session)?;
    let scripted = Scripted::new()
        .expect(Method::Get, REVIEWS_PAGE_1, empty_page())
        .expect(
            Method::Post,
            REVIEWS,
            Reply::json(200, &review(601, "bob", "PENDING", HEAD)),
        )
        .expect(
            Method::Post,
            "/graphql",
            thread_reply("PRRT_1", "PRRC_1", 701, "added", "PRR_601"),
        )
        .expect(
            Method::Post,
            "/graphql",
            thread_reply("PRRT_2", "PRRC_2", 702, "range", "PRR_601"),
        )
        .expect(
            Method::Post,
            "/graphql",
            update_reply("PRRC_1", 701, "added v2", "PRR_601"),
        )
        .expect(
            Method::Post,
            "/repos/o/r/pulls/7/reviews/601/events",
            Reply::json(200, &review(601, "bob", "COMMENTED", HEAD)),
        );
    let github = adapter(&scripted)?;
    let clear = github.check_no_pending(&caps())?;
    let pending = github.start_pending(&session, &caps(), &clear)?;
    assert_eq!(pending.id(), &ProviderId::new("601"));
    assert_eq!(pending.node_id(), &ProviderId::new("PRR_601"));
    assert!(!pending.is_adopted() && pending.foreign().is_empty());
    let first = github.add_pending_comment(&session, &pending, &drafts[0])?;
    assert_eq!(first.id, ProviderId::new("PRRC_1"));
    assert_eq!(first.database_id, ProviderId::new("701"));
    assert_eq!(first.thread_id, Some(ProviderId::new("PRRT_1")));
    let range = github.add_pending_comment(&session, &pending, &drafts[2])?;
    assert_eq!(range.id, ProviderId::new("PRRC_2"));
    let updated = github.update_pending_comment(&pending, &first, "added v2")?;
    assert_eq!(updated.id, first.id);
    assert_eq!(updated.database_id, first.database_id);
    assert_eq!(updated.body, "added v2");
    let submitted = github.submit_pending(&pending, ReviewEvent::Comment, "Итог")?;
    assert_eq!(submitted.id, ProviderId::new("601"));
    assert_eq!(submitted.state, ReviewState::Commented);

    let requests = scripted.finish()?;
    let paths: Vec<(Method, &str)> = requests.iter().map(|r| (r.method, r.path())).collect();
    assert_eq!(
        paths,
        vec![
            (Method::Get, REVIEWS_PAGE_1),
            (Method::Post, REVIEWS),
            (Method::Post, "/graphql"),
            (Method::Post, "/graphql"),
            (Method::Post, "/graphql"),
            (Method::Post, "/repos/o/r/pulls/7/reviews/601/events"),
        ]
    );
    assert_eq!(
        bodies(&requests)[1..],
        [
            json!({"commit_id": HEAD}),
            json!({"query": ADD_THREAD, "variables": {"input": {
                "pullRequestReviewId": "PRR_601",
                "path": PATH, "line": 2, "side": "RIGHT", "body": "added",
            }}}),
            json!({"query": ADD_THREAD, "variables": {"input": {
                "pullRequestReviewId": "PRR_601",
                "path": PATH, "line": 3, "side": "RIGHT",
                "startLine": 1, "startSide": "RIGHT", "body": "range",
            }}}),
            json!({"query": UPDATE_COMMENT, "variables": {"input": {
                "pullRequestReviewCommentId": "PRRC_1", "body": "added v2",
            }}}),
            json!({"event": "COMMENT", "body": "Итог"}),
        ]
    );
    Ok(())
}

#[test]
fn pending_comment_answers_without_ids_or_with_other_ids_are_unknown() -> TestResult {
    let session = session_on(HEAD)?;
    let drafts = drafts(&session)?;
    let pending = PendingReview {
        id: ProviderId::new("601"),
        node_id: ProviderId::new("PRR_601"),
        commit_id: Sha::parse(HEAD)?,
        foreign: Vec::new(),
        adopted: false,
    };
    let adds = [
        Reply::json(
            200,
            &json!({"data": {"addPullRequestReviewThread": {"thread": null}}}),
        ),
        Reply::json(
            200,
            &json!({"data": {"addPullRequestReviewThread": {"thread": {"id": "PRRT_1", "comments": {"nodes": []}}}}}),
        ),
        thread_reply("PRRT_1", "PRRC_1", 701, "added", "PRR_999"),
    ];
    for reply in adds {
        let scripted = Scripted::new().expect(Method::Post, "/graphql", reply);
        let result = adapter(&scripted)?.add_pending_comment(&session, &pending, &drafts[0]);
        assert!(matches!(result, Err(ApiError::Unexpected(_))), "{result:?}");
        assert_eq!(
            result.err().map(|e| e.write_fate()),
            Some(WriteFate::Unknown)
        );
        scripted.finish()?;
    }
    let comment = PendingComment {
        id: ProviderId::new("PRRC_1"),
        database_id: ProviderId::new("701"),
        thread_id: Some(ProviderId::new("PRRT_1")),
        body: "added".to_owned(),
        review: ProviderId::new("PRR_601"),
    };
    let updates = [
        update_reply("PRRC_9", 709, "v2", "PRR_601"),
        update_reply("PRRC_1", 709, "v2", "PRR_601"),
        update_reply("PRRC_1", 701, "v2", "PRR_999"),
    ];
    for reply in updates {
        let scripted = Scripted::new().expect(Method::Post, "/graphql", reply);
        let result = adapter(&scripted)?.update_pending_comment(&pending, &comment, "v2");
        assert!(matches!(result, Err(ApiError::Unexpected(_))), "{result:?}");
        assert_eq!(
            result.err().map(|e| e.write_fate()),
            Some(WriteFate::Unknown)
        );
        scripted.finish()?;
    }
    Ok(())
}

#[test]
fn existing_pending_is_read_across_pages_refused_and_adopted_explicitly() -> TestResult {
    let session = session_on(HEAD)?;
    let drafts = drafts(&session)?;
    let comments = "/repos/o/r/pulls/7/reviews/77/comments";
    let scripted = Scripted::new()
        .expect(
            Method::Get,
            REVIEWS_PAGE_1,
            page_with_next(json!([review(10, "alice", "COMMENTED", HEAD)]), REVIEWS),
        )
        .expect(
            Method::Get,
            REVIEWS_PAGE_2,
            Reply::json(200, &json!([review(77, "Bob", "PENDING", HEAD)])),
        )
        .expect(
            Method::Get,
            format!("{comments}?per_page=100&page=1"),
            page_with_next(json!([rest_comment(78, 77, 2, "первый")]), comments),
        )
        .expect(
            Method::Get,
            format!("{comments}?per_page=100&page=2"),
            Reply::json(200, &json!([rest_comment(79, 77, 3, "второй")])),
        )
        .expect(
            Method::Post,
            "/graphql",
            thread_reply("PRRT_5", "PRRC_5", 705, "added", "PRR_77"),
        )
        .expect(
            Method::Post,
            "/repos/o/r/pulls/7/reviews/77/events",
            Reply::json(200, &review(77, "bob", "APPROVED", HEAD)),
        );
    let github = adapter(&scripted)?;
    let refused = github.check_no_pending(&caps());
    let Err(ApiError::PendingExists(existing)) = refused else {
        panic!("ожидался PendingExists: {refused:?}");
    };
    assert_eq!(existing.node_id(), &ProviderId::new("PRR_77"));
    assert_eq!(existing.commit_id().as_str(), HEAD);
    let texts: Vec<&str> = existing.comments.iter().map(|c| c.body.as_str()).collect();
    assert_eq!(texts, ["первый", "второй"]);
    assert!(scripted.requests().iter().all(|r| !r.method.is_write()));

    let pending = existing.adopt();
    assert!(pending.is_adopted());
    assert_eq!(
        pending.foreign(),
        [ProviderId::new("78"), ProviderId::new("79")]
    );
    github.add_pending_comment(&session, &pending, &drafts[0])?;
    let submitted = github.submit_pending(&pending, ReviewEvent::Approve, "")?;
    assert_eq!(submitted.state, ReviewState::Approved);

    let requests = scripted.finish()?;
    assert!(requests.iter().all(|r| r.url.starts_with(API)));
    let writes: Vec<&Request> = requests.iter().filter(|r| r.method.is_write()).collect();
    assert_eq!(writes.len(), 2);
    assert!(writes.iter().all(|r| r.method == Method::Post));
    for write in &writes {
        let body = write.body_json().unwrap_or(Value::Null).to_string();
        assert!(
            !body.contains("PRRC_78") && !body.contains("PRRC_79"),
            "{body}"
        );
    }
    assert_eq!(writes[1].body_json(), Some(json!({"event": "APPROVE"})));
    Ok(())
}

#[test]
fn adopted_pending_on_another_commit_takes_no_comment() -> TestResult {
    let session = session_on(NEW_HEAD)?;
    let drafts = drafts(&session)?;
    let scripted = Scripted::new()
        .expect(
            Method::Get,
            REVIEWS_PAGE_1,
            Reply::json(200, &json!([review(77, "bob", "PENDING", HEAD)])),
        )
        .expect(
            Method::Get,
            "/repos/o/r/pulls/7/reviews/77/comments?per_page=100&page=1",
            empty_page(),
        );
    let github = adapter(&scripted)?;
    let Some(existing) = github.find_pending("bob")? else {
        panic!("PENDING не найден");
    };
    let pending = existing.adopt();
    let refused = github.add_pending_comment(&session, &pending, &drafts[0]);
    assert!(matches!(refused, Err(ApiError::Invalid(_))), "{refused:?}");
    let other = PendingReview {
        node_id: ProviderId::new("PRR_88"),
        ..pending.clone()
    };
    let comment = PendingComment {
        id: ProviderId::new("PRRC_1"),
        database_id: ProviderId::new("701"),
        thread_id: Some(ProviderId::new("PRRT_1")),
        body: "x".to_owned(),
        review: ProviderId::new("PRR_77"),
    };
    let foreign = github.update_pending_comment(&other, &comment, "y");
    assert!(matches!(foreign, Err(ApiError::Invalid(_))), "{foreign:?}");
    assert_eq!(scripted.finish()?.len(), 2);
    Ok(())
}

#[test]
fn two_pending_reviews_of_one_account_are_unexpected() -> TestResult {
    let scripted = Scripted::new().expect(
        Method::Get,
        REVIEWS_PAGE_1,
        Reply::json(
            200,
            &json!([
                review(77, "bob", "PENDING", HEAD),
                review(78, "bob", "PENDING", HEAD),
            ]),
        ),
    );
    let result = adapter(&scripted)?.find_pending("bob");
    assert!(matches!(result, Err(ApiError::Unexpected(_))), "{result:?}");
    scripted.finish()?;
    Ok(())
}

#[test]
fn viewed_mark_unmark_and_read_by_cursors() -> TestResult {
    let pull = ProviderId::new("PR_7");
    let files_page = |nodes: Value, next: bool, cursor: Value| {
        Reply::json(
            200,
            &json!({"data": {"node": {"files": {
                "nodes": nodes,
                "pageInfo": {"hasNextPage": next, "endCursor": cursor},
            }}}}),
        )
    };
    let scripted = Scripted::new()
        .expect(
            Method::Post,
            "/graphql",
            Reply::json(
                200,
                &json!({"data": {"markFileAsViewed": {"pullRequest": {"id": "PR_7"}}}}),
            ),
        )
        .expect(
            Method::Post,
            "/graphql",
            Reply::json(
                200,
                &json!({"data": {"unmarkFileAsViewed": {"pullRequest": {"id": "PR_7"}}}}),
            ),
        )
        .expect(
            Method::Post,
            "/graphql",
            files_page(
                json!([{"path": PATH, "viewerViewedState": "VIEWED"}]),
                true,
                json!("c1"),
            ),
        )
        .expect(
            Method::Post,
            "/graphql",
            files_page(
                json!([{"path": "src/b.rs", "viewerViewedState": "DISMISSED"}]),
                false,
                Value::Null,
            ),
        );
    let github = adapter(&scripted)?;
    github.set_viewed(&caps(), &pull, PATH, true)?;
    github.set_viewed(&caps(), &pull, PATH, false)?;
    let files = github.viewed_files(&pull)?;
    assert_eq!(
        files,
        vec![
            (PATH.to_owned(), ViewedState::Viewed),
            ("src/b.rs".to_owned(), ViewedState::Dismissed),
        ]
    );
    let requests = scripted.finish()?;
    assert_eq!(
        bodies(&requests),
        vec![
            json!({"query": MARK_VIEWED, "variables": {"input": {"pullRequestId": "PR_7", "path": PATH}}}),
            json!({"query": UNMARK_VIEWED, "variables": {"input": {"pullRequestId": "PR_7", "path": PATH}}}),
            json!({"query": VIEWED_FILES, "variables": {"id": "PR_7", "cursor": null}}),
            json!({"query": VIEWED_FILES, "variables": {"id": "PR_7", "cursor": "c1"}}),
        ]
    );
    Ok(())
}

fn submit_with(reply: Reply) -> Result<ApiError, Box<dyn std::error::Error>> {
    let pending = PendingReview {
        id: ProviderId::new("601"),
        node_id: ProviderId::new("PRR_601"),
        commit_id: Sha::parse(HEAD)?,
        foreign: Vec::new(),
        adopted: false,
    };
    let scripted =
        Scripted::new().expect(Method::Post, "/repos/o/r/pulls/7/reviews/601/events", reply);
    let result = adapter(&scripted)?.submit_pending(&pending, ReviewEvent::Comment, "x");
    scripted.finish()?;
    result.err().ok_or_else(|| "ожидалась ошибка".into())
}

#[test]
fn rest_answers_classify_the_write_fate() -> TestResult {
    use crate::transport::FailureKind;
    let limited = |status: u16, headers: &[(&str, &str)], message: &str| {
        let mut response = Response::json(status, &json!({"message": message}));
        for (name, value) in headers {
            response = response.with_header(name, *value);
        }
        Reply::Respond(response)
    };
    let cases: Vec<(Reply, WriteFate, &str)> = vec![
        (Reply::status(502), WriteFate::Unknown, "status"),
        (Reply::timeout(), WriteFate::Unknown, "transport"),
        (Reply::disconnect(), WriteFate::Unknown, "transport"),
        (Reply::not_sent(), WriteFate::NotAccepted, "not_sent"),
        (
            Reply::json(
                422,
                &json!({"message": "Unprocessable Entity", "errors": ["body can't be blank"]}),
            ),
            WriteFate::NotAccepted,
            "status",
        ),
        (
            limited(
                403,
                &[("x-ratelimit-remaining", "0")],
                "API rate limit exceeded",
            ),
            WriteFate::NotAccepted,
            "rate",
        ),
        (
            limited(429, &[], "Too Many Requests"),
            WriteFate::NotAccepted,
            "rate",
        ),
        (
            limited(403, &[], "You have exceeded a secondary rate limit."),
            WriteFate::NotAccepted,
            "rate",
        ),
        (
            limited(403, &[("retry-after", "60")], "Forbidden"),
            WriteFate::NotAccepted,
            "rate",
        ),
        (
            limited(
                403,
                &[("x-ratelimit-remaining", "41")],
                "Must be the review author",
            ),
            WriteFate::NotAccepted,
            "status",
        ),
        (
            Reply::json(200, &review(602, "bob", "COMMENTED", HEAD)),
            WriteFate::Unknown,
            "unexpected",
        ),
        (
            Reply::Respond(Response::new(200).with_body("not json")),
            WriteFate::Unknown,
            "unexpected",
        ),
    ];
    for (reply, fate, kind) in cases {
        let err = submit_with(reply)?;
        let observed = match &err {
            ApiError::Status { .. } => "status",
            ApiError::Transport(failure) => {
                assert_ne!(failure.kind, FailureKind::NotSent);
                "transport"
            }
            ApiError::NotSent(_) => "not_sent",
            ApiError::RateLimited { .. } => "rate",
            ApiError::Unexpected(_) => "unexpected",
            other => panic!("неожиданный вид ошибки {other:?}"),
        };
        assert_eq!((observed, err.write_fate()), (kind, fate), "{err}");
    }
    let err = submit_with(Reply::json(
        422,
        &json!({"message": "Validation Failed", "errors": [{"message": "Line could not be resolved"}]}),
    ))?;
    assert_eq!(
        err,
        ApiError::Status {
            status: 422,
            message: "Validation Failed".to_owned(),
            errors: vec!["Line could not be resolved".to_owned()],
        }
    );
    Ok(())
}

#[test]
fn graphql_errors_at_http_200_classify_the_write_fate() -> TestResult {
    let pending = PendingReview {
        id: ProviderId::new("601"),
        node_id: ProviderId::new("PRR_601"),
        commit_id: Sha::parse(HEAD)?,
        foreign: Vec::new(),
        adopted: false,
    };
    let comment = PendingComment {
        id: ProviderId::new("PRRC_1"),
        database_id: ProviderId::new("701"),
        thread_id: Some(ProviderId::new("PRRT_1")),
        body: "x".to_owned(),
        review: ProviderId::new("PRR_601"),
    };
    let errors =
        |errors: Value| json!({"data": {"updatePullRequestReviewComment": null}, "errors": errors});
    let cases: Vec<(Reply, WriteFate, &str)> = vec![
        (
            Reply::json(
                200,
                &errors(json!([{"type": "NOT_FOUND", "message": "Could not resolve"}])),
            ),
            WriteFate::NotAccepted,
            "graphql",
        ),
        (
            Reply::json(
                200,
                &errors(
                    json!([{"message": "Field 'x' doesn't exist", "extensions": {"code": "undefinedField"}}]),
                ),
            ),
            WriteFate::NotAccepted,
            "graphql",
        ),
        (
            Reply::json(
                200,
                &errors(json!([{"message": "Something went wrong while executing your query"}])),
            ),
            WriteFate::Unknown,
            "graphql",
        ),
        (
            Reply::Respond(
                Response::json(
                    200,
                    &errors(json!([{"message": "API rate limit exceeded"}])),
                )
                .with_header("x-ratelimit-remaining", "0"),
            ),
            WriteFate::NotAccepted,
            "rate",
        ),
        (
            Reply::Respond(
                Response::json(
                    200,
                    &errors(
                        json!([{"message": "Something went wrong while executing your query"}]),
                    ),
                )
                .with_header("x-ratelimit-remaining", "0"),
            ),
            WriteFate::Unknown,
            "graphql",
        ),
        (
            Reply::json(
                200,
                &errors(json!([{"type": "RATE_LIMITED", "message": "limit"}])),
            ),
            WriteFate::NotAccepted,
            "rate",
        ),
        (
            Reply::json(
                200,
                &json!({"data": {"updatePullRequestReviewComment": null}}),
            ),
            WriteFate::Unknown,
            "unexpected",
        ),
    ];
    for (reply, fate, kind) in cases {
        let scripted = Scripted::new().expect(Method::Post, "/graphql", reply);
        let result = adapter(&scripted)?.update_pending_comment(&pending, &comment, "y");
        scripted.finish()?;
        let Err(err) = result else {
            panic!("ожидалась ошибка");
        };
        let observed = match &err {
            ApiError::Graphql(_) => "graphql",
            ApiError::RateLimited { .. } => "rate",
            ApiError::Unexpected(_) => "unexpected",
            other => panic!("неожиданный вид ошибки {other:?}"),
        };
        assert_eq!((observed, err.write_fate()), (kind, fate), "{err}");
    }
    Ok(())
}

#[test]
fn submit_of_another_review_or_state_is_unknown() -> TestResult {
    for reply in [
        Reply::json(200, &review(602, "bob", "COMMENTED", HEAD)),
        Reply::json(200, &review(601, "bob", "PENDING", HEAD)),
    ] {
        let err = submit_with(reply)?;
        assert!(matches!(err, ApiError::Unexpected(_)), "{err:?}");
        assert_eq!(err.write_fate(), WriteFate::Unknown);
    }
    Ok(())
}

fn pull_json(head: &str) -> Value {
    json!({
        "number": 7,
        "node_id": "PR_7",
        "user": {"login": "alice"},
        "head": {"sha": head},
        "base": {"sha": BASE},
    })
}

#[test]
fn pull_snapshot_reads_revision_around_the_files() -> TestResult {
    let files = json!([
        {"filename": PATH, "status": "modified", "patch": PATCH},
        {"filename": "logo.png", "status": "added"},
        {"filename": "src/new.rs", "previous_filename": "src/old.rs", "status": "renamed",
         "patch": "@@ -1 +1 @@\n-a\n+b\n"},
    ]);
    let scripted = Scripted::new()
        .expect(Method::Get, PULL, Reply::json(200, &pull_json(HEAD)))
        .expect(
            Method::Get,
            "/repos/o/r/pulls/7/files?per_page=100&page=1",
            Reply::json(200, &files),
        )
        .expect(Method::Get, PULL, Reply::json(200, &pull_json(HEAD)));
    let snapshot = adapter(&scripted)?.pull()?;
    assert_eq!(snapshot.node_id, ProviderId::new("PR_7"));
    assert_eq!(snapshot.author, "alice");
    assert_eq!(snapshot.revision, revision(HEAD)?);
    let paths: Vec<(&str, &str)> = snapshot
        .files
        .iter()
        .map(|file| (file.old_path(), file.new_path()))
        .collect();
    assert_eq!(paths, [(PATH, PATH), ("src/old.rs", "src/new.rs")]);
    assert_eq!(snapshot.files[0], file()?);
    scripted.finish()?;

    let moved = Scripted::new()
        .expect(Method::Get, PULL, Reply::json(200, &pull_json(HEAD)))
        .expect(
            Method::Get,
            "/repos/o/r/pulls/7/files?per_page=100&page=1",
            Reply::json(200, &files),
        )
        .expect(Method::Get, PULL, Reply::json(200, &pull_json(NEW_HEAD)));
    let result = adapter(&moved)?.pull();
    assert_eq!(
        result,
        Err(ApiError::HeadMoved {
            before: Box::new(revision(HEAD)?),
            after: Box::new(revision(NEW_HEAD)?),
        })
    );
    moved.finish()?;
    Ok(())
}

#[test]
fn review_by_numeric_id_restores_the_node_id() -> TestResult {
    let scripted = Scripted::new().expect(
        Method::Get,
        "/repos/o/r/pulls/7/reviews/601",
        Reply::json(200, &review(601, "bob", "PENDING", HEAD)),
    );
    let found = adapter(&scripted)?.review(&ProviderId::new("601"))?;
    assert_eq!(found.node_id, ProviderId::new("PRR_601"));
    assert_eq!(found.state, ReviewState::Pending);
    scripted.finish()?;
    Ok(())
}

#[test]
fn reply_and_resolve_are_standalone_calls() -> TestResult {
    let mut answer = rest_comment(720, 610, 2, "ответ");
    answer["in_reply_to_id"] = json!(701);
    let scripted = Scripted::new()
        .expect(
            Method::Post,
            "/repos/o/r/pulls/7/comments/701/replies",
            Reply::json(201, &answer),
        )
        .expect(
            Method::Post,
            "/graphql",
            Reply::json(
                200,
                &json!({"data": {"resolveReviewThread": {"thread": {"id": "PRRT_1", "isResolved": true}}}}),
            ),
        )
        .expect(
            Method::Post,
            "/graphql",
            Reply::json(
                200,
                &json!({"data": {"unresolveReviewThread": {"thread": {"id": "PRRT_1", "isResolved": true}}}}),
            ),
        );
    let github = adapter(&scripted)?;
    let reply = github.reply(&ProviderId::new("701"), "ответ")?;
    assert_eq!(reply.in_reply_to, Some(ProviderId::new("701")));
    github.set_thread_resolved(&ProviderId::new("PRRT_1"), true)?;
    let stale = github.set_thread_resolved(&ProviderId::new("PRRT_1"), false);
    assert!(matches!(stale, Err(ApiError::Unexpected(_))), "{stale:?}");
    let requests = scripted.finish()?;
    assert_eq!(
        bodies(&requests),
        vec![
            json!({"body": "ответ"}),
            json!({"query": RESOLVE_THREAD, "variables": {"input": {"threadId": "PRRT_1"}}}),
            json!({"query": UNRESOLVE_THREAD, "variables": {"input": {"threadId": "PRRT_1"}}}),
        ]
    );
    Ok(())
}

#[test]
fn adapter_needs_a_github_target_with_owner_and_name() {
    let scripted = Scripted::new();
    for (provider, repo) in [
        (Provider::GitLab, "o/r"),
        (Provider::GitHub, "r"),
        (Provider::GitHub, "o/r/x"),
        (Provider::GitHub, "/r"),
    ] {
        let target = ReviewTarget {
            provider,
            host: "github.com".to_owned(),
            repo: repo.to_owned(),
            number: 7,
        };
        let result = GitHub::new(&scripted, Endpoints::cloud(), &target);
        assert!(matches!(result, Err(ApiError::Invalid(_))), "{repo}");
    }
}

// Сквозная проверка против `forge-fake` по loopback: адаптер говорит на языке оракула.

fn fake_server() -> Result<FakeServer<GithubFake>, String> {
    let mut github = GithubFake::new("o", "r");
    github.add_user("alice", "tok-author", Permission::Write);
    github.add_user("bob", "tok-reviewer", Permission::Write);
    github.add_pull(
        7,
        "alice",
        BASE,
        FakeCommit {
            sha: HEAD.to_owned(),
            files: vec![
                FakeFile::modified(PATH, PATCH),
                FakeFile::modified("src/b.rs", "@@ -1 +1 @@\n-x\n+y\n"),
            ],
        },
    );
    FakeServer::start(github)
}

fn http_adapter(
    server: &FakeServer<GithubFake>,
    token: &str,
) -> Result<GitHub<HttpTransport>, ApiError> {
    GitHub::new(
        HttpTransport::new(
            HttpConfig::default(),
            Some(Credential::bearer(Secret::new(token))),
        ),
        Endpoints::single(server.base()),
        &target(),
    )
}

#[test]
fn adapter_drives_forge_fake_through_both_branches() -> TestResult {
    let server = fake_server()?;
    let github = http_adapter(&server, "tok-reviewer")?;
    let caps = github.capabilities()?;
    assert_eq!(caps.login, "bob");
    assert!(caps.server_pending() && caps.viewed() && caps.resolve());
    assert_eq!(caps.api.supported, Some(vec![API_VERSION.to_owned()]));
    let snapshot = github.pull()?;
    assert_eq!(snapshot.revision, revision(HEAD)?);
    let session = ReviewSession::new("bob", target(), snapshot.revision.clone());
    let drafts = drafts(&session)?;

    let clear = github.check_no_pending(&caps)?;
    let local = github.create_review(&session, &clear, &drafts, ReviewEvent::Comment, "Итог")?;
    assert_eq!(local.state, ReviewState::Commented);

    let (planted, planted_comments) = server.with(|fake| {
        fake.plant_pending(
            7,
            "bob",
            "из браузера",
            &[CommentSpec::line(PATH, DiffSide::Right, 3, "чужой")],
        )
    })?;
    let refused = github.check_no_pending(&caps);
    let Err(ApiError::PendingExists(existing)) = refused else {
        panic!("ожидался PendingExists: {refused:?}");
    };
    assert_eq!(existing.id(), &ProviderId::new(planted.to_string()));
    assert_eq!(existing.comments.len(), 1);
    let adopted = existing.adopt();
    let ours = github.add_pending_comment(&session, &adopted, &drafts[0])?;
    let ours = github.update_pending_comment(&adopted, &ours, "наш v2")?;
    let submitted = github.submit_pending(&adopted, ReviewEvent::Approve, "")?;
    assert_eq!(submitted.state, ReviewState::Approved);

    let clear = github.check_no_pending(&caps)?;
    let pending = github.start_pending(&session, &caps, &clear)?;
    github.add_pending_comment(&session, &pending, &drafts[2])?;
    github.submit_pending(&pending, ReviewEvent::RequestChanges, "Правки")?;

    github.set_viewed(&caps, &snapshot.node_id, PATH, true)?;
    let viewed = github.viewed_files(&snapshot.node_id)?;
    assert_eq!(viewed[0], (PATH.to_owned(), ViewedState::Viewed));
    github.set_viewed(&caps, &snapshot.node_id, PATH, false)?;
    assert_eq!(
        github.viewed_files(&snapshot.node_id)?[0],
        (PATH.to_owned(), ViewedState::Unviewed)
    );

    let reviews = server.with(|fake| fake.reviews(7));
    let states: Vec<FakeState> = reviews.iter().map(|review| review.state).collect();
    assert_eq!(
        states,
        [
            FakeState::Commented,
            FakeState::Approved,
            FakeState::ChangesRequested
        ]
    );
    let comments = server.with(|fake| fake.comments(7));
    assert_eq!(comments.len(), 3 + 2 + 1);
    let planted_comment = comments
        .iter()
        .find(|comment| comment.id == planted_comments[0])
        .ok_or("подложенный комментарий пропал")?;
    assert_eq!(
        (planted_comment.body.as_str(), planted_comment.review_id),
        ("чужой", planted)
    );
    let edited = comments
        .iter()
        .find(|comment| comment.node_id == ours.id.as_str())
        .ok_or("наш комментарий пропал")?;
    assert_eq!(edited.body, "наш v2");
    assert!(
        server
            .with(|fake| fake.viewed(7, "bob"))
            .get(PATH)
            .is_none_or(|state| *state != Viewed::Viewed)
    );
    assert_eq!(
        server.count("POST /repos/{owner}/{repo}/pulls/{number}/reviews"),
        2
    );
    assert_eq!(
        server.count("POST /repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/events"),
        2
    );
    server.finish()?;
    Ok(())
}

#[test]
fn absent_mutation_on_forge_fake_gates_the_pending_branch() -> TestResult {
    let server = fake_server()?;
    server.with(|fake| fake.remove_mutation("updatePullRequestReviewComment"));
    let github = http_adapter(&server, "tok-reviewer")?;
    let caps = github.capabilities()?;
    assert!(!caps.server_pending());
    assert!(caps.viewed());
    let session = session_on(HEAD)?;
    let absent = github.start_pending(&session, &caps, &clear());
    assert!(matches!(absent, Err(ApiError::Absent(_))), "{absent:?}");
    assert_eq!(
        server.count("POST /repos/{owner}/{repo}/pulls/{number}/reviews"),
        0
    );
    Ok(())
}

#[test]
fn own_pending_is_resumed_from_recorded_ids_without_becoming_foreign() -> TestResult {
    let server = fake_server()?;
    let github = http_adapter(&server, "tok-reviewer")?;
    let caps = github.capabilities()?;
    let session = ReviewSession::new("bob", target(), github.pull()?.revision);
    let drafts = drafts(&session)?;
    let clear = github.check_no_pending(&caps)?;
    let pending = github.start_pending(&session, &caps, &clear)?;
    let first = github.add_pending_comment(&session, &pending, &drafts[0])?;
    let recorded_review = pending.id().clone();
    let recorded_comment = first.id.clone();
    drop(pending);

    let missing = github.resume_pending(
        &session,
        &recorded_review,
        &[ProviderId::new("PRRC_нет")],
        false,
    );
    assert!(
        matches!(missing, Err(ApiError::Unexpected(_))),
        "{missing:?}"
    );
    let (resumed, own) = github.resume_pending(
        &session,
        &recorded_review,
        std::slice::from_ref(&recorded_comment),
        false,
    )?;
    assert!(!resumed.is_adopted());
    assert!(resumed.foreign().is_empty(), "свой комментарий не чужой");
    assert_eq!(own.len(), 1);
    assert_eq!(own[0].id, recorded_comment);
    assert_eq!(own[0].database_id, first.database_id);
    assert_eq!(own[0].thread_id, None);
    let (unlisted, _) = github.resume_pending(&session, &recorded_review, &[], false)?;
    assert_eq!(unlisted.foreign(), std::slice::from_ref(&first.database_id));

    let edited = github.update_pending_comment(&resumed, &own[0], "после переоткрытия")?;
    assert_eq!(edited.id, recorded_comment);
    github.add_pending_comment(&session, &resumed, &drafts[1])?;
    github.submit_pending(&resumed, ReviewEvent::Comment, "Итог")?;

    let submitted = github.resume_pending(&session, &recorded_review, &[], false);
    assert!(
        matches!(submitted, Err(ApiError::Unexpected(_))),
        "{submitted:?}"
    );
    let other = ReviewSession::new("alice", target(), session.revision().clone());
    let foreign_account = github.resume_pending(&other, &recorded_review, &[], false);
    assert!(foreign_account.is_err(), "{foreign_account:?}");
    let comments = server.with(|fake| fake.comments(7));
    assert_eq!(comments.len(), 2);
    assert!(
        comments
            .iter()
            .any(|comment| comment.body == "после переоткрытия")
    );
    server.finish()?;
    Ok(())
}
