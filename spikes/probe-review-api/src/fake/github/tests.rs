use std::time::Duration;

use serde_json::{Value, json};

use super::*;
use crate::fake::{FakeServer, Fault, Reply};
use crate::transport::http::{Credential, HttpConfig, HttpTransport, Secret};
use crate::transport::{FailureKind, Request, Transport};

const AUTHOR: &str = "alice";
const REVIEWER: &str = "bob";
const PATCH: &str = "@@ -1,4 +1,4 @@\n ctx1\n-old2\n+new2\n ctx3\n ctx4\n@@ -10,2 +10,3 @@\n ctx10\n+add11\n ctx11\n";
const PULL: u64 = 7;
const REVIEWS: &str = "/repos/o/r/pulls/7/reviews";
const RECEIPT: Duration = Duration::from_secs(10);

fn fake() -> FakeServer<GithubFake> {
    let mut github = GithubFake::new("o", "r");
    github.add_user(AUTHOR, "tok-author", Permission::Write);
    github.add_user(REVIEWER, "tok-reviewer", Permission::Write);
    github.add_user("eve", "tok-reader", Permission::Read);
    github.add_pull(
        PULL,
        AUTHOR,
        "base0",
        FakeCommit {
            sha: "head1".to_owned(),
            files: vec![
                FakeFile::modified("src/a.rs", PATCH),
                FakeFile::modified("src/b.rs", "@@ -1 +1 @@\n-x\n+y\n"),
            ],
        },
    );
    FakeServer::start(github).expect("сервер")
}

struct Client {
    base: String,
    http: HttpTransport,
}

impl Client {
    fn new(server: &FakeServer<GithubFake>, token: &str) -> Client {
        Client::with_timeout(server, token, Duration::from_secs(5))
    }

    fn with_timeout(server: &FakeServer<GithubFake>, token: &str, timeout: Duration) -> Client {
        Client {
            base: server.base().to_owned(),
            http: HttpTransport::new(
                HttpConfig {
                    connect_timeout: Duration::from_secs(5),
                    call_timeout: timeout,
                },
                Some(Credential::bearer(Secret::new(token))),
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
            response.body_json().expect("JSON")
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

    fn graphql(&self, query: &str, variables: Value) -> Value {
        let (status, value) =
            self.post("/graphql", json!({"query": query, "variables": variables}));
        assert_eq!(status, 200, "{value}");
        value
    }
}

const ADD_THREAD: &str = "mutation($input: AddPullRequestReviewThreadInput!) { \
    addPullRequestReviewThread(input: $input) { thread { id comments(first: 1) { nodes { id fullDatabaseId body } } } } }";
const UPDATE_COMMENT: &str = "mutation($input: UpdatePullRequestReviewCommentInput!) { \
    updatePullRequestReviewComment(input: $input) { pullRequestReviewComment { id fullDatabaseId body } } }";

fn spec(line: u32, body: &str) -> CommentSpec {
    CommentSpec::line("src/a.rs", DiffSide::Right, line, body)
}

#[test]
fn reads_repo_user_pull_files_and_commits() {
    let server = fake();
    let author = Client::new(&server, "tok-author");
    assert_eq!(author.get("/user").1["login"], AUTHOR);
    let (status, repo) = author.get("/repos/o/r");
    assert_eq!(status, 200);
    assert_eq!(repo["private"], true);
    assert_eq!(repo["permissions"]["push"], true);
    let reader = Client::new(&server, "tok-reader");
    assert_eq!(reader.get("/repos/o/r").1["permissions"]["push"], false);

    let (_, pull) = author.get("/repos/o/r/pulls/7");
    assert_eq!(pull["head"]["sha"], "head1");
    assert_eq!(pull["base"]["sha"], "base0");
    assert_eq!(pull["node_id"], "PR_fake7");
    let (_, files) = author.get("/repos/o/r/pulls/7/files");
    assert_eq!(files[0]["filename"], "src/a.rs");
    assert_eq!(files[0]["patch"], PATCH);
    assert_eq!(
        author.get("/repos/o/r/pulls/7/commits").1,
        json!([{"sha": "head1"}])
    );
    assert_eq!(author.get("/repos/o/other").0, 404);
    assert_eq!(author.get("/repos/o/r/pulls/8").0, 404);
}

#[test]
fn authentication_is_required() {
    let server = fake();
    let anonymous = HttpTransport::new(HttpConfig::default(), None);
    let response = anonymous
        .send(&Request::get(format!("{}/repos/o/r", server.base())))
        .expect("ответ");
    assert_eq!(response.status, 401);
    let stranger = Client::new(&server, "tok-unknown");
    let (status, body) = stranger.get("/repos/o/r");
    assert_eq!(
        (status, body["message"].as_str()),
        (401, Some("Bad credentials"))
    );
}

#[test]
fn create_review_with_event_publishes_comments_at_once() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let (status, review) = reviewer.post(
        REVIEWS,
        json!({
            "commit_id": "head1",
            "event": "REQUEST_CHANGES",
            "body": "итог",
            "comments": [
                {"path": "src/a.rs", "line": 2, "side": "RIGHT", "body": "новая"},
                {"path": "src/a.rs", "line": 2, "side": "LEFT", "body": "старая"},
                {"path": "src/a.rs", "start_line": 1, "start_side": "RIGHT", "line": 3, "side": "RIGHT", "body": "диапазон"},
            ],
        }),
    );
    assert_eq!(status, 200, "{review}");
    assert_eq!(review["state"], "CHANGES_REQUESTED");
    assert!(review["submitted_at"].is_string());
    assert!(
        review["node_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("PRR_"))
    );

    let author = Client::new(&server, "tok-author");
    let (_, comments) = author.get("/repos/o/r/pulls/7/comments");
    let comments = comments.as_array().expect("список");
    assert_eq!(comments.len(), 3);
    assert_eq!(comments[1]["side"], "LEFT");
    assert_eq!(comments[2]["start_line"], 1);
    assert_eq!(comments[2]["start_side"], "RIGHT");
    assert_eq!(server.with(|github| github.threads(PULL).len()), 3);
}

#[test]
fn create_review_rejections_change_nothing() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let author = Client::new(&server, "tok-author");
    let cases = [
        (
            &reviewer,
            json!({"event": "COMMENT", "comments": [{"path": "src/a.rs", "line": 2, "body": "x"}]}),
            "Body is required",
        ),
        (
            &reviewer,
            json!({"event": "REQUEST_CHANGES", "body": " "}),
            "Body is required",
        ),
        (&author, json!({"event": "APPROVE"}), "approve your own"),
        (
            &author,
            json!({"event": "REQUEST_CHANGES", "body": "x"}),
            "request changes on your own",
        ),
        (&reviewer, json!({"event": "MERGE"}), "not included"),
        (
            &reviewer,
            json!({"commit_id": "nope", "event": "APPROVE"}),
            "not part of the pull request",
        ),
        (
            &reviewer,
            json!({"event": "COMMENT", "body": "x", "comments": [{"path": "src/a.rs", "line": 2, "body": "ok"}, {"path": "src/a.rs", "line": 5, "body": "нет строки"}]}),
            "Line could not be resolved",
        ),
        (
            &reviewer,
            json!({"event": "COMMENT", "body": "x", "comments": [{"path": "src/a.rs", "position": 3, "body": "x"}]}),
            "position",
        ),
        (
            &reviewer,
            json!({"event": "COMMENT", "body": "x", "comments": [{"path": "src/a.rs", "start_line": 3, "line": 11, "body": "два блока"}]}),
            "same hunk",
        ),
        (
            &reviewer,
            json!({"event": "COMMENT", "body": "x", "comments": [{"path": "src/a.rs", "start_line": 3, "line": 2, "body": "обратный"}]}),
            "precede",
        ),
        (
            &reviewer,
            json!({"event": "COMMENT", "body": "x", "comments": [{"path": "src/zzz.rs", "line": 1, "body": "x"}]}),
            "Path could not be resolved",
        ),
    ];
    for (client, body, expected) in cases {
        let (status, error) = client.post(REVIEWS, body.clone());
        assert_eq!(status, 422, "{body} → {error}");
        assert!(
            error["errors"][0]
                .as_str()
                .is_some_and(|e| e.contains(expected)),
            "{body} → {error}"
        );
    }
    assert!(server.with(|github| github.reviews(PULL).is_empty()));
    assert!(server.with(|github| github.comments(PULL).is_empty()));
    let (status, review) = author.post(
        REVIEWS,
        json!({"event": "COMMENT", "body": "автор может комментировать"}),
    );
    assert_eq!((status, review["state"].as_str()), (200, Some("COMMENTED")));
}

#[test]
fn pending_review_is_visible_only_to_its_owner() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let author = Client::new(&server, "tok-author");
    let (status, pending) = reviewer.post(
        REVIEWS,
        json!({"body": "", "comments": [{"path": "src/a.rs", "line": 2, "body": "черновик"}]}),
    );
    assert_eq!(status, 200, "{pending}");
    assert_eq!(pending["state"], "PENDING");
    assert!(pending.get("submitted_at").is_none());
    let id = pending["id"].as_u64().expect("id");

    assert_eq!(reviewer.get(REVIEWS).1.as_array().map(Vec::len), Some(1));
    let (status, own) = reviewer.get(&format!("{REVIEWS}/{id}"));
    assert_eq!(status, 200, "{own}");
    assert_eq!(
        (own["id"].as_u64(), own["state"].as_str()),
        (Some(id), Some("PENDING"))
    );
    assert!(own.get("submitted_at").is_none());
    assert_eq!(author.get(REVIEWS).1, json!([]));
    assert_eq!(author.get(&format!("{REVIEWS}/{id}")).0, 404);
    assert_eq!(author.get(&format!("{REVIEWS}/{id}/comments")).0, 404);
    assert_eq!(
        reviewer.get(&format!("{REVIEWS}/{id}/comments")).1[0]["body"],
        "черновик"
    );
    assert_eq!(reviewer.get("/repos/o/r/pulls/7/comments").1, json!([]));
    assert_eq!(
        author
            .post(
                &format!("{REVIEWS}/{id}/events"),
                json!({"event": "COMMENT", "body": "x"})
            )
            .0,
        404
    );

    let (status, error) = reviewer.post(REVIEWS, json!({"body": "второй"}));
    assert_eq!(status, 422);
    assert!(
        error["errors"][0]
            .as_str()
            .is_some_and(|e| e.contains("one pending review"))
    );
}

#[test]
fn pending_cycle_with_graphql_threads_and_submit_without_comments() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let (_, pending) = reviewer.post(REVIEWS, json!({"commit_id": "head1"}));
    let review_id = pending["id"].as_u64().expect("id");
    let review_node = pending["node_id"].as_str().expect("node_id").to_owned();

    let added = reviewer.graphql(
        ADD_THREAD,
        json!({"input": {"pullRequestReviewId": review_node, "path": "src/a.rs", "line": 2, "side": "LEFT", "body": "первый"}}),
    );
    let comment = &added["data"]["addPullRequestReviewThread"]["thread"]["comments"]["nodes"][0];
    let comment_node = comment["id"].as_str().expect("id").to_owned();
    let comment_id = comment["fullDatabaseId"]
        .as_str()
        .and_then(|id| id.parse::<u64>().ok())
        .expect("fullDatabaseId строкой, как BigInt");
    assert!(comment.get("databaseId").is_none(), "устаревший databaseId");
    assert_eq!(comment["body"], "первый");

    let updated = reviewer.graphql(
        UPDATE_COMMENT,
        json!({"input": {"pullRequestReviewCommentId": comment_node, "body": "исправленный"}}),
    );
    let after = &updated["data"]["updatePullRequestReviewComment"]["pullRequestReviewComment"];
    assert_eq!(
        after["fullDatabaseId"].as_str(),
        Some(comment_id.to_string().as_str())
    );
    assert_eq!(after["body"], "исправленный");

    let bad_line = reviewer.graphql(
        ADD_THREAD,
        json!({"input": {"pullRequestReviewId": review_node, "path": "src/a.rs", "line": 9, "body": "x"}}),
    );
    assert_eq!(bad_line["errors"][0]["type"], "UNPROCESSABLE");

    let (status, error) = reviewer.post(
        &format!("{REVIEWS}/{review_id}/events"),
        json!({"event": "COMMENT", "body": "итог", "comments": [{"path": "src/a.rs", "line": 2, "body": "x"}]}),
    );
    assert_eq!(status, 422, "{error}");
    assert_eq!(
        server.with(|github| github.reviews(PULL)[0].state),
        ReviewState::Pending
    );

    let (status, submitted) = reviewer.post(
        &format!("{REVIEWS}/{review_id}/events"),
        json!({"event": "COMMENT", "body": "итог"}),
    );
    assert_eq!(status, 200, "{submitted}");
    assert_eq!(submitted["id"].as_u64(), Some(review_id));
    assert_eq!(submitted["state"], "COMMENTED");
    let author = Client::new(&server, "tok-author");
    let (_, comments) = author.get("/repos/o/r/pulls/7/comments");
    assert_eq!(comments[0]["id"].as_u64(), Some(comment_id));
    assert_eq!(comments[0]["body"], "исправленный");
    assert_eq!(
        comments[0]["pull_request_review_id"].as_u64(),
        Some(review_id)
    );

    let (status, _) = reviewer.post(
        &format!("{REVIEWS}/{review_id}/events"),
        json!({"event": "APPROVE"}),
    );
    assert_eq!(status, 422, "повторный Submit");
    assert_eq!(server.count("graphql addPullRequestReviewThread"), 2);
    assert_eq!(server.count("graphql updatePullRequestReviewComment"), 1);
    assert_eq!(
        server.count("POST /repos/{owner}/{repo}/pulls/{number}/reviews/{review_id}/events"),
        3
    );
}

#[test]
fn foreign_pending_is_planted_paged_and_not_writable_by_others() {
    let server = fake();
    let (review_id, ids) = server
        .with(|github| {
            github.plant_pending(
                PULL,
                REVIEWER,
                "из браузера",
                &[spec(2, "раз"), spec(3, "два"), spec(11, "три")],
            )
        })
        .expect("подложен");
    assert_eq!(ids.len(), 3);
    assert!(server.log().is_empty(), "подложенное не идёт через HTTP");

    let reviewer = Client::new(&server, "tok-reviewer");
    let (_, reviews) = reviewer.get(REVIEWS);
    assert_eq!(reviews[0]["state"], "PENDING");
    assert_eq!(reviews[0]["body"], "из браузера");
    let first = reviewer.send(
        Method::Get,
        &format!("{REVIEWS}/{review_id}/comments?per_page=2"),
        None,
    );
    assert_eq!(first.1.as_array().map(Vec::len), Some(2));
    let link = first.2.header("link").expect("Link").to_owned();
    assert!(link.contains("page=2>; rel=\"next\""), "{link}");
    let (_, second) = reviewer.get(&format!("{REVIEWS}/{review_id}/comments?per_page=2&page=2"));
    assert_eq!(second[0]["id"].as_u64(), Some(ids[2]));

    let (status, _) = reviewer.post(REVIEWS, json!({"event": "COMMENT", "body": "поверх"}));
    assert_eq!(status, 422, "второй PENDING не создаётся");
    let author = Client::new(&server, "tok-author");
    let review_node = server.with(|github| github.reviews(PULL)[0].node_id.clone());
    let foreign = author.graphql(
        ADD_THREAD,
        json!({"input": {"pullRequestReviewId": review_node, "path": "src/a.rs", "line": 2, "body": "чужой"}}),
    );
    assert_eq!(foreign["errors"][0]["type"], "NOT_FOUND");
    assert_eq!(foreign["data"]["addPullRequestReviewThread"], Value::Null);
    assert_eq!(server.with(|github| github.comments(PULL).len()), 3);
    assert!(server.with(|github| {
        github
            .comments(PULL)
            .iter()
            .all(|c| c.origin == Origin::Planted)
    }));

    let (status, _, _) = reviewer.send(Method::Delete, &format!("{REVIEWS}/{review_id}"), None);
    assert_eq!(status, 200);
    assert!(
        server.with(|github| github.comments(PULL).is_empty() && github.threads(PULL).is_empty())
    );
}

#[test]
fn update_and_delete_review_rules() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let (_, review) = reviewer.post(REVIEWS, json!({"event": "COMMENT", "body": "был"}));
    let id = review["id"].as_u64().expect("id");
    let (status, updated, _) = reviewer.send(
        Method::Put,
        &format!("{REVIEWS}/{id}"),
        Some(json!({"body": "стал"})),
    );
    assert_eq!((status, updated["body"].as_str()), (200, Some("стал")));
    let author = Client::new(&server, "tok-author");
    let (status, _, _) = author.send(
        Method::Put,
        &format!("{REVIEWS}/{id}"),
        Some(json!({"body": "x"})),
    );
    assert_eq!(status, 403);
    let (status, _, _) = reviewer.send(Method::Delete, &format!("{REVIEWS}/{id}"), None);
    assert_eq!(status, 422, "отправленный review не удаляется");
}

#[test]
fn replies_and_resolve_threads() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let (_, review) = reviewer.post(
        REVIEWS,
        json!({"event": "COMMENT", "body": "итог", "comments": [{"path": "src/a.rs", "line": 2, "body": "корень"}]}),
    );
    assert_eq!(review["state"], "COMMENTED");
    let root = server.with(|github| github.comments(PULL)[0].clone());
    let author = Client::new(&server, "tok-author");
    let replies = format!("/repos/o/r/pulls/7/comments/{}/replies", root.id);
    let (status, reply) = author.post(&replies, json!({"body": "ответ"}));
    assert_eq!(status, 201, "{reply}");
    assert_eq!(reply["in_reply_to_id"].as_u64(), Some(root.id));
    let reply_id = reply["id"].as_u64().expect("id");
    let (status, _) = author.post(
        &format!("/repos/o/r/pulls/7/comments/{reply_id}/replies"),
        json!({"body": "ответ на ответ"}),
    );
    assert_eq!(status, 422);

    let resolve = "mutation($input: ResolveReviewThreadInput!) { resolveReviewThread(input: $input) { thread { id isResolved } } }";
    let thread_node = server.with(|github| github.threads(PULL)[0].node_id.clone());
    let resolved = author.graphql(resolve, json!({"input": {"threadId": thread_node}}));
    assert_eq!(
        resolved["data"]["resolveReviewThread"]["thread"]["isResolved"],
        true
    );
    assert_eq!(
        resolved["data"]["resolveReviewThread"]["thread"]["comments"]["totalCount"],
        2
    );
    let unresolve = "mutation($input: UnresolveReviewThreadInput!) { unresolveReviewThread(input: $input) { thread { id isResolved } } }";
    author.graphql(unresolve, json!({"input": {"threadId": thread_node}}));
    assert!(!server.with(|github| github.threads(PULL)[0].resolved));
}

#[test]
fn viewed_files_mark_unmark_and_dismiss_on_head_change() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let mark = "mutation($input: MarkFileAsViewedInput!) { markFileAsViewed(input: $input) { pullRequest { id } } }";
    let unmark = "mutation($input: UnmarkFileAsViewedInput!) { unmarkFileAsViewed(input: $input) { pullRequest { id } } }";
    let node = "query($id: ID!) { node(id: $id) { ... on PullRequest { files(first: 100) { nodes { path viewerViewedState } } } } }";
    for path in ["src/a.rs", "src/b.rs"] {
        let marked = reviewer.graphql(
            mark,
            json!({"input": {"pullRequestId": "PR_fake7", "path": path}}),
        );
        assert_eq!(
            marked["data"]["markFileAsViewed"]["pullRequest"]["id"],
            "PR_fake7"
        );
    }
    let missing = reviewer.graphql(
        mark,
        json!({"input": {"pullRequestId": "PR_fake7", "path": "nope"}}),
    );
    assert_eq!(missing["errors"][0]["type"], "NOT_FOUND");
    reviewer.graphql(
        unmark,
        json!({"input": {"pullRequestId": "PR_fake7", "path": "src/b.rs"}}),
    );
    let state = reviewer.graphql(node, json!({"id": "PR_fake7"}));
    let files = &state["data"]["node"]["files"]["nodes"];
    assert_eq!(files[0]["viewerViewedState"], "VIEWED");
    assert_eq!(files[1]["viewerViewedState"], "UNVIEWED");
    let author = Client::new(&server, "tok-author");
    let theirs = author.graphql(node, json!({"id": "PR_fake7"}));
    assert_eq!(
        theirs["data"]["node"]["files"]["nodes"][0]["viewerViewedState"],
        "UNVIEWED"
    );

    server
        .with(|github| {
            github.push_head(
                PULL,
                FakeCommit {
                    sha: "head2".to_owned(),
                    files: vec![FakeFile::modified("src/a.rs", "@@ -1 +1 @@\n-a\n+b\n")],
                },
            )
        })
        .expect("смена head");
    assert_eq!(
        server.with(|github| github.viewed(PULL, REVIEWER)),
        BTreeMap::from([("src/a.rs".to_owned(), Viewed::Dismissed)])
    );
    assert_eq!(author.get("/repos/o/r/pulls/7").1["head"]["sha"], "head2");
}

#[test]
fn head_change_keeps_old_commit_anchors_valid_for_old_commit_only() {
    let server = fake();
    server
        .with(|github| {
            github.push_head(
                PULL,
                FakeCommit {
                    sha: "head2".to_owned(),
                    files: vec![FakeFile::modified(
                        "src/a.rs",
                        "@@ -1,2 +1,2 @@\n-old1\n+new1\n ctx2\n",
                    )],
                },
            )
        })
        .expect("смена head");
    let reviewer = Client::new(&server, "tok-reviewer");
    let comment = json!([{"path": "src/a.rs", "line": 11, "body": "строка старого head"}]);
    let (status, _) = reviewer.post(
        REVIEWS,
        json!({"event": "COMMENT", "body": "x", "comments": comment}),
    );
    assert_eq!(status, 422, "без commit_id — свежий head без строки 11");
    let (status, review) = reviewer.post(
        REVIEWS,
        json!({"commit_id": "head1", "event": "COMMENT", "body": "x", "comments": comment}),
    );
    assert_eq!(status, 200, "{review}");
    assert_eq!(review["commit_id"], "head1");
    assert_eq!(
        server.with(|github| github.head_sha(PULL)),
        Some("head2".to_owned())
    );
}

#[test]
fn graphql_unknown_field_and_missing_input_are_errors_with_http_200() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let unknown = reviewer.graphql("mutation { bulkSubmit(input: {}) { ok } }", json!({}));
    assert_eq!(unknown["errors"][0]["type"], "undefinedField");
    assert!(unknown.get("data").is_none());
    let no_input = reviewer.graphql(ADD_THREAD, json!({}));
    assert_eq!(no_input["errors"][0]["type"], "VARIABLE");
    assert_eq!(server.count("graphql bulkSubmit"), 1);
}

const INTROSPECT: &str = "query($name: String!) { __type(name: $name) { fields { name } } }";

fn mutation_names(value: &Value) -> Vec<String> {
    value["data"]["__type"]["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|field| field["name"].as_str().expect("name").to_owned())
        .collect()
}

#[test]
fn versions_and_mutation_introspection() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    assert_eq!(reviewer.get("/versions"), (200, json!(["2022-11-28"])));
    let schema = reviewer.graphql(INTROSPECT, json!({"name": "Mutation"}));
    assert_eq!(mutation_names(&schema), MUTATIONS.to_vec());
    let other = reviewer.graphql(INTROSPECT, json!({"name": "Query"}));
    assert_eq!(other["data"]["__type"], Value::Null);
    assert_eq!(server.count("GET /versions"), 1);
    assert_eq!(server.count("graphql __type"), 2);
}

#[test]
fn removed_mutation_is_absent_from_schema_and_undefined() {
    let server = fake();
    server.with(|github| github.remove_mutation("addPullRequestReviewThread"));
    let reviewer = Client::new(&server, "tok-reviewer");
    let schema = reviewer.graphql(INTROSPECT, json!({"name": "Mutation"}));
    let names = mutation_names(&schema);
    assert!(
        !names
            .iter()
            .any(|name| name == "addPullRequestReviewThread")
    );
    assert_eq!(names.len(), MUTATIONS.len() - 1);
    let (_, review) = reviewer.post(REVIEWS, json!({}));
    let call = reviewer.graphql(
        ADD_THREAD,
        json!({"input": {
            "pullRequestReviewId": review["node_id"],
            "path": "src/a.rs",
            "line": 2,
            "body": "x",
        }}),
    );
    assert_eq!(call["errors"][0]["type"], "undefinedField");
    assert!(call.get("data").is_none());
    assert!(server.with(|github| github.comments(PULL)).is_empty());
}

#[test]
fn applied_write_with_lost_response_times_out_and_keeps_state() {
    let server = fake();
    server.inject(
        Fault::new(
            format!("POST {}", Route::Reviews(PULL).template()),
            Reply::Silence,
        )
        .after_apply(),
    );
    let reviewer = Client::with_timeout(&server, "tok-reviewer", Duration::from_millis(300));
    let request = Request::new(Method::Post, format!("{}{REVIEWS}", server.base()))
        .json(&json!({"event": "COMMENT", "body": "дошло"}));
    let failure = reviewer.http.send(&request).expect_err("ответа нет");
    assert!(failure.may_have_been_sent(), "{failure}");
    server.wait_calls(1, RECEIPT).expect("квитанция сервера");
    assert_eq!(server.parked(), 1);
    let reviews = server.with(|github| github.reviews(PULL));
    assert_eq!(reviews.len(), 1, "запись принята");
    assert_eq!(reviews[0].body, "дошло");
    let log = server.log();
    assert_eq!(
        (log[0].status, log[0].applied, log[0].fault),
        (None, true, Some(Reply::Silence))
    );
    server.release_parked();
    assert_eq!(server.parked(), 0);
    assert!(server.finish().is_ok());
}

#[test]
fn faults_without_apply_skip_and_rate_limit() {
    let server = fake();
    let key = format!("POST {}", Route::Reviews(PULL).template());
    server.inject(Fault::new(&key, Reply::Status(502)).skip(1));
    server.inject(Fault::new("GET /repos/{owner}/{repo}", Reply::RateLimited));
    server.inject(Fault::new("graphql markFileAsViewed", Reply::Silence));
    let reviewer = Client::new(&server, "tok-reviewer");

    let (status, _) = reviewer.post(REVIEWS, json!({"event": "COMMENT", "body": "первый"}));
    assert_eq!(status, 200, "первый вызов пропущен правилом");
    let (status, _) = reviewer.post(REVIEWS, json!({"event": "COMMENT", "body": "второй"}));
    assert_eq!(status, 502);
    assert_eq!(
        server.with(|github| github.reviews(PULL).len()),
        1,
        "запись не применена"
    );

    let (status, body, response) = reviewer.send(Method::Get, "/repos/o/r", None);
    assert_eq!(status, 403);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("rate limit"))
    );
    assert_eq!(response.header("x-ratelimit-remaining"), Some("0"));
    let (_, _, normal) = reviewer.send(Method::Get, "/repos/o/r", None);
    assert!(
        normal
            .header("x-ratelimit-remaining")
            .is_some_and(|left| left != "0")
    );
    assert_eq!(normal.header("x-ratelimit-resource"), Some("core"));

    let unconsumed = server.finish().expect_err("правило молчания не исполнено");
    assert!(
        unconsumed.contains("graphql markFileAsViewed"),
        "{unconsumed}"
    );
    assert_eq!(server.count(&key), 2);
    assert_eq!(server.counts().get("GET /repos/{owner}/{repo}"), Some(&2));
}

#[test]
fn silence_on_read_is_a_timeout() {
    let server = fake();
    server.inject(Fault::new(
        "GET /repos/{owner}/{repo}/pulls/{number}",
        Reply::Silence,
    ));
    let reviewer = Client::with_timeout(&server, "tok-reviewer", Duration::from_millis(300));
    let failure = reviewer
        .http
        .send(&Request::get(format!(
            "{}/repos/o/r/pulls/7",
            server.base()
        )))
        .expect_err("молчание");
    assert_eq!(failure.kind, FailureKind::Timeout, "{failure}");
    server.wait_calls(1, RECEIPT).expect("квитанция сервера");
    assert_eq!(server.parked(), 1);
    assert!(!server.log()[0].applied);
}

#[test]
fn paging_caps_per_page_at_100() {
    let server = fake();
    server.with(|github| {
        for n in 0..105u32 {
            let login = format!("user{n}");
            github.add_user(&login, &format!("tok-{n}"), Permission::Write);
            github.insert_review(
                PULL,
                &login,
                "x",
                Some(ReviewState::Commented),
                "head1".to_owned(),
                Origin::Api,
            );
        }
    });
    let reviewer = Client::new(&server, "tok-reviewer");
    let (_, page, response) = reviewer.send(Method::Get, &format!("{REVIEWS}?per_page=500"), None);
    assert_eq!(page.as_array().map(Vec::len), Some(100));
    assert!(
        response
            .header("link")
            .is_some_and(|link| link.contains("per_page=100&page=2"))
    );
    let (_, default) = reviewer.get(REVIEWS);
    assert_eq!(default.as_array().map(Vec::len), Some(30));
    let (_, last, response) =
        reviewer.send(Method::Get, &format!("{REVIEWS}?per_page=100&page=2"), None);
    assert_eq!(last.as_array().map(Vec::len), Some(5));
    assert!(
        response
            .header("link")
            .is_some_and(|link| !link.contains("rel=\"next\""))
    );
}

#[test]
fn unrouted_requests_are_404_and_counted_separately() {
    let server = fake();
    let reviewer = Client::new(&server, "tok-reviewer");
    let (status, _) = reviewer.post("/repos/o/r/pulls/7/comments", json!({"body": "x"}));
    assert_eq!(status, 404, "REST-комментарий вне PENDING не моделируется");
    assert_eq!(reviewer.get("/nowhere").0, 404);
    assert_eq!(server.count("unrouted GET"), 1);
    assert_eq!(
        server.count("POST /repos/{owner}/{repo}/pulls/{number}/comments"),
        1
    );
}

#[test]
fn snapshot_has_origin_and_states() {
    let server = fake();
    server
        .with(|github| github.plant_pending(PULL, REVIEWER, "b", &[spec(2, "c")]))
        .expect("подложен");
    let snapshot = server.with(|github| github.snapshot());
    let pull = &snapshot["pulls"][0];
    assert_eq!(pull["reviews"][0]["state"], "PENDING");
    assert_eq!(pull["reviews"][0]["origin"], "planted");
    assert_eq!(pull["comments"][0]["side"], "RIGHT");
    assert_eq!(pull["commits"], json!(["head1"]));
}

#[test]
fn hunk_scanner_counts_both_sides() {
    let lines = FileLines::scan(PATCH);
    assert_eq!(lines.hunk(DiffSide::Left, 2), Some(1));
    assert_eq!(lines.hunk(DiffSide::Right, 2), Some(1));
    assert_eq!(lines.hunk(DiffSide::Right, 4), Some(1));
    assert_eq!(lines.hunk(DiffSide::Left, 5), None);
    assert_eq!(lines.hunk(DiffSide::Right, 11), Some(2));
    assert_eq!(lines.hunk(DiffSide::Left, 11), Some(2));
    assert_eq!(lines.hunk(DiffSide::Right, 12), Some(2));
    assert_eq!(lines.hunk(DiffSide::Left, 12), None);
}

const NODE: &str = "query($id: ID!) { node(id: $id) { __typename id } }";

#[test]
fn node_reads_review_thread_and_pull_threads_with_visibility() {
    let server = fake();
    let (review_id, ids) = server
        .with(|github| {
            github.plant_pending(PULL, REVIEWER, "итог", &[spec(2, "раз"), spec(3, "два")])
        })
        .expect("подложен");
    let (review_node, thread_node) = server.with(|github| {
        (
            github.reviews(PULL)[0].node_id.clone(),
            github.threads(PULL)[0].node_id.clone(),
        )
    });
    let reviewer = Client::new(&server, "tok-reviewer");
    let author = Client::new(&server, "tok-author");

    let review = reviewer.graphql(NODE, json!({"id": review_node}));
    let review = &review["data"]["node"];
    assert_eq!(review["__typename"], "PullRequestReview");
    assert_eq!(review["databaseId"].as_u64(), Some(review_id));
    assert_eq!(review["state"], "PENDING");
    assert_eq!(review["body"], "итог");
    assert_eq!(review["commit"]["oid"], "head1");
    assert_eq!(review["comments"]["totalCount"], 2);
    assert_eq!(
        review["comments"]["nodes"][1]["fullDatabaseId"].as_str(),
        Some(ids[1].to_string().as_str())
    );
    assert_eq!(review["comments"]["nodes"][1]["state"], "PENDING");
    assert_eq!(review["comments"]["pageInfo"]["hasNextPage"], false);

    let thread = reviewer.graphql(NODE, json!({"id": thread_node}));
    let thread = &thread["data"]["node"];
    assert_eq!(thread["__typename"], "PullRequestReviewThread");
    assert_eq!(thread["isResolved"], false);
    assert_eq!(thread["path"], "src/a.rs");
    assert_eq!(thread["line"], 2);
    assert_eq!(thread["diffSide"], "RIGHT");
    assert_eq!(
        thread["comments"]["nodes"][0]["pullRequestReview"]["databaseId"].as_u64(),
        Some(review_id)
    );

    let pull = reviewer.graphql(NODE, json!({"id": "PR_fake7"}));
    let pull = &pull["data"]["node"];
    assert_eq!(pull["__typename"], "PullRequest");
    assert_eq!(pull["headRefOid"], "head1");
    assert_eq!(pull["reviewThreads"]["totalCount"], 2);
    assert_eq!(
        pull["reviewThreads"]["nodes"][0]["id"],
        thread_node.as_str()
    );

    for id in [review_node.as_str(), thread_node.as_str()] {
        let hidden = author.graphql(NODE, json!({"id": id}));
        assert_eq!(hidden["errors"][0]["type"], "NOT_FOUND", "{id}");
        assert_eq!(hidden["data"]["node"], Value::Null);
    }
    let theirs = author.graphql(NODE, json!({"id": "PR_fake7"}));
    assert_eq!(theirs["data"]["node"]["reviewThreads"]["totalCount"], 0);

    let (status, _) = reviewer.post(
        &format!("{REVIEWS}/{review_id}/events"),
        json!({"event": "COMMENT", "body": "итог"}),
    );
    assert_eq!(status, 200);
    let published = author.graphql(NODE, json!({"id": "PR_fake7"}));
    assert_eq!(published["data"]["node"]["reviewThreads"]["totalCount"], 2);
    let thread = author.graphql(NODE, json!({"id": thread_node}));
    assert_eq!(
        thread["data"]["node"]["comments"]["nodes"][0]["state"],
        "SUBMITTED"
    );
}

#[test]
fn server_error_after_apply_keeps_the_write() {
    let server = fake();
    let key = format!("POST {}", Route::Reviews(PULL).template());
    server.inject(Fault::new(&key, Reply::Status(502)).after_apply());
    let reviewer = Client::new(&server, "tok-reviewer");
    let (status, body) = reviewer.post(REVIEWS, json!({"event": "COMMENT", "body": "принято"}));
    assert_eq!(status, 502, "{body}");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("injected 502"))
    );
    let reviews = server.with(|github| github.reviews(PULL));
    assert_eq!(reviews.len(), 1, "запись применена до ответа 502");
    assert_eq!(reviews[0].state, ReviewState::Commented);
    let log = server.log();
    assert_eq!(
        (log[0].status, log[0].applied, log[0].fault),
        (Some(502), true, Some(Reply::Status(502)))
    );
    assert!(server.finish().is_ok());
}

#[test]
fn graphql_fault_rule_fires_by_field_key() {
    let server = fake();
    let (_, pending) = Client::new(&server, "tok-reviewer").post(REVIEWS, json!({}));
    let review_node = pending["node_id"].as_str().expect("node_id").to_owned();
    server.inject(Fault::new(
        "graphql addPullRequestReviewThread",
        Reply::Status(503),
    ));
    server
        .inject(Fault::new("graphql addPullRequestReviewThread", Reply::Status(500)).after_apply());
    let reviewer = Client::new(&server, "tok-reviewer");
    let input = json!({"input": {"pullRequestReviewId": review_node, "path": "src/a.rs", "line": 2, "body": "x"}});
    let (status, _) = reviewer.post(
        "/graphql",
        json!({"query": ADD_THREAD, "variables": input.clone()}),
    );
    assert_eq!(status, 503);
    assert!(
        server.with(|github| github.comments(PULL).is_empty()),
        "запись не применена"
    );
    let (status, _) = reviewer.post("/graphql", json!({"query": ADD_THREAD, "variables": input}));
    assert_eq!(status, 500);
    assert_eq!(
        server.with(|github| github.comments(PULL).len()),
        1,
        "запись применена до ответа 500"
    );
    assert_eq!(server.count("graphql addPullRequestReviewThread"), 2);
    assert!(server.finish().is_ok());
}

#[test]
fn receipt_times_out_without_calls() {
    let server = fake();
    let err = server
        .wait_calls(1, Duration::from_millis(50))
        .expect_err("вызовов не было");
    assert!(err.contains("0 из 1"), "{err}");
    Client::new(&server, "tok-reviewer").get("/user");
    server.wait_calls(1, RECEIPT).expect("вызов записан");
}
