//! Запись дельт `SqlitePersist` в настоящий `state.db`: каждая дельта, рост ревизии на 1
//! и отказы записи, которые не меняют ни базу, ни ревизию.

use std::collections::BTreeMap;
use std::error::Error;
use std::path::PathBuf;

use rusqlite::Connection;
use shellio_core::test_support::{ManualClock, SequentialIds};
use shellio_core::{
    Change, CommitError, CoreError, ErrorCode, IdGen, Loaded, Panel, PanelData, PanelId,
    PanelStatus, Persist, PluginId, Project, ProjectId, ProjectRoot, ProjectRootId, Revision,
    RootKey, RootKind, Route, State, Title, UnixMillis, Workbench, WorkbenchId, WorkbenchLayout,
    Workspace, WorkspaceId,
};
use shellio_store::{Database, Profile, SqlitePersist};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const NOW: UnixMillis = UnixMillis(1_795_000_000_000);

struct Store {
    _dir: TempDir,
    profile: Profile,
    persist: SqlitePersist,
}

impl Store {
    fn new() -> TestResult<Self> {
        let dir = TempDir::new()?;
        let profile = Profile::isolated(dir.path().join("state"));
        let persist = open(&profile)?;
        Ok(Self {
            _dir: dir,
            profile,
            persist,
        })
    }

    /// Загрузка новым хранилищем на той же базе.
    fn reload(&self) -> TestResult<Loaded> {
        open(&self.profile)?.load()
    }

    /// Соединение с `state.db` в обход хранилища.
    fn raw(&self) -> TestResult<Connection> {
        Ok(Connection::open(self.profile.state_db())?)
    }

    fn revision_in_db(&self) -> TestResult<i64> {
        Ok(self
            .raw()?
            .query_row("SELECT value FROM meta WHERE key = 'revision'", [], |row| {
                row.get(0)
            })?)
    }

    fn count(&self, table: &str) -> TestResult<i64> {
        Ok(self
            .raw()?
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })?)
    }

    /// Отказ записи `changes` поверх `base` не изменил ни базу, ни ревизию.
    fn assert_refused(&mut self, base: Revision, changes: &[Change]) -> TestResult<CommitError> {
        let before = self.reload()?;
        let error = match self.persist.commit(base, changes, None) {
            Ok(()) => return Err(format!("запись {changes:?} прошла").into()),
            Err(error) => error,
        };
        assert_eq!(self.reload()?, before, "отказ {error} изменил базу");
        Ok(error)
    }
}

fn open(profile: &Profile) -> TestResult<SqlitePersist> {
    Ok(SqlitePersist::new(Database::open(
        profile,
        &ManualClock::new(NOW),
    )?))
}

fn title(name: &str) -> TestResult<Title> {
    Ok(Title::new(name)?)
}

fn workspace(ids: &mut dyn IdGen, name: &str, position: u32) -> TestResult<Workspace> {
    Ok(Workspace {
        id: WorkspaceId::generate(ids),
        title: title(name)?,
        position,
        last_workbench: None,
        deleted_at: None,
    })
}

fn project(ids: &mut dyn IdGen, workspace: WorkspaceId, name: &str) -> TestResult<Project> {
    Ok(Project {
        id: ProjectId::generate(ids),
        workspace,
        title: title(name)?,
        list_position: Some(0),
        removed_at: None,
        last_opened_at: None,
    })
}

fn root(ids: &mut dyn IdGen, project: ProjectId, path: &str, kind: RootKind) -> ProjectRoot {
    let key = match kind {
        RootKind::Git => format!("{path}/.git"),
        RootKind::Folder => path.to_owned(),
    };
    ProjectRoot {
        id: ProjectRootId::generate(ids),
        project,
        path: PathBuf::from(path),
        kind,
        key: RootKey::new(key),
    }
}

fn workbench(
    ids: &mut dyn IdGen,
    project: ProjectId,
    name: &str,
    position: u32,
) -> TestResult<Workbench> {
    Ok(Workbench {
        id: WorkbenchId::generate(ids),
        project,
        title: title(name)?,
        position,
    })
}

fn panel(ids: &mut dyn IdGen, workbench: WorkbenchId, plugin: &str) -> TestResult<Panel> {
    Ok(Panel {
        id: PanelId::generate(ids),
        workbench,
        plugin: PluginId::new(plugin)?,
        title: title(plugin)?,
        created_at: NOW,
    })
}

fn assert_conflict(error: &CommitError) {
    assert!(
        matches!(error, CommitError::Conflict(_)),
        "ожидался конфликт, получено {error:?}"
    );
    assert_eq!(
        CoreError::from(error.clone()).code(),
        ErrorCode::StorageConflict
    );
}

/// Воркспейс с проектом и рабочей областью, записанные ревизией 1.
struct Seed {
    store: Store,
    ids: SequentialIds,
    workspace: Workspace,
    project: Project,
    workbench: Workbench,
}

fn seed() -> TestResult<Seed> {
    let mut store = Store::new()?;
    store.persist.load()?;
    let mut ids = SequentialIds::new();
    let workspace = workspace(&mut ids, "Работа", 0)?;
    let project = project(&mut ids, workspace.id, "shellio")?;
    let root = root(&mut ids, project.id, "/src/shellio", RootKind::Git);
    let workbench = workbench(&mut ids, project.id, "main", 0)?;
    store.persist.commit(
        Revision::ZERO,
        &[
            Change::WorkspaceInserted {
                workspace: workspace.clone(),
            },
            Change::WindowSet {
                active: workspace.id,
                route: Route::WorkspaceHome,
            },
            Change::ProjectInserted {
                project: project.clone(),
                roots: vec![root],
            },
            Change::WorkbenchInserted {
                workbench: workbench.clone(),
            },
        ],
        None,
    )?;
    Ok(Seed {
        store,
        ids,
        workspace,
        project,
        workbench,
    })
}

#[test]
fn every_change_is_written_and_reloads() {
    let mut store = Store::new().unwrap();
    let loaded = store.persist.load().unwrap();
    assert_eq!(loaded.revision, Revision::ZERO);
    assert_eq!(loaded.state, State::default());
    assert_eq!(loaded.storage.schema_version, 3);
    assert_eq!(loaded.storage.state_db, store.profile.state_db());

    let mut ids = SequentialIds::new();
    let work = workspace(&mut ids, "Работа", 0).unwrap();
    let home = workspace(&mut ids, "Дом", 1).unwrap();
    let archive = Workspace {
        deleted_at: Some(UnixMillis(NOW.0 - 1)),
        ..workspace(&mut ids, "Архив", 2).unwrap()
    };
    let app = project(&mut ids, work.id, "shellio").unwrap();
    let app_git = root(&mut ids, app.id, "/src/shellio", RootKind::Git);
    let app_docs = root(&mut ids, app.id, "/src/docs", RootKind::Folder);
    let old = Project {
        list_position: None,
        removed_at: Some(UnixMillis(NOW.0 - 2)),
        last_opened_at: Some(UnixMillis(NOW.0 - 3)),
        ..project(&mut ids, work.id, "old").unwrap()
    };
    let old_root = root(&mut ids, old.id, "/src/old", RootKind::Folder);
    let main = workbench(&mut ids, app.id, "main", 0).unwrap();
    let review = workbench(&mut ids, app.id, "review", 1).unwrap();
    let navigator = panel(&mut ids, main.id, "navigator").unwrap();
    let terminal = panel(&mut ids, main.id, "terminal").unwrap();
    let notes = panel(&mut ids, main.id, "notes").unwrap();
    let default_layout = WorkbenchLayout::default_for(navigator.id, terminal.id);
    let mut with_notes = default_layout.clone();
    with_notes.zones.center.parts[0].tabs.push(notes.id);
    with_notes.hidden.navigator = true;
    // Раскладка второй рабочей области с переехавшей панелью и она же после её удаления.
    let mut review_layout = WorkbenchLayout::default_for(navigator.id, notes.id);
    review_layout.navigator = None;
    let mut review_empty = review_layout.clone();
    review_empty.zones.center.parts.clear();
    review_empty.focus.clear();
    with_notes.sizes.bottom = 300;

    let steps = vec![
        vec![
            Change::WorkspaceInserted {
                workspace: work.clone(),
            },
            Change::WindowSet {
                active: work.id,
                route: Route::WorkspaceHome,
            },
        ],
        vec![Change::ProjectInserted {
            project: app.clone(),
            roots: vec![app_git, app_docs],
        }],
        vec![
            Change::WorkbenchInserted {
                workbench: main.clone(),
            },
            Change::WorkbenchInserted {
                workbench: review.clone(),
            },
        ],
        vec![Change::WorkspaceRenamed {
            workspace: work.id,
            title: title("Работа и код").unwrap(),
        }],
        vec![Change::WorkbenchRenamed {
            workbench: review.id,
            title: title("Ревью").unwrap(),
        }],
        vec![
            Change::ProjectOpened {
                project: app.id,
                at: NOW,
            },
            Change::WorkspaceLastWorkbenchSet {
                workspace: work.id,
                workbench: Some(main.id),
            },
            Change::WindowSet {
                active: work.id,
                route: Route::Workbench(main.id),
            },
        ],
        vec![
            Change::WorkspaceInserted { workspace: home },
            Change::WorkspaceInserted { workspace: archive },
            Change::ProjectInserted {
                project: old,
                roots: vec![old_root],
            },
        ],
        vec![Change::WindowSet {
            active: work.id,
            route: Route::WorkspaceHome,
        }],
        vec![Change::WorkspaceLastWorkbenchSet {
            workspace: work.id,
            workbench: None,
        }],
        vec![
            Change::PanelAdd {
                panel: navigator.clone(),
            },
            Change::PanelAdd {
                panel: terminal.clone(),
            },
            Change::LayoutSet {
                workbench: main.id,
                layout: default_layout.clone(),
            },
        ],
        // Раскладка заменяется целиком вставкой с обновлением.
        vec![
            Change::PanelAdd {
                panel: notes.clone(),
            },
            Change::LayoutSet {
                workbench: main.id,
                layout: with_notes,
            },
        ],
        // Данные панели пишутся вставкой, вызывающая панель — столбцом панели.
        vec![
            Change::PanelData {
                panel: terminal.id,
                version: 1,
                data: "null".to_owned(),
            },
            Change::PanelData {
                panel: notes.id,
                version: 1,
                data: r#"{"count":1}"#.to_owned(),
            },
            Change::PanelOpenedFrom {
                panel: notes.id,
                opened_from: Some(terminal.id),
            },
        ],
        // Повторная запись заменяет строку данных, вызывающая панель сбрасывается в NULL.
        vec![
            Change::PanelData {
                panel: notes.id,
                version: 2,
                data: r#"{"count":2}"#.to_owned(),
            },
            Change::PanelOpenedFrom {
                panel: notes.id,
                opened_from: None,
            },
        ],
        // Переезд меняет владельца строки панели: перезагрузка видит её во второй рабочей области
        // с прежними данными.
        vec![
            Change::LayoutSet {
                workbench: main.id,
                layout: default_layout,
            },
            Change::PanelRehome {
                panel: notes.id,
                workbench: review.id,
            },
            Change::LayoutSet {
                workbench: review.id,
                layout: review_layout,
            },
        ],
        // Статус в базу не пишется; удаление панели уносит строку данных каскадом.
        vec![
            Change::PanelStatus {
                panel: notes.id,
                status: PanelStatus::StateNewer,
            },
            Change::LayoutSet {
                workbench: review.id,
                layout: review_empty,
            },
            Change::PanelRemove { panel: notes.id },
        ],
    ];
    let rehome_step = steps.len() - 2;

    let mut expected = State::default();
    for (step, changes) in steps.iter().enumerate() {
        let base = Revision(u64::try_from(step).unwrap());
        store.persist.commit(base, changes, None).unwrap();
        expected.apply_all(changes);
        let reloaded = store.reload().unwrap();
        assert_eq!(reloaded.revision, base.next(), "шаг {step}");
        assert_eq!(reloaded.state, expected, "шаг {step}");
        assert_eq!(
            store.revision_in_db().unwrap(),
            i64::try_from(base.next().0).unwrap()
        );
        if step == rehome_step {
            assert_eq!(reloaded.state.panels[&notes.id].workbench, review.id);
            assert!(reloaded.state.panel_data.contains_key(&notes.id));
        }
    }
    assert_eq!(expected.check_invariants(), Vec::new());
    assert_eq!(expected.roots.len(), 3);
    assert_eq!(expected.panels.len(), 2);
    assert_eq!(store.count("panels").unwrap(), 2);
    assert_eq!(store.count("workbench_layouts").unwrap(), 2);
    assert_eq!(
        expected.panel_data,
        BTreeMap::from([(
            terminal.id,
            PanelData::Text {
                version: 1,
                raw: "null".to_owned()
            }
        )])
    );
    assert_eq!(store.count("panel_states").unwrap(), 1);

    // Маршрут на главную и сброс последней области пишутся явным NULL.
    let raw = store.raw().unwrap();
    let route_is_null: bool = raw
        .query_row(
            "SELECT route_workbench_id IS NULL FROM window_state WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(route_is_null);
    let last_is_null: bool = raw
        .query_row(
            "SELECT last_workbench_id IS NULL FROM workspaces WHERE id = ?1",
            [work.id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(last_is_null);
}

#[test]
fn wrong_base_revision_is_conflict_and_changes_nothing() {
    let Seed {
        mut store, mut ids, ..
    } = seed().unwrap();
    let other = workspace(&mut ids, "Дом", 1).unwrap();
    let insert = [Change::WorkspaceInserted {
        workspace: other.clone(),
    }];

    for base in [Revision::ZERO, Revision(2), Revision(7)] {
        let error = store.assert_refused(base, &insert).unwrap();
        assert_conflict(&error);
        assert!(error.to_string().contains("ревизия state.db 1"), "{error}");
    }
    assert_eq!(store.revision_in_db().unwrap(), 1);

    store.persist.commit(Revision(1), &insert, None).unwrap();
    let reloaded = store.reload().unwrap();
    assert_eq!(reloaded.revision, Revision(2));
    assert_eq!(reloaded.state.workspaces[&other.id], other);
}

#[test]
fn root_key_taken_outside_core_is_conflict() {
    let Seed {
        mut store,
        mut ids,
        workspace,
        ..
    } = seed().unwrap();
    // Проект с корнем, вставленные в обход ядра: ядро о ключе не знает.
    let outsider = project(&mut ids, workspace.id, "outsider").unwrap();
    let outsider_root = root(&mut ids, outsider.id, "/src/site", RootKind::Git);
    let raw = store.raw().unwrap();
    raw.execute(
        "INSERT INTO projects (id, workspace_id, title, list_position) VALUES (?1, ?2, ?3, 1)",
        (
            outsider.id.as_bytes().as_slice(),
            workspace.id.as_bytes().as_slice(),
            outsider.title.as_str(),
        ),
    )
    .unwrap();
    raw.execute(
        "INSERT INTO project_roots (id, project_id, path, kind, key) VALUES (?1, ?2, ?3, 'git', ?4)",
        (
            outsider_root.id.as_bytes().as_slice(),
            outsider.id.as_bytes().as_slice(),
            "/src/site",
            outsider_root.key.as_str(),
        ),
    )
    .unwrap();
    drop(raw);

    let site = Project {
        list_position: Some(2),
        ..project(&mut ids, workspace.id, "site").unwrap()
    };
    let mut site_root = root(&mut ids, site.id, "/src/site-feature", RootKind::Git);
    site_root.key = outsider_root.key.clone();
    let error = store
        .assert_refused(
            Revision(1),
            &[Change::ProjectInserted {
                project: site.clone(),
                roots: vec![site_root],
            }],
        )
        .unwrap();
    assert_conflict(&error);
    assert!(error.to_string().contains("UNIQUE"), "{error}");

    // Вставка проекта до корня откатилась вместе с ним.
    let site_rows: i64 = store
        .raw()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM projects WHERE id = ?1",
            [site.id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(site_rows, 0);
    assert_eq!(store.count("project_roots").unwrap(), 2);
    assert_eq!(store.revision_in_db().unwrap(), 1);
}

#[test]
fn root_path_not_in_utf8_is_conflict_and_changes_nothing() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let Seed {
        mut store,
        mut ids,
        workspace,
        ..
    } = seed().unwrap();
    let site = Project {
        list_position: Some(1),
        ..project(&mut ids, workspace.id, "site").unwrap()
    };
    let mut site_root = root(&mut ids, site.id, "/src/site", RootKind::Folder);
    site_root.path = PathBuf::from(OsStr::from_bytes(b"/src/\xff"));
    let error = store
        .assert_refused(
            Revision(1),
            &[Change::ProjectInserted {
                project: site,
                roots: vec![site_root],
            }],
        )
        .unwrap();
    assert_conflict(&error);
    assert!(error.to_string().contains("UTF-8"), "{error}");
    assert_eq!(store.count("projects").unwrap(), 1);
    assert_eq!(store.count("project_roots").unwrap(), 1);
}

#[test]
fn primary_and_foreign_key_violations_are_conflicts() {
    let Seed {
        mut store,
        mut ids,
        workspace,
        project,
        workbench,
    } = seed().unwrap();
    let ghost_workspace = WorkspaceId::generate(&mut ids);
    let ghost_project = ProjectId::generate(&mut ids);

    let mut orphan_project = self::project(&mut ids, ghost_workspace, "orphan").unwrap();
    orphan_project.list_position = None;
    let orphan_root = root(&mut ids, ghost_project, "/src/orphan", RootKind::Folder);
    let moved = panel(&mut ids, workbench.id, "terminal").unwrap();
    let cases = [
        // Данные панели, которой нет, и переезд в рабочую область, которой нет.
        vec![Change::PanelData {
            panel: PanelId::generate(&mut ids),
            version: 1,
            data: "null".to_owned(),
        }],
        vec![
            Change::PanelAdd {
                panel: moved.clone(),
            },
            Change::PanelRehome {
                panel: moved.id,
                workbench: WorkbenchId::generate(&mut ids),
            },
        ],
        vec![Change::WorkspaceInserted {
            workspace: Workspace {
                title: title("Другой").unwrap(),
                position: 5,
                ..workspace.clone()
            },
        }],
        vec![Change::WorkbenchInserted {
            workbench: Workbench {
                title: title("Другая").unwrap(),
                ..workbench.clone()
            },
        }],
        vec![Change::ProjectInserted {
            project: orphan_project,
            roots: Vec::new(),
        }],
        vec![Change::ProjectInserted {
            project: self::project(&mut ids, workspace.id, "rootless").unwrap(),
            roots: vec![orphan_root],
        }],
        vec![Change::WorkbenchInserted {
            workbench: self::workbench(&mut ids, ghost_project, "orphan", 0).unwrap(),
        }],
        vec![Change::ProjectInserted {
            project: Project {
                title: title("Повтор").unwrap(),
                ..project.clone()
            },
            roots: Vec::new(),
        }],
    ];
    for changes in cases {
        let error = store.assert_refused(Revision(1), &changes).unwrap();
        assert_conflict(&error);
    }
    assert_eq!(store.revision_in_db().unwrap(), 1);
}

#[test]
fn change_of_record_missing_in_database_is_conflict() {
    let Seed {
        mut store,
        mut ids,
        workspace,
        workbench,
        ..
    } = seed().unwrap();
    let ghost_workspace = WorkspaceId::generate(&mut ids);
    let ghost_project = ProjectId::generate(&mut ids);
    let ghost_workbench = WorkbenchId::generate(&mut ids);
    let ghost_panel = PanelId::generate(&mut ids);
    let cases = [
        Change::PanelRemove { panel: ghost_panel },
        Change::PanelRehome {
            panel: ghost_panel,
            workbench: workbench.id,
        },
        Change::PanelOpenedFrom {
            panel: ghost_panel,
            opened_from: None,
        },
        Change::WorkspaceRenamed {
            workspace: ghost_workspace,
            title: title("Нет").unwrap(),
        },
        Change::WorkspaceLastWorkbenchSet {
            workspace: ghost_workspace,
            workbench: None,
        },
        Change::ProjectOpened {
            project: ghost_project,
            at: NOW,
        },
        Change::WorkbenchRenamed {
            workbench: ghost_workbench,
            title: title("Нет").unwrap(),
        },
    ];
    for change in cases {
        let error = store.assert_refused(Revision(1), &[change]).unwrap();
        assert_conflict(&error);
        assert!(error.to_string().contains("в обход ядра"), "{error}");
    }

    // Первая дельта записи прошла бы, вторая отказывает: откатывается вся запись.
    store
        .assert_refused(
            Revision(1),
            &[
                Change::WorkspaceRenamed {
                    workspace: workspace.id,
                    title: title("Новое имя").unwrap(),
                },
                Change::ProjectOpened {
                    project: ghost_project,
                    at: NOW,
                },
            ],
        )
        .unwrap();
    assert_eq!(
        store.reload().unwrap().state.workspaces[&workspace.id].title,
        workspace.title
    );
}

#[test]
fn window_set_restores_window_row_deleted_outside() {
    let Seed {
        mut store,
        workspace,
        workbench,
        ..
    } = seed().unwrap();
    store
        .raw()
        .unwrap()
        .execute("DELETE FROM window_state", [])
        .unwrap();

    let changes = [Change::WindowSet {
        active: workspace.id,
        route: Route::Workbench(workbench.id),
    }];
    store.persist.commit(Revision(1), &changes, None).unwrap();
    let reloaded = store.reload().unwrap();
    assert_eq!(reloaded.state.window.active_workspace, Some(workspace.id));
    assert_eq!(reloaded.state.window.route, Route::Workbench(workbench.id));
    assert_eq!(reloaded.state.diagnostics, Vec::new());
    assert_eq!(store.count("window_state").unwrap(), 1);
}

#[test]
fn layout_set_restores_layout_row_deleted_outside() {
    let Seed {
        mut store,
        mut ids,
        workbench,
        ..
    } = seed().unwrap();
    let navigator = panel(&mut ids, workbench.id, "navigator").unwrap();
    let terminal = panel(&mut ids, workbench.id, "terminal").unwrap();
    let layout = WorkbenchLayout::default_for(navigator.id, terminal.id);
    let mut changes = vec![
        Change::PanelAdd { panel: navigator },
        Change::PanelAdd { panel: terminal },
        Change::LayoutSet {
            workbench: workbench.id,
            layout: layout.clone(),
        },
    ];
    store.persist.commit(Revision(1), &changes, None).unwrap();
    store
        .raw()
        .unwrap()
        .execute("DELETE FROM workbench_layouts", [])
        .unwrap();
    let missing = store.reload().unwrap();
    assert_eq!(missing.state.layouts.get(&workbench.id), None);

    changes.drain(..2);
    store.persist.commit(Revision(2), &changes, None).unwrap();
    let reloaded = store.reload().unwrap();
    assert_eq!(reloaded.state.layouts[&workbench.id], layout);
    assert_eq!(reloaded.state.diagnostics, Vec::new());
    assert_eq!(store.count("workbench_layouts").unwrap(), 1);
}

#[test]
fn panel_of_missing_workbench_is_conflict() {
    let Seed {
        mut store, mut ids, ..
    } = seed().unwrap();
    let ghost = WorkbenchId::generate(&mut ids);
    let orphan = panel(&mut ids, ghost, "terminal").unwrap();
    for change in [
        Change::PanelAdd { panel: orphan },
        Change::LayoutSet {
            workbench: ghost,
            layout: WorkbenchLayout::default_for(
                PanelId::generate(&mut ids),
                PanelId::generate(&mut ids),
            ),
        },
    ] {
        let error = store.assert_refused(Revision(1), &[change]).unwrap();
        assert_conflict(&error);
    }
}
