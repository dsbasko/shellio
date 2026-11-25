//! Адаптер раскладки обычного окна на главном потоке (ADR-0009, «AppKit-адаптер»): зеркало и
//! размер окна → сцена [`shellio::scene`] → разница → операции над компонентами `shellio-appkit`.
//!
//! Корень — горизонтальный [`SplitView`]: колонка навигатора, плитка левой зоны, столбец центра,
//! правая зона. Столбец центра — вертикальный сплит, центр над нижней зоной. Части каждой зоны —
//! сплит вдоль её оси, в элементе части стоит [`Plate`]. Контейнер панели [`PanelView`] создаётся
//! один раз на `PanelId`: смена вкладки его прячет и показывает, перенос и `zoom` переставляют
//! родителя, уход окна к другой рабочей области паркует его скрытым в корне раскладки. Внутри
//! контейнера фабрика плагина строит тело один раз, пока панель в `ready` (ADR-0008); что тело
//! получает по сцене, решает [`shellio::bodies`]. Развёрнутая панель лежит на отдельной плитке
//! поверх корня, деления под ней не меняются. Перетаскиваемая вкладка подсвечивает цель на её
//! плитке, отпускание — одна команда переноса. На месте тела меньше `min_size` панели и на месте
//! тела панели не в `ready` стоит заглушка [`ExpandStub`]: одна на место тела, контейнер панели под
//! ней скрыт и жив.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use shellio::bodies::{self, Bodies, Build, SharedRequests, Shell, Ticket};
use shellio::plugins::{self, Factories};
use shellio::scene::{
    self, Bench, Host, MenuRow, Op, PanelMenuItem, PartKey, PlateRef, PlateSlot, Scene, SplitId,
    SplitPlan,
};
use shellio_appkit::{
    App, EmptyNote, ExpandStub, Frame, Insets, MainWindow, Menu, MenuEntry, MenuIntent, MenuItem,
    Pane, PanelView, Plate, Presentation, SplitDirection, SplitDrag, SplitSlot, SplitSpec,
    SplitView, StubCard, StubIntent, Tab, TabDrag, TabIntent, TabStatus, TabStripState, ZoneSide,
};
use shellio_control::{ComponentState, ComponentView, PanelNodeView, PartNodeView};
use shellio_core::{Command, PanelId, PluginId, State, layout_metrics};
use shellio_layout::{Point, Rect, Viewport, Zone, ZoneMap};
use shellio_plugin_api::Registry;
use shellio_plugin_view::{Body, Done, PanelRequest, PluginView};
use shellio_theme::{AppTheme, KeyMode, Motion};

use crate::window_ui;

/// Id узлов `ui.inspect` раскладки.
const ROOT_ID: &str = "layout";
const ROOT_SPLIT_ID: &str = "layout.split.root";
const COLUMN_SPLIT_ID: &str = "layout.split.column";
const NAVIGATOR_ID: &str = "layout.navigator";
const ZOOM_ID: &str = "layout.zoom";
const NOTE_ID: &str = "layout.note";
const PICKER_ID: &str = "layout.picker";
/// Заголовок меню выбора для новой части (ADR-0031).
const PICKER_TITLE: &str = "Открыть в новой части";

/// Действие в раскладке, которое становится командой ядра.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PanelAction {
    /// Щелчок по вкладке — `panel.select`.
    Select(PanelId),
    /// Кнопка закрытия вкладки — `panel.close`.
    Close(PanelId),
    /// Пункт меню «…» плитки — перенос её активной вкладки или закрытие.
    Menu(PlateRef, PanelMenuItem),
    /// Вкладку тянут: подсветка цели под точкой содержимого окна.
    Drag { panel: PanelId, point: Point },
    /// Вкладку отпустили: перенос в цель под точкой или отмена.
    Drop { panel: PanelId, point: Point },
    /// Щелчок в тело панели — `panel.focus`.
    Focus(PanelId),
    /// «Разделить» плитки — `layout.split` её активной вкладки.
    Split(PlateRef),
    /// «Развернуть панель» или «Вернуть раскладку» плитки — `layout.zoom`.
    Zoom(PlateRef),
    /// Пункт выбора «Открыть в новой части» — `layout.split` панели с `open`.
    Open {
        panel: PanelId,
        plugin: &'static str,
    },
    /// Кнопка заглушки «Развернуть» на месте тела — `layout.zoom` её панели.
    Expand(Host),
    /// Щелчок в тело заглушки мимо кнопки — `panel.focus` её панели.
    StubPress(Host),
}

/// Запрос тела панели хосту: номер запроса, по которому вернётся исход, и сам запрос.
type OnRequest = Rc<dyn Fn(Ticket, PanelRequest)>;

/// Раскладка обычного окна.
pub struct LayoutUi {
    // Порядок полей — порядок освобождения: сначала заглушки, тела, контейнеры панелей и плитки,
    // затем сплиты и контейнер.
    /// Заглушки по месту тела: создаются при первой нужде и переиспользуются.
    stubs: BTreeMap<Host, ExpandStub>,
    /// Тела плагинов в контейнерах панелей.
    bodies: Bodies,
    /// Контейнеры панелей: показанной рабочей области и припаркованные.
    views: BTreeMap<PanelId, PanelView>,
    plates: BTreeMap<PartKey, Plate>,
    zoom: Option<Plate>,
    note: EmptyNote,
    /// Выбор «Открыть в новой части»: одно меню на окно, открывается у кнопки «Разделить».
    /// `None` — меню не построено: отказ `split_needs_panel` идёт в строку статуса.
    picker: Option<Menu>,
    /// Панель, которую делит открытый выбор.
    picker_panel: Rc<Cell<Option<PanelId>>>,
    /// Плитка с подсветкой цели и перетаскиваемая панель.
    drop_at: Cell<Option<PartKey>>,
    dragging: Cell<Option<PanelId>>,
    zones: ZoneMap<SplitView>,
    column: SplitView,
    root: SplitView,
    container: Pane,
    scene: Option<Scene>,
    theme: AppTheme,
    motion: Motion,
    on_panel: Rc<dyn Fn(PanelAction)>,
    on_request: OnRequest,
    /// Реестр плагинов процесса: `min_size` панелей сцены, модели и значки плагинов.
    registry: Arc<Registry>,
    /// Фабрики тел по plugin id; `None` — список регистраций неверен, тела не строятся.
    factories: Option<Factories>,
    /// Окно было видимо при последней доставке.
    visible: bool,
}

impl LayoutUi {
    /// Пустая раскладка в содержимом окна `window`. `on_drag` получает конец перетаскивания
    /// разделителя, `on_panel` — щелчки по вкладкам и телам панелей, `on_request` — запросы тел
    /// плагинов; все вызываются на главном потоке изнутри AppKit и не должны синхронно трогать
    /// раскладку.
    pub fn new(
        app: &App,
        window: &MainWindow,
        on_drag: impl Fn(SplitId, SplitDrag) + 'static,
        on_panel: impl Fn(PanelAction) + 'static,
        on_request: impl Fn(Ticket, PanelRequest) + 'static,
    ) -> LayoutUi {
        let selection = window.selection();
        let theme = selection.theme();
        let on_drag: Rc<dyn Fn(SplitId, SplitDrag)> = Rc::new(on_drag);
        let split = |id: SplitId, direction: SplitDirection| {
            let on_drag = Rc::clone(&on_drag);
            SplitView::new(app, direction, move |drag| on_drag(id, drag))
        };
        let container = Pane::new(app);
        let root = split(SplitId::Root, SplitDirection::Columns);
        let column = split(SplitId::Column, SplitDirection::Rows);
        let zones = ZoneMap {
            left: split(SplitId::Zone(Zone::Left), SplitDirection::Rows),
            right: split(SplitId::Zone(Zone::Right), SplitDirection::Rows),
            center: split(SplitId::Zone(Zone::Center), SplitDirection::Columns),
            bottom: split(SplitId::Zone(Zone::Bottom), SplitDirection::Columns),
        };
        let note = EmptyNote::new(app, &theme, scene::NO_WORKBENCH);
        let (width, height) = window.content_size();
        // Без верного списка плагинов владелец не запустится и скажет причину сам: окну до его
        // отказа хватает пустого реестра без фабрик.
        let registry = plugins::shared_registry().unwrap_or_default();
        let factories = plugins::factories(&registry).ok();
        let empty = Scene::of(
            &State::default(),
            &registry,
            scene::viewport_of(width, height),
            layout_metrics(),
        );
        window.place(&container, frame(empty.area));
        container.place(&root, Insets::ZERO);
        root.apply(&spec(&empty.splits().root));
        root.with_pane(2, |pane| pane.place(&column, Insets::ZERO));
        column.apply(&spec(&empty.splits().column));
        root.with_pane(1, |pane| pane.place(&zones.left, Insets::ZERO));
        root.with_pane(3, |pane| pane.place(&zones.right, Insets::ZERO));
        column.with_pane(0, |pane| pane.place(&zones.center, Insets::ZERO));
        column.with_pane(1, |pane| pane.place(&zones.bottom, Insets::ZERO));
        container.place(&note, Insets::ZERO);
        let on_panel: Rc<dyn Fn(PanelAction)> = Rc::new(on_panel);
        let picker_panel = Rc::new(Cell::new(None));
        let picker = picker_menu(
            app,
            &registry,
            Rc::clone(&on_panel),
            Rc::clone(&picker_panel),
        );
        LayoutUi {
            stubs: BTreeMap::new(),
            bodies: Bodies::default(),
            views: BTreeMap::new(),
            plates: BTreeMap::new(),
            zoom: None,
            note,
            picker,
            picker_panel,
            drop_at: Cell::new(None),
            dragging: Cell::new(None),
            zones,
            column,
            root,
            container,
            scene: None,
            theme,
            motion: selection.motion,
            on_panel,
            on_request: Rc::new(on_request),
            registry,
            factories,
            visible: false,
        }
    }

    /// Обратный вызов исхода запроса `ticket`, если тело его ещё ждёт.
    pub fn take_done(&self, ticket: Ticket) -> Option<Done> {
        bodies::settle_request(&self.bodies.requests(), ticket)
    }

    /// Сколько тел построено в контейнере каждой панели: живые и припаркованные контейнеры.
    pub fn bodies_built(&self) -> Vec<(PanelId, u64)> {
        self.bodies.built()
    }

    /// Открывает выбор «Открыть в новой части» у кнопки «Разделить» плитки панели `panel`: пункты
    /// и их доступность — по состоянию `state` на момент открытия. `false` — панель не стоит в
    /// плитке: выбор не открыт.
    pub fn open_picker(&self, state: &State, panel: PanelId) -> bool {
        let (Some(plate), Some(picker)) = (self.plate_of(panel), &self.picker) else {
            return false;
        };
        let items = scene::picker_items(state, &self.registry, Some(panel));
        if let Err(error) = picker.set_entries(picker_entries(&items)) {
            shellio::owner::say_err(format_args!(
                "shellio: выбор новой части не обновлён: {error}"
            ));
        }
        self.picker_panel.set(Some(panel));
        plate.pop_up_choice(picker);
        true
    }

    /// Плитка, в которой стоит вид панели `panel`.
    fn plate_of(&self, panel: PanelId) -> Option<&Plate> {
        let bench = self.scene.as_ref()?.bench.as_ref()?;
        match bench.views.get(&panel)?.host {
            Host::Part(key) => self.plates.get(&key),
            Host::Zoom => self.zoom.as_ref(),
            Host::Navigator => None,
        }
    }

    /// Плитка, у которой открыт или был открыт выбор новой части.
    fn picker_plate(&self) -> Option<&Plate> {
        self.picker_panel
            .get()
            .and_then(|panel| self.plate_of(panel))
    }

    /// Последняя применённая сцена.
    pub fn scene(&self) -> Option<&Scene> {
        self.scene.as_ref()
    }

    /// Вкладку панели `panel` тянут через точку `point`: подсветка цели на её плитке, вне цели
    /// подсветки нет.
    pub fn drag_over(&self, panel: PanelId, point: Point) {
        self.dragging.set(Some(panel));
        let plan = self
            .scene
            .as_ref()
            .and_then(|scene| scene::drop_plan(scene, panel, point));
        self.show_drop(plan.map(|plan| (plan.part, plan.highlight)));
    }

    /// Вкладку панели `panel` отпустили в точке `point`: подсветка уходит, ответ — команда
    /// переноса в цель; `None` — отмена.
    pub fn drop_at(&self, panel: PanelId, point: Point) -> Option<Command> {
        self.dragging.set(None);
        self.show_drop(None);
        let scene = self.scene.as_ref()?;
        scene::drop_plan(scene, panel, point).map(|plan| plan.command)
    }

    /// Подсветка цели перетаскивания на плитке `target`; прежняя подсветка снимается.
    fn show_drop(&self, target: Option<(PartKey, Rect)>) {
        let key = target.map(|(key, _)| key);
        if let Some(previous) = self.drop_at.replace(key)
            && Some(previous) != key
            && let Some(plate) = self.plates.get(&previous)
        {
            plate.set_drop(None);
        }
        if let Some((key, rect)) = target
            && let Some(plate) = self.plates.get(&key)
        {
            plate.set_drop(Some(frame(rect)));
        }
    }

    /// Показывает состояние `state` (`None` — снимка ещё нет) при размере содержимого `viewport`;
    /// `visible` — окно видимо: тела скрытого окна неактивны.
    pub fn render(&mut self, app: &App, state: Option<&State>, viewport: Viewport, visible: bool) {
        let empty = State::default();
        let state = state.unwrap_or(&empty);
        let next = Scene::of(state, &self.registry, viewport, layout_metrics());
        let before = self.scene.as_ref().and_then(|scene| scene.bench.as_ref());
        let ops = scene::diff(before, next.bench.as_ref());
        if let Some(bench) = &next.bench {
            for op in &ops {
                if let Op::AddPlate(key) = *op {
                    self.add_plate(app, key, bench);
                }
            }
            // Контейнер панели, которую окно уже показывало, припаркован и встаёт на место без
            // пересоздания; новый создаётся здесь.
            for (&panel, slot) in &bench.views {
                if !self.views.contains_key(&panel) {
                    let on_panel = Rc::clone(&self.on_panel);
                    let view = PanelView::new(app, &self.theme, &slot.title, move || {
                        on_panel(PanelAction::Focus(panel));
                    });
                    self.views.insert(panel, view);
                }
            }
        }
        // Перестановка контейнера вместе с родителем сбрасывает first responder окна: кто держал
        // ввод, запоминается до неё.
        let held = self
            .views
            .iter()
            .find(|(_, view)| view.holds_focus())
            .map(|(&panel, _)| panel);
        self.place(app, &next);
        let gone = self.dragging.get().is_some_and(|panel| {
            next.bench
                .as_ref()
                .is_none_or(|bench| !bench.views.contains_key(&panel))
        });
        if gone {
            self.dragging.set(None);
            self.show_drop(None);
        }
        for op in &ops {
            match *op {
                Op::Show(panel) => self.set_view_hidden(panel, false),
                Op::Hide(panel) => self.set_view_hidden(panel, true),
                Op::AddPlate(_) | Op::RemovePlate(_) => {}
            }
        }
        // Тела получают сцену, когда контейнеры уже стоят в окне: `focus_preferred` переводит ввод в
        // вид внутри окна. Контейнер панели, ушедшей из показанной рабочей области, паркуется или
        // освобождается здесь, до удаления плитки, в которой он стоял.
        let mut hosts = Hosts {
            app,
            theme: &self.theme,
            views: &mut self.views,
            factories: self.factories.as_ref(),
            parking: &self.container,
            requests: self.bodies.requests(),
            on_request: &self.on_request,
        };
        self.bodies.sync(
            &bodies::Frame {
                state,
                registry: &self.registry,
                bench: next.bench.as_ref(),
                visible,
                held,
            },
            &mut hosts,
        );
        for op in ops {
            if let Op::RemovePlate(key) = op {
                self.stubs.remove(&Host::Part(key));
                self.plates.remove(&key);
                if self.drop_at.get() == Some(key) {
                    self.drop_at.set(None);
                }
            }
        }
        self.scene = Some(next);
        self.visible = visible;
    }

    /// Новая ячейка темы и движение.
    pub fn set_theme(&mut self, theme: &AppTheme, motion: Motion) {
        self.theme = *theme;
        self.motion = motion;
        for plate in self.plates.values().chain(self.zoom.as_ref()) {
            plate.set_theme(theme);
            plate.set_motion(motion);
        }
        for view in self.views.values() {
            view.set_theme(theme);
        }
        for body in self.bodies.views_mut() {
            body.set_theme(theme);
        }
        for stub in self.stubs.values() {
            stub.set_theme(theme);
        }
        self.note.set_theme(theme);
    }

    /// Узлы `ui.inspect` раскладки.
    pub fn inspect(&self) -> Vec<ComponentView> {
        let mut nodes = vec![window_ui::component(
            self.container.inspect(ROOT_ID, "Layout", None),
        )];
        let push = |nodes: &mut Vec<ComponentView>, list: Vec<shellio_appkit::InspectNode>| {
            nodes.extend(list.into_iter().map(window_ui::component));
        };
        let bench = self.scene.as_ref().and_then(|scene| scene.bench.as_ref());
        // Под развёрнутой панелью плитки частей скрыты: ключи сверки несёт только она.
        let zoomed = bench.is_some_and(|bench| bench.zoomed.is_some());
        // Контейнеры зон для сверки: кадр каждого — объединение кадров плиток его зоны.
        let mut zones: Vec<(Zone, ComponentView, Option<shellio_control::Rect>)> = Vec::new();
        push(&mut nodes, self.root.inspect(ROOT_SPLIT_ID, Some(ROOT_ID)));
        push(
            &mut nodes,
            self.root
                .with_pane(0, |pane| {
                    vec![pane.inspect(NAVIGATOR_ID, "NavigatorColumn", Some(ROOT_SPLIT_ID))]
                })
                .unwrap_or_default(),
        );
        push(
            &mut nodes,
            self.column.inspect(COLUMN_SPLIT_ID, Some(ROOT_SPLIT_ID)),
        );
        for zone in Zone::ALL {
            let parent = match zone {
                Zone::Left | Zone::Right => ROOT_SPLIT_ID,
                Zone::Center | Zone::Bottom => COLUMN_SPLIT_ID,
            };
            let id = split_id(zone);
            let shown = !zoomed
                && bench.is_some_and(|bench| bench.plates.keys().any(|key| key.zone == zone));
            let list = self.zones.get(zone).inspect(&id, Some(parent));
            if let Some(split) = list.iter().find(|node| node.id == id)
                && shown
                && split.facts.visible
            {
                zones.push((zone, window_ui::component(zone_node(split, zone)), None));
            }
            push(&mut nodes, list);
        }
        if let Some(bench) = bench {
            let mut plates = Vec::new();
            for (key, plate) in &self.plates {
                let Some(slot) = bench.plates.get(key) else {
                    continue;
                };
                let id = part_id(*key);
                let base = host_ck(Host::Part(*key), zoomed);
                let zone = zones.iter_mut().find(|(zone, ..)| *zone == key.zone);
                let parent = match &zone {
                    Some((_, zone, _)) => zone.id.clone(),
                    None => split_id(key.zone),
                };
                let mut list = keyed(plate.inspect(&id, Some(&parent)), &id, base.as_deref());
                if let Some(first) = list.first_mut() {
                    let view = part_view(key.zone, key.index, slot, false);
                    let mut component = window_ui::component(first.clone());
                    component.state = Some(ComponentState::Part(Box::new(view)));
                    if let Some((.., frame)) = zone {
                        *frame = Some(match *frame {
                            Some(frame) => bounds(frame, component.frame),
                            None => component.frame,
                        });
                    }
                    plates.push(component);
                    list.remove(0);
                }
                plates.extend(list.into_iter().map(window_ui::component));
            }
            // Зона без кадра плитки контейнера не получает.
            nodes.extend(zones.into_iter().filter_map(|(_, mut zone, frame)| {
                zone.frame = frame?;
                zone.object = 0;
                Some(zone)
            }));
            nodes.extend(plates);
            if let (Some(zoom), Some(zoomed)) = (&self.zoom, &bench.zoomed) {
                let base = host_ck(Host::Zoom, true);
                let mut list = keyed(
                    zoom.inspect(ZOOM_ID, Some(ROOT_ID)),
                    ZOOM_ID,
                    base.as_deref(),
                );
                if !list.is_empty() {
                    let zone = zoom_zone(&list[0]).map(window_ui::component);
                    let mut component = window_ui::component(list.remove(0));
                    if let Some(zone) = zone {
                        component.parent = Some(zone.id.clone());
                        nodes.push(zone);
                    }
                    let slot = PlateSlot {
                        tabs: vec![zoomed.active],
                        active: zoomed.active,
                        frame: Some(zoomed.clone()),
                        focused: bench.focus == Some(zoomed.active),
                    };
                    component.state = Some(ComponentState::Part(Box::new(part_view(
                        Zone::Center,
                        0,
                        &slot,
                        true,
                    ))));
                    nodes.push(component);
                }
                nodes.extend(list.into_iter().map(window_ui::component));
            }
            for (panel, view) in &self.views {
                let Some(slot) = bench.views.get(panel) else {
                    continue;
                };
                let parent = host_id(slot.host);
                let node = view.inspect(&panel_id(*panel), Some(&parent));
                let host =
                    host_ck(slot.host, zoomed).filter(|_| slot.shown() && node.facts.visible);
                // Своей плитки у колонки навигатора нет: место части — контейнер её панели, а
                // тело — производный узел того же кадра.
                let (node, body) = match (slot.host, host) {
                    (Host::Navigator, Some(host)) => {
                        let body = column_body(&node, &host);
                        (node.with_root_ck(host), Some(body))
                    }
                    (_, Some(host)) => (node.with_ck(format!("{host}.body")), None),
                    (_, None) => (node, None),
                };
                let mut component = window_ui::component(node);
                component.state = Some(ComponentState::Panel(Box::new(PanelNodeView {
                    panel: *panel,
                    component: view.id().get(),
                    plugin: slot.plugin.clone(),
                    title: slot.title.clone(),
                    shown: slot.shown(),
                    zone: slot.geometry.map(|geometry| geometry.zone),
                    size: slot.geometry.map(|geometry| geometry.size),
                    status: slot.status,
                    active: self.bodies.active(*panel),
                    focused: bench.focus == Some(*panel),
                    variant: self.bodies.variant(*panel).map(str::to_owned),
                })));
                nodes.push(component);
                nodes.extend(body.map(window_ui::component));
                if let Some(body) = self.bodies.view(*panel) {
                    nodes.extend(body_nodes(*panel, body));
                }
            }
            for host in bench.stubs().keys() {
                if let Some(stub) = self.stubs.get(host) {
                    let parent = host_id(*host);
                    let id = format!("{parent}.stub");
                    let base = host_ck(*host, zoomed).map(|host| format!("{host}.body.stub"));
                    let list = stub.inspect(&id, Some(&parent));
                    nodes.extend(list.into_iter().map(|node| {
                        let ck = base
                            .as_deref()
                            .filter(|_| node.facts.visible)
                            .and_then(|base| stub_ck(base, &node.id[id.len()..]));
                        let mut component = window_ui::component(node);
                        component.ck = ck;
                        component
                    }));
                }
            }
        }
        if self.scene.as_ref().and_then(Scene::note).is_some() {
            // Сообщение стоит над областью зон, а не в части зоны: ключ `workbench.empty`.
            nodes.push(window_ui::component(
                self.note
                    .inspect(NOTE_ID, Some(ROOT_ID))
                    .with_root_ck(NOTE_CK),
            ));
        }
        if let Some(picker) = &self.picker {
            let node = match self.picker_plate() {
                Some(plate) => picker.inspect_at(PICKER_ID, Some(ROOT_ID), plate.strip()),
                None => picker.inspect_at(PICKER_ID, Some(ROOT_ID), &self.container),
            };
            // Выбор — меню AppKit: ключ несёт узел его состояния, кадр — место привязки.
            let node = if node.facts.visible {
                node.with_ck(PICKER_CK)
            } else {
                node
            };
            nodes.push(window_ui::component(node));
        }
        with_dividers(nodes)
    }

    fn add_plate(&mut self, app: &App, key: PartKey, bench: &Bench) {
        let Some(slot) = bench.plates.get(&key) else {
            return;
        };
        let count = bench.layout.zones.get(key.zone).parts.len();
        let state = strip_state(
            &self.registry,
            key.zone,
            key.index,
            count,
            slot,
            bench,
            false,
        );
        let intent = self.tab_intent(PlateRef::Part(key));
        match Plate::new(app, &self.theme, self.motion, state, intent) {
            Ok(plate) => {
                plate.set_drag_handler(self.tab_drag());
                self.plates.insert(key, plate);
            }
            Err(error) => shellio::owner::say_err(format_args!(
                "shellio: плитка части {key:?} не построена: {error:?}"
            )),
        }
    }

    /// Намерения строки вкладок плитки `plate`: щелчок по вкладке — `panel.select`, её кнопка
    /// закрытия — `panel.close`, «Разделить», «Развернуть панель» / «Вернуть раскладку» и пункты
    /// меню «…» — команда с явной целью этой плитки. Плитка адресуется местом: панель берётся из
    /// сцены в момент действия.
    fn tab_intent(&self, plate: PlateRef) -> impl Fn(TabIntent) + 'static {
        let on_panel = Rc::clone(&self.on_panel);
        move |intent| match intent {
            TabIntent::Select { id } => {
                if let Ok(panel) = id.parse::<PanelId>() {
                    on_panel(PanelAction::Select(panel));
                }
            }
            TabIntent::Close { id } => {
                if let Ok(panel) = id.parse::<PanelId>() {
                    on_panel(PanelAction::Close(panel));
                }
            }
            TabIntent::Split(_) => on_panel(PanelAction::Split(plate)),
            TabIntent::Zoom | TabIntent::Restore => on_panel(PanelAction::Zoom(plate)),
            TabIntent::Menu { id } => {
                if let Some(item) = PanelMenuItem::of_id(&id) {
                    on_panel(PanelAction::Menu(plate, item));
                }
            }
        }
    }

    /// Перетаскивание вкладок строки: движения и отпускание с панелью вкладки.
    fn tab_drag(&self) -> impl Fn(TabDrag) + 'static {
        let on_panel = Rc::clone(&self.on_panel);
        move |drag| {
            let (id, point, dropped) = match drag {
                TabDrag::Moved { id, point } => (id, point, false),
                TabDrag::Dropped { id, point } => (id, point, true),
            };
            let Ok(panel) = id.parse::<PanelId>() else {
                return;
            };
            let point = Point {
                x: point.x,
                y: point.y,
            };
            on_panel(if dropped {
                PanelAction::Drop { panel, point }
            } else {
                PanelAction::Drag { panel, point }
            });
        }
    }

    fn set_view_hidden(&self, panel: PanelId, hidden: bool) {
        if let Some(view) = self.views.get(&panel) {
            view.set_hidden(hidden);
        }
    }

    /// Сплиты, плитки, виды панелей и сообщение по сцене `next`.
    fn place(&mut self, app: &App, next: &Scene) {
        let splits = next.splits();
        let area = next.area;
        self.root.apply(&spec(&splits.root));
        self.column.apply(&spec(&splits.column));
        for zone in Zone::ALL {
            self.zones.get(zone).apply(&spec(splits.zones.get(zone)));
        }
        let bench = next.bench.as_ref();
        let zoomed = bench.and_then(|bench| bench.zoomed.as_ref());
        self.root.set_hidden(bench.is_none() || zoomed.is_some());

        if let Some(bench) = bench {
            for (key, slot) in &bench.plates {
                let Some(plate) = self.plates.get(key) else {
                    continue;
                };
                let pane = splits.part_panes.get(key).copied();
                let insets = match (&slot.frame, pane) {
                    (Some(frame), Some(pane)) => to_insets(scene::insets(frame.frame, pane)),
                    _ => Insets::ZERO,
                };
                self.zones
                    .get(key.zone)
                    .with_pane(key.index, |pane| pane.place(plate, insets));
                if let Some(frame) = &slot.frame {
                    plate.set_strip_frame(self::frame(scene::relative(
                        frame.tab_strip,
                        frame.frame,
                    )));
                }
                let count = bench.layout.zones.get(key.zone).parts.len();
                let state = strip_state(
                    &self.registry,
                    key.zone,
                    key.index,
                    count,
                    slot,
                    bench,
                    false,
                );
                if let Err(error) = plate.set_state(state) {
                    shellio::owner::say_err(format_args!(
                        "shellio: строка вкладок части {key:?} не обновлена: {error:?}"
                    ));
                }
            }
        }

        match (bench, zoomed) {
            (Some(bench), Some(zoomed)) => {
                let slot = PlateSlot {
                    tabs: vec![zoomed.active],
                    active: zoomed.active,
                    frame: Some(zoomed.clone()),
                    focused: bench.focus == Some(zoomed.active),
                };
                let state = strip_state(&self.registry, Zone::Center, 0, 1, &slot, bench, true);
                if self.zoom.is_none() {
                    let intent = self.tab_intent(PlateRef::Zoom);
                    match Plate::new(app, &self.theme, self.motion, state.clone(), intent) {
                        Ok(plate) => {
                            plate.set_drag_handler(self.tab_drag());
                            self.zoom = Some(plate);
                        }
                        Err(error) => shellio::owner::say_err(format_args!(
                            "shellio: плитка развёрнутой панели не построена: {error:?}"
                        )),
                    }
                }
                if let Some(zoom) = &self.zoom {
                    self.container
                        .place(zoom, to_insets(scene::insets(zoomed.frame, area)));
                    zoom.set_strip_frame(frame(scene::relative(zoomed.tab_strip, zoomed.frame)));
                    if let Err(error) = zoom.set_state(state) {
                        shellio::owner::say_err(format_args!(
                            "shellio: строка вкладок развёрнутой панели не обновлена: {error:?}"
                        ));
                    }
                    zoom.set_hidden(false);
                }
            }
            _ => {
                if let Some(zoom) = &self.zoom {
                    zoom.set_hidden(true);
                }
            }
        }

        if let Some(bench) = bench {
            for (panel, slot) in &bench.views {
                let Some(view) = self.views.get(panel) else {
                    continue;
                };
                view.set_title(&slot.title);
                view.set_flat(slot.host == Host::Navigator);
                match slot.host {
                    Host::Navigator => {
                        let insets = slot
                            .body
                            .zip(splits.root_panes.first().copied())
                            .map_or(Insets::ZERO, |(body, pane)| {
                                to_insets(scene::insets(body, pane))
                            });
                        self.root.with_pane(0, |pane| pane.place(view, insets));
                    }
                    Host::Part(key) => {
                        let Some(plate) = self.plates.get(&key) else {
                            continue;
                        };
                        let body = bench
                            .plates
                            .get(&key)
                            .and_then(|plate| plate.frame.as_ref())
                            .zip(slot.body)
                            .map_or(Frame::new(0.0, 0.0, 0.0, 0.0), |(plate, body)| {
                                frame(scene::relative(body, plate.frame))
                            });
                        plate.place_body(view, body);
                    }
                    Host::Zoom => {
                        if let (Some(zoom), Some(zoomed), Some(body)) =
                            (&self.zoom, zoomed, slot.body)
                        {
                            zoom.place_body(view, frame(scene::relative(body, zoomed.frame)));
                        }
                    }
                }
            }
        }

        self.place_stubs(app, next, &splits);

        match next.note() {
            Some((text, rect)) => {
                self.note.set_text(text);
                self.container
                    .place(&self.note, to_insets(scene::insets(rect, area)));
                self.note.set_hidden(false);
            }
            None => self.note.set_hidden(true),
        }
    }
}

impl LayoutUi {
    /// Заглушки по сцене `next`: на месте тела меньше `min_size` панели и тела панели не в `ready`
    /// — заглушка этого места поверх скрытого контейнера, остальные заглушки скрыты.
    fn place_stubs(&mut self, app: &App, next: &Scene, splits: &scene::Splits) {
        let bench = next.bench.as_ref();
        let stubs = bench.map(Bench::stubs).unwrap_or_default();
        for (&host, stub) in &stubs {
            let card = StubCard {
                title: stub.title.clone(),
                expandable: stub.expandable,
                status: stub.status.clone(),
            };
            if !self.stubs.contains_key(&host) {
                let on_panel = Rc::clone(&self.on_panel);
                let view = ExpandStub::new(app, &self.theme, card.clone(), move |intent| {
                    on_panel(match intent {
                        StubIntent::Expand => PanelAction::Expand(host),
                        StubIntent::Press => PanelAction::StubPress(host),
                    });
                });
                self.stubs.insert(host, view);
            }
            let Some(view) = self.stubs.get(&host) else {
                continue;
            };
            view.set_card(card);
            view.set_flat(host == Host::Navigator);
            match host {
                Host::Navigator => {
                    let insets = splits.root_panes.first().map_or(Insets::ZERO, |pane| {
                        to_insets(scene::insets(stub.body, *pane))
                    });
                    self.root.with_pane(0, |pane| pane.place(view, insets));
                }
                Host::Part(key) => {
                    let plate_frame = bench
                        .and_then(|bench| bench.plates.get(&key))
                        .and_then(|slot| slot.frame.as_ref());
                    if let (Some(plate), Some(plate_frame)) = (self.plates.get(&key), plate_frame) {
                        plate
                            .place_body(view, frame(scene::relative(stub.body, plate_frame.frame)));
                    }
                }
                Host::Zoom => {
                    let zoomed = bench.and_then(|bench| bench.zoomed.as_ref());
                    if let (Some(zoom), Some(zoomed)) = (&self.zoom, zoomed) {
                        zoom.place_body(view, frame(scene::relative(stub.body, zoomed.frame)));
                    }
                }
            }
            view.set_hidden(false);
        }
        for (host, view) in &self.stubs {
            if !stubs.contains_key(host) {
                view.set_hidden(true);
            }
        }
    }
}

fn picker_entries(items: &[scene::PickerItem]) -> Vec<MenuEntry> {
    items
        .iter()
        .map(|item| {
            MenuEntry::Item(MenuItem {
                enabled: item.enabled,
                ..MenuItem::new(item.plugin, &item.title)
            })
        })
        .collect()
}

/// Меню выбора «Открыть в новой части»: плагины реестра, `terminal` первым; id пункта — plugin id.
/// Выбор уходит действием [`PanelAction::Open`] для панели открытого выбора, `Esc` закрывает меню
/// без действия.
fn picker_menu(
    app: &App,
    registry: &Arc<Registry>,
    on_panel: Rc<dyn Fn(PanelAction)>,
    panel: Rc<Cell<Option<PanelId>>>,
) -> Option<Menu> {
    let entries = picker_entries(&scene::picker_items(&State::default(), registry, None));
    let known = Arc::clone(registry);
    let chosen = move |intent| match intent {
        MenuIntent::Chosen { id } => {
            let plugin = known
                .iter()
                .map(|plugin| plugin.manifest().id)
                .find(|plugin| *plugin == id);
            if let (Some(plugin), Some(panel)) = (plugin, panel.take()) {
                on_panel(PanelAction::Open { panel, plugin });
            }
        }
    };
    Menu::new(app, PICKER_TITLE, entries, chosen)
        .map_err(|error| {
            shellio::owner::say_err(format_args!(
                "shellio: выбор новой части не построен: {error}"
            ));
        })
        .ok()
}

fn spec(plan: &SplitPlan) -> SplitSpec {
    SplitSpec {
        slots: plan
            .spans
            .iter()
            .map(|span| SplitSlot {
                start: span.start,
                length: span.length,
                collapsed: span.collapsed,
            })
            .collect(),
        grips: plan.grips.clone(),
    }
}

fn frame(rect: Rect) -> Frame {
    Frame::new(rect.x, rect.y, rect.width, rect.height)
}

fn to_insets([left, top, right, bottom]: [f64; 4]) -> Insets {
    Insets {
        left,
        top,
        right,
        bottom,
    }
}

fn zone_name(zone: Zone) -> &'static str {
    match zone {
        Zone::Left => "left",
        Zone::Right => "right",
        Zone::Center => "center",
        Zone::Bottom => "bottom",
    }
}

fn split_id(zone: Zone) -> String {
    format!("layout.split.{}", zone_name(zone))
}

fn part_id(key: PartKey) -> String {
    format!("layout.part.{}.{}", zone_name(key.zone), key.index)
}

/// Id узла `ui.inspect` места тела: колонки навигатора, плитки части или развёрнутой плитки.
fn host_id(host: Host) -> String {
    match host {
        Host::Navigator => NAVIGATOR_ID.to_owned(),
        Host::Part(key) => part_id(key),
        Host::Zoom => ZOOM_ID.to_owned(),
    }
}

/// Ключ сверки сообщения пустой рабочей области (таблица `shellio-conformance`).
const NOTE_CK: &str = "workbench.empty";

/// Ключ сверки выбора «Открыть в новой части».
const PICKER_CK: &str = "picker";

/// Ключ сверки контейнера зоны.
fn zone_ck(zone: Zone) -> String {
    format!("zone.{}", zone_name(zone))
}

/// Номер части на экране с единицы: колонка навигатора — первая часть левой зоны, плитки левой
/// зоны идут со второй.
fn part_number(key: PartKey) -> usize {
    let column = usize::from(key.zone == Zone::Left);
    key.index + 1 + column
}

/// Ключ сверки места тела; `None` — место скрыто развёрнутой панелью. Развёрнутая панель —
/// первая часть центра, как в паспорте.
fn host_ck(host: Host, zoomed: bool) -> Option<String> {
    match host {
        Host::Zoom => Some("zone.center.part.1".to_owned()),
        Host::Navigator | Host::Part(_) if zoomed => None,
        Host::Navigator => Some("zone.left.part.1".to_owned()),
        Host::Part(key) => Some(format!("{}.part.{}", zone_ck(key.zone), part_number(key))),
    }
}

/// Производный узел сверки без собственного вида: кадр и видимость — вида `source`, из свойств
/// только заливка `fill`.
fn derived(
    source: &shellio_appkit::InspectNode,
    id: String,
    class: &'static str,
    ck: String,
    fill: Option<shellio_appkit::Rgba>,
) -> shellio_appkit::InspectNode {
    let mut node = source.clone();
    node.id = id;
    node.class = class;
    node.ck = Some(ck);
    node.style = fill.map(|fill| shellio_appkit::Style {
        fill: Some(fill),
        ..shellio_appkit::Style::default()
    });
    node.truncated = None;
    node.state = None;
    node.facts.role = None;
    node.facts.label = None;
    node.facts.help = None;
    node.facts.first_responder = false;
    node.facts.animation_keys = 0;
    node
}

/// Контейнер центра под развёрнутой панелью для сверки. Отдельного вида у него нет: область зон
/// скрыта, развёрнутая плитка — ребёнок корня раскладки. Его кадр — кадр плитки, заливка — та,
/// что видна за ней. `None` — плитка не показана.
fn zoom_zone(plate: &shellio_appkit::InspectNode) -> Option<shellio_appkit::InspectNode> {
    plate.ck.as_ref()?;
    Some(derived(
        plate,
        format!("{}.zone", plate.id),
        "ZoomZone",
        zone_ck(Zone::Center),
        plate.behind,
    ))
}

fn zone_id(zone: Zone) -> String {
    format!("layout.zone.{}", zone_name(zone))
}

/// Контейнер зоны для сверки. Вид деления зоны держит ещё и промежутки вокруг плиток, поэтому
/// контейнер — производный узел: ребёнок вида деления с заливкой, видимой под ним. Кадр ему
/// даёт объединение кадров плиток зоны.
fn zone_node(split: &shellio_appkit::InspectNode, zone: Zone) -> shellio_appkit::InspectNode {
    let mut node = derived(split, zone_id(zone), "Zone", zone_ck(zone), split.under);
    node.parent = Some(split.id.clone());
    node
}

/// Тело колонки навигатора для сверки: производный узел с кадром контейнера панели, его ребёнок.
fn column_body(panel: &shellio_appkit::InspectNode, host: &str) -> shellio_appkit::InspectNode {
    let mut node = derived(
        panel,
        format!("{NAVIGATOR_ID}.body"),
        "ColumnBody",
        format!("{host}.body"),
        None,
    );
    node.parent = Some(panel.id.clone());
    node
}

/// Наименьший кадр, который вмещает оба.
fn bounds(a: shellio_control::Rect, b: shellio_control::Rect) -> shellio_control::Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    shellio_control::Rect {
        x,
        y,
        width: (a.x + a.width).max(b.x + b.width) - x,
        height: (a.y + a.height).max(b.y + b.height) - y,
    }
}

/// Промежуток между кадрами соседних частей: части рядом — по горизонтали, одна над другой —
/// по вертикали (начало кадра внизу слева, первая часть выше).
fn gap(first: shellio_control::Rect, second: shellio_control::Rect) -> shellio_control::Rect {
    if second.x >= first.x + first.width {
        shellio_control::Rect {
            x: first.x + first.width,
            y: first.y,
            width: second.x - (first.x + first.width),
            height: first.height,
        }
    } else {
        let top = second.y + second.height;
        shellio_control::Rect {
            x: first.x,
            y: top,
            width: first.width,
            height: first.y - top,
        }
    }
}

/// Разделители частей зоны для сверки. Вида у разделителя нет: его кадр — промежуток между
/// кадрами соседних плиток зоны. Колонка навигатора плиткой не считается: между ней и плиткой
/// левой зоны разделителя частей нет.
fn with_dividers(mut nodes: Vec<ComponentView>) -> Vec<ComponentView> {
    let mut dividers = Vec::new();
    for zone in Zone::ALL {
        let base = zone_ck(zone);
        let first = 1 + usize::from(zone == Zone::Left);
        let part = |number: usize| {
            let ck = format!("{base}.part.{number}");
            nodes
                .iter()
                .find(|node| node.ck.as_deref() == Some(ck.as_str()))
        };
        let mut number = first;
        while let (Some(above), Some(below)) = (part(number), part(number + 1)) {
            let index = number - first + 1;
            dividers.push(ComponentView {
                id: format!("{}.divider.{index}", split_id(zone)),
                class: "Divider".to_owned(),
                object: 0,
                parent: above.parent.clone(),
                ck: Some(format!("{base}.divider.{index}")),
                frame: gap(above.frame, below.frame),
                visible: above.visible && below.visible,
                first_responder: false,
                enabled: None,
                role: None,
                label: None,
                help: None,
                selected: None,
                expanded: None,
                animation_keys: 0,
                style: None,
                truncated: None,
                state: None,
            });
            number += 1;
        }
    }
    nodes.extend(dividers);
    nodes
}

/// Ключ сверки узла плитки: `suffix` — остаток имени узла после имени плитки, `class` — класс
/// узла, `tab` — номер вкладки на экране с единицы. Узлы вне таблицы ключей (нагрузка строки,
/// пункты меню) ключа не получают.
fn plate_ck(base: &str, suffix: &str, class: &str, tab: u32) -> Option<String> {
    match (suffix, class) {
        ("", _) => Some(base.to_owned()),
        (".tabs", _) => Some(format!("{base}.tabs")),
        (".tabs.split" | ".tabs.zoom" | ".tabs.restore" | ".tabs.menu", _) => {
            Some(format!("{base}{suffix}"))
        }
        (".tabs.split.icon" | ".tabs.zoom.icon" | ".tabs.restore.icon" | ".tabs.menu.icon", _) => {
            Some(format!("{base}{suffix}"))
        }
        (_, "Tab") => Some(format!("{base}.tabs.tab.{tab}")),
        (_, "TabCloseIcon") => Some(format!("{base}.tabs.tab.{tab}.close.icon")),
        (_, "TabIcon") => Some(format!("{base}.tabs.tab.{tab}.icon")),
        (_, "TabLabel") => Some(format!("{base}.tabs.tab.{tab}.label")),
        (_, "TabClose") => Some(format!("{base}.tabs.tab.{tab}.close")),
        _ => None,
    }
}

/// Ключ сверки узла заглушки «Развернуть».
fn stub_ck(base: &str, suffix: &str) -> Option<String> {
    match suffix {
        "" => Some(base.to_owned()),
        ".label" | ".expand" | ".expand.label" => Some(format!("{base}{suffix}")),
        _ => None,
    }
}

/// Узлы плитки с ключами сверки по месту `base`; без него узлы остаются без ключей.
fn keyed(
    list: Vec<shellio_appkit::InspectNode>,
    id: &str,
    base: Option<&str>,
) -> Vec<shellio_appkit::InspectNode> {
    let Some(base) = base else {
        return list;
    };
    let mut tab = 0;
    list.into_iter()
        .map(|node| {
            let suffix = node.id.get(id.len()..).unwrap_or_default().to_owned();
            if node.class == "Tab" {
                tab += 1;
            }
            match plate_ck(base, &suffix, node.class, tab) {
                Some(ck) if node.facts.visible => node.with_ck(ck),
                _ => node,
            }
        })
        .collect()
}

fn panel_id(panel: PanelId) -> String {
    format!("layout.panel.{panel}")
}

fn side(zone: Zone) -> ZoneSide {
    match zone {
        Zone::Left | Zone::Right => ZoneSide::Side,
        Zone::Center => ZoneSide::Center,
        Zone::Bottom => ZoneSide::Bottom,
    }
}

/// AX-имя строки вкладок части.
fn strip_label(zone: Zone, index: usize, count: usize, zoomed: bool) -> String {
    if zoomed {
        return "Вкладки развёрнутой панели".to_owned();
    }
    let zone = match zone {
        Zone::Left => "левой зоны",
        Zone::Right => "правой зоны",
        Zone::Center => "центра",
        Zone::Bottom => "нижней зоны",
    };
    if count > 1 {
        format!("Вкладки {zone}, часть {}", index + 1)
    } else {
        format!("Вкладки {zone}")
    }
}

/// Системный значок типа панели: из манифеста плагина, у незарегистрированного — пунктирный
/// квадрат.
fn icon(registry: &Registry, plugin: &str) -> &'static str {
    PluginId::new(plugin)
        .ok()
        .and_then(|id| registry.get(&id).map(|plugin| plugin.manifest().icon))
        .unwrap_or(shellio_theme::icons::FALLBACK)
}

/// Узлы тела панели для `ui.inspect`: id начинаются с `layout.panel.<PanelId>.`, кадры тело уже
/// отдаёт в точках окна, поэтому перевод один — тот же, что у остальных узлов окна.
fn body_nodes(panel: PanelId, body: &dyn PluginView) -> Vec<ComponentView> {
    body.inspect(&format!("{}.", panel_id(panel)))
        .into_iter()
        .map(window_ui::component)
        .collect()
}

/// Контейнеры панелей и фабрики тел на время одной доставки сцены.
struct Hosts<'a> {
    app: &'a App,
    theme: &'a AppTheme,
    views: &'a mut BTreeMap<PanelId, PanelView>,
    factories: Option<&'a Factories>,
    /// Корень раскладки: в нём стоят припаркованные контейнеры.
    parking: &'a Pane,
    requests: SharedRequests,
    on_request: &'a OnRequest,
}

impl Shell for Hosts<'_> {
    fn build(&mut self, order: Build<'_>) -> Option<Body> {
        let factory = self.factories?.get(order.plugin)?;
        let view = self.views.get(&order.panel)?;
        let on_request = Rc::clone(self.on_request);
        let sink = bodies::sink(
            order.panel,
            order.generation,
            Rc::clone(&self.requests),
            move |ticket, request| on_request(ticket, request),
        );
        let body = Body::build(factory, self.app, self.theme, sink, order.data);
        view.set_body(body.view().body());
        Some(body)
    }

    fn resign(&mut self, panel: PanelId) {
        if let Some(view) = self.views.get(&panel) {
            view.resign_focus();
        }
    }

    fn holds_input(&self, panel: PanelId) -> bool {
        self.views.get(&panel).is_some_and(PanelView::holds_focus)
    }

    fn park(&mut self, panel: PanelId) {
        if let Some(view) = self.views.get(&panel) {
            view.set_hidden(true);
            self.parking.place(view, Insets::ZERO);
        }
    }

    fn release(&mut self, panel: PanelId) {
        self.views.remove(&panel);
    }
}

fn strip_state(
    registry: &Registry,
    zone: Zone,
    index: usize,
    count: usize,
    slot: &PlateSlot,
    bench: &Bench,
    zoomed: bool,
) -> TabStripState {
    let presentation = Presentation {
        zone: side(zone),
        navigator: false,
        zoomed,
    };
    let mut state = TabStripState::new(&strip_label(zone, index, count, zoomed), presentation);
    state.tabs = slot
        .tabs
        .iter()
        .filter_map(|panel| {
            bench.views.get(panel).map(|view| {
                Tab::new(
                    &panel.to_string(),
                    &view.title,
                    &view.plugin,
                    icon(registry, &view.plugin),
                    TabStatus::None,
                )
            })
        })
        .collect();
    let active = slot.active.to_string();
    state.selected = state
        .tabs
        .iter()
        .any(|tab| tab.id == active)
        .then_some(active);
    state.focused = slot.focused;
    state.mode = KeyMode::Navigation;
    state.actions = scene::panel_menu(&bench.layout, slot.active)
        .into_iter()
        .map(|row| match row {
            MenuRow::Item(item, title) => MenuEntry::Item(MenuItem::new(item.id(), title)),
            MenuRow::Separator(id) => MenuEntry::Separator { id: id.to_owned() },
        })
        .collect();
    state
}

fn part_view(zone: Zone, index: usize, slot: &PlateSlot, zoomed: bool) -> PartNodeView {
    PartNodeView {
        zone,
        index: u32::try_from(index).unwrap_or(u32::MAX),
        tabs: slot.tabs.clone(),
        active: slot.active,
        shown: slot.frame.is_some(),
        focused: slot.focused,
        zoomed,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use shellio_appkit::{InspectNode, Piece, ViewFacts};
    use shellio_layout::Geometry;

    use super::*;

    /// Каждый ключ сверки, который назначают заголовок, строка статуса и раскладка, есть в
    /// таблице шаблонов `shellio-conformance`.
    /// Разделитель — промежуток между соседними плитками зоны; колонка навигатора не плитка.
    #[test]
    fn zone_frame_is_the_bounds_of_its_plates_without_the_gaps() {
        let rect = |x, y, width, height| shellio_control::Rect {
            x,
            y,
            width,
            height,
        };
        // Две части правой зоны одна над другой с промежутком 6 точек.
        assert_eq!(
            bounds(
                rect(802.0, 356.0, 372.0, 318.0),
                rect(802.0, 32.0, 372.0, 318.0)
            ),
            rect(802.0, 32.0, 372.0, 642.0)
        );
        // Две части центра рядом; порядок кадров исход не меняет.
        assert_eq!(
            bounds(
                rect(519.0, 274.0, 277.0, 400.0),
                rect(236.0, 274.0, 277.0, 400.0)
            ),
            rect(236.0, 274.0, 560.0, 400.0)
        );
    }

    #[test]
    fn dividers_fill_the_gaps_between_zone_plates() {
        use shellio_conformance::keys::check_key;

        let plate = |ck: &str, x: f64, y: f64, width: f64, height: f64| ComponentView {
            id: ck.to_owned(),
            class: "Plate".to_owned(),
            object: 1,
            parent: Some("layout.split".to_owned()),
            ck: Some(ck.to_owned()),
            frame: shellio_control::Rect {
                x,
                y,
                width,
                height,
            },
            visible: true,
            first_responder: false,
            enabled: None,
            role: None,
            label: None,
            help: None,
            selected: None,
            expanded: None,
            animation_keys: 0,
            style: None,
            truncated: None,
            state: None,
        };
        let nodes = with_dividers(vec![
            plate("zone.left.part.1", 8.0, 34.0, 222.0, 640.0),
            plate("zone.left.part.2", 236.0, 34.0, 372.0, 642.0),
            plate("zone.center.part.1", 236.0, 274.0, 277.0, 400.0),
            plate("zone.center.part.2", 519.0, 274.0, 277.0, 400.0),
            plate("zone.right.part.1", 802.0, 356.0, 372.0, 318.0),
            plate("zone.right.part.2", 802.0, 32.0, 372.0, 318.0),
            plate("zone.right.part.3", 802.0, 0.0, 372.0, 26.0),
        ]);
        let divider = |ck: &str| {
            nodes
                .iter()
                .find(|node| node.ck.as_deref() == Some(ck))
                .map(|node| {
                    assert_eq!(node.class, "Divider");
                    assert_eq!(node.parent.as_deref(), Some("layout.split"));
                    check_key(ck).unwrap();
                    (
                        node.frame.x,
                        node.frame.y,
                        node.frame.width,
                        node.frame.height,
                    )
                })
        };
        assert_eq!(divider("zone.left.divider.1"), None, "колонка — не плитка");
        assert_eq!(
            divider("zone.center.divider.1"),
            Some((513.0, 274.0, 6.0, 400.0))
        );
        assert_eq!(
            divider("zone.right.divider.1"),
            Some((802.0, 350.0, 372.0, 6.0))
        );
        assert_eq!(
            divider("zone.right.divider.2"),
            Some((802.0, 26.0, 372.0, 6.0))
        );
        assert_eq!(nodes.len(), 10);
    }

    #[test]
    fn every_assigned_key_is_in_the_template_table() {
        use shellio_conformance::keys::{check_key, check_unique};

        let mut keys: Vec<String> = shellio_appkit::ck::ALL
            .into_iter()
            .map(str::to_owned)
            .collect();
        keys.push(NOTE_CK.to_owned());
        keys.push(PICKER_CK.to_owned());
        let mut hosts = vec![Host::Navigator];
        for zone in Zone::ALL {
            keys.push(zone_ck(zone));
            for index in 0..3 {
                hosts.push(Host::Part(PartKey { zone, index }));
            }
        }
        for host in hosts {
            let base = host_ck(host, false).unwrap();
            assert_eq!(
                host_ck(host, true),
                None,
                "под развёрнутой панелью ключа нет"
            );
            keys.push(format!("{base}.body"));
            for suffix in ["", ".label", ".expand", ".expand.label"] {
                keys.push(stub_ck(&format!("{base}.body.stub"), suffix).unwrap());
            }
            for (suffix, class, tab) in [
                ("", "Plate", 0),
                (".tabs", "TabStrip", 0),
                (".tabs.tab.a", "Tab", 1),
                (".tabs.tab.a.icon", "TabIcon", 1),
                (".tabs.tab.a.label", "TabLabel", 1),
                (".tabs.tab.a.close", "TabClose", 1),
                (".tabs.tab.a.close.icon", "TabCloseIcon", 1),
                (".tabs.split.icon", "ButtonIcon", 2),
                (".tabs.zoom.icon", "ButtonIcon", 2),
                (".tabs.restore.icon", "ButtonIcon", 2),
                (".tabs.menu.icon", "ButtonIcon", 2),
                (".tabs.tab.b", "Tab", 2),
                (".tabs.tab.b.label", "TabLabel", 2),
                (".tabs.split", "StripButton", 2),
                (".tabs.zoom", "StripButton", 2),
                (".tabs.restore", "StripButton", 2),
                (".tabs.menu", "MenuButton", 2),
            ] {
                keys.push(plate_ck(&base, suffix, class, tab).unwrap());
            }
            assert_eq!(plate_ck(&base, ".tabs.payload", "Payload", 0), None);
            assert_eq!(plate_ck(&base, ".tabs.menu.item", "Menu", 0), None);
            assert_eq!(stub_ck(&base, ".text"), None);
        }
        for key in &keys {
            check_key(key).unwrap();
        }
        check_unique(keys.iter().map(String::as_str)).unwrap();
        assert_eq!(
            host_ck(Host::Zoom, true).as_deref(),
            Some("zone.center.part.1")
        );
    }

    /// Номер части — порядок на экране: колонка навигатора — первая часть левой зоны.
    #[test]
    fn part_numbers_follow_the_screen_order() {
        let key = |zone, index| PartKey { zone, index };
        assert_eq!(
            host_ck(Host::Navigator, false).as_deref(),
            Some("zone.left.part.1")
        );
        assert_eq!(
            host_ck(Host::Part(key(Zone::Left, 0)), false).as_deref(),
            Some("zone.left.part.2")
        );
        assert_eq!(
            host_ck(Host::Part(key(Zone::Center, 0)), false).as_deref(),
            Some("zone.center.part.1")
        );
        assert_eq!(
            host_ck(Host::Part(key(Zone::Right, 1)), false).as_deref(),
            Some("zone.right.part.2")
        );
    }

    /// Корень поверхности отдаёт заливку, которую видно под ним; у остальных узлов заливка своя.
    #[test]
    fn root_key_takes_the_fill_seen_under_the_view() {
        let under = shellio_appkit::Rgba {
            r: 30,
            g: 29,
            b: 27,
            a: 1.0,
        };
        let plain = InspectNode {
            under: Some(under),
            ..node(
                "layout.split.center",
                "layout",
                Frame::new(0.0, 0.0, 1.0, 1.0),
            )
        };
        let child = plain.clone().with_ck("zone.center.part.1");
        assert_eq!(child.ck.as_deref(), Some("zone.center.part.1"));
        assert_eq!(
            child.style, None,
            "узел с родителем заливку предков не берёт"
        );
        let root = plain.clone().with_root_ck("zone.center");
        assert_eq!(root.ck.as_deref(), Some("zone.center"));
        assert_eq!(root.style.and_then(|style| style.fill), Some(under));
        let bare = InspectNode {
            under: None,
            ..plain
        }
        .with_root_ck("zone.center");
        assert_eq!(
            bare.style, None,
            "без непрозрачной заливки под видом поля нет"
        );
    }

    fn classed(class: &'static str, mut node: InspectNode) -> InspectNode {
        node.class = class;
        node
    }

    /// Вкладки получают номера по порядку узлов, нагрузка и скрытые узлы — без ключа.
    #[test]
    fn plate_nodes_are_keyed_by_place() {
        let list = vec![
            node("p", "x", Frame::new(0.0, 0.0, 1.0, 1.0)),
            node("p.tabs", "p", Frame::new(0.0, 0.0, 1.0, 1.0)),
            classed(
                "Tab",
                node("p.tabs.tab.7f", "p.tabs", Frame::new(0.0, 0.0, 1.0, 1.0)),
            ),
            classed(
                "TabLabel",
                node(
                    "p.tabs.tab.7f.label",
                    "p.tabs.tab.7f",
                    Frame::new(0.0, 0.0, 1.0, 1.0),
                ),
            ),
            classed(
                "Tab",
                node("p.tabs.tab.1c", "p.tabs", Frame::new(0.0, 0.0, 1.0, 1.0)),
            ),
            classed(
                "TabClose",
                node(
                    "p.tabs.tab.1c.close",
                    "p.tabs.tab.1c",
                    Frame::new(0.0, 0.0, 1.0, 1.0),
                ),
            ),
            node("p.tabs.payload", "p.tabs", Frame::new(0.0, 0.0, 1.0, 1.0)),
            node("p.tabs.zoom", "p.tabs", Frame::new(0.0, 0.0, 1.0, 1.0)),
        ];
        let keys: Vec<Option<String>> = keyed(list.clone(), "p", Some("zone.right.part.1"))
            .into_iter()
            .map(|node| node.ck)
            .collect();
        let expected = [
            Some("zone.right.part.1"),
            Some("zone.right.part.1.tabs"),
            Some("zone.right.part.1.tabs.tab.1"),
            Some("zone.right.part.1.tabs.tab.1.label"),
            Some("zone.right.part.1.tabs.tab.2"),
            Some("zone.right.part.1.tabs.tab.2.close"),
            None,
            Some("zone.right.part.1.tabs.zoom"),
        ];
        assert_eq!(keys, expected.map(|key| key.map(str::to_owned)).to_vec());
        assert!(keyed(list, "p", None).iter().all(|node| node.ck.is_none()));
    }

    /// Тело без AppKit: узел тела с кадром `frame` в точках окна и вложенный узел со смещением
    /// `offset` от него.
    struct FakeBody {
        frame: Frame,
        offset: (f64, f64),
    }

    fn node(id: &str, parent: &str, frame: Frame) -> InspectNode {
        InspectNode {
            id: id.to_owned(),
            class: "Surface",
            object: 1,
            parent: Some(parent.to_owned()),
            ck: None,
            style: None,
            truncated: None,
            under: None,
            behind: None,
            facts: ViewFacts {
                frame,
                visible: true,
                first_responder: false,
                enabled: None,
                role: None,
                label: None,
                help: None,
                selected: None,
                expanded: None,
                animation_keys: 0,
            },
            state: None,
        }
    }

    impl PluginView for FakeBody {
        fn body(&self) -> Piece<'_> {
            unimplemented!("тесты без NSApplication тело не размещают")
        }
        fn adapt(
            &mut self,
            _geometry: &Geometry,
            _presentation: &shellio_plugin_api::Presentation,
        ) {
        }
        fn update(&mut self, _state: &Value) {}
        fn set_theme(&mut self, _theme: &AppTheme) {}
        fn set_active(&mut self, _active: bool) {}
        fn set_focused(&mut self, _focused: bool) {}
        fn focus_preferred(&self) {}
        fn inspect(&self, prefix: &str) -> Vec<InspectNode> {
            let parent = prefix.strip_suffix('.').unwrap_or(prefix);
            let body = format!("{prefix}body");
            let nested = Frame::new(
                self.frame.x + self.offset.0,
                self.frame.y + self.offset.1,
                self.frame.width - self.offset.0,
                self.frame.height - self.offset.1,
            );
            vec![
                node(&body, parent, self.frame),
                node(&format!("{prefix}list"), &body, nested),
            ]
        }
        fn close_view(&mut self) {}
    }

    fn panel(n: u8) -> PanelId {
        format!("00000000-0000-0000-0000-{n:012x}").parse().unwrap()
    }

    #[test]
    fn two_bodies_of_one_plugin_get_different_node_ids() {
        let body = FakeBody {
            frame: Frame::new(900.0, 40.0, 280.0, 600.0),
            offset: (0.0, 0.0),
        };
        let (first, second) = (panel(1), panel(2));
        let ids = |panel| -> Vec<String> {
            body_nodes(panel, &body)
                .into_iter()
                .map(|node| node.id)
                .collect()
        };
        assert_eq!(
            ids(first),
            [
                format!("layout.panel.{first}.body"),
                format!("layout.panel.{first}.list")
            ]
        );
        assert!(
            ids(first).iter().all(|id| !ids(second).contains(id)),
            "id узлов двух панелей одного плагина не совпадают"
        );
        let nodes = body_nodes(second, &body);
        assert_eq!(
            nodes[0].parent.as_deref(),
            Some(format!("layout.panel.{second}").as_str()),
            "узел тела стоит под узлом контейнера панели"
        );
        assert_eq!(nodes[1].parent.as_deref(), Some(nodes[0].id.as_str()));
    }

    #[test]
    fn nested_node_keeps_its_window_frame_without_a_second_shift() {
        let body = FakeBody {
            frame: Frame::new(900.0, 40.0, 280.0, 600.0),
            offset: (112.0, 36.0),
        };
        let nodes = body_nodes(panel(1), &body);
        let (outer, nested) = (&nodes[0].frame, &nodes[1].frame);
        assert_eq!(
            (outer.x, outer.y),
            (900.0, 40.0),
            "кадр тела — как отдал вид"
        );
        assert_eq!(
            (nested.x, nested.y, nested.width, nested.height),
            (
                outer.x + 112.0,
                outer.y + 36.0,
                outer.width - 112.0,
                outer.height - 36.0
            ),
            "кадр вложенного узла — кадр тела плюс его смещение"
        );
    }

    #[test]
    fn tab_icon_comes_from_the_manifest() {
        let registry = plugins::registry().unwrap();
        for plugin in registry.iter() {
            assert_eq!(
                icon(&registry, plugin.id().as_str()),
                plugin.manifest().icon
            );
        }
        assert_eq!(icon(&registry, "terminal"), "term");
        assert_eq!(icon(&registry, "navigator"), "grid");
        assert_eq!(icon(&registry, "gone-plugin"), "file");
        assert_eq!(icon(&registry, "Не id"), "file");
    }
}
