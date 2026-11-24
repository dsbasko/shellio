//! Команды раскладки и панелей (ADR-0007 и ADR-0009, «Уточнено в P1.4 (планирование)»).
//!
//! Обработчик выбирает рабочую область, выполняет чистую операцию `shellio-layout` и отвечает
//! одной дельтой `LayoutSet` с новой раскладкой. Раскладка, равная прежней, — ответ без новой
//! ревизии. Область доступа уже проверил актор на том же снимке.

use serde_json::{Value, json};
use shellio_layout::{Hideable, Location, MinSize, OpError, Room, Viewport, Zone};
use shellio_plugin_api::{Cardinality, Registry};

use crate::change::Change;
use crate::command::{CommandValue, LayoutCommand, chosen_workbench, panel_workbench};
use crate::error::{CoreError, Entity};
use crate::handler::{Ctx, Outcome};
use crate::ids::{PanelId, WorkbenchId};
use crate::model::State;
use crate::panel::{WorkbenchLayout, is_navigator, layout_metrics};
use crate::panel_cmd::{allowed_for_panel, allowed_zone, create, registered, require_all};
use crate::plugin_host::{heir, instance, min_size};

/// Обработать команду раскладки над снимком состояния.
pub(crate) fn layout_command(
    state: &State,
    command: LayoutCommand,
    ctx: &mut Ctx<'_>,
) -> Result<Outcome, CoreError> {
    let metrics = layout_metrics();
    let registry = ctx.registry;
    let sizes = |panel: PanelId| panel_min_size(state, registry, panel);
    match command {
        LayoutCommand::Split { panel, open } => {
            let workbench = panel_workbench(state, panel)?;
            let layout = current(state, workbench)?;
            let Some(open) = open else {
                let viewport = need_viewport(ctx, "layout.split")?;
                let next = layout
                    .split(panel, None, viewport, &metrics, sizes)
                    .map_err(|error| refusal(error, "panel"))?;
                return Ok(set(workbench, layout, next));
            };
            // Новую панель создаёт та же функция, что `panel.open`: существующий singleton —
            // отказ, а не фокус.
            let plugin = registered(registry, &open, "open")?;
            let manifest = plugin.manifest();
            require_all(manifest, ctx.scope, "layout.split")?;
            if let Some(Location::Part { zone, .. }) = layout.locate(panel) {
                allowed_zone(manifest, zone, "open")?;
            }
            if let Some(existing) = instance(state, plugin, workbench, None) {
                return Err(CoreError::singleton_exists(existing));
            }
            let viewport = need_viewport(ctx, "layout.split")?;
            let need = manifest.min_size;
            let (id, mut changes) =
                create(plugin, workbench, None, layout.focused(), Some("open"), ctx)?;
            let next = layout
                .split(panel, Some(id), viewport, &metrics, |p| {
                    if p == id { need } else { sizes(p) }
                })
                .map_err(|error| refusal(error, "panel"))?;
            changes.push(Change::LayoutSet {
                workbench,
                layout: next,
            });
            Ok(Outcome::new(CommandValue::Panel { panel: id }, changes))
        }
        LayoutCommand::Focus { panel } | LayoutCommand::Select { panel } => {
            with_panel(state, panel, |layout| layout.focus_panel(panel))
        }
        LayoutCommand::FocusDirection {
            workbench,
            direction,
        } => with_workbench(state, workbench, |layout| {
            Ok(layout.focus_direction(direction, ctx.viewport, &metrics))
        }),
        LayoutCommand::Move { panel, zone } => {
            panel_workbench(state, panel)?;
            allowed_for_panel(state, registry, panel, zone, "zone")?;
            let navigator = state
                .panels
                .get(&panel)
                .is_some_and(|record| is_navigator(&record.plugin));
            with_panel(state, panel, |layout| {
                layout.move_to_zone(panel, zone, navigator)
            })
        }
        LayoutCommand::MovePart {
            panel,
            anchor,
            edge,
        } => {
            let workbench = panel_workbench(state, panel)?;
            let layout = current(state, workbench)?;
            if let Some(Location::Part { zone, .. }) = layout.locate(anchor) {
                allowed_for_panel(state, registry, panel, zone, "anchor")?;
            }
            let next = layout
                .move_to_part(panel, anchor, edge)
                .map_err(|error| refusal(error, "anchor"))?;
            Ok(set(workbench, layout, next))
        }
        LayoutCommand::Close { panel } => {
            let workbench = panel_workbench(state, panel)?;
            let layout = current(state, workbench)?;
            let mut next = layout
                .close(panel)
                .map_err(|error| refusal(error, "panel"))?;
            // Возврат фокуса `PerApp` (ADR-0008): вызывающей панели, если она стоит в показанной
            // части этой раскладки. Скрытая зона не раскрывается.
            let per_app = state
                .panels
                .get(&panel)
                .and_then(|record| registry.get(&record.plugin))
                .is_some_and(|plugin| plugin.manifest().cardinality == Cardinality::PerApp);
            if per_app
                && layout.focused() == Some(panel)
                && let Some(&caller) = state.opened_from.get(&panel)
                && part_shown(&next, caller)
            {
                next = next
                    .focus_panel(caller)
                    .map_err(|error| refusal(error, "panel"))?;
            }
            let mut changes = vec![
                Change::LayoutSet {
                    workbench,
                    layout: next,
                },
                Change::PanelRemove { panel },
            ];
            changes.extend(heir(state, registry, panel));
            Ok(Outcome::new(CommandValue::None, changes))
        }
        LayoutCommand::Resize {
            workbench,
            zone,
            part_of: Some(panel),
            size,
        } => {
            let owner = panel_workbench(state, panel)?;
            if workbench.is_some_and(|explicit| explicit != owner) {
                return Err(CoreError::not_found(Entity::Panel, panel));
            }
            let layout = current(state, owner)?;
            let viewport = need_viewport(ctx, "layout.resize")?;
            let share = u16::try_from(size).unwrap_or(u16::MAX);
            let next = layout
                .resize_part(zone, panel, share, viewport, &metrics, sizes)
                .map_err(|error| refusal(error, "part_of"))?;
            Ok(set(owner, layout, next))
        }
        LayoutCommand::Resize {
            workbench,
            zone,
            part_of: None,
            size,
        } => {
            let workbench = chosen_workbench(state, workbench)?;
            let layout = current(state, workbench)?;
            if zone == Zone::Center {
                return Err(refusal(OpError::ZoneNotResizable(zone), "zone"));
            }
            let viewport = need_viewport(ctx, "layout.resize")?;
            let next = layout
                .resize_zone(zone, size, viewport, &metrics, sizes)
                .map_err(|error| refusal(error, "zone"))?;
            Ok(set(workbench, layout, next))
        }
        LayoutCommand::Equal { workbench, zone } => {
            with_workbench(state, workbench, |layout| layout.equal(zone))
        }
        LayoutCommand::Zoom { panel } => with_panel(state, panel, |layout| layout.zoom(panel)),
        LayoutCommand::Unzoom { workbench } => {
            with_workbench(state, workbench, |layout| Ok(layout.unzoom()))
        }
        LayoutCommand::Only { workbench } => {
            with_workbench(state, workbench, |layout| Ok(layout.toggle_only()))
        }
        LayoutCommand::Visibility {
            workbench,
            target,
            visible,
        } => with_workbench(state, workbench, |layout| {
            layout.set_visible(target, visible)
        }),
    }
}

/// `min_size` панели из манифеста её плагина. Панели нет в состоянии — значение по умолчанию.
pub(crate) fn panel_min_size(state: &State, registry: &Registry, panel: PanelId) -> MinSize {
    state.panels.get(&panel).map_or(MinSize::DEFAULT, |record| {
        min_size(registry, &record.plugin)
    })
}

/// Место панели показано: колонка навигатора или часть зоны без скрытия, чужого `only` и чужого
/// `zoom`. Вкладка может быть невыбранной.
fn part_shown(layout: &WorkbenchLayout, panel: PanelId) -> bool {
    if layout.zoomed_panel().is_some_and(|zoomed| zoomed != panel) {
        return false;
    }
    match layout.locate(panel) {
        None => false,
        Some(Location::Navigator) => layout.navigator_shown(),
        Some(Location::Part { zone, .. }) => layout.zone_shown(zone),
    }
}

fn need_viewport(ctx: &Ctx<'_>, command: &str) -> Result<Viewport, CoreError> {
    ctx.viewport.ok_or_else(|| CoreError::no_ui(command))
}

/// Раскладка рабочей области. После запуска и `workbench.create` она есть у каждой.
pub(crate) fn current(
    state: &State,
    workbench: WorkbenchId,
) -> Result<&WorkbenchLayout, CoreError> {
    state
        .layouts
        .get(&workbench)
        .ok_or_else(|| CoreError::not_found(Entity::Workbench, workbench))
}

/// Операция над раскладкой рабочей области панели.
fn with_panel(
    state: &State,
    panel: PanelId,
    op: impl FnOnce(&WorkbenchLayout) -> Result<WorkbenchLayout, OpError<PanelId>>,
) -> Result<Outcome, CoreError> {
    let workbench = panel_workbench(state, panel)?;
    let layout = current(state, workbench)?;
    let next = op(layout).map_err(|error| refusal(error, "panel"))?;
    Ok(set(workbench, layout, next))
}

/// Операция над раскладкой явной или открытой рабочей области.
fn with_workbench(
    state: &State,
    workbench: Option<WorkbenchId>,
    op: impl FnOnce(&WorkbenchLayout) -> Result<WorkbenchLayout, OpError<PanelId>>,
) -> Result<Outcome, CoreError> {
    let workbench = chosen_workbench(state, workbench)?;
    let layout = current(state, workbench)?;
    let next = op(layout).map_err(|error| refusal(error, "panel"))?;
    Ok(set(workbench, layout, next))
}

/// Новая раскладка одной дельтой; равная прежней — без изменений.
fn set(workbench: WorkbenchId, before: &WorkbenchLayout, after: WorkbenchLayout) -> Outcome {
    if *before == after {
        return Outcome::unchanged();
    }
    Outcome::new(
        CommandValue::None,
        vec![Change::LayoutSet {
            workbench,
            layout: after,
        }],
    )
}

/// Отказ модели как ошибка ядра. `column` — поле панели, которая оказалась в колонке
/// навигатора: `panel` у деления, `anchor` у переноса в часть, `part_of` у размера части.
pub(crate) fn refusal(error: OpError<PanelId>, column: &str) -> CoreError {
    match error {
        OpError::NotFound(panel) => CoreError::not_found(Entity::Panel, panel),
        OpError::AlreadyPresent(panel) => CoreError::storage_conflict(format!(
            "новая панель {panel} уже стоит в раскладке: идентификатор выдан повторно"
        )),
        OpError::NavigatorColumn(panel) => CoreError::navigator_column(column, panel),
        OpError::SplitNeedsPanel(panel) => CoreError::split_needs_panel(panel),
        OpError::SplitNoRoom(rooms) => split_no_room(&rooms),
        OpError::ZoneEmpty(zone) => CoreError::zone_empty("zone", zone_name(zone)),
        OpError::TargetEmpty(target) => CoreError::zone_empty("target", target_name(target)),
        OpError::ZoneNotResizable(zone) => CoreError::zone_not_resizable(zone_name(zone)),
        OpError::NotInZone { panel, zone } => CoreError::panel_not_in_zone(panel, zone_name(zone)),
        OpError::ResizeOutOfRange { min, max } => CoreError::resize_out_of_range(min, max),
    }
}

fn split_no_room(rooms: &[Room<PanelId>]) -> CoreError {
    let parts: Vec<Value> = rooms
        .iter()
        .map(|room| {
            json!({
                "panel": room.panel.to_string(),
                "need": {"width": room.need.width, "height": room.need.height},
                "have": {"width": room.have.width, "height": room.have.height},
            })
        })
        .collect();
    let detail: Vec<String> = rooms
        .iter()
        .map(|room| {
            format!(
                "панели {} нужно {}×{}, будет {}×{}",
                room.panel, room.need.width, room.need.height, room.have.width, room.have.height
            )
        })
        .collect();
    CoreError::split_no_room(
        Value::Array(parts),
        format!("новой части не хватит места: {}", detail.join("; ")),
    )
}

/// Имя зоны на проводе.
pub(crate) const fn zone_name(zone: Zone) -> &'static str {
    match zone {
        Zone::Left => "left",
        Zone::Right => "right",
        Zone::Center => "center",
        Zone::Bottom => "bottom",
    }
}

const fn target_name(target: Hideable) -> &'static str {
    match target {
        Hideable::Navigator => "navigator",
        Hideable::Plate => "plate",
        Hideable::Right => "right",
        Hideable::Bottom => "bottom",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use shellio_layout::{Direction, Edge};

    use super::*;
    use crate::clock::UnixMillis;
    use crate::command::Command;
    use crate::error::ErrorCode;
    use crate::handler::{handle, startup};
    use crate::model::Route;
    use crate::panel::PanelStatus;
    use crate::query::{LayoutView, Query, StorageInfo, read};
    use crate::test_support::{
        FakeRootResolver, ManualClock, SequentialIds, TestPlugin, registry, registry_of,
    };
    use crate::tree::TreeScope;
    use crate::wire::ReplyValue;

    const VIEWPORT: Viewport = Viewport {
        width: 1180,
        height: 720,
    };

    /// Рабочая область, созданная и открытая штатными командами, и окно 1180×720.
    struct H {
        state: State,
        ids: SequentialIds,
        clock: ManualClock,
        resolver: FakeRootResolver,
        registry: Arc<Registry>,
        viewport: Option<Viewport>,
        workbench: WorkbenchId,
        navigator: PanelId,
        terminal: PanelId,
    }

    impl H {
        fn new() -> Self {
            let mut h = Self {
                state: State::default(),
                ids: SequentialIds::new(),
                clock: ManualClock::new(UnixMillis(1_000)),
                resolver: FakeRootResolver::new()
                    .with_git("/src/app", "/src/app/.git")
                    .with_git("/src/docs", "/src/docs/.git"),
                registry: registry(),
                viewport: Some(VIEWPORT),
                workbench: WorkbenchId::from_bytes([0; 16]),
                navigator: PanelId::from_bytes([0; 16]),
                terminal: PanelId::from_bytes([0; 16]),
            };
            let mut ctx = Ctx::new(&h.clock, &mut h.ids, &h.registry);
            let outcome = startup(&h.state, &mut ctx).unwrap().unwrap();
            h.state.apply_all(&outcome.changes);
            let workspace = h.state.workspaces_in_order()[0].id;
            h.workbench = h.workbench_in(workspace, "/src/app");
            h.ok(Command::WorkbenchOpen {
                workbench: h.workbench,
            });
            let (navigator, terminal) = (h.layout().navigator, h.layout().focused());
            h.navigator = navigator.unwrap();
            h.terminal = terminal.unwrap();
            h
        }

        /// Новая рабочая область в новом проекте воркспейса.
        fn workbench_in(&mut self, workspace: crate::ids::WorkspaceId, path: &str) -> WorkbenchId {
            let CommandValue::Project { project, .. } = self
                .ok(Command::ProjectAdd {
                    workspace,
                    path: path.into(),
                    title: None,
                })
                .value
            else {
                panic!("ожидался проект");
            };
            let CommandValue::Workbench { workbench } = self
                .ok(Command::WorkbenchCreate {
                    project,
                    title: "Основная".into(),
                })
                .value
            else {
                panic!("ожидалась рабочая область");
            };
            workbench
        }

        fn run(&mut self, command: Command) -> Result<Outcome, CoreError> {
            let prepared = command.prepare(&self.resolver)?;
            let mut ctx =
                Ctx::new(&self.clock, &mut self.ids, &self.registry).with_viewport(self.viewport);
            let outcome = handle(&self.state, prepared, &mut ctx)?;
            self.state.apply_all(&outcome.changes);
            assert_eq!(self.state.check_invariants(), Vec::new());
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

        fn layout(&self) -> &WorkbenchLayout {
            &self.state.layouts[&self.workbench]
        }

        /// Деление части панели с новой панелью `plugin`.
        fn split_open(&mut self, panel: PanelId, plugin: &str) -> PanelId {
            match self
                .ok(Command::LayoutSplit {
                    panel,
                    open: Some(plugin.into()),
                })
                .value
            {
                CommandValue::Panel { panel } => panel,
                other => panic!("ожидалась панель, получено {other:?}"),
            }
        }

        fn center_tabs(&self) -> Vec<Vec<PanelId>> {
            self.layout()
                .zones
                .center
                .parts
                .iter()
                .map(|part| part.tabs.clone())
                .collect()
        }
    }

    fn assert_refused(error: &CoreError, code: ErrorCode, details: &[(&str, &str)]) {
        assert_eq!(error.code(), code, "{error}");
        for (key, value) in details {
            assert_eq!(error.detail_str(key), Some(*value), "{key} в {error}");
        }
    }

    fn single_layout_set(outcome: &Outcome) -> &WorkbenchLayout {
        let [Change::LayoutSet { layout, .. }] = outcome.changes.as_slice() else {
            panic!("ожидалась одна дельта LayoutSet: {:?}", outcome.changes);
        };
        layout
    }

    // --- успех ---

    #[test]
    fn split_with_open_adds_panel_in_new_part_with_focus() {
        let mut h = H::new();
        let terminal = h.terminal;
        let outcome = h.ok(Command::LayoutSplit {
            panel: terminal,
            open: Some("terminal".into()),
        });
        let CommandValue::Panel { panel } = outcome.value else {
            panic!("ожидалась панель, получено {:?}", outcome.value);
        };
        // Начальные данные новой панели идут той же записью, что сама панель.
        let [
            Change::PanelAdd { panel: added },
            Change::PanelData {
                panel: with_data,
                version: 1,
                data,
            },
            Change::LayoutSet { .. },
        ] = outcome.changes.as_slice()
        else {
            panic!("неожиданные дельты {:?}", outcome.changes);
        };
        assert_eq!((*with_data, data.as_str()), (panel, "null"));
        assert_eq!(h.state.status_of(panel), PanelStatus::Ready);
        assert_eq!(added.id, panel);
        assert_eq!(added.workbench, h.workbench);
        assert_eq!(added.plugin.as_str(), "terminal");
        assert_eq!(added.title.as_str(), "Терминал");
        assert_eq!(h.center_tabs(), [vec![terminal], vec![panel]]);
        assert_eq!(h.layout().focused(), Some(panel));
    }

    #[test]
    fn split_with_open_takes_title_and_data_from_the_registered_plugin() {
        let mut h = H::new();
        h.registry = registry_of(vec![TestPlugin::new("notes").title("Заметки")]);
        let notes = h.split_open(h.terminal, "notes");
        assert_eq!(h.state.panels[&notes].title.as_str(), "Заметки");
        assert_eq!(
            h.state.panel_data.get(&notes),
            Some(&crate::panel::PanelData::Text {
                version: 1,
                raw: r#"{"count":0}"#.to_owned(),
            })
        );
        assert_eq!(h.state.status_of(notes), PanelStatus::Ready);
    }

    #[test]
    fn split_with_open_refuses_unregistered_plugin() {
        let mut h = H::new();
        let error = h.err(Command::LayoutSplit {
            panel: h.terminal,
            open: Some("notes".into()),
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "plugin_unknown"), ("field", "open")],
        );
    }

    #[test]
    fn split_without_open_moves_active_tab_to_new_part() {
        let mut h = H::new();
        let terminal = h.terminal;
        let other = h.split_open(terminal, "terminal");
        h.ok(Command::PanelMovePart {
            panel: other,
            anchor: terminal,
            edge: None,
        });
        assert_eq!(h.center_tabs(), [vec![terminal, other]]);
        let outcome = h.ok(Command::LayoutSplit {
            panel: terminal,
            open: None,
        });
        assert_eq!(outcome.value, CommandValue::None);
        single_layout_set(&outcome);
        assert_eq!(h.center_tabs(), [vec![other], vec![terminal]]);
        assert_eq!(h.layout().focused(), Some(terminal));
    }

    #[test]
    fn explicit_target_in_unfocused_part_is_one_revision() {
        let mut h = H::new();
        let terminal = h.terminal;
        let right = h.split_open(terminal, "terminal");
        assert_eq!(h.layout().focused(), Some(right));

        // «Развернуть» в левой части без фокуса: фокус и разворот одной дельтой.
        let outcome = h.ok(Command::LayoutZoom {
            panel: Some(terminal),
            workbench: None,
            off: None,
        });
        let layout = single_layout_set(&outcome);
        assert_eq!(layout.zoomed, Some(terminal));
        assert_eq!(layout.focused(), Some(terminal));
        h.ok(Command::LayoutZoom {
            panel: None,
            workbench: None,
            off: Some(true),
        });
        assert_eq!(h.layout().zoomed, None);

        // «Разделить» в правой части без фокуса: одна запись с новой панелью.
        h.ok(Command::PanelFocus {
            panel: Some(terminal),
            workbench: None,
            direction: None,
        });
        let outcome = h.ok(Command::LayoutSplit {
            panel: right,
            open: Some("terminal".into()),
        });
        assert!(
            matches!(
                outcome.changes.as_slice(),
                [
                    Change::PanelAdd { .. },
                    Change::PanelData { .. },
                    Change::LayoutSet { .. }
                ]
            ),
            "{:?}",
            outcome.changes
        );
        let CommandValue::Panel { panel } = outcome.value else {
            panic!("ожидалась панель");
        };
        assert_eq!(h.center_tabs(), [vec![terminal], vec![right], vec![panel]]);
        assert_eq!(h.layout().focused(), Some(panel));
    }

    #[test]
    fn single_tab_of_first_part_moves_into_second_part_by_anchor() {
        let mut h = H::new();
        let terminal = h.terminal;
        let second = h.split_open(terminal, "terminal");
        let outcome = h.ok(Command::PanelMovePart {
            panel: terminal,
            anchor: second,
            edge: None,
        });
        single_layout_set(&outcome);
        assert_eq!(h.center_tabs(), [vec![second, terminal]]);
        let part = &h.layout().zones.center.parts[0];
        assert_eq!(part.active, 1);
        assert_eq!(part.share, 1000);

        // С краем — новая часть рядом с частью якоря.
        h.ok(Command::PanelMovePart {
            panel: terminal,
            anchor: second,
            edge: Some(Edge::Before),
        });
        assert_eq!(h.center_tabs(), [vec![terminal], vec![second]]);
    }

    #[test]
    fn focus_follows_moved_panel() {
        let mut h = H::new();
        let terminal = h.terminal;
        assert_eq!(h.layout().focused(), Some(terminal));
        h.ok(Command::PanelMove {
            panel: terminal,
            zone: Zone::Right,
        });
        assert_eq!(h.layout().focused(), Some(terminal));
        assert!(matches!(
            h.layout().locate(terminal),
            Some(Location::Part {
                zone: Zone::Right,
                ..
            })
        ));
        assert!(h.layout().zones.center.is_empty());

        // Навигатор, перенесённый в левую зону, остаётся в колонке.
        let navigator = h.navigator;
        let outcome = h.ok(Command::PanelMove {
            panel: navigator,
            zone: Zone::Left,
        });
        assert_eq!(h.layout().locate(navigator), Some(Location::Navigator));
        assert_eq!(h.layout().focused(), Some(terminal));
        assert_eq!(outcome.changes, Vec::new(), "раскладка не изменилась");
    }

    #[test]
    fn close_removes_panel_and_its_place() {
        let mut h = H::new();
        let terminal = h.terminal;
        let second = h.split_open(terminal, "terminal");
        let outcome = h.ok(Command::PanelClose { panel: second });
        let [Change::LayoutSet { .. }, Change::PanelRemove { panel }] = outcome.changes.as_slice()
        else {
            panic!("неожиданные дельты {:?}", outcome.changes);
        };
        assert_eq!(*panel, second);
        assert!(!h.state.panels.contains_key(&second));
        assert_eq!(h.center_tabs(), [vec![terminal]]);
        assert_eq!(h.layout().focused(), Some(terminal));
    }

    #[test]
    fn select_focus_direction_resize_equal_only_and_visibility() {
        let mut h = H::new();
        let terminal = h.terminal;
        // Фокус уже на панели: без изменений.
        assert_eq!(
            h.ok(Command::PanelSelect { panel: terminal }).changes,
            Vec::new()
        );
        let right = h.split_open(terminal, "terminal");

        // Фокус по направлению работает и без окна: по опорному размеру.
        h.viewport = None;
        h.ok(Command::PanelFocus {
            panel: None,
            workbench: None,
            direction: Some(Direction::Left),
        });
        assert_eq!(h.layout().focused(), Some(terminal));
        h.ok(Command::PanelFocus {
            panel: None,
            workbench: Some(h.workbench),
            direction: Some(Direction::Right),
        });
        assert_eq!(h.layout().focused(), Some(right));
        h.viewport = Some(VIEWPORT);

        h.ok(Command::LayoutResize {
            workbench: None,
            zone: Zone::Center,
            part_of: Some(terminal),
            size: 600,
        });
        let shares: Vec<u16> = h
            .layout()
            .zones
            .center
            .parts
            .iter()
            .map(|p| p.share)
            .collect();
        assert_eq!(shares, [600, 400]);
        h.ok(Command::LayoutEqual {
            workbench: None,
            zone: Zone::Center,
        });
        let shares: Vec<u16> = h
            .layout()
            .zones
            .center
            .parts
            .iter()
            .map(|p| p.share)
            .collect();
        assert_eq!(shares, [500, 500]);

        h.ok(Command::PanelMove {
            panel: right,
            zone: Zone::Right,
        });
        h.ok(Command::LayoutResize {
            workbench: Some(h.workbench),
            zone: Zone::Right,
            part_of: None,
            size: 300,
        });
        assert_eq!(h.layout().sizes.right, 300);

        h.ok(Command::LayoutVisibility {
            workbench: None,
            target: Hideable::Right,
            visible: false,
        });
        assert!(h.layout().hidden.right);
        assert_eq!(
            h.layout().focused(),
            Some(terminal),
            "фокус ушёл из скрытой зоны"
        );
        h.ok(Command::LayoutVisibility {
            workbench: None,
            target: Hideable::Right,
            visible: true,
        });
        assert_eq!(h.layout().sizes.right, 300, "размер после показа прежний");

        h.ok(Command::LayoutOnly { workbench: None });
        assert_eq!(h.layout().only, Some(Zone::Center));
        h.ok(Command::LayoutOnly { workbench: None });
        assert_eq!(h.layout().only, None);
    }

    #[test]
    fn query_layout_reads_frames_by_viewport() {
        let mut h = H::new();
        let storage = StorageInfo {
            schema_version: 2,
            state_db: "/state/state.db".into(),
        };
        let query = Query::Layout {
            scope: TreeScope::All,
            workbench: None,
        };
        let view = |viewport, state: &State| -> LayoutView {
            match read(
                state,
                &registry(),
                crate::model::Revision(1),
                &storage,
                viewport,
                &query,
            )
            .unwrap()
            .value
            {
                ReplyValue::Layout(view) => *view,
                other => panic!("ожидалась раскладка, получено {other:?}"),
            }
        };
        let with = view(Some(VIEWPORT), &h.state);
        assert_eq!(with.workbench, h.workbench);
        assert_eq!(&with.layout, h.layout());
        let panels: Vec<PanelId> = with.panels.iter().map(|panel| panel.id).collect();
        assert_eq!(panels, [h.navigator, h.terminal]);
        let frames = with.frames.unwrap();
        assert_eq!(frames.navigator.unwrap().frame.width, 236.0);
        assert_eq!(frames.parts[0].active, h.terminal);
        assert_eq!(view(None, &h.state).frames, None);

        h.state.window.route = Route::WorkspaceHome;
        let error = read(
            &h.state,
            &h.registry,
            crate::model::Revision(1),
            &storage,
            None,
            &query,
        )
        .unwrap_err();
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "no_open_workbench")],
        );
    }

    // --- отказы ---

    #[test]
    fn split_refusals() {
        let mut h = H::new();
        let terminal = h.terminal;
        let navigator = h.navigator;
        let error = h.err(Command::LayoutSplit {
            panel: terminal,
            open: None,
        });
        let panel = terminal.to_string();
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[
                ("reason", "split_needs_panel"),
                ("field", "open"),
                ("panel", &panel),
            ],
        );
        let error = h.err(Command::LayoutSplit {
            panel: navigator,
            open: Some("terminal".into()),
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "navigator_not_splittable"), ("field", "panel")],
        );
        let error = h.err(Command::LayoutSplit {
            panel: terminal,
            open: Some("Bad Name".into()),
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "plugin_invalid"), ("field", "open")],
        );

        // Две части центра около 465 точек помещаются, деление половины ещё нет, третье — нет.
        let mut target = terminal;
        let refused = loop {
            match h.run(Command::LayoutSplit {
                panel: target,
                open: Some("terminal".into()),
            }) {
                Ok(outcome) => {
                    let CommandValue::Panel { panel } = outcome.value else {
                        panic!("ожидалась панель");
                    };
                    target = panel;
                    assert!(h.center_tabs().len() <= 4, "деление не отказало");
                }
                Err(error) => break error,
            }
        };
        assert_refused(
            &refused,
            ErrorCode::InvalidArgument,
            &[("reason", "split_no_room"), ("field", "panel")],
        );
        let parts = refused.detail("parts").and_then(Value::as_array).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["need"], json!({"width": 200.0, "height": 120.0}));
        assert!(
            parts
                .iter()
                .any(|part| part["have"]["width"].as_f64() < Some(200.0))
        );
    }

    #[test]
    fn commands_that_need_room_refuse_without_window() {
        let mut h = H::new();
        h.viewport = None;
        let terminal = h.terminal;
        let error = h.err(Command::LayoutSplit {
            panel: terminal,
            open: Some("terminal".into()),
        });
        assert_refused(&error, ErrorCode::NoUi, &[]);
        let error = h.err(Command::LayoutResize {
            workbench: None,
            zone: Zone::Center,
            part_of: Some(terminal),
            size: 1000,
        });
        assert_refused(&error, ErrorCode::NoUi, &[]);
        // Остальные команды без окна выполняются.
        h.ok(Command::PanelMove {
            panel: terminal,
            zone: Zone::Bottom,
        });
        h.ok(Command::LayoutZoom {
            panel: Some(terminal),
            workbench: None,
            off: None,
        });
    }

    #[test]
    fn resize_refusals() {
        let mut h = H::new();
        let terminal = h.terminal;
        let navigator = h.navigator;
        let error = h.err(Command::LayoutResize {
            workbench: None,
            zone: Zone::Center,
            part_of: None,
            size: 300,
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "zone_not_resizable"), ("field", "zone")],
        );
        let error = h.err(Command::LayoutResize {
            workbench: None,
            zone: Zone::Right,
            part_of: None,
            size: 300,
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[
                ("reason", "zone_empty"),
                ("field", "zone"),
                ("zone", "right"),
            ],
        );
        let error = h.err(Command::LayoutResize {
            workbench: None,
            zone: Zone::Right,
            part_of: Some(terminal),
            size: 500,
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[
                ("reason", "panel_not_in_zone"),
                ("field", "part_of"),
                ("zone", "right"),
            ],
        );
        let error = h.err(Command::LayoutResize {
            workbench: None,
            zone: Zone::Left,
            part_of: Some(navigator),
            size: 500,
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "navigator_column"), ("field", "part_of")],
        );
        let error = h.err(Command::LayoutResize {
            workbench: None,
            zone: Zone::Center,
            part_of: Some(terminal),
            size: 70_000,
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "resize_out_of_range"), ("field", "size")],
        );
        assert_eq!(error.detail("min"), Some(&json!(1000)));
        assert_eq!(error.detail("max"), Some(&json!(1000)));

        h.ok(Command::PanelMove {
            panel: terminal,
            zone: Zone::Right,
        });
        let error = h.err(Command::LayoutResize {
            workbench: None,
            zone: Zone::Right,
            part_of: None,
            size: 10,
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "resize_out_of_range")],
        );
        let min = error.detail("min").and_then(Value::as_u64).unwrap();
        let max = error.detail("max").and_then(Value::as_u64).unwrap();
        assert!(min >= 202 && min < max, "{min}..={max}");
    }

    #[test]
    fn empty_targets_and_missing_workbench_refuse() {
        let mut h = H::new();
        let error = h.err(Command::LayoutEqual {
            workbench: None,
            zone: Zone::Bottom,
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[
                ("reason", "zone_empty"),
                ("field", "zone"),
                ("zone", "bottom"),
            ],
        );
        let error = h.err(Command::LayoutVisibility {
            workbench: None,
            target: Hideable::Plate,
            visible: false,
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[
                ("reason", "zone_empty"),
                ("field", "target"),
                ("target", "plate"),
            ],
        );

        h.state.window.route = Route::WorkspaceHome;
        let error = h.err(Command::LayoutOnly { workbench: None });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "no_open_workbench"), ("field", "workbench")],
        );
        // Явная рабочая область не требует открытой.
        let workbench = h.workbench;
        h.ok(Command::LayoutOnly {
            workbench: Some(workbench),
        });
        let missing = WorkbenchId::from_bytes([0xee; 16]);
        let error = h.err(Command::LayoutOnly {
            workbench: Some(missing),
        });
        assert_refused(&error, ErrorCode::NotFound, &[("entity", "workbench")]);
    }

    #[test]
    fn missing_foreign_and_hidden_panels_are_not_found() {
        let mut h = H::new();
        let terminal = h.terminal;
        let missing = PanelId::from_bytes([0xee; 16]);
        let not_found = |panel: PanelId| CoreError::not_found(Entity::Panel, panel);
        for command in [
            Command::PanelSelect { panel: missing },
            Command::PanelClose { panel: missing },
            Command::PanelMove {
                panel: missing,
                zone: Zone::Right,
            },
            Command::LayoutZoom {
                panel: Some(missing),
                workbench: None,
                off: None,
            },
            Command::PanelMovePart {
                panel: terminal,
                anchor: missing,
                edge: None,
            },
        ] {
            assert_eq!(h.err(command), not_found(missing));
        }

        // Якорь из другой рабочей области — как несуществующий.
        let workspace = h.state.workspaces_in_order()[0].id;
        let other = h.workbench_in(workspace, "/src/docs");
        let foreign = h.state.layouts[&other].focused().unwrap();
        assert_eq!(
            h.err(Command::PanelMovePart {
                panel: terminal,
                anchor: foreign,
                edge: Some(Edge::After),
            }),
            not_found(foreign)
        );
        assert_eq!(
            h.err(Command::LayoutResize {
                workbench: Some(other),
                zone: Zone::Center,
                part_of: Some(terminal),
                size: 1000,
            }),
            not_found(terminal)
        );
        let navigator = h.navigator;
        let error = h.err(Command::PanelMovePart {
            panel: terminal,
            anchor: navigator,
            edge: None,
        });
        assert_refused(
            &error,
            ErrorCode::InvalidArgument,
            &[("reason", "navigator_column"), ("field", "anchor")],
        );

        // Панель рабочей области убранного проекта не найдена.
        let project = h.state.workbenches[&other].project;
        h.state.projects.get_mut(&project).unwrap().removed_at = Some(UnixMillis(5));
        assert_eq!(
            h.err(Command::PanelSelect { panel: foreign }),
            not_found(foreign)
        );
    }

    #[test]
    fn conflicting_fields_are_refused_before_state() {
        let mut h = H::new();
        let terminal = h.terminal;
        let workbench = h.workbench;
        for (command, field) in [
            (
                Command::PanelFocus {
                    panel: Some(terminal),
                    workbench: None,
                    direction: Some(Direction::Up),
                },
                "direction",
            ),
            (
                Command::PanelFocus {
                    panel: None,
                    workbench: None,
                    direction: None,
                },
                "panel",
            ),
            (
                Command::PanelFocus {
                    panel: Some(terminal),
                    workbench: Some(workbench),
                    direction: None,
                },
                "workbench",
            ),
            (
                Command::LayoutZoom {
                    panel: None,
                    workbench: None,
                    off: Some(false),
                },
                "off",
            ),
            (
                Command::LayoutZoom {
                    panel: Some(terminal),
                    workbench: None,
                    off: Some(true),
                },
                "off",
            ),
            (
                Command::LayoutZoom {
                    panel: None,
                    workbench: None,
                    off: None,
                },
                "panel",
            ),
        ] {
            let error = h.err(command);
            assert_refused(
                &error,
                ErrorCode::InvalidArgument,
                &[("reason", "arguments_conflict"), ("field", field)],
            );
        }
    }
}
