use serde_json::{Value, json};

use super::*;
use crate::fake::FakeServer;
use crate::fake::gitlab::{
    Access, BULK_PUBLISH, FakeDiff, GitlabFake, NOTES, PUBLISH, REQUEST_CHANGES as RC_KEY,
    RequestChangesMode, ReviewerState as FakeState,
};
use crate::model::anchor::{Anchor, LinePoint, Side};
use crate::model::{DraftId, Placement};
use crate::transport::http::{Credential, HttpConfig, HttpTransport, Secret};
use crate::transport::scripted::{Reply, Scripted};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const ROOT: &str = "https://gitlab.test";
const BASE: &str = "1111111111111111111111111111111111111111";
const START: &str = "4444444444444444444444444444444444444444";
const HEAD: &str = "2222222222222222222222222222222222222222";
const NEW_HEAD: &str = "3333333333333333333333333333333333333333";
const PROJECT: &str = "group/sub/proj";
const PATH: &str = "src/a.rs";
const DIFF: &str = "@@ -1,4 +1,4 @@\n ctx1\n-old2\n+new2\n ctx3\n ctx4\n@@ -10,2 +10,3 @@\n ctx10\n+add11\n ctx11\n";
const MR: &str = "/api/v4/projects/group%2Fsub%2Fproj/merge_requests/3";
const GRAPHQL: &str = "/api/graphql";

fn target() -> ReviewTarget {
    ReviewTarget {
        provider: Provider::GitLab,
        host: "gitlab.test".to_owned(),
        repo: PROJECT.to_owned(),
        number: 3,
    }
}

fn revision(head: &str) -> Result<Revision, ModelError> {
    Ok(Revision::new(
        Sha::parse(BASE)?,
        Sha::parse(START)?,
        Sha::parse(head)?,
    ))
}

fn session_on(head: &str) -> Result<ReviewSession, ModelError> {
    Ok(ReviewSession::new("bob", target(), revision(head)?))
}

fn file() -> Result<FileDiff, ModelError> {
    FileDiff::parse(PATH, PATH, DIFF)
}

/// Три черновика: добавленная строка (новая сторона), удалённая (старая), диапазон.
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

fn code(old: u32, new: u32) -> String {
    format!("{}_{old}_{new}", sha1_smol::Sha1::from(PATH).digest())
}

/// Позиции трёх черновиков [`drafts`] в форме API, тройка — ревизия на `head`.
fn positions(head: &str) -> [Value; 3] {
    let base = json!({
        "base_sha": BASE, "start_sha": START, "head_sha": head,
        "position_type": "text", "old_path": PATH, "new_path": PATH,
    });
    let with = |extra: Value| {
        let mut value = base.clone();
        if let (Some(target), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
            target.extend(extra.clone());
        }
        value
    };
    [
        with(json!({"new_line": 2})),
        with(json!({"old_line": 2})),
        with(json!({
            "old_line": 3, "new_line": 3,
            "line_range": {
                "start": {"line_code": code(1, 1), "old_line": 1, "new_line": 1},
                "end": {"line_code": code(3, 3), "old_line": 3, "new_line": 3},
            },
        })),
    ]
}

fn caps() -> Capabilities {
    Capabilities {
        username: "bob".to_owned(),
        host: HostVersion {
            version: "18.5.1".to_owned(),
            revision: None,
            enterprise: Some(false),
            source: "metadata",
        },
        plan: None,
        plan_source: None,
        visibility: "private".to_owned(),
        access_level: Some(DEVELOPER),
        mutations: capabilities::MUTATIONS
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        introspection: capabilities::FORM_TYPE,
    }
}

fn adapter(scripted: &Scripted) -> Result<GitLab<&Scripted>, ApiError> {
    GitLab::new(scripted, Endpoints::instance(ROOT), &target())
}

/// Ответ сервера на черновик: позиция в форме ответа (отсутствующие строки — `null`).
fn draft_reply(status: u16, id: u64, note: &str, position: Option<&Value>) -> Reply {
    let position = position.map_or_else(
        || {
            json!({"base_sha": null, "start_sha": null, "head_sha": null, "old_path": null,
                   "new_path": null, "position_type": "text", "old_line": null,
                   "new_line": null, "line_range": null})
        },
        |sent| {
            let mut echoed = sent.clone();
            for name in ["old_line", "new_line", "line_range"] {
                if echoed.get(name).is_none() {
                    echoed[name] = Value::Null;
                }
            }
            echoed
        },
    );
    Reply::json(
        status,
        &json!({
            "id": id, "author_id": 7, "merge_request_id": 77, "resolve_discussion": false,
            "discussion_id": null, "note": note, "commit_id": null, "line_code": null,
            "position": position,
        }),
    )
}

fn no_content() -> Reply {
    Reply::status(204)
}

fn page(items: Value, next: &str) -> Reply {
    Reply::Respond(Response::json(200, &items).with_header("x-next-page", next))
}

fn rc_ok() -> Reply {
    Reply::json(
        200,
        &json!({"data": {"mergeRequestRequestChanges": {"errors": []}}}),
    )
}

fn reviewers_reply(state: &str) -> Reply {
    Reply::json(
        200,
        &json!({"data": {"project": {"mergeRequest": {"reviewers": {"nodes": [
            {"username": "carol", "mergeRequestInteraction": {"reviewState": "APPROVED"}},
            {"username": "bob", "mergeRequestInteraction": {"reviewState": state}},
        ]}}}}}),
    )
}

fn no_notes(requests: &[Request]) -> bool {
    requests
        .iter()
        .all(|request| !request.path().contains("/notes"))
}

#[test]
fn capabilities_are_read_from_the_host() -> TestResult {
    let scripted = Scripted::new()
        .expect(
            Method::Get,
            "/api/v4/user",
            Reply::json(200, &json!({"id": 5, "username": "bob", "name": "Bob B"})),
        )
        .expect(
            Method::Get,
            "/api/v4/metadata",
            Reply::json(
                200,
                &json!({"version": "18.6.0-ee", "revision": "abc123", "enterprise": true}),
            ),
        )
        .expect(
            Method::Get,
            "/api/v4/projects/group%2Fsub%2Fproj",
            Reply::json(
                200,
                &json!({
                    "visibility": "private",
                    "namespace": {"id": 9, "kind": "group"},
                    "permissions": {
                        "project_access": {"access_level": 30},
                        "group_access": {"access_level": 40},
                    },
                }),
            ),
        )
        .expect(
            Method::Get,
            "/api/v4/namespaces/9",
            Reply::json(200, &json!({"id": 9, "plan": "premium"})),
        )
        .expect(
            Method::Post,
            GRAPHQL,
            Reply::json(
                200,
                &json!({"data": {"__type": {"fields": [
                    {"name": "createNote"}, {"name": "mergeRequestRequestChanges"},
                ]}}}),
            ),
        );
    let caps = adapter(&scripted)?.capabilities()?;
    assert_eq!(caps.username, "bob");
    assert_eq!(
        caps.host,
        HostVersion {
            version: "18.6.0-ee".to_owned(),
            revision: Some("abc123".to_owned()),
            enterprise: Some(true),
            source: "metadata",
        }
    );
    assert_eq!(caps.plan.as_deref(), Some("premium"));
    assert_eq!(caps.access_level, Some(40), "большая из двух ролей");
    assert!(caps.private() && caps.can_review() && caps.request_changes());
    assert_eq!(caps.mutations, capabilities::MUTATIONS.to_vec());
    let json = caps.to_json();
    assert_eq!(json["request_changes"], true);
    assert!(json.get("name").is_none(), "имя профиля не записывается");
    let requests = scripted.finish()?;
    for request in &requests[..4] {
        assert!(request.url.starts_with(&format!("{ROOT}/api/v4/")));
    }
    assert_eq!(requests[4].url, format!("{ROOT}{GRAPHQL}"));
    assert_eq!(
        requests[4].body_json(),
        Some(json!({"query": capabilities::INTROSPECT, "variables": {"name": "Mutation"}}))
    );
    Ok(())
}

#[test]
fn old_version_hidden_plan_and_missing_rights_are_recorded_and_gate_writes() -> TestResult {
    let scripted = Scripted::new()
        .expect(
            Method::Get,
            "/api/v4/user",
            Reply::json(200, &json!({"username": "rita"})),
        )
        .expect(
            Method::Get,
            "/api/v4/metadata",
            Reply::json(404, &json!({"message": "404 Not Found"})),
        )
        .expect(
            Method::Get,
            "/api/v4/version",
            Reply::json(200, &json!({"version": "15.1.0", "revision": "def"})),
        )
        .expect(
            Method::Get,
            "/api/v4/projects/group%2Fsub%2Fproj",
            Reply::json(
                200,
                &json!({
                    "visibility": "internal",
                    "namespace": {"id": 9},
                    "permissions": {"project_access": null, "group_access": {"access_level": 20}},
                }),
            ),
        )
        .expect(
            Method::Get,
            "/api/v4/namespaces/9",
            Reply::json(403, &json!({"message": "403 Forbidden"})),
        )
        .expect(
            Method::Post,
            GRAPHQL,
            Reply::json(
                200,
                &json!({"data": {"__type": {"fields": [{"name": "createNote"}]}}}),
            ),
        );
    let caps = adapter(&scripted)?.capabilities()?;
    assert_eq!(caps.host.source, "version");
    assert_eq!(caps.host.enterprise, None);
    assert_eq!(caps.plan, None);
    assert_eq!(caps.access_level, Some(20));
    assert!(!caps.private() && !caps.can_review() && !caps.request_changes());
    scripted.finish()?;

    let idle = Scripted::new();
    let gitlab = adapter(&idle)?;
    let absent = gitlab.request_changes(&caps);
    assert_eq!(absent, Err(ApiError::Absent(capabilities::REQUEST_CHANGES)));
    let reporter = Capabilities {
        mutations: capabilities::MUTATIONS
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        ..caps
    };
    let refused = gitlab.request_changes(&reporter);
    assert_eq!(
        refused,
        Err(ApiError::NotPermitted {
            access_level: Some(20),
            required: DEVELOPER,
        })
    );
    for err in [absent, refused] {
        assert_eq!(
            err.err().map(|e| e.write_fate()),
            Some(WriteFate::NotAccepted)
        );
    }
    assert!(idle.finish()?.is_empty(), "ни мутации, ни заметки");
    Ok(())
}

/// Полная интроспекция, как у GitLab CE 19.6.1 на `__type(name: $name)`: `Mutation` среди
/// `filler` посторонних типов.
fn full_schema(mutation_fields: &[&str], filler: usize) -> Value {
    let mut types: Vec<Value> = (0..filler)
        .map(|n| json!({"kind": "OBJECT", "name": format!("Type{n}"), "fields": [{"name": "id"}]}))
        .collect();
    types.push(json!({
        "kind": "OBJECT",
        "name": "Mutation",
        "fields": mutation_fields.iter().map(|name| json!({"name": name})).collect::<Vec<_>>(),
    }));
    json!({"data": {"__schema": {
        "queryType": {"name": "Query"},
        "mutationType": {"name": "Mutation"},
        "types": types,
    }}})
}

fn capability_reads(metadata: Value, namespace: Reply, introspection: Value) -> Scripted {
    Scripted::new()
        .expect(
            Method::Get,
            "/api/v4/user",
            Reply::json(200, &json!({"username": "bob"})),
        )
        .expect(Method::Get, "/api/v4/metadata", Reply::json(200, &metadata))
        .expect(
            Method::Get,
            "/api/v4/projects/group%2Fsub%2Fproj",
            Reply::json(
                200,
                &json!({
                    "visibility": "private",
                    "namespace": {"id": 9},
                    "permissions": {"project_access": {"access_level": 30}},
                }),
            ),
        )
        .expect(Method::Get, "/api/v4/namespaces/9", namespace)
        .expect(Method::Post, GRAPHQL, Reply::json(200, &introspection))
}

#[test]
fn full_schema_reply_gives_mutations_and_plan_falls_back_to_metadata() -> TestResult {
    let reply = full_schema(&["createNote", "mergeRequestRequestChanges"], 3000);
    let raw = reply.to_string().len();
    let scripted = capability_reads(
        json!({"version": "19.6.1", "revision": "r", "enterprise": false, "plan": "free"}),
        Reply::json(404, &json!({"message": "404 Namespace Not Found"})),
        reply,
    );
    let trace = crate::transport::Trace::new();
    let traced = crate::transport::Traced::new(&scripted, trace.clone(), "reviewer");
    let gitlab = GitLab::new(traced, Endpoints::instance(ROOT), &target())?;
    let caps = gitlab.capabilities()?;
    assert_eq!(caps.introspection, capabilities::FORM_SCHEMA);
    assert!(caps.request_changes());
    assert_eq!(caps.plan.as_deref(), Some("free"));
    assert_eq!(caps.plan_source, Some("metadata"));
    let json = caps.to_json();
    assert_eq!(json["plan_source"], "metadata");
    assert_eq!(json["introspection"], "__schema");
    scripted.finish()?;

    let entries = trace.entries(&crate::scrub::Scrubber::new());
    let graphql = entries.last().ok_or("нет записи GraphQL")?;
    let summary = &graphql.response_body;
    assert_eq!(summary["summary"], "introspection __schema");
    assert_eq!(summary["bytes"], raw);
    assert_eq!(summary["keys"], json!(["data"]));
    assert_eq!(summary["types"], 3001);
    assert_eq!(summary["mutation_type"], "Mutation");
    assert_eq!(summary["mutation_fields"], 2);
    assert_eq!(summary["mutations"]["mergeRequestRequestChanges"], true);
    assert_eq!(summary["mutations"]["addPullRequestReviewThread"], false);
    let written = serde_json::to_string(graphql)?.len();
    assert!(
        written < 2_000 && raw > 100_000,
        "сводка {written} байт вместо тела {raw} байт"
    );
    let user = &entries[0].response_body;
    assert_eq!(user["username"], "bob", "короткие ответы идут телом");
    Ok(())
}

#[test]
fn full_schema_without_the_mutation_is_absent_and_namespace_plan_wins() -> TestResult {
    let scripted = capability_reads(
        json!({"version": "19.6.1", "enterprise": false, "plan": "free"}),
        Reply::json(200, &json!({"id": 9, "plan": "premium"})),
        full_schema(&["createNote"], 2),
    );
    let caps = adapter(&scripted)?.capabilities()?;
    assert_eq!(caps.introspection, capabilities::FORM_SCHEMA);
    assert!(!caps.request_changes());
    assert_eq!(caps.plan.as_deref(), Some("premium"));
    assert_eq!(caps.plan_source, Some("namespace"));
    scripted.finish()?;

    let scripted = capability_reads(
        json!({"version": "19.6.1", "enterprise": false}),
        Reply::json(200, &json!({"id": 9})),
        json!({"data": {"__type": {"fields": [{"name": "mergeRequestRequestChanges"}]}}}),
    );
    let caps = adapter(&scripted)?.capabilities()?;
    assert_eq!(caps.introspection, capabilities::FORM_TYPE);
    assert_eq!((caps.plan, caps.plan_source), (None, None));
    scripted.finish()?;
    Ok(())
}

#[test]
fn introspection_without_type_and_schema_or_with_broken_schema_is_unexpected() -> TestResult {
    let scripted = capability_reads(
        json!({"version": "19.6.1", "enterprise": false}),
        Reply::json(404, &json!({})),
        json!({"data": {}}),
    );
    let err = adapter(&scripted)?.capabilities();
    assert!(
        matches!(&err, Err(ApiError::Unexpected(reason)) if reason.contains("без __type и без __schema")),
        "{err:?}"
    );
    scripted.finish()?;

    let broken = [
        json!({"__type": null}),
        json!({"__schema": {"mutationType": null, "types": []}}),
        json!({"__schema": {"mutationType": {"name": "Mutation"}}}),
        json!({"__schema": {"mutationType": {"name": "Mutation"}, "types": [{"name": "Query"}]}}),
        json!({"__schema": {"mutationType": {"name": "Mutation"}, "types": [{"name": "Mutation"}]}}),
        json!({"__type": {"name": "Mutation"}}),
    ];
    for data in broken {
        let map = data.as_object().ok_or("не объект")?;
        let result = capabilities::mutation_fields(map);
        assert!(
            matches!(result, Err(ApiError::Unexpected(_))),
            "{data}: {result:?}"
        );
    }
    Ok(())
}

#[test]
fn literal_introspection_reports_form_size_and_mutation() -> TestResult {
    let schema = full_schema(&["mergeRequestRequestChanges"], 5);
    let scripted = Scripted::new()
        .expect(Method::Post, GRAPHQL, Reply::json(200, &schema))
        .expect(
            Method::Post,
            GRAPHQL,
            Reply::json(
                200,
                &json!({"data": {"__type": {"fields": [{"name": "x"}]}}}),
            ),
        )
        .expect(
            Method::Post,
            GRAPHQL,
            Reply::json(200, &json!({"errors": [{"message": "boom"}]})),
        );
    let gitlab = adapter(&scripted)?;
    let reply = gitlab.introspect_literal()?;
    assert_eq!(reply.form, capabilities::FORM_SCHEMA);
    assert_eq!(reply.bytes, schema.to_string().len());
    assert_eq!(reply.request_changes, Some(true));
    let reply = gitlab.introspect_literal()?;
    assert_eq!(
        (reply.form, reply.request_changes),
        (capabilities::FORM_TYPE, Some(false))
    );
    let reply = gitlab.introspect_literal()?;
    assert_eq!((reply.form, reply.request_changes), ("errors", None));
    let requests = scripted.finish()?;
    for request in &requests {
        assert_eq!(
            request.body_json(),
            Some(json!({"query": capabilities::INTROSPECT_LITERAL})),
            "литерал без variables"
        );
    }
    Ok(())
}

fn mr_json(head: &str) -> Value {
    json!({
        "id": 77, "iid": 3, "author": {"username": "alice"},
        "reviewers": [{"username": "bob"}],
        "diff_refs": {"base_sha": BASE, "start_sha": START, "head_sha": head},
    })
}

fn version_json(id: u64, head: &str) -> Value {
    json!({"id": id, "base_commit_sha": BASE, "start_commit_sha": START, "head_commit_sha": head})
}

#[test]
fn snapshot_takes_the_diff_of_the_version_with_the_diff_refs_triple() -> TestResult {
    let mut detail = version_json(11, HEAD);
    detail["diffs"] = json!([
        {"old_path": PATH, "new_path": PATH, "diff": DIFF},
        {"old_path": "big.bin", "new_path": "big.bin", "diff": "", "too_large": true},
        {"old_path": "old/b.rs", "new_path": "src/b.rs", "diff": "@@ -1 +1 @@\n-x\n+y\n"},
    ]);
    let scripted = Scripted::new()
        .expect(Method::Get, MR, Reply::json(200, &mr_json(HEAD)))
        .expect(
            Method::Get,
            format!("{MR}/versions?per_page=100&page=1"),
            page(
                json!([version_json(12, NEW_HEAD), version_json(11, HEAD)]),
                "",
            ),
        )
        .expect(
            Method::Get,
            format!("{MR}/versions/11"),
            Reply::json(200, &detail),
        );
    let snapshot = adapter(&scripted)?.merge_request()?;
    assert_eq!(snapshot.revision, revision(HEAD)?);
    assert_eq!(snapshot.version_id, ProviderId::new("11"));
    assert_eq!(snapshot.author, "alice");
    assert_eq!(snapshot.reviewers, ["bob"]);
    let paths: Vec<(&str, &str)> = snapshot
        .files
        .iter()
        .map(|file| (file.old_path(), file.new_path()))
        .collect();
    assert_eq!(paths, [(PATH, PATH), ("old/b.rs", "src/b.rs")]);
    scripted.finish()?;
    Ok(())
}

#[test]
fn snapshot_without_a_version_of_the_triple_is_an_error() -> TestResult {
    let scripted = Scripted::new()
        .expect(Method::Get, MR, Reply::json(200, &mr_json(HEAD)))
        .expect(
            Method::Get,
            format!("{MR}/versions"),
            page(json!([version_json(12, NEW_HEAD)]), ""),
        );
    let missing = adapter(&scripted)?.merge_request();
    assert!(
        matches!(missing, Err(ApiError::Unexpected(_))),
        "{missing:?}"
    );
    scripted.finish()?;

    let mut other = version_json(11, NEW_HEAD);
    other["diffs"] = json!([]);
    let scripted = Scripted::new()
        .expect(Method::Get, MR, Reply::json(200, &mr_json(HEAD)))
        .expect(
            Method::Get,
            format!("{MR}/versions"),
            page(json!([version_json(11, HEAD)]), ""),
        )
        .expect(
            Method::Get,
            format!("{MR}/versions/11"),
            Reply::json(200, &other),
        );
    let mixed = adapter(&scripted)?.merge_request();
    assert!(matches!(mixed, Err(ApiError::Unexpected(_))), "{mixed:?}");
    scripted.finish()?;

    let scripted = Scripted::new().expect(
        Method::Get,
        MR,
        Reply::json(200, &json!({"id": 77, "diff_refs": null})),
    );
    let no_refs = adapter(&scripted)?.merge_request();
    assert!(
        matches!(no_refs, Err(ApiError::Unexpected(_))),
        "{no_refs:?}"
    );
    Ok(())
}

#[test]
fn drafts_carry_the_session_triple_and_their_echo_is_checked() -> TestResult {
    let session = session_on(HEAD)?;
    let drafts = drafts(&session)?;
    let expected = positions(HEAD);
    let scripted = Scripted::new()
        .expect(
            Method::Post,
            format!("{MR}/draft_notes"),
            draft_reply(201, 501, "added", Some(&expected[0])),
        )
        .expect(
            Method::Post,
            format!("{MR}/draft_notes"),
            draft_reply(201, 502, "removed", Some(&expected[1])),
        )
        .expect(
            Method::Post,
            format!("{MR}/draft_notes"),
            draft_reply(201, 503, "range", Some(&expected[2])),
        );
    let gitlab = adapter(&scripted)?;
    let mut known = Vec::new();
    for draft in &drafts {
        let (id, remote) = gitlab.create_draft(&session, draft, false)?;
        assert_eq!(&remote.id, id.id());
        assert_eq!(
            remote.position.as_ref().map(|p| p.revision()),
            Some(Ok(revision(HEAD)?))
        );
        known.push(id);
    }
    let ids: Vec<&str> = known.iter().map(|k| k.id().as_str()).collect();
    assert_eq!(ids, ["501", "502", "503"]);
    let requests = scripted.finish()?;
    let bodies: Vec<Value> = requests
        .iter()
        .map(|request| request.body_json().unwrap_or(Value::Null))
        .collect();
    assert_eq!(
        bodies,
        [
            json!({"note": "added", "position": expected[0]}),
            json!({"note": "removed", "position": expected[1]}),
            json!({"note": "range", "position": expected[2]}),
        ]
    );
    Ok(())
}

#[test]
fn draft_echo_with_another_triple_line_or_without_position_is_unknown() -> TestResult {
    let session = session_on(HEAD)?;
    let drafts = drafts(&session)?;
    let mut fresh_refs = positions(HEAD)[0].clone();
    fresh_refs["head_sha"] = json!(NEW_HEAD);
    let mut moved = positions(HEAD)[0].clone();
    moved["new_line"] = json!(3);
    for reply in [
        draft_reply(201, 601, "added", Some(&fresh_refs)),
        draft_reply(201, 602, "added", Some(&moved)),
        draft_reply(201, 603, "added", None),
        Reply::json(201, &json!({"note": "без id"})),
    ] {
        let scripted = Scripted::new().expect(Method::Post, format!("{MR}/draft_notes"), reply);
        let result = adapter(&scripted)?.create_draft(&session, &drafts[0], false);
        let Err(err) = result else {
            panic!("ожидалась ошибка: {result:?}");
        };
        assert!(matches!(err, ApiError::Unexpected(_)), "{err:?}");
        assert_eq!(err.write_fate(), WriteFate::Unknown);
        scripted.finish()?;
    }
    Ok(())
}

#[test]
fn drafts_refuse_bad_input_before_any_request() -> TestResult {
    let session = session_on(HEAD)?;
    let drafts = drafts(&session)?;
    let file = file()?;
    let anchor = Anchor::line(&file, LinePoint::at(&file, Side::New, 2)?)?;
    let other = DraftComment::new(
        DraftId(9),
        &session_on(NEW_HEAD)?,
        Placement::Line(anchor),
        "other",
    );
    let blank = DraftComment::new(DraftId(10), &session, drafts[0].placement().clone(), "  ");
    let github = ReviewSession::new(
        "bob",
        ReviewTarget {
            provider: Provider::GitHub,
            ..target()
        },
        revision(HEAD)?,
    );
    let reply = DraftComment::new(
        DraftId(11),
        &github,
        Placement::Reply {
            thread: ProviderId::new("d1"),
        },
        "ответ",
    );
    let scripted = Scripted::new();
    let gitlab = adapter(&scripted)?;
    let mixed = gitlab.create_draft(&session, &other, false);
    assert!(
        matches!(
            mixed,
            Err(ApiError::Model(ModelError::RevisionMismatch { .. }))
        ),
        "{mixed:?}"
    );
    let blank = gitlab.create_draft(&session, &blank, false);
    assert!(matches!(blank, Err(ApiError::Invalid(_))), "{blank:?}");
    let resolving = gitlab.create_draft(&session, &drafts[0], true);
    assert!(
        matches!(resolving, Err(ApiError::Invalid(_))),
        "{resolving:?}"
    );
    let foreign = gitlab.create_draft(&github, &reply, false);
    assert!(
        matches!(foreign, Err(ApiError::Model(ModelError::Provider { .. }))),
        "{foreign:?}"
    );
    let line_on_github = gitlab.create_draft(&github, &drafts[0], false);
    assert!(
        matches!(line_on_github, Err(ApiError::Model(_))),
        "{line_on_github:?}"
    );
    assert!(scripted.finish()?.is_empty());
    Ok(())
}

#[test]
fn reply_draft_goes_into_the_thread_and_may_resolve_it() -> TestResult {
    let session = session_on(HEAD)?;
    let reply = DraftComment::new(
        DraftId(4),
        &session,
        Placement::Reply {
            thread: ProviderId::new("d1"),
        },
        "ответ",
    );
    let echoed = |discussion: &str| {
        Reply::json(
            201,
            &json!({"id": 700, "note": "ответ", "discussion_id": discussion,
                    "resolve_discussion": true, "position": null}),
        )
    };
    let scripted = Scripted::new()
        .expect(Method::Post, format!("{MR}/draft_notes"), echoed("d1"))
        .expect(Method::Post, format!("{MR}/draft_notes"), echoed("d2"));
    let gitlab = adapter(&scripted)?;
    let (known, remote) = gitlab.create_draft(&session, &reply, true)?;
    assert_eq!(known.id().as_str(), "700");
    assert!(remote.resolve_discussion && remote.position.is_none());
    let wrong = gitlab.create_draft(&session, &reply, true);
    assert!(matches!(wrong, Err(ApiError::Unexpected(_))), "{wrong:?}");
    let requests = scripted.finish()?;
    assert_eq!(
        requests[0].body_json(),
        Some(
            json!({"note": "ответ", "in_reply_to_discussion_id": "d1", "resolve_discussion": true})
        )
    );
    Ok(())
}

#[test]
fn publish_sends_only_known_ids_one_by_one_and_the_summary_separately() -> TestResult {
    let session = session_on(HEAD)?;
    let drafts = drafts(&session)?;
    let expected = positions(HEAD);
    let scripted = Scripted::new()
        .expect(
            Method::Post,
            format!("{MR}/draft_notes"),
            draft_reply(201, 501, "added", Some(&expected[0])),
        )
        .expect(
            Method::Post,
            format!("{MR}/draft_notes"),
            draft_reply(201, 502, "removed", Some(&expected[1])),
        )
        .expect(
            Method::Get,
            format!("{MR}/draft_notes?per_page=100&page=1"),
            page(
                json!([
                    {"id": 900, "note": "из браузера", "position": null},
                    {"id": 501, "note": "added", "position": expected[0]},
                    {"id": 502, "note": "removed", "position": expected[1]},
                ]),
                "",
            ),
        )
        .expect(
            Method::Put,
            format!("{MR}/draft_notes/501/publish"),
            no_content(),
        )
        .expect(
            Method::Put,
            format!("{MR}/draft_notes/502/publish"),
            no_content(),
        )
        .expect(
            Method::Put,
            format!("{MR}/draft_notes/503/publish"),
            no_content(),
        )
        .expect(
            Method::Post,
            format!("{MR}/notes"),
            Reply::json(
                201,
                &json!({"id": 801, "body": "Итог", "author": {"username": "bob"}, "system": false}),
            ),
        );
    let gitlab = adapter(&scripted)?;
    let (first, _) = gitlab.create_draft(&session, &drafts[0], false)?;
    let (second, _) = gitlab.create_draft(&session, &drafts[1], false)?;
    let listed = gitlab.drafts()?;
    assert_eq!(listed.len(), 3, "список видит и черновик другого клиента");
    for known in [&first, &second] {
        gitlab.publish_draft(known)?;
    }
    gitlab.publish_draft(&KnownDraft::recorded(ProviderId::new("503")))?;
    let summary = gitlab.summary_note("Итог")?;
    assert_eq!(summary.id, ProviderId::new("801"));
    let requests = scripted.finish()?;
    let publishes: Vec<&str> = requests
        .iter()
        .filter(|request| request.path().ends_with("/publish"))
        .map(Request::path)
        .collect();
    assert_eq!(
        publishes,
        [
            format!("{MR}/draft_notes/501/publish"),
            format!("{MR}/draft_notes/502/publish"),
            format!("{MR}/draft_notes/503/publish"),
        ]
    );
    for request in &requests {
        assert!(!request.path().contains("bulk"), "{}", request.path());
        assert!(!request.path().contains("/900"), "чужой черновик не тронут");
    }
    for request in requests.iter().filter(|r| r.path().ends_with("/publish")) {
        assert_eq!(request.method, Method::Put);
        assert!(request.body_json().is_none(), "публикация без тела");
    }
    assert_eq!(requests[6].body_json(), Some(json!({"body": "Итог"})));
    Ok(())
}

#[test]
fn update_keeps_the_id_and_takes_the_position_of_the_session() -> TestResult {
    let session = session_on(HEAD)?;
    let drafts = drafts(&session)?;
    let expected = positions(HEAD);
    let known = KnownDraft::recorded(ProviderId::new("501"));
    let scripted = Scripted::new()
        .expect(
            Method::Put,
            format!("{MR}/draft_notes/501"),
            draft_reply(200, 501, "added", Some(&expected[0])),
        )
        .expect(
            Method::Put,
            format!("{MR}/draft_notes/501"),
            draft_reply(200, 777, "added", Some(&expected[0])),
        )
        .expect(
            Method::Delete,
            format!("{MR}/draft_notes/501"),
            no_content(),
        );
    let gitlab = adapter(&scripted)?;
    let updated = gitlab.update_draft(&session, &known, &drafts[0])?;
    assert_eq!(updated.id, ProviderId::new("501"));
    let changed = gitlab.update_draft(&session, &known, &drafts[0]);
    assert!(
        matches!(changed, Err(ApiError::Unexpected(_))),
        "{changed:?}"
    );
    gitlab.delete_draft(&known)?;
    let requests = scripted.finish()?;
    assert_eq!(
        requests[0].body_json(),
        Some(json!({"note": "added", "position": expected[0]}))
    );
    Ok(())
}

fn approval(approved_by: &[&str], own: Option<bool>) -> Value {
    let mut value = json!({
        "approved": !approved_by.is_empty(),
        "approved_by": approved_by.iter().map(|name| json!({"user": {"username": name}})).collect::<Vec<_>>(),
    });
    if let Some(own) = own {
        value["user_has_approved"] = json!(own);
    }
    value
}

#[test]
fn approve_sends_the_head_of_the_session_and_reads_409_as_sha_rejected() -> TestResult {
    let session = session_on(HEAD)?;
    let approve = format!("{MR}/approve");
    let scripted = Scripted::new()
        .expect(
            Method::Post,
            approve.clone(),
            Reply::json(201, &approval(&["bob"], Some(true))),
        )
        .expect(
            Method::Post,
            approve.clone(),
            Reply::json(201, &approval(&["bob"], None)),
        )
        .expect(
            Method::Post,
            approve.clone(),
            Reply::json(
                409,
                &json!({"message": format!("SHA does not match HEAD of source branch: {NEW_HEAD}")}),
            ),
        )
        .expect(
            Method::Post,
            approve.clone(),
            Reply::json(201, &approval(&["carol"], Some(false))),
        )
        .expect(
            Method::Post,
            approve.clone(),
            Reply::json(401, &json!({"message": "401 Unauthorized"})),
        );
    let gitlab = adapter(&scripted)?;
    assert_eq!(gitlab.approve(&session)?.approved_by, ["bob"]);
    assert!(
        gitlab.approve(&session)?.approved,
        "без user_has_approved — по approved_by"
    );
    let stale = gitlab.approve(&session);
    assert!(matches!(stale, Err(ApiError::ShaRejected(_))), "{stale:?}");
    let other = gitlab.approve(&session);
    assert!(matches!(other, Err(ApiError::Unexpected(_))), "{other:?}");
    let refused = gitlab.approve(&session);
    assert!(
        matches!(refused, Err(ApiError::Status { status: 401, .. })),
        "{refused:?}"
    );
    assert_eq!(
        stale.err().map(|e| e.write_fate()),
        Some(WriteFate::NotAccepted)
    );
    assert_eq!(
        other.err().map(|e| e.write_fate()),
        Some(WriteFate::Unknown)
    );
    assert_eq!(
        refused.err().map(|e| e.write_fate()),
        Some(WriteFate::NotAccepted)
    );
    let requests = scripted.finish()?;
    for request in &requests {
        assert_eq!(request.body_json(), Some(json!({"sha": HEAD})));
    }
    Ok(())
}

#[test]
fn request_changes_is_verified_by_a_separate_read_of_the_reviewer_state() -> TestResult {
    let scripted = Scripted::new()
        .expect(Method::Post, GRAPHQL, rc_ok())
        .expect(Method::Post, GRAPHQL, reviewers_reply("REQUESTED_CHANGES"));
    let observed = adapter(&scripted)?.request_changes(&caps())?;
    assert_eq!(observed, ReviewerState::RequestedChanges);
    let requests = scripted.finish()?;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].body_json(),
        Some(json!({
            "query": REQUEST_CHANGES,
            "variables": {"input": {"projectPath": PROJECT, "iid": "3"}},
        }))
    );
    assert_eq!(
        requests[1].body_json(),
        Some(json!({"query": REVIEWERS, "variables": {"fullPath": PROJECT, "iid": "3"}}))
    );
    assert!(no_notes(&requests));
    Ok(())
}

#[test]
fn request_changes_outcomes_differ_only_where_the_answers_differ() -> TestResult {
    let not_available = Reply::json(
        200,
        &json!({"data": {"mergeRequestRequestChanges": null}, "errors": [{
            "message": "The resource that you are attempting to access does not exist or you don't have permission to perform this action",
            "path": ["mergeRequestRequestChanges"],
        }]}),
    );
    let undefined = Reply::json(
        200,
        &json!({"errors": [{
            "message": "Field 'mergeRequestRequestChanges' doesn't exist on type 'Mutation'",
            "extensions": {"code": "undefinedField", "typeName": "Mutation",
                           "fieldName": "mergeRequestRequestChanges"},
        }]}),
    );
    let execution = Reply::json(
        200,
        &json!({"data": {"mergeRequestRequestChanges": null},
                "errors": [{"message": "Internal server error"}]}),
    );
    let payload = Reply::json(
        200,
        &json!({"data": {"mergeRequestRequestChanges": {"errors": ["Reviewer not found"]}}}),
    );
    let null_payload = Reply::json(200, &json!({"data": {"mergeRequestRequestChanges": null}}));
    let no_project = Reply::json(200, &json!({"data": {"project": null}}));
    type Check = fn(&ApiError) -> bool;
    let cases: Vec<(Vec<Reply>, Check, WriteFate)> = vec![
        (
            vec![undefined],
            |e| *e == ApiError::Absent(capabilities::REQUEST_CHANGES),
            WriteFate::NotAccepted,
        ),
        (
            vec![not_available],
            |e| matches!(e, ApiError::Unavailable(_)),
            WriteFate::NotAccepted,
        ),
        (
            vec![payload],
            |e| *e == ApiError::Payload(vec!["Reviewer not found".to_owned()]),
            WriteFate::NotAccepted,
        ),
        (
            vec![Reply::Respond(
                Response::new(429)
                    .with_header("content-type", "text/plain")
                    .with_header("retry-after", "60")
                    .with_body("Retry later\n"),
            )],
            |e| matches!(e, ApiError::RateLimited { status: 429, detail } if detail == "Retry later"),
            WriteFate::NotAccepted,
        ),
        (
            vec![execution],
            |e| matches!(e, ApiError::Graphql(errors) if errors[0].code.is_none()),
            WriteFate::Unknown,
        ),
        (
            vec![null_payload],
            |e| matches!(e, ApiError::Unexpected(_)),
            WriteFate::Unknown,
        ),
        (
            vec![Reply::json(
                500,
                &json!({"message": "500 Internal Server Error"}),
            )],
            |e| matches!(e, ApiError::Status { status: 500, .. }),
            WriteFate::Unknown,
        ),
        (
            vec![Reply::timeout()],
            |e| matches!(e, ApiError::Transport(_)),
            WriteFate::Unknown,
        ),
        (
            vec![rc_ok(), reviewers_reply("UNREVIEWED")],
            |e| matches!(e, ApiError::Unexpected(_)),
            WriteFate::Unknown,
        ),
        (
            vec![rc_ok(), Reply::timeout()],
            |e| matches!(e, ApiError::Unverified(read) if matches!(**read, ApiError::Transport(_))),
            WriteFate::Unknown,
        ),
        (
            vec![rc_ok(), no_project],
            |e| matches!(e, ApiError::Unverified(read) if matches!(**read, ApiError::Unavailable(_))),
            WriteFate::Unknown,
        ),
    ];
    for (replies, check, fate) in cases {
        let scripted = replies
            .into_iter()
            .fold(Scripted::new(), |scripted, reply| {
                scripted.expect(Method::Post, GRAPHQL, reply)
            });
        let result = adapter(&scripted)?.request_changes(&caps());
        let Err(err) = result else {
            panic!("ожидалась ошибка: {result:?}");
        };
        assert!(check(&err), "{err:?}");
        assert_eq!(err.write_fate(), fate, "{err:?}");
        let requests = scripted.finish()?;
        assert!(no_notes(&requests), "запрос изменений не подменён заметкой");
    }
    Ok(())
}

#[test]
fn rest_answers_classify_the_write_fate() -> TestResult {
    let known = KnownDraft::recorded(ProviderId::new("501"));
    let publish = format!("{MR}/draft_notes/501/publish");
    let cases = [
        (
            Reply::json(404, &json!({"message": "404 Not found"})),
            WriteFate::NotAccepted,
        ),
        (
            Reply::json(400, &json!({"error": "note is missing"})),
            WriteFate::NotAccepted,
        ),
        (
            Reply::json(502, &json!({"message": "502 Bad Gateway"})),
            WriteFate::Unknown,
        ),
        (Reply::not_sent(), WriteFate::NotAccepted),
        (Reply::timeout(), WriteFate::Unknown),
        (Reply::disconnect(), WriteFate::Unknown),
    ];
    for (reply, fate) in cases {
        let scripted = Scripted::new().expect(Method::Put, publish.clone(), reply);
        let result = adapter(&scripted)?.publish_draft(&known);
        let Err(err) = result else {
            panic!("ожидалась ошибка");
        };
        assert_eq!(err.write_fate(), fate, "{err:?}");
        scripted.finish()?;
    }
    let scripted = Scripted::new().expect(
        Method::Put,
        publish,
        Reply::json(400, &json!({"error": "note is missing"})),
    );
    let err = adapter(&scripted)?.publish_draft(&known);
    assert_eq!(
        err,
        Err(ApiError::Status {
            status: 400,
            message: "note is missing".to_owned(),
        })
    );
    Ok(())
}

#[test]
fn lists_read_every_page_and_build_the_next_address_themselves() -> TestResult {
    let scripted = Scripted::new()
        .expect(
            Method::Get,
            format!("{MR}/draft_notes?per_page=100&page=1"),
            page(json!([{"id": 1, "note": "a"}]), "2"),
        )
        .expect(
            Method::Get,
            format!("{MR}/draft_notes?per_page=100&page=2"),
            page(json!([{"id": 2, "note": "b"}]), ""),
        )
        .expect(
            Method::Get,
            format!("{MR}/notes?sort=asc&per_page=100&page=1"),
            Reply::Respond(
                Response::json(
                    200,
                    &json!([{"id": 3, "body": "x", "author": {"username": "bob"}}]),
                )
                .with_header(
                    "link",
                    format!("<https://elsewhere.test{MR}/notes?page=2&per_page=100>; rel=\"next\""),
                ),
            ),
        )
        .expect(
            Method::Get,
            format!("{MR}/notes?sort=asc&per_page=100&page=2"),
            Reply::json(200, &json!([])),
        )
        .expect(
            Method::Get,
            format!("{MR}/discussions?per_page=100&page=1"),
            page(
                json!([{"id": "d1", "individual_note": false, "notes": [
                    {"id": 4, "body": "y", "author": {"username": "bob"}, "resolvable": true,
                     "resolved": false, "position": positions(HEAD)[0]},
                ]}]),
                "2",
            ),
        )
        .expect(
            Method::Get,
            format!("{MR}/discussions?per_page=100&page=2"),
            page(json!([]), "3"),
        );
    let gitlab = adapter(&scripted)?;
    let drafts = gitlab.drafts()?;
    assert_eq!(drafts.len(), 2);
    assert!(drafts.iter().all(|draft| draft.position.is_none()));
    let notes = gitlab.notes()?;
    assert_eq!(notes[0].author, "bob");
    let broken = gitlab.discussions();
    assert!(matches!(broken, Err(ApiError::Unexpected(_))), "{broken:?}");
    for request in scripted.finish()? {
        assert!(request.url.starts_with(ROOT));
    }
    Ok(())
}

#[test]
fn reviewer_read_of_a_hidden_project_has_no_exact_cause() -> TestResult {
    let scripted = Scripted::new()
        .expect(
            Method::Post,
            GRAPHQL,
            Reply::json(200, &json!({"data": {"project": null}})),
        )
        .expect(
            Method::Post,
            GRAPHQL,
            Reply::json(200, &json!({"data": {"project": {"mergeRequest": null}}})),
        )
        .expect(Method::Post, GRAPHQL, reviewers_reply("REVIEW_STARTED"));
    let gitlab = adapter(&scripted)?;
    assert!(matches!(gitlab.reviewers(), Err(ApiError::Unavailable(_))));
    assert!(matches!(gitlab.reviewers(), Err(ApiError::Unavailable(_))));
    assert_eq!(
        gitlab.reviewer_state("BOB")?,
        Some(ReviewerState::ReviewStarted)
    );
    scripted.finish()?;
    Ok(())
}

#[test]
fn adapter_needs_a_gitlab_target_with_a_namespace() {
    let idle = Scripted::new();
    let github = ReviewTarget {
        provider: Provider::GitHub,
        ..target()
    };
    assert!(matches!(
        GitLab::new(&idle, Endpoints::instance(ROOT), &github),
        Err(ApiError::Invalid(_))
    ));
    for repo in ["proj", "/proj", "group/", "group//proj", ""] {
        let target = ReviewTarget {
            repo: repo.to_owned(),
            ..target()
        };
        let result = GitLab::new(&idle, Endpoints::instance(ROOT), &target);
        assert!(matches!(result, Err(ApiError::Invalid(_))), "{repo}");
    }
    assert_eq!(
        Endpoints::cloud(),
        Endpoints {
            rest: "https://gitlab.com/api/v4".to_owned(),
            graphql: "https://gitlab.com/api/graphql".to_owned(),
        }
    );
    assert_eq!(
        Endpoints::instance("http://127.0.0.1:8080/"),
        Endpoints {
            rest: "http://127.0.0.1:8080/api/v4".to_owned(),
            graphql: "http://127.0.0.1:8080/api/graphql".to_owned(),
        }
    );
}

#[test]
fn adapter_source_has_no_mass_publication() {
    for (name, source) in [
        ("mod.rs", include_str!("mod.rs")),
        ("capabilities.rs", include_str!("capabilities.rs")),
    ] {
        assert!(!source.contains("bulk_publish"), "{name}");
        assert!(!source.contains("bulk"), "{name}");
    }
}

// Сквозная проверка против `forge-fake` по loopback: адаптер говорит на языке оракула.

fn fake_server() -> Result<FakeServer<GitlabFake>, String> {
    let mut gitlab = GitlabFake::new(PROJECT);
    gitlab.add_user("alice", "tok-author", Some(Access::Developer));
    gitlab.add_user("bob", "tok-reviewer", Some(Access::Developer));
    gitlab.add_user("rita", "tok-reporter", Some(Access::Reporter));
    gitlab.add_mr(
        3,
        "alice",
        BASE,
        HEAD,
        vec![
            FakeDiff::modified(PATH, DIFF),
            FakeDiff::renamed("old/b.rs", "src/b.rs", "@@ -1 +1 @@\n-x\n+y\n"),
        ],
    );
    gitlab.push_version(
        3,
        BASE,
        START,
        HEAD_ON_START,
        vec![FakeDiff::modified(PATH, DIFF)],
    )?;
    gitlab.add_reviewer(3, "bob")?;
    FakeServer::start(gitlab)
}

/// Head второй версии MR, у которой `start_sha` отличается от `base_sha`.
const HEAD_ON_START: &str = "5555555555555555555555555555555555555555";

fn http_adapter(
    server: &FakeServer<GitlabFake>,
    token: &str,
) -> Result<GitLab<HttpTransport>, ApiError> {
    GitLab::new(
        HttpTransport::new(
            HttpConfig::default(),
            Some(Credential::private_token(Secret::new(token))),
        ),
        Endpoints::instance(server.base()),
        &target(),
    )
}

#[test]
fn adapter_drives_forge_fake_without_touching_foreign_drafts() -> TestResult {
    let server = fake_server()?;
    let planted = server.with(|fake| fake.plant_draft(3, "bob", "из браузера", None))?;
    let gitlab = http_adapter(&server, "tok-reviewer")?;
    let caps = gitlab.capabilities()?;
    assert_eq!(caps.username, "bob");
    assert_eq!(caps.host.version, "18.5.1");
    assert_eq!(caps.host.enterprise, Some(false));
    assert!(caps.private() && caps.can_review() && caps.request_changes());

    let snapshot = gitlab.merge_request()?;
    assert_eq!(snapshot.revision, revision(HEAD_ON_START)?);
    assert_eq!(snapshot.files.len(), 1);
    let session = ReviewSession::new("bob", target(), snapshot.revision.clone());
    let drafts = drafts(&session)?;

    let mut known = Vec::new();
    for draft in &drafts {
        known.push(gitlab.create_draft(&session, draft, false)?.0);
    }
    let listed = gitlab.drafts()?;
    assert_eq!(listed.len(), 4, "свои и подложенный");
    let own = gitlab.draft(known[2].id())?;
    assert_eq!(
        own.line_code.as_deref(),
        Some(code(3, 3).as_str()),
        "line_code диапазона — его конец"
    );
    for draft in &known {
        gitlab.publish_draft(draft)?;
    }
    gitlab.summary_note("Итог\r\n")?;

    let notes = server.with(|fake| fake.notes(3));
    let published: Vec<Option<u64>> = notes.iter().map(|note| note.from_draft).collect();
    let ids: Vec<Option<u64>> = known
        .iter()
        .map(|draft| draft.id().as_str().parse().ok())
        .chain([None])
        .collect();
    assert_eq!(published, ids, "три черновика и сводка");
    let remaining = server.with(|fake| fake.drafts(3));
    assert_eq!(remaining.len(), 1);
    assert_eq!(
        remaining[0].id, planted,
        "чужой черновик остался черновиком"
    );
    assert_eq!(server.count(BULK_PUBLISH), 0);
    assert_eq!(server.count(PUBLISH), 3);

    let discussions = gitlab.discussions()?;
    assert_eq!(discussions.len(), 4);
    for discussion in &discussions[..3] {
        let position = discussion.notes[0].position.as_ref().ok_or("нет позиции")?;
        assert_eq!(position.revision()?, session.revision().clone());
    }
    assert!(discussions[3].individual_note);
    assert_eq!(
        gitlab.notes()?.last().map(|note| note.body.as_str()),
        Some("Итог")
    );

    let reply = DraftComment::new(
        DraftId(4),
        &session,
        Placement::Reply {
            thread: discussions[0].id.clone(),
        },
        "ответ",
    );
    let (answer, _) = gitlab.create_draft(&session, &reply, true)?;
    gitlab.publish_draft(&answer)?;
    let thread = gitlab.discussion(&discussions[0].id)?;
    assert_eq!(thread.notes.len(), 2);
    assert_eq!(thread.notes[0].resolved, Some(true));

    let stale = gitlab.approve(&session_on(HEAD)?);
    assert!(matches!(stale, Err(ApiError::ShaRejected(_))), "{stale:?}");
    let approval = gitlab.approve(&session)?;
    assert_eq!(approval.approved_by, ["bob"]);
    assert_eq!(gitlab.approvals()?.user_has_approved, Some(true));

    assert_eq!(
        gitlab.request_changes(&caps)?,
        ReviewerState::RequestedChanges
    );
    assert_eq!(
        server.with(|fake| fake.reviewer_state(3, "bob")),
        Some(FakeState::RequestedChanges)
    );
    assert!(server.with(|fake| fake.approved_by(3)).is_empty());
    assert_eq!(server.count(BULK_PUBLISH), 0);
    server.finish()?;
    Ok(())
}

#[test]
fn approve_after_a_target_change_on_the_same_head_is_sha_rejected() -> TestResult {
    let server = fake_server()?;
    let gitlab = http_adapter(&server, "tok-reviewer")?;
    let session = ReviewSession::new("bob", target(), gitlab.merge_request()?.revision);
    server.with(|fake| {
        fake.move_target(3, NEW_HEAD, NEW_HEAD, vec![FakeDiff::modified(PATH, DIFF)])
    })?;
    let rejected = gitlab.approve(&session);
    assert!(
        matches!(rejected, Err(ApiError::ShaRejected(_))),
        "{rejected:?}"
    );
    assert!(server.with(|fake| fake.approved_by(3)).is_empty());
    let moved = gitlab.merge_request()?;
    assert_eq!(moved.revision.head_sha(), session.revision().head_sha());
    assert_ne!(moved.revision, *session.revision());
    Ok(())
}

#[test]
fn request_changes_modes_on_forge_fake() -> TestResult {
    let server = fake_server()?;
    let reviewer = http_adapter(&server, "tok-reviewer")?;
    let caps = reviewer.capabilities()?;

    server.with(|fake| fake.set_request_changes(RequestChangesMode::Absent));
    let absent_caps = reviewer.capabilities()?;
    assert!(!absent_caps.request_changes());
    assert_eq!(
        reviewer.request_changes(&absent_caps),
        Err(ApiError::Absent(capabilities::REQUEST_CHANGES))
    );
    assert_eq!(
        server.count(RC_KEY),
        0,
        "по чтению возможностей — без запроса"
    );
    assert_eq!(
        reviewer.request_changes(&caps),
        Err(ApiError::Absent(capabilities::REQUEST_CHANGES)),
        "устаревшее чтение возможностей: undefinedField"
    );
    assert_eq!(server.count(RC_KEY), 1);

    server.with(|fake| fake.set_request_changes(RequestChangesMode::Forbidden));
    let forbidden = reviewer.request_changes(&caps);
    assert!(
        matches!(forbidden, Err(ApiError::Unavailable(_))),
        "{forbidden:?}"
    );

    server.with(|fake| fake.set_request_changes(RequestChangesMode::ExecutionError));
    let failed = reviewer.request_changes(&caps);
    let Err(failed) = failed else {
        panic!("ожидалась ошибка");
    };
    assert!(matches!(failed, ApiError::Graphql(_)), "{failed:?}");
    assert_eq!(failed.write_fate(), WriteFate::Unknown);

    server.with(|fake| fake.set_request_changes(RequestChangesMode::PayloadError));
    let payload = reviewer.request_changes(&caps);
    assert!(matches!(payload, Err(ApiError::Payload(_))), "{payload:?}");

    server.with(|fake| fake.set_request_changes(RequestChangesMode::Available));
    let author = http_adapter(&server, "tok-author")?;
    let author_caps = author.capabilities()?;
    assert_eq!(
        author.request_changes(&author_caps),
        Err(ApiError::Payload(vec!["Reviewer not found".to_owned()]))
    );
    let reporter = http_adapter(&server, "tok-reporter")?;
    let reporter_caps = reporter.capabilities()?;
    let before = server.count(RC_KEY);
    assert!(matches!(
        reporter.request_changes(&reporter_caps),
        Err(ApiError::NotPermitted { .. })
    ));
    assert_eq!(server.count(RC_KEY), before, "без прав — без запроса");

    assert_eq!(
        server.with(|fake| fake.reviewer_state(3, "bob")),
        Some(FakeState::Unreviewed)
    );
    assert_eq!(server.count(NOTES), 0, "заметкой не подменён");
    assert!(server.with(|fake| fake.notes(3)).is_empty());
    Ok(())
}

/// Каждая запись адаптера, построенного от цели сторожа (как `gitlab_adapter` живой клетки),
/// проходит через `Guarded` без отказа: REST — под путём проверенного проекта, GraphQL — на
/// адрес цели.
#[test]
fn every_adapter_write_passes_the_write_guard() -> TestResult {
    use crate::live::guard::{GuardTarget, Guarded, WriteScope};
    let session = session_on(HEAD)?;
    let drafts = drafts(&session)?;
    let expected = positions(HEAD);
    let scripted = Scripted::new()
        .expect(
            Method::Get,
            "/api/v4/projects/group%2Fsub%2Fproj",
            Reply::json(
                200,
                &json!({"path_with_namespace": PROJECT, "visibility": "private",
                        "default_branch": "main"}),
            ),
        )
        .expect(
            Method::Post,
            format!("{MR}/draft_notes"),
            draft_reply(201, 501, "added", Some(&expected[0])),
        )
        .expect(
            Method::Put,
            format!("{MR}/draft_notes/501"),
            draft_reply(200, 501, "added", Some(&expected[0])),
        )
        .expect(
            Method::Put,
            format!("{MR}/draft_notes/501/publish"),
            no_content(),
        )
        .expect(
            Method::Delete,
            format!("{MR}/draft_notes/502"),
            no_content(),
        )
        .expect(
            Method::Post,
            format!("{MR}/notes"),
            Reply::json(
                201,
                &json!({"id": 801, "body": "Итог", "author": {"username": "bob"}, "system": false}),
            ),
        )
        .expect(
            Method::Post,
            format!("{MR}/approve"),
            Reply::json(201, &approval(&["bob"], Some(true))),
        )
        .expect(Method::Post, GRAPHQL, rc_ok())
        .expect(Method::Post, GRAPHQL, reviewers_reply("REQUESTED_CHANGES"));
    let guard = Guarded::new(
        &scripted,
        GuardTarget::gitlab(&Endpoints::instance(ROOT), PROJECT),
        WriteScope::Listed(vec![PROJECT.to_owned()]),
    );
    let gitlab = crate::scenario::live::gitlab_adapter(&guard, &target())?;
    let (known, _) = gitlab.create_draft(&session, &drafts[0], false)?;
    gitlab.update_draft(&session, &known, &drafts[0])?;
    gitlab.publish_draft(&known)?;
    gitlab.delete_draft(&KnownDraft::recorded(ProviderId::new("502")))?;
    gitlab.summary_note("Итог")?;
    gitlab.approve(&session)?;
    assert_eq!(
        gitlab.request_changes(&caps())?,
        ReviewerState::RequestedChanges
    );
    assert!(guard.refusal().is_none(), "{:?}", guard.refusal());
    assert!(
        guard.refused_writes().is_empty(),
        "{:?}",
        guard.refused_writes()
    );
    let requests = scripted.finish()?;
    let writes = requests.iter().filter(|r| r.method.is_write()).count();
    assert_eq!(writes, 8, "все записи адаптера дошли до транспорта");
    Ok(())
}
