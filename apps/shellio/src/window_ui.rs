//! Окно владельца на главном потоке: окно с заголовком, главное меню, зеркало состояния, монитор
//! цикла, единый обработчик [`UiCommand`] и ответы на запросы `ui` управляющего API.
//!
//! Состояние живёт только на главном потоке. Функции модуля вызываются из замыканий
//! [`MainThread::exec`]. Обратные вызовы AppKit (меню, кнопки заголовка, смена ячейки) не трогают
//! состояние синхронно: они ставят работу в очередь главного потока, поэтому повторного входа в
//! состояние изнутри вызова AppKit нет.

use std::cell::RefCell;
use std::mem::ManuallyDrop;

use shellio::launch::WindowMode;
use shellio::owner;
use shellio_appkit::{
    App, ChromeIntent, ChromeState, CountKind, Frame, InspectNode, LoopMonitor, MainMenu,
    MainThread, MainWindow, MenuAction, MenuEntry, MenuState, NodeState, Startup, TextFieldState,
    ThemeSelection, instance_counts, late_callbacks,
};
use shellio_control::{
    Accepted, Axis, AxisSource, ChromeView, ColorSpaceView, ComponentState, ComponentView,
    InspectView, InstanceKind, InstanceView, LateCallbackView, MaterialView, MenuItemView,
    MenuView, Rect, RunLoopView, ScreenView, Size, StartupView, StatsView, TextFieldView,
    UiRefusal, UiReply, UiRequest, WindowView,
};
use shellio_core::{AppearanceChoice, Delivery, Mirror, MirrorStep, UiCommand};
use shellio_theme::AppearanceChoice as ThemeChoice;
use shellio_theme::{Appearance, Contrast, Motion, Source};

thread_local! {
    /// Окно владельца; создаётся один раз после выбора роли и живёт до выхода процесса.
    /// `ManuallyDrop` не даёт слоту деструктора потока: `exit()` выполняет деструкторы главного
    /// потока, а выходы владельца деструкторов не выполняют (`owner.rs`), окно и монитор цикла
    /// при выходе не освобождаются.
    static WINDOW: RefCell<Option<ManuallyDrop<OwnerWindow>>> = const { RefCell::new(None) };
    /// Зеркало состояния ядра; ведёт его задача владельца доставками.
    static MIRROR: RefCell<Option<Mirror>> = const { RefCell::new(None) };
}

struct OwnerWindow {
    mode: WindowMode,
    window: MainWindow,
    menu: MainMenu,
    monitor: Option<LoopMonitor>,
    choice: AppearanceChoice,
    /// Поколение отрисовки: растёт с каждой видимой сменой состояния окна.
    generation: u64,
    /// Поколение первой проекции снимка зеркала: старт процесса считается до отрисовки с ним.
    first_snapshot: Option<u64>,
    /// Старт процесса, однажды наблюдённый.
    startup: Option<Startup>,
}

impl OwnerWindow {
    fn bump(&mut self) {
        self.generation += 1;
        if let Some(monitor) = &self.monitor {
            monitor.set_generation(self.generation);
        }
    }

    fn set_choice(&mut self, choice: AppearanceChoice) {
        self.choice = choice;
        let theme = theme_choice(choice);
        let overrides = self.window.overrides();
        self.window.set_overrides(shellio_theme::Overrides {
            choice: theme,
            ..overrides
        });
        self.window.chrome().set_choice(theme);
        self.menu.set_appearance(theme);
        self.bump();
    }

    /// Проекции зеркала: имя воркспейса в заголовке и системный заголовок окна.
    fn project(&mut self, projection: &Projection) {
        self.window.set_title(projection.title.as_deref());
        self.window
            .chrome()
            .set_workspace(projection.workspace.as_deref());
        self.bump();
        self.first_snapshot.get_or_insert(self.generation);
    }

    fn window_view(&self, app: &App, mirror_revision: Option<u64>) -> WindowView {
        let facts = self.window.facts(app);
        let selection = self.window.selection();
        let cell = selection.cell;
        let screen = facts.screen;
        WindowView {
            pid: std::process::id(),
            mode: wire_mode(self.mode),
            window_number: facts.number,
            frame: rect(facts.frame),
            backing_scale_factor: facts.backing_scale_factor,
            visible: facts.visible,
            key: facts.key,
            color_space: ColorSpaceView {
                name: facts.color_space,
                srgb: facts.color_space_srgb,
            },
            title: facts.title,
            appearance_choice: self.choice,
            lightness: Axis {
                value: match cell.cell.appearance {
                    Appearance::Light => shellio_control::Lightness::Light,
                    Appearance::Dark => shellio_control::Lightness::Dark,
                },
                source: source(cell.appearance_source),
            },
            contrast: Axis {
                value: match cell.cell.contrast {
                    Contrast::Normal => shellio_control::Contrast::Normal,
                    Contrast::Increased => shellio_control::Contrast::Increased,
                },
                source: source(cell.contrast_source),
            },
            motion: Axis {
                value: match selection.motion {
                    Motion::Normal => shellio_control::Motion::Normal,
                    Motion::Reduced => shellio_control::Motion::Reduced,
                },
                source: source(selection.motion_source),
            },
            screen: screen.and_then(|index| u32::try_from(index).ok()),
            maximum_frames_per_second: screen
                .and_then(|index| facts.screens.get(index))
                .map(|screen| screen.maximum_frames_per_second),
            screens: facts
                .screens
                .into_iter()
                .enumerate()
                .map(|(index, screen)| ScreenView {
                    index: u32::try_from(index).unwrap_or(u32::MAX),
                    name: screen.name,
                    frame: rect(screen.frame),
                    backing_scale_factor: screen.backing_scale_factor,
                    maximum_frames_per_second: screen.maximum_frames_per_second,
                })
                .collect(),
            mirror_revision,
            generation: self.generation,
        }
    }

    fn inspect_view(&self) -> InspectView {
        let inspection = self.window.inspect();
        let (width, height) = inspection.content;
        InspectView {
            window_number: self.window.number(),
            content: Size { width, height },
            generation: self.generation,
            components: inspection.nodes.into_iter().map(component).collect(),
            materials: inspection
                .materials
                .into_iter()
                .map(|material| MaterialView {
                    class: material.class,
                    visible: material.visible,
                    own: material.own,
                })
                .collect(),
        }
    }

    fn stats_view(&mut self) -> StatsView {
        if self.startup.is_none()
            && let (Some(monitor), Some(generation)) = (&self.monitor, self.first_snapshot)
        {
            self.startup = self.window.startup(monitor, generation);
        }
        StatsView {
            pid: std::process::id(),
            generation: self.generation,
            instances: instance_counts()
                .into_iter()
                .map(|count| InstanceView {
                    kind: match count.kind {
                        CountKind::Class => InstanceKind::Class,
                        CountKind::Owner => InstanceKind::Owner,
                    },
                    name: count.name.to_owned(),
                    created: count.created,
                    released: count.released,
                    live: count.live(),
                })
                .collect(),
            late_callbacks: late_callbacks()
                .into_iter()
                .map(|late| LateCallbackView {
                    site: late.site.to_owned(),
                    count: late.count,
                })
                .collect(),
            run_loop: self.monitor.as_ref().map(|monitor| {
                let stats = monitor.stats();
                let loops = stats.iterations;
                RunLoopView {
                    iterations: loops.iterations,
                    wakeups: loops.wakeups,
                    unpaired: loops.unpaired,
                    display_passes: loops.display_passes,
                    over_budget: loops.over_budget,
                    max_ms: loops.max_ms,
                    p95_ms: loops.p95_ms,
                    window: u64::try_from(loops.window).unwrap_or(u64::MAX),
                    draws: stats.draws,
                }
            }),
            startup: self.startup.map(|startup| StartupView {
                first_draw_ms: startup.first_draw_ms,
                key_ms: startup.key_ms,
                key_draw_ms: startup.key_draw_ms,
            }),
            animations: Vec::new(),
            row_views: Vec::new(),
            gallery_trees: Vec::new(),
        }
    }

    fn accepted(&self, command: UiCommand) -> UiReply {
        UiReply::Accepted(Accepted {
            command: command.name().to_owned(),
            pid: std::process::id(),
            generation: self.generation,
        })
    }
}

fn rect(frame: Frame) -> Rect {
    Rect {
        x: frame.x,
        y: frame.y,
        width: frame.width,
        height: frame.height,
    }
}

fn wire_mode(mode: WindowMode) -> shellio_control::WindowMode {
    match mode {
        WindowMode::Normal => shellio_control::WindowMode::Normal,
        WindowMode::Gallery => shellio_control::WindowMode::Gallery,
    }
}

fn source(source: Source) -> AxisSource {
    match source {
        Source::System => AxisSource::System,
        Source::Choice => AxisSource::UiAppearance,
        Source::Gallery => AxisSource::Gallery,
    }
}

fn chrome_view(state: ChromeState) -> ChromeView {
    ChromeView {
        workspace_text: state.workspace_text().to_owned(),
        workspace: state.workspace,
        appearance_choice: core_choice(state.choice),
        workspace_menu: state.workspace_menu,
        zones: state.zones,
    }
}

fn text_field_view(state: TextFieldState, editing: bool) -> TextFieldView {
    TextFieldView {
        value: state.value,
        placeholder: state.placeholder,
        editable: state.editable,
        editing,
    }
}

fn menu_items(entries: Vec<MenuEntry>) -> Vec<MenuItemView> {
    entries
        .into_iter()
        .map(|entry| match entry {
            MenuEntry::Item(item) => MenuItemView {
                id: item.id,
                separator: false,
                title: item.title,
                key: item.key.as_ref().map(shellio_appkit::MenuKey::chord),
                enabled: item.enabled,
                checked: item.checked,
                submenu: item.submenu.map(menu_items),
            },
            MenuEntry::Separator { id } => MenuItemView {
                id,
                separator: true,
                title: String::new(),
                key: None,
                enabled: false,
                checked: false,
                submenu: None,
            },
        })
        .collect()
}

fn menu_view(state: MenuState) -> MenuView {
    MenuView {
        open: state.open,
        highlighted: state.highlighted,
        items: menu_items(state.entries),
    }
}

fn component_state(state: NodeState) -> ComponentState {
    match state {
        NodeState::Chrome(state) => ComponentState::Chrome(chrome_view(state)),
        NodeState::TextField { state, editing } => {
            ComponentState::TextField(text_field_view(state, editing))
        }
        NodeState::Menu(state) => ComponentState::Menu(menu_view(state)),
    }
}

fn component(node: InspectNode) -> ComponentView {
    let facts = node.facts;
    ComponentView {
        id: node.id.to_owned(),
        class: node.class.to_owned(),
        object: node.object,
        parent: node.parent.map(str::to_owned),
        frame: rect(facts.frame),
        visible: facts.visible,
        first_responder: facts.first_responder,
        enabled: facts.enabled,
        role: facts.role,
        label: facts.label,
        help: facts.help,
        selected: facts.selected,
        expanded: facts.expanded,
        animation_keys: facts.animation_keys,
        state: node.state.map(component_state),
    }
}

/// Проекции зеркала для окна.
struct Projection {
    workspace: Option<String>,
    title: Option<String>,
}

impl Projection {
    fn of(mirror: &Mirror) -> Projection {
        Projection {
            workspace: mirror.workspace_name().map(|name| name.as_str().to_owned()),
            title: mirror.window_title(),
        }
    }
}

/// Работа с окном владельца, если оно есть. Повторный вход не паникует: работа пропускается со
/// строкой в stderr.
fn with_window<R>(work: impl FnOnce(&mut OwnerWindow) -> R) -> Option<R> {
    WINDOW.with(|slot| match slot.try_borrow_mut() {
        Ok(mut window) => window.as_deref_mut().map(work),
        Err(_) => {
            owner::say_err("shellio: окно владельца занято, повторный вход пропущен");
            None
        }
    })
}

/// Отложить работу на следующий оборот очереди главного потока.
fn defer(work: impl FnOnce(&App) + Send + 'static) {
    MainThread::new().exec(work);
}

/// Создаёт окно владельца в режиме `mode`, не показывая его, ставит главное меню и монитор
/// цикла. Повторный вызов ничего не создаёт. Возвращает `windowNumber`.
pub fn create(app: &App, mode: WindowMode) -> i64 {
    if let Some(number) = with_window(|owner| owner.window.number()) {
        return number;
    }
    let window = MainWindow::new(
        app,
        |_: ThemeSelection| defer(|_| on_theme()),
        |intent| defer(move |app| on_intent(app, intent)),
    );
    let menu = MainMenu::install(app, |action| {
        defer(move |app| handle(app, command_of(action)));
    });
    let monitor = match LoopMonitor::install(app) {
        Ok(monitor) => Some(monitor),
        Err(error) => {
            owner::say_err(format_args!("shellio: монитор цикла не поставлен: {error}"));
            None
        }
    };
    let number = window.number();
    let projection = MIRROR.with(|mirror| mirror.borrow().as_ref().map(Projection::of));
    WINDOW.with(|slot| {
        *slot.borrow_mut() = Some(ManuallyDrop::new(OwnerWindow {
            mode,
            window,
            menu,
            monitor,
            choice: AppearanceChoice::System,
            generation: 0,
            first_snapshot: None,
            startup: None,
        }));
    });
    if let Some(projection) = projection {
        with_window(|owner| owner.project(&projection));
    }
    number
}

/// Показывает окно владельца (`makeKeyAndOrderFront`). `false` — окна нет: процесс не владелец.
pub fn show(_app: &App) -> bool {
    with_window(|owner| owner.window.show()).is_some()
}

/// Приложение открыли снова: окно показывается, владелец просит передний план. Без окна ничего не
/// делается.
pub fn reopen(app: &App) {
    if show(app) {
        app.activate();
    }
}

/// Применить доставку к зеркалу. Применённая доставка обновляет проекции окна.
pub fn deliver(_app: &App, delivery: Delivery) -> MirrorStep {
    let (step, projection) = MIRROR.with(|slot| {
        let mut mirror = slot.borrow_mut();
        let step = match (&mut *mirror, delivery) {
            (Some(mirror), delivery) => mirror.apply(delivery),
            (None, Delivery::Snapshot(snapshot)) => {
                *mirror = Some(Mirror::from_snapshot(snapshot));
                MirrorStep::Applied
            }
            (None, Delivery::Event(_) | Delivery::Reset) => MirrorStep::NeedSnapshot,
        };
        let projection = match step {
            MirrorStep::Applied => mirror.as_ref().map(Projection::of),
            MirrorStep::Ignored | MirrorStep::NeedSnapshot => None,
        };
        (step, projection)
    });
    if let Some(projection) = projection {
        with_window(|owner| owner.project(&projection));
    }
    step
}

/// Запрос `ui` управляющего API. Без окна — `no_ui`. Действие выполняет [`handle`]; `app.quit`
/// отвечает квитанцией до `terminate:`, который [`App::terminate`] ставит таймером цикла.
pub fn request(app: &App, request: UiRequest) -> Result<UiReply, UiRefusal> {
    let reply = match request {
        UiRequest::Window => {
            let revision = MIRROR.with(|mirror| mirror.borrow().as_ref().map(|m| m.revision().0));
            with_window(|owner| UiReply::Window(Box::new(owner.window_view(app, revision))))
        }
        UiRequest::Inspect => with_window(|owner| UiReply::Inspect(Box::new(owner.inspect_view()))),
        UiRequest::Stats => with_window(|owner| UiReply::Stats(Box::new(owner.stats_view()))),
        UiRequest::Command(command) => {
            let owner = with_window(|_| ());
            owner.and_then(|()| {
                handle(app, command);
                with_window(|owner| owner.accepted(command))
            })
        }
    };
    reply.ok_or(UiRefusal::NoUi)
}

/// Единый обработчик действий интерфейса: меню, кнопка заголовка и CLI.
pub fn handle(app: &App, command: UiCommand) {
    match command {
        UiCommand::WindowClose => {
            with_window(|owner| owner.window.hide());
        }
        // Вне состояния окна: `terminate:` спрашивает делегата приложения на следующем обороте цикла.
        UiCommand::AppQuit => app.terminate(),
        UiCommand::AppearanceSet(choice) => {
            with_window(|owner| owner.set_choice(choice));
        }
    }
}

fn on_theme() {
    with_window(OwnerWindow::bump);
}

fn on_intent(app: &App, intent: ChromeIntent) {
    match intent {
        ChromeIntent::AppearanceCycle => {
            if let Some(choice) = with_window(|owner| owner.choice) {
                handle(app, UiCommand::AppearanceSet(choice.next()));
            }
        }
        // Меню воркспейса появится вместе с источником в P1.7.
        ChromeIntent::WorkspaceMenu => {}
    }
}

fn command_of(action: MenuAction) -> UiCommand {
    match action {
        MenuAction::Quit => UiCommand::AppQuit,
        MenuAction::CloseWindow => UiCommand::WindowClose,
        MenuAction::Appearance(choice) => UiCommand::AppearanceSet(core_choice(choice)),
    }
}

fn theme_choice(choice: AppearanceChoice) -> ThemeChoice {
    match choice {
        AppearanceChoice::System => ThemeChoice::System,
        AppearanceChoice::Light => ThemeChoice::Light,
        AppearanceChoice::Dark => ThemeChoice::Dark,
    }
}

fn core_choice(choice: ThemeChoice) -> AppearanceChoice {
    match choice {
        ThemeChoice::System => AppearanceChoice::System,
        ThemeChoice::Light => AppearanceChoice::Light,
        ThemeChoice::Dark => AppearanceChoice::Dark,
    }
}

#[cfg(test)]
mod tests {
    use shellio_appkit::{MenuItem, MenuKey};

    use super::*;

    #[test]
    fn component_states_reach_the_wire() {
        let field = component_state(NodeState::TextField {
            state: TextFieldState {
                value: "shellio".into(),
                placeholder: "Поиск".into(),
                editable: true,
            },
            editing: true,
        });
        assert_eq!(
            field,
            ComponentState::TextField(TextFieldView {
                value: "shellio".into(),
                placeholder: "Поиск".into(),
                editable: true,
                editing: true,
            })
        );
        let menu = component_state(NodeState::Menu(MenuState {
            open: true,
            highlighted: Some("copy".into()),
            entries: vec![
                MenuEntry::Item(MenuItem {
                    key: Some(MenuKey::command("c")),
                    checked: true,
                    submenu: Some(vec![MenuEntry::Item(MenuItem::new("inner", "Внутри"))]),
                    ..MenuItem::new("copy", "Копировать")
                }),
                MenuEntry::Separator { id: "sep".into() },
            ],
        }));
        let ComponentState::Menu(menu) = menu else {
            panic!("не меню: {menu:?}");
        };
        assert!(menu.open);
        assert_eq!(menu.highlighted.as_deref(), Some("copy"));
        assert_eq!(menu.items.len(), 2);
        let copy = &menu.items[0];
        assert_eq!(
            (
                copy.id.as_str(),
                copy.key.as_deref(),
                copy.checked,
                copy.separator
            ),
            ("copy", Some("cmd+c"), true, false)
        );
        assert_eq!(copy.submenu.as_ref().map(Vec::len), Some(1));
        assert!(menu.items[1].separator);
        assert_eq!(menu.items[1].id, "sep");
    }

    #[test]
    fn menu_actions_map_to_ui_commands() {
        assert_eq!(command_of(MenuAction::Quit), UiCommand::AppQuit);
        assert_eq!(command_of(MenuAction::CloseWindow), UiCommand::WindowClose);
        for choice in AppearanceChoice::ALL {
            assert_eq!(core_choice(theme_choice(choice)), choice);
            assert_eq!(
                command_of(MenuAction::Appearance(theme_choice(choice))),
                UiCommand::AppearanceSet(choice)
            );
        }
    }
}
