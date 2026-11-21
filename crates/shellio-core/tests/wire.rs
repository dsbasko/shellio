//! Эталоны JSON транспортной границы в `tests/wire/*.json`.
//!
//! Каждый эталон сравнивается со своим образцом в обе стороны: сериализация образца равна
//! эталону как значение JSON, разбор эталона даёт образец. Значения сравниваются, а не текст:
//! порядок ключей карты деталей ошибки зависит от функций `serde_json` в сборке.
//! Эталон меняется только вместе с записью причины в плане.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use shellio_core::test_support::SequentialIds;
use shellio_core::{
    Change, Command, CommandValue, CoreError, Diagnostic, Entity, ErrorCode, Event, IdGen, Project,
    ProjectId, ProjectRoot, ProjectRootId, Query, Reply, ReplyValue, Revision, RootError, RootKey,
    RootKind, Route, State, StorageInfo, Title, TreeScope, UnixMillis, WindowState, Workbench,
    WorkbenchId, Workspace, WorkspaceId, read,
};

fn wire_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wire")
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

/// Идентификаторы образцов: последовательные, поэтому эталоны читаемы.
struct Ids {
    workspace: WorkspaceId,
    project: ProjectId,
    root: ProjectRootId,
    workbench: WorkbenchId,
    second_workspace: WorkspaceId,
    second_project: ProjectId,
    second_root: ProjectRootId,
    second_workbench: WorkbenchId,
}

fn ids() -> Ids {
    let mut generator = SequentialIds::new();
    let generator: &mut dyn IdGen = &mut generator;
    Ids {
        workspace: WorkspaceId::generate(generator),
        project: ProjectId::generate(generator),
        root: ProjectRootId::generate(generator),
        workbench: WorkbenchId::generate(generator),
        second_workspace: WorkspaceId::generate(generator),
        second_project: ProjectId::generate(generator),
        second_root: ProjectRootId::generate(generator),
        second_workbench: WorkbenchId::generate(generator),
    }
}

fn command_golden(command: &Command) -> &'static str {
    match command {
        Command::WorkspaceCreate { .. } => "command.workspace.create",
        Command::WorkspaceRename { .. } => "command.workspace.rename",
        Command::ProjectAdd { .. } => "command.project.add",
        Command::WorkbenchCreate { .. } => "command.workbench.create",
        Command::WorkbenchRename { .. } => "command.workbench.rename",
        Command::WorkbenchOpen { .. } => "command.workbench.open",
        Command::WindowFocusWorkspace { .. } => "command.window.focus_workspace",
    }
}

fn commands() -> Vec<Command> {
    let ids = ids();
    vec![
        Command::WorkspaceCreate {
            title: "Работа".into(),
        },
        Command::WorkspaceRename {
            workspace: ids.workspace,
            title: "Дом".into(),
        },
        Command::ProjectAdd {
            workspace: ids.workspace,
            path: "/src/shellio".into(),
            title: Some("shellio".into()),
        },
        Command::WorkbenchCreate {
            project: ids.project,
            title: "Ревью".into(),
        },
        Command::WorkbenchRename {
            workbench: ids.workbench,
            title: "Основная".into(),
        },
        Command::WorkbenchOpen {
            workbench: ids.workbench,
        },
        Command::WindowFocusWorkspace {
            workspace: ids.workspace,
        },
    ]
}

fn command_samples() -> Samples {
    commands()
        .into_iter()
        .map(|command| sample(command_golden(&command), command))
        .collect()
}

fn query_golden(query: &Query) -> &'static str {
    match query {
        Query::Tree {
            scope: TreeScope::Workspace(_),
        } => "query.tree.workspace",
        Query::Tree {
            scope: TreeScope::All,
        } => "query.tree.all",
        Query::Status {} => "query.status",
    }
}

fn query_samples() -> Samples {
    let ids = ids();
    [
        Query::Tree {
            scope: TreeScope::Workspace(ids.workspace),
        },
        Query::Tree {
            scope: TreeScope::All,
        },
        Query::Status {},
    ]
    .into_iter()
    .map(|query| sample(query_golden(&query), query))
    .collect()
}

fn change_golden(change: &Change) -> &'static str {
    match change {
        Change::WorkspaceInserted { .. } => "change.workspace_inserted",
        Change::WorkspaceRenamed { .. } => "change.workspace_renamed",
        Change::WorkspaceLastWorkbenchSet { .. } => "change.workspace_last_workbench_set",
        Change::ProjectInserted { .. } => "change.project_inserted",
        Change::ProjectOpened { .. } => "change.project_opened",
        Change::WorkbenchInserted { .. } => "change.workbench_inserted",
        Change::WorkbenchRenamed { .. } => "change.workbench_renamed",
        Change::WindowSet { .. } => "change.window_set",
    }
}

fn change_samples() -> Result<Samples, String> {
    let ids = ids();
    let changes = [
        Change::WorkspaceInserted {
            workspace: Workspace {
                id: ids.workspace,
                title: title("Работа")?,
                position: 0,
                last_workbench: None,
                deleted_at: None,
            },
        },
        Change::WorkspaceRenamed {
            workspace: ids.workspace,
            title: title("Дом")?,
        },
        Change::WorkspaceLastWorkbenchSet {
            workspace: ids.workspace,
            workbench: Some(ids.workbench),
        },
        Change::ProjectInserted {
            project: Project {
                id: ids.project,
                workspace: ids.workspace,
                title: title("shellio")?,
                list_position: Some(0),
                removed_at: None,
                last_opened_at: None,
            },
            roots: vec![ProjectRoot {
                id: ids.root,
                project: ids.project,
                path: "/src/shellio".into(),
                kind: RootKind::Git,
                key: RootKey::new("/src/shellio/.git"),
            }],
        },
        Change::ProjectOpened {
            project: ids.project,
            at: UnixMillis(1_795_000_000_000),
        },
        Change::WorkbenchInserted {
            workbench: Workbench {
                id: ids.workbench,
                project: ids.project,
                title: title("Основная")?,
                position: 0,
            },
        },
        Change::WorkbenchRenamed {
            workbench: ids.workbench,
            title: title("Ревью")?,
        },
        Change::WindowSet {
            active: ids.workspace,
            route: Route::WorkspaceHome,
        },
    ];
    Ok(changes
        .into_iter()
        .map(|change| sample(change_golden(&change), change))
        .collect())
}

fn error_sample(code: ErrorCode) -> Result<CoreError, String> {
    let ids = ids();
    Ok(match code {
        ErrorCode::InvalidArgument => CoreError::from(RootError::NotAbsolute {
            path: "src/shellio".into(),
        }),
        ErrorCode::NotFound => CoreError::not_found(Entity::Workspace, ids.workspace),
        ErrorCode::NameTaken => {
            CoreError::name_taken(Entity::Project, &title("shellio")?, ids.project)
        }
        ErrorCode::RootTaken => CoreError::root_taken(
            &RootKey::new("/src/shellio/.git"),
            ids.project,
            ids.workspace,
        ),
        ErrorCode::StorageUnavailable => {
            CoreError::storage_unavailable("запись в state.db не удалась: disk I/O error")
        }
        ErrorCode::StorageConflict => {
            CoreError::storage_conflict("ревизия базы 5 не совпадает с ревизией ядра 4")
        }
        ErrorCode::Busy => CoreError::busy(),
        ErrorCode::ShuttingDown => CoreError::shutting_down(),
    })
}

fn error_samples() -> Result<Samples, String> {
    ErrorCode::ALL
        .into_iter()
        .map(|code| {
            let error = error_sample(code)?;
            assert_eq!(error.code(), code);
            Ok(sample(format!("error.{code}"), error))
        })
        .collect()
}

/// Состояние для эталона дерева: воркспейс с проектом в списке и проектом вне списка,
/// открытая рабочая область и пустой второй воркспейс.
fn tree_state(ids: &Ids) -> Result<State, String> {
    let mut state = State {
        window: WindowState {
            active_workspace: Some(ids.workspace),
            route: Route::Workbench(ids.workbench),
        },
        ..State::default()
    };
    for (id, name, position, last_workbench) in [
        (ids.workspace, "Работа", 0, Some(ids.workbench)),
        (ids.second_workspace, "Дом", 1, None),
    ] {
        state.workspaces.insert(
            id,
            Workspace {
                id,
                title: title(name)?,
                position,
                last_workbench,
                deleted_at: None,
            },
        );
    }
    for (id, name, list_position, last_opened_at) in [
        (
            ids.project,
            "shellio",
            Some(0),
            Some(UnixMillis(1_795_000_000_000)),
        ),
        (ids.second_project, "notes", None, None),
    ] {
        state.projects.insert(
            id,
            Project {
                id,
                workspace: ids.workspace,
                title: title(name)?,
                list_position,
                removed_at: None,
                last_opened_at,
            },
        );
    }
    for (id, project, path, kind, key) in [
        (
            ids.root,
            ids.project,
            "/src/shellio",
            RootKind::Git,
            "/src/shellio/.git",
        ),
        (
            ids.second_root,
            ids.second_project,
            "/src/notes",
            RootKind::Folder,
            "/src/notes",
        ),
    ] {
        state.roots.insert(
            id,
            ProjectRoot {
                id,
                project,
                path: path.into(),
                kind,
                key: RootKey::new(key),
            },
        );
    }
    for (id, name, position) in [
        (ids.workbench, "Основная", 0),
        (ids.second_workbench, "Ревью", 1),
    ] {
        state.workbenches.insert(
            id,
            Workbench {
                id,
                project: ids.project,
                title: title(name)?,
                position,
            },
        );
    }
    assert_eq!(state.check_invariants(), Vec::new());
    Ok(state)
}

fn storage() -> StorageInfo {
    StorageInfo {
        schema_version: 1,
        state_db: "/state/state.db".into(),
    }
}

fn reply_golden(reply: &Reply) -> &'static str {
    match &reply.value {
        ReplyValue::Tree(_) => "reply.tree",
        ReplyValue::Status(_) => "reply.status",
        ReplyValue::Command(CommandValue::Workspace { .. }) => "reply.workspace",
        ReplyValue::Command(CommandValue::Project { .. }) => "reply.project",
        ReplyValue::Command(CommandValue::Workbench { .. }) => "reply.workbench",
        ReplyValue::Command(CommandValue::None) => "reply.none",
    }
}

fn reply_samples() -> Result<Samples, String> {
    let ids = ids();
    let mut state = tree_state(&ids)?;
    let tree = read(
        &state,
        Revision(12),
        &storage(),
        &Query::Tree {
            scope: TreeScope::All,
        },
    )
    .map_err(|error| error.to_string())?;
    state.diagnostics.push(Diagnostic {
        table: "projects".into(),
        rowid: 7,
        reason: "висячая ссылка workspace_id".into(),
    });
    let status = read(&state, Revision(12), &storage(), &Query::Status {})
        .map_err(|error| error.to_string())?;
    let command = |revision, value: CommandValue| Reply {
        revision: Revision(revision),
        value: value.into(),
    };
    let replies = [
        tree,
        status,
        command(
            1,
            CommandValue::Workspace {
                workspace: ids.workspace,
            },
        ),
        command(
            2,
            CommandValue::Project {
                project: ids.project,
                root: ids.root,
            },
        ),
        command(
            3,
            CommandValue::Workbench {
                workbench: ids.workbench,
            },
        ),
        command(12, CommandValue::None),
    ];
    Ok(replies
        .into_iter()
        .map(|reply| sample(reply_golden(&reply), reply))
        .collect())
}

fn event_samples() -> Samples {
    let ids = ids();
    vec![sample(
        "event",
        Event {
            revision: Revision(12),
            changes: vec![
                Change::WorkspaceLastWorkbenchSet {
                    workspace: ids.workspace,
                    workbench: Some(ids.workbench),
                },
                Change::WindowSet {
                    active: ids.workspace,
                    route: Route::Workbench(ids.workbench),
                },
            ],
        },
    )]
}

fn all_samples() -> Result<Samples, String> {
    Ok([
        command_samples(),
        query_samples(),
        change_samples()?,
        error_samples()?,
        reply_samples()?,
        event_samples(),
    ]
    .into_iter()
    .flatten()
    .collect())
}

#[test]
fn commands_match_goldens() {
    assert_goldens(&command_samples());
}

#[test]
fn command_golden_names_follow_command_names() {
    for command in commands() {
        assert_eq!(
            command_golden(&command),
            format!("command.{}", command.name())
        );
    }
}

#[test]
fn queries_match_goldens() {
    assert_goldens(&query_samples());
}

#[test]
fn changes_match_goldens() {
    assert_goldens(&change_samples().unwrap());
}

#[test]
fn errors_match_goldens() {
    assert_goldens(&error_samples().unwrap());
}

#[test]
fn replies_match_goldens() {
    assert_goldens(&reply_samples().unwrap());
}

#[test]
fn events_match_goldens() {
    assert_goldens(&event_samples());
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

/// Разбор отвергается, и причина — ожидаемая.
fn assert_rejected<T: DeserializeOwned + Debug>(json: &str, reason: &str) {
    match serde_json::from_str::<T>(json) {
        Ok(value) => panic!("{json} разобран как {value:?}"),
        Err(error) => assert!(
            error.to_string().contains(reason),
            "{json}: «{error}» не содержит «{reason}»"
        ),
    }
}

const ID: &str = "00000000-0000-0000-0000-000000000001";

#[test]
fn unknown_command_is_rejected() {
    assert_rejected::<Command>(
        &format!(r#"{{"cmd":"workspace.delete","workspace":"{ID}"}}"#),
        "unknown variant `workspace.delete`",
    );
    assert_rejected::<Command>(r#"{"title":"Работа"}"#, "missing field `cmd`");
}

#[test]
fn extra_command_field_is_rejected() {
    assert_rejected::<Command>(
        r#"{"cmd":"workspace.create","title":"Работа","extra":1}"#,
        "unknown field `extra`",
    );
    assert_rejected::<Command>(
        &format!(r#"{{"cmd":"workbench.open","workbench":"{ID}","workspace":"{ID}"}}"#),
        "unknown field `workspace`",
    );
    assert_rejected::<Command>(
        &format!(r#"{{"cmd":"project.add","workspace":"{ID}","path":"/src","root":"/src"}}"#),
        "unknown field `root`",
    );
}

#[test]
fn unknown_query_and_extra_query_field_are_rejected() {
    assert_rejected::<Query>(r#"{"query":"tree.all"}"#, "unknown variant `tree.all`");
    assert_rejected::<Query>(r#"{"query":"status","extra":1}"#, "unknown field `extra`");
    assert_rejected::<Query>(
        r#"{"query":"tree","scope":"all","extra":1}"#,
        "unknown field `extra`",
    );
    assert_rejected::<Query>(
        &format!(r#"{{"query":"tree","scope":{{"workspace":"{ID}","extra":1}}}}"#),
        "expected map with a single key",
    );
    assert_rejected::<Query>(
        r#"{"query":"tree","scope":"none"}"#,
        "unknown variant `none`",
    );
    assert_rejected::<Query>(r#"{"query":"tree"}"#, "missing field `scope`");
}

#[test]
fn extra_fields_of_outgoing_messages_are_rejected() {
    assert_rejected::<Reply>(
        &format!(r#"{{"revision":1,"value":{{"workspace":"{ID}","extra":1}}}}"#),
        "did not match any variant",
    );
    assert_rejected::<Reply>(
        r#"{"revision":1,"value":null,"extra":1}"#,
        "unknown field `extra`",
    );
    assert_rejected::<Event>(
        r#"{"revision":1,"changes":[],"extra":1}"#,
        "unknown field `extra`",
    );
    assert_rejected::<Change>(
        &format!(r#"{{"change":"workspace_renamed","workspace":"{ID}","title":"Дом","extra":1}}"#),
        "unknown field `extra`",
    );
    assert_rejected::<CoreError>(
        r#"{"code":"busy","message":"","retryable":true,"details":{},"extra":1}"#,
        "unknown field `extra`",
    );
}

#[test]
fn command_reply_without_value_is_null() {
    let reply = Reply {
        revision: Revision(4),
        value: CommandValue::None.into(),
    };
    assert_eq!(
        serde_json::to_string(&reply).unwrap(),
        r#"{"revision":4,"value":null}"#
    );
}
