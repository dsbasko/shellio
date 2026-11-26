//! История переходов воркспейсов (ADR-0010, «История переходов»): правило точек по состояниям
//! зеркала и выбор цели шага. Состояние живёт на главном потоке и в базу не пишется.

use std::collections::BTreeMap;

use shellio_core::{HistoryStep, PanelId, State, WorkbenchId, WorkspaceId, open_workbench};
use shellio_keymap::{JumpList, Mode};

/// Точка истории: рабочая область, панель и режим, в котором панель была оставлена. Точки равны по
/// рабочей области и панели: смена режима панели новым местом её не делает.
#[derive(Clone, Copy, Debug)]
pub struct JumpPoint {
    pub workbench: WorkbenchId,
    pub panel: PanelId,
    pub mode: Mode,
}

impl PartialEq for JumpPoint {
    fn eq(&self, other: &Self) -> bool {
        self.workbench == other.workbench && self.panel == other.panel
    }
}

impl Eq for JumpPoint {}

/// Сделанный шаг: цель и место курсора до шага и после него. По нему шаг, на который ядро ответило
/// отказом, возвращает курсор.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Jump {
    pub point: JumpPoint,
    workspace: WorkspaceId,
    from: usize,
    to: usize,
}

/// Истории воркспейсов и место последнего наблюдения зеркала.
#[derive(Debug, Default)]
pub struct History {
    lists: BTreeMap<WorkspaceId, JumpList<JumpPoint>>,
    /// Активный воркспейс последнего наблюдения. Хранится отдельно от места: экран воркспейса без
    /// открытой рабочей области места не даёт, но воркспейс не меняет.
    workspace: Option<WorkspaceId>,
    seen: Option<(WorkbenchId, PanelId)>,
}

/// Панель с фокусом открытой рабочей области.
fn location(state: &State) -> Option<(WorkbenchId, PanelId)> {
    let workbench = open_workbench(state)?;
    let panel = state.layouts.get(&workbench)?.focused()?;
    Some((workbench, panel))
}

/// Точка, к которой можно перейти: панель не закрыта, её рабочая область живая и лежит в
/// воркспейсе истории.
fn alive(state: &State, workspace: WorkspaceId, point: &JumpPoint) -> bool {
    state
        .panels
        .get(&point.panel)
        .is_some_and(|panel| panel.workbench == point.workbench)
        && state.live_workbench(point.workbench).is_some()
        && state.workbench_workspace(point.workbench) == Some(workspace)
}

impl History {
    /// Состояние зеркала после применённой доставки. Точку добавляет только смена панели с фокусом
    /// относительно прошлого наблюдения. Не добавляют: фоновое изменение, запасной фокус после
    /// закрытия панели, смена воркспейса и приход к точке под курсором, то есть шаг по самой
    /// истории. Первая панель воркспейса засевает его историю как место. `mode_of` — режим панели в
    /// момент наблюдения: с ним записывается оставленная точка.
    pub fn observe(&mut self, state: &State, mode_of: impl Fn(PanelId) -> Mode) {
        self.lists
            .retain(|workspace, _| state.workspaces.contains_key(workspace));
        let workspace = state.window.active_workspace;
        let here = location(state);
        let switched = std::mem::replace(&mut self.workspace, workspace) != workspace;
        let before = std::mem::replace(&mut self.seen, here);
        let (Some(workspace), Some((workbench, panel))) = (workspace, here) else {
            return;
        };
        let list = self.lists.entry(workspace).or_default();
        let arrived = JumpPoint {
            workbench,
            panel,
            mode: mode_of(panel),
        };
        if list.cursor().is_none() {
            list.visit(arrived);
            return;
        }
        if switched || before == here {
            return;
        }
        if let Some((left, source)) = before {
            if !state.panels.contains_key(&source) {
                return;
            }
            if let Some(at) = list.cursor()
                && let Some(current) = list.point_mut(at)
                && current.workbench == left
                && current.panel == source
            {
                current.mode = mode_of(source);
            }
        }
        list.visit(arrived);
    }

    /// Шаг по истории активного воркспейса: курсор встаёт на ближайшую точку, к которой можно
    /// перейти, и она возвращается. Закрытые панели, панели вне живых рабочих областей и точки
    /// панели, на которой фокус уже стоит, пропускаются. `None` — идти некуда, курсор на месте.
    pub fn step(
        &mut self,
        state: &State,
        step: HistoryStep,
        mode_of: impl Fn(PanelId) -> Mode,
    ) -> Option<JumpPoint> {
        let workspace = state.window.active_workspace?;
        let list = self.lists.get_mut(&workspace)?;
        let before = list.cursor();
        let here = location(state);
        // Точка места, где фокус уже стоит, шагом не считается: переход к ней ничего не меняет.
        let open = |point: &JumpPoint| {
            alive(state, workspace, point) && here != Some((point.workbench, point.panel))
        };
        let target = match step {
            HistoryStep::Back => list.back(open).copied(),
            HistoryStep::Forward => list.forward(open).copied(),
        }?;
        // Оставленная точка получает режим своей панели, если фокус стоял на ней.
        if let Some(at) = before
            && let Some((workbench, panel)) = here
            && let Some(left) = list.point_mut(at)
            && left.workbench == workbench
            && left.panel == panel
        {
            left.mode = mode_of(panel);
        }
        Some(target)
    }

    /// [`History::step`] с местом курсора до шага: исполнитель шага возвращает курсор по отказу
    /// ядра через [`History::undo`].
    pub fn jump(
        &mut self,
        state: &State,
        step: HistoryStep,
        mode_of: impl Fn(PanelId) -> Mode,
    ) -> Option<Jump> {
        let workspace = state.window.active_workspace?;
        let from = self.lists.get(&workspace)?.cursor()?;
        let point = self.step(state, step, mode_of)?;
        let to = self.lists.get(&workspace)?.cursor()?;
        Some(Jump {
            point,
            workspace,
            from,
            to,
        })
    }

    /// Шаг не состоялся: курсор возвращается на место до шага. Если с тех пор курсор ушёл с цели
    /// шага (новый переход или другой шаг), история не трогается.
    pub fn undo(&mut self, jump: &Jump) {
        if let Some(list) = self.lists.get_mut(&jump.workspace)
            && list.cursor() == Some(jump.to)
            && list.points().get(jump.to) == Some(&jump.point)
        {
            list.seek(jump.from);
        }
    }

    /// История активного воркспейса для чтения `ui.history`.
    pub fn of_active(&self, state: &State) -> Option<(WorkspaceId, &JumpList<JumpPoint>)> {
        let workspace = state.window.active_workspace?;
        Some((workspace, self.lists.get(&workspace)?))
    }
}

#[cfg(test)]
mod tests {
    use shellio_core::{
        Panel, PluginId, Project, ProjectId, Route, TERMINAL_PLUGIN, Title, UnixMillis, Workbench,
        Workspace,
    };
    use shellio_keymap::JUMP_DEPTH;
    use shellio_layout::Layout;

    use super::*;
    use crate::scene::tests::{Fixture, add_panel, fixture, id, layout_mut};

    const NAV: fn(PanelId) -> Mode = |_| Mode::Navigation;

    fn focus(fixture: &mut Fixture, panel: PanelId) {
        let next = layout_mut(fixture).focus_panel(panel).unwrap();
        *layout_mut(fixture) = next;
    }

    /// Новая панель в раскладке открытой рабочей области.
    fn open_panel(fixture: &mut Fixture, n: u8) -> PanelId {
        let panel = add_panel(fixture, n);
        let next = layout_mut(fixture).open(panel, None).unwrap();
        *layout_mut(fixture) = next;
        panel
    }

    fn close(fixture: &mut Fixture, panel: PanelId) {
        let next = layout_mut(fixture).close(panel).unwrap();
        *layout_mut(fixture) = next;
        fixture.state.panels.remove(&panel);
    }

    /// Вторая рабочая область того же проекта со своими навигатором и терминалом.
    fn second_workbench(fixture: &mut Fixture) -> (WorkbenchId, PanelId) {
        let workbench: WorkbenchId = id(4);
        let project = fixture.state.workbenches[&fixture.workbench].project;
        fixture.state.workbenches.insert(
            workbench,
            Workbench {
                id: workbench,
                project,
                title: Title::new("review").unwrap(),
                position: 1,
            },
        );
        let navigator: PanelId = id(20);
        let terminal: PanelId = id(21);
        for panel in [navigator, terminal] {
            fixture.state.panels.insert(
                panel,
                Panel {
                    id: panel,
                    workbench,
                    plugin: PluginId::new(TERMINAL_PLUGIN).unwrap(),
                    title: Title::new("Терминал").unwrap(),
                    created_at: UnixMillis(0),
                },
            );
        }
        fixture
            .state
            .layouts
            .insert(workbench, Layout::default_for(navigator, terminal));
        (workbench, terminal)
    }

    /// Второй воркспейс с проектом, рабочей областью и терминалом.
    fn second_workspace(fixture: &mut Fixture) -> (WorkspaceId, WorkbenchId, PanelId) {
        let workspace: WorkspaceId = id(5);
        let project: ProjectId = id(6);
        let workbench: WorkbenchId = id(7);
        fixture.state.workspaces.insert(
            workspace,
            Workspace {
                id: workspace,
                title: Title::new("Дом").unwrap(),
                position: 1,
                last_workbench: None,
                deleted_at: None,
            },
        );
        fixture.state.projects.insert(
            project,
            Project {
                id: project,
                workspace,
                title: Title::new("notes").unwrap(),
                list_position: Some(0),
                removed_at: None,
                last_opened_at: None,
            },
        );
        fixture.state.workbenches.insert(
            workbench,
            Workbench {
                id: workbench,
                project,
                title: Title::new("main").unwrap(),
                position: 0,
            },
        );
        let navigator: PanelId = id(30);
        let terminal: PanelId = id(31);
        for panel in [navigator, terminal] {
            fixture.state.panels.insert(
                panel,
                Panel {
                    id: panel,
                    workbench,
                    plugin: PluginId::new(TERMINAL_PLUGIN).unwrap(),
                    title: Title::new("Терминал").unwrap(),
                    created_at: UnixMillis(0),
                },
            );
        }
        fixture
            .state
            .layouts
            .insert(workbench, Layout::default_for(navigator, terminal));
        (workspace, workbench, terminal)
    }

    fn route(fixture: &mut Fixture, workspace: WorkspaceId, workbench: WorkbenchId) {
        fixture.state.window.active_workspace = Some(workspace);
        fixture.state.window.route = Route::Workbench(workbench);
    }

    fn panels(history: &History, state: &State) -> Vec<PanelId> {
        history
            .of_active(state)
            .map(|(_, list)| list.points().iter().map(|point| point.panel).collect())
            .unwrap_or_default()
    }

    fn cursor(history: &History, state: &State) -> Option<usize> {
        history.of_active(state).and_then(|(_, list)| list.cursor())
    }

    #[test]
    fn focus_changes_add_points_and_background_changes_do_not() {
        let mut fixture = fixture();
        let (a, b) = (fixture.terminal, fixture.navigator);
        let mut history = History::default();
        history.observe(&fixture.state, NAV);
        assert_eq!(
            panels(&history, &fixture.state),
            [a],
            "первая панель — место"
        );

        // Фоновые изменения: новая панель без фокуса, раскладка закрытой рабочей области.
        add_panel(&mut fixture, 12);
        let (other, _) = second_workbench(&mut fixture);
        history.observe(&fixture.state, NAV);
        let navigator = fixture.state.layouts[&other].navigator.unwrap();
        let moved = fixture.state.layouts[&other]
            .focus_panel(navigator)
            .unwrap();
        fixture.state.layouts.insert(other, moved);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a]);

        focus(&mut fixture, b);
        history.observe(&fixture.state, NAV);
        history.observe(&fixture.state, NAV);
        focus(&mut fixture, a);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a, b, a]);
        assert_eq!(cursor(&history, &fixture.state), Some(2));
    }

    #[test]
    fn a_refused_step_returns_the_cursor_unless_it_moved_on() {
        let mut fixture = fixture();
        let (a, b) = (fixture.terminal, fixture.navigator);
        let c = open_panel(&mut fixture, 12);
        let mut history = History::default();
        for panel in [a, b, a] {
            focus(&mut fixture, panel);
            history.observe(&fixture.state, NAV);
        }

        // Отказ ядра: фокус остался на месте, курсор возвращается к нему.
        let jump = history
            .jump(&fixture.state, HistoryStep::Back, NAV)
            .unwrap();
        assert_eq!(jump.point.panel, b);
        assert_eq!(cursor(&history, &fixture.state), Some(1));
        history.undo(&jump);
        assert_eq!(cursor(&history, &fixture.state), Some(2));
        assert_eq!(panels(&history, &fixture.state), [a, b, a]);
        history.undo(&jump);
        assert_eq!(cursor(&history, &fixture.state), Some(2), "повтор отката");

        // Курсор ушёл с цели шага: новый переход отсёк ветку, запоздалый отказ её не трогает.
        let jump = history
            .jump(&fixture.state, HistoryStep::Back, NAV)
            .unwrap();
        focus(&mut fixture, c);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a, b, c]);
        history.undo(&jump);
        assert_eq!(cursor(&history, &fixture.state), Some(2));

        // Идти некуда: шага нет и отката нет.
        fixture.state.window.active_workspace = None;
        assert_eq!(history.jump(&fixture.state, HistoryStep::Back, NAV), None);
    }

    #[test]
    fn steps_walk_the_points_and_a_new_jump_cuts_the_branch() {
        let mut fixture = fixture();
        let (a, b) = (fixture.terminal, fixture.navigator);
        let c = open_panel(&mut fixture, 12);
        focus(&mut fixture, a);
        let mut history = History::default();
        for panel in [a, b, a] {
            focus(&mut fixture, panel);
            history.observe(&fixture.state, NAV);
        }

        let back = history
            .step(&fixture.state, HistoryStep::Back, NAV)
            .unwrap();
        assert_eq!(back.panel, b);
        // Фоновая доставка приходит раньше доставки фокуса: точки нет, ветка вперёд цела.
        history.observe(&fixture.state, NAV);
        focus(&mut fixture, b);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a, b, a]);
        assert_eq!(cursor(&history, &fixture.state), Some(1));

        let forward = history
            .step(&fixture.state, HistoryStep::Forward, NAV)
            .unwrap();
        assert_eq!(forward.panel, a);
        focus(&mut fixture, a);
        history.observe(&fixture.state, NAV);
        assert_eq!(cursor(&history, &fixture.state), Some(2));
        assert_eq!(
            history.step(&fixture.state, HistoryStep::Forward, NAV),
            None
        );

        history
            .step(&fixture.state, HistoryStep::Back, NAV)
            .unwrap();
        focus(&mut fixture, b);
        history.observe(&fixture.state, NAV);
        focus(&mut fixture, c);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a, b, c]);
        assert_eq!(
            history.step(&fixture.state, HistoryStep::Forward, NAV),
            None
        );
    }

    #[test]
    fn closing_adds_no_point_and_closed_targets_are_skipped() {
        let mut fixture = fixture();
        let (a, b) = (fixture.terminal, fixture.navigator);
        let c = open_panel(&mut fixture, 12);
        let mut history = History::default();
        for panel in [a, b, c] {
            focus(&mut fixture, panel);
            history.observe(&fixture.state, NAV);
        }

        // Закрытие панели с фокусом: запасной фокус точки не даёт.
        close(&mut fixture, c);
        let fallback = fixture.state.layouts[&fixture.workbench].focused().unwrap();
        assert_ne!(fallback, c);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a, b, c]);
        assert_eq!(cursor(&history, &fixture.state), Some(2));

        // Закрытая цель пропускается в обе стороны, как и точка места, где фокус уже стоит.
        let other = if fallback == a { b } else { a };
        focus(&mut fixture, other);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a, b, c, other]);
        let back = history
            .step(&fixture.state, HistoryStep::Back, NAV)
            .unwrap();
        assert_eq!(back.panel, fallback, "закрытая панель пропущена");
        focus(&mut fixture, fallback);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a, b, c, other]);
        let forward = history
            .step(&fixture.state, HistoryStep::Forward, NAV)
            .unwrap();
        assert_eq!(forward.panel, other);
    }

    #[test]
    fn another_workbench_is_a_point_and_a_hidden_one_is_skipped() {
        let mut fixture = fixture();
        let (first, a) = (fixture.workbench, fixture.terminal);
        let workspace = fixture.state.window.active_workspace.unwrap();
        let (second, b) = second_workbench(&mut fixture);
        let mut history = History::default();
        history.observe(&fixture.state, NAV);

        // Экран воркспейса места не даёт и воркспейс не меняет.
        fixture.state.window.route = Route::WorkspaceHome;
        history.observe(&fixture.state, NAV);
        route(&mut fixture, workspace, second);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a, b]);

        let back = history
            .step(&fixture.state, HistoryStep::Back, NAV)
            .unwrap();
        assert_eq!((back.workbench, back.panel), (first, a));
        route(&mut fixture, workspace, first);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a, b]);
        assert_eq!(cursor(&history, &fixture.state), Some(0));

        // Убранный проект: его рабочие области не живые, точка пропускается.
        let (_, home, c) = {
            let (other, bench, panel) = second_workspace(&mut fixture);
            (other, bench, panel)
        };
        let project = fixture.state.workbenches[&second].project;
        let spare: ProjectId = id(8);
        let mut moved = fixture.state.projects[&project].clone();
        moved.id = spare;
        moved.removed_at = Some(UnixMillis(1));
        fixture.state.projects.insert(spare, moved);
        fixture.state.workbenches.get_mut(&second).unwrap().project = spare;
        assert_eq!(
            history.step(&fixture.state, HistoryStep::Forward, NAV),
            None
        );
        assert_eq!(cursor(&history, &fixture.state), Some(0));

        // Смена воркспейса точки не добавляет: у каждого своя история.
        let other = fixture.state.workbench_workspace(home).unwrap();
        route(&mut fixture, other, home);
        history.observe(&fixture.state, NAV);
        assert_eq!(
            panels(&history, &fixture.state),
            [c],
            "место нового воркспейса"
        );
        route(&mut fixture, workspace, first);
        history.observe(&fixture.state, NAV);
        assert_eq!(panels(&history, &fixture.state), [a, b]);
        assert_eq!(cursor(&history, &fixture.state), Some(0));
        assert_eq!(history.step(&fixture.state, HistoryStep::Back, NAV), None);
    }

    #[test]
    fn depth_is_one_hundred_points_per_workspace() {
        let mut fixture = fixture();
        let (a, b) = (fixture.terminal, fixture.navigator);
        let mut history = History::default();
        for turn in 0..JUMP_DEPTH + 7 {
            focus(&mut fixture, if turn % 2 == 0 { a } else { b });
            history.observe(&fixture.state, NAV);
        }
        assert_eq!(panels(&history, &fixture.state).len(), JUMP_DEPTH);
        assert_eq!(cursor(&history, &fixture.state), Some(JUMP_DEPTH - 1));
    }

    #[test]
    fn a_left_point_keeps_the_mode_of_its_panel() {
        let mut fixture = fixture();
        let (a, b) = (fixture.terminal, fixture.navigator);
        let mut history = History::default();
        history.observe(&fixture.state, NAV);
        focus(&mut fixture, b);
        // Панель `a` оставлена в `Input`: режим в точке есть, новым местом он её не делает.
        history.observe(&fixture.state, |panel| {
            if panel == a {
                Mode::Input
            } else {
                Mode::Navigation
            }
        });
        let (_, list) = history.of_active(&fixture.state).unwrap();
        assert_eq!(list.points()[0].mode, Mode::Input);
        assert_eq!(list.points()[1].mode, Mode::Navigation);

        let back = history
            .step(&fixture.state, HistoryStep::Back, |_| Mode::Input)
            .unwrap();
        assert_eq!((back.panel, back.mode), (a, Mode::Input));
        focus(&mut fixture, a);
        history.observe(&fixture.state, NAV);
        let (_, list) = history.of_active(&fixture.state).unwrap();
        assert_eq!(
            list.points().len(),
            2,
            "возврат в другом режиме точки не добавил"
        );
        assert_eq!(
            list.points()[1].mode,
            Mode::Input,
            "оставленная шагом точка"
        );
    }
}
