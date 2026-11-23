//! Окно владельца на главном потоке: окно с заголовком, главное меню, зеркало состояния, монитор
//! цикла, единый обработчик [`UiCommand`], галерея в режиме `--gallery` и ответы на запросы `ui`
//! управляющего API.
//!
//! Поколение отрисовки и всё, что живёт до выхода процесса (меню, монитор, выбор оформления),
//! лежит рядом с окном, а не в нём: `gallery.window.recreate` заменяет окно и дерево галереи, а
//! поколение продолжает расти.
//!
//! В обычном режиме содержимое окна — раскладка открытой рабочей области ([`LayoutUi`]): её
//! получают доставки зеркала и изменения размера окна. Размер содержимого уходит ядру внутренним
//! входом [`CoreHandle::set_viewport`]: первый — до публикации сокета, дальше — с объединением до
//! конца оборота цикла и без повторов.
//!
//! Состояние живёт только на главном потоке. Функции модуля вызываются из замыканий
//! [`MainThread::exec`]. Обратные вызовы AppKit (меню, кнопки заголовка, смена ячейки) не трогают
//! состояние синхронно: они ставят работу в очередь главного потока, поэтому повторного входа в
//! состояние изнутри вызова AppKit нет.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::mem::ManuallyDrop;

use shellio::gallery::{self, Changed, Effect, Generation, Intent};
use shellio::launch::WindowMode;
use shellio::owner;
use shellio::scene::{self, SplitId, ViewportGate, ZoneToggle};
use shellio_appkit::{
    App, ChromeIntent, ChromeState, CountKind, Frame, InspectNode, LoopMonitor, MainMenu,
    MainThread, MainWindow, MenuAction, MenuEntry, MenuState, NodeState, Regime, SplitAxis,
    SplitDrag, Startup, StatusLine, TabStatus, TabStripFacts, TextFieldState, ThemeSelection,
    Transition, TransitionPath, TreeListFacts, VirtualListFacts, ZoneButton, ZoneSide,
    instance_counts, late_callbacks,
};
use shellio_control::{
    Accepted, AnimationPath, AnimationRecord, Axis, AxisSource, ChromeView, ColorSpaceView,
    ComponentState, ComponentView, GalleryCommand, GalleryView, InspectView, InstanceKind,
    InstanceView, LateCallbackView, ListRowView, MaterialView, MenuItemView, MenuView, Rect,
    RunLoopView, ScreenView, Size, StartupView, StatsView, StripButtonView, TabAxis, TabItemView,
    TabRegime, TabStatusView, TabStripView, TabZone, TextFieldView, TreeListView, TreeRowView,
    UiRefusal, UiReply, UiRequest, VirtualListView, WindowView,
};
use shellio_core::{
    AppearanceChoice, Command, CoreError, CoreHandle, Delivery, Mirror, MirrorStep, UiCommand,
};
use shellio_theme::AppearanceChoice as ThemeChoice;
use shellio_theme::{Appearance, Contrast, Motion, Overrides, Source};

use crate::gallery_ui::GalleryUi;
use crate::layout_ui::{LayoutUi, PanelAction};

thread_local! {
    /// Окно владельца; создаётся один раз после выбора роли и живёт до выхода процесса.
    /// `ManuallyDrop` не даёт слоту деструктора потока: `exit()` выполняет деструкторы главного
    /// потока, а выходы владельца деструкторов не выполняют (`owner.rs`), окно и монитор цикла
    /// при выходе не освобождаются.
    static WINDOW: RefCell<Option<ManuallyDrop<OwnerWindow>>> = const { RefCell::new(None) };
    /// Зеркало состояния ядра; ведёт его задача владельца доставками.
    static MIRROR: RefCell<Option<Mirror>> = const { RefCell::new(None) };
    /// Ручка ядра: размер окна и команды раскладки из интерфейса. Ставится до публикации сокета.
    static CORE: RefCell<Option<CoreLink>> = const { RefCell::new(None) };
}

/// Ядро для главного потока: вход размера окна и команды без ожидания, исход — задачей runtime.
struct CoreLink {
    core: CoreHandle,
    runtime: tokio::runtime::Handle,
}

impl CoreLink {
    /// Подать команду без ожидания. Отказ разбирает [`on_refused`] на главном потоке.
    fn send(&self, command: Command) {
        let sent = command.clone();
        match self.core.try_request(command) {
            Ok(receipt) => {
                self.runtime.spawn(async move {
                    match receipt.await {
                        // Размер без изменений события не даёт: если зеркало уже на ревизии ответа,
                        // раскладка ставится заново, иначе её поставит доставка. Так вид, сдвинутый
                        // перетаскиванием разделителя, возвращается к модели.
                        Ok(Ok(reply)) if matches!(sent, Command::LayoutResize { .. }) => {
                            defer(move |app| {
                                let caught_up = MIRROR.with(|mirror| {
                                    mirror
                                        .borrow()
                                        .as_ref()
                                        .is_some_and(|mirror| mirror.revision() >= reply.revision)
                                });
                                if caught_up {
                                    refresh_layout(app);
                                }
                            });
                        }
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => defer(move |app| on_refused(app, &sent, &error)),
                        Err(_) => defer(move |app| {
                            owner::say_err(format_args!(
                                "shellio: {} отклонена: ядро не ответило",
                                sent.name()
                            ));
                            with_window(|owner| owner.show_status(Some("Ядро не ответило")));
                            refresh_layout(app);
                        }),
                    }
                });
            }
            Err(error) => defer(move |app| on_refused(app, &sent, &error)),
        }
    }
}

/// Отказ ядра на команду интерфейса. `split_needs_panel` деления без `open` открывает выбор
/// «Открыть в новой части» у кнопки (ADR-0009); остальные отказы пишут причину в строку статуса и
/// показывают раскладку модели заново: событий после отказа не будет, а вид мог остаться там, куда
/// его увёл пользователь.
fn on_refused(app: &App, sent: &Command, error: &CoreError) {
    owner::say_err(format_args!("shellio: {} отклонена: {error}", sent.name()));
    if let Some(panel) = scene::picker_target(sent, error)
        && with_window(|owner| owner.open_picker(panel)) == Some(true)
    {
        return;
    }
    let text = MIRROR.with(|mirror| {
        let mirror = mirror.borrow();
        let state = mirror.as_ref().map(Mirror::state);
        scene::refusal_text(sent, error, |panel| {
            state
                .and_then(|state| state.panels.get(&panel))
                .map(|panel| panel.title.to_string())
        })
    });
    with_window(|owner| owner.show_status(Some(&text)));
    refresh_layout(app);
}

/// Сколько последних записей держит журнал анимаций `ui.stats`.
const ANIMATION_LOG: usize = 256;

struct OwnerWindow {
    mode: WindowMode,
    menu: MainMenu,
    monitor: Option<LoopMonitor>,
    choice: AppearanceChoice,
    /// Поколение отрисовки: растёт с каждой видимой сменой состояния окна.
    generation: Generation,
    /// Поколение первой проекции снимка зеркала: старт процесса считается до отрисовки с ним.
    first_snapshot: Option<u64>,
    /// Старт процесса, однажды наблюдённый.
    startup: Option<Startup>,
    /// Последние проекции зеркала: новое окно получает их сразу.
    projection: Option<Projection>,
    /// Выбор ячейки, который уже получили компоненты галереи.
    applied: ThemeSelection,
    /// Пересоздаваемая часть: окно и дерево галереи.
    window: MainWindow,
    gallery: Option<GalleryUi>,
    /// Раскладка обычного режима; в галерее её нет.
    layout: Option<LayoutUi>,
    /// Строка статуса обычного режима.
    status: Option<StatusLine>,
    /// Размер окна для ядра.
    viewport: ViewportGate,
    /// Смены компонентов, ещё не записанные в журнал: запись получает поколение публикации.
    pending: Vec<(&'static str, Transition)>,
    /// Журнал анимаций `ui.stats`.
    animations: VecDeque<AnimationRecord>,
}

impl OwnerWindow {
    fn bump(&mut self) {
        self.generation.bump();
        self.publish();
    }

    /// Текущее поколение получают монитор цикла и маркер галереи, смены компонентов уходят в
    /// журнал анимаций с этим поколением.
    fn publish(&mut self) {
        let generation = self.generation.get();
        for (component, transition) in self.pending.drain(..) {
            if self.animations.len() == ANIMATION_LOG {
                self.animations.pop_front();
            }
            self.animations.push_back(AnimationRecord {
                generation,
                component: component.to_owned(),
                path: match transition.path {
                    TransitionPath::Animated => AnimationPath::Animated,
                    TransitionPath::Instant => AnimationPath::Instant,
                },
                added: transition.added,
            });
        }
        if let Some(monitor) = &self.monitor {
            monitor.set_generation(generation);
        }
        if let Some(gallery) = &self.gallery {
            gallery.set_generation(generation);
        }
    }

    /// Компоненты галереи получают действующую ячейку. `false` — она уже применена: отложенный
    /// отклик окна на смену, которую команда применила сама, поколения не прибавляет.
    fn sync_theme(&mut self) -> bool {
        let selection = self.window.selection();
        if selection == self.applied {
            return false;
        }
        self.applied = selection;
        if let Some(gallery) = &self.gallery {
            let transitions = gallery.set_theme(&selection.theme(), selection.motion);
            self.pending.extend(transitions);
        }
        if let Some(layout) = self.layout.as_mut() {
            layout.set_theme(&selection.theme(), selection.motion);
        }
        if let Some(status) = &self.status {
            status.set_theme(&selection.theme());
        }
        true
    }

    /// Сообщение строки статуса; `None` убирает его.
    fn show_status(&self, message: Option<&str>) {
        if let Some(status) = &self.status {
            status.set_message(message);
        }
    }

    /// Выбор «Открыть в новой части» для панели `panel`; `false` — выбора нет.
    fn open_picker(&self, panel: shellio_core::PanelId) -> bool {
        self.layout
            .as_ref()
            .is_some_and(|layout| layout.open_picker(panel))
    }

    /// Размер содержимого окна в целых точках.
    fn content_viewport(&self) -> shellio_layout::Viewport {
        let (width, height) = self.window.content_size();
        scene::viewport_of(width, height)
    }

    /// Раскладка по зеркалу и текущему размеру окна, строка статуса под ней, кнопки зон заголовка
    /// и пункты меню «Вид» по сцене.
    fn render_layout(&mut self, app: &App) {
        let viewport = self.content_viewport();
        let Some(layout) = self.layout.as_mut() else {
            return;
        };
        MIRROR.with(|mirror| {
            let mirror = mirror.borrow();
            layout.render(app, mirror.as_ref().map(Mirror::state), viewport);
        });
        let Some(scene) = layout.scene() else {
            return;
        };
        if let Some(status) = &self.status {
            let top = scene.area.y + scene.area.height;
            let height = f64::from(viewport.height) - top;
            self.window.place(
                status,
                Frame::new(0.0, top, f64::from(viewport.width), height.max(0.0)),
            );
        }
        let zones = scene.zone_buttons().map(|buttons| {
            (
                buttons.side == shellio_layout::Hideable::Plate,
                [buttons.navigator, buttons.bottom, buttons.side_shown],
            )
        });
        self.window.chrome().set_zones(zones);
        self.menu.set_zones(zones);
    }

    /// Переопределения окна: выбор оформления и оси галереи.
    fn overrides(&self) -> Overrides {
        let mut overrides = Overrides {
            choice: theme_choice(self.choice),
            ..Overrides::default()
        };
        if let Some(gallery) = &self.gallery {
            let state = gallery.model().state();
            overrides.gallery_appearance = state.appearance;
            overrides.gallery_contrast = state.contrast;
            overrides.gallery_motion = state.motion;
        }
        overrides
    }

    fn set_choice(&mut self, choice: AppearanceChoice) {
        self.choice = choice;
        let theme = theme_choice(choice);
        self.window.set_overrides(self.overrides());
        self.window.chrome().set_choice(theme);
        self.menu.set_appearance(theme);
        self.sync_theme();
        self.bump();
    }

    /// Проекции зеркала: имя воркспейса в заголовке и системный заголовок окна.
    fn project(&mut self, projection: Projection) {
        self.show_projection(&projection);
        self.projection = Some(projection);
        self.bump();
        self.first_snapshot.get_or_insert(self.generation.get());
    }

    fn show_projection(&self, projection: &Projection) {
        self.window.set_title(projection.title.as_deref());
        self.window
            .chrome()
            .set_workspace(projection.workspace.as_deref());
    }

    /// Служебная команда галереи. Эффект исполняется в этом же обороте главного потока, поэтому
    /// маркер поколения из ответа появляется в том же проходе отображения, что и состояние.
    fn gallery_command(
        &mut self,
        app: &App,
        command: GalleryCommand,
    ) -> Result<UiReply, UiRefusal> {
        gallery::admit(self.mode)?;
        let Some(gallery) = self.gallery.as_mut() else {
            return Err(UiRefusal::NoUi);
        };
        let effect = gallery.model_mut().apply(command, &mut self.generation);
        let generation = self.generation.get();
        match effect {
            Effect::Theme => self.window.set_overrides(self.overrides()),
            Effect::Focus(focus) => gallery.focus(&self.window, focus),
            Effect::OpenMenu => gallery.open_menu(),
            Effect::Rebuild => gallery.rebuild(app, &self.window, generation),
            Effect::RecreateWindow => self.recreate_window(app),
            Effect::Place(screen) => {
                let moved = usize::try_from(screen)
                    .is_ok_and(|screen| self.window.move_to_screen(app, screen));
                if !moved {
                    owner::say_err(format_args!(
                        "shellio: экрана {screen} нет, окно галереи осталось на месте"
                    ));
                }
            }
            Effect::Reset => {
                self.window.set_overrides(self.overrides());
                let motion = self.window.selection().motion;
                if let Some(gallery) = &self.gallery {
                    let transitions = gallery.reset_view(&self.window, motion);
                    self.pending.extend(transitions);
                }
            }
            Effect::Tabs(strip) => {
                let transition = gallery.show_tabs(strip);
                self.pending.extend(transition);
            }
            Effect::List => gallery.show_list(),
            Effect::ListScroll(row) => gallery.scroll_list(row),
            Effect::ListTicks => gallery.apply_list_ticks(),
            Effect::Tree => gallery.show_tree(),
        }
        self.sync_theme();
        self.publish();
        Ok(UiReply::Accepted(Accepted {
            command: command.name().to_owned(),
            pid: std::process::id(),
            generation: self.generation.get(),
        }))
    }

    /// Закрывает и отпускает окно вместе с деревом галереи и строит новые. Видимость, ячейка,
    /// движение и проекции зеркала переходят в новое окно.
    fn recreate_window(&mut self, app: &App) {
        let visible = self.window.is_visible();
        if let Some(gallery) = self.gallery.as_mut() {
            gallery.retire_with_window(&self.window);
        }
        let old = std::mem::replace(&mut self.window, new_window(app));
        drop(old);
        self.window.set_overrides(self.overrides());
        self.window.chrome().set_choice(theme_choice(self.choice));
        if let Some(projection) = &self.projection {
            self.show_projection(projection);
        }
        let generation = self.generation.get();
        if let Some(gallery) = self.gallery.as_mut() {
            gallery.build(app, &self.window, generation);
        }
        // Новое дерево строится уже в ячейке нового окна.
        self.applied = self.window.selection();
        if visible {
            self.window.show();
        }
    }

    fn gallery_intent(&mut self, intent: &Intent) {
        if let Some(gallery) = self.gallery.as_mut() {
            match gallery.model_mut().intent(intent, &mut self.generation) {
                Some(Changed::Tabs(strip)) => {
                    let transition = gallery.show_tabs(strip);
                    self.pending.extend(transition);
                }
                Some(Changed::List) => gallery.show_list(),
                Some(Changed::Tree) => gallery.show_tree(),
                None => {}
            }
            self.publish();
        }
    }

    fn gallery_view(&self) -> Option<GalleryView> {
        let gallery = self.gallery.as_ref()?;
        let model = gallery.model();
        let state = model.state();
        Some(GalleryView {
            tree: gallery.tree_number().unwrap_or(model.tree()),
            window: model.window(),
            appearance: state.appearance.map(|value| match value {
                Appearance::Light => shellio_control::Lightness::Light,
                Appearance::Dark => shellio_control::Lightness::Dark,
            }),
            contrast: state.contrast.map(|value| match value {
                Contrast::Normal => shellio_control::Contrast::Normal,
                Contrast::Increased => shellio_control::Contrast::Increased,
            }),
            motion: state.motion.map(|value| match value {
                Motion::Normal => shellio_control::Motion::Normal,
                Motion::Reduced => shellio_control::Motion::Reduced,
            }),
            focus: state.focus,
            screen: state.screen,
            intents: state.intents.iter().cloned().collect(),
        })
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
            generation: self.generation.get(),
        }
    }

    fn inspect_view(&self) -> InspectView {
        let inspection = self.window.inspect();
        let (width, height) = inspection.content;
        let gallery_nodes = self.gallery.as_ref().map(GalleryUi::inspect);
        let layout_nodes = self.layout.as_ref().map(LayoutUi::inspect);
        let status_nodes = self
            .status
            .as_ref()
            .map(|status| status.inspect("status", None));
        InspectView {
            window_number: self.window.number(),
            content: Size { width, height },
            generation: self.generation.get(),
            components: inspection
                .nodes
                .into_iter()
                .chain(gallery_nodes.into_iter().flatten())
                .chain(status_nodes.into_iter().flatten())
                .map(component)
                .chain(layout_nodes.into_iter().flatten())
                .collect(),
            materials: inspection
                .materials
                .into_iter()
                .map(|material| MaterialView {
                    class: material.class,
                    visible: material.visible,
                    own: material.own,
                })
                .collect(),
            gallery: self.gallery_view(),
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
            generation: self.generation.get(),
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
            animations: self.animations.iter().cloned().collect(),
            row_views: self
                .gallery
                .as_ref()
                .map(GalleryUi::row_views)
                .unwrap_or_default(),
            gallery_trees: self
                .gallery
                .as_ref()
                .map(GalleryUi::retired_trees)
                .unwrap_or_default(),
            key_latency: self
                .gallery
                .as_ref()
                .and_then(|gallery| gallery.key_latency(self.monitor.as_ref())),
        }
    }

    fn accepted(&self, command: UiCommand) -> UiReply {
        UiReply::Accepted(Accepted {
            command: command.name().to_owned(),
            pid: std::process::id(),
            generation: self.generation.get(),
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
        NodeState::TabStrip(facts) => ComponentState::TabStrip(Box::new(tab_strip_view(*facts))),
        NodeState::VirtualList(facts) => {
            ComponentState::VirtualList(Box::new(virtual_list_view(*facts)))
        }
        NodeState::TreeList(facts) => ComponentState::TreeList(Box::new(tree_list_view(*facts))),
    }
}

fn tree_list_view(facts: TreeListFacts) -> TreeListView {
    let wide = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
    TreeListView {
        label: facts.label,
        count: wide(facts.count),
        nodes: wide(facts.nodes),
        selected: facts.selected,
        expanded: facts.expanded,
        offset: facts.offset,
        max_offset: facts.max_offset,
        keyboard_focus: facts.keyboard_focus,
        row_views_created: facts.row_views_created,
        cell_views_created: facts.cell_views_created,
        visible: facts
            .visible
            .into_iter()
            .map(|row| TreeRowView {
                index: wide(row.index),
                id: row.id,
                text: row.text,
                level: wide(row.level),
                expandable: row.expandable,
                expanded: row.expanded,
            })
            .collect(),
    }
}

fn virtual_list_view(facts: VirtualListFacts) -> VirtualListView {
    let wide = |value: usize| u64::try_from(value).unwrap_or(u64::MAX);
    VirtualListView {
        label: facts.label,
        count: wide(facts.count),
        selected: facts.selected,
        offset: facts.offset,
        max_offset: facts.max_offset,
        keyboard_focus: facts.keyboard_focus,
        row_views_created: facts.row_views_created,
        cell_views_created: facts.cell_views_created,
        visible: facts
            .visible
            .into_iter()
            .map(|row| ListRowView {
                index: wide(row.index),
                id: row.id,
                text: row.text,
            })
            .collect(),
    }
}

fn tab_strip_view(facts: TabStripFacts) -> TabStripView {
    let state = facts.state;
    let presentation = state.presentation;
    let tabs = state
        .tabs
        .iter()
        .map(|tab| {
            let seen = facts.tabs.iter().find(|seen| seen.id == tab.id);
            TabItemView {
                id: tab.id.clone(),
                title: tab.title.clone(),
                kind: tab.kind.clone(),
                status: match tab.status {
                    TabStatus::None => TabStatusView::None,
                    TabStatus::Running => TabStatusView::Running,
                    TabStatus::Waiting => TabStatusView::Waiting,
                    TabStatus::Error => TabStatusView::Error,
                    TabStatus::Done => TabStatusView::Done,
                    TabStatus::Idle => TabStatusView::Idle,
                    TabStatus::Exited => TabStatusView::Exited,
                },
                icon: seen
                    .is_some_and(|seen| seen.icon_shown)
                    .then(|| tab.icon.clone()),
                close_visible: seen.is_some_and(|seen| seen.close_visible),
                fully_visible: seen.is_some_and(|seen| seen.fully_visible),
            }
        })
        .collect();
    TabStripView {
        label: state.label.clone(),
        zone: match presentation.zone {
            ZoneSide::Side => TabZone::Side,
            ZoneSide::Center => TabZone::Center,
            ZoneSide::Bottom => TabZone::Bottom,
        },
        axis: match presentation.axis() {
            SplitAxis::Columns => TabAxis::Columns,
            SplitAxis::Rows => TabAxis::Rows,
        },
        navigator: presentation.navigator,
        zoomed: presentation.zoomed,
        focused: state.focused,
        keyboard_focus: facts.keyboard_focus,
        regime: facts.regime.map(|regime| match regime {
            Regime::Visible => TabRegime::Visible,
            Regime::InMenu => TabRegime::InMenu,
            Regime::Scroll => TabRegime::Scroll,
        }),
        selected: state.selected.clone(),
        payload: state.payload.clone(),
        payload_visible: facts.payload_visible,
        offset: facts.offset,
        max_offset: facts.max_offset,
        tabs,
        buttons: facts
            .buttons
            .into_iter()
            .map(|button| StripButtonView {
                id: button.button.id().to_owned(),
                label: button.button.label().to_owned(),
                symbol: button.symbol.to_owned(),
                hint: button.hint,
                help: button.help,
            })
            .collect(),
    }
}

pub(crate) fn component(node: InspectNode) -> ComponentView {
    let facts = node.facts;
    ComponentView {
        id: node.id,
        class: node.class.to_owned(),
        object: node.object,
        parent: node.parent,
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
#[derive(Clone)]
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
    let window = new_window(app);
    let menu = MainMenu::install(app, |action| {
        defer(move |app| match command_of(action) {
            Some(command) => handle(app, command),
            None => {
                if let MenuAction::Zone(button) = action {
                    on_zone(app, button);
                }
            }
        });
    });
    let monitor = match LoopMonitor::install(app) {
        Ok(monitor) => Some(monitor),
        Err(error) => {
            owner::say_err(format_args!("shellio: монитор цикла не поставлен: {error}"));
            None
        }
    };
    let number = window.number();
    let gallery = match mode {
        WindowMode::Gallery => Some(GalleryUi::new(app, &window, 0, |intent| {
            defer(move |_| {
                with_window(|owner| owner.gallery_intent(&intent));
            });
        })),
        WindowMode::Normal => None,
    };
    let layout = match mode {
        WindowMode::Normal => Some(LayoutUi::new(
            app,
            &window,
            |split, drag| defer(move |app| on_drag(app, split, drag)),
            |action| defer(move |app| on_panel(app, action)),
        )),
        WindowMode::Gallery => None,
    };
    let status = match mode {
        WindowMode::Normal => Some(StatusLine::new(app, &window.selection().theme())),
        WindowMode::Gallery => None,
    };
    let projection = MIRROR.with(|mirror| mirror.borrow().as_ref().map(Projection::of));
    let applied = window.selection();
    WINDOW.with(|slot| {
        *slot.borrow_mut() = Some(ManuallyDrop::new(OwnerWindow {
            mode,
            menu,
            monitor,
            choice: AppearanceChoice::System,
            generation: Generation::default(),
            first_snapshot: None,
            startup: None,
            projection: None,
            applied,
            window,
            gallery,
            layout,
            status,
            viewport: ViewportGate::default(),
            pending: Vec::new(),
            animations: VecDeque::new(),
        }));
    });
    if let Some(projection) = projection {
        with_window(|owner| owner.project(projection));
    }
    with_window(|owner| owner.render_layout(app));
    number
}

/// Новое окно владельца: смена ячейки и намерения заголовка уходят в очередь главного потока.
fn new_window(app: &App) -> MainWindow {
    MainWindow::new(
        app,
        |_: ThemeSelection| defer(|_| on_theme()),
        |intent| defer(move |app| on_intent(app, intent)),
        || defer(on_resize),
    )
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

/// Применить доставку к зеркалу. Применённая доставка обновляет проекции окна и раскладку.
pub fn deliver(app: &App, delivery: Delivery) -> MirrorStep {
    let (step, projection) = MIRROR.with(|slot| {
        let mut mirror = slot.borrow_mut();
        let step = Mirror::deliver_to(&mut mirror, delivery);
        let projection = match step {
            MirrorStep::Applied => mirror.as_ref().map(Projection::of),
            MirrorStep::Ignored | MirrorStep::NeedSnapshot => None,
        };
        (step, projection)
    });
    if let Some(projection) = projection {
        with_window(|owner| {
            owner.project(projection);
            owner.render_layout(app);
        });
    }
    step
}

/// Ядро загружено, сокет ещё не опубликован: главный поток получает ручку ядра, обычное окно
/// передаёт первый размер содержимого. Галерея раскладку не показывает и размера не передаёт:
/// деление и размер отвечают `no_ui`.
pub fn attach_core(_app: &App, core: CoreHandle, runtime: tokio::runtime::Handle) {
    let first = with_window(|owner| {
        let current = owner.content_viewport();
        owner
            .layout
            .is_some()
            .then(|| owner.viewport.flush(current))
            .flatten()
    })
    .flatten();
    if let Some(viewport) = first {
        core.set_viewport(Some(viewport));
    }
    CORE.with(|slot| *slot.borrow_mut() = Some(CoreLink { core, runtime }));
}

/// Размер содержимого окна изменился: раскладка встаёт по нему сразу, ядро получает его в конце
/// оборота цикла.
fn on_resize(app: &App) {
    let schedule = with_window(|owner| {
        owner.render_layout(app);
        owner.layout.is_some() && owner.viewport.request()
    });
    if schedule == Some(true) {
        defer(|_| flush_viewport());
    }
}

fn flush_viewport() {
    // Без ядра размер не отмечается отправленным: отправка остаётся запланированной, текущий размер
    // передаст `attach_core`.
    if CORE.with(|slot| slot.borrow().is_none()) {
        return;
    }
    let next = with_window(|owner| {
        let current = owner.content_viewport();
        owner.viewport.flush(current)
    })
    .flatten();
    if let Some(viewport) = next {
        CORE.with(|slot| {
            if let Some(link) = slot.borrow().as_ref() {
                link.core.set_viewport(Some(viewport));
            }
        });
    }
}

/// Раскладка модели заново: после отказа команды из интерфейса.
fn refresh_layout(app: &App) {
    with_window(|owner| owner.render_layout(app));
}

/// Команда раскладки из интерфейса: сообщение строки статуса убирается, отказ ядра показывается в
/// ней. Без ядра раскладка возвращается к модели.
fn send_ui(app: &App, command: Command) {
    with_window(|owner| owner.show_status(None));
    let sent = CORE.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|link| link.send(command))
            .is_some()
    });
    if !sent {
        refresh_layout(app);
    }
}

/// Действие в раскладке: щелчок по вкладке или в тело панели (`panel.select`, `panel.focus`, если
/// он что-то меняет), кнопка закрытия вкладки, кнопки «Разделить» и «Развернуть панель» /
/// «Вернуть раскладку», пункты меню «…», пункт выбора «Открыть в новой части», кнопка и тело
/// заглушки «Развернуть», отпускание перетаскиваемой вкладки. Каждое — одна команда ядра; движение
/// перетаскивания только подсвечивает цель.
fn on_panel(app: &App, action: PanelAction) {
    let command = with_window(|owner| {
        owner.show_status(None);
        let layout = owner.layout.as_ref()?;
        let scene = layout.scene()?;
        match action {
            PanelAction::Select(panel) => scene::select_command(scene, panel),
            PanelAction::Close(panel) => scene::close_command(scene, panel),
            PanelAction::Focus(panel) => scene::focus_command(scene, panel),
            PanelAction::Split(plate) => scene::split_command(scene, plate),
            PanelAction::Zoom(plate) => scene::zoom_command(scene, plate),
            PanelAction::Menu(plate, item) => scene::menu_command(scene, plate, item),
            PanelAction::Open { panel, plugin } => Some(scene::open_command(panel, plugin)),
            PanelAction::Expand(host) => scene::expand_command(scene, host),
            PanelAction::StubPress(host) => scene::stub_focus_command(scene, host),
            PanelAction::Drag { panel, point } => {
                layout.drag_over(panel, point);
                None
            }
            PanelAction::Drop { panel, point } => layout.drop_at(panel, point),
        }
    })
    .flatten();
    if let Some(command) = command {
        send_ui(app, command);
    }
}

/// Кнопка зон заголовка или пункт меню «Вид»: одна команда `layout.visibility`.
fn on_zone(app: &App, button: ZoneButton) {
    let toggle = match button {
        ZoneButton::Navigator => ZoneToggle::Navigator,
        ZoneButton::Bottom => ZoneToggle::Bottom,
        ZoneButton::Side => ZoneToggle::Side,
    };
    // Клавиша пункта меню «Вид» срабатывает и без окна: скрытое окно её не получает.
    let command = with_window(|owner| {
        if !owner.window.is_visible() {
            return None;
        }
        let scene = owner.layout.as_ref().and_then(LayoutUi::scene)?;
        scene::toggle_command(scene, toggle)
    })
    .flatten();
    if let Some(command) = command {
        send_ui(app, command);
    }
}

/// Конец перетаскивания разделителя: одна команда `layout.resize`. Разделитель, который размер не
/// меняет, возвращается на место модели.
fn on_drag(app: &App, split: SplitId, drag: SplitDrag) {
    let command = with_window(|owner| {
        owner.show_status(None);
        let command = owner
            .layout
            .as_ref()
            .and_then(LayoutUi::scene)
            .and_then(|scene| scene::resize_command(scene, split, drag.divider, drag.position));
        if command.is_none() {
            owner.render_layout(app);
        }
        command
    })
    .flatten();
    if let Some(command) = command {
        send_ui(app, command);
    }
}

/// Запрос `ui` управляющего API. Без окна — `no_ui`. Действие выполняет [`handle`]; `app.quit`
/// отвечает квитанцией до `terminate:`, который [`App::terminate`] ставит таймером цикла.
pub fn request(app: &App, request: UiRequest) -> Result<UiReply, UiRefusal> {
    if let UiRequest::Gallery(command) = request {
        return with_window(|owner| owner.gallery_command(app, command))
            .unwrap_or(Err(UiRefusal::NoUi));
    }
    let reply = match request {
        UiRequest::Window => {
            let revision = MIRROR.with(|mirror| mirror.borrow().as_ref().map(|m| m.revision().0));
            with_window(|owner| UiReply::Window(Box::new(owner.window_view(app, revision))))
        }
        UiRequest::Inspect => with_window(|owner| UiReply::Inspect(Box::new(owner.inspect_view()))),
        UiRequest::Stats => {
            let reply = with_window(|owner| UiReply::Stats(Box::new(owner.stats_view())));
            // Отложенные освобождения AppKit ждут в пуле `run` очередного события: следующее чтение
            // считает живые объекты уже после слива, и утечку не путает с ожиданием ввода.
            app.drain_event_pool();
            reply
        }
        UiRequest::Command(command) => {
            let owner = with_window(|_| ());
            owner.and_then(|()| {
                handle(app, command);
                with_window(|owner| owner.accepted(command))
            })
        }
        UiRequest::Gallery(_) => None,
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
    with_window(|owner| {
        if owner.sync_theme() {
            owner.bump();
        }
    });
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
        ChromeIntent::Zone(button) => on_zone(app, button),
    }
}

/// Действие меню в команде `ui`; `None` — пункт зон: он идёт командой ядра.
fn command_of(action: MenuAction) -> Option<UiCommand> {
    match action {
        MenuAction::Quit => Some(UiCommand::AppQuit),
        MenuAction::CloseWindow => Some(UiCommand::WindowClose),
        MenuAction::Appearance(choice) => Some(UiCommand::AppearanceSet(core_choice(choice))),
        MenuAction::Zone(_) => None,
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
        assert_eq!(command_of(MenuAction::Quit), Some(UiCommand::AppQuit));
        assert_eq!(
            command_of(MenuAction::CloseWindow),
            Some(UiCommand::WindowClose)
        );
        for button in ZoneButton::ALL {
            assert_eq!(command_of(MenuAction::Zone(button)), None);
        }
        for choice in AppearanceChoice::ALL {
            assert_eq!(core_choice(theme_choice(choice)), choice);
            assert_eq!(
                command_of(MenuAction::Appearance(theme_choice(choice))),
                Some(UiCommand::AppearanceSet(choice))
            );
        }
    }
}
