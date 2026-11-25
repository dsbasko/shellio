//! `panel.open` (`open-or-focus`) и `panel.command` (ADR-0007 и ADR-0008, разделы «Уточнено в P1.5
//! (планирование)»).
//!
//! Поиск канонического экземпляра, `reopen`, размещение, фокус, переезд между раскладками, маршрут
//! и данные панели считаются над одним снимком и уходят одной записью. Новую панель создаёт та же
//! функция [`create`], что и `layout.split {open}`.

use serde_json::Value;
use shellio_layout::Zone;
use shellio_plugin_api::{Cardinality, Manifest, Plugin, Refusal, Registry, Step};

use crate::change::Change;
use crate::command::{
    CommandValue, OpenOutcome, open_workbench, panel_workbench, scoped_workbench,
};
use crate::error::{CoreError, Entity};
use crate::handler::{Ctx, Outcome, new_panel, workbench_open};
use crate::ids::{PanelId, WorkbenchId};
use crate::layout_cmd::{current, refusal, zone_name};
use crate::model::State;
use crate::panel::{Panel, PanelData, PanelStatus, PluginId, WorkbenchLayout, is_navigator};
use crate::plugin_host::instance;
use crate::tree::TreeScope;

/// Зарегистрированный плагин по строке поля `field`: неверная строка — `plugin_invalid`, верная,
/// но без регистрации — `plugin_unknown`.
pub(crate) fn registered<'a>(
    registry: &'a Registry,
    raw: &str,
    field: &str,
) -> Result<&'a Plugin, CoreError> {
    let id = PluginId::new(raw)
        .map_err(|error| CoreError::invalid_argument(field, "plugin_invalid", error.to_string()))?;
    registry
        .get(&id)
        .ok_or_else(|| CoreError::plugin_unknown(field, &id))
}

/// Плагин `PerApp` открывается только в области всех воркспейсов (ADR-0025).
pub(crate) fn require_all(
    manifest: &Manifest,
    scope: TreeScope,
    command: &str,
) -> Result<(), CoreError> {
    match (manifest.cardinality, scope) {
        (Cardinality::PerApp, TreeScope::Workspace(_)) => {
            Err(CoreError::scope_requires_all(command))
        }
        (Cardinality::PerApp, TreeScope::All)
        | (
            Cardinality::Multi
            | Cardinality::PerWorkbench
            | Cardinality::PerProject
            | Cardinality::PerWorkspace,
            TreeScope::All | TreeScope::Workspace(_),
        ) => Ok(()),
    }
}

/// Зона назначения входит в `placements` плагина, иначе `zone_not_allowed` поля `field`.
pub(crate) fn allowed_zone(manifest: &Manifest, zone: Zone, field: &str) -> Result<(), CoreError> {
    if manifest.placements.contains(zone) {
        return Ok(());
    }
    let allowed: Vec<&str> = manifest.placements.iter().map(zone_name).collect();
    Err(CoreError::zone_not_allowed(
        field,
        zone_name(zone),
        &allowed,
    ))
}

/// Зона назначения панели `panel` входит в `placements` её плагина. Панель неизвестного плагина
/// допускается во всех зонах.
pub(crate) fn allowed_for_panel(
    state: &State,
    registry: &Registry,
    panel: PanelId,
    zone: Zone,
    field: &str,
) -> Result<(), CoreError> {
    match state
        .panels
        .get(&panel)
        .and_then(|record| registry.get(&record.plugin))
    {
        Some(plugin) => allowed_zone(plugin.manifest(), zone, field),
        None => Ok(()),
    }
}

/// Отказ модели как ошибка ядра. `field` заменяет поле отказа у команды без `args`.
pub(crate) fn refused(refusal: &Refusal, field: Option<&str>) -> CoreError {
    CoreError::plugin_refusal(
        field,
        &refusal.reason,
        &refusal.message,
        &refusal.field,
        &refusal.details,
    )
}

/// Новая панель плагина с данными `init(args)` и записью о вызывающей панели `caller` у `PerApp`.
/// Отказ `init` — отказ команды без записи. Раскладку дописывает вызывающий.
pub(crate) fn create(
    plugin: &Plugin,
    workbench: WorkbenchId,
    args: Option<Value>,
    caller: Option<PanelId>,
    field: Option<&str>,
    ctx: &mut Ctx<'_>,
) -> Result<(PanelId, Vec<Change>), CoreError> {
    let manifest = plugin.manifest();
    let data = plugin
        .model()
        .init(args)
        .map_err(|refusal| refused(&refusal, field))?;
    let panel: Panel = new_panel(workbench, plugin.id().clone(), ctx)?;
    let id = panel.id;
    let mut changes = vec![
        Change::PanelAdd { panel },
        Change::PanelData {
            panel: id,
            version: manifest.state_version,
            data,
        },
    ];
    if let (Cardinality::PerApp, Some(caller)) = (manifest.cardinality, caller) {
        changes.push(Change::PanelOpenedFrom {
            panel: id,
            opened_from: Some(caller),
        });
    }
    Ok((id, changes))
}

/// Место новой или переехавшей панели: последняя активная часть зоны. Навигатор в левой зоне при
/// пустой колонке встаёт в колонку. Панель получает фокус.
fn place(
    layout: &WorkbenchLayout,
    panel: PanelId,
    zone: Zone,
    navigator: bool,
) -> Result<WorkbenchLayout, CoreError> {
    if navigator && zone == Zone::Left && layout.navigator.is_none() {
        let mut next = layout.clone();
        next.navigator = Some(panel);
        return next
            .focus_panel(panel)
            .map_err(|error| refusal(error, "plugin"));
    }
    layout
        .open(panel, Some(zone))
        .map_err(|error| refusal(error, "plugin"))
}

/// Данные готовой панели текущей версии плагина.
fn ready_data<'a>(
    state: &'a State,
    manifest: &Manifest,
    panel: PanelId,
    field: &str,
) -> Result<&'a str, CoreError> {
    match state.panel_data.get(&panel) {
        Some(PanelData::Text { version, raw }) if *version == manifest.state_version => Ok(raw),
        Some(PanelData::Text { .. } | PanelData::Unreadable {}) | None => Err(
            CoreError::panel_unavailable(field, panel, PanelStatus::StateCorrupt.as_str()),
        ),
    }
}

/// `panel.open`. Порядок проверок: plugin id, область, рабочая область, статус канонического
/// экземпляра, зона.
pub(crate) fn panel_open(
    state: &State,
    plugin: &str,
    workbench: Option<WorkbenchId>,
    zone: Option<Zone>,
    args: Option<Value>,
    ctx: &mut Ctx<'_>,
) -> Result<Outcome, CoreError> {
    let plugin = registered(ctx.registry, plugin, "plugin")?;
    let manifest = plugin.manifest();
    require_all(manifest, ctx.scope, "panel.open")?;
    let target = scoped_workbench(state, ctx.scope, workbench)?;
    let layout = current(state, target)?;
    let found = instance(state, plugin, target, None);
    if let Some(panel) = found {
        let status = state.status_of(panel);
        if status != PanelStatus::Ready {
            return Err(CoreError::panel_unavailable(
                "plugin",
                panel,
                status.as_str(),
            ));
        }
    }
    if let Some(zone) = zone {
        allowed_zone(manifest, zone, "zone")?;
    }
    let zone = zone.unwrap_or(manifest.default_zone);
    let navigator = is_navigator(plugin.id());
    let opened = |panel, workbench, outcome| CommandValue::Opened {
        panel,
        workbench,
        outcome,
    };

    let Some(panel) = found else {
        let (panel, mut changes) = create(plugin, target, args, layout.focused(), None, ctx)?;
        changes.push(Change::LayoutSet {
            workbench: target,
            layout: place(layout, panel, zone, navigator)?,
        });
        return Ok(Outcome::new(
            opened(panel, target, OpenOutcome::Created),
            changes,
        ));
    };

    let owner = state
        .panels
        .get(&panel)
        .map(|record| record.workbench)
        .ok_or_else(|| CoreError::not_found(Entity::Panel, panel))?;
    let step = plugin
        .model()
        .reopen(ready_data(state, manifest, panel, "plugin")?, args)
        .map_err(|refusal| refused(&refusal, None))?;
    let per_app = manifest.cardinality == Cardinality::PerApp;
    let recorded = state.opened_from.get(&panel).copied();

    let mut changes = Vec::new();
    let mut caller = recorded;
    let mut route = Vec::new();
    let (home, outcome) = if owner == target {
        let next = layout
            .focus_panel(panel)
            .map_err(|error| refusal(error, "plugin"))?;
        if per_app && let Some(focused) = layout.focused().filter(|&focused| focused != panel) {
            caller = Some(focused);
        }
        if next != *layout {
            changes.push(Change::LayoutSet {
                workbench: target,
                layout: next,
            });
        }
        (target, OpenOutcome::Focused)
    } else {
        let source = current(state, owner)?;
        match manifest.cardinality {
            Cardinality::PerWorkspace | Cardinality::PerApp => {
                let left = source
                    .close(panel)
                    .map_err(|error| refusal(error, "plugin"))?;
                changes.push(Change::PanelRehome {
                    panel,
                    workbench: target,
                });
                changes.push(Change::LayoutSet {
                    workbench: owner,
                    layout: left,
                });
                changes.push(Change::LayoutSet {
                    workbench: target,
                    layout: place(layout, panel, zone, navigator)?,
                });
                if per_app {
                    caller = layout.focused();
                }
                (target, OpenOutcome::Moved)
            }
            Cardinality::PerProject => {
                let next = source
                    .focus_panel(panel)
                    .map_err(|error| refusal(error, "plugin"))?;
                if next != *source {
                    changes.push(Change::LayoutSet {
                        workbench: owner,
                        layout: next,
                    });
                }
                if open_workbench(state) == Some(target) {
                    route = workbench_open(state, owner, ctx)?.changes;
                    (owner, OpenOutcome::Routed)
                } else {
                    (owner, OpenOutcome::Focused)
                }
            }
            Cardinality::Multi | Cardinality::PerWorkbench => {
                return Err(CoreError::storage_conflict(format!(
                    "экземпляр {panel} плагина «{}» найден вне своей рабочей области",
                    manifest.id
                )));
            }
        }
    };
    if let Step::Changed(data) = step {
        changes.push(Change::PanelData {
            panel,
            version: manifest.state_version,
            data,
        });
    }
    if caller != recorded {
        changes.push(Change::PanelOpenedFrom {
            panel,
            opened_from: caller,
        });
    }
    changes.extend(route);
    Ok(Outcome::new(opened(panel, home, outcome), changes))
}

/// `panel.command`: действие плагина над данными готовой панели. Прежние данные — ответ без
/// записи.
pub(crate) fn panel_command(
    state: &State,
    panel: PanelId,
    name: &str,
    args: Option<Value>,
    ctx: &mut Ctx<'_>,
) -> Result<Outcome, CoreError> {
    panel_workbench(state, panel)?;
    let record = state
        .panels
        .get(&panel)
        .ok_or_else(|| CoreError::not_found(Entity::Panel, panel))?;
    let status = state.status_of(panel);
    let plugin = match (ctx.registry.get(&record.plugin), status) {
        (Some(plugin), PanelStatus::Ready) => plugin,
        (None, PanelStatus::Ready) => {
            return Err(CoreError::panel_unavailable(
                "panel",
                panel,
                PanelStatus::PluginMissing.as_str(),
            ));
        }
        (
            Some(_) | None,
            PanelStatus::PluginMissing
            | PanelStatus::Duplicate
            | PanelStatus::StateCorrupt
            | PanelStatus::InitFailed
            | PanelStatus::MigrateFailed
            | PanelStatus::StateNewer,
        ) => {
            return Err(CoreError::panel_unavailable(
                "panel",
                panel,
                status.as_str(),
            ));
        }
    };
    let manifest = plugin.manifest();
    let step = plugin
        .model()
        .command(ready_data(state, manifest, panel, "panel")?, name, args)
        .map_err(|refusal| refused(&refusal, None))?;
    let changes = match step {
        Step::Unchanged => Vec::new(),
        Step::Changed(data) => vec![Change::PanelData {
            panel,
            version: manifest.state_version,
            data,
        }],
    };
    Ok(Outcome::new(CommandValue::Panel { panel }, changes))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;
    use shellio_layout::{Edge, Hideable, Location, Viewport};
    use shellio_plugin_api::{STATE_LIMIT, ZoneSet};

    use super::*;
    use crate::clock::UnixMillis;
    use crate::command::Command;
    use crate::error::ErrorCode;
    use crate::handler::{handle, startup};
    use crate::ids::{IdGen, WorkspaceId};
    use crate::model::{Revision, Route};
    use crate::plugin_host::load_panels;
    use crate::query::{PanelView, Query, StorageInfo, read};
    use crate::test_support::{
        FakeRootResolver, ManualClock, SequentialIds, TestInit, TestPlugin, registry_of,
    };
    use crate::title::Title;
    use crate::wire::ReplyValue;

    const VIEWPORT: Viewport = Viewport {
        width: 1180,
        height: 720,
    };

    fn list() -> TestPlugin {
        TestPlugin::new("rows")
            .title("Список")
            .cardinality(Cardinality::PerWorkbench)
            .zones(ZoneSet::ALL, Zone::Right)
            .actions()
    }

    fn field() -> TestPlugin {
        TestPlugin::new("field")
            .title("Поле")
            .cardinality(Cardinality::PerApp)
            .actions()
    }

    /// Два воркспейса: в первом проект с рабочими областями `a` и `a2` и проект с `b`, во втором —
    /// проект с `c`. В окне открыта `a`.
    struct W {
        state: State,
        ids: SequentialIds,
        clock: ManualClock,
        resolver: FakeRootResolver,
        registry: Arc<Registry>,
        scope: TreeScope,
        w1: WorkspaceId,
        w2: WorkspaceId,
        a: WorkbenchId,
        a2: WorkbenchId,
        b: WorkbenchId,
        c: WorkbenchId,
    }

    impl W {
        fn new(plugins: Vec<TestPlugin>) -> Self {
            let zero = WorkbenchId::from_bytes([0; 16]);
            let mut w = Self {
                state: State::default(),
                ids: SequentialIds::new(),
                clock: ManualClock::new(UnixMillis(1_000)),
                resolver: FakeRootResolver::new()
                    .with_git("/src/app", "/src/app/.git")
                    .with_git("/src/docs", "/src/docs/.git")
                    .with_git("/src/home", "/src/home/.git"),
                registry: registry_of(plugins),
                scope: TreeScope::All,
                w1: WorkspaceId::from_bytes([0; 16]),
                w2: WorkspaceId::from_bytes([0; 16]),
                a: zero,
                a2: zero,
                b: zero,
                c: zero,
            };
            let mut ctx = Ctx::new(&w.clock, &mut w.ids, &w.registry);
            let outcome = startup(&w.state, &mut ctx).unwrap().unwrap();
            w.state.apply_all(&outcome.changes);
            w.w1 = w.state.workspaces_in_order()[0].id;
            let CommandValue::Workspace { workspace } = w
                .ok(Command::WorkspaceCreate {
                    title: "Дом".into(),
                })
                .value
            else {
                panic!("ожидался воркспейс");
            };
            w.w2 = workspace;
            let first = w.project(w.w1, "/src/app");
            let second = w.project(w.w1, "/src/docs");
            let third = w.project(w.w2, "/src/home");
            w.a = w.workbench(first, "a");
            w.a2 = w.workbench(first, "a2");
            w.b = w.workbench(second, "b");
            w.c = w.workbench(third, "c");
            w.ok(Command::WorkbenchOpen { workbench: w.a });
            w
        }

        fn project(&mut self, workspace: WorkspaceId, path: &str) -> crate::ids::ProjectId {
            match self
                .ok(Command::ProjectAdd {
                    workspace,
                    path: path.into(),
                    title: None,
                })
                .value
            {
                CommandValue::Project { project, .. } => project,
                other => panic!("ожидался проект, получено {other:?}"),
            }
        }

        fn workbench(&mut self, project: crate::ids::ProjectId, title: &str) -> WorkbenchId {
            match self
                .ok(Command::WorkbenchCreate {
                    project,
                    title: title.into(),
                })
                .value
            {
                CommandValue::Workbench { workbench } => workbench,
                other => panic!("ожидалась рабочая область, получено {other:?}"),
            }
        }

        /// Команда над состоянием. После неё инварианты целы, а статусы совпадают с теми, что
        /// посчитал бы холодный запуск, и запуску нечего дописывать.
        fn run(&mut self, command: Command) -> Result<Outcome, CoreError> {
            let prepared = command.prepare(&self.resolver)?;
            let mut ctx = Ctx::new(&self.clock, &mut self.ids, &self.registry)
                .with_viewport(Some(VIEWPORT))
                .with_scope(self.scope);
            let outcome = handle(&self.state, prepared, &mut ctx)?;
            self.state.apply_all(&outcome.changes);
            assert_eq!(self.state.check_invariants(), Vec::new());
            let cold = load_panels(&self.state, &self.registry);
            assert_eq!(cold.statuses, self.state.panel_status);
            assert_eq!(cold.changes, Vec::new());
            Ok(outcome)
        }

        fn ok(&mut self, command: Command) -> Outcome {
            self.run(command).unwrap()
        }

        /// Отказ без изменения состояния.
        fn err(&mut self, command: Command) -> CoreError {
            let before = self.state.clone();
            let error = self.run(command).unwrap_err();
            assert_eq!(self.state, before, "{error}");
            error
        }

        fn open_command(
            plugin: &str,
            workbench: Option<WorkbenchId>,
            zone: Option<Zone>,
            args: Option<Value>,
        ) -> Command {
            Command::PanelOpen {
                plugin: plugin.into(),
                workbench,
                zone,
                args,
            }
        }

        /// `panel.open` с исходом и дельтами.
        fn open_with(
            &mut self,
            plugin: &str,
            workbench: Option<WorkbenchId>,
            zone: Option<Zone>,
            args: Option<Value>,
        ) -> (PanelId, WorkbenchId, OpenOutcome, Vec<Change>) {
            let outcome = self.ok(Self::open_command(plugin, workbench, zone, args));
            match outcome.value {
                CommandValue::Opened {
                    panel,
                    workbench,
                    outcome: kind,
                } => (panel, workbench, kind, outcome.changes),
                other => panic!("ожидался исход panel.open, получено {other:?}"),
            }
        }

        fn open(&mut self, plugin: &str, workbench: WorkbenchId) -> (PanelId, OpenOutcome) {
            let (panel, _, outcome, _) = self.open_with(plugin, Some(workbench), None, None);
            (panel, outcome)
        }

        fn layout(&self, workbench: WorkbenchId) -> &WorkbenchLayout {
            &self.state.layouts[&workbench]
        }

        fn focused(&self, workbench: WorkbenchId) -> PanelId {
            self.layout(workbench).focused().unwrap()
        }

        fn navigator(&self, workbench: WorkbenchId) -> PanelId {
            self.layout(workbench).navigator.unwrap()
        }

        fn zone_of(&self, panel: PanelId) -> Zone {
            let workbench = self.state.panels[&panel].workbench;
            self.layout(workbench).locate(panel).unwrap().zone()
        }

        fn focus(&mut self, panel: PanelId) {
            self.ok(Command::PanelFocus {
                panel: Some(panel),
                workbench: None,
                direction: None,
            });
        }

        fn close(&mut self, panel: PanelId) -> Outcome {
            self.ok(Command::PanelClose { panel })
        }

        fn count(&self, panel: PanelId) -> Value {
            match &self.state.panel_data[&panel] {
                PanelData::Text { raw, .. } => {
                    serde_json::from_str::<Value>(raw).unwrap()["count"].clone()
                }
                PanelData::Unreadable {} => panic!("данные панели нечитаемы"),
            }
        }

        fn view(&self, scope: TreeScope, panel: PanelId) -> Result<PanelView, CoreError> {
            let storage = StorageInfo {
                schema_version: 3,
                state_db: "/state/state.db".into(),
            };
            let query = Query::Panel { scope, panel };
            match read(
                &self.state,
                &self.registry,
                Revision(1),
                &storage,
                None,
                &query,
            )?
            .value
            {
                ReplyValue::Panel(view) => Ok(*view),
                other => panic!("ожидалась панель, получено {other:?}"),
            }
        }

        /// Панель в обход команд, как строка базы прежней сборки: без данных, в центре.
        fn legacy(&mut self, workbench: WorkbenchId, plugin: &str, created_at: i64) -> PanelId {
            let ids: &mut dyn IdGen = &mut self.ids;
            let id = PanelId::generate(ids);
            let layout = self.layout(workbench).open(id, None).unwrap();
            self.state.apply_all(&[
                Change::PanelAdd {
                    panel: Panel {
                        id,
                        workbench,
                        plugin: PluginId::new(plugin).unwrap(),
                        title: Title::new(plugin).unwrap(),
                        created_at: UnixMillis(created_at),
                    },
                },
                Change::LayoutSet { workbench, layout },
            ]);
            id
        }

        /// Статусы и данные, как после запуска ядра над текущими строками.
        fn restart(&mut self) {
            let loaded = load_panels(&self.state, &self.registry);
            self.state.panel_status = loaded.statuses;
            self.state.apply_all(&loaded.changes);
        }
    }

    fn assert_refused(error: &CoreError, details: &[(&str, &str)]) {
        assert_eq!(error.code(), ErrorCode::InvalidArgument, "{error}");
        for (key, value) in details {
            assert_eq!(error.detail_str(key), Some(*value), "{key} в {error}");
        }
    }

    #[test]
    fn open_creates_panel_in_default_zone_with_focus_and_init_data() {
        let mut w = W::new(vec![list()]);
        let terminal = w.focused(w.a);
        let (panel, workbench, outcome, changes) = w.open_with("rows", None, None, None);
        assert_eq!((workbench, outcome), (w.a, OpenOutcome::Created));
        let [
            Change::PanelAdd { panel: added },
            Change::PanelData {
                panel: with_data,
                version: 1,
                data,
            },
            Change::LayoutSet { .. },
        ] = changes.as_slice()
        else {
            panic!("неожиданные дельты {changes:?}");
        };
        assert_eq!(
            (added.id, *with_data, data.as_str()),
            (panel, panel, r#"{"count":0}"#)
        );
        assert_eq!(added.title.as_str(), "Список");
        assert_eq!(w.zone_of(panel), Zone::Right);
        assert_eq!(w.focused(w.a), panel);
        assert_ne!(panel, terminal);
        // Singleton не `PerApp` вызывающую панель не запоминает.
        assert_eq!(w.state.opened_from.get(&panel), None);

        // Контекст создания доходит до `init`, явная зона — подсказка места.
        let (other, workbench, outcome, _) = w.open_with(
            "rows",
            Some(w.b),
            Some(Zone::Bottom),
            Some(json!({"count": 3})),
        );
        assert_eq!((workbench, outcome), (w.b, OpenOutcome::Created));
        assert_eq!(w.count(other), json!(3));
        assert_eq!(w.zone_of(other), Zone::Bottom);
    }

    #[test]
    fn repeat_focuses_the_same_panel_and_reopens_with_args() {
        let mut w = W::new(vec![list()]);
        let (panel, _) = w.open("rows", w.a);
        // Полный повтор: тот же `PanelId`, исход `focused`, дельт нет.
        let (again, workbench, outcome, changes) = w.open_with("rows", None, None, None);
        assert_eq!(
            (again, workbench, outcome),
            (panel, w.a, OpenOutcome::Focused)
        );
        assert_eq!(changes, Vec::new());
        // Новый контекст — только данные.
        let (again, _, outcome, changes) =
            w.open_with("rows", None, None, Some(json!({"count": 5})));
        assert_eq!((again, outcome), (panel, OpenOutcome::Focused));
        assert!(
            matches!(changes.as_slice(), [Change::PanelData { panel: p, .. }] if *p == panel),
            "{changes:?}"
        );
        assert_eq!(w.count(panel), json!(5));
        let (_, _, _, changes) = w.open_with("rows", None, None, Some(json!({"count": 5})));
        assert_eq!(changes, Vec::new());
        // Явная другая допустимая зона у существующего singleton позицию не меняет.
        let before = w.layout(w.a).clone();
        let (again, _, outcome, changes) = w.open_with("rows", None, Some(Zone::Bottom), None);
        assert_eq!((again, outcome), (panel, OpenOutcome::Focused));
        assert_eq!(changes, Vec::new());
        assert_eq!(*w.layout(w.a), before);
        assert_eq!(w.zone_of(panel), Zone::Right);
    }

    #[test]
    fn repeat_shows_panel_behind_hidden_zone_foreign_zoom_and_only() {
        let mut w = W::new(vec![list()]);
        let terminal = w.focused(w.a);
        let (panel, _) = w.open("rows", w.a);
        let visible = |w: &W| w.layout(w.a).is_visible(panel) && w.focused(w.a) == panel;

        w.focus(terminal);
        w.ok(Command::LayoutVisibility {
            workbench: None,
            target: Hideable::Right,
            visible: false,
        });
        assert!(!w.layout(w.a).is_visible(panel));
        assert_eq!(w.open("rows", w.a), (panel, OpenOutcome::Focused));
        assert!(visible(&w));

        w.ok(Command::LayoutZoom {
            panel: Some(terminal),
            workbench: None,
            off: None,
        });
        assert!(!w.layout(w.a).is_visible(panel));
        assert_eq!(w.open("rows", w.a), (panel, OpenOutcome::Focused));
        assert!(visible(&w));
        assert_eq!(w.layout(w.a).zoomed_panel(), None);

        w.focus(terminal);
        w.ok(Command::LayoutOnly { workbench: None });
        assert!(!w.layout(w.a).is_visible(panel));
        assert_eq!(w.open("rows", w.a), (panel, OpenOutcome::Focused));
        assert!(visible(&w));
    }

    #[test]
    fn open_works_in_emptied_layout_and_navigator_returns_to_its_column() {
        let mut w = W::new(vec![list()]);
        let (terminal, navigator) = (w.focused(w.a), w.navigator(w.a));
        w.close(terminal);
        w.close(navigator);
        assert_eq!(w.layout(w.a).panels().count(), 0);

        let (panel, outcome) = w.open("terminal", w.a);
        assert_eq!(outcome, OpenOutcome::Created);
        assert_eq!(w.zone_of(panel), Zone::Center);
        assert_eq!(w.focused(w.a), panel);

        // Навигатор по умолчанию открывается в левую зону: при пустой колонке — в колонку.
        let (column, outcome) = w.open("navigator", w.a);
        assert_eq!(outcome, OpenOutcome::Created);
        assert_eq!(w.layout(w.a).locate(column), Some(Location::Navigator));
        assert_eq!(w.focused(w.a), column);
        // Он `PerWorkbench`: повтор его находит.
        w.focus(panel);
        assert_eq!(w.open("navigator", w.a), (column, OpenOutcome::Focused));
        assert_eq!(w.focused(w.a), column);
    }

    #[test]
    fn new_panel_goes_to_the_last_active_part_of_its_zone() {
        let mut w = W::new(Vec::new());
        let first = w.focused(w.a);
        let CommandValue::Panel { panel: second } = w
            .ok(Command::LayoutSplit {
                panel: first,
                open: Some("terminal".into()),
                axis: None,
            })
            .value
        else {
            panic!("ожидалась панель");
        };
        let part_of = |w: &W, panel| match w.layout(w.a).locate(panel) {
            Some(Location::Part { part, .. }) => part,
            other => panic!("ожидалась часть, получено {other:?}"),
        };
        w.focus(first);
        // `Multi`: каждое открытие — новая панель.
        let (third, outcome) = w.open("terminal", w.a);
        assert_eq!(outcome, OpenOutcome::Created);
        assert_eq!(part_of(&w, third), part_of(&w, first));
        w.focus(second);
        let (fourth, outcome) = w.open("terminal", w.a);
        assert_eq!(outcome, OpenOutcome::Created);
        assert_eq!(part_of(&w, fourth), part_of(&w, second));
        assert_ne!(third, fourth);
    }

    #[test]
    fn zone_outside_placements_is_refused_for_open_move_and_split() {
        let narrow = TestPlugin::new("narrow")
            .zones(ZoneSet::of(&[Zone::Center, Zone::Bottom]), Zone::Center);
        let mut w = W::new(vec![narrow]);
        let terminal = w.focused(w.a);
        let allowed = |error: &CoreError| {
            let mut zones: Vec<String> = error
                .detail("allowed")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|zone| zone.as_str().unwrap().to_owned())
                .collect();
            zones.sort();
            zones
        };

        let error = w.err(W::open_command("narrow", None, Some(Zone::Left), None));
        assert_refused(&error, &[("reason", "zone_not_allowed"), ("field", "zone")]);
        assert_eq!(allowed(&error), ["bottom", "center"]);

        let (panel, _) = w.open("narrow", w.a);
        let error = w.err(Command::PanelMove {
            panel,
            zone: Zone::Right,
        });
        assert_refused(&error, &[("reason", "zone_not_allowed"), ("field", "zone")]);
        assert_eq!(allowed(&error), ["bottom", "center"]);
        w.ok(Command::PanelMove {
            panel,
            zone: Zone::Bottom,
        });
        assert_eq!(w.zone_of(panel), Zone::Bottom);

        // Якорь в правой зоне: перенос к нему с краем и без края.
        w.ok(Command::PanelMove {
            panel: terminal,
            zone: Zone::Right,
        });
        for edge in [None, Some(Edge::After)] {
            let error = w.err(Command::PanelMovePart {
                panel,
                anchor: terminal,
                edge,
            });
            assert_refused(
                &error,
                &[("reason", "zone_not_allowed"), ("field", "anchor")],
            );
            assert_eq!(allowed(&error), ["bottom", "center"]);
        }
        let error = w.err(Command::LayoutSplit {
            panel: terminal,
            open: Some("narrow".into()),
            axis: None,
        });
        assert_refused(&error, &[("reason", "zone_not_allowed"), ("field", "open")]);

        // Панель незарегистрированного плагина допускается во всех зонах.
        let gone = w.legacy(w.a, "gone", 1);
        w.state
            .panel_status
            .insert(gone, PanelStatus::PluginMissing);
        w.ok(Command::PanelMove {
            panel: gone,
            zone: Zone::Left,
        });
        assert_eq!(w.zone_of(gone), Zone::Left);
    }

    #[test]
    fn checks_go_in_order_plugin_scope_workbench_status_zone() {
        let narrow = TestPlugin::new("narrow")
            .cardinality(Cardinality::PerWorkbench)
            .zones(ZoneSet::of(&[Zone::Center]), Zone::Center);
        let mut w = W::new(vec![field(), narrow, list()]);
        let missing = WorkbenchId::from_bytes([9; 16]);

        // Аргументы и plugin id — раньше области и рабочей области.
        w.scope = TreeScope::Workspace(w.w2);
        let error = w.err(W::open_command("rows", Some(missing), None, Some(json!(5))));
        assert_refused(&error, &[("reason", "args_invalid"), ("field", "args")]);
        let error = w.err(W::open_command("Bad Id", Some(missing), None, None));
        assert_refused(&error, &[("reason", "plugin_invalid"), ("field", "plugin")]);
        let error = w.err(W::open_command("gone", Some(missing), None, None));
        assert_refused(&error, &[("reason", "plugin_unknown"), ("field", "plugin")]);
        // Область — раньше рабочей области.
        let error = w.err(W::open_command("field", Some(missing), None, None));
        assert_refused(
            &error,
            &[("reason", "scope_requires_all"), ("field", "scope")],
        );
        // Рабочая область вне области — как несуществующая.
        for workbench in [missing, w.a] {
            let error = w.err(W::open_command("rows", Some(workbench), None, None));
            assert_eq!(error.code(), ErrorCode::NotFound, "{error}");
            assert_eq!(error.detail_str("entity"), Some("workbench"));
        }
        // Открытая в окне рабочая область чужого воркспейса не раскрывается.
        let error = w.err(W::open_command("rows", None, None, None));
        assert_eq!(error.code(), ErrorCode::NotFound, "{error}");
        assert_eq!(error.detail("id"), Some(&Value::Null));
        w.scope = TreeScope::All;

        // Статус канонического — раньше зоны.
        let stale = w.legacy(w.a, "narrow", 1);
        w.state.panel_data.insert(
            stale,
            PanelData::Text {
                version: 2,
                raw: "{}".into(),
            },
        );
        w.restart();
        assert_eq!(w.state.status_of(stale), PanelStatus::StateNewer);
        let error = w.err(W::open_command("narrow", None, Some(Zone::Left), None));
        assert_refused(
            &error,
            &[
                ("reason", "panel_unavailable"),
                ("field", "plugin"),
                ("status", "state_newer"),
            ],
        );
        assert_eq!(error.detail_str("panel"), Some(stale.to_string().as_str()));
        let error = w.err(W::open_command("narrow", Some(w.b), Some(Zone::Left), None));
        assert_refused(&error, &[("reason", "zone_not_allowed"), ("field", "zone")]);

        // Без явной и открытой рабочей области.
        w.ok(Command::WindowFocusWorkspace { workspace: w.w2 });
        assert_eq!(w.state.window.route, Route::WorkspaceHome);
        let error = w.err(W::open_command("rows", None, None, None));
        assert_refused(&error, &[("reason", "no_open_workbench")]);
    }

    #[test]
    fn model_refusals_keep_reason_field_and_details() {
        let mut w = W::new(vec![
            list(),
            TestPlugin::new("stuck").init(TestInit::Refuse),
            TestPlugin::new("huge").init(TestInit::Oversize),
        ]);
        let error = w.err(W::open_command("rows", None, None, Some(json!({"x": 1}))));
        assert_refused(&error, &[("reason", "args_invalid"), ("field", "args")]);
        let error = w.err(W::open_command(
            "rows",
            None,
            None,
            Some(json!({"count": 99})),
        ));
        assert_refused(
            &error,
            &[("reason", "count_out_of_range"), ("field", "count")],
        );
        let error = w.err(W::open_command("stuck", None, None, None));
        assert_refused(&error, &[("reason", "init_refused"), ("field", "args")]);
        let error = w.err(W::open_command("huge", None, None, None));
        assert_refused(
            &error,
            &[("reason", "panel_state_too_large"), ("field", "args")],
        );
        assert_eq!(error.detail("limit"), Some(&json!(STATE_LIMIT)));
        assert!(error.detail("size").unwrap().as_u64().unwrap() > STATE_LIMIT as u64);

        // У `layout.split` поля `args` нет: отказ `init` называет `open`.
        let terminal = w.focused(w.a);
        let error = w.err(Command::LayoutSplit {
            panel: terminal,
            open: Some("huge".into()),
            axis: None,
        });
        assert_refused(
            &error,
            &[("reason", "panel_state_too_large"), ("field", "open")],
        );
        assert_eq!(error.detail("limit"), Some(&json!(STATE_LIMIT)));

        // Отказ `reopen` существующего экземпляра ничего не меняет.
        w.open("rows", w.a);
        let error = w.err(W::open_command(
            "rows",
            None,
            None,
            Some(json!({"count": 99})),
        ));
        assert_refused(
            &error,
            &[("reason", "count_out_of_range"), ("field", "count")],
        );
    }

    #[test]
    fn per_project_focuses_at_owner_and_routes_the_shown_workbench() {
        let project = TestPlugin::new("proj")
            .cardinality(Cardinality::PerProject)
            .actions();
        let mut w = W::new(vec![project]);
        let (panel, outcome) = w.open("proj", w.a);
        assert_eq!(outcome, OpenOutcome::Created);
        let terminal = w
            .layout(w.a)
            .panels()
            .find(|&p| p != panel && p != w.navigator(w.a));
        w.focus(terminal.unwrap());

        // Фоновая целевая рабочая область: фокус у владельца, маршрут прежний.
        let window = w.state.window.clone();
        let (again, home, outcome, changes) =
            w.open_with("proj", Some(w.a2), None, Some(json!({"count": 2})));
        assert_eq!((again, home, outcome), (panel, w.a, OpenOutcome::Focused));
        assert_eq!(w.state.window, window);
        assert_eq!(w.focused(w.a), panel);
        assert_eq!(w.count(panel), json!(2));
        assert!(
            matches!(
                changes.as_slice(),
                [Change::LayoutSet { workbench, .. }, Change::PanelData { .. }] if *workbench == w.a
            ),
            "{changes:?}"
        );

        // Показанная целевая рабочая область: окно переходит к владельцу набором `workbench.open`.
        w.ok(Command::WorkbenchOpen { workbench: w.a2 });
        w.clock.advance(50);
        let expected = {
            let mut probe = w.state.clone();
            let mut ids = SequentialIds::new();
            let mut ctx = Ctx::new(&w.clock, &mut ids, &w.registry);
            let outcome = workbench_open(&probe, w.a, &mut ctx).unwrap();
            probe.apply_all(&outcome.changes);
            assert_eq!(outcome.changes.len(), 3, "{:?}", outcome.changes);
            outcome.changes
        };
        let (again, home, outcome, changes) = w.open_with("proj", None, None, None);
        assert_eq!((again, home, outcome), (panel, w.a, OpenOutcome::Routed));
        assert_eq!(changes, expected);
        assert_eq!(w.state.window.route, Route::Workbench(w.a));
        // Владелец показан: повтор — `focused` без дельт.
        let (_, _, outcome, changes) = w.open_with("proj", None, None, None);
        assert_eq!(outcome, OpenOutcome::Focused);
        assert_eq!(changes, Vec::new());

        // Другой проект — другой экземпляр.
        let (other, outcome) = w.open("proj", w.b);
        assert_eq!(outcome, OpenOutcome::Created);
        assert_ne!(other, panel);
        assert_eq!(w.state.panels[&other].workbench, w.b);
        assert_eq!(w.state.window.route, Route::Workbench(w.a));
    }

    #[test]
    fn per_workspace_moves_inside_workspace_and_creates_in_another() {
        let plugin = TestPlugin::new("ws")
            .cardinality(Cardinality::PerWorkspace)
            .zones(ZoneSet::ALL, Zone::Bottom)
            .actions();
        let mut w = W::new(vec![plugin]);
        let (panel, _) = w.open("ws", w.a);
        let window = w.state.window.clone();

        let (again, home, outcome, changes) =
            w.open_with("ws", Some(w.b), None, Some(json!({"count": 4})));
        assert_eq!((again, home, outcome), (panel, w.b, OpenOutcome::Moved));
        let [
            Change::PanelRehome {
                panel: moved,
                workbench,
            },
            Change::LayoutSet {
                workbench: from, ..
            },
            Change::LayoutSet { workbench: to, .. },
            Change::PanelData { .. },
        ] = changes.as_slice()
        else {
            panic!("неожиданные дельты {changes:?}");
        };
        assert_eq!((*moved, *workbench, *from, *to), (panel, w.b, w.a, w.b));
        assert_eq!(w.state.panels[&panel].workbench, w.b);
        assert_eq!(w.layout(w.a).locate(panel), None);
        assert_eq!(w.zone_of(panel), Zone::Bottom);
        assert_eq!(w.focused(w.b), panel);
        assert_eq!(w.count(panel), json!(4));
        assert_eq!(w.state.window, window);
        // Singleton не `PerApp` вызывающую не запоминает и при переезде.
        assert_eq!(w.state.opened_from.get(&panel), None);

        assert_eq!(w.open("ws", w.b), (panel, OpenOutcome::Focused));
        // Другой воркспейс — другой экземпляр, прежний на месте.
        let (other, outcome) = w.open("ws", w.c);
        assert_eq!(outcome, OpenOutcome::Created);
        assert_ne!(other, panel);
        assert_eq!(w.state.panels[&panel].workbench, w.b);
    }

    #[test]
    fn per_app_moves_between_workspaces_and_remembers_the_caller() {
        let mut w = W::new(vec![field()]);
        let terminal = w.focused(w.a);
        let (panel, _, outcome, changes) = w.open_with("field", None, None, None);
        assert_eq!(outcome, OpenOutcome::Created);
        assert!(changes.contains(&Change::PanelOpenedFrom {
            panel,
            opened_from: Some(terminal),
        }));
        assert_eq!(w.state.opened_from.get(&panel), Some(&terminal));

        // Самоповтор при фокусе на панели запись не меняет.
        let (_, _, outcome, changes) = w.open_with("field", None, None, None);
        assert_eq!(outcome, OpenOutcome::Focused);
        assert_eq!(changes, Vec::new());
        // Вызов из другой панели запись заменяет.
        let navigator = w.navigator(w.a);
        w.focus(navigator);
        let (_, _, outcome, changes) = w.open_with("field", None, None, None);
        assert_eq!(outcome, OpenOutcome::Focused);
        assert!(changes.contains(&Change::PanelOpenedFrom {
            panel,
            opened_from: Some(navigator),
        }));
        assert_eq!(w.focused(w.a), panel);

        // Переезд в другой воркспейс: тот же `PanelId`, данные, вызывающая — панель с фокусом там.
        w.ok(Command::PanelCommand {
            panel,
            name: "field.add".into(),
            args: Some(json!({"by": 7})),
        });
        let caller = w.focused(w.c);
        let window = w.state.window.clone();
        let (again, home, outcome, changes) = w.open_with("field", Some(w.c), None, None);
        assert_eq!((again, home, outcome), (panel, w.c, OpenOutcome::Moved));
        let [
            Change::PanelRehome { .. },
            Change::LayoutSet {
                workbench: from, ..
            },
            Change::LayoutSet { workbench: to, .. },
            Change::PanelOpenedFrom { opened_from, .. },
        ] = changes.as_slice()
        else {
            panic!("неожиданные дельты {changes:?}");
        };
        assert_eq!((*from, *to, *opened_from), (w.a, w.c, Some(caller)));
        assert_eq!(w.count(panel), json!(7));
        assert_eq!(w.state.window, window);
        assert_eq!(w.layout(w.a).locate(panel), None);
        assert_eq!(w.focused(w.c), panel);

        // Область воркспейса для `PerApp` не годится ни у `panel.open`, ни у `layout.split`.
        w.scope = TreeScope::Workspace(w.w2);
        let error = w.err(W::open_command("field", Some(w.c), None, None));
        assert_refused(
            &error,
            &[("reason", "scope_requires_all"), ("field", "scope")],
        );
        let error = w.err(Command::LayoutSplit {
            panel: caller,
            open: Some("field".into()),
            axis: None,
        });
        assert_refused(
            &error,
            &[("reason", "scope_requires_all"), ("field", "scope")],
        );
    }

    /// Список справа с фокусом, поле создано делением центра: вызывающая — список.
    fn field_from_list() -> (W, PanelId, PanelId, PanelId) {
        let mut w = W::new(vec![list(), field()]);
        let terminal = w.focused(w.a);
        let (list, _) = w.open("rows", w.a);
        assert_eq!(w.focused(w.a), list);
        let outcome = w.ok(Command::LayoutSplit {
            panel: terminal,
            open: Some("field".into()),
            axis: None,
        });
        let CommandValue::Panel { panel: field } = outcome.value else {
            panic!("ожидалась панель");
        };
        assert!(outcome.changes.contains(&Change::PanelOpenedFrom {
            panel: field,
            opened_from: Some(list),
        }));
        assert_eq!(w.focused(w.a), field);
        (w, terminal, list, field)
    }

    #[test]
    fn closing_per_app_panel_returns_focus_to_its_caller() {
        let (mut w, terminal, list, field) = field_from_list();
        // Штатный запасной фокус отдал бы его терминалу центра.
        assert_eq!(
            w.layout(w.a).close(field).unwrap().focused(),
            Some(terminal)
        );
        w.close(field);
        assert_eq!(w.focused(w.a), list);
        assert_eq!(w.state.opened_from.get(&field), None);

        // Вызывающая — невыбранная вкладка показанной части: она выбирается.
        let (mut w, terminal, list, field) = field_from_list();
        w.ok(Command::PanelMovePart {
            panel: terminal,
            anchor: list,
            edge: None,
        });
        w.focus(field);
        assert!(!w.layout(w.a).is_visible(list));
        w.close(field);
        assert_eq!(w.focused(w.a), list);
        assert!(w.layout(w.a).is_visible(list));
    }

    #[test]
    fn closing_per_app_panel_falls_back_without_a_shown_caller() {
        let fallback = |w: &W, field| w.layout(w.a).close(field).unwrap();

        // Вызывающая удалена.
        let (mut w, terminal, list, field) = field_from_list();
        w.close(list);
        w.focus(field);
        let expected = fallback(&w, field);
        w.close(field);
        assert_eq!(*w.layout(w.a), expected);
        assert_eq!(w.focused(w.a), terminal);

        // Зона вызывающей скрыта: она не раскрывается.
        let (mut w, terminal, list, field) = field_from_list();
        w.ok(Command::LayoutVisibility {
            workbench: None,
            target: Hideable::Right,
            visible: false,
        });
        assert_eq!(w.focused(w.a), field);
        w.close(field);
        assert_eq!(w.focused(w.a), terminal);
        assert!(!w.layout(w.a).zone_shown(Zone::Right));
        assert!(!w.layout(w.a).is_visible(list));

        // Закрытая панель фокуса не имела: фокус не меняется.
        let (mut w, terminal, _, field) = field_from_list();
        w.focus(terminal);
        w.close(field);
        assert_eq!(w.focused(w.a), terminal);
    }

    #[test]
    fn closing_per_app_panel_with_caller_in_another_workbench_keeps_the_route() {
        let ws = TestPlugin::new("ws").cardinality(Cardinality::PerWorkspace);
        let mut w = W::new(vec![ws, field()]);
        let terminal = w.focused(w.a);
        let (caller, _) = w.open("ws", w.a);
        let (field, _) = w.open("field", w.a);
        assert_eq!(w.state.opened_from.get(&field), Some(&caller));
        // Вызывающая переехала в фоновую рабочую область.
        assert_eq!(w.open("ws", w.b), (caller, OpenOutcome::Moved));
        w.focus(field);
        let (window, other) = (w.state.window.clone(), w.layout(w.b).clone());
        let outcome = w.close(field);
        assert!(
            matches!(
                outcome.changes.as_slice(),
                [Change::LayoutSet { workbench, .. }, Change::PanelRemove { .. }] if *workbench == w.a
            ),
            "{:?}",
            outcome.changes
        );
        assert_eq!(w.focused(w.a), terminal);
        assert_eq!((&w.state.window, w.layout(w.b)), (&window, &other));

        // Закрытие поля в фоновой рабочей области возвращает фокус там и маршрут не меняет.
        let (field, _) = w.open("field", w.c);
        let caller = w.state.opened_from[&field];
        let window = w.state.window.clone();
        w.close(field);
        assert_eq!(w.focused(w.c), caller);
        assert_eq!(w.state.window, window);
    }

    #[test]
    fn split_open_refuses_existing_singleton_and_creates_through_the_same_rule() {
        let mut w = W::new(vec![list()]);
        let terminal = w.focused(w.a);
        let (panel, _) = w.open("rows", w.a);
        let error = w.err(Command::LayoutSplit {
            panel: terminal,
            open: Some("rows".into()),
            axis: None,
        });
        assert_refused(&error, &[("reason", "singleton_exists"), ("field", "open")]);
        assert_eq!(error.detail_str("panel"), Some(panel.to_string().as_str()));

        // В другой рабочей области экземпляра нет: деление создаёт его, `panel.open` находит.
        let other = w.focused(w.b);
        let CommandValue::Panel { panel: made } = w
            .ok(Command::LayoutSplit {
                panel: other,
                open: Some("rows".into()),
                axis: None,
            })
            .value
        else {
            panic!("ожидалась панель");
        };
        assert_eq!(w.state.panels[&made].title.as_str(), "Список");
        assert_eq!(w.count(made), json!(0));
        assert_eq!(w.open("rows", w.b), (made, OpenOutcome::Focused));
    }

    #[test]
    fn panel_command_applies_action_and_refuses_bad_requests() {
        let mut w = W::new(vec![list(), TestPlugin::new("plain")]);
        let (panel, _) = w.open("rows", w.a);
        let command = |name: &str, args: Option<Value>| Command::PanelCommand {
            panel,
            name: name.into(),
            args,
        };
        let outcome = w.ok(command("rows.add", Some(json!({"by": 2}))));
        assert_eq!(outcome.value, CommandValue::Panel { panel });
        assert_eq!(
            outcome.changes,
            [Change::PanelData {
                panel,
                version: 1,
                data: r#"{"count":2}"#.into(),
            }]
        );
        // Прежние данные — ответ без записи.
        let outcome = w.ok(command("rows.add", Some(json!({"by": 0}))));
        assert_eq!(outcome.value, CommandValue::Panel { panel });
        assert_eq!(outcome.changes, Vec::new());

        let error = w.err(command("rows.none", None));
        assert_refused(&error, &[("reason", "command_unknown"), ("field", "name")]);
        let error = w.err(command("rows.add", None));
        assert_refused(&error, &[("reason", "args_invalid"), ("field", "args")]);
        let error = w.err(command("rows.add", Some(json!({"by": 1, "x": 2}))));
        assert_refused(&error, &[("reason", "args_invalid"), ("field", "args")]);
        let error = w.err(command("rows.add", Some(json!([1]))));
        assert_refused(&error, &[("reason", "args_invalid"), ("field", "args")]);
        let error = w.err(command("rows.add", Some(json!({"by": 9}))));
        assert_refused(&error, &[("reason", "count_out_of_range"), ("field", "by")]);
        let error = w.err(command("rows.pad", Some(json!({"size": STATE_LIMIT}))));
        assert_refused(
            &error,
            &[("reason", "panel_state_too_large"), ("field", "args")],
        );
        assert_eq!(error.detail("limit"), Some(&json!(STATE_LIMIT)));

        // Плагин без действий и панель, которой нет.
        let (plain, _) = w.open("plain", w.a);
        let error = w.err(Command::PanelCommand {
            panel: plain,
            name: "plain.add".into(),
            args: None,
        });
        assert_refused(&error, &[("reason", "command_unknown"), ("field", "name")]);
        let error = w.err(Command::PanelCommand {
            panel: PanelId::from_bytes([9; 16]),
            name: "rows.add".into(),
            args: None,
        });
        assert_eq!(error.code(), ErrorCode::NotFound, "{error}");
    }

    /// Две панели `field` прежней сборки: старшая в `a`, младшая в `c`.
    fn duplicates() -> (W, PanelId, PanelId) {
        let mut w = W::new(vec![field()]);
        let old = w.legacy(w.a, "field", 10);
        let young = w.legacy(w.c, "field", 20);
        w.state.panel_data.insert(
            old,
            PanelData::Text {
                version: 1,
                raw: r#"{"count":1}"#.into(),
            },
        );
        w.restart();
        assert_eq!(w.state.status_of(old), PanelStatus::Ready);
        assert_eq!(w.state.status_of(young), PanelStatus::Duplicate);
        (w, old, young)
    }

    #[test]
    fn duplicate_refuses_commands_and_names_canonical_only_inside_scope() {
        let (mut w, old, young) = duplicates();
        let error = w.err(Command::PanelCommand {
            panel: young,
            name: "field.add".into(),
            args: Some(json!({"by": 1})),
        });
        assert_refused(
            &error,
            &[
                ("reason", "panel_unavailable"),
                ("field", "panel"),
                ("status", "duplicate"),
            ],
        );
        let view = w.view(TreeScope::All, young).unwrap();
        assert_eq!(
            (view.status, view.canonical),
            (PanelStatus::Duplicate, Some(old))
        );
        assert_eq!(
            (view.version, &view.data, view.readable),
            (None, &Value::Null, true)
        );
        // Канонический экземпляр чужого воркспейса не раскрывается.
        let view = w.view(TreeScope::Workspace(w.w2), young).unwrap();
        assert_eq!(
            (view.status, view.canonical),
            (PanelStatus::Duplicate, None)
        );
        let error = w.view(TreeScope::Workspace(w.w2), old).unwrap_err();
        assert_eq!(error.code(), ErrorCode::NotFound, "{error}");
        let view = w.view(TreeScope::All, old).unwrap();
        assert_eq!((view.status, view.canonical), (PanelStatus::Ready, None));
        assert_eq!((view.version, &view.data), (Some(1), &json!({"count": 1})));
    }

    #[test]
    fn canonical_moves_past_its_duplicate_and_closing_it_passes_the_right() {
        let (mut w, old, young) = duplicates();
        // `panel.open` переносит канонический экземпляр, дубликат остаётся заглушкой.
        let (panel, home, outcome, _) = w.open_with("field", Some(w.c), None, None);
        assert_eq!((panel, home, outcome), (old, w.c, OpenOutcome::Moved));
        assert_eq!(w.state.status_of(young), PanelStatus::Duplicate);
        assert_eq!(w.state.panels[&young].workbench, w.c);

        // Закрытие канонического: старейший оставшийся получает право и `init` той же записью.
        let outcome = w.close(old);
        let [
            Change::LayoutSet { .. },
            Change::PanelRemove { panel: removed },
            Change::PanelData {
                panel: heir,
                version: 1,
                data,
            },
            Change::PanelStatus {
                panel: ready,
                status: PanelStatus::Ready,
            },
        ] = outcome.changes.as_slice()
        else {
            panic!("неожиданные дельты {:?}", outcome.changes);
        };
        assert_eq!(
            (*removed, *heir, *ready, data.as_str()),
            (old, young, young, r#"{"count":0}"#)
        );
        assert_eq!(w.state.status_of(young), PanelStatus::Ready);
        // Следующий `open` находит наследника, команды его принимают.
        assert_eq!(w.open("field", w.c), (young, OpenOutcome::Focused));
        w.ok(Command::PanelCommand {
            panel: young,
            name: "field.add".into(),
            args: Some(json!({"by": 3})),
        });
        assert_eq!(w.count(young), json!(3));
    }

    #[test]
    fn heir_keeps_its_bytes_and_takes_the_status_of_its_data() {
        // Данные текущей версии остаются побайтно, данные новее дают `state_newer`.
        for (version, raw, status) in [
            (1, r#"{"count": 4}"#, PanelStatus::Ready),
            (2, r#"{"later":true}"#, PanelStatus::StateNewer),
            (1, "{", PanelStatus::StateCorrupt),
        ] {
            let (mut w, old, young) = duplicates();
            let bytes = PanelData::Text {
                version,
                raw: raw.into(),
            };
            w.state.panel_data.insert(young, bytes.clone());
            let outcome = w.close(old);
            assert_eq!(w.state.status_of(young), status, "{raw}");
            assert_eq!(w.state.panel_data.get(&young), Some(&bytes), "{raw}");
            assert!(
                !outcome
                    .changes
                    .iter()
                    .any(|change| matches!(change, Change::PanelData { .. })),
                "{:?}",
                outcome.changes
            );
            if status != PanelStatus::Ready {
                let error = w.err(W::open_command("field", Some(w.c), None, None));
                assert_refused(
                    &error,
                    &[("reason", "panel_unavailable"), ("status", status.as_str())],
                );
                let view = w.view(TreeScope::All, young).unwrap();
                assert_eq!(view.data, Value::String(raw.into()));
            }
        }
    }

    #[test]
    fn query_panel_and_plugins_describe_panel_and_registry() {
        let mut w = W::new(vec![list(), field()]);
        let terminal = w.focused(w.a);
        let (panel, _) = w.open("field", w.a);
        let view = w.view(TreeScope::Workspace(w.w1), panel).unwrap();
        assert_eq!(view.panel, w.state.panels[&panel]);
        assert_eq!(view.status, PanelStatus::Ready);
        assert_eq!((view.version, &view.data), (Some(1), &json!({"count": 0})));
        assert!(view.readable);
        assert_eq!((view.opened_from, view.canonical), (Some(terminal), None));
        assert_eq!(view.min_size, shellio_layout::MinSize::DEFAULT);
        // Данные заполнителя — `null` версии 1.
        let view = w.view(TreeScope::All, terminal).unwrap();
        assert_eq!(
            (view.version, &view.data, view.readable),
            (Some(1), &Value::Null, true)
        );
        // Нечитаемая строка.
        w.state.panel_data.insert(panel, PanelData::Unreadable {});
        w.restart();
        let view = w.view(TreeScope::All, panel).unwrap();
        assert_eq!(view.status, PanelStatus::StateCorrupt);
        assert_eq!(
            (view.version, &view.data, view.readable),
            (None, &Value::Null, false)
        );

        let storage = StorageInfo {
            schema_version: 3,
            state_db: "/state/state.db".into(),
        };
        let reply = read(
            &w.state,
            &w.registry,
            Revision(4),
            &storage,
            None,
            &Query::Plugins {},
        )
        .unwrap();
        let ReplyValue::Plugins(view) = reply.value else {
            panic!("ожидались плагины");
        };
        assert_eq!(view.plugins, w.registry.infos());
        let ids: Vec<&str> = view
            .plugins
            .iter()
            .map(|plugin| plugin.id.as_str())
            .collect();
        assert_eq!(ids, ["terminal", "navigator", "rows", "field"]);
        assert_eq!(view.plugins[2].commands, ["rows.add", "rows.pad"]);
        assert_eq!(view.plugins[2].default_zone, Zone::Right);
        assert_eq!(view.plugins[3].cardinality, Cardinality::PerApp);
    }
}
