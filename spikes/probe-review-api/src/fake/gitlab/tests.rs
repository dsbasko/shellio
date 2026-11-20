use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::fake::{FakeServer, Fault, Reply};
use crate::transport::http::{Credential, HttpConfig, HttpTransport, Secret};
use crate::transport::{Request, Transport};

const AUTHOR: &str = "alice";
const REVIEWER: &str = "bob";
const DIFF: &str = "@@ -1,4 +1,4 @@\n ctx1\n-old2\n+new2\n ctx3\n ctx4\n@@ -10,2 +10,3 @@\n ctx10\n+add11\n ctx11\n";
const IID: u64 = 3;
const MR: &str = "/api/v4/projects/group%2Fproj/merge_requests/3";
const RECEIPT: Duration = Duration::from_secs(10);
const RC: &str = "mutation($input: MergeRequestRequestChangesInput!) { \
    mergeRequestRequestChanges(input: $input) { errors mergeRequest { iid } } }";
const STATE: &str = "query($fullPath: ID!, $iid: String!) { project(fullPath: $fullPath) { \
    mergeRequest(iid: $iid) { reviewers { nodes { username mergeRequestInteraction { reviewState } } } } } }";

fn fake() -> FakeServer<GitlabFake> {
    let mut gitlab = GitlabFake::new("group/proj");
    gitlab.add_user(AUTHOR, "tok-author", Some(Access::Developer));
    gitlab.add_user(REVIEWER, "tok-reviewer", Some(Access::Developer));
    gitlab.add_user("rita", "tok-reporter", Some(Access::Reporter));
    gitlab.add_user("olga", "tok-outsider", None);
    gitlab.add_mr(
        IID,
        AUTHOR,
        "base0",
        "head1",
        vec![
            FakeDiff::modified("src/a.rs", DIFF),
            FakeDiff::renamed("old/b.rs", "src/b.rs", "@@ -1 +1 @@\n-x\n+y\n"),
        ],
    );
    gitlab.add_reviewer(IID, REVIEWER).expect("ревьюер");
    FakeServer::start(gitlab).expect("сервер")
}

struct Client {
    base: String,
    http: HttpTransport,
}

impl Client {
    fn new(server: &FakeServer<GitlabFake>, token: &str) -> Client {
        Client::with(server, Credential::private_token(Secret::new(token)), 5000)
    }

    fn with(server: &FakeServer<GitlabFake>, credential: Credential, millis: u64) -> Client {
        Client {
            base: server.base().to_owned(),
            http: HttpTransport::new(
                HttpConfig {
                    connect_timeout: Duration::from_secs(5),
                    call_timeout: Duration::from_millis(millis),
                },
                Some(credential),
            ),
        }
    }

    fn send(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value, Response) {
        let mut request = Request::new(method, format!("{}{path}", self.base));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = self.http.send(&request).expect("ответ");
        let value = if response.body.is_empty() {
            Value::Null
        } else {
            response
                .body_json()
                .unwrap_or_else(|_| json!(String::from_utf8_lossy(&response.body)))
        };
        (response.status, value, response)
    }

    fn get(&self, path: &str) -> (u16, Value) {
        let (status, value, _) = self.send(Method::Get, path, None);
        (status, value)
    }

    fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let (status, value, _) = self.send(Method::Post, path, Some(body));
        (status, value)
    }

    fn put(&self, path: &str, body: Value) -> (u16, Value) {
        let (status, value, _) = self.send(Method::Put, path, Some(body));
        (status, value)
    }

    fn graphql(&self, query: &str, variables: Value) -> Value {
        let (status, value) = self.post(
            "/api/graphql",
            json!({"query": query, "variables": variables}),
        );
        assert_eq!(status, 200, "{value}");
        value
    }
}

fn position(base: &str, start: &str, head: &str, old: Option<u32>, new: Option<u32>) -> Value {
    json!({
        "position_type": "text",
        "base_sha": base,
        "start_sha": start,
        "head_sha": head,
        "old_path": "src/a.rs",
        "new_path": "src/a.rs",
        "old_line": old,
        "new_line": new,
    })
}

fn added2() -> Value {
    position("base0", "base0", "head1", None, Some(2))
}

fn code(old: u32, new: u32) -> String {
    format!("{}_{old}_{new}", sha1_smol::Sha1::from("src/a.rs").digest())
}

fn rc_input() -> Value {
    json!({"input": {"projectPath": "group/proj", "iid": "3"}})
}

fn review_state(client: &Client) -> Value {
    let state = client.graphql(STATE, json!({"fullPath": "group/proj", "iid": "3"}));
    state["data"]["project"]["mergeRequest"]["reviewers"]["nodes"][0]["mergeRequestInteraction"]
        ["reviewState"]
        .clone()
}

#[test]
fn reads_metadata_project_mr_versions_diffs_and_reviewers() {
    let server = fake();
    let author = Client::new(&server, "tok-author");
    let (_, metadata) = author.get("/api/v4/metadata");
    assert_eq!(metadata["version"], "18.5.1");
    assert_eq!(metadata["enterprise"], false);
    assert_eq!(author.get("/api/v4/version").1["version"], "18.5.1");
    assert_eq!(author.get("/api/v4/user").1["username"], AUTHOR);
    let (status, project) = author.get("/api/v4/projects/group%2Fproj");
    assert_eq!(status, 200);
    assert_eq!(project["visibility"], "private");
    assert_eq!(project["permissions"]["project_access"]["access_level"], 30);
    assert_eq!(author.get("/api/v4/projects/42").0, 200);
    assert_eq!(author.get("/api/v4/projects/group%2Fother").0, 404);

    let (_, mr) = author.get(MR);
    assert_eq!(
        mr["diff_refs"],
        json!({"base_sha": "base0", "head_sha": "head1", "start_sha": "base0"})
    );
    assert_eq!(mr["reviewers"][0]["username"], REVIEWER);
    server
        .with(|gitlab| gitlab.push_head(IID, "head2", vec![FakeDiff::modified("src/a.rs", DIFF)]))
        .expect("смена head");
    let (_, versions) = author.get(&format!("{MR}/versions"));
    assert_eq!(versions[0]["head_commit_sha"], "head2");
    assert_eq!(versions[1]["head_commit_sha"], "head1");
    let first = versions[1]["id"].as_u64().expect("id");
    let (_, version) = author.get(&format!("{MR}/versions/{first}"));
    assert_eq!(version["diffs"][1]["old_path"], "old/b.rs");
    assert_eq!(version["diffs"][1]["renamed_file"], true);
    let (_, diffs) = author.get(&format!("{MR}/diffs"));
    assert_eq!(diffs.as_array().map(Vec::len), Some(1));
    assert_eq!(diffs[0]["diff"], DIFF);
    let (_, reviewers) = author.get(&format!("{MR}/reviewers"));
    assert_eq!(reviewers[0]["user"]["username"], REVIEWER);
    assert_eq!(reviewers[0]["state"], "unreviewed");

    let bearer = Client::with(
        &server,
        Credential::bearer(Secret::new("tok-reviewer")),
        5000,
    );
    assert_eq!(bearer.get("/api/v4/user").1["username"], REVIEWER);
    assert_eq!(Client::new(&server, "tok-outsider").get(MR).0, 404);
    assert_eq!(Client::new(&server, "tok-unknown").get(MR).0, 401);
    assert_eq!(author.get("/api/v4/projects/42/merge_requests/9").0, 404);
}

#[test]
fn drafts_are_per_user_and_published_one_by_one_without_ids() {
    let server = fake();
    let planted = server
        .with(|gitlab| gitlab.plant_draft(IID, REVIEWER, "из браузера", None))
        .expect("подложен");
    assert!(server.log().is_empty(), "подложенное не идёт через HTTP");
    let reviewer = Client::new(&server, "tok-reviewer");
    let (status, draft) = reviewer.post(
        &format!("{MR}/draft_notes"),
        json!({"note": "строка", "position": added2()}),
    );
    assert_eq!(status, 201, "{draft}");
    assert_eq!(draft["position"]["new_line"], 2);
    assert_eq!(draft["position"]["old_line"], Value::Null);
    assert_eq!(draft["line_code"], code(3, 2));
    let id = draft["id"].as_u64().expect("id");
    let (status, plain) = reviewer.post(&format!("{MR}/draft_notes"), json!({"note": "общий"}));
    assert_eq!(status, 201);
    assert_eq!(plain["position"]["position_type"], "text");
    assert_eq!(plain["line_code"], Value::Null);

    let (_, mine) = reviewer.get(&format!("{MR}/draft_notes"));
    assert_eq!(
        mine.as_array().map(Vec::len),
        Some(3),
        "черновики того же пользователя"
    );
    let author = Client::new(&server, "tok-author");
    assert_eq!(author.get(&format!("{MR}/draft_notes")).1, json!([]));
    assert_eq!(author.get(&format!("{MR}/draft_notes/{id}")).0, 404);
    assert_eq!(
        author
            .put(&format!("{MR}/draft_notes/{id}/publish"), json!({}))
            .0,
        404
    );

    let (status, updated) = reviewer.put(
        &format!("{MR}/draft_notes/{id}"),
        json!({"note": "строка 2"}),
    );
    assert_eq!((status, updated["note"].as_str()), (200, Some("строка 2")));
    assert_eq!(updated["position"]["new_line"], 2, "позиция сохраняется");

    let (status, body, response) =
        reviewer.send(Method::Put, &format!("{MR}/draft_notes/{id}/publish"), None);
    assert_eq!((status, body), (204, Value::Null));
    assert!(response.body.is_empty(), "ответ без ID заметки");
    let notes = server.with(|gitlab| gitlab.notes(IID));
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].from_draft, Some(id));
    assert_eq!(notes[0].body, "строка 2");
    let drafts = server.with(|gitlab| gitlab.drafts(IID));
    assert_eq!(drafts.len(), 2);
    assert!(
        drafts
            .iter()
            .any(|d| d.id == planted && d.origin == Origin::Planted)
    );

    let (_, discussions) = author.get(&format!("{MR}/discussions"));
    let note = &discussions[0]["notes"][0];
    assert_eq!(note["type"], "DiffNote");
    assert_eq!(note["position"]["head_sha"], "head1");
    assert_eq!(note["author"]["username"], REVIEWER);
    assert_eq!(note["resolvable"], true);
    assert!(note.get("from_draft").is_none());
    assert_eq!(discussions[0]["id"].as_str().map(str::len), Some(40));
    assert_eq!(server.count(BULK_PUBLISH), 0);

    let (status, _, _) =
        reviewer.send(Method::Delete, &format!("{MR}/draft_notes/{planted}"), None);
    assert_eq!(status, 204);
    assert_eq!(server.with(|gitlab| gitlab.drafts(IID).len()), 1);
}

#[test]
fn bulk_publish_takes_every_draft_of_the_user_including_planted() {
    let server = fake();
    let planted = server
        .with(|gitlab| gitlab.plant_draft(IID, REVIEWER, "из браузера", Some(&added2())))
        .expect("подложен");
    let reviewer = Client::new(&server, "tok-reviewer");
    reviewer.post(&format!("{MR}/draft_notes"), json!({"note": "наш"}));
    let author = Client::new(&server, "tok-author");
    author.post(&format!("{MR}/draft_notes"), json!({"note": "автора"}));
    let (status, _, _) = reviewer.send(
        Method::Post,
        &format!("{MR}/draft_notes/bulk_publish"),
        Some(json!({"note": "итог", "reviewer_state": "reviewed"})),
    );
    assert_eq!(status, 204);
    assert_eq!(server.count(BULK_PUBLISH), 1);
    let notes = server.with(|gitlab| gitlab.notes(IID));
    assert_eq!(notes.len(), 3, "два черновика и итог");
    assert!(
        notes
            .iter()
            .any(|n| n.from_draft == Some(planted) && n.origin == Origin::Planted)
    );
    let drafts = server.with(|gitlab| gitlab.drafts(IID));
    assert_eq!(drafts.len(), 1);
    assert_eq!(
        drafts[0].author, AUTHOR,
        "черновик другого пользователя не тронут"
    );
    assert_eq!(
        server.with(|gitlab| gitlab.reviewer_state(IID, REVIEWER)),
        Some(ReviewerState::Reviewed)
    );
}

#[test]
fn positions_must_match_a_version_triple_and_its_diff() {
    let server = fake();
    server
        .with(|gitlab| {
            gitlab.push_version(
                IID,
                "base1",
                "start1",
                "head2",
                vec![FakeDiff::modified("src/a.rs", DIFF)],
            )
        })
        .expect("rebase");
    let reviewer = Client::new(&server, "tok-reviewer");
    let range = |start: Value, end: Value| {
        let mut value = position("base1", "start1", "head2", None, Some(2));
        value["line_range"] = json!({"start": start, "end": end});
        value
    };
    let ctx1 = json!({"line_code": code(1, 1), "type": null, "old_line": 1, "new_line": 1});
    let new2 = json!({"line_code": code(3, 2), "type": "new", "old_line": null, "new_line": 2});
    let accepted = [
        position("base0", "base0", "head1", None, Some(2)),
        position("base1", "start1", "head2", Some(2), None),
        position("base1", "start1", "head2", Some(3), Some(3)),
        position("base1", "start1", "head2", Some(11), Some(12)),
        range(ctx1.clone(), new2.clone()),
    ];
    for value in accepted {
        let (status, body) = reviewer.post(
            &format!("{MR}/draft_notes"),
            json!({"note": "x", "position": value}),
        );
        assert_eq!(status, 201, "{value} → {body}");
    }
    let mut not_text = added2();
    not_text["position_type"] = json!("image");
    let mut other_path = added2();
    other_path["new_path"] = json!("src/zzz.rs");
    let mut bad_code = ctx1.clone();
    bad_code["line_code"] = json!(code(1, 2));
    let mut bad_type = new2.clone();
    bad_type["type"] = json!("old");
    let add11 = json!({"line_code": code(11, 11), "type": "new", "old_line": null, "new_line": 11});
    let rejected = [
        (
            position("base1", "start1", "head1", None, Some(2)),
            "do not match",
        ),
        (
            position("base0", "base0", "head2", None, Some(2)),
            "do not match",
        ),
        (
            position("base1", "start1", "head2", Some(3), Some(2)),
            "not in the diff",
        ),
        (
            position("base1", "start1", "head2", None, Some(3)),
            "not in the diff",
        ),
        (
            position("base1", "start1", "head2", None, Some(5)),
            "not in the diff",
        ),
        (
            position("base1", "start1", "head2", None, None),
            "incomplete",
        ),
        (not_text, "only text"),
        (other_path, "paths"),
        (range(bad_code, new2.clone()), "line_code"),
        (range(ctx1.clone(), bad_type), "type"),
        (range(add11.clone(), new2.clone()), "one hunk"),
        (range(ctx1.clone(), add11), "end is not"),
    ];
    for (value, expected) in rejected {
        let (status, body) = reviewer.post(
            &format!("{MR}/draft_notes"),
            json!({"note": "x", "position": value}),
        );
        assert_eq!(status, 400, "{value} → {body}");
        assert!(
            body["message"]
                .as_str()
                .is_some_and(|m| m.contains(expected)),
            "{value} → {body}"
        );
    }
    assert_eq!(server.with(|gitlab| gitlab.drafts(IID).len()), 5);
    let (status, body) = reviewer.post(
        &format!("{MR}/discussions"),
        json!({"body": "x", "position": position("base1", "start1", "head1", None, Some(2))}),
    );
    assert_eq!(status, 400, "{body}");
    assert!(server.with(|gitlab| gitlab.discussions(IID).is_empty()));
}

#[test]
fn approve_checks_sha_eligibility_and_target_change() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let (status, body) = reviewer.post(&format!("{MR}/approve"), json!({"sha": "stale"}));
    assert_eq!(status, 409, "{body}");
    assert!(server.with(|gitlab| gitlab.approved_by(IID).is_empty()));
    let (status, state) = reviewer.post(&format!("{MR}/approve"), json!({"sha": "head1"}));
    assert_eq!(status, 201, "{state}");
    assert_eq!(state["approved_by"][0]["user"]["username"], REVIEWER);
    assert_eq!(state["user_has_approved"], true);
    assert_eq!(
        server.with(|gitlab| gitlab.reviewer_state(IID, REVIEWER)),
        Some(ReviewerState::Approved)
    );
    assert_eq!(
        reviewer.post(&format!("{MR}/approve"), json!({})).0,
        401,
        "повторное одобрение"
    );
    let author = Client::new(&server, "tok-author");
    assert_eq!(
        author.post(&format!("{MR}/approve"), json!({})).0,
        401,
        "автор"
    );
    let reporter = Client::new(&server, "tok-reporter");
    assert_eq!(
        reporter
            .post(&format!("{MR}/approve"), json!({"sha": "head1"}))
            .0,
        401
    );
    let (status, state) = reviewer.post(&format!("{MR}/unapprove"), json!({}));
    assert_eq!((status, state["approved"].as_bool()), (201, Some(false)));
    assert_eq!(
        server.with(|gitlab| gitlab.reviewer_state(IID, REVIEWER)),
        Some(ReviewerState::Unapproved)
    );

    server
        .with(|gitlab| {
            gitlab.move_target(
                IID,
                "base1",
                "start1",
                vec![FakeDiff::modified("src/a.rs", DIFF)],
            )
        })
        .expect("целевая ветка");
    assert_eq!(
        server.with(|gitlab| gitlab.diff_refs(IID)),
        Some(("base1".to_owned(), "start1".to_owned(), "head1".to_owned()))
    );
    let (status, _) = reviewer.post(&format!("{MR}/approve"), json!({"sha": "head1"}));
    assert_eq!(status, 409, "тот же head после смены целевой ветки");
    assert_eq!(
        reviewer.post(&format!("{MR}/approve"), json!({})).0,
        201,
        "без sha"
    );
    let (_, approvals) = author.get(&format!("{MR}/approvals"));
    assert_eq!(approvals["approved"], true);
    assert_eq!(approvals["user_can_approve"], false);
}

#[test]
fn request_changes_succeeds_only_for_assigned_developer() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    reviewer.post(&format!("{MR}/approve"), json!({}));
    assert_eq!(review_state(&reviewer), "APPROVED");
    let done = reviewer.graphql(RC, rc_input());
    assert_eq!(
        done["data"]["mergeRequestRequestChanges"]["errors"],
        json!([]),
        "{done}"
    );
    assert!(done.get("errors").is_none());
    assert_eq!(review_state(&reviewer), "REQUESTED_CHANGES");
    assert!(
        server.with(|gitlab| gitlab.approved_by(IID).is_empty()),
        "одобрение снято"
    );
    assert_eq!(server.count(REQUEST_CHANGES), 1);

    let author = Client::new(&server, "tok-author");
    let not_reviewer = author.graphql(RC, rc_input());
    assert_eq!(
        not_reviewer["data"]["mergeRequestRequestChanges"]["errors"],
        json!(["Reviewer not found"])
    );
    let reporter = Client::new(&server, "tok-reporter").graphql(RC, rc_input());
    let missing = reviewer.graphql(
        RC,
        json!({"input": {"projectPath": "group/proj", "iid": "99"}}),
    );
    assert_eq!(reporter, missing, "нет прав и нет MR неразличимы");
    assert_eq!(reporter["errors"][0]["message"], NOT_AVAILABLE);
    assert_eq!(reporter["data"]["mergeRequestRequestChanges"], Value::Null);
    let outsider = Client::new(&server, "tok-outsider");
    assert_eq!(outsider.graphql(RC, rc_input()), missing);
    assert_eq!(
        outsider.graphql(STATE, json!({"fullPath": "group/proj", "iid": "3"}))["data"]["project"],
        Value::Null
    );
}

#[test]
fn request_changes_modes_leave_reviewer_state_unchanged() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let introspection = "{ __type(name: \"Mutation\") { fields { name } } }";
    let fields = |client: &Client| -> Vec<String> {
        client.graphql(introspection, json!({}))["data"]["__type"]["fields"]
            .as_array()
            .map(|fields| {
                fields
                    .iter()
                    .filter_map(|f| f["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(fields(&reviewer).contains(&"mergeRequestRequestChanges".to_owned()));
    assert_eq!(
        reviewer.graphql(
            "query($name: String!) { __type(name: $name) { name } }",
            json!({"name": "Query"})
        )["data"]["__type"],
        Value::Null
    );

    server.with(|gitlab| gitlab.set_request_changes(RequestChangesMode::Absent));
    assert!(!fields(&reviewer).contains(&"mergeRequestRequestChanges".to_owned()));
    let absent = reviewer.graphql(RC, rc_input());
    assert_eq!(absent["errors"][0]["extensions"]["code"], "undefinedField");
    assert!(absent.get("data").is_none());

    server.with(|gitlab| gitlab.set_request_changes(RequestChangesMode::Forbidden));
    let forbidden = reviewer.graphql(RC, rc_input());
    assert_eq!(forbidden["errors"][0]["message"], NOT_AVAILABLE);

    server.with(|gitlab| gitlab.set_request_changes(RequestChangesMode::ExecutionError));
    let failed = reviewer.graphql(RC, rc_input());
    assert_eq!(
        failed["errors"][0]["message"],
        "forge-fake: execution error"
    );
    assert_eq!(failed["data"]["mergeRequestRequestChanges"], Value::Null);

    server.with(|gitlab| gitlab.set_request_changes(RequestChangesMode::PayloadError));
    let payload = reviewer.graphql(RC, rc_input());
    assert!(payload.get("errors").is_none());
    assert_eq!(
        payload["data"]["mergeRequestRequestChanges"]["errors"],
        json!(["forge-fake: payload error"])
    );
    assert_eq!(
        payload["data"]["mergeRequestRequestChanges"]["mergeRequest"],
        Value::Null
    );

    assert_eq!(review_state(&reviewer), "UNREVIEWED");
    assert_eq!(server.count(REQUEST_CHANGES), 4);
}

#[test]
fn notes_discussions_replies_resolve_and_normalization() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let (status, note) = reviewer.post(
        &format!("{MR}/notes"),
        json!({"body": "итог\r\nвторая  \n\n"}),
    );
    assert_eq!(status, 201, "{note}");
    assert_eq!(note["body"], "итог\nвторая");
    assert_eq!(note["type"], Value::Null);
    let (status, discussion) = reviewer.post(
        &format!("{MR}/discussions"),
        json!({"body": "тред", "position": added2()}),
    );
    assert_eq!(status, 201, "{discussion}");
    assert_eq!(discussion["individual_note"], false);
    assert_eq!(discussion["notes"][0]["type"], "DiffNote");
    let id = discussion["id"].as_str().expect("id").to_owned();
    let author = Client::new(&server, "tok-author");
    let (status, reply) = author.post(
        &format!("{MR}/discussions/{id}/notes"),
        json!({"body": "ответ"}),
    );
    assert_eq!(
        (status, reply["type"].as_str()),
        (201, Some("DiscussionNote"))
    );
    let (status, resolved) =
        author.put(&format!("{MR}/discussions/{id}"), json!({"resolved": true}));
    assert_eq!(status, 200, "{resolved}");
    assert_eq!(resolved["notes"][0]["resolved"], true);
    let reporter = Client::new(&server, "tok-reporter");
    assert_eq!(
        reporter
            .put(
                &format!("{MR}/discussions/{id}"),
                json!({"resolved": false})
            )
            .0,
        403
    );

    let (_, draft) = reviewer.post(
        &format!("{MR}/draft_notes"),
        json!({"note": "текст\r\n/approve\n/label ~x\nхвост   \n"}),
    );
    assert_eq!(
        draft["note"], "текст\n/approve\n/label ~x\nхвост",
        "quick actions до публикации"
    );
    let draft_id = draft["id"].as_u64().expect("id");
    reviewer.put(&format!("{MR}/draft_notes/{draft_id}/publish"), json!({}));
    let published = server.with(|gitlab| gitlab.notes(IID).last().map(|n| n.body.clone()));
    assert_eq!(published.as_deref(), Some("текст\nхвост"));
    assert!(
        server.with(|gitlab| gitlab.approved_by(IID).is_empty()),
        "quick action не исполняется"
    );

    let (_, desc, response) = author.send(Method::Get, &format!("{MR}/notes?per_page=2"), None);
    assert_eq!(desc.as_array().map(Vec::len), Some(2));
    assert_eq!(
        desc[0]["body"], "текст\nхвост",
        "по умолчанию новые первыми"
    );
    assert_eq!(response.header("x-total"), Some("4"));
    assert_eq!(response.header("x-total-pages"), Some("2"));
    assert_eq!(response.header("x-next-page"), Some("2"));
    assert_eq!(response.header("x-prev-page"), Some(""));
    assert!(
        response
            .header("link")
            .is_some_and(|l| l.contains("page=2&per_page=2>; rel=\"next\""))
    );
    let (_, asc, response) = author.send(
        Method::Get,
        &format!("{MR}/notes?sort=asc&page=2&per_page=3"),
        None,
    );
    assert_eq!(asc.as_array().map(Vec::len), Some(1), "неполная страница");
    assert_eq!(asc[0]["body"], "текст\nхвост");
    assert_eq!(response.header("x-next-page"), Some(""));
    assert!(
        response
            .header("link")
            .is_some_and(|l| l.contains("sort=asc&page=1&per_page=3>; rel=\"prev\""))
    );
    let (status, body) = reviewer.post(&format!("{MR}/notes"), json!({"body": "/approve\n  "}));
    assert_eq!(status, 400, "{body}");
}

#[test]
fn out_of_band_controls_for_reconcile() {
    let server = fake();
    let (discussion, note) = server
        .with(|gitlab| gitlab.plant_discussion(IID, REVIEWER, "тот же текст", Some(&added2())))
        .expect("подложен");
    let reviewer = Client::new(&server, "tok-reviewer");
    let (_, discussions) = reviewer.get(&format!("{MR}/discussions"));
    assert_eq!(discussions[0]["id"], discussion.as_str());
    assert_eq!(discussions[0]["notes"][0]["id"].as_u64(), Some(note));
    let notes = server.with(|gitlab| gitlab.notes(IID));
    assert_eq!(
        (notes[0].origin, notes[0].from_draft),
        (Origin::Planted, None)
    );

    let (_, draft) = reviewer.post(&format!("{MR}/draft_notes"), json!({"note": "наш"}));
    let id = draft["id"].as_u64().expect("id");
    server
        .with(|gitlab| gitlab.remove_draft(IID, id))
        .expect("удалён");
    assert_eq!(
        reviewer
            .put(&format!("{MR}/draft_notes/{id}/publish"), json!({}))
            .0,
        404
    );
    assert!(server.with(|gitlab| gitlab.remove_draft(IID, id)).is_err());
    assert!(
        server
            .log()
            .iter()
            .all(|call| call.user.as_deref() == Some(REVIEWER))
    );
}

#[test]
fn partial_publish_failure_and_lost_response_after_apply() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let ids: Vec<u64> = (0..3)
        .map(|n| {
            let (_, draft) = reviewer.post(
                &format!("{MR}/draft_notes"),
                json!({"note": format!("черновик {n}")}),
            );
            draft["id"].as_u64().expect("id")
        })
        .collect();
    server.inject(Fault::new(PUBLISH, Reply::Status(500)).skip(1));
    server.inject(Fault::new(PUBLISH, Reply::Silence).after_apply());
    let publish = |id: u64| {
        reviewer
            .put(&format!("{MR}/draft_notes/{id}/publish"), json!({}))
            .0
    };
    assert_eq!(publish(ids[0]), 204);
    assert_eq!(publish(ids[1]), 500);
    let remaining = server.with(|gitlab| gitlab.drafts(IID));
    assert_eq!(remaining.len(), 2, "отказавшая публикация не применена");

    let quick = Client::with(
        &server,
        Credential::private_token(Secret::new("tok-reviewer")),
        300,
    );
    let request = Request::new(
        Method::Put,
        format!("{}{MR}/draft_notes/{}/publish", server.base(), ids[2]),
    );
    let failure = quick.http.send(&request).expect_err("ответа нет");
    assert!(failure.may_have_been_sent(), "{failure}");
    server
        .wait_calls(3 + 3, RECEIPT)
        .expect("квитанция сервера");
    assert_eq!(server.parked(), 1);
    let notes = server.with(|gitlab| gitlab.notes(IID));
    assert!(
        notes.iter().any(|n| n.from_draft == Some(ids[2])),
        "публикация принята"
    );
    assert_eq!(server.with(|gitlab| gitlab.drafts(IID).len()), 1);
    let log = server.log();
    let last = log.last().expect("вызов");
    assert_eq!(
        (last.key.as_str(), last.status, last.applied),
        (PUBLISH, None, true)
    );
    server.release_parked();
    assert!(server.finish().is_ok());
    assert_eq!(server.count(PUBLISH), 3);
    assert_eq!(server.count(BULK_PUBLISH), 0);
}

#[test]
fn rate_limit_is_429_with_retry_after() {
    let server = fake();
    server.inject(Fault::new(DRAFTS, Reply::RateLimited));
    let reviewer = Client::new(&server, "tok-reviewer");
    let (status, _, response) = reviewer.send(
        Method::Post,
        &format!("{MR}/draft_notes"),
        Some(json!({"note": "x"})),
    );
    assert_eq!(status, 429);
    assert_eq!(response.header("retry-after"), Some("60"));
    assert_eq!(response.header("ratelimit-remaining"), Some("0"));
    assert!(server.with(|gitlab| gitlab.drafts(IID).is_empty()));
    let (_, _, normal) = reviewer.send(Method::Get, MR, None);
    assert!(
        normal
            .header("ratelimit-remaining")
            .is_some_and(|left| left != "0")
    );
    assert_eq!(server.count("unrouted GET"), 0);
}

#[test]
fn scanner_counts_like_gitlab_parser() {
    let lines = scan(DIFF);
    let pairs: Vec<(u32, u32)> = lines.iter().map(|l| (l.old, l.new)).collect();
    assert_eq!(
        pairs,
        [
            (1, 1),
            (2, 2),
            (3, 2),
            (3, 3),
            (4, 4),
            (10, 10),
            (11, 11),
            (11, 12)
        ]
    );
    assert_eq!(lines[2].old_line(), None);
    assert_eq!(lines[1].new_line(), None);
    assert_eq!(lines[6].hunk, 2);
    assert_eq!(decode_segment("a%2Fb+c%zz%2"), "a/b+c%zz%2");
    assert_eq!(normalize("a \r\nb\t\n\n"), "a \nb");
}
