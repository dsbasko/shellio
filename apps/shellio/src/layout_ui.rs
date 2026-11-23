//! Адаптер раскладки обычного окна на главном потоке (ADR-0009, «AppKit-адаптер»): зеркало и
//! размер окна → сцена [`shellio::scene`] → разница → операции над компонентами `shellio-appkit`.
//!
//! Корень — горизонтальный [`SplitView`]: колонка навигатора, плитка левой зоны, столбец центра,
//! правая зона. Столбец центра — вертикальный сплит, центр над нижней зоной. Части каждой зоны —
//! сплит вдоль её оси, в элементе части стоит [`Plate`]. Вид панели создаётся один раз на
//! `PanelId`: смена вкладки его прячет и показывает, перенос и `zoom` переставляют родителя.
//! Развёрнутая панель лежит на отдельной плитке поверх корня, деления под ней не меняются.

use std::collections::BTreeMap;
use std::rc::Rc;

use shellio::scene::{self, Bench, Host, Op, PartKey, PlateSlot, Scene, SplitId, SplitPlan};
use shellio_appkit::{
    App, EmptyNote, Frame, Insets, MainWindow, Pane, PanelCard, PanelView, Plate, Presentation,
    SplitDirection, SplitDrag, SplitSlot, SplitSpec, SplitView, Tab, TabIntent, TabStatus,
    TabStripState, ZoneSide,
};
use shellio_control::{ComponentState, ComponentView, PanelNodeView, PartNodeView};
use shellio_core::{PanelId, State, layout_metrics};
use shellio_layout::{Rect, SizeClass, Viewport, Zone, ZoneMap};
use shellio_theme::{AppTheme, KeyMode, Motion};

use crate::window_ui;

/// Id узлов `ui.inspect` раскладки.
const ROOT_ID: &str = "layout";
const ROOT_SPLIT_ID: &str = "layout.split.root";
const COLUMN_SPLIT_ID: &str = "layout.split.column";
const NAVIGATOR_ID: &str = "layout.navigator";
const ZOOM_ID: &str = "layout.zoom";
const NOTE_ID: &str = "layout.note";

/// Раскладка обычного окна.
pub struct LayoutUi {
    // Порядок полей — порядок освобождения: сначала виды панелей и плитки, затем сплиты и
    // контейнер.
    views: BTreeMap<PanelId, PanelView>,
    plates: BTreeMap<PartKey, Plate>,
    zoom: Option<Plate>,
    note: EmptyNote,
    zones: ZoneMap<SplitView>,
    column: SplitView,
    root: SplitView,
    container: Pane,
    scene: Option<Scene>,
    theme: AppTheme,
    motion: Motion,
}

impl LayoutUi {
    /// Пустая раскладка в содержимом окна `window`. `on_drag` получает конец перетаскивания
    /// разделителя на главном потоке изнутри AppKit и не должен синхронно трогать раскладку.
    pub fn new(
        app: &App,
        window: &MainWindow,
        on_drag: impl Fn(SplitId, SplitDrag) + 'static,
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
        let empty = Scene::of(
            &State::default(),
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
        LayoutUi {
            views: BTreeMap::new(),
            plates: BTreeMap::new(),
            zoom: None,
            note,
            zones,
            column,
            root,
            container,
            scene: None,
            theme,
            motion: selection.motion,
        }
    }

    /// Последняя применённая сцена.
    pub fn scene(&self) -> Option<&Scene> {
        self.scene.as_ref()
    }

    /// Показывает состояние `state` (`None` — снимка ещё нет) при размере содержимого `viewport`.
    pub fn render(&mut self, app: &App, state: Option<&State>, viewport: Viewport) {
        let empty = State::default();
        let next = Scene::of(state.unwrap_or(&empty), viewport, layout_metrics());
        let before = self.scene.as_ref().and_then(|scene| scene.bench.as_ref());
        let ops = scene::diff(before, next.bench.as_ref());
        if let Some(bench) = &next.bench {
            for op in &ops {
                match *op {
                    Op::AddPlate(key) => self.add_plate(app, key, bench),
                    Op::AddView(panel, _) => {
                        if let Some(slot) = bench.views.get(&panel) {
                            let card = card(slot);
                            self.views
                                .insert(panel, PanelView::new(app, &self.theme, card));
                        }
                    }
                    Op::Reparent(..)
                    | Op::Show(_)
                    | Op::Hide(_)
                    | Op::RemoveView(_)
                    | Op::RemovePlate(_) => {}
                }
            }
        }
        self.place(app, &next);
        for op in ops {
            match op {
                Op::Show(panel) => self.set_view_hidden(panel, false),
                Op::Hide(panel) => self.set_view_hidden(panel, true),
                Op::RemoveView(panel) => {
                    self.views.remove(&panel);
                }
                Op::RemovePlate(key) => {
                    self.plates.remove(&key);
                }
                Op::AddPlate(_) | Op::AddView(..) | Op::Reparent(..) => {}
            }
        }
        self.scene = Some(next);
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
            push(
                &mut nodes,
                self.zones.get(zone).inspect(&split_id(zone), Some(parent)),
            );
        }
        let bench = self.scene.as_ref().and_then(|scene| scene.bench.as_ref());
        if let Some(bench) = bench {
            for (key, plate) in &self.plates {
                let Some(slot) = bench.plates.get(key) else {
                    continue;
                };
                let id = part_id(*key);
                let mut list = plate.inspect(&id, Some(&split_id(key.zone)));
                if let Some(first) = list.first_mut() {
                    let view = part_view(key.zone, key.index, slot, false);
                    let mut component = window_ui::component(first.clone());
                    component.state = Some(ComponentState::Part(Box::new(view)));
                    nodes.push(component);
                    list.remove(0);
                }
                nodes.extend(list.into_iter().map(window_ui::component));
            }
            if let (Some(zoom), Some(zoomed)) = (&self.zoom, &bench.zoomed) {
                let mut list = zoom.inspect(ZOOM_ID, Some(ROOT_ID));
                if !list.is_empty() {
                    let mut component = window_ui::component(list.remove(0));
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
                let parent = match slot.host {
                    Host::Navigator => NAVIGATOR_ID.to_owned(),
                    Host::Part(key) => part_id(key),
                    Host::Zoom => ZOOM_ID.to_owned(),
                };
                let mut component =
                    window_ui::component(view.inspect(&panel_id(*panel), Some(&parent)));
                component.state = Some(ComponentState::Panel(Box::new(PanelNodeView {
                    panel: *panel,
                    component: view.id().get(),
                    plugin: slot.plugin.clone(),
                    title: slot.title.clone(),
                    shown: slot.shown(),
                    zone: slot.geometry.map(|geometry| geometry.zone),
                    size: slot.geometry.map(|geometry| geometry.size),
                })));
                nodes.push(component);
            }
        }
        if self.scene.as_ref().and_then(Scene::note).is_some() {
            nodes.push(window_ui::component(
                self.note.inspect(NOTE_ID, Some(ROOT_ID)),
            ));
        }
        nodes
    }

    fn add_plate(&mut self, app: &App, key: PartKey, bench: &Bench) {
        let Some(slot) = bench.plates.get(&key) else {
            return;
        };
        let count = bench.layout.zones.get(key.zone).parts.len();
        let state = strip_state(key.zone, key.index, count, slot, bench, false);
        match Plate::new(app, &self.theme, self.motion, state, tab_intent) {
            Ok(plate) => {
                self.plates.insert(key, plate);
            }
            Err(error) => shellio::owner::say_err(format_args!(
                "shellio: плитка части {key:?} не построена: {error:?}"
            )),
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
                let state = strip_state(key.zone, key.index, count, slot, bench, false);
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
                let state = strip_state(Zone::Center, 0, 1, &slot, bench, true);
                if self.zoom.is_none() {
                    match Plate::new(app, &self.theme, self.motion, state.clone(), tab_intent) {
                        Ok(plate) => self.zoom = Some(plate),
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
                view.set_card(card(slot));
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

/// Намерения строк вкладок раскладки получают обработчики вместе с кнопками и щелчками
/// (P1.4, задачи 9 и 10).
fn tab_intent(_intent: TabIntent) {}

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

/// Системный значок типа панели.
fn icon(plugin: &str) -> &'static str {
    match plugin {
        shellio_core::NAVIGATOR_PLUGIN => "sidebar.left",
        shellio_core::TERMINAL_PLUGIN => "terminal",
        _ => "square.dashed",
    }
}

fn strip_state(
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
                    icon(&view.plugin),
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
    state
}

fn card(slot: &scene::ViewSlot) -> PanelCard {
    let place = match slot.host {
        Host::Navigator => "Колонка навигатора",
        Host::Part(key) => match key.zone {
            Zone::Left => "Левая зона",
            Zone::Right => "Правая зона",
            Zone::Center => "Центр",
            Zone::Bottom => "Нижняя зона",
        },
        Host::Zoom => "Развёрнута",
    };
    let size = slot.geometry.map_or("—", |geometry| match geometry.size {
        SizeClass::Narrow => "Narrow",
        SizeClass::Regular => "Regular",
        SizeClass::Wide => "Wide",
        SizeClass::ShortBar => "ShortBar",
    });
    PanelCard {
        title: slot.title.clone(),
        plugin: slot.plugin.clone(),
        place: place.to_owned(),
        size: size.to_owned(),
    }
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
