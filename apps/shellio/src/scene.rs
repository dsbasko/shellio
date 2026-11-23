//! Сцена раскладки обычного окна без AppKit (ADR-0009, «AppKit-адаптер»): что окно показывает по
//! зеркалу состояния и размеру содержимого. Адаптер окна применяет её к видам. Здесь чистые части
//! адаптера:
//!
//! - [`Scene::of`] — кадры модели, место и видимость вида каждой панели, плитки частей;
//! - [`diff`] — разница двух сцен в операции: создать, переставить, показать или скрыть, убрать;
//! - [`Scene::splits`] — элементы сплитов и прямоугольники их содержимого по кадрам модели;
//! - [`ViewportGate`] — размер окна для ядра с объединением до конца оборота цикла и без повторов;
//! - [`resize_command`] — конец перетаскивания разделителя в одну команду `layout.resize`.
//!
//! Вид панели живёт, пока панель есть в открытой рабочей области: смена вкладки, перенос и `zoom`
//! его не пересоздают. Начало координат — левый верхний угол содержимого окна, как у геометрии.

use std::collections::{BTreeMap, BTreeSet};

use shellio_core::{Command, PanelId, State, WorkbenchId, min_size, open_workbench};
use shellio_layout::{
    Axis, Frames, Geometry, Layout, LayoutMetrics, PartFrame, Rect, Viewport, Zone, ZoneMap,
    geometry,
};

/// Сообщение области зон без открытой рабочей области.
pub const NO_WORKBENCH: &str = "Рабочая область не открыта";
/// Сообщение на месте центра, когда вне колонки навигатора панелей нет.
pub const NO_PANELS: &str = "Нет открытых панелей";

/// Часть зоны по месту в раскладке.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartKey {
    pub zone: Zone,
    pub index: usize,
}

/// Где стоит вид панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Host {
    /// Колонка навигатора.
    Navigator,
    /// Тело плитки части.
    Part(PartKey),
    /// Тело плитки развёрнутой панели.
    Zoom,
}

/// Вид панели в сцене.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewSlot {
    pub host: Host,
    /// Тело показанного вида в точках содержимого окна; `None` — вид скрыт.
    pub body: Option<Rect>,
    pub geometry: Option<Geometry>,
    pub title: String,
    pub plugin: String,
}

impl ViewSlot {
    pub fn shown(&self) -> bool {
        self.body.is_some()
    }
}

/// Плитка части.
#[derive(Clone, Debug, PartialEq)]
pub struct PlateSlot {
    pub tabs: Vec<PanelId>,
    pub active: PanelId,
    /// Кадры показанной части; `None` — зона скрыта, пуста для показа или вне `only`.
    pub frame: Option<PartFrame<PanelId>>,
    /// Фокус в этой части.
    pub focused: bool,
}

/// Открытая рабочая область в сцене.
#[derive(Clone, Debug, PartialEq)]
pub struct Bench {
    pub workbench: WorkbenchId,
    pub layout: Layout<PanelId>,
    /// Кадры без `zoom`: по ним стоят сплиты и плитки, пока развёрнутая панель лежит поверх.
    pub frames: Frames<PanelId>,
    /// Плитка развёрнутой панели.
    pub zoomed: Option<PartFrame<PanelId>>,
    pub views: BTreeMap<PanelId, ViewSlot>,
    pub plates: BTreeMap<PartKey, PlateSlot>,
    /// Панель с фокусом.
    pub focus: Option<PanelId>,
}

/// Что показывает обычное окно.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub viewport: Viewport,
    /// Область зон: под заголовком и над строкой статуса.
    pub area: Rect,
    pub metrics: LayoutMetrics,
    /// `None` — рабочая область не открыта.
    pub bench: Option<Bench>,
}

impl Scene {
    /// Сцена открытой в окне рабочей области по состоянию и размеру содержимого окна.
    pub fn of(state: &State, viewport: Viewport, metrics: LayoutMetrics) -> Scene {
        let empty = Layout::<PanelId> {
            navigator: None,
            zones: ZoneMap::default(),
            sizes: shellio_layout::ZoneSizes::DEFAULT,
            hidden: shellio_layout::Hidden::default(),
            only: None,
            zoomed: None,
            focus: Vec::new(),
        };
        let area = geometry(&empty, viewport, &metrics, |_| {
            shellio_layout::MinSize::DEFAULT
        })
        .area;
        let bench = open_workbench(state).and_then(|workbench| {
            let layout = state.layouts.get(&workbench)?;
            Some(bench(state, workbench, layout, viewport, &metrics))
        });
        Scene {
            viewport,
            area,
            metrics,
            bench,
        }
    }

    /// Сообщение на месте области зон и его кадр; `None` — сообщения нет.
    pub fn note(&self) -> Option<(&'static str, Rect)> {
        match &self.bench {
            None => Some((NO_WORKBENCH, self.area)),
            Some(bench) if bench.zoomed.is_none() => {
                bench.frames.no_panels.map(|rect| (NO_PANELS, rect))
            }
            Some(_) => None,
        }
    }

    /// Элементы сплитов по кадрам модели без `zoom`. Без рабочей области всё свёрнуто.
    pub fn splits(&self) -> Splits {
        let area = self.area;
        let gap = f64::from(self.metrics.gap);
        let frames = self.bench.as_ref().map(|bench| &bench.frames);
        let zones = frames.map(|frames| &frames.zones);
        let root = plan(
            area.width,
            &[
                zones.and_then(|z| z.navigator).map(|r| r.x - area.x),
                zones.and_then(|z| z.left).map(|r| r.x - area.x),
                zones.and_then(|z| z.column).map(|r| r.x - area.x),
                zones.and_then(|z| z.right).map(|r| r.x - area.x),
            ],
            &[false, true, true],
            gap,
        );
        let root_panes = panes(area, Axis::SideBySide, &root.spans);
        let column = plan(
            area.height,
            &[
                zones.and_then(|z| z.center).map(|r| r.y - area.y),
                zones.and_then(|z| z.bottom).map(|r| r.y - area.y),
            ],
            &[true],
            gap,
        );
        let column_panes = panes(root_panes[2], Axis::Stacked, &column.spans);
        let zone_panes = ZoneMap {
            left: root_panes[1],
            right: root_panes[3],
            center: column_panes[0],
            bottom: column_panes[1],
        };
        let mut part_panes = BTreeMap::new();
        let mut zone_plans: ZoneMap<SplitPlan> = ZoneMap::default();
        for zone in Zone::ALL {
            let pane = *zone_panes.get(zone);
            let axis = zone.axis();
            let count = self
                .bench
                .as_ref()
                .map_or(0, |bench| bench.layout.zones.get(zone).parts.len());
            let starts: Vec<Option<f64>> = (0..count)
                .map(|index| {
                    frames
                        .and_then(|frames| {
                            frames
                                .parts
                                .iter()
                                .find(|part| part.zone == zone && part.index == index)
                        })
                        .map(|part| along(axis, part.frame) - along(axis, pane))
                })
                .collect();
            let grips = vec![true; count.saturating_sub(1)];
            let zone_plan = plan(length(axis, pane), &starts, &grips, gap);
            for (index, rect) in panes(pane, axis, &zone_plan.spans).into_iter().enumerate() {
                part_panes.insert(PartKey { zone, index }, rect);
            }
            *zone_plans.get_mut(zone) = zone_plan;
        }
        Splits {
            root,
            column,
            zones: zone_plans,
            root_panes,
            column_panes,
            part_panes,
        }
    }
}

fn bench(
    state: &State,
    workbench: WorkbenchId,
    layout: &Layout<PanelId>,
    viewport: Viewport,
    metrics: &LayoutMetrics,
) -> Bench {
    let min = |panel: PanelId| {
        state
            .panels
            .get(&panel)
            .map_or(shellio_layout::MinSize::DEFAULT, |record| {
                min_size(&record.plugin)
            })
    };
    let base_layout = Layout {
        zoomed: None,
        ..layout.clone()
    };
    let frames = geometry(&base_layout, viewport, metrics, min);
    let zoomed = layout
        .zoomed_panel()
        .and_then(|_| geometry(layout, viewport, metrics, min).zoomed);
    let focus = layout.focused();

    let mut views = BTreeMap::new();
    for panel in layout.panels() {
        let Some(record) = state.panels.get(&panel) else {
            continue;
        };
        let location = layout.locate(panel);
        let host = match (zoomed.as_ref(), location) {
            (Some(zoom), _) if zoom.active == panel => Host::Zoom,
            (_, Some(shellio_layout::Location::Navigator)) => Host::Navigator,
            (_, Some(shellio_layout::Location::Part { zone, part, .. })) => {
                Host::Part(PartKey { zone, index: part })
            }
            (_, None) => continue,
        };
        let shown = match (&zoomed, host) {
            (Some(zoom), Host::Zoom) => Some((zoom.body, zoom.geometry)),
            (Some(_), Host::Navigator | Host::Part(_)) | (None, Host::Zoom) => None,
            (None, Host::Navigator) => frames
                .navigator
                .as_ref()
                .filter(|column| column.panel == panel)
                .map(|column| (column.body, column.geometry)),
            (None, Host::Part(_)) => frames
                .part_of(&panel)
                .filter(|part| part.active == panel)
                .map(|part| (part.body, part.geometry)),
        };
        views.insert(
            panel,
            ViewSlot {
                host,
                body: shown.map(|(body, _)| body),
                geometry: shown.map(|(_, geometry)| geometry),
                title: record.title.as_str().to_owned(),
                plugin: record.plugin.as_str().to_owned(),
            },
        );
    }

    let mut plates = BTreeMap::new();
    for (zone, state) in layout.zones.iter() {
        for (index, part) in state.parts.iter().enumerate() {
            let Some(active) = part.active_panel() else {
                continue;
            };
            let frame = frames
                .parts
                .iter()
                .find(|frame| frame.zone == zone && frame.index == index)
                .cloned();
            let focused = zoomed.is_none()
                && frame.is_some()
                && focus.is_some_and(|focus| part.tabs.contains(&focus));
            plates.insert(
                PartKey { zone, index },
                PlateSlot {
                    tabs: part.tabs.clone(),
                    active,
                    frame,
                    focused,
                },
            );
        }
    }

    Bench {
        workbench,
        layout: layout.clone(),
        frames,
        zoomed,
        views,
        plates,
        focus,
    }
}

/// Элемент сплита вдоль его оси в точках сплита.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Span {
    pub start: f64,
    pub length: f64,
    pub collapsed: bool,
}

/// Элементы сплита и полосы захвата разделителей.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SplitPlan {
    pub spans: Vec<Span>,
    /// Ширина полосы захвата разделителя `i` перед элементом `i + 1`, 0 — не перетаскивается.
    pub grips: Vec<f64>,
}

/// Сплиты окна: корень (колонка навигатора, плитка левой зоны, столбец центра, правая зона),
/// столбец центра (центр над нижней зоной) и части каждой зоны.
#[derive(Clone, Debug, PartialEq)]
pub struct Splits {
    pub root: SplitPlan,
    pub column: SplitPlan,
    pub zones: ZoneMap<SplitPlan>,
    /// Содержимое элементов корня в точках содержимого окна: по одному на элемент.
    pub root_panes: Vec<Rect>,
    pub column_panes: Vec<Rect>,
    pub part_panes: BTreeMap<PartKey, Rect>,
}

/// Элементы сплита длины `total` по началам показанных кадров. Показанный элемент тянется от
/// своего начала до начала следующего показанного, первый показанный — от нуля, последний — до
/// конца: промежуток после плитки лежит в её элементе. Свёрнутый элемент стоит в начале следующего
/// показанного. Полоса захвата разделителя есть, только если оба соседа показаны и разделитель
/// перетаскиваемый (`draggable`).
fn plan(total: f64, starts: &[Option<f64>], draggable: &[bool], gap: f64) -> SplitPlan {
    let first = starts.iter().position(Option::is_some);
    let cut = |index: usize| -> Option<f64> {
        starts[index].map(|start| if Some(index) == first { 0.0 } else { start })
    };
    let next_cut =
        |index: usize| -> f64 { (index + 1..starts.len()).find_map(cut).unwrap_or(total) };
    let spans = (0..starts.len())
        .map(|index| match cut(index) {
            Some(start) => Span {
                start,
                length: (next_cut(index) - start).max(0.0),
                collapsed: false,
            },
            None => {
                let start = (index..starts.len()).find_map(cut).unwrap_or(total);
                Span {
                    start,
                    length: 0.0,
                    collapsed: true,
                }
            }
        })
        .collect();
    let grips = (0..starts.len().saturating_sub(1))
        .map(|divider| {
            let both = starts[divider].is_some() && starts[divider + 1].is_some();
            if both && draggable.get(divider).copied().unwrap_or(false) {
                gap
            } else {
                0.0
            }
        })
        .collect();
    SplitPlan { spans, grips }
}

fn along(axis: Axis, rect: Rect) -> f64 {
    match axis {
        Axis::SideBySide => rect.x,
        Axis::Stacked => rect.y,
    }
}

fn length(axis: Axis, rect: Rect) -> f64 {
    match axis {
        Axis::SideBySide => rect.width,
        Axis::Stacked => rect.height,
    }
}

/// Прямоугольники элементов сплита внутри `rect`.
fn panes(rect: Rect, axis: Axis, spans: &[Span]) -> Vec<Rect> {
    spans.iter().map(|span| pane(rect, axis, *span)).collect()
}

fn pane(rect: Rect, axis: Axis, span: Span) -> Rect {
    match axis {
        Axis::SideBySide => Rect {
            x: rect.x + span.start,
            y: rect.y,
            width: span.length,
            height: rect.height,
        },
        Axis::Stacked => Rect {
            x: rect.x,
            y: rect.y + span.start,
            width: rect.width,
            height: span.length,
        },
    }
}

/// Отступы кадра `frame` от краёв прямоугольника `pane`: слева, сверху, справа, снизу.
pub fn insets(frame: Rect, pane: Rect) -> [f64; 4] {
    [
        frame.x - pane.x,
        frame.y - pane.y,
        pane.right() - frame.right(),
        pane.bottom() - frame.bottom(),
    ]
}

/// Кадр `frame` в точках прямоугольника `origin`.
pub fn relative(frame: Rect, origin: Rect) -> Rect {
    Rect {
        x: frame.x - origin.x,
        y: frame.y - origin.y,
        ..frame
    }
}

/// Операция адаптера над видами.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Создать плитку части.
    AddPlate(PartKey),
    /// Создать вид панели в месте `host`.
    AddView(PanelId, Host),
    /// Переставить вид в другое место без пересоздания.
    Reparent(PanelId, Host),
    Show(PanelId),
    Hide(PanelId),
    /// Убрать вид: панели больше нет в открытой рабочей области.
    RemoveView(PanelId),
    /// Убрать плитку: части больше нет.
    RemovePlate(PartKey),
}

/// Разница двух сцен в операции. Порядок применения: новые плитки, новые виды, перестановки,
/// показ и скрытие, затем удаление видов и плиток, из которых виды уже переставлены.
pub fn diff(before: Option<&Bench>, after: Option<&Bench>) -> Vec<Op> {
    let empty_views = BTreeMap::new();
    let empty_plates = BTreeMap::new();
    let old_views = before.map_or(&empty_views, |bench| &bench.views);
    let new_views = after.map_or(&empty_views, |bench| &bench.views);
    let old_plates: BTreeSet<PartKey> = before
        .map_or(&empty_plates, |bench| &bench.plates)
        .keys()
        .copied()
        .collect();
    let new_plates: BTreeSet<PartKey> = after
        .map_or(&empty_plates, |bench| &bench.plates)
        .keys()
        .copied()
        .collect();

    let mut ops: Vec<Op> = new_plates
        .difference(&old_plates)
        .map(|&key| Op::AddPlate(key))
        .collect();
    let mut visibility = Vec::new();
    for (&panel, slot) in new_views {
        match old_views.get(&panel) {
            None => {
                ops.push(Op::AddView(panel, slot.host));
                visibility.push(show(panel, slot.shown()));
            }
            Some(old) => {
                if old.host != slot.host {
                    ops.push(Op::Reparent(panel, slot.host));
                }
                if old.shown() != slot.shown() {
                    visibility.push(show(panel, slot.shown()));
                }
            }
        }
    }
    ops.extend(visibility);
    ops.extend(
        old_views
            .keys()
            .filter(|panel| !new_views.contains_key(panel))
            .map(|&panel| Op::RemoveView(panel)),
    );
    ops.extend(
        old_plates
            .difference(&new_plates)
            .map(|&key| Op::RemovePlate(key)),
    );
    ops
}

fn show(panel: PanelId, shown: bool) -> Op {
    if shown {
        Op::Show(panel)
    } else {
        Op::Hide(panel)
    }
}

/// Размер содержимого окна в целых точках.
pub fn viewport_of(width: f64, height: f64) -> Viewport {
    let whole = |value: f64| value.round().clamp(0.0, f64::from(u32::MAX)) as u32;
    Viewport {
        width: whole(width),
        height: whole(height),
    }
}

/// Размер окна для ядра (ADR-0009, «Размер окна»): изменения до конца оборота цикла объединяются в
/// одну отправку, одинаковый размер не отправляется повторно.
#[derive(Debug, Default)]
pub struct ViewportGate {
    scheduled: bool,
    sent: Option<Viewport>,
}

impl ViewportGate {
    /// Размер изменился. `true` — отправка ещё не запланирована: её надо поставить на конец оборота.
    pub fn request(&mut self) -> bool {
        !std::mem::replace(&mut self.scheduled, true)
    }

    /// Отправка в конце оборота: текущий размер, если ядро его ещё не получало.
    pub fn flush(&mut self, current: Viewport) -> Option<Viewport> {
        self.scheduled = false;
        if self.sent == Some(current) {
            return None;
        }
        self.sent = Some(current);
        Some(current)
    }
}

/// Сплит, разделитель которого перетащил пользователь.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitId {
    Root,
    Column,
    Zone(Zone),
}

/// Конец перетаскивания разделителя `divider` сплита `split` с новым началом следующего элемента
/// `position` в точках сплита — одна команда `layout.resize` (правило «Разделитель»). Размер зоны —
/// в точках, доля части — в тысячных. `None` — разделитель не меняет размер: колонку навигатора
/// команды P1.4 не меняют.
pub fn resize_command(
    scene: &Scene,
    split: SplitId,
    divider: usize,
    position: f64,
) -> Option<Command> {
    let bench = scene.bench.as_ref()?;
    let area = scene.area;
    let gap = f64::from(scene.metrics.gap);
    let zones = &bench.frames.zones;
    let points = |value: f64| value.round().clamp(0.0, f64::from(u32::MAX)) as u32;
    let zone_size = |zone: Zone, size: f64| Command::LayoutResize {
        workbench: Some(bench.workbench),
        zone,
        part_of: None,
        size: points(size),
    };
    match (split, divider) {
        (SplitId::Root, 1) => {
            let plate = zones.left?;
            Some(zone_size(Zone::Left, position - gap - (plate.x - area.x)))
        }
        (SplitId::Root, 2) => {
            zones.right?;
            Some(zone_size(Zone::Right, area.width - gap - position))
        }
        (SplitId::Column, 0) => {
            zones.bottom?;
            Some(zone_size(Zone::Bottom, area.height - gap - position))
        }
        (SplitId::Zone(zone), divider) => {
            let state = bench.layout.zones.get(zone);
            let part = state.parts.get(divider)?;
            let part_of = part.active_panel()?;
            let axis = zone.axis();
            let zone_rect = match zone {
                Zone::Left => zones.left,
                Zone::Right => zones.right,
                Zone::Center => zones.center,
                Zone::Bottom => zones.bottom,
            }?;
            let frame = bench
                .frames
                .parts
                .iter()
                .find(|frame| frame.zone == zone && frame.index == divider)?;
            let splits = scene.splits();
            let pane = splits.part_panes.get(&PartKey { zone, index: 0 })?;
            let start = along(axis, frame.frame) - along(axis, *pane);
            let count = state.parts.len() as f64;
            let available = length(axis, zone_rect) - gap * (count - 1.0);
            if available <= 0.0 {
                return None;
            }
            let share =
                (position - gap - start) * f64::from(shellio_layout::SHARE_TOTAL) / available;
            Some(Command::LayoutResize {
                workbench: Some(bench.workbench),
                zone,
                part_of: Some(part_of),
                size: points(share),
            })
        }
        (SplitId::Root | SplitId::Column, _) => None,
    }
}

#[cfg(test)]
mod tests {
    use shellio_core::{
        NAVIGATOR_PLUGIN, Panel, PluginId, ProjectId, Route, TERMINAL_PLUGIN, Title, UnixMillis,
        Workbench, WorkspaceId, layout_metrics,
    };
    use shellio_layout::{Part, SHARE_TOTAL};

    use super::*;

    const SMALL: Viewport = Viewport {
        width: 1180,
        height: 720,
    };

    fn id<T: std::str::FromStr>(n: u8) -> T
    where
        T::Err: std::fmt::Debug,
    {
        format!("00000000-0000-0000-0000-0000000000{n:02x}")
            .parse()
            .unwrap()
    }

    struct Fixture {
        state: State,
        workbench: WorkbenchId,
        navigator: PanelId,
        terminal: PanelId,
    }

    /// Открытая рабочая область с раскладкой по умолчанию.
    fn fixture() -> Fixture {
        let mut state = State::default();
        let workspace: WorkspaceId = id(1);
        let project: ProjectId = id(2);
        let workbench: WorkbenchId = id(3);
        state.workspaces.insert(
            workspace,
            shellio_core::Workspace {
                id: workspace,
                title: Title::new("Работа").unwrap(),
                position: 0,
                last_workbench: None,
                deleted_at: None,
            },
        );
        state.projects.insert(
            project,
            shellio_core::Project {
                id: project,
                workspace,
                title: Title::new("shellio").unwrap(),
                list_position: Some(0),
                removed_at: None,
                last_opened_at: None,
            },
        );
        state.workbenches.insert(
            workbench,
            Workbench {
                id: workbench,
                project,
                title: Title::new("main").unwrap(),
                position: 0,
            },
        );
        let navigator: PanelId = id(10);
        let terminal: PanelId = id(11);
        for (panel, plugin, title) in [
            (navigator, NAVIGATOR_PLUGIN, "Навигатор"),
            (terminal, TERMINAL_PLUGIN, "Терминал"),
        ] {
            state.panels.insert(
                panel,
                Panel {
                    id: panel,
                    workbench,
                    plugin: PluginId::new(plugin).unwrap(),
                    title: Title::new(title).unwrap(),
                    created_at: UnixMillis(0),
                },
            );
        }
        state
            .layouts
            .insert(workbench, Layout::default_for(navigator, terminal));
        state.window.active_workspace = Some(workspace);
        state.window.route = Route::Workbench(workbench);
        Fixture {
            state,
            workbench,
            navigator,
            terminal,
        }
    }

    fn add_panel(fixture: &mut Fixture, n: u8) -> PanelId {
        let panel: PanelId = id(n);
        fixture.state.panels.insert(
            panel,
            Panel {
                id: panel,
                workbench: fixture.workbench,
                plugin: PluginId::new(TERMINAL_PLUGIN).unwrap(),
                title: Title::new("Терминал").unwrap(),
                created_at: UnixMillis(0),
            },
        );
        panel
    }

    fn layout_mut(fixture: &mut Fixture) -> &mut Layout<PanelId> {
        fixture.state.layouts.get_mut(&fixture.workbench).unwrap()
    }

    fn scene(fixture: &Fixture) -> Scene {
        Scene::of(&fixture.state, SMALL, layout_metrics())
    }

    #[test]
    fn without_open_workbench_the_area_shows_its_note() {
        let mut fixture = fixture();
        fixture.state.window.route = Route::WorkspaceHome;
        let scene = scene(&fixture);
        assert!(scene.bench.is_none());
        assert_eq!(scene.note(), Some((NO_WORKBENCH, scene.area)));
        assert_eq!(
            scene.area,
            Rect {
                x: 0.0,
                y: 40.0,
                width: 1180.0,
                height: 720.0 - 40.0 - 26.0
            }
        );
        let splits = scene.splits();
        assert!(splits.root.spans.iter().all(|span| span.collapsed));
    }

    #[test]
    fn default_layout_places_navigator_column_and_terminal_in_center() {
        let fixture = fixture();
        let scene = scene(&fixture);
        let bench = scene.bench.as_ref().unwrap();
        assert_eq!(scene.note(), None);
        let navigator = &bench.views[&fixture.navigator];
        assert_eq!(navigator.host, Host::Navigator);
        assert_eq!(
            navigator.body,
            bench.frames.navigator.as_ref().map(|column| column.body)
        );
        let terminal = &bench.views[&fixture.terminal];
        let center = PartKey {
            zone: Zone::Center,
            index: 0,
        };
        assert_eq!(terminal.host, Host::Part(center));
        assert!(terminal.shown());
        assert!(bench.plates[&center].focused);
        assert_eq!(bench.focus, Some(fixture.terminal));

        let splits = scene.splits();
        let spans = &splits.root.spans;
        assert_eq!(
            spans[0],
            Span {
                start: 0.0,
                length: 236.0,
                collapsed: false
            }
        );
        assert!(spans[1].collapsed, "плитка левой зоны пуста");
        assert_eq!((spans[2].start, spans[2].collapsed), (236.0, false));
        assert_eq!(spans[2].start + spans[2].length, 1180.0);
        assert!(spans[3].collapsed && spans[3].start == 1180.0);
        assert_eq!(splits.root.grips, [0.0, 0.0, 0.0]);
        let column = &splits.column.spans;
        assert_eq!(
            (column[0].start, column[0].length, column[0].collapsed),
            (0.0, scene.area.height, false)
        );
        assert!(column[1].collapsed);
        // Плитка центра стоит внутри своего элемента с промежутками вокруг.
        let pane = splits.part_panes[&center];
        let plate = bench.plates[&center].frame.as_ref().unwrap().frame;
        assert_eq!(insets(plate, pane), [0.0, 6.0, 6.0, 6.0]);
        let column = splits.root_panes[0];
        assert_eq!(
            insets(navigator.body.unwrap(), column),
            [6.0, 6.0, 6.0, 6.0]
        );
    }

    #[test]
    fn every_pane_contains_its_plate_for_open_zones() {
        let mut fixture = fixture();
        let right = add_panel(&mut fixture, 12);
        let bottom = add_panel(&mut fixture, 13);
        let left = add_panel(&mut fixture, 14);
        let second = add_panel(&mut fixture, 15);
        let layout = layout_mut(&mut fixture);
        layout
            .zones
            .right
            .parts
            .push(Part::single(right, SHARE_TOTAL));
        layout
            .zones
            .bottom
            .parts
            .push(Part::single(bottom, SHARE_TOTAL));
        layout.zones.left.parts.push(Part::single(left, 500));
        layout.zones.left.parts.push(Part::single(second, 500));
        let scene = scene(&fixture);
        let bench = scene.bench.as_ref().unwrap();
        let splits = scene.splits();
        assert!(splits.root.spans.iter().all(|span| !span.collapsed));
        assert_eq!(splits.root.grips, [0.0, 6.0, 6.0]);
        assert_eq!(splits.column.grips, [6.0]);
        assert_eq!(splits.zones.left.grips, [6.0]);
        let total: f64 = splits.root.spans.iter().map(|span| span.length).sum();
        assert_eq!(total, 1180.0);
        for (key, plate) in &bench.plates {
            let frame = plate.frame.as_ref().unwrap().frame;
            let pane = splits.part_panes[key];
            let [l, t, r, b] = insets(frame, pane);
            assert!(
                l >= 0.0 && t >= 0.0 && r >= 0.0 && b >= 0.0,
                "{key:?}: {frame:?} вне {pane:?}"
            );
            assert!(r <= 6.0 && b <= 6.0, "{key:?}: лишний зазор");
        }
    }

    #[test]
    fn hidden_zone_collapses_its_slot_and_hides_its_views() {
        let mut fixture = fixture();
        let right = add_panel(&mut fixture, 12);
        let layout = layout_mut(&mut fixture);
        layout
            .zones
            .right
            .parts
            .push(Part::single(right, SHARE_TOTAL));
        layout.hidden.right = true;
        let scene = scene(&fixture);
        let bench = scene.bench.as_ref().unwrap();
        assert!(scene.splits().root.spans[3].collapsed);
        let slot = &bench.views[&right];
        assert_eq!(
            slot.host,
            Host::Part(PartKey {
                zone: Zone::Right,
                index: 0
            })
        );
        assert!(!slot.shown(), "вид скрытой зоны жив, но скрыт");
    }

    #[test]
    fn diff_creates_reparents_hides_and_removes() {
        let mut fixture = fixture();
        let empty = diff(None, scene(&fixture).bench.as_ref());
        let center = PartKey {
            zone: Zone::Center,
            index: 0,
        };
        assert_eq!(
            empty,
            [
                Op::AddPlate(center),
                Op::AddView(fixture.navigator, Host::Navigator),
                Op::AddView(fixture.terminal, Host::Part(center)),
                Op::Show(fixture.navigator),
                Op::Show(fixture.terminal),
            ]
        );
        let before = scene(&fixture);

        let terminal = fixture.terminal;
        let layout = layout_mut(&mut fixture);
        layout.zones.center.parts.clear();
        layout
            .zones
            .right
            .parts
            .push(Part::single(terminal, SHARE_TOTAL));
        let moved = scene(&fixture);
        let right = PartKey {
            zone: Zone::Right,
            index: 0,
        };
        assert_eq!(
            diff(before.bench.as_ref(), moved.bench.as_ref()),
            [
                Op::AddPlate(right),
                Op::Reparent(terminal, Host::Part(right)),
                Op::RemovePlate(center),
            ],
            "перенос переставляет тот же вид"
        );

        layout_mut(&mut fixture).hidden.navigator = true;
        let hidden = scene(&fixture);
        assert_eq!(
            diff(moved.bench.as_ref(), hidden.bench.as_ref()),
            [Op::Hide(fixture.navigator)]
        );

        fixture.state.window.route = Route::WorkspaceHome;
        let closed = scene(&fixture);
        assert_eq!(
            diff(hidden.bench.as_ref(), closed.bench.as_ref()),
            [
                Op::RemoveView(fixture.navigator),
                Op::RemoveView(terminal),
                Op::RemovePlate(right),
            ]
        );
    }

    #[test]
    fn tab_change_only_toggles_visibility() {
        let mut fixture = fixture();
        let second = add_panel(&mut fixture, 12);
        layout_mut(&mut fixture).zones.center.parts[0]
            .tabs
            .push(second);
        let before = scene(&fixture);
        layout_mut(&mut fixture).zones.center.parts[0].active = 1;
        let after = scene(&fixture);
        assert_eq!(
            diff(before.bench.as_ref(), after.bench.as_ref()),
            [Op::Hide(fixture.terminal), Op::Show(second)],
            "смена вкладки не создаёт и не переставляет виды"
        );
    }

    #[test]
    fn zoom_moves_the_view_onto_the_zoom_plate_and_back() {
        let mut fixture = fixture();
        let before = scene(&fixture);
        layout_mut(&mut fixture).zoomed = Some(fixture.terminal);
        let zoomed = scene(&fixture);
        let bench = zoomed.bench.as_ref().unwrap();
        assert!(bench.zoomed.is_some());
        assert_eq!(bench.views[&fixture.terminal].host, Host::Zoom);
        assert_eq!(
            bench.frames,
            before.bench.as_ref().unwrap().frames,
            "деления под развёрнутой панелью не меняются"
        );
        assert_eq!(zoomed.note(), None);
        assert_eq!(
            diff(before.bench.as_ref(), zoomed.bench.as_ref()),
            [
                Op::Reparent(fixture.terminal, Host::Zoom),
                Op::Hide(fixture.navigator),
            ]
        );
        let center = PartKey {
            zone: Zone::Center,
            index: 0,
        };
        assert_eq!(
            diff(zoomed.bench.as_ref(), before.bench.as_ref()),
            [
                Op::Reparent(fixture.terminal, Host::Part(center)),
                Op::Show(fixture.navigator),
            ]
        );
    }

    #[test]
    fn empty_center_shows_no_panels_note_only_without_tiles() {
        let mut fixture = fixture();
        layout_mut(&mut fixture).zones.center.parts.clear();
        let scene = scene(&fixture);
        let (text, rect) = scene.note().unwrap();
        assert_eq!(text, NO_PANELS);
        assert_eq!(rect.x, 236.0);
    }

    #[test]
    fn viewport_gate_coalesces_and_never_repeats() {
        let mut gate = ViewportGate::default();
        assert!(gate.request(), "первое изменение планирует отправку");
        assert!(!gate.request(), "следующие до конца оборота объединяются");
        assert!(!gate.request());
        assert_eq!(gate.flush(SMALL), Some(SMALL));
        assert!(gate.request(), "после отправки снова планируется");
        assert_eq!(gate.flush(SMALL), None, "тот же размер не повторяется");
        let wider = Viewport {
            width: 1440,
            height: 900,
        };
        assert!(gate.request());
        assert_eq!(gate.flush(wider), Some(wider));
        assert_eq!(gate.flush(wider), None);
        assert_eq!(viewport_of(1180.4, 719.6), SMALL);
        assert_eq!(
            viewport_of(-3.0, 0.0),
            Viewport {
                width: 0,
                height: 0
            }
        );
    }

    #[test]
    fn divider_drags_become_one_resize_command() {
        let mut fixture = fixture();
        let right = add_panel(&mut fixture, 12);
        let bottom = add_panel(&mut fixture, 13);
        let second = add_panel(&mut fixture, 14);
        let layout = layout_mut(&mut fixture);
        layout
            .zones
            .right
            .parts
            .push(Part::single(right, SHARE_TOTAL));
        layout
            .zones
            .bottom
            .parts
            .push(Part::single(bottom, SHARE_TOTAL));
        layout.zones.center.parts[0].share = 500;
        layout.zones.center.parts.push(Part::single(second, 500));
        let scene = scene(&fixture);
        let bench = scene.bench.as_ref().unwrap();
        let workbench = Some(fixture.workbench);
        let splits = scene.splits();
        let right_start = splits.root.spans[3].start;
        assert_eq!(
            resize_command(&scene, SplitId::Root, 2, right_start - 10.0),
            Some(Command::LayoutResize {
                workbench,
                zone: Zone::Right,
                part_of: None,
                size: 372 + 10,
            })
        );
        let bottom_start = splits.column.spans[1].start;
        assert_eq!(
            resize_command(&scene, SplitId::Column, 0, bottom_start + 20.0),
            Some(Command::LayoutResize {
                workbench,
                zone: Zone::Bottom,
                part_of: None,
                size: 236 - 20,
            })
        );
        assert_eq!(resize_command(&scene, SplitId::Root, 0, 300.0), None);
        // Разделитель частей центра: новая длина первой части в тысячных доступной ширины.
        let center = bench.frames.zones.center.unwrap();
        let available = center.width - 6.0;
        let first = &splits.zones.center.spans[1];
        let target = (first.start + 30.0) - 6.0;
        let Some(Command::LayoutResize {
            zone,
            part_of,
            size,
            ..
        }) = resize_command(&scene, SplitId::Zone(Zone::Center), 0, first.start + 30.0)
        else {
            panic!("ожидалась команда размера части");
        };
        assert_eq!((zone, part_of), (Zone::Center, Some(fixture.terminal)));
        let expected = (target * 1000.0 / available).round() as u32;
        assert_eq!(size, expected);
    }
}
