//! Сцена раскладки обычного окна без AppKit (ADR-0009, «AppKit-адаптер»): что окно показывает по
//! зеркалу состояния и размеру содержимого. Адаптер окна применяет её к видам. Здесь чистые части
//! адаптера:
//!
//! - [`Scene::of`] — кадры модели, место и видимость вида каждой панели, плитки частей;
//! - [`diff`] — разница двух сцен в операции: создать, переставить, показать или скрыть, убрать;
//! - [`Scene::splits`] — элементы сплитов и прямоугольники их содержимого по кадрам модели;
//! - [`ViewportGate`] — размер окна для ядра с объединением до конца оборота цикла и без повторов;
//! - [`resize_command`] — конец перетаскивания разделителя в одну команду `layout.resize`;
//! - [`Scene::zone_buttons`] и [`toggle_command`] — кнопки зон заголовка и пункты меню «Вид»;
//! - [`focus_command`] — щелчок в тело панели, [`refusal_text`] — отказ ядра в строке статуса;
//! - [`panel_menu`] и [`menu_command`] — меню «…» плитки: перенос в соседнюю часть, в зону и
//!   закрытие;
//! - [`drop_plan`] — отпускание перетаскиваемой вкладки: цель, подсветка и одна команда переноса;
//! - [`Bench::stubs`] и [`expand_command`] — заглушка на месте тела: «Развернуть» с кнопкой у тела
//!   меньше `min_size` панели и причина статуса [`status_text`] у панели не в `ready`.
//!
//! Смена вкладки, перенос и `zoom` вид панели не пересоздают. Что происходит с контейнером и телом
//! плагина, когда панель уходит из открытой рабочей области или из `ready`, решает
//! [`crate::bodies`]. Начало координат — левый верхний угол содержимого окна, как у геометрии.

use std::collections::{BTreeMap, BTreeSet};

use shellio_core::{
    Command, CoreError, Entity, ErrorCode, PanelId, PanelStatus, SplitOpen, State, WorkbenchId,
    min_size, open_workbench, split_open,
};
use shellio_layout::{
    Axis, DropTarget, Edge, Frames, Geometry, Hideable, Layout, LayoutMetrics, Location, PartFrame,
    Point, Rect, Viewport, Zone, ZoneMap, drop_target, geometry,
};
use shellio_plugin_api::Registry;

/// Сообщение области зон без открытой рабочей области.
pub const NO_WORKBENCH: &str = "Рабочая область не открыта";
/// Сообщение на месте центра, когда вне колонки навигатора панелей нет.
pub const NO_PANELS: &str = "Нет открытых панелей. Откройте панель: Вид → Открыть панель";

/// Часть зоны по месту в раскладке.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartKey {
    pub zone: Zone,
    pub index: usize,
}

/// Где стоит вид панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// Показанное тело панели в точках содержимого окна; `None` — тело не показано.
    pub body: Option<Rect>,
    pub geometry: Option<Geometry>,
    /// Показанное тело меньше `min_size` панели: на его месте заглушка «Развернуть», вид скрыт
    /// и жив (ADR-0009, ADR-0031).
    pub stub: bool,
    pub title: String,
    pub plugin: String,
    /// Статус панели: вне `ready` тела плагина нет, на месте показанного тела — заглушка статуса
    /// (ADR-0008).
    pub status: PanelStatus,
}

impl ViewSlot {
    /// Вид панели виден: тело показано, не меньше `min_size`, панель в `ready`.
    pub fn shown(&self) -> bool {
        self.body.is_some() && !self.stub && self.status == PanelStatus::Ready
    }
}

/// Заглушка на месте тела панели: «Развернуть» или причина статуса.
#[derive(Clone, Debug, PartialEq)]
pub struct Stub {
    pub panel: PanelId,
    /// Тело в точках содержимого окна.
    pub body: Rect,
    pub title: String,
    /// Кнопка «Развернуть» есть: у развёрнутой панели разворачивать некуда, у заглушки статуса
    /// кнопки нет.
    pub expandable: bool,
    /// Причина статуса панели не в `ready`: она важнее заглушки `min_size`.
    pub status: Option<String>,
}

/// Текст заглушки статуса панели плагина `plugin` с названием `title`; `None` — панель в `ready`.
pub fn status_text(status: PanelStatus, plugin: &str, title: &str) -> Option<String> {
    match status {
        PanelStatus::Ready => None,
        PanelStatus::PluginMissing => Some(format!("Плагин «{plugin}» недоступен")),
        PanelStatus::Duplicate => Some(format!("Уже открыт: {title}")),
        PanelStatus::StateCorrupt => Some("Состояние не прочитано".to_owned()),
        PanelStatus::InitFailed => Some("Состояние не создано".to_owned()),
        PanelStatus::MigrateFailed => Some("Состояние не перенесено на новую версию".to_owned()),
        PanelStatus::StateNewer => Some("Сохранено более новой версией".to_owned()),
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
    /// Сцена открытой в окне рабочей области по состоянию и размеру содержимого окна. `registry`
    /// даёт `min_size` панелей — тот же, по которому ядро считает кадры `query.layout`.
    pub fn of(
        state: &State,
        registry: &Registry,
        viewport: Viewport,
        metrics: LayoutMetrics,
    ) -> Scene {
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
            Some(bench(
                state, registry, workbench, layout, viewport, &metrics,
            ))
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
    registry: &Registry,
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
                min_size(registry, &record.plugin)
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
            (Some(zoom), Host::Zoom) => Some((zoom.body, zoom.geometry, zoom.stub)),
            (Some(_), Host::Navigator | Host::Part(_)) | (None, Host::Zoom) => None,
            (None, Host::Navigator) => frames
                .navigator
                .as_ref()
                .filter(|column| column.panel == panel)
                .map(|column| (column.body, column.geometry, column.stub)),
            (None, Host::Part(_)) => frames
                .part_of(&panel)
                .filter(|part| part.active == panel)
                .map(|part| (part.body, part.geometry, part.stub)),
        };
        views.insert(
            panel,
            ViewSlot {
                host,
                body: shown.map(|(body, _, _)| body),
                geometry: shown.map(|(_, geometry, _)| geometry),
                stub: shown.is_some_and(|(_, _, stub)| stub),
                title: record.title.as_str().to_owned(),
                plugin: record.plugin.as_str().to_owned(),
                status: state.status_of(panel),
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

impl Bench {
    /// Заглушки по месту: показанное тело панели меньше её `min_size` или панель не в `ready`. Вид
    /// панели под заглушкой скрыт и жив. Колонка навигатора получает заглушку так же, как часть; у
    /// развёрнутой панели и у заглушки статуса кнопки нет. Статус важнее `min_size`: тесная панель
    /// не в `ready` показывает причину статуса.
    pub fn stubs(&self) -> BTreeMap<Host, Stub> {
        self.views
            .iter()
            .filter_map(|(&panel, slot)| {
                let body = slot.body?;
                let status = status_text(slot.status, &slot.plugin, &slot.title);
                (slot.stub || status.is_some()).then(|| {
                    (
                        slot.host,
                        Stub {
                            panel,
                            body,
                            title: slot.title.clone(),
                            expandable: slot.host != Host::Zoom && status.is_none(),
                            status,
                        },
                    )
                })
            })
            .collect()
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
    /// Ширина полосы захвата разделителя `i` после элемента `i` перед следующим показанным
    /// элементом, 0 — не перетаскивается.
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
/// показанного. Полоса захвата разделителя `i` есть, только если элемент `i` показан, после него
/// есть показанный элемент и разделитель перетаскиваемый (`draggable`): между ними могут лежать
/// свёрнутые элементы, например пустой столбец центра между левой и правой зонами.
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
            let both =
                starts[divider].is_some() && starts[divider + 1..].iter().any(Option::is_some);
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

/// Конец перетаскивания разделителя `divider` сплита `split` с новым началом следующего показанного
/// элемента `position` в точках сплита — одна команда `layout.resize` (правило «Разделитель»). Без
/// столбца центра и нижней зоны разделитель плитки левой зоны граничит с правой зоной и меняет
/// размер левой: свободную ширину получает правая. Размер зоны —
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

/// Кнопка группы зон заголовка и пункт меню «Вид» (ADR-0024): колонка навигатора, нижняя зона и
/// третья — правая зона или плитка левой.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ZoneToggle {
    Navigator,
    Bottom,
    Side,
}

impl ZoneToggle {
    pub const ALL: [ZoneToggle; 3] = [ZoneToggle::Navigator, ZoneToggle::Bottom, ZoneToggle::Side];
}

/// Состояние кнопок зон открытой рабочей области: видна ли цель каждой кнопки и куда смотрит
/// третья.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZoneButtons {
    pub navigator: bool,
    pub bottom: bool,
    /// Цель третьей кнопки: [`Hideable::Right`] или [`Hideable::Plate`].
    pub side: Hideable,
    pub side_shown: bool,
}

/// Цель третьей кнопки (ADR-0024, `⌥⌘0`): плитка левой зоны, когда она занята, а правая зона пуста;
/// иначе правая зона. Решает занятость зон, а не их скрытие.
pub fn side_target<P: Copy + Ord>(layout: &Layout<P>) -> Hideable {
    if !layout.zones.left.is_empty() && layout.zones.right.is_empty() {
        Hideable::Plate
    } else {
        Hideable::Right
    }
}

/// Цель видна при текущем скрытии и режиме `only`.
fn target_shown<P: Copy + Ord>(layout: &Layout<P>, target: Hideable) -> bool {
    match target {
        Hideable::Navigator => layout.navigator_shown(),
        Hideable::Plate => layout.zone_shown(Zone::Left),
        Hideable::Right => layout.zone_shown(Zone::Right),
        Hideable::Bottom => layout.zone_shown(Zone::Bottom),
    }
}

fn toggle_target<P: Copy + Ord>(layout: &Layout<P>, toggle: ZoneToggle) -> Hideable {
    match toggle {
        ZoneToggle::Navigator => Hideable::Navigator,
        ZoneToggle::Bottom => Hideable::Bottom,
        ZoneToggle::Side => side_target(layout),
    }
}

impl Scene {
    /// Кнопки зон; `None` — рабочая область не открыта, кнопки недоступны.
    pub fn zone_buttons(&self) -> Option<ZoneButtons> {
        let layout = &self.bench.as_ref()?.layout;
        let side = side_target(layout);
        Some(ZoneButtons {
            navigator: target_shown(layout, Hideable::Navigator),
            bottom: target_shown(layout, Hideable::Bottom),
            side,
            side_shown: target_shown(layout, side),
        })
    }
}

/// Нажатие кнопки зон или пункта меню «Вид» — одна команда `layout.visibility`: видимая цель
/// скрывается, невидимая показывается (показ снимает режим `only`). `None` — рабочая область не
/// открыта.
pub fn toggle_command(scene: &Scene, toggle: ZoneToggle) -> Option<Command> {
    let bench = scene.bench.as_ref()?;
    let target = toggle_target(&bench.layout, toggle);
    Some(Command::LayoutVisibility {
        workbench: Some(bench.workbench),
        target,
        visible: !target_shown(&bench.layout, target),
    })
}

/// Щелчок в тело панели — `panel.focus`. `None` — фокус уже на ней или рабочая область не открыта.
pub fn focus_command(scene: &Scene, panel: PanelId) -> Option<Command> {
    let bench = scene.bench.as_ref()?;
    (bench.focus != Some(panel) && bench.views.contains_key(&panel)).then_some(
        Command::PanelFocus {
            panel: Some(panel),
            workbench: None,
            direction: None,
        },
    )
}

/// Щелчок по вкладке — `panel.select`. `None` — вкладка уже активна и фокус на ней, панели нет или
/// рабочая область не открыта.
pub fn select_command(scene: &Scene, panel: PanelId) -> Option<Command> {
    let bench = scene.bench.as_ref()?;
    let active = bench
        .plates
        .values()
        .any(|plate| plate.active == panel && plate.tabs.contains(&panel));
    (bench.views.contains_key(&panel) && !(active && bench.focus == Some(panel)))
        .then_some(Command::PanelSelect { panel })
}

/// Плитка, на кнопку строки вкладок которой нажали: часть по месту или плитка развёрнутой панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PlateRef {
    Part(PartKey),
    Zoom,
}

/// Активная вкладка плитки `plate` в сцене.
fn plate_panel(bench: &Bench, plate: PlateRef) -> Option<PanelId> {
    match plate {
        PlateRef::Part(key) => bench.plates.get(&key).map(|slot| slot.active),
        PlateRef::Zoom => bench.zoomed.as_ref().map(|zoomed| zoomed.active),
    }
}

/// «Разделить» — `layout.split` активной вкладки плитки с явной целью (ADR-0032): одна команда,
/// фокус в часть переводит само ядро той же ревизией. У развёрнутой плитки кнопки нет. `None` —
/// плитки или рабочей области нет.
pub fn split_command(scene: &Scene, plate: PlateRef) -> Option<Command> {
    let bench = scene.bench.as_ref()?;
    match plate {
        PlateRef::Part(_) => {
            plate_panel(bench, plate).map(|panel| Command::LayoutSplit { panel, open: None })
        }
        PlateRef::Zoom => None,
    }
}

/// «Развернуть панель» — `layout.zoom` активной вкладки части с явной целью; у развёрнутой плитки
/// та же кнопка — «Вернуть раскладку», `layout.zoom` с `off`.
pub fn zoom_command(scene: &Scene, plate: PlateRef) -> Option<Command> {
    let bench = scene.bench.as_ref()?;
    match plate {
        PlateRef::Part(_) => plate_panel(bench, plate).map(|panel| Command::LayoutZoom {
            panel: Some(panel),
            workbench: None,
            off: None,
        }),
        PlateRef::Zoom => bench.zoomed.as_ref().map(|_| Command::LayoutZoom {
            panel: None,
            workbench: Some(bench.workbench),
            off: Some(true),
        }),
    }
}

/// Кнопка заглушки «Развернуть» на месте `host` — `layout.zoom` панели под заглушкой с явной
/// целью, как «Развернуть панель» строки вкладок: одна команда, фокус переводит само ядро. `None` —
/// на месте нет заглушки с кнопкой.
pub fn expand_command(scene: &Scene, host: Host) -> Option<Command> {
    let stub = scene.bench.as_ref()?.stubs().remove(&host)?;
    stub.expandable.then_some(Command::LayoutZoom {
        panel: Some(stub.panel),
        workbench: None,
        off: None,
    })
}

/// Щелчок в тело заглушки на месте `host` мимо кнопки — `panel.focus` её панели, как щелчок в
/// тело вида. `None` — заглушки нет или фокус уже на панели.
pub fn stub_focus_command(scene: &Scene, host: Host) -> Option<Command> {
    let stub = scene.bench.as_ref()?.stubs().remove(&host)?;
    focus_command(scene, stub.panel)
}

/// Кнопка закрытия вкладки — `panel.close`. `None` — панели нет в открытой рабочей области.
pub fn close_command(scene: &Scene, panel: PanelId) -> Option<Command> {
    let bench = scene.bench.as_ref()?;
    bench
        .views
        .contains_key(&panel)
        .then_some(Command::PanelClose { panel })
}

/// Пункт меню «…» плитки (ADR-0031, `docs/ux.md`, «Две панели рядом»). Id пункта — имя действия
/// в keymap (ADR-0007).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PanelMenuItem {
    /// В соседнюю часть вдоль оси зоны: `Before` — левее или выше, `After` — правее или ниже.
    Part(Edge),
    /// В другую зону.
    Zone(Zone),
    Close,
}

impl PanelMenuItem {
    const ALL: [PanelMenuItem; 7] = [
        PanelMenuItem::Part(Edge::Before),
        PanelMenuItem::Part(Edge::After),
        PanelMenuItem::Zone(Zone::Left),
        PanelMenuItem::Zone(Zone::Right),
        PanelMenuItem::Zone(Zone::Bottom),
        PanelMenuItem::Zone(Zone::Center),
        PanelMenuItem::Close,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            PanelMenuItem::Part(Edge::Before) => "panel.move_part.prev",
            PanelMenuItem::Part(Edge::After) => "panel.move_part.next",
            PanelMenuItem::Zone(Zone::Left) => "panel.move.left",
            PanelMenuItem::Zone(Zone::Right) => "panel.move.right",
            PanelMenuItem::Zone(Zone::Bottom) => "panel.move.down",
            PanelMenuItem::Zone(Zone::Center) => "panel.move.up",
            PanelMenuItem::Close => "panel.close",
        }
    }

    /// Пункт по id; `None` — чужой id, например заголовок нагрузки.
    pub fn of_id(id: &str) -> Option<PanelMenuItem> {
        PanelMenuItem::ALL.into_iter().find(|item| item.id() == id)
    }

    /// Название пункта в зоне `zone`: соседняя часть называется по оси зоны.
    pub const fn title(self, zone: Zone) -> &'static str {
        match (self, zone.axis()) {
            (PanelMenuItem::Part(Edge::Before), Axis::SideBySide) => "В часть левее",
            (PanelMenuItem::Part(Edge::After), Axis::SideBySide) => "В часть правее",
            (PanelMenuItem::Part(Edge::Before), Axis::Stacked) => "В часть выше",
            (PanelMenuItem::Part(Edge::After), Axis::Stacked) => "В часть ниже",
            (PanelMenuItem::Zone(Zone::Left), _) => "В левую зону",
            (PanelMenuItem::Zone(Zone::Right), _) => "В правую зону",
            (PanelMenuItem::Zone(Zone::Bottom), _) => "В нижнюю зону",
            (PanelMenuItem::Zone(Zone::Center), _) => "В центр",
            (PanelMenuItem::Close, _) => "Закрыть",
        }
    }
}

/// Строка меню «…»: пункт с названием или разделитель с id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuRow {
    Item(PanelMenuItem, &'static str),
    Separator(&'static str),
}

/// Меню «…» плитки с активной вкладкой `panel`: соседние части вдоль оси зоны, если они есть,
/// другие зоны и «Закрыть». Пустое — панель не стоит в части.
pub fn panel_menu<P: Copy + Ord>(layout: &Layout<P>, panel: P) -> Vec<MenuRow> {
    let Some(Location::Part { zone, part, .. }) = layout.locate(panel) else {
        return Vec::new();
    };
    let count = layout.zones.get(zone).parts.len();
    let mut rows = Vec::new();
    let item = |item: PanelMenuItem| MenuRow::Item(item, item.title(zone));
    if part > 0 {
        rows.push(item(PanelMenuItem::Part(Edge::Before)));
    }
    if part + 1 < count {
        rows.push(item(PanelMenuItem::Part(Edge::After)));
    }
    if !rows.is_empty() {
        rows.push(MenuRow::Separator("panel.menu.parts"));
    }
    for target in [Zone::Left, Zone::Right, Zone::Bottom, Zone::Center] {
        if target != zone {
            rows.push(item(PanelMenuItem::Zone(target)));
        }
    }
    rows.push(MenuRow::Separator("panel.menu.zones"));
    rows.push(item(PanelMenuItem::Close));
    rows
}

/// Пункт меню «…» плитки `plate` — одна команда для её активной вкладки: соседняя часть —
/// `panel.move_part` с якорем, активной вкладкой соседней части, без края (вкладка встаёт в неё
/// последней); зона — `panel.move`; «Закрыть» — `panel.close`. Соседняя часть берётся по раскладке
/// в момент выбора. `None` — плитки, панели или соседней части уже нет.
pub fn menu_command(scene: &Scene, plate: PlateRef, item: PanelMenuItem) -> Option<Command> {
    let bench = scene.bench.as_ref()?;
    let panel = plate_panel(bench, plate)?;
    match item {
        PanelMenuItem::Part(edge) => {
            let Location::Part { zone, part, .. } = bench.layout.locate(panel)? else {
                return None;
            };
            let neighbour = match edge {
                Edge::Before => part.checked_sub(1)?,
                Edge::After => part + 1,
            };
            let anchor = bench
                .layout
                .zones
                .get(zone)
                .parts
                .get(neighbour)?
                .active_panel()?;
            Some(Command::PanelMovePart {
                panel,
                anchor,
                edge: None,
            })
        }
        PanelMenuItem::Zone(zone) => Some(Command::PanelMove { panel, zone }),
        PanelMenuItem::Close => Some(Command::PanelClose { panel }),
    }
}

/// Цель отпускания перетаскиваемой вкладки.
#[derive(Clone, Debug, PartialEq)]
pub struct DropPlan {
    /// Плитка цели.
    pub part: PartKey,
    /// Подсветка в точках плитки: вся плитка или её половина со стороны новой части.
    pub highlight: Rect,
    /// Одна команда `panel.move_part`.
    pub command: Command,
}

/// Вкладка `panel`, отпущенная в точке `point` содержимого окна (ADR-0031): цель по
/// [`drop_target`] — строка вкладок или тело части переносят вкладку в эту часть последней, полоса
/// края тела вдоль оси зоны — в новую часть с этой стороны. Отмена (`None`): точка вне частей,
/// развёрнутая панель, вкладка не из части, собственная часть без края или с краем у одиночной
/// вкладки (как в прототипе: перенос ничего бы не изменил). Место перенос не проверяет (ADR-0009).
pub fn drop_plan(scene: &Scene, panel: PanelId, point: Point) -> Option<DropPlan> {
    let bench = scene.bench.as_ref()?;
    if bench.zoomed.is_some() {
        return None;
    }
    let Location::Part {
        zone: from_zone,
        part: from,
        ..
    } = bench.layout.locate(panel)?
    else {
        return None;
    };
    let source_tabs = bench
        .layout
        .zones
        .get(from_zone)
        .parts
        .get(from)?
        .tabs
        .len();
    let (anchor, edge) = match drop_target(&bench.frames, point, &scene.metrics) {
        DropTarget::Part { anchor } => (anchor, None),
        DropTarget::NewPart { anchor, edge } => (anchor, Some(edge)),
        DropTarget::None => return None,
    };
    let target = bench.frames.part_of(&anchor)?;
    let own = target.zone == from_zone && target.index == from;
    if own && (edge.is_none() || source_tabs < 2) {
        return None;
    }
    let (width, height) = (target.frame.width, target.frame.height);
    let whole = Rect {
        x: 0.0,
        y: 0.0,
        width,
        height,
    };
    let (half_w, half_h) = ((width / 2.0).floor(), (height / 2.0).floor());
    let highlight = match (edge, target.zone.axis()) {
        (None, _) => whole,
        (Some(Edge::Before), Axis::SideBySide) => Rect {
            width: half_w,
            ..whole
        },
        (Some(Edge::After), Axis::SideBySide) => Rect {
            x: half_w,
            width: width - half_w,
            ..whole
        },
        (Some(Edge::Before), Axis::Stacked) => Rect {
            height: half_h,
            ..whole
        },
        (Some(Edge::After), Axis::Stacked) => Rect {
            y: half_h,
            height: height - half_h,
            ..whole
        },
    };
    Some(DropPlan {
        part: PartKey {
            zone: target.zone,
            index: target.index,
        },
        highlight,
        command: Command::PanelMovePart {
            panel,
            anchor,
            edge,
        },
    })
}

/// Строка меню «Вид → Открыть панель»: плагин реестра и его название.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenRow {
    pub plugin: &'static str,
    pub title: &'static str,
}

/// Строки меню «Открыть панель»: плагины реестра в порядке регистрации, `terminal` первым.
pub fn open_rows(registry: &Registry) -> Vec<OpenRow> {
    registry
        .iter()
        .map(|plugin| {
            let manifest = plugin.manifest();
            OpenRow {
                plugin: manifest.id,
                title: manifest.title,
            }
        })
        .collect()
}

/// Пункт меню «Открыть панель»: `panel.open` плагина `plugin` в рабочую область, открытую в момент
/// нажатия; экземпляр ищет актор. `None` — рабочая область не открыта.
pub fn open_panel_command(state: &State, plugin: &str) -> Option<Command> {
    let workbench = open_workbench(state)?;
    Some(Command::PanelOpen {
        plugin: plugin.to_owned(),
        workbench: Some(workbench),
        zone: None,
        args: None,
    })
}

/// Пункт выбора «Открыть в новой части». У недоступного причина стоит в названии: поля подсказки
/// у пункта меню нет, а название видно в AX.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerItem {
    pub plugin: &'static str,
    pub title: String,
    pub enabled: bool,
}

/// Пункты выбора для деления панели `panel`: плагины реестра в порядке регистрации, `terminal`
/// первым. Singleton с существующим экземпляром и плагин, которому зона части не подходит,
/// недоступны. `None` — выбор ещё не открывали: доступны все. Окончательно судит актор.
pub fn picker_items(state: &State, registry: &Registry, panel: Option<PanelId>) -> Vec<PickerItem> {
    registry
        .iter()
        .map(|plugin| {
            let manifest = plugin.manifest();
            let verdict = panel.map_or(SplitOpen::Allowed, |panel| {
                split_open(state, registry, panel, plugin.id())
            });
            let (title, enabled) = picker_title(manifest.title, verdict);
            PickerItem {
                plugin: manifest.id,
                title,
                enabled,
            }
        })
        .collect()
}

fn picker_title(title: &str, verdict: SplitOpen) -> (String, bool) {
    match verdict {
        SplitOpen::Allowed => (title.to_owned(), true),
        SplitOpen::SingletonExists(_) => (format!("{title} (уже открыт)"), false),
        SplitOpen::ZoneNotAllowed => (format!("{title} (не для этой зоны)"), false),
    }
}

/// Отказ, по которому интерфейс открывает выбор «Открыть в новой части» вместо сообщения строки
/// статуса (ADR-0009, «Деление»): `split_needs_panel` на `layout.split` без `open`. Ответ — панель,
/// которую делили: выбор адресует её, а не место части, номера частей могут сдвинуться.
pub fn picker_target(command: &Command, error: &CoreError) -> Option<PanelId> {
    match command {
        Command::LayoutSplit { panel, open: None }
            if error.code() == ErrorCode::InvalidArgument
                && error.detail_str("reason") == Some("split_needs_panel") =>
        {
            Some(*panel)
        }
        _ => None,
    }
}

/// Выбран пункт `plugin` выбора: `layout.split` той же панели с `open`.
pub fn open_command(panel: PanelId, plugin: &str) -> Command {
    Command::LayoutSplit {
        panel,
        open: Some(plugin.to_owned()),
    }
}

/// Текст строки статуса для отказа ядра на команду интерфейса (ADR-0031, `docs/ux.md`): что не
/// вышло и сколько места нужно. `title` — название панели по её id.
pub fn refusal_text(
    command: &Command,
    error: &CoreError,
    title: impl Fn(PanelId) -> Option<String>,
) -> String {
    match error.code() {
        ErrorCode::NotFound => {
            return match error.detail_str("entity") {
                Some(name) if name == Entity::Panel.as_str() => "Панели уже нет".to_owned(),
                Some(name) if name == Entity::Workbench.as_str() => {
                    "Рабочая область не найдена".to_owned()
                }
                _ => error.message().to_owned(),
            };
        }
        ErrorCode::NoUi => return "Размер окна ещё не передан ядру".to_owned(),
        ErrorCode::Busy => return "Ядро занято: повторите действие".to_owned(),
        ErrorCode::ShuttingDown => return "Приложение завершается".to_owned(),
        ErrorCode::StorageUnavailable => {
            return "Раскладка не записана: повторите действие".to_owned();
        }
        ErrorCode::InvalidArgument
        | ErrorCode::NameTaken
        | ErrorCode::RootTaken
        | ErrorCode::StorageConflict => {}
    }
    match error.detail_str("reason") {
        Some("split_no_room") => {
            // Деление с `open` называет и панель, которой ещё нет: это новая часть.
            let unknown = match command {
                Command::LayoutSplit { open: Some(_), .. } => "новая часть",
                _ => "часть",
            };
            split_no_room_text(error, title, unknown)
        }
        Some("split_needs_panel") => "У части одна вкладка: новой части нужна панель".to_owned(),
        Some("navigator_not_splittable") => "Колонка навигатора не делится".to_owned(),
        Some("navigator_column") => {
            "Панель стоит в колонке навигатора: частей у колонки нет".to_owned()
        }
        Some("no_open_workbench") => "Рабочая область не открыта".to_owned(),
        Some("zone_empty") => {
            let name = error
                .detail_str("target")
                .or_else(|| error.detail_str("zone"));
            match name {
                Some("navigator") => "Колонка навигатора пуста: в ней нет панели".to_owned(),
                Some("plate" | "left") => "Левая зона пуста: перенесите в неё панель".to_owned(),
                Some("right") => "Правая зона пуста: перенесите в неё панель".to_owned(),
                Some("bottom") => "Нижняя зона пуста: перенесите в неё панель".to_owned(),
                Some("center") => "Центр пуст: перенесите в него панель".to_owned(),
                _ => error.message().to_owned(),
            }
        }
        Some("resize_out_of_range") => {
            let bound = |key: &str| error.detail(key).and_then(serde_json::Value::as_u64);
            let (Some(min), Some(max)) = (bound("min"), bound("max")) else {
                return error.message().to_owned();
            };
            match command {
                Command::LayoutResize {
                    part_of: Some(_), ..
                } => {
                    let percent = |permille: u64| (permille + 5) / 10;
                    format!(
                        "Доля части — от {} до {} % зоны",
                        percent(min),
                        percent(max)
                    )
                }
                _ => format!("Размер зоны — от {min} до {max} точек"),
            }
        }
        Some("zone_not_resizable") => {
            "Размер центра — остаток: меняйте размер соседней зоны".to_owned()
        }
        Some("panel_not_in_zone") => "Панель стоит в другой зоне".to_owned(),
        _ => error.message().to_owned(),
    }
}

/// «Не хватает места для новой части»: нужный и доступный размер каждой затронутой части. Часть
/// без известного названия панели зовётся `unknown`.
fn split_no_room_text(
    error: &CoreError,
    title: impl Fn(PanelId) -> Option<String>,
    unknown: &str,
) -> String {
    let parts = error
        .detail("parts")
        .and_then(serde_json::Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .map(|part| {
                    let size = |key: &str, axis: &str| {
                        part.get(key)
                            .and_then(|size| size.get(axis))
                            .and_then(serde_json::Value::as_f64)
                            .map_or_else(|| "?".to_owned(), |value| format!("{value:.0}"))
                    };
                    let name = part
                        .get("panel")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|panel| panel.parse::<PanelId>().ok())
                        .and_then(&title)
                        .map_or_else(|| unknown.to_owned(), |title| format!("«{title}»"));
                    format!(
                        "{name} — нужно {}×{}, будет {}×{}",
                        size("need", "width"),
                        size("need", "height"),
                        size("have", "width"),
                        size("have", "height"),
                    )
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if parts.is_empty() {
        "Не хватает места для новой части".to_owned()
    } else {
        format!("Не хватает места для новой части: {}", parts.join("; "))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use shellio_core::{
        NAVIGATOR_PLUGIN, Panel, PluginId, ProjectId, Route, TERMINAL_PLUGIN, Title, UnixMillis,
        Workbench, WorkspaceId, layout_metrics,
    };
    use shellio_layout::{Part, SHARE_TOTAL};

    use super::*;

    pub(crate) const SMALL: Viewport = Viewport {
        width: 1180,
        height: 720,
    };

    pub(crate) fn id<T: std::str::FromStr>(n: u8) -> T
    where
        T::Err: std::fmt::Debug,
    {
        format!("00000000-0000-0000-0000-0000000000{n:02x}")
            .parse()
            .unwrap()
    }

    pub(crate) struct Fixture {
        pub(crate) state: State,
        pub(crate) workbench: WorkbenchId,
        pub(crate) navigator: PanelId,
        pub(crate) terminal: PanelId,
    }

    /// Открытая рабочая область с раскладкой по умолчанию.
    pub(crate) fn fixture() -> Fixture {
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

    pub(crate) fn add_panel(fixture: &mut Fixture, n: u8) -> PanelId {
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

    pub(crate) fn layout_mut(fixture: &mut Fixture) -> &mut Layout<PanelId> {
        fixture.state.layouts.get_mut(&fixture.workbench).unwrap()
    }

    /// Продуктовый реестр: `min_size` заполнителей — 200×120.
    pub(crate) fn registry() -> Registry {
        crate::plugins::registry().unwrap()
    }

    fn scene(fixture: &Fixture) -> Scene {
        Scene::of(&fixture.state, &registry(), SMALL, layout_metrics())
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

    /// Размер окна, при котором тело центра раскладки по умолчанию ровно `want` по ширине (при
    /// высоте 720) или по высоте (при ширине 1180).
    fn viewport_for_center_body(fixture: &Fixture, width: bool, want: f64) -> Viewport {
        let center = |viewport: Viewport| {
            let scene = Scene::of(&fixture.state, &registry(), viewport, layout_metrics());
            let body = scene.bench.unwrap().views[&fixture.terminal].body.unwrap();
            if width { body.width } else { body.height }
        };
        (100..1180)
            .map(|size| {
                if width {
                    Viewport {
                        width: size,
                        ..SMALL
                    }
                } else {
                    Viewport {
                        height: size,
                        ..SMALL
                    }
                }
            })
            .find(|&viewport| center(viewport) == want)
            .unwrap()
    }

    #[test]
    fn stub_replaces_the_view_exactly_below_min_size_and_keeps_it_alive() {
        let fixture = fixture();
        let center = Host::Part(PartKey {
            zone: Zone::Center,
            index: 0,
        });
        let min = shellio_layout::MinSize::DEFAULT;
        for width in [true, false] {
            let edge = viewport_for_center_body(
                &fixture,
                width,
                if width { min.width } else { min.height },
            );
            let below = if width {
                Viewport {
                    width: edge.width - 1,
                    ..edge
                }
            } else {
                Viewport {
                    height: edge.height - 1,
                    ..edge
                }
            };
            let fits = Scene::of(&fixture.state, &registry(), edge, layout_metrics());
            let tight = Scene::of(&fixture.state, &registry(), below, layout_metrics());
            let (fits_bench, tight_bench) =
                (fits.bench.as_ref().unwrap(), tight.bench.as_ref().unwrap());

            let slot = &fits_bench.views[&fixture.terminal];
            assert!(slot.shown() && !slot.stub, "граница min_size — не заглушка");
            assert!(fits_bench.stubs().is_empty());
            assert_eq!(expand_command(&fits, center), None);

            let slot = &tight_bench.views[&fixture.terminal];
            assert!(slot.stub && !slot.shown(), "на точку меньше — заглушка");
            assert_eq!(slot.host, center, "вид остаётся на своём месте");
            let stubs = tight_bench.stubs();
            assert_eq!(stubs.len(), 1, "только центр: {stubs:?}");
            assert_eq!(
                stubs[&center],
                Stub {
                    panel: fixture.terminal,
                    body: slot.body.unwrap(),
                    title: "Терминал".into(),
                    expandable: true,
                    status: None,
                }
            );
            assert_eq!(
                expand_command(&tight, center),
                Some(Command::LayoutZoom {
                    panel: Some(fixture.terminal),
                    workbench: None,
                    off: None,
                })
            );
            assert_eq!(
                stub_focus_command(&tight, center),
                None,
                "фокус уже на панели"
            );

            assert_eq!(
                diff(fits.bench.as_ref(), tight.bench.as_ref()),
                [Op::Hide(fixture.terminal)],
                "заглушка только прячет вид"
            );
            assert_eq!(
                diff(tight.bench.as_ref(), fits.bench.as_ref()),
                [Op::Show(fixture.terminal)],
                "и показывает его снова"
            );
        }
    }

    #[test]
    fn every_status_but_ready_shows_its_own_reason_without_a_button() {
        let mut fixture = fixture();
        let center = Host::Part(PartKey {
            zone: Zone::Center,
            index: 0,
        });
        let reasons = [
            (PanelStatus::PluginMissing, "Плагин «terminal» недоступен"),
            (PanelStatus::Duplicate, "Уже открыт: Терминал"),
            (PanelStatus::StateCorrupt, "Состояние не прочитано"),
            (PanelStatus::InitFailed, "Состояние не создано"),
            (
                PanelStatus::MigrateFailed,
                "Состояние не перенесено на новую версию",
            ),
            (PanelStatus::StateNewer, "Сохранено более новой версией"),
        ];
        let ready = scene(&fixture);
        assert_eq!(
            status_text(PanelStatus::Ready, "terminal", "Терминал"),
            None
        );
        assert!(ready.bench.as_ref().unwrap().stubs().is_empty());

        let mut texts = BTreeSet::new();
        for (status, text) in reasons {
            fixture.state.panel_status.insert(fixture.terminal, status);
            let scene = scene(&fixture);
            let bench = scene.bench.as_ref().unwrap();
            let slot = &bench.views[&fixture.terminal];
            assert!(!slot.stub, "места телу хватает");
            assert!(!slot.shown(), "{status:?}: вид панели скрыт под заглушкой");
            assert_eq!(
                bench.stubs()[&center],
                Stub {
                    panel: fixture.terminal,
                    body: slot.body.unwrap(),
                    title: "Терминал".into(),
                    expandable: false,
                    status: Some(text.into()),
                },
                "{status:?}"
            );
            assert_eq!(expand_command(&scene, center), None, "кнопки нет");
            assert_eq!(
                diff(ready.bench.as_ref(), scene.bench.as_ref()),
                [Op::Hide(fixture.terminal)],
                "{status:?}: заглушка статуса только прячет вид"
            );
            texts.insert(text);
        }
        assert_eq!(texts.len(), reasons.len(), "у каждой причины свой текст");
    }

    #[test]
    fn status_wins_over_the_min_size_stub() {
        let mut fixture = fixture();
        let center = Host::Part(PartKey {
            zone: Zone::Center,
            index: 0,
        });
        let edge = viewport_for_center_body(&fixture, true, shellio_layout::MinSize::DEFAULT.width);
        let below = Viewport {
            width: edge.width - 1,
            ..edge
        };
        let cramped = Scene::of(&fixture.state, &registry(), below, layout_metrics());
        let stub = &cramped.bench.as_ref().unwrap().stubs()[&center];
        assert!(
            stub.expandable && stub.status.is_none(),
            "тесная панель в ready"
        );

        fixture
            .state
            .panel_status
            .insert(fixture.terminal, PanelStatus::StateNewer);
        let stale = Scene::of(&fixture.state, &registry(), below, layout_metrics());
        let bench = stale.bench.as_ref().unwrap();
        assert!(
            bench.views[&fixture.terminal].stub,
            "тело по-прежнему тесное"
        );
        let stub = &bench.stubs()[&center];
        assert_eq!(
            stub.status.as_deref(),
            Some("Сохранено более новой версией")
        );
        assert!(
            !stub.expandable,
            "у заглушки статуса кнопки «Развернуть» нет"
        );
        assert_eq!(expand_command(&stale, center), None);
        assert_eq!(
            diff(cramped.bench.as_ref(), stale.bench.as_ref()),
            [],
            "вид был скрыт и остаётся скрытым"
        );
    }

    #[test]
    fn navigator_column_gets_a_stub_and_zoomed_stub_has_no_button() {
        let mut fixture = fixture();
        let low = Viewport {
            width: 1180,
            height: 170,
        };
        let scene = Scene::of(&fixture.state, &registry(), low, layout_metrics());
        let stubs = scene.bench.as_ref().unwrap().stubs();
        assert_eq!(stubs[&Host::Navigator].panel, fixture.navigator);
        assert!(stubs[&Host::Navigator].expandable);
        assert_eq!(
            expand_command(&scene, Host::Navigator),
            Some(Command::LayoutZoom {
                panel: Some(fixture.navigator),
                workbench: None,
                off: None,
            })
        );
        assert_eq!(
            stub_focus_command(&scene, Host::Navigator),
            Some(Command::PanelFocus {
                panel: Some(fixture.navigator),
                workbench: None,
                direction: None,
            })
        );

        layout_mut(&mut fixture).zoomed = Some(fixture.terminal);
        let zoomed = Scene::of(&fixture.state, &registry(), low, layout_metrics());
        let stubs = zoomed.bench.as_ref().unwrap().stubs();
        assert_eq!(stubs.keys().copied().collect::<Vec<_>>(), [Host::Zoom]);
        assert!(
            !stubs[&Host::Zoom].expandable,
            "развёрнутую панель некуда разворачивать"
        );
        assert_eq!(expand_command(&zoomed, Host::Zoom), None);
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

    #[test]
    fn plate_and_side_part_dividers_become_resize_commands() {
        let mut fixture = fixture();
        let left = add_panel(&mut fixture, 12);
        let top = add_panel(&mut fixture, 13);
        let lower = add_panel(&mut fixture, 14);
        let layout = layout_mut(&mut fixture);
        layout
            .zones
            .left
            .parts
            .push(Part::single(left, SHARE_TOTAL));
        layout.zones.right.parts.push(Part::single(top, 500));
        layout.zones.right.parts.push(Part::single(lower, 500));
        let workbench = Some(fixture.workbench);
        let plate = |fixture: &Fixture| {
            let scene = scene(fixture);
            let column_start = scene.splits().root.spans[2].start;
            resize_command(&scene, SplitId::Root, 1, column_start + 10.0)
        };
        let grown = Some(Command::LayoutResize {
            workbench,
            zone: Zone::Left,
            part_of: None,
            size: 372 + 10,
        });
        assert_eq!(plate(&fixture), grown, "колонка навигатора показана");
        layout_mut(&mut fixture).hidden.navigator = true;
        assert_eq!(plate(&fixture), grown, "колонка навигатора скрыта");

        // Разделитель частей правой зоны идёт по высоте: неподвижный оставляет доли как были,
        // сдвиг на 40 точек вверх уменьшает верхнюю часть на 40 точек доступной высоты.
        let scene = scene(&fixture);
        let bench = scene.bench.as_ref().unwrap();
        let available = bench.frames.zones.right.unwrap().height - 6.0;
        let lower_start = scene.splits().zones.right.spans[1].start;
        let share =
            |position: f64| match resize_command(&scene, SplitId::Zone(Zone::Right), 0, position) {
                Some(Command::LayoutResize {
                    zone: Zone::Right,
                    part_of: Some(panel),
                    size,
                    ..
                }) if panel == top => size,
                other => panic!("ожидалась команда размера верхней части, получено {other:?}"),
            };
        assert_eq!(share(lower_start), 500);
        let expected = ((available / 2.0 - 40.0) * 1000.0 / available).round() as u32;
        assert_eq!(share(lower_start - 40.0), expected);
    }

    #[test]
    fn side_zones_without_the_column_share_a_draggable_divider() {
        let mut fixture = fixture();
        let left = add_panel(&mut fixture, 12);
        let terminal = fixture.terminal;
        let layout = layout_mut(&mut fixture);
        layout.zones.center.parts.clear();
        layout
            .zones
            .left
            .parts
            .push(Part::single(left, SHARE_TOTAL));
        layout
            .zones
            .right
            .parts
            .push(Part::single(terminal, SHARE_TOTAL));
        let now = scene(&fixture);
        let splits = now.splits();
        let spans = &splits.root.spans;
        assert!(spans[2].collapsed, "столбец центра пуст");
        assert!(!spans[1].collapsed && !spans[3].collapsed);
        assert_eq!(
            splits.root.grips,
            [0.0, 6.0, 0.0],
            "граница левой и правой зон перетаскивается полосой плитки"
        );
        // Свободную ширину получает правая зона, поэтому граница меняет размер левой.
        let right_start = spans[3].start;
        assert_eq!(
            resize_command(&now, SplitId::Root, 1, right_start + 10.0),
            Some(Command::LayoutResize {
                workbench: Some(fixture.workbench),
                zone: Zone::Left,
                part_of: None,
                size: 372 + 10,
            })
        );

        layout_mut(&mut fixture).hidden.right = true;
        assert_eq!(scene(&fixture).splits().root.grips, [0.0, 0.0, 0.0]);
        let layout = layout_mut(&mut fixture);
        layout.hidden.right = false;
        layout.hidden.plate = true;
        assert_eq!(scene(&fixture).splits().root.grips, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn zone_buttons_follow_occupancy_and_visibility() {
        let mut fixture = fixture();
        let right = add_panel(&mut fixture, 12);
        let left = add_panel(&mut fixture, 13);
        fixture.state.window.route = Route::WorkspaceHome;
        assert_eq!(
            scene(&fixture).zone_buttons(),
            None,
            "без рабочей области кнопки недоступны"
        );
        assert_eq!(
            toggle_command(&scene(&fixture), ZoneToggle::Navigator),
            None
        );
        fixture.state.window.route = Route::Workbench(fixture.workbench);

        // Первая конфигурация: правая занята, третья кнопка — правая зона.
        layout_mut(&mut fixture)
            .zones
            .right
            .parts
            .push(Part::single(right, SHARE_TOTAL));
        let buttons = scene(&fixture).zone_buttons().unwrap();
        assert_eq!(
            buttons,
            ZoneButtons {
                navigator: true,
                bottom: false,
                side: Hideable::Right,
                side_shown: true,
            }
        );
        let toggles: Vec<_> = ZoneToggle::ALL
            .into_iter()
            .map(|toggle| toggle_command(&scene(&fixture), toggle).unwrap())
            .collect();
        let workbench = fixture.workbench;
        let expect = move |target, visible| Command::LayoutVisibility {
            workbench: Some(workbench),
            target,
            visible,
        };
        assert_eq!(
            toggles,
            [
                expect(Hideable::Navigator, false),
                expect(Hideable::Bottom, true),
                expect(Hideable::Right, false),
            ]
        );
        layout_mut(&mut fixture).hidden.right = true;
        let buttons = scene(&fixture).zone_buttons().unwrap();
        assert_eq!((buttons.side, buttons.side_shown), (Hideable::Right, false));
        assert_eq!(
            toggle_command(&scene(&fixture), ZoneToggle::Side),
            Some(expect(Hideable::Right, true))
        );

        // Вторая конфигурация: правая пуста, плитка левой зоны занята — третья кнопка — плитка.
        let layout = layout_mut(&mut fixture);
        layout.hidden.right = false;
        layout.zones.right.parts.clear();
        layout
            .zones
            .left
            .parts
            .push(Part::single(left, SHARE_TOTAL));
        let buttons = scene(&fixture).zone_buttons().unwrap();
        assert_eq!((buttons.side, buttons.side_shown), (Hideable::Plate, true));
        assert_eq!(
            toggle_command(&scene(&fixture), ZoneToggle::Side),
            Some(expect(Hideable::Plate, false))
        );
        layout_mut(&mut fixture).hidden.plate = true;
        assert_eq!(
            toggle_command(&scene(&fixture), ZoneToggle::Side),
            Some(expect(Hideable::Plate, true))
        );
        // Обе боковые заняты — третья снова правая зона.
        layout_mut(&mut fixture)
            .zones
            .right
            .parts
            .push(Part::single(right, SHARE_TOTAL));
        assert_eq!(
            scene(&fixture).zone_buttons().unwrap().side,
            Hideable::Right
        );
    }

    #[test]
    fn clicks_become_select_and_focus_only_when_they_change_something() {
        let mut fixture = fixture();
        let second = add_panel(&mut fixture, 12);
        layout_mut(&mut fixture).zones.center.parts[0]
            .tabs
            .push(second);
        let scene_now = scene(&fixture);
        let focus = scene_now.bench.as_ref().unwrap().focus;
        assert_eq!(focus, Some(fixture.terminal));
        assert_eq!(select_command(&scene_now, fixture.terminal), None);
        assert_eq!(
            select_command(&scene_now, second),
            Some(Command::PanelSelect { panel: second })
        );
        assert_eq!(focus_command(&scene_now, fixture.terminal), None);
        assert_eq!(
            focus_command(&scene_now, fixture.navigator),
            Some(Command::PanelFocus {
                panel: Some(fixture.navigator),
                workbench: None,
                direction: None,
            })
        );
        let stranger: PanelId = id(99);
        assert_eq!(select_command(&scene_now, stranger), None);
        assert_eq!(focus_command(&scene_now, stranger), None);
        fixture.state.window.route = Route::WorkspaceHome;
        assert_eq!(focus_command(&scene(&fixture), fixture.navigator), None);
    }

    /// Центр из двух частей: терминал слева, новая панель справа; фокус на терминале.
    fn two_center_parts(fixture: &mut Fixture) -> PanelId {
        let second = add_panel(fixture, 12);
        let center = &mut layout_mut(fixture).zones.center;
        center.parts[0].share = SHARE_TOTAL / 2;
        center.parts.push(Part::single(second, SHARE_TOTAL / 2));
        second
    }

    #[test]
    fn strip_buttons_become_one_command_with_an_explicit_target() {
        let mut fixture = fixture();
        let second = two_center_parts(&mut fixture);
        let now = scene(&fixture);
        assert_eq!(now.bench.as_ref().unwrap().focus, Some(fixture.terminal));
        let right = PlateRef::Part(PartKey {
            zone: Zone::Center,
            index: 1,
        });
        assert_eq!(
            split_command(&now, right),
            Some(Command::LayoutSplit {
                panel: second,
                open: None,
            }),
            "часть без фокуса адресуется своей активной вкладкой"
        );
        assert_eq!(
            zoom_command(&now, right),
            Some(Command::LayoutZoom {
                panel: Some(second),
                workbench: None,
                off: None,
            })
        );
        assert_eq!(split_command(&now, PlateRef::Zoom), None);
        assert_eq!(zoom_command(&now, PlateRef::Zoom), None, "развёрнутой нет");
        let missing = PlateRef::Part(PartKey {
            zone: Zone::Bottom,
            index: 0,
        });
        assert_eq!(split_command(&now, missing), None);
        assert_eq!(zoom_command(&now, missing), None);

        layout_mut(&mut fixture).zoomed = Some(second);
        let zoomed = scene(&fixture);
        assert_eq!(
            zoom_command(&zoomed, PlateRef::Zoom),
            Some(Command::LayoutZoom {
                panel: None,
                workbench: Some(fixture.workbench),
                off: Some(true),
            }),
            "«Вернуть раскладку» снимает zoom рабочей области"
        );
        assert_eq!(split_command(&zoomed, PlateRef::Zoom), None);

        fixture.state.window.route = Route::WorkspaceHome;
        let home = scene(&fixture);
        assert_eq!(split_command(&home, right), None);
        assert_eq!(zoom_command(&home, right), None);
    }

    #[test]
    fn split_needs_panel_opens_the_picker_instead_of_a_message() {
        let fixture = fixture();
        let panel = fixture.terminal;
        let split = Command::LayoutSplit { panel, open: None };
        let needs = CoreError::split_needs_panel(panel);
        assert_eq!(picker_target(&split, &needs), Some(panel));
        let opened = open_command(panel, TERMINAL_PLUGIN);
        assert_eq!(
            opened,
            Command::LayoutSplit {
                panel,
                open: Some(TERMINAL_PLUGIN.to_owned()),
            }
        );
        assert_eq!(
            picker_target(&opened, &needs),
            None,
            "выбор не зацикливается"
        );
        let room = CoreError::split_no_room(serde_json::json!([]), "нет места".to_owned());
        assert_eq!(picker_target(&split, &room), None);
        assert_eq!(
            picker_target(&split, &CoreError::not_found(Entity::Panel, panel)),
            None
        );
        let zoom = Command::LayoutZoom {
            panel: Some(panel),
            workbench: None,
            off: None,
        };
        assert_eq!(picker_target(&zoom, &needs), None);
    }

    #[test]
    fn open_panel_menu_lists_the_registry_in_registration_order() {
        let registry = registry();
        let rows = open_rows(&registry);
        assert_eq!(
            rows.iter().map(|row| row.plugin).collect::<Vec<_>>(),
            registry
                .iter()
                .map(|plugin| plugin.manifest().id)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            rows[..2],
            [
                OpenRow {
                    plugin: TERMINAL_PLUGIN,
                    title: "Терминал",
                },
                OpenRow {
                    plugin: NAVIGATOR_PLUGIN,
                    title: "Навигатор",
                },
            ],
            "терминал первым"
        );
        for row in rows {
            let id: PluginId = PluginId::new(row.plugin).unwrap();
            assert_eq!(registry.get(&id).unwrap().manifest().title, row.title);
        }
    }

    #[test]
    fn open_panel_item_targets_the_workbench_open_at_the_press() {
        let mut fixture = fixture();
        assert_eq!(
            open_panel_command(&fixture.state, "demo-list"),
            Some(Command::PanelOpen {
                plugin: "demo-list".to_owned(),
                workbench: Some(fixture.workbench),
                zone: None,
                args: None,
            })
        );
        fixture.state.window.route = Route::WorkspaceHome;
        assert_eq!(open_panel_command(&fixture.state, "demo-list"), None);
    }

    #[test]
    fn picker_lists_the_registry_and_disables_open_singletons() {
        let mut fixture = fixture();
        let registry = registry();
        let closed = picker_items(&fixture.state, &registry, None);
        assert_eq!(
            closed.iter().map(|item| item.plugin).collect::<Vec<_>>(),
            open_rows(&registry)
                .iter()
                .map(|row| row.plugin)
                .collect::<Vec<_>>()
        );
        assert!(closed.iter().all(|item| item.enabled));
        let item = |items: &[PickerItem], plugin: &str| {
            items
                .iter()
                .find(|item| item.plugin == plugin)
                .cloned()
                .unwrap()
        };
        let before = picker_items(&fixture.state, &registry, Some(fixture.terminal));
        assert_eq!(
            before[0],
            PickerItem {
                plugin: TERMINAL_PLUGIN,
                title: "Терминал".to_owned(),
                enabled: true,
            },
            "терминал первым и доступен: его подсвечивает выбор"
        );
        assert_eq!(
            item(&before, NAVIGATOR_PLUGIN),
            PickerItem {
                plugin: NAVIGATOR_PLUGIN,
                title: "Навигатор (уже открыт)".to_owned(),
                enabled: false,
            }
        );
        assert_eq!(
            item(&before, "demo-list"),
            PickerItem {
                plugin: "demo-list",
                title: "Демо: список".to_owned(),
                enabled: true,
            }
        );
        let list: PanelId = id(12);
        fixture.state.panels.insert(
            list,
            Panel {
                id: list,
                workbench: fixture.workbench,
                plugin: PluginId::new("demo-list").unwrap(),
                title: Title::new("Демо: список").unwrap(),
                created_at: UnixMillis(0),
            },
        );
        let after = picker_items(&fixture.state, &registry, Some(fixture.terminal));
        assert_eq!(
            item(&after, "demo-list"),
            PickerItem {
                plugin: "demo-list",
                title: "Демо: список (уже открыт)".to_owned(),
                enabled: false,
            }
        );
        assert!(item(&after, TERMINAL_PLUGIN).enabled);
    }

    #[test]
    fn picker_title_names_the_reason_of_a_disabled_item() {
        assert_eq!(
            picker_title("Заметки", SplitOpen::Allowed),
            ("Заметки".to_owned(), true)
        );
        assert_eq!(
            picker_title("Заметки", SplitOpen::SingletonExists(id(9))),
            ("Заметки (уже открыт)".to_owned(), false)
        );
        assert_eq!(
            picker_title("Заметки", SplitOpen::ZoneNotAllowed),
            ("Заметки (не для этой зоны)".to_owned(), false)
        );
    }

    /// Строка вкладок по ADR-0032 в фикстурах боковой зоны 372 с двумя вкладками, центра около
    /// 560 с тремя и половин центра: при любой нагрузке все три кнопки видны внутри строки и не
    /// заходят на вкладки, а нагрузка показана целиком или скрыта в меню «…». Ширины строк — из
    /// настоящей геометрии, ширины вкладок — заголовок «Терминал» шириной 56 точек.
    #[test]
    fn tab_strips_keep_buttons_and_show_the_payload_whole_or_hidden() {
        use shellio_appkit::{Presentation, ZoneSide, tab_parts, tab_strip_layout};

        const TITLE: f64 = 56.0;
        const TITLE_HEIGHT: f64 = 16.0;
        const BUTTONS: usize = 3;

        let mut cases: Vec<(String, Zone, f64, f64, usize)> = Vec::new();
        for viewport in [
            SMALL,
            Viewport {
                width: 1440,
                height: 900,
            },
        ] {
            let mut fixture = fixture();
            let side = add_panel(&mut fixture, 20);
            let side_two = add_panel(&mut fixture, 21);
            let center_two = add_panel(&mut fixture, 22);
            let center_three = add_panel(&mut fixture, 23);
            let layout = layout_mut(&mut fixture);
            layout
                .zones
                .right
                .parts
                .push(Part::single(side, SHARE_TOTAL));
            layout.zones.right.parts[0].tabs.push(side_two);
            layout.zones.center.parts[0]
                .tabs
                .extend([center_two, center_three]);
            let whole = Scene::of(&fixture.state, &registry(), viewport, layout_metrics());
            let halves = {
                let mut halved = Fixture {
                    state: fixture.state.clone(),
                    ..fixture
                };
                let half = add_panel(&mut halved, 24);
                let center = &mut layout_mut(&mut halved).zones.center;
                center.parts[0].share = SHARE_TOTAL / 2;
                center.parts.push(Part::single(half, SHARE_TOTAL / 2));
                Scene::of(&halved.state, &registry(), viewport, layout_metrics())
            };
            for (name, scene) in [("whole", whole), ("halves", halves)] {
                let bench = scene.bench.unwrap();
                for part in &bench.frames.parts {
                    cases.push((
                        format!(
                            "{}×{} {name} {:?}.{}",
                            viewport.width, viewport.height, part.zone, part.index
                        ),
                        part.zone,
                        part.tab_strip.width,
                        part.tab_strip.height,
                        part.tabs.len(),
                    ));
                }
            }
        }
        let side_372 = cases
            .iter()
            .any(|(_, zone, width, _, tabs)| *zone == Zone::Right && *width <= 372.0 && *tabs == 2);
        assert!(
            side_372,
            "есть боковая строка до 372 точек с двумя вкладками"
        );
        let center_560 = cases.iter().any(|(_, zone, width, _, tabs)| {
            *zone == Zone::Center && (500.0..=600.0).contains(width) && *tabs == 3
        });
        assert!(
            center_560,
            "есть центр около 560 точек с тремя вкладками: {cases:?}"
        );

        let (mut shown, mut hidden) = (0, 0);
        for (name, zone, width, height, tabs) in &cases {
            let presentation = Presentation::new(match zone {
                Zone::Left | Zone::Right => ZoneSide::Side,
                Zone::Center => ZoneSide::Center,
                Zone::Bottom => ZoneSide::Bottom,
            });
            assert_eq!(presentation.buttons().len(), BUTTONS);
            let tab = tab_parts(
                TITLE,
                TITLE_HEIGHT,
                presentation.shows_icons(),
                false,
                presentation.reserves_close(),
            )
            .width;
            let widths = vec![tab; *tabs];
            for payload in [None, Some((70.0, 15.0)), Some((150.0, 15.0))] {
                let strip = tab_strip_layout(*width, *height, &widths, payload, BUTTONS);
                assert_eq!(strip.buttons.len(), BUTTONS, "{name}");
                for pair in strip.buttons.windows(2) {
                    assert!(pair[0].right() <= pair[1].x, "{name}: кнопки не налегают");
                }
                for button in &strip.buttons {
                    assert!(
                        button.x >= 0.0 && button.right() <= *width,
                        "{name}: кнопка внутри строки {button:?}"
                    );
                }
                let first = strip.buttons[0].x;
                assert!(
                    strip.clip.right() <= first,
                    "{name}: вкладки не заходят под кнопки"
                );
                match (payload, strip.payload) {
                    (Some((full, _)), Some(frame)) => {
                        assert_eq!(frame.width, full, "{name}: нагрузка целиком");
                        assert!(frame.x >= strip.clip.right() && frame.right() <= first);
                        shown += 1;
                    }
                    (Some(_), None) => {
                        assert!(strip.regime.payload_hidden(), "{name}: нагрузка в меню «…»");
                        hidden += 1;
                    }
                    (None, frame) => assert_eq!(frame, None),
                }
            }
        }
        assert!(
            shown > 0 && hidden > 0,
            "обе ветви: целиком {shown}, скрыта {hidden}"
        );
    }

    #[test]
    fn refusals_read_as_status_line_text() {
        let fixture = fixture();
        let titles = |panel: PanelId| (panel == fixture.terminal).then(|| "Терминал".to_owned());
        let split = Command::LayoutSplit {
            panel: fixture.terminal,
            open: None,
        };
        let parts = serde_json::json!([{
            "panel": fixture.terminal.to_string(),
            "need": {"width": 200.0, "height": 120.0},
            "have": {"width": 151.0, "height": 600.0},
        }]);
        let error = CoreError::split_no_room(parts, "нет места".to_owned());
        assert_eq!(
            refusal_text(&split, &error, titles),
            "Не хватает места для новой части: «Терминал» — нужно 200×120, будет 151×600"
        );
        let opened = open_command(fixture.terminal, TERMINAL_PLUGIN);
        let fresh: PanelId = id(77);
        let two = serde_json::json!([
            {
                "panel": fixture.terminal.to_string(),
                "need": {"width": 200.0, "height": 120.0},
                "have": {"width": 113.0, "height": 606.0},
            },
            {
                "panel": fresh.to_string(),
                "need": {"width": 200.0, "height": 120.0},
                "have": {"width": 113.0, "height": 606.0},
            },
        ]);
        assert_eq!(
            refusal_text(
                &opened,
                &CoreError::split_no_room(two, "нет места".to_owned()),
                titles
            ),
            "Не хватает места для новой части: «Терминал» — нужно 200×120, будет 113×606; \
             новая часть — нужно 200×120, будет 113×606"
        );
        let error = CoreError::split_no_room(serde_json::json!([]), "нет места".to_owned());
        assert_eq!(
            refusal_text(&split, &error, titles),
            "Не хватает места для новой части"
        );
        assert_eq!(
            refusal_text(
                &split,
                &CoreError::split_needs_panel(fixture.terminal),
                titles
            ),
            "У части одна вкладка: новой части нужна панель"
        );
        let visibility = Command::LayoutVisibility {
            workbench: None,
            target: Hideable::Right,
            visible: true,
        };
        assert_eq!(
            refusal_text(
                &visibility,
                &CoreError::zone_empty("target", "right"),
                titles
            ),
            "Правая зона пуста: перенесите в неё панель"
        );
        assert_eq!(
            refusal_text(
                &visibility,
                &CoreError::zone_empty("target", "plate"),
                titles
            ),
            "Левая зона пуста: перенесите в неё панель"
        );
        assert_eq!(
            refusal_text(&visibility, &CoreError::no_open_workbench(), titles),
            "Рабочая область не открыта"
        );
        let zone = Command::LayoutResize {
            workbench: None,
            zone: Zone::Right,
            part_of: None,
            size: 10,
        };
        assert_eq!(
            refusal_text(&zone, &CoreError::resize_out_of_range(200, 640), titles),
            "Размер зоны — от 200 до 640 точек"
        );
        let share = Command::LayoutResize {
            workbench: None,
            zone: Zone::Center,
            part_of: Some(fixture.terminal),
            size: 10,
        };
        assert_eq!(
            refusal_text(&share, &CoreError::resize_out_of_range(150, 850), titles),
            "Доля части — от 15 до 85 % зоны"
        );
        assert_eq!(
            refusal_text(&zone, &CoreError::zone_not_resizable("center"), titles),
            "Размер центра — остаток: меняйте размер соседней зоны"
        );
        assert_eq!(
            refusal_text(&zone, &CoreError::busy(), titles),
            "Ядро занято: повторите действие"
        );
    }

    fn titles(rows: &[MenuRow]) -> Vec<&'static str> {
        rows.iter()
            .map(|row| match row {
                MenuRow::Item(_, title) => title,
                MenuRow::Separator(_) => &"—",
            })
            .copied()
            .collect()
    }

    #[test]
    fn panel_menu_follows_the_zone_axis_and_its_neighbours() {
        let mut fixture = fixture();
        let layout = &scene(&fixture).bench.unwrap().layout;
        assert_eq!(
            titles(&panel_menu(layout, fixture.terminal)),
            [
                "В левую зону",
                "В правую зону",
                "В нижнюю зону",
                "—",
                "Закрыть"
            ],
            "одна часть: соседних частей нет, своей зоны в списке нет"
        );
        assert!(
            panel_menu(layout, fixture.navigator).is_empty(),
            "у колонки навигатора плитки и меню нет"
        );

        let second = two_center_parts(&mut fixture);
        let third = add_panel(&mut fixture, 13);
        let center = &mut layout_mut(&mut fixture).zones.center;
        center.parts[0].share = 334;
        center.parts[1].share = 333;
        center.parts.push(Part::single(third, 333));
        let layout = &scene(&fixture).bench.unwrap().layout;
        assert_eq!(
            titles(&panel_menu(layout, second))[..3],
            ["В часть левее", "В часть правее", "—"],
            "центр делится рядом"
        );
        assert_eq!(
            titles(&panel_menu(layout, fixture.terminal))[..2],
            ["В часть правее", "—"]
        );
        assert_eq!(
            titles(&panel_menu(layout, third))[..2],
            ["В часть левее", "—"]
        );

        let (up, down) = (add_panel(&mut fixture, 14), add_panel(&mut fixture, 15));
        let right = &mut layout_mut(&mut fixture).zones.right;
        right.parts.push(Part::single(up, SHARE_TOTAL / 2));
        right.parts.push(Part::single(down, SHARE_TOTAL / 2));
        let layout = &scene(&fixture).bench.unwrap().layout;
        assert_eq!(
            titles(&panel_menu(layout, up)),
            [
                "В часть ниже",
                "—",
                "В левую зону",
                "В нижнюю зону",
                "В центр",
                "—",
                "Закрыть"
            ],
            "боковая зона делится одна над другой"
        );
        assert_eq!(titles(&panel_menu(layout, down))[0], "В часть выше");

        for item in PanelMenuItem::ALL {
            assert_eq!(PanelMenuItem::of_id(item.id()), Some(item));
        }
        assert_eq!(PanelMenuItem::of_id(shellio_appkit::PAYLOAD_ITEM), None);
        let ids: BTreeSet<&str> = PanelMenuItem::ALL.iter().map(|item| item.id()).collect();
        assert_eq!(ids.len(), PanelMenuItem::ALL.len());
    }

    #[test]
    fn menu_items_become_one_command_for_the_active_tab() {
        let mut fixture = fixture();
        let second = two_center_parts(&mut fixture);
        let guest = add_panel(&mut fixture, 13);
        let part = &mut layout_mut(&mut fixture).zones.center.parts[1];
        part.tabs.push(guest);
        part.active = 1;
        let now = scene(&fixture);
        let left = PlateRef::Part(PartKey {
            zone: Zone::Center,
            index: 0,
        });
        let right = PlateRef::Part(PartKey {
            zone: Zone::Center,
            index: 1,
        });
        assert_eq!(
            menu_command(&now, left, PanelMenuItem::Part(Edge::After)),
            Some(Command::PanelMovePart {
                panel: fixture.terminal,
                anchor: guest,
                edge: None,
            }),
            "якорь — активная вкладка соседней части"
        );
        assert_eq!(
            menu_command(&now, left, PanelMenuItem::Part(Edge::Before)),
            None
        );
        assert_eq!(
            menu_command(&now, right, PanelMenuItem::Part(Edge::Before)),
            Some(Command::PanelMovePart {
                panel: guest,
                anchor: fixture.terminal,
                edge: None,
            })
        );
        assert_eq!(
            menu_command(&now, right, PanelMenuItem::Zone(Zone::Bottom)),
            Some(Command::PanelMove {
                panel: guest,
                zone: Zone::Bottom,
            })
        );
        assert_eq!(
            menu_command(&now, left, PanelMenuItem::Close),
            Some(Command::PanelClose {
                panel: fixture.terminal
            })
        );
        assert_eq!(
            close_command(&now, second),
            Some(Command::PanelClose { panel: second }),
            "кнопка закрытия неактивной вкладки закрывает её"
        );
        assert_eq!(close_command(&now, id(99)), None);
        let missing = PlateRef::Part(PartKey {
            zone: Zone::Bottom,
            index: 0,
        });
        assert_eq!(menu_command(&now, missing, PanelMenuItem::Close), None);

        layout_mut(&mut fixture).zoomed = Some(guest);
        let zoomed = scene(&fixture);
        assert_eq!(
            menu_command(&zoomed, PlateRef::Zoom, PanelMenuItem::Zone(Zone::Right)),
            Some(Command::PanelMove {
                panel: guest,
                zone: Zone::Right,
            }),
            "развёрнутая плитка переносит свою панель"
        );
        assert_eq!(
            menu_command(&zoomed, PlateRef::Zoom, PanelMenuItem::Part(Edge::Before)),
            Some(Command::PanelMovePart {
                panel: guest,
                anchor: fixture.terminal,
                edge: None,
            })
        );
    }

    fn part_frame(scene: &Scene, zone: Zone, index: usize) -> PartFrame<PanelId> {
        scene
            .bench
            .as_ref()
            .unwrap()
            .frames
            .parts
            .iter()
            .find(|part| part.zone == zone && part.index == index)
            .cloned()
            .unwrap()
    }

    fn middle(rect: Rect) -> Point {
        Point {
            x: rect.x + rect.width / 2.0,
            y: rect.y + rect.height / 2.0,
        }
    }

    #[test]
    fn drop_plan_targets_parts_and_edges_and_cancels_elsewhere() {
        let mut fixture = fixture();
        let second = two_center_parts(&mut fixture);
        let guest = add_panel(&mut fixture, 13);
        let part = &mut layout_mut(&mut fixture).zones.center.parts[1];
        part.tabs.push(guest);
        part.active = 1;
        let now = scene(&fixture);
        let left = part_frame(&now, Zone::Center, 0);
        let right = part_frame(&now, Zone::Center, 1);
        let key = |index| PartKey {
            zone: Zone::Center,
            index,
        };
        let whole = |frame: &PartFrame<PanelId>| Rect {
            x: 0.0,
            y: 0.0,
            width: frame.frame.width,
            height: frame.frame.height,
        };

        let body = drop_plan(&now, guest, middle(left.body)).unwrap();
        assert_eq!(body.part, key(0));
        assert_eq!(body.highlight, whole(&left), "часть подсвечивается целиком");
        assert_eq!(
            body.command,
            Command::PanelMovePart {
                panel: guest,
                anchor: fixture.terminal,
                edge: None,
            }
        );
        let strip = drop_plan(&now, guest, middle(left.tab_strip)).unwrap();
        assert_eq!(strip.command, body.command, "строка вкладок — та же цель");

        let edge = Point {
            x: left.body.right() - 5.0,
            y: middle(left.body).y,
        };
        let after = drop_plan(&now, guest, edge).unwrap();
        assert_eq!(
            after.command,
            Command::PanelMovePart {
                panel: guest,
                anchor: fixture.terminal,
                edge: Some(Edge::After),
            }
        );
        let half = (left.frame.width / 2.0).floor();
        assert_eq!(
            after.highlight,
            Rect {
                x: half,
                width: left.frame.width - half,
                ..whole(&left)
            },
            "новая часть правее — правая половина плитки"
        );

        let own_edge = Point {
            x: right.body.x + 5.0,
            y: middle(right.body).y,
        };
        let split_off = drop_plan(&now, guest, own_edge).unwrap();
        assert_eq!(
            split_off.command,
            Command::PanelMovePart {
                panel: guest,
                anchor: guest,
                edge: Some(Edge::Before),
            },
            "край своей части с двумя вкладками отделяет вкладку"
        );
        assert_eq!(
            split_off.highlight,
            Rect {
                width: (right.frame.width / 2.0).floor(),
                ..whole(&right)
            }
        );
        assert_eq!(
            drop_plan(&now, second, middle(right.body)),
            None,
            "своя часть без края — отмена"
        );
        let lone_edge = Point {
            x: left.body.x + 5.0,
            y: middle(left.body).y,
        };
        assert_eq!(
            drop_plan(&now, fixture.terminal, lone_edge),
            None,
            "край своей части с одной вкладкой ничего не меняет — отмена"
        );
        assert_eq!(
            drop_plan(&now, fixture.terminal, Point { x: 100.0, y: 300.0 }),
            None,
            "колонка навигатора — не цель"
        );
        assert_eq!(
            drop_plan(&now, fixture.terminal, Point { x: 600.0, y: 10.0 }),
            None,
            "заголовок окна — отмена"
        );
        assert_eq!(
            drop_plan(&now, fixture.navigator, middle(right.body)),
            None,
            "колонку навигатора не перетаскивают"
        );

        layout_mut(&mut fixture).zoomed = Some(guest);
        assert_eq!(
            drop_plan(&scene(&fixture), fixture.terminal, middle(right.body)),
            None,
            "под развёрнутой панелью целей нет"
        );
    }

    #[test]
    fn drop_on_a_stacked_edge_highlights_the_lower_half() {
        let mut fixture = fixture();
        let (up, down) = (add_panel(&mut fixture, 14), add_panel(&mut fixture, 15));
        let right = &mut layout_mut(&mut fixture).zones.right;
        right.parts.push(Part::single(up, SHARE_TOTAL / 2));
        right.parts.push(Part::single(down, SHARE_TOTAL / 2));
        let now = scene(&fixture);
        let lower = part_frame(&now, Zone::Right, 1);
        let point = Point {
            x: middle(lower.body).x,
            y: lower.body.bottom() - 5.0,
        };
        let plan = drop_plan(&now, fixture.terminal, point).unwrap();
        assert_eq!(
            plan.command,
            Command::PanelMovePart {
                panel: fixture.terminal,
                anchor: down,
                edge: Some(Edge::After),
            }
        );
        let half = (lower.frame.height / 2.0).floor();
        assert_eq!(
            plan.highlight,
            Rect {
                x: 0.0,
                y: half,
                width: lower.frame.width,
                height: lower.frame.height - half,
            },
            "новая часть ниже — нижняя половина плитки"
        );
    }

    #[test]
    fn moving_a_tab_out_of_a_vanishing_part_reparents_the_same_views() {
        let mut fixture = fixture();
        let second = two_center_parts(&mut fixture);
        let before = scene(&fixture);
        let moved = before
            .bench
            .as_ref()
            .unwrap()
            .layout
            .move_to_part(fixture.terminal, second, None)
            .unwrap();
        *layout_mut(&mut fixture) = moved;
        let after = scene(&fixture);
        let left = PartKey {
            zone: Zone::Center,
            index: 0,
        };
        assert_eq!(
            after.bench.as_ref().unwrap().plates[&left].tabs,
            [second, fixture.terminal]
        );
        let ops = diff(before.bench.as_ref(), after.bench.as_ref());
        assert_eq!(
            ops,
            [
                Op::Reparent(second, Host::Part(left)),
                Op::Hide(second),
                Op::RemovePlate(PartKey {
                    zone: Zone::Center,
                    index: 1,
                }),
            ],
            "номера частей сдвинулись, а виды переставлены без пересоздания"
        );
    }
}
