//! Эталоны JSON протокола v1 в `tests/wire/*.json` и отказы разбора кадров.
//!
//! Каждый эталон сравнивается со своим образцом в обе стороны: сериализация образца равна
//! эталону как значение JSON, разбор эталона даёт образец. Значения сравниваются, а не текст.
//! Эталон меняется только вместе с записью причины в плане: после публикации v1 формат
//! меняется только новыми вариантами.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use shellio_control::{
    Activation, ControlError, ControlErrorCode, Hello, HelloReply, Launch, LaunchReceipt, OkValue,
    OwnerState, PROTOCOL, REQUEST_LINE_LIMIT, RefuseReason, RefusedArg, Rejected, Request,
    RequestBody, RequestId, Response,
};
use shellio_core::test_support::SequentialIds;
use shellio_core::{
    Change, Command, CommandValue, CoreError, Entity, Event, EventsSince, IdGen, Query, Reply,
    Revision, Route, Snapshot, State, Title, TreeScope, WindowState, Workspace, WorkspaceId,
};

fn wire_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wire")
}

fn core_wire_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../shellio-core/tests/wire")
}

/// Образец эталона любого сериализуемого типа.
trait Sample {
    /// Сверить образец с текстом эталона. `Err` — описание расхождения.
    fn check(&self, text: &str) -> Result<(), String>;
    fn pretty(&self) -> String;
}

impl<T: Serialize + DeserializeOwned + PartialEq + Debug> Sample for T {
    fn check(&self, text: &str) -> Result<(), String> {
        let expected: Value =
            serde_json::from_str(text).map_err(|error| format!("эталон не JSON: {error}"))?;
        let actual = serde_json::to_value(self).map_err(|error| error.to_string())?;
        if expected != actual {
            return Err(format!(
                "сериализация расходится с эталоном, получено:\n{}",
                self.pretty()
            ));
        }
        let parsed: T = serde_json::from_str(text)
            .map_err(|error| format!("эталон не разбирается: {error}"))?;
        if parsed != *self {
            return Err(format!(
                "разбор эталона расходится с образцом:\n{parsed:#?}\n!=\n{self:#?}"
            ));
        }
        Ok(())
    }

    fn pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|error| error.to_string())
    }
}

type Samples = Vec<(String, Box<dyn Sample>)>;

fn sample<T: Sample + 'static>(name: impl Into<String>, value: T) -> (String, Box<dyn Sample>) {
    (name.into(), Box::new(value))
}

fn check_goldens(samples: &Samples) -> Vec<String> {
    let mut failures = Vec::new();
    for (name, sample) in samples {
        let path = wire_dir().join(format!("{name}.json"));
        match fs::read_to_string(&path) {
            Ok(text) => {
                if let Err(reason) = sample.check(&text) {
                    failures.push(format!("{name}: {reason}"));
                }
            }
            Err(error) => failures.push(format!(
                "{name}: нет эталона {} ({error}), образец:\n{}",
                path.display(),
                sample.pretty()
            )),
        }
    }
    failures
}

fn assert_goldens(samples: &Samples) {
    let failures = check_goldens(samples);
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

fn title(name: &str) -> Result<Title, String> {
    Title::new(name).map_err(|error| error.to_string())
}

/// Первый идентификатор последовательного генератора: эталоны читаемы.
fn workspace_id() -> WorkspaceId {
    let mut generator = SequentialIds::new();
    let generator: &mut dyn IdGen = &mut generator;
    WorkspaceId::generate(generator)
}

/// Состояние снимков: один воркспейс, он же активный.
fn state() -> Result<Arc<State>, String> {
    let id = workspace_id();
    let mut state = State {
        window: WindowState {
            active_workspace: Some(id),
            route: Route::WorkspaceHome,
        },
        ..State::default()
    };
    state.workspaces.insert(
        id,
        Workspace {
            id,
            title: title("Работа")?,
            position: 0,
            last_workbench: None,
            deleted_at: None,
        },
    );
    assert_eq!(state.check_invariants(), Vec::new());
    Ok(Arc::new(state))
}

fn snapshot(revision: u64) -> Result<Snapshot, String> {
    Ok(Snapshot {
        revision: Revision(revision),
        state: state()?,
    })
}

fn request_golden(request: &Request) -> &'static str {
    match &request.body {
        RequestBody::Hello(_) => "request.hello",
        RequestBody::Command { .. } => "request.command",
        RequestBody::Query(_) => "request.query",
        RequestBody::EventsSince { .. } => "request.events_since",
        RequestBody::Subscribe => "request.subscribe",
        RequestBody::Launch(_) => "request.launch",
    }
}

fn requests() -> Vec<Request> {
    let workspace = workspace_id();
    let request = |id, body| Request {
        id: RequestId(id),
        body,
    };
    vec![
        request(
            1,
            RequestBody::Hello(Hello {
                protocol: PROTOCOL,
                client: "shellioctl 0.1.0".into(),
            }),
        ),
        request(
            2,
            RequestBody::Command {
                command: Command::WorkspaceRename {
                    workspace,
                    title: "Дом".into(),
                },
                scope: TreeScope::Workspace(workspace),
            },
        ),
        request(
            3,
            RequestBody::Query(Query::Tree {
                scope: TreeScope::All,
            }),
        ),
        request(
            4,
            RequestBody::EventsSince {
                cursor: Revision(12),
            },
        ),
        request(5, RequestBody::Subscribe),
        request(
            6,
            RequestBody::Launch(Launch {
                args: vec!["/abs/path".into()],
                cwd: "/abs".into(),
            }),
        ),
    ]
}

fn request_samples() -> Samples {
    requests()
        .into_iter()
        .map(|request| sample(request_golden(&request), request))
        .collect()
}

fn response_golden(response: &Response) -> &'static str {
    match response {
        Response::Ok {
            value: OkValue::Hello(_),
            ..
        } => "response.ok.hello",
        Response::Ok {
            value: OkValue::Reply(_),
            ..
        } => "response.ok.reply",
        Response::Ok {
            value: OkValue::EventsSince(EventsSince::Events(_)),
            ..
        } => "response.ok.events_since.events",
        Response::Ok {
            value: OkValue::EventsSince(EventsSince::Reset(_)),
            ..
        } => "response.ok.events_since.reset",
        Response::Ok {
            value: OkValue::Launch(receipt),
            ..
        } if receipt
            .refused
            .iter()
            .any(|refused| refused.reason == RefuseReason::GalleryNotAtStart) =>
        {
            "response.ok.launch.gallery_not_at_start"
        }
        Response::Ok {
            value: OkValue::Launch(_),
            ..
        } => "response.ok.launch",
        Response::CoreError { .. } => "response.core_error",
        Response::Error { id: Some(_), .. } => "response.error",
        Response::Error { id: None, .. } => "response.error.unnumbered",
        Response::Snapshot { .. } => "response.snapshot",
        Response::Event { .. } => "response.event",
        Response::Reset { .. } => "response.reset",
    }
}

fn responses() -> Result<Vec<Response>, String> {
    let workspace = workspace_id();
    let ok = |id, value| Response::Ok {
        id: RequestId(id),
        value,
    };
    let refused = |arg: &str, reason| RefusedArg {
        arg: arg.into(),
        reason,
    };
    Ok(vec![
        ok(
            1,
            OkValue::Hello(HelloReply {
                protocol: PROTOCOL,
                server: "0.1.0".into(),
                pid: 123,
                owner: OwnerState::Ready,
            }),
        ),
        ok(
            2,
            OkValue::Reply(Reply {
                revision: Revision(13),
                value: CommandValue::None.into(),
            }),
        ),
        ok(
            4,
            OkValue::EventsSince(EventsSince::Events(vec![Event {
                revision: Revision(13),
                changes: vec![Change::WorkspaceRenamed {
                    workspace,
                    title: title("Дом")?,
                }],
            }])),
        ),
        ok(4, OkValue::EventsSince(EventsSince::Reset(snapshot(13)?))),
        ok(
            6,
            OkValue::Launch(LaunchReceipt {
                activation: Activation::Requested,
                refused: vec![
                    refused("/abs/path", RefuseReason::FolderOpenUnsupported),
                    refused("/abs/notes.txt", RefuseReason::NotADirectory),
                    refused("/abs/missing", RefuseReason::PathNotFound),
                    refused("--unknown", RefuseReason::UnknownArgument),
                ],
            }),
        ),
        // P1.3: `--gallery` второго запуска; эталон P1.2 `response.ok.launch` не меняется.
        ok(
            7,
            OkValue::Launch(LaunchReceipt {
                activation: Activation::Requested,
                refused: vec![refused("--gallery", RefuseReason::GalleryNotAtStart)],
            }),
        ),
        Response::CoreError {
            id: RequestId(2),
            error: CoreError::not_found(Entity::Workspace, workspace),
        },
        Response::Error {
            id: Some(RequestId(2)),
            error: ControlError::owner_leaving(),
        },
        Response::Error {
            id: None,
            error: ControlError::request_too_large(REQUEST_LINE_LIMIT),
        },
        Response::Snapshot {
            id: RequestId(5),
            snapshot: snapshot(13)?,
        },
        Response::Event {
            id: RequestId(5),
            event: Event {
                revision: Revision(14),
                changes: vec![Change::WindowSet {
                    active: workspace,
                    route: Route::WorkspaceHome,
                }],
            },
        },
        Response::Reset {
            id: RequestId(5),
            snapshot: snapshot(1300)?,
        },
    ])
}

fn response_samples() -> Result<Samples, String> {
    Ok(responses()?
        .into_iter()
        .map(|response| sample(response_golden(&response), response))
        .collect())
}

fn error_sample(code: ControlErrorCode) -> ControlError {
    match code {
        ControlErrorCode::HelloRequired => ControlError::hello_required(),
        ControlErrorCode::ProtocolMismatch => ControlError::protocol_mismatch(2),
        ControlErrorCode::MalformedRequest => {
            ControlError::malformed_request("лишнее поле `extra`")
        }
        ControlErrorCode::RequestTooLarge => ControlError::request_too_large(REQUEST_LINE_LIMIT),
        ControlErrorCode::ScopeRequired => ControlError::scope_required("command"),
        ControlErrorCode::ScopeUnsupported => ControlError::scope_unsupported("subscribe"),
        ControlErrorCode::OwnerLeaving => ControlError::owner_leaving(),
        ControlErrorCode::NotRunning => ControlError::not_running(),
        ControlErrorCode::OwnerStarting => ControlError::owner_starting(),
        ControlErrorCode::Unavailable => ControlError::unavailable("срок 5000 мс истёк"),
        ControlErrorCode::OutcomeUnknown => {
            ControlError::outcome_unknown("соединение оборвалось после отправки запроса")
        }
    }
}

fn error_samples() -> Samples {
    ControlErrorCode::ALL
        .into_iter()
        .map(|code| {
            let error = error_sample(code);
            assert_eq!(error.code(), code);
            sample(format!("error.{code}"), error)
        })
        .collect()
}

fn all_samples() -> Result<Samples, String> {
    Ok([request_samples(), response_samples()?, error_samples()]
        .into_iter()
        .flatten()
        .collect())
}

#[test]
fn requests_match_goldens() {
    assert_goldens(&request_samples());
}

#[test]
fn responses_match_goldens() {
    assert_goldens(&response_samples().unwrap());
}

#[test]
fn errors_match_goldens() {
    assert_goldens(&error_samples());
}

#[test]
fn request_golden_names_follow_frame_keys() {
    for request in requests() {
        assert_eq!(
            request_golden(&request),
            format!("request.{}", request.body.kind())
        );
    }
}

#[test]
fn request_goldens_pass_strict_parse() {
    for request in requests() {
        let name = request_golden(&request);
        let text = fs::read_to_string(wire_dir().join(format!("{name}.json"))).unwrap();
        assert_eq!(Request::parse(text.as_bytes()), Ok(request), "{name}");
    }
}

#[test]
fn every_golden_has_exactly_one_sample() {
    let mut names = BTreeMap::new();
    for (name, _) in all_samples().unwrap() {
        *names.entry(name).or_insert(0_u32) += 1;
    }
    let duplicates: Vec<_> = names.iter().filter(|(_, count)| **count > 1).collect();
    assert!(
        duplicates.is_empty(),
        "эталоны с несколькими образцами: {duplicates:?}"
    );

    let files: BTreeSet<String> = fs::read_dir(wire_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let expected: BTreeSet<String> = names.keys().map(|name| format!("{name}.json")).collect();
    let orphans: Vec<_> = files.difference(&expected).collect();
    assert!(orphans.is_empty(), "эталоны без образца: {orphans:?}");
    let missing: Vec<_> = expected.difference(&files).collect();
    assert!(missing.is_empty(), "образцы без эталона: {missing:?}");
}

/// Эталоны P1.1 идут внутри кадров без изменений: кадр с ними разбирается, даёт ожидаемый вид
/// и сериализуется обратно в тот же JSON.
#[test]
fn p1_1_messages_travel_inside_frames_unchanged() {
    let mut checked = 0;
    for entry in fs::read_dir(core_wire_dir()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let message: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let prefix = name.split('.').next().unwrap_or_default();
        match prefix {
            "command" | "query" => {
                let frame = if prefix == "command" {
                    json!({"id": 9, "command": message, "scope": "all"})
                } else {
                    json!({"id": 9, "query": message})
                };
                let request = Request::parse(frame.to_string().as_bytes())
                    .unwrap_or_else(|rejected| panic!("{name}: {rejected:?}"));
                assert_eq!(request.body.kind(), prefix, "{name}");
                assert_eq!(serde_json::to_value(&request).unwrap(), frame, "{name}");
            }
            "reply" | "events_since" | "error" | "event" => {
                let key = match prefix {
                    "error" => "core_error",
                    "event" => "event",
                    _ => "ok",
                };
                let frame = json!({"id": 9, key: message});
                let response = Response::parse(frame.to_string().as_bytes())
                    .unwrap_or_else(|error| panic!("{name}: {error}"));
                let kind_matches = matches!(
                    (&response, prefix),
                    (
                        Response::Ok {
                            value: OkValue::Reply(_),
                            ..
                        },
                        "reply",
                    ) | (
                        Response::Ok {
                            value: OkValue::EventsSince(_),
                            ..
                        },
                        "events_since",
                    ) | (Response::CoreError { .. }, "error")
                        | (Response::Event { .. }, "event")
                );
                assert!(kind_matches, "{name}: разобран как {response:?}");
                assert_eq!(serde_json::to_value(&response).unwrap(), frame, "{name}");
            }
            // Изменения идут внутри событий.
            "change" => continue,
            other => panic!("{name}: эталон P1.1 вида «{other}» не отнесён к кадру"),
        }
        checked += 1;
    }
    assert!(checked > 20, "проверено {checked} эталонов P1.1");
}

fn rejected(line: &[u8]) -> Rejected {
    match Request::parse(line) {
        Ok(request) => panic!("{} разобран как {request:?}", String::from_utf8_lossy(line)),
        Err(rejected) => rejected,
    }
}

/// Строка отвергается кодом `code` с номером `id`, сообщение содержит `reason`.
fn assert_rejected(line: &str, code: ControlErrorCode, id: Option<u64>, reason: &str) {
    let rejected = rejected(line.as_bytes());
    assert_eq!(rejected.error.code(), code, "{line}: {:?}", rejected.error);
    assert_eq!(rejected.id, id.map(RequestId), "{line}");
    assert!(
        rejected.error.message().contains(reason),
        "{line}: «{}» не содержит «{reason}»",
        rejected.error.message()
    );
}

fn assert_malformed(line: &str, id: Option<u64>, reason: &str) {
    assert_rejected(line, ControlErrorCode::MalformedRequest, id, reason);
}

const HELLO: &str = r#"{"protocol":1,"client":"test"}"#;
const ID: &str = "00000000-0000-0000-0000-000000000001";

#[test]
fn extra_field_is_malformed_and_keeps_id() {
    assert_malformed(
        &format!(r#"{{"id":1,"hello":{HELLO},"extra":1}}"#),
        Some(1),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":1,"hello":{"protocol":1,"client":"test","extra":1}}"#,
        Some(1),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":2,"command":{"cmd":"workspace.create","title":"Работа","extra":1},"scope":"all"}"#,
        Some(2),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":3,"query":{"query":"status","extra":1}}"#,
        Some(3),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":4,"events_since":{"cursor":1,"extra":1},"scope":"all"}"#,
        Some(4),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":5,"subscribe":{"extra":1},"scope":"all"}"#,
        Some(5),
        "unknown field `extra`",
    );
    assert_malformed(
        r#"{"id":6,"launch":{"args":[],"cwd":"/","extra":1}}"#,
        Some(6),
        "unknown field `extra`",
    );
}

#[test]
fn scope_on_hello_query_and_launch_is_malformed() {
    for scope in [r#""all""#, &format!(r#"{{"workspace":"{ID}"}}"#)] {
        assert_malformed(
            &format!(r#"{{"id":1,"hello":{HELLO},"scope":{scope}}}"#),
            Some(1),
            "поле scope у запроса hello не допускается",
        );
        assert_malformed(
            &format!(r#"{{"id":3,"query":{{"query":"status"}},"scope":{scope}}}"#),
            Some(3),
            "поле scope у запроса query не допускается",
        );
        assert_malformed(
            &format!(r#"{{"id":6,"launch":{{"args":[],"cwd":"/"}},"scope":{scope}}}"#),
            Some(6),
            "поле scope у запроса launch не допускается",
        );
    }
}

#[test]
fn null_in_place_of_body_or_scope_is_malformed() {
    // Внешне помеченный перечень области на `null` отвечает «expected value».
    assert_malformed(
        &format!(r#"{{"id":1,"hello":{HELLO},"scope":null}}"#),
        Some(1),
        "expected value",
    );
    assert_malformed(r#"{"id":3,"query":null}"#, Some(3), "null");
    assert_malformed(
        r#"{"id":5,"subscribe":null,"scope":"all"}"#,
        Some(5),
        "null",
    );
    assert_malformed(
        &format!(r#"{{"id":1,"hello":{HELLO},"query":null}}"#),
        Some(1),
        "null",
    );
}

#[test]
fn frame_needs_exactly_one_kind() {
    assert_malformed(r#"{"id":1}"#, Some(1), "нет вида запроса");
    assert_malformed(r#"{"id":1,"scope":"all"}"#, Some(1), "нет вида запроса");
    assert_malformed(
        &format!(r#"{{"id":1,"hello":{HELLO},"query":{{"query":"status"}}}}"#),
        Some(1),
        "2 вида запроса",
    );
    assert_malformed(
        r#"{"id":1,"events_since":{"cursor":1},"subscribe":{},"scope":"all"}"#,
        Some(1),
        "2 вида запроса",
    );
}

#[test]
fn unknown_variant_is_malformed() {
    assert_malformed(
        r#"{"id":1,"shutdown":{}}"#,
        Some(1),
        "unknown field `shutdown`",
    );
    assert_malformed(
        &format!(
            r#"{{"id":2,"command":{{"cmd":"workspace.delete","workspace":"{ID}"}},"scope":"all"}}"#
        ),
        Some(2),
        "unknown variant `workspace.delete`",
    );
    assert_malformed(
        r#"{"id":3,"query":{"query":"tree.all"}}"#,
        Some(3),
        "unknown variant `tree.all`",
    );
    assert_malformed(
        r#"{"id":4,"events_since":{"cursor":1},"scope":"none"}"#,
        Some(4),
        "unknown variant `none`",
    );
}

#[test]
fn missing_or_non_numeric_id_is_malformed_without_id() {
    assert_malformed(
        &format!(r#"{{"hello":{HELLO}}}"#),
        None,
        "missing field `id`",
    );
    for id in [
        r#""1""#,
        "1.5",
        "-1",
        "null",
        "true",
        "18446744073709551616",
    ] {
        assert_malformed(&format!(r#"{{"id":{id},"hello":{HELLO}}}"#), None, "");
    }
    assert_malformed(
        &format!(r#"{{"id":"1","hello":{HELLO},"extra":1}}"#),
        None,
        "",
    );
}

#[test]
fn not_a_json_object_is_malformed_without_id() {
    for line in [&b""[..], b"hello", b"{\"id\":1", b"[]", b"1", b"\xff\xfe"] {
        let rejected = rejected(line);
        assert_eq!(rejected.error.code(), ControlErrorCode::MalformedRequest);
        assert_eq!(rejected.id, None, "{}", String::from_utf8_lossy(line));
    }
}

#[test]
fn largest_id_is_kept() {
    let request =
        Request::parse(format!(r#"{{"id":{},"hello":{HELLO}}}"#, u64::MAX).as_bytes()).unwrap();
    assert_eq!(request.id, RequestId(u64::MAX));
}

#[test]
fn missing_scope_is_scope_required() {
    assert_rejected(
        r#"{"id":2,"command":{"cmd":"workspace.create","title":"Работа"}}"#,
        ControlErrorCode::ScopeRequired,
        Some(2),
        "command",
    );
    assert_rejected(
        r#"{"id":4,"events_since":{"cursor":1}}"#,
        ControlErrorCode::ScopeRequired,
        Some(4),
        "events_since",
    );
    assert_rejected(
        r#"{"id":5,"subscribe":{}}"#,
        ControlErrorCode::ScopeRequired,
        Some(5),
        "subscribe",
    );
}

#[test]
fn workspace_scope_of_events_is_unsupported() {
    let scope = format!(r#"{{"workspace":"{ID}"}}"#);
    assert_rejected(
        &format!(r#"{{"id":4,"events_since":{{"cursor":1}},"scope":{scope}}}"#),
        ControlErrorCode::ScopeUnsupported,
        Some(4),
        "events_since",
    );
    assert_rejected(
        &format!(r#"{{"id":5,"subscribe":{{}},"scope":{scope}}}"#),
        ControlErrorCode::ScopeUnsupported,
        Some(5),
        "subscribe",
    );
}

#[test]
fn command_takes_either_scope() {
    for (scope, expected) in [
        (r#""all""#.to_owned(), TreeScope::All),
        (
            format!(r#"{{"workspace":"{ID}"}}"#),
            TreeScope::Workspace(workspace_id()),
        ),
    ] {
        let request = Request::parse(
            format!(r#"{{"id":2,"command":{{"cmd":"workspace.create","title":"Работа"}},"scope":{scope}}}"#)
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            request.body,
            RequestBody::Command {
                command: Command::WorkspaceCreate {
                    title: "Работа".into()
                },
                scope: expected,
            }
        );
    }
}

#[test]
fn rejection_becomes_error_frame() {
    let response = rejected(br#"{"id":7,"shutdown":{}}"#).into_response();
    let json = serde_json::to_value(&response).unwrap();
    assert_eq!(json["id"], json!(7));
    assert_eq!(json["error"]["code"], json!("malformed_request"));
    assert_eq!(json["error"]["retryable"], json!(false));

    let response = rejected(b"not json").into_response();
    assert_eq!(serde_json::to_value(&response).unwrap()["id"], Value::Null);
}

/// Кадр сервера отвергается, и причина — ожидаемая.
fn assert_response_rejected(line: &str, reason: &str) {
    match Response::parse(line.as_bytes()) {
        Ok(response) => panic!("{line} разобран как {response:?}"),
        Err(error) => assert!(
            error.to_string().contains(reason),
            "{line}: «{error}» не содержит «{reason}»"
        ),
    }
}

const HELLO_REPLY: &str = r#"{"protocol":1,"server":"0.1.0","pid":1,"owner":"ready"}"#;
const LEAVING: &str = r#"{"code":"owner_leaving","message":"","retryable":true,"details":{}}"#;

#[test]
fn response_frame_is_strict() {
    assert_response_rejected(
        &format!(r#"{{"id":1,"ok":{{"hello":{HELLO_REPLY}}},"extra":1}}"#),
        "unknown field `extra`",
    );
    assert_response_rejected(
        r#"{"ok":{"revision":1,"value":null}}"#,
        "missing field `id`",
    );
    assert_response_rejected(
        r#"{"id":null,"ok":{"revision":1,"value":null}}"#,
        "id равен null только у кадра error",
    );
    assert_response_rejected(r#"{"id":1}"#, "нет вида ответа");
    assert_response_rejected(
        &format!(r#"{{"id":1,"error":{LEAVING},"ok":{{"revision":1,"value":null}}}}"#),
        "2 вида ответа",
    );
    assert_response_rejected(r#"{"id":1,"ok":null}"#, "did not match any variant");
    assert_response_rejected(
        &format!(r#"{{"id":1,"error":{LEAVING},"ok":null}}"#),
        "did not match any variant",
    );
    assert_response_rejected(r#"{"id":"1","error":{LEAVING}}"#, "invalid type");
}

#[test]
fn ok_value_is_strict() {
    for ok in [
        r#"{"status":{}}"#.to_owned(),
        format!(r#"{{"hello":{HELLO_REPLY},"extra":1}}"#),
        r#"{"hello":{"protocol":1,"server":"0.1.0","pid":1,"owner":"ready","extra":1}}"#.to_owned(),
        r#"{"hello":{"protocol":1,"server":"0.1.0","pid":1,"owner":"starting"}}"#.to_owned(),
        r#"{"launch":{"activation":"requested","refused":[{"arg":"x","reason":"busy"}]}}"#
            .to_owned(),
        r#"{"launch":{"activation":"done","refused":[]}}"#.to_owned(),
        r#"{"launch":{"activation":"requested","refused":[],"extra":1}}"#.to_owned(),
        r#"{"revision":1,"value":null,"extra":1}"#.to_owned(),
        r#"{"events":[],"extra":1}"#.to_owned(),
    ] {
        assert_response_rejected(
            &format!(r#"{{"id":1,"ok":{ok}}}"#),
            "did not match any variant",
        );
    }
}

#[test]
fn error_frame_checks_retry_flag() {
    assert_response_rejected(
        r#"{"id":1,"error":{"code":"owner_leaving","message":"","retryable":false,"details":{}}}"#,
        "retryable",
    );
    assert_response_rejected(
        r#"{"id":1,"error":{"code":"gone","message":"","retryable":false,"details":{}}}"#,
        "unknown variant `gone`",
    );
    assert_response_rejected(
        r#"{"id":1,"error":{"code":"owner_leaving","message":"","retryable":true,"details":{},"extra":1}}"#,
        "unknown field `extra`",
    );
}

#[test]
fn launch_receipt_reasons_round_trip() {
    let receipt = LaunchReceipt {
        activation: Activation::NoUi,
        refused: RefuseReason::ALL
            .into_iter()
            .map(|reason| RefusedArg {
                arg: reason.as_str().into(),
                reason,
            })
            .collect(),
    };
    let response = Response::Ok {
        id: RequestId(6),
        value: OkValue::Launch(receipt),
    };
    let line = serde_json::to_vec(&response).unwrap();
    assert_eq!(Response::parse(&line).unwrap(), response);
}
