//! Вид галереи на главном потоке: компоненты `shellio-appkit` по фикстурам чистой модели
//! [`shellio::gallery`], маркер поколения, перестройка дерева и слабое наблюдение за видами
//! прежних деревьев. Намерения компонентов уходят получателю, который ставит их в очередь главного
//! потока: вид не трогает состояние окна изнутри вызова AppKit.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use shellio::gallery::{
    self, Gallery, GalleryState, Intent, ListAction, MenuFixtureItem, Page, StripHost, StripStatus,
    StripZone, TabAction, TreeAction, TreeFixture, ids,
};
use shellio_appkit::{
    App, FocusLayer, Frame, GenerationMarker, InspectNode, ListIntent, ListKey, ListState,
    LoopMonitor, MainWindow, MenuButton, MenuEntry, MenuIntent, MenuItem, MenuKey, Piece,
    Presentation, Row, Surface, SurfaceFill, SurfaceRadius, SurfaceSpec, Tab, TabIntent, TabStatus,
    TabStrip, TabStripState, TextField, TextFieldIntent, TextFieldState, Transition, TreeList,
    TreeListIntent, TreeListState, TreeNode, ViewWatch, VirtualList, ZoneSide, text_field_height,
};
use shellio_control::{
    ClassAliveView, GalleryFocus, GalleryStrip, GalleryTreeView, KeyLatencyView, KeyPressView,
    RowViewsView,
};
use shellio_theme::{AppTheme, KeyMode, Motion};

/// Сколько прежних деревьев держит `ui.stats.gallery_trees`.
const RETIRED_TREES: usize = 64;

/// Дерево компонентов одного построения. Освобождение убирает виды из окна.
struct Tree {
    number: u64,
    // Порядок полей — порядок освобождения: сначала содержимое подложки.
    /// Полосы вкладок в порядке [`GalleryStrip::ALL`].
    strips: Vec<(GalleryStrip, TabStrip)>,
    list: VirtualList,
    tree_list: TreeList,
    field: TextField,
    menu: MenuButton,
    focus: Surface,
    plain: Surface,
    marker: GenerationMarker,
    watch: ViewWatch,
}

/// Дерево страницы «Калибровка»: лист материалов калибровки и маркер поколения.
struct Calibration {
    number: u64,
    sheet: Surface,
    marker: GenerationMarker,
    watch: ViewWatch,
}

/// Освобождённое дерево: слабые ссылки на его объекты.
struct Retired {
    number: u64,
    watch: ViewWatch,
}

/// Вставки в дерево и обмен первых двух корней: по ним строятся узлы дерева галереи.
type TreeKey = (u32, bool);

/// Галерея окна владельца.
pub struct GalleryUi {
    model: Gallery,
    tree: Option<Tree>,
    /// Дерево страницы «Калибровка»: построено либо оно, либо `tree`.
    calibration: Option<Calibration>,
    retired: VecDeque<Retired>,
    on_intent: Rc<dyn Fn(Intent)>,
    /// Строки списка последнего зерна: смена выбора не пересобирает набор.
    list_rows: RefCell<Option<(u64, Rc<[Row]>)>>,
    /// Корни дерева последних вставок и обмена: смена раскрытия и выбора не пересобирает узлы.
    tree_roots: RefCell<Option<(TreeKey, Rc<[TreeNode]>)>>,
}

impl GalleryUi {
    /// Строит первое дерево в окне `window`. `on_intent` вызывается на главном потоке изнутри
    /// AppKit и не должен синхронно трогать галерею.
    pub fn new(
        app: &App,
        window: &MainWindow,
        generation: u64,
        on_intent: impl Fn(Intent) + 'static,
    ) -> GalleryUi {
        let mut gallery = GalleryUi {
            model: Gallery::new(),
            tree: None,
            calibration: None,
            retired: VecDeque::new(),
            on_intent: Rc::new(on_intent),
            list_rows: RefCell::new(None),
            tree_roots: RefCell::new(None),
        };
        gallery.build(app, window, generation);
        gallery
    }

    pub fn model(&self) -> &Gallery {
        &self.model
    }

    pub fn model_mut(&mut self) -> &mut Gallery {
        &mut self.model
    }

    /// Страница, дерево которой построено.
    pub fn built_page(&self) -> Option<Page> {
        if self.calibration.is_some() {
            Some(Page::Calibration)
        } else {
            self.tree.as_ref().map(|_| Page::Components)
        }
    }

    /// Строит дерево с номером модели по текущему состоянию галереи: страница — из состояния.
    pub fn build(&mut self, app: &App, window: &MainWindow, generation: u64) {
        let selection = window.selection();
        let theme = selection.theme();
        let (width, height) = window.content_size();
        match self.model.state().page {
            Page::Components => {}
            Page::Calibration => {
                // Лист несёт фон материалов калибровки — заливку `--surface` ячейки.
                let sheet = Surface::new(
                    app,
                    &theme,
                    SurfaceSpec {
                        fill: SurfaceFill::Surface,
                        line: false,
                        radius: SurfaceRadius::None,
                        layer: FocusLayer::Plain,
                    },
                );
                let marker = GenerationMarker::new(app, generation);
                let marker_frame =
                    gallery::layout(width, height, theme.metrics.titlebar, 0.0, 0.0, 0.0);
                window.place(&marker, frame(marker_frame.marker));
                window.place(
                    &sheet,
                    frame(gallery::calibration_sheet(
                        width,
                        height,
                        theme.metrics.titlebar,
                    )),
                );
                let mut watch = ViewWatch::new();
                watch.watch(Piece::from(&marker));
                watch.watch(Piece::from(&sheet));
                self.calibration = Some(Calibration {
                    number: self.model.tree(),
                    sheet,
                    marker,
                    watch,
                });
                window.clear_focus();
                return;
            }
        }
        let layout = gallery::layout(
            width,
            height,
            theme.metrics.titlebar,
            text_field_height(&theme),
            theme.metrics.tabbar,
            theme.metrics.row,
        );
        let fixture = gallery::field_fixture();
        let state = TextFieldState {
            value: self.model.state().field_value.clone(),
            placeholder: fixture.placeholder,
            editable: fixture.editable,
        };
        let on_intent = Rc::clone(&self.on_intent);
        let field = TextField::new(app, &theme, state, fixture.label, move |intent| {
            on_intent(match intent {
                TextFieldIntent::Changed(value) => Intent::FieldChanged(value),
                TextFieldIntent::Submitted(value) => Intent::FieldSubmitted(value),
                TextFieldIntent::Cancelled => Intent::FieldCancelled,
            });
        });
        let menu_fixture = gallery::menu_fixture();
        let on_intent = Rc::clone(&self.on_intent);
        let menu = match MenuButton::new(
            app,
            &theme,
            menu_fixture.label,
            menu_fixture.title,
            menu_entries(&menu_fixture.items),
            move |intent| match intent {
                MenuIntent::Chosen { id } => on_intent(Intent::MenuChosen(id)),
            },
        ) {
            Ok(menu) => menu,
            Err(error) => {
                // Фикстура закреплена тестом уникальности id: отказ — нарушенный инвариант.
                shellio::owner::say_err(format_args!(
                    "shellio: меню галереи не построено: {error}"
                ));
                return;
            }
        };
        let panel = |layer| SurfaceSpec {
            fill: SurfaceFill::Panel,
            line: true,
            radius: SurfaceRadius::Plate,
            layer,
        };
        let focus = Surface::new(app, &theme, panel(FocusLayer::Tone(KeyMode::Navigation)));
        let plain = Surface::new(app, &theme, panel(FocusLayer::Veil));
        let marker = GenerationMarker::new(app, generation);
        window.place(&marker, frame(layout.marker));
        window.place(&focus, frame(layout.focus));
        window.place(&plain, frame(layout.plain));
        focus.place(&field, frame(layout.field));
        focus.place(&menu, frame(layout.menu));
        let mut strips = Vec::new();
        for strip in GalleryStrip::ALL {
            let on_intent = Rc::clone(&self.on_intent);
            let state = strip_state(strip, self.model.state());
            let built = TabStrip::new(app, &theme, selection.motion, state, move |intent| {
                on_intent(Intent::Tab(strip, tab_action(intent)));
            });
            let built = match built {
                Ok(built) => built,
                Err(error) => {
                    // Фикстуры закреплены тестом уникальности id: отказ — нарушенный инвариант.
                    shellio::owner::say_err(format_args!(
                        "shellio: полоса вкладок {} не построена: {error}",
                        ids::strip(strip)
                    ));
                    continue;
                }
            };
            let rect = frame(layout.strip(strip));
            match gallery::strip_fixture(strip).host {
                StripHost::FocusSurface => focus.place(&built, rect),
                StripHost::Window => window.place(&built, rect),
            }
            strips.push((strip, built));
        }
        let on_intent = Rc::clone(&self.on_intent);
        let list = match VirtualList::new(app, &theme, self.list_state(), move |intent| {
            let action = match intent {
                ListIntent::Select { id } => ListAction::Select(id),
                ListIntent::Activate { id } => ListAction::Activate(id),
                // Уровней у списка галереи нет.
                ListIntent::Cancel => return,
            };
            on_intent(Intent::List(action));
        }) {
            Ok(list) => list,
            Err(error) => {
                // Фикстура закреплена тестом уникальности id: отказ — нарушенный инвариант.
                shellio::owner::say_err(format_args!(
                    "shellio: список галереи не построен: {error}"
                ));
                return;
            }
        };
        // Список галереи — прибор пунктов задержки `make e2e`.
        list.enable_latency_probe();
        list.set_latency_ticks(self.model.state().list_ticks);
        plain.place(&list, frame(layout.list));
        let on_intent = Rc::clone(&self.on_intent);
        let tree_list = match TreeList::new(app, &theme, self.tree_state(), move |intent| {
            on_intent(Intent::Tree(match intent {
                TreeListIntent::Select { id } => TreeAction::Select(id),
                TreeListIntent::Toggle { id, expand } => TreeAction::Toggle(id, expand),
                TreeListIntent::Activate { id } => TreeAction::Activate(id),
            }));
        }) {
            Ok(tree_list) => tree_list,
            Err(error) => {
                // Фикстура и правила модели закреплены тестами: отказ — нарушенный инвариант.
                shellio::owner::say_err(format_args!(
                    "shellio: дерево галереи не построено: {error}"
                ));
                return;
            }
        };
        plain.place(&tree_list, frame(layout.tree));
        let mut watch = ViewWatch::new();
        for piece in [
            Piece::from(&marker),
            Piece::from(&focus),
            Piece::from(&plain),
            Piece::from(&list),
            Piece::from(&tree_list),
        ] {
            watch.watch(piece);
        }
        for (_, strip) in &strips {
            watch.watch(Piece::from(strip));
        }
        self.tree = Some(Tree {
            number: self.model.tree(),
            strips,
            list,
            tree_list,
            field,
            menu,
            focus,
            plain,
            marker,
            watch,
        });
        // Новое дерево получает фокус последнего запроса `gallery.focus`.
        self.focus(window, self.model.state().focus);
    }

    /// Освобождает дерево и строит новое в том же окне.
    pub fn rebuild(&mut self, app: &App, window: &MainWindow, generation: u64) {
        self.retire(window, false);
        self.build(app, window, generation);
    }

    /// Освобождает дерево перед заменой окна: наблюдение получает и окно.
    pub fn retire_with_window(&mut self, window: &MainWindow) {
        self.retire(window, true);
    }

    fn retire(&mut self, window: &MainWindow, with_window: bool) {
        // Ввод в поле заканчивается до того, как поле уходит из окна: вид поля, снятый посреди ввода,
        // оставляет клип-вид field editor живым после окна (`window-cycle` (в), P1.3).
        window.clear_focus();
        let (number, mut watch) = if let Some(tree) = self.tree.take() {
            let Tree { number, watch, .. } = tree;
            (number, watch)
        } else if let Some(page) = self.calibration.take() {
            let Calibration { number, watch, .. } = page;
            (number, watch)
        } else {
            return;
        };
        if with_window {
            watch.watch_window(window);
        }
        if self.retired.len() == RETIRED_TREES {
            self.retired.pop_front();
        }
        self.retired.push_back(Retired { number, watch });
    }

    /// Компоненты получают ячейку и движение. Возвращает смены полос для журнала анимаций.
    pub fn set_theme(&self, theme: &AppTheme, motion: Motion) -> Vec<(&'static str, Transition)> {
        if let Some(page) = &self.calibration {
            page.sheet.set_theme(theme);
        }
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        tree.focus.set_theme(theme);
        tree.plain.set_theme(theme);
        tree.field.set_theme(theme);
        tree.menu.set_theme(theme);
        let mut transitions = vec![
            (ids::LIST, tree.list.set_theme(theme)),
            (ids::TREE, tree.tree_list.set_theme(theme)),
        ];
        transitions.extend(tree.strips.iter().map(|(strip, view)| {
            view.set_motion(motion);
            (ids::strip(*strip), view.set_theme(theme))
        }));
        transitions
    }

    /// Полоса `strip` приводится к состоянию модели. Возвращает смену для журнала анимаций.
    pub fn show_tabs(&self, strip: GalleryStrip) -> Option<(&'static str, Transition)> {
        let tree = self.tree.as_ref()?;
        let (_, view) = tree
            .strips
            .iter()
            .find(|(candidate, _)| *candidate == strip)?;
        match view.set_state(strip_state(strip, self.model.state())) {
            Ok(transition) => Some((ids::strip(strip), transition)),
            Err(error) => {
                shellio::owner::say_err(format_args!(
                    "shellio: полоса вкладок {} не обновлена: {error}",
                    ids::strip(strip)
                ));
                None
            }
        }
    }

    /// Список приводится к состоянию модели: набор строк зерна и выбор.
    pub fn show_list(&self) {
        let Some(tree) = &self.tree else {
            return;
        };
        if let Err(error) = tree.list.set_state(self.list_state()) {
            shellio::owner::say_err(format_args!("shellio: список галереи не обновлён: {error}"));
        }
    }

    /// Дерево приводится к состоянию модели: узлы вставок и обмена, раскрытие и выбор.
    pub fn show_tree(&self) {
        let Some(tree) = &self.tree else {
            return;
        };
        if let Err(error) = tree.tree_list.set_state(self.tree_state()) {
            shellio::owner::say_err(format_args!(
                "shellio: дерево галереи не обновлено: {error}"
            ));
        }
    }

    /// Состояние дерева по модели. Узлы строятся один раз на вставки и обмен.
    fn tree_state(&self) -> TreeListState {
        let state = self.model.state();
        let key = (state.tree_inserts, state.tree_swapped);
        let mut cache = self.tree_roots.borrow_mut();
        let roots = match cache.as_ref() {
            Some((cached, roots)) if *cached == key => Rc::clone(roots),
            _ => {
                let roots: Rc<[TreeNode]> = gallery::tree_roots(key.0, key.1)
                    .into_iter()
                    .map(tree_node)
                    .collect();
                *cache = Some((key, Rc::clone(&roots)));
                roots
            }
        };
        TreeListState {
            label: gallery::TREE_LABEL.to_owned(),
            roots,
            expanded: state.tree_expanded.clone(),
            selected: state.tree_selected.clone(),
        }
    }

    /// Такты серии задержки списка по состоянию галереи: действуют со следующего фокуса списка.
    pub fn apply_list_ticks(&self) {
        if let Some(tree) = &self.tree {
            tree.list.set_latency_ticks(self.model.state().list_ticks);
        }
    }

    /// Прокрутка списка: строка `row` у верхнего края.
    pub fn scroll_list(&self, row: u32) {
        if let (Some(tree), Ok(row)) = (&self.tree, usize::try_from(row)) {
            tree.list.scroll_to_row(row);
        }
    }

    /// Созданные виды строк списка и дерева для `ui.stats`.
    pub fn row_views(&self) -> Vec<RowViewsView> {
        self.tree
            .iter()
            .flat_map(|tree| {
                [
                    RowViewsView {
                        component: ids::LIST.to_owned(),
                        created: tree.list.row_views_created(),
                    },
                    RowViewsView {
                        component: ids::TREE.to_owned(),
                        created: tree.tree_list.row_views_created(),
                    },
                ]
            })
            .collect()
    }

    /// Состояние списка по модели. Строки зерна строятся один раз и делятся между состояниями.
    fn list_state(&self) -> ListState {
        let state = self.model.state();
        let seed = state.list_seed;
        let mut cache = self.list_rows.borrow_mut();
        let rows = match cache.as_ref() {
            Some((cached, rows)) if *cached == seed => Rc::clone(rows),
            _ => {
                let rows: Rc<[Row]> = (0..gallery::LIST_ROWS)
                    .map(|index| {
                        Row::new(
                            &gallery::list_row_id(seed, index),
                            &gallery::list_row_text(seed, index),
                        )
                    })
                    .collect();
                *cache = Some((seed, Rc::clone(&rows)));
                rows
            }
        };
        let selected = state
            .list_selected
            .filter(|index| *index < rows.len())
            .map(|index| gallery::list_row_id(seed, index));
        ListState::new(gallery::LIST_LABEL, rows, selected.as_deref())
    }

    /// Маркер показывает поколение `generation` в том же проходе отображения, что и состояние.
    pub fn set_generation(&self, generation: u64) {
        if let Some(tree) = &self.tree {
            tree.marker.set_generation(generation);
            tree.list.set_generation(generation);
        }
        if let Some(page) = &self.calibration {
            page.marker.set_generation(generation);
        }
    }

    /// Ряд задержек клавиш списка: нажатия серии с первой отрисовкой списка в поколении их выбора
    /// и такты `CADisplayLink`. `None` — серии не было.
    pub fn key_latency(&self, monitor: Option<&LoopMonitor>) -> Option<KeyLatencyView> {
        let tree = self.tree.as_ref()?;
        let presses = tree.list.key_presses();
        let ticks = tree.list.ticks();
        if presses.is_empty() && ticks.is_empty() {
            return None;
        }
        let presses = presses
            .into_iter()
            .map(|press| {
                let drawn = press.generation.and_then(|generation| {
                    monitor.and_then(|monitor| monitor.first_draw(tree.list.id(), generation))
                });
                KeyPressView {
                    key: match press.key {
                        ListKey::Up => "up",
                        ListKey::Down => "down",
                        ListKey::Enter => "enter",
                        ListKey::Escape => "escape",
                    }
                    .to_owned(),
                    event: press.event,
                    wake: press.wake,
                    entry: press.entry,
                    handled: press.handled,
                    applied: press.applied,
                    target: press.target,
                    generation: press.generation,
                    drawn: drawn.map(|mark| mark.at),
                    drawn_generation: drawn.map(|mark| mark.generation),
                }
            })
            .collect();
        Some(KeyLatencyView {
            component: ids::LIST.to_owned(),
            presses,
            ticks,
        })
    }

    pub fn focus(&self, window: &MainWindow, focus: GalleryFocus) {
        match (focus, &self.tree) {
            (GalleryFocus::Field, Some(tree)) => {
                if !tree.field.focus() {
                    shellio::owner::say_err("shellio: поле галереи не приняло фокус");
                }
            }
            (GalleryFocus::Tabs, Some(tree)) => {
                let side = tree
                    .strips
                    .iter()
                    .find(|(strip, _)| *strip == GalleryStrip::Side);
                if !side.is_some_and(|(_, view)| view.focus()) {
                    shellio::owner::say_err("shellio: полоса вкладок галереи не приняла фокус");
                }
            }
            (GalleryFocus::List, Some(tree)) => {
                if !tree.list.focus() {
                    shellio::owner::say_err("shellio: список галереи не принял фокус");
                }
            }
            (GalleryFocus::Tree, Some(tree)) => {
                if !tree.tree_list.focus() {
                    shellio::owner::say_err("shellio: дерево галереи не приняло фокус");
                }
            }
            (
                GalleryFocus::Field | GalleryFocus::Tabs | GalleryFocus::List | GalleryFocus::Tree,
                None,
            ) => {}
            (GalleryFocus::Window, _) => {
                window.clear_focus();
            }
        }
    }

    pub fn open_menu(&self) {
        if let Some(tree) = &self.tree {
            tree.menu.open();
        }
    }

    /// Вид приводится к состоянию фикстуры: меню закрыто, поле с её значением, полосы с выбором
    /// фикстуры без анимации прокрутки, фокус у окна. Возвращает смены полос для журнала.
    pub fn reset_view(
        &self,
        window: &MainWindow,
        motion: Motion,
    ) -> Vec<(&'static str, Transition)> {
        let mut transitions = Vec::new();
        if let Some(tree) = &self.tree {
            tree.menu.menu().close();
            for (strip, view) in &tree.strips {
                view.menu().menu().close();
                view.set_motion(Motion::Reduced);
                if let Ok(transition) = view.set_state(strip_state(*strip, self.model.state())) {
                    transitions.push((ids::strip(*strip), transition));
                }
                view.set_motion(motion);
            }
            if let Err(error) = tree.list.set_state(self.list_state()) {
                shellio::owner::say_err(format_args!(
                    "shellio: список галереи не сброшен: {error}"
                ));
            }
            tree.list.scroll_to_row(0);
            tree.list.set_latency_ticks(self.model.state().list_ticks);
            if let Err(error) = tree.tree_list.set_state(self.tree_state()) {
                shellio::owner::say_err(format_args!(
                    "shellio: дерево галереи не сброшено: {error}"
                ));
            }
            let fixture = gallery::field_fixture();
            tree.field.set_state(TextFieldState {
                value: self.model.state().field_value.clone(),
                placeholder: fixture.placeholder,
                editable: fixture.editable,
            });
        }
        window.clear_focus();
        transitions
    }

    /// Узлы галереи для `ui.inspect`.
    pub fn inspect(&self) -> Vec<InspectNode> {
        if let Some(page) = &self.calibration {
            let mut nodes = page.marker.inspect(ids::MARKER, None);
            nodes.extend(page.sheet.inspect(ids::CALIBRATION, None));
            return nodes;
        }
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let mut nodes = tree.marker.inspect(ids::MARKER, None);
        nodes.extend(tree.focus.inspect(ids::SURFACE_FOCUS, None));
        nodes.extend(tree.field.inspect(ids::FIELD, Some(ids::SURFACE_FOCUS)));
        nodes.extend(tree.menu.inspect(ids::MENU, Some(ids::SURFACE_FOCUS)));
        nodes.extend(tree.plain.inspect(ids::SURFACE_PLAIN, None));
        nodes.extend(tree.list.inspect(ids::LIST, Some(ids::SURFACE_PLAIN)));
        nodes.extend(tree.tree_list.inspect(ids::TREE, Some(ids::SURFACE_PLAIN)));
        for (strip, view) in &tree.strips {
            let parent = match gallery::strip_fixture(*strip).host {
                StripHost::FocusSurface => Some(ids::SURFACE_FOCUS),
                StripHost::Window => None,
            };
            nodes.extend(view.inspect(ids::strip(*strip), parent));
        }
        nodes
    }

    /// Номер текущего дерева.
    pub fn tree_number(&self) -> Option<u64> {
        self.tree
            .as_ref()
            .map(|tree| tree.number)
            .or_else(|| self.calibration.as_ref().map(|page| page.number))
    }

    /// Прежние деревья для `ui.stats`.
    pub fn retired_trees(&self) -> Vec<GalleryTreeView> {
        self.retired
            .iter()
            .map(|retired| GalleryTreeView {
                tree: retired.number,
                views: retired.watch.len(),
                alive: retired.watch.alive(),
                alive_classes: retired
                    .watch
                    .alive_classes()
                    .into_iter()
                    .map(|(class, alive)| ClassAliveView { class, alive })
                    .collect(),
            })
            .collect()
    }
}

fn tree_node(fixture: TreeFixture) -> TreeNode {
    TreeNode {
        id: fixture.id,
        text: fixture.text,
        children: fixture.children.into_iter().map(tree_node).collect(),
    }
}

fn frame(rect: gallery::Rect) -> Frame {
    Frame::new(rect.x, rect.y, rect.width, rect.height)
}

/// Действия панели в меню «…» полос галереи.
fn panel_actions() -> Vec<MenuEntry> {
    vec![
        MenuEntry::Item(MenuItem::new("panel.rename", "Переименовать панель")),
        MenuEntry::Item(MenuItem::new("panel.close", "Закрыть панель")),
    ]
}

/// Состояние полосы `strip` по фикстуре и выбору модели.
fn strip_state(strip: GalleryStrip, state: &GalleryState) -> TabStripState {
    let fixture = gallery::strip_fixture(strip);
    let zone = match fixture.zone {
        StripZone::Side => ZoneSide::Side,
        StripZone::Center => ZoneSide::Center,
    };
    let tabs: Vec<Tab> = fixture
        .tabs
        .iter()
        .map(|tab| Tab::new(tab.id, tab.title, tab.kind, tab.icon, status(tab.status)))
        .collect();
    let selected = tabs.get(state.selected(strip)).map(|tab| tab.id.clone());
    TabStripState {
        tabs,
        selected,
        payload: Some(fixture.payload.to_owned()),
        focused: fixture.focused,
        mode: KeyMode::Navigation,
        actions: panel_actions(),
        ..TabStripState::new(fixture.label, Presentation::new(zone))
    }
}

const fn status(status: StripStatus) -> TabStatus {
    match status {
        StripStatus::None => TabStatus::None,
        StripStatus::Running => TabStatus::Running,
        StripStatus::Waiting => TabStatus::Waiting,
        StripStatus::Error => TabStatus::Error,
        StripStatus::Done => TabStatus::Done,
        StripStatus::Idle => TabStatus::Idle,
        StripStatus::Exited => TabStatus::Exited,
    }
}

fn tab_action(intent: TabIntent) -> TabAction {
    match intent {
        TabIntent::Select { id } => TabAction::Select(id),
        TabIntent::Close { id } => TabAction::Close(id),
        TabIntent::Split(axis) => TabAction::Split(axis.name()),
        TabIntent::Zoom => TabAction::Zoom,
        TabIntent::Restore => TabAction::Restore,
        TabIntent::Menu { id } => TabAction::Menu(id),
    }
}

fn menu_entries(items: &[MenuFixtureItem]) -> Vec<MenuEntry> {
    items
        .iter()
        .map(|item| match *item {
            MenuFixtureItem::Item {
                id,
                title,
                key,
                enabled,
                checked,
            } => MenuEntry::Item(MenuItem {
                key: key.map(MenuKey::command),
                enabled,
                checked,
                ..MenuItem::new(id, title)
            }),
            MenuFixtureItem::Separator { id } => MenuEntry::Separator { id: id.to_owned() },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use shellio_appkit::check_menu_ids;

    use super::*;

    #[test]
    fn menu_fixture_becomes_a_valid_menu() {
        let entries = menu_entries(&gallery::menu_fixture().items);
        assert_eq!(check_menu_ids(&entries), Ok(()));
        assert_eq!(entries.len(), gallery::menu_fixture().items.len());
        let MenuEntry::Item(copy) = &entries[0] else {
            panic!("первый пункт — разделитель: {entries:?}");
        };
        assert_eq!(copy.id, "copy");
        assert_eq!(copy.key, Some(MenuKey::command("c")));
        assert!(matches!(&entries[3], MenuEntry::Separator { id } if id == "sep"));
    }
}
