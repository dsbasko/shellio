use serde_json::json;

use super::scripted::{Reply, Scripted};
use super::*;
use crate::scrub::{LABEL_AUTHOR, LABEL_TOKEN};

const BASE: &str = "https://api.github.com";

fn post(path: &str, body: &Value) -> Request {
    Request::new(Method::Post, format!("{BASE}{path}")).json(body)
}

#[test]
fn scripted_returns_responses_in_order_and_records_requests() {
    let transport = Scripted::new()
        .expect(
            Method::Post,
            "/repos/o/r/pulls/1/reviews",
            Reply::json(200, &json!({"id": 11, "state": "PENDING"})),
        )
        .expect(
            Method::Get,
            "/repos/o/r/pulls/1/reviews?per_page=100&page=2",
            Reply::json(200, &json!([])),
        )
        .then(Reply::status(404));

    let first = transport
        .send(&post("/repos/o/r/pulls/1/reviews", &json!({"body": "x"})))
        .expect("ответ");
    assert_eq!(first.status, 200);
    assert_eq!(first.body_json(), Ok(json!({"id": 11, "state": "PENDING"})));
    assert_eq!(first.header("Content-Type"), Some("application/json"));

    let second = transport
        .send(&Request::get(format!(
            "{BASE}/repos/o/r/pulls/1/reviews?per_page=100&page=2"
        )))
        .expect("ответ");
    assert_eq!(second.body_json(), Ok(json!([])));

    let third = transport
        .send(&Request::new(Method::Delete, format!("{BASE}/anything")))
        .expect("ответ");
    assert_eq!(third.status, 404);

    let requests = transport.finish().expect("сценарий исполнен");
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].body_json(), Some(json!({"body": "x"})));
    assert_eq!(
        requests[1].path(),
        "/repos/o/r/pulls/1/reviews?per_page=100&page=2"
    );
}

#[test]
fn scripted_path_without_query_ignores_query() {
    let transport = Scripted::new().expect(Method::Get, "/a", Reply::status(200));
    transport
        .send(&Request::get(format!("{BASE}/a?page=3")))
        .expect("ответ");
    assert!(transport.finish().is_ok());

    let transport = Scripted::new().expect(Method::Get, "/a?page=2", Reply::status(200));
    let failure = transport
        .send(&Request::get(format!("{BASE}/a?page=3")))
        .expect_err("другая страница");
    assert_eq!(failure.kind, FailureKind::NotSent);
    assert!(transport.finish().is_err());
}

#[test]
fn scripted_timeout_and_disconnect_after_send_reach_server() {
    let transport = Scripted::new()
        .then(Reply::timeout())
        .then(Reply::disconnect())
        .then(Reply::not_sent());

    let timeout = transport
        .send(&post("/x", &json!({"n": 1})))
        .expect_err("таймаут");
    assert_eq!(timeout.kind, FailureKind::Timeout);
    assert!(timeout.may_have_been_sent());

    let dropped = transport
        .send(&post("/x", &json!({"n": 2})))
        .expect_err("обрыв");
    assert_eq!(dropped.kind, FailureKind::Disconnected);
    assert!(dropped.may_have_been_sent());

    let refused = transport
        .send(&post("/x", &json!({"n": 3})))
        .expect_err("не ушёл");
    assert!(!refused.may_have_been_sent());

    let delivered: Vec<Value> = transport
        .delivered()
        .iter()
        .filter_map(Request::body_json)
        .collect();
    assert_eq!(delivered, vec![json!({"n": 1}), json!({"n": 2})]);
    assert_eq!(transport.requests().len(), 3);
    assert!(transport.finish().is_ok());
}

#[test]
fn scripted_unexpected_request_is_violation_and_step_is_kept() {
    let transport = Scripted::new().expect(Method::Put, "/publish", Reply::status(204));
    let failure = transport
        .send(&post("/bulk_publish", &json!({})))
        .expect_err("неожиданный");
    assert_eq!(failure.kind, FailureKind::NotSent);
    assert_eq!(transport.remaining(), 1);
    let problems = transport.finish().expect_err("нарушение");
    assert!(
        problems.contains("неожиданный запрос POST /bulk_publish"),
        "{problems}"
    );
    assert!(
        problems.contains("шаг не исполнен: PUT /publish"),
        "{problems}"
    );
    assert!(transport.delivered().is_empty());
}

#[test]
fn scripted_exhausted_and_unconsumed_steps_fail_finish() {
    let transport = Scripted::new();
    assert!(transport.send(&Request::get(format!("{BASE}/x"))).is_err());
    assert!(transport.finish().is_err());

    let transport = Scripted::new().then(Reply::status(200));
    assert!(transport.finish().is_err());
}

#[test]
fn trace_records_method_path_bodies_status_rate_limits_and_failures() {
    let scripted = Scripted::new()
        .then(Reply::Respond(
            Response::json(201, &json!({"id": 5, "user": {"login": "alice"}}))
                .with_header("X-RateLimit-Remaining", "4998")
                .with_header("x-ratelimit-reset", "1700000000")
                .with_header("RateLimit-Observed", "3")
                .with_header("Retry-After", "60")
                .with_header("link", "<https://api.github.com/x?page=2>; rel=\"next\"")
                .with_header("set-cookie", "session=abc"),
        ))
        .then(Reply::timeout());
    let trace = Trace::new();
    let transport = Traced::new(scripted, trace.clone(), "reviewer");

    transport
        .send(
            &post(
                "/repos/o/r/pulls/1/reviews?x=1",
                &json!({"event": "COMMENT"}),
            )
            .header("authorization", "Bearer ghp_shouldnotappear"),
        )
        .expect("ответ");
    transport
        .send(&Request::get(format!("{BASE}/repos/o/r/pulls/1/reviews")))
        .expect_err("таймаут");

    assert_eq!(trace.len(), 2);
    let mut scrubber = Scrubber::new();
    scrubber.add_login("alice", LABEL_AUTHOR);
    let entries = trace.entries(&scrubber);

    let first = &entries[0];
    assert_eq!(first.seq, 1);
    assert_eq!(first.actor, "reviewer");
    assert_eq!(first.method, "POST");
    assert_eq!(first.path, "/repos/o/r/pulls/1/reviews?x=1");
    assert_eq!(first.request_body, json!({"event": "COMMENT"}));
    assert_eq!(first.status, Some(201));
    assert_eq!(first.failure, None);
    assert_eq!(
        first.response_body,
        json!({"id": 5, "user": {"login": "<author>"}})
    );
    let names: Vec<&str> = first.rate_limit.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        vec![
            "ratelimit-observed",
            "retry-after",
            "x-ratelimit-remaining",
            "x-ratelimit-reset"
        ]
    );

    let second = &entries[1];
    assert_eq!(second.seq, 2);
    assert_eq!(second.method, "GET");
    assert_eq!(second.request_body, Value::Null);
    assert_eq!(second.status, None);
    assert_eq!(second.failure.as_deref(), Some("timeout"));
    assert!(second.failure_detail.is_some());
    assert_eq!(second.response_body, Value::Null);

    let serialized = serde_json::to_string(&entries).expect("JSON");
    assert!(!serialized.contains("ghp_"), "{serialized}");
    assert!(!serialized.contains("session=abc"), "{serialized}");
    assert!(!serialized.contains("link"), "{serialized}");

    let back: Vec<TraceEntry> = serde_json::from_str(&serialized).expect("чтение");
    assert_eq!(back, entries);
}

#[test]
fn trace_is_shared_between_actors_and_counts_calls() {
    let trace = Trace::new();
    let author = Traced::new(
        Scripted::new().then(Reply::status(200)),
        trace.clone(),
        "author",
    );
    let reviewer = Traced::new(
        Scripted::new()
            .then(Reply::status(201))
            .then(Reply::status(201)),
        trace.clone(),
        "reviewer",
    );
    let drafts = "/projects/alice%2Fsandbox/merge_requests/1/draft_notes";
    reviewer
        .send(&post(&format!("{drafts}?page=1"), &json!({"note": "a"})))
        .expect("ответ");
    author
        .send(&Request::get(format!("{BASE}/repos/Alice/sandbox/pulls/1")))
        .expect("ответ");
    reviewer
        .send(&post(&format!("{drafts}?page=2"), &json!({"note": "b"})))
        .expect("ответ");

    let calls = trace.calls();
    let actors: Vec<(u64, &str)> = calls.iter().map(|c| (c.seq, c.actor)).collect();
    assert_eq!(
        actors,
        vec![(1, "reviewer"), (2, "author"), (3, "reviewer")]
    );
    assert_eq!(calls[2].status(), Some(201));
    let mut scrubber = Scrubber::new();
    scrubber.add_repo("alice/sandbox");
    let counts = trace.counts(&scrubber);
    let keys: Vec<&str> = counts.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "GET /repos/<repo>/pulls/1",
            "POST /projects/<repo>/merge_requests/1/draft_notes"
        ]
    );
    assert_eq!(
        counts.get("POST /projects/<repo>/merge_requests/1/draft_notes"),
        Some(&2)
    );
}

#[test]
fn trace_scrubs_failure_detail_and_non_json_bodies() {
    let scripted = Scripted::new().then(Reply::Fail(Failure::disconnected(
        "сброс при чтении alice/repo",
    )));
    let trace = Trace::new();
    let transport = Traced::new(scripted, trace.clone(), "root");
    let request = Request::new(Method::Post, "http://127.0.0.1:8929/oauth/token").form(&[
        ("grant_type", "password"),
        ("username", "root"),
        ("password", "p@ss w0rd"),
    ]);
    assert_eq!(
        request.body.as_deref(),
        Some(&b"grant_type=password&username=root&password=p%40ss%20w0rd"[..])
    );
    transport.send(&request).expect_err("обрыв");

    let mut scrubber = Scrubber::new();
    scrubber
        .add_repo("alice/repo")
        .add_gitlab_ce("127.0.0.1:8929");
    let entry = &trace.entries(&scrubber)[0];
    assert_eq!(entry.path, "/oauth/token");
    assert_eq!(
        entry.request_body,
        Value::String(format!(
            "grant_type=password&username=root&password={LABEL_TOKEN}"
        ))
    );
    assert_eq!(
        entry.failure_detail.as_deref(),
        Some("сброс при чтении <repo>")
    );
    assert_eq!(entry.failure.as_deref(), Some("disconnected"));
}

#[test]
fn request_debug_redacts_secret_headers() {
    let request = Request::get(format!("{BASE}/user"))
        .header("Authorization", "Bearer ghp_visible")
        .header("PRIVATE-TOKEN", "glpat-visible")
        .header("accept", "application/json");
    let shown = format!("{request:?}");
    assert!(!shown.contains("ghp_visible"), "{shown}");
    assert!(!shown.contains("glpat-visible"), "{shown}");
    assert!(shown.contains("application/json"), "{shown}");
}

#[test]
fn path_of_strips_scheme_and_host() {
    assert_eq!(path_of("https://gitlab.com/api/v4/x?y=1"), "/api/v4/x?y=1");
    assert_eq!(path_of("http://127.0.0.1:1234"), "/");
    assert_eq!(path_of("http://127.0.0.1:1234?q=1"), "?q=1");
    assert_eq!(path_of("/already/path"), "/already/path");
}

#[test]
fn methods_and_failure_kinds() {
    assert!(!Method::Get.is_write());
    assert!(Method::Post.is_write() && Method::Put.is_write());
    assert!(Method::Patch.is_write() && Method::Delete.is_write());
    let failure = Failure::disconnected("x").with_status(201);
    assert_eq!(failure.to_string(), "disconnected: x (код 201)");
    assert!(failure.may_have_been_sent());
    assert!(!Failure::not_sent("y").may_have_been_sent());
}
