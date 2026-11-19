//! Сцена вида diff: модель на главном потоке, worker разбора, индекса высот и подсветки,
//! клавиши и мышь. Сцена общая для вариантов A и B: вид только рисует и применяет разницу, а
//! состояние и обработчики живут здесь, поэтому клавиша, мышь и сценарий вызывают одни
//! обработчики.
//!
//! До клавиши открытия в памяти только сырые байты фикстуры, а worker уже созданы: компиляция
//! запроса подсветки не входит в открытие. Доставка с worker — сообщение в канал и вызов
//! [`pump`] на главной очереди dispatch, опроса таймером нет.
//!
//! Каждое изменение содержимого — новое поколение, объявленное через `drawmark::declare` вместе
//! со строками показа, которые оно затронуло. Открытие и новый индекс высот затрагивают все
//! строки, снимок подсветки и движение курсора — только свои. Прокрутка поколения не меняет.
//!
//! Сцена не держит заимствование модели, пока зовёт вид: `scrollToPoint` может сразу вернуть
//! уведомление о прокрутке в [`Scene::scrolled`].

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use dispatch2::DispatchQueue;
use serde::Serialize;

use probe_diff_view::fixtures::Fixture;
use probe_diff_view::highlight::store::{Changed, StyleStore};
use probe_diff_view::highlight::{HighlightWorker, LineSet, Sides, Snapshot, Source};
use probe_diff_view::keymap::{Action, Decoder, KeyInput};
use probe_diff_view::model::heights::{
    HeightIndex, HeightJob, HeightSnapshot, HeightWorker, Metrics,
};
use probe_diff_view::model::layout::{DiffGeometry, DiffLayout, Geometry, TextMetrics, Zone};
use probe_diff_view::model::state::{Anchor, Command, ViewState, Viewport};
use probe_diff_view::model::{
    Diff, DiffSnapshot, LineAddr, LineKind, Mode, ParseOutcome, ParseWorker, Side, TextBody,
};
use probe_diff_view::worker::Delivery;

use crate::scenario::drawmark::{self, Declared};
use crate::scenario::sampler;

/// Ширина разделителя половин «Рядом» в точках.
pub const DIVIDER: f64 = 1.0;
/// Стили держатся для строк не дальше стольких экранов от видимых.
const STYLE_SCREENS: u32 = 3;

/// Вид варианта отрисовки глазами сцены.
pub trait Surface {
    /// Новый diff или индекс высот: высота документа и всё содержимое заново.
    fn reload(&self, height: f64);
    /// Строки показа с новым содержимым: перерисовать их и сбросить их раскладку.
    fn invalidate(&self, rows: &[Range<u32>]);
    /// Верхний край видимой области на `y` через `scrollToPoint` у clip view.
    fn scroll_to(&self, y: f64);
    /// Видимая область документа.
    fn viewport(&self) -> Visible;
}

/// Видимая область документа в точках.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Visible {
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Метрики шрифта, которые вид передаёт модели.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontMetrics {
    pub char_width: f64,
    pub line_height: u32,
}

/// Применённый снимок подсветки.
#[derive(Clone, Debug, Serialize)]
pub struct Applied {
    pub source: Source,
    pub request: u64,
    pub at_ns: u64,
    /// Поколение после применения: прежнее, если строки не изменились.
    pub generation: u64,
    pub changed_rows: usize,
    pub compute_ms: f64,
}

/// Отметки открытия по `CLOCK_UPTIME_RAW`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Timeline {
    pub open_ns: Option<u64>,
    pub parsed_ns: Option<u64>,
    /// Модель и индекс высот готовы, поколение открытия объявлено.
    pub model_ns: Option<u64>,
    pub open_generation: Option<u64>,
    pub rows: Option<u32>,
    pub document_height: Option<u64>,
    pub snapshots: Vec<Applied>,
}

enum Msg {
    Parsed(Delivery<ParseOutcome>),
    Heights(Delivery<Arc<HeightSnapshot>>),
    Styles(Snapshot),
}

/// Раскладка, под которую строится индекс высот: геометрия части зоны, показанный режим и
/// перенос.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Target {
    pub geometry: DiffGeometry,
    pub mode: Mode,
    pub metrics: Metrics,
}

impl Target {
    /// Раскладка текста `body` в видимой области центра. `None` — ниже `min_size`.
    fn of(body: &TextBody, font: FontMetrics, requested: Mode, visible: Visible) -> Option<Self> {
        let metrics = TextMetrics {
            char_width: font.char_width,
            digits: TextMetrics::digits_of(max_line(body)),
            divider: DIVIDER,
        };
        let DiffLayout::Diff(geometry) =
            metrics.adapt(Geometry::new(Zone::Center, visible.width, visible.height))
        else {
            return None;
        };
        let mode = geometry.mode(requested);
        Some(Target {
            geometry,
            mode,
            metrics: Metrics {
                line_height: font.line_height,
                wrap: geometry.wrap(mode),
            },
        })
    }
}

/// Перестройка индекса высот после смены ширины: от запроса до установки.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Relayout {
    /// Запрошено перестроек на worker.
    pub submitted: u64,
    /// Установлено перестроенных индексов.
    pub installed: u64,
    /// Смен геометрии без перестройки: перенос и режим те же.
    pub geometry_only: u64,
    /// Смен режима при установке.
    pub mode_switches: u64,
    pub last_submit_ns: Option<u64>,
    pub last_install_ns: Option<u64>,
}

/// Состояние вида на главном потоке. Вид читает его при отрисовке. `geometry`, `mode`,
/// `metrics` и `heights` меняются только вместе: цель перестройки ждёт в `pending`, иначе вид
/// нарисовал бы новый перенос по старым высотам.
pub struct Model {
    pub diff: Option<Arc<DiffSnapshot>>,
    pub file: usize,
    pub requested: Mode,
    pub mode: Mode,
    pub geometry: Option<DiffGeometry>,
    pub metrics: Option<Metrics>,
    pub heights: Option<HeightIndex>,
    pub state: Option<ViewState>,
    pub styles: StyleStore,
    pub generation: u64,
    pub font: FontMetrics,
    pub pending: Option<Target>,
    pub relayout: Relayout,
    last_visible: Option<Sides<LineSet>>,
}

/// Части модели, нужные для отрисовки открытого текстового файла.
pub struct Shown<'a> {
    pub diff: &'a Diff,
    pub body: &'a TextBody,
    pub heights: &'a HeightIndex,
    pub geometry: DiffGeometry,
    pub metrics: Metrics,
}

impl Model {
    pub fn shown(&self) -> Option<Shown<'_>> {
        let diff = &self.diff.as_ref()?.diff;
        Some(Shown {
            diff,
            body: diff.file(self.file)?.text()?,
            heights: self.heights.as_ref()?,
            geometry: self.geometry?,
            metrics: self.metrics?,
        })
    }

    /// Раскладка установленного индекса высот.
    pub fn installed(&self) -> Option<Target> {
        self.heights.as_ref()?;
        Some(Target {
            geometry: self.geometry?,
            mode: self.mode,
            metrics: self.metrics?,
        })
    }

    fn viewport(&self, height: f64) -> Option<Viewport<'_>> {
        let shown = self.shown()?;
        Some(Viewport {
            body: shown.body,
            heights: shown.heights,
            height: height.max(0.0) as u64,
        })
    }
}

/// Сторона, стили которой рисуются в половине строки показа: в unified у контекстной и
/// добавленной строки новая, у удалённой старая.
pub fn drawn_side(kind: LineKind, mode: Mode, half: Side) -> Side {
    match mode {
        Mode::SideBySide => half,
        Mode::Unified => match kind {
            LineKind::Removed => Side::Old,
            LineKind::Context | LineKind::Added => Side::New,
        },
    }
}

/// Строки файла сторон, которые нужны видимым строкам показа `rows`.
pub fn visible_lines(body: &TextBody, mode: Mode, rows: Range<u32>) -> Sides<LineSet> {
    let mut lines: Sides<Vec<u32>> = Sides::default();
    for row in rows {
        let Some(halves) = body.row(mode, row) else {
            continue;
        };
        for half in Side::BOTH {
            let Some(line) = halves.half(half).and_then(|index| body.line(index)) else {
                continue;
            };
            let side = drawn_side(line.kind(), mode, half);
            if let Some(number) = line.line(side) {
                lines.get_mut(side).push(number);
            }
            if mode == Mode::Unified {
                break;
            }
        }
    }
    lines.map(|_, numbers| LineSet::from_lines(numbers.iter().copied()))
}

/// Строки показа, которые рисуют изменившиеся строки сторон, слитыми полуинтервалами.
pub fn changed_rows(body: &TextBody, mode: Mode, changed: &Changed) -> Vec<Range<u32>> {
    let mut rows: Vec<u32> = Vec::new();
    for side in Side::BOTH {
        for &number in changed.get(side) {
            let Some(index) = body.line_of_addr(LineAddr { side, line: number }) else {
                continue;
            };
            let Some(line) = body.line(index) else {
                continue;
            };
            let Some(row) = body.row_of_line(mode, index) else {
                continue;
            };
            let drawn = match mode {
                Mode::SideBySide => true,
                Mode::Unified => drawn_side(line.kind(), mode, side) == side,
            };
            if drawn {
                rows.push(row);
            }
        }
    }
    merge_rows(rows)
}

/// Номера строк в слитые полуинтервалы.
pub fn merge_rows(mut rows: Vec<u32>) -> Vec<Range<u32>> {
    rows.sort_unstable();
    rows.dedup();
    let mut ranges: Vec<Range<u32>> = Vec::new();
    for row in rows {
        match ranges.last_mut() {
            Some(last) if last.end == row => last.end = row + 1,
            _ => ranges.push(row..row + 1),
        }
    }
    ranges
}

/// Самый большой номер строки файла: для ширины колонки номеров.
pub fn max_line(body: &TextBody) -> u32 {
    body.hunks()
        .iter()
        .map(|hunk| (hunk.old_start + hunk.old_count).max(hunk.new_start + hunk.new_count))
        .max()
        .unwrap_or(1)
}

/// Строки показа курсора и выделения: что перерисовать при их смене.
fn marked_rows(
    state: &ViewState,
    view: &Viewport<'_>,
    mode: Mode,
) -> (Option<u32>, Option<Range<u32>>) {
    let cursor = state.cursor_row(view);
    let selection = state.selection().and_then(|selection| {
        let first = view
            .body
            .row_of_addr(
                mode,
                LineAddr {
                    side: selection.side,
                    line: selection.start(),
                },
            )?
            .index;
        let last = view
            .body
            .row_of_addr(
                mode,
                LineAddr {
                    side: selection.side,
                    line: selection.end(),
                },
            )?
            .index;
        Some(first.min(last)..first.max(last) + 1)
    });
    (cursor, selection)
}

thread_local! {
    static CURRENT: RefCell<Weak<Scene>> = const { RefCell::new(Weak::new()) };
}

/// Разбирает канал сцены на главном потоке.
fn pump() {
    let scene = CURRENT.with(|current| current.borrow().upgrade());
    if let Some(scene) = scene {
        scene.drain();
    }
}

fn post(tx: &Sender<Msg>, msg: Msg) {
    if tx.send(msg).is_ok() {
        DispatchQueue::main().exec_async(pump);
    }
}

fn now_ns() -> u64 {
    sampler::uptime_ns().unwrap_or(0)
}

pub struct Scene {
    model: RefCell<Model>,
    surface: RefCell<Option<Rc<dyn Surface>>>,
    parse: ParseWorker,
    heights: HeightWorker,
    highlight: HighlightWorker,
    inbox: Receiver<Msg>,
    decoder: RefCell<Decoder>,
    fixture: Fixture,
    timeline: RefCell<Timeline>,
    errors: RefCell<Vec<String>>,
    /// Идёт перезагрузка вида и прокрутка к якорю: уведомления о прокрутке якорь не меняют.
    reloading: Cell<bool>,
}

impl Scene {
    /// Сцена с сырыми байтами фикстуры и запущенными worker. Разбор и индексы до [`Scene::open`]
    /// не строятся.
    pub fn new(fixture: Fixture, mode: Mode, font: FontMetrics) -> Result<Rc<Scene>, String> {
        let (tx, inbox) = mpsc::channel::<Msg>();
        let parse = {
            let tx = tx.clone();
            ParseWorker::spawn(move |delivery| post(&tx, Msg::Parsed(delivery)))
                .map_err(|err| format!("worker разбора не запущен: {err}"))?
        };
        let heights = {
            let tx = tx.clone();
            HeightWorker::spawn(move |delivery| post(&tx, Msg::Heights(delivery)))
                .map_err(|err| format!("worker индекса высот не запущен: {err}"))?
        };
        let highlight =
            HighlightWorker::spawn(move |snapshot| post(&tx, Msg::Styles(snapshot)), None)
                .map_err(|err| format!("worker подсветки не запущен: {err}"))?;
        let scene = Rc::new(Scene {
            model: RefCell::new(Model {
                diff: None,
                file: 0,
                requested: mode,
                mode,
                geometry: None,
                metrics: None,
                heights: None,
                state: None,
                styles: StyleStore::default(),
                generation: 0,
                font,
                pending: None,
                relayout: Relayout::default(),
                last_visible: None,
            }),
            surface: RefCell::new(None),
            parse,
            heights,
            highlight,
            inbox,
            decoder: RefCell::new(Decoder::default()),
            fixture,
            timeline: RefCell::new(Timeline::default()),
            errors: RefCell::new(Vec::new()),
            reloading: Cell::new(false),
        });
        CURRENT.with(|current| *current.borrow_mut() = Rc::downgrade(&scene));
        Ok(scene)
    }

    pub fn attach(&self, surface: Rc<dyn Surface>) {
        *self.surface.borrow_mut() = Some(surface);
    }

    pub fn model(&self) -> &RefCell<Model> {
        &self.model
    }

    pub fn timeline(&self) -> Timeline {
        self.timeline.borrow().clone()
    }

    pub fn errors(&self) -> Vec<String> {
        self.errors.borrow().clone()
    }

    fn surface(&self) -> Option<Rc<dyn Surface>> {
        self.surface.borrow().clone()
    }

    fn viewport(&self) -> Visible {
        self.surface().map_or(
            Visible {
                y: 0.0,
                width: 0.0,
                height: 0.0,
            },
            |surface| surface.viewport(),
        )
    }

    fn fail(&self, reason: String) {
        self.errors.borrow_mut().push(reason);
    }

    /// Новое поколение затронутых строк: объявление стенду и перерисовка.
    fn changed(&self, generation: u64, rows: Option<Vec<Range<u32>>>) {
        drawmark::declare(Declared {
            generation,
            rows: rows.as_ref().map(|rows| {
                rows.iter()
                    .map(|range| range.start as usize..range.end as usize)
                    .collect()
            }),
            at_ns: now_ns(),
        });
        if let (Some(rows), Some(surface)) = (rows, self.surface()) {
            surface.invalidate(&rows);
        }
    }

    /// Строки показа, которые пересекает видимая область.
    pub fn visible_rows(&self) -> Range<u32> {
        let visible = self.viewport();
        let model = self.model.borrow();
        model.shown().map_or(0..0, |shown| {
            shown
                .heights
                .visible(visible.y.max(0.0) as u64, visible.height.max(0.0) as u64)
        })
    }

    /// Высота документа и предел прокрутки в точках.
    pub fn scroll_limit(&self) -> Option<(u64, u64)> {
        let visible = self.viewport();
        let model = self.model.borrow();
        let total = model.shown()?.heights.total();
        Some((total, total.saturating_sub(visible.height.max(0.0) as u64)))
    }

    /// Якорь прокрутки состояния.
    pub fn anchor(&self) -> Option<Anchor> {
        self.model.borrow().state.as_ref()?.anchor()
    }

    /// Верхний край видимой области, выведенный из якоря по установленному индексу высот.
    pub fn anchor_y(&self) -> Option<u64> {
        let visible = self.viewport();
        let model = self.model.borrow();
        let view = model.viewport(visible.height)?;
        Some(model.state.as_ref()?.shown(&view)?.y)
    }

    /// Перестройка раскладки ещё не установлена.
    pub fn relayout_pending(&self) -> bool {
        self.model.borrow().pending.is_some()
    }

    pub fn relayout(&self) -> Relayout {
        self.model.borrow().relayout
    }

    pub fn cursor(&self) -> Option<LineAddr> {
        self.model
            .borrow()
            .state
            .as_ref()?
            .cursor()
            .map(|cursor| cursor.addr)
    }

    /// Адрес строки показа для щелчка: unified — сторона строки, у контекстной новая.
    pub fn row_addr(&self, row: u32) -> Option<LineAddr> {
        let model = self.model.borrow();
        let shown = model.shown()?;
        let halves = shown.body.row(model.mode, row)?;
        let side = if halves.new.is_some() {
            Side::New
        } else {
            Side::Old
        };
        shown.body.row_addr(model.mode, row, side)
    }

    /// Адрес половины `side` строки показа: в «Рядом» щелчок по половине, у пустой половины
    /// адреса нет.
    pub fn row_addr_at(&self, row: u32, side: Side) -> Option<LineAddr> {
        let model = self.model.borrow();
        model.shown()?.body.row_addr(model.mode, row, side)
    }

    /// Показанный режим.
    pub fn mode(&self) -> Mode {
        self.model.borrow().mode
    }

    /// Раскладка части зоны, с которой построен индекс высот.
    pub fn geometry(&self) -> Option<DiffGeometry> {
        self.model.borrow().geometry
    }

    /// Щелчок по точке документа: в «Рядом» сторону задаёт половина под `x`.
    pub fn click_at(&self, x: f64, y: f64, extend: bool) {
        let side = {
            let model = self.model.borrow();
            match (model.mode, model.geometry) {
                (Mode::SideBySide, Some(geometry)) => Some(geometry.side_at(x)),
                _ => None,
            }
        };
        self.click(y, side, extend);
    }

    /// Открытие фикстуры по клавише: разбор и подсветка уходят на worker, прежний diff
    /// снимается.
    pub fn open(&self) {
        let now = now_ns();
        let generation = {
            let mut model = self.model.borrow_mut();
            model.diff = None;
            model.geometry = None;
            model.metrics = None;
            model.heights = None;
            model.state = None;
            model.pending = None;
            model.last_visible = None;
            model.generation += 1;
            model.generation
        };
        *self.timeline.borrow_mut() = Timeline {
            open_ns: Some(now),
            ..Timeline::default()
        };
        if let Err(stopped) = self.parse.submit(Arc::clone(&self.fixture.diff)) {
            self.fail(format!("разбор не принят: {stopped:?}"));
        }
        let texts = Sides {
            old: Arc::clone(&self.fixture.old),
            new: Arc::clone(&self.fixture.new),
        };
        match self.highlight.open(texts, Sides::default()) {
            Ok((document, _)) => self.model.borrow_mut().styles.reset(document),
            Err(stopped) => self.fail(format!("подсветка не принята: {stopped:?}")),
        }
        self.changed(generation, None);
        if let Some(surface) = self.surface() {
            surface.reload(0.0);
        }
    }

    /// Закрытие вида: работа worker, привязанная к виду, отменяется.
    pub fn close(&self) {
        self.parse.cancel();
        self.heights.cancel();
        self.highlight.close();
        *self.surface.borrow_mut() = None;
        CURRENT.with(|current| *current.borrow_mut() = Weak::new());
    }

    fn drain(&self) {
        while let Ok(msg) = self.inbox.try_recv() {
            match msg {
                Msg::Parsed(delivery) => self.parsed(delivery),
                Msg::Heights(delivery) => self.heights_ready(delivery),
                Msg::Styles(snapshot) => self.styles(&snapshot),
            }
        }
    }

    fn parsed(&self, delivery: Delivery<ParseOutcome>) {
        if delivery.generation != self.parse.generation() {
            return;
        }
        let snapshot = match delivery.value {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.fail(format!("diff не разобран: {error:?}"));
                return;
            }
        };
        self.timeline.borrow_mut().parsed_ns = Some(now_ns());
        let visible = self.viewport();
        let target = {
            let mut model = self.model.borrow_mut();
            let Some(body) = snapshot.diff.file(model.file).and_then(|file| file.text()) else {
                drop(model);
                self.fail("у активного файла нет текста".to_owned());
                return;
            };
            let Some(target) = Target::of(body, model.font, model.requested, visible) else {
                drop(model);
                self.fail(format!(
                    "часть зоны {} × {} ниже min_size",
                    visible.width, visible.height
                ));
                return;
            };
            model.diff = Some(snapshot);
            target
        };
        self.relayout_to(target);
    }

    /// Задание индекса высот под раскладку `target`. Установленная раскладка остаётся до
    /// [`Scene::heights_ready`].
    fn relayout_to(&self, target: Target) {
        let job = {
            let mut model = self.model.borrow_mut();
            let Some(snapshot) = model.diff.clone() else {
                return;
            };
            model.pending = Some(target);
            model.relayout.submitted += 1;
            model.relayout.last_submit_ns = Some(now_ns());
            HeightJob {
                snapshot,
                file: model.file,
                mode: target.mode,
                metrics: target.metrics,
                blocks: Arc::from([]),
            }
        };
        if let Err(stopped) = self.heights.submit(job) {
            self.fail(format!("индекс высот не принят: {stopped:?}"));
        }
    }

    /// Видимая область сменила размер. Новый перенос или режим строится на worker, якорь
    /// остаётся адресом и смещением. Та же раскладка при другой геометрии ставится сразу.
    /// Ниже `min_size` раскладка не меняется: заглушка — сцена зон.
    pub fn resized(&self) {
        let visible = self.viewport();
        let geometry_only = {
            let mut model = self.model.borrow_mut();
            let Some(diff) = model.diff.clone() else {
                return;
            };
            let Some(body) = diff.diff.file(model.file).and_then(|file| file.text()) else {
                return;
            };
            let Some(target) = Target::of(body, model.font, model.requested, visible) else {
                return;
            };
            let installed = model.installed();
            if model.pending.or(installed) == Some(target) {
                return;
            }
            match installed {
                None => None,
                Some(current)
                    if current.mode == target.mode && current.metrics == target.metrics =>
                {
                    model.geometry = Some(target.geometry);
                    if model.pending.take().is_some() {
                        self.heights.cancel();
                    }
                    model.relayout.geometry_only += 1;
                    model.generation += 1;
                    Some((
                        model.generation,
                        model.heights.as_ref().map_or(0, HeightIndex::total),
                    ))
                }
                Some(_) => None,
            }
            .ok_or(target)
        };
        match geometry_only {
            Ok((generation, total)) => {
                self.changed(generation, None);
                self.reload(total);
            }
            Err(target) => self.relayout_to(target),
        }
    }

    /// Вид заново с высотой `total` и прокрутка к якорю. Прижатие clip view при более коротком
    /// документе и сама прокрутка якорь не выводят заново.
    fn reload(&self, total: u64) {
        self.reloading.set(true);
        if let Some(surface) = self.surface() {
            surface.reload(total as f64);
        }
        self.sync_scroll();
        self.reloading.set(false);
        self.request_visible();
    }

    fn heights_ready(&self, delivery: Delivery<Arc<HeightSnapshot>>) {
        if delivery.generation != self.heights.generation() {
            return;
        }
        let snapshot = delivery.value;
        let (generation, total) = {
            let mut model = self.model.borrow_mut();
            let Some(target) = model.pending.filter(|target| {
                target.mode == snapshot.mode && target.metrics == snapshot.metrics
            }) else {
                return;
            };
            let matches = model
                .diff
                .as_ref()
                .is_some_and(|diff| diff.generation == snapshot.diff_generation)
                && model.file == snapshot.file;
            if !matches {
                return;
            }
            let Some(diff) = model.diff.clone() else {
                return;
            };
            let reinstall = model.heights.is_some();
            let switched = reinstall && model.mode != target.mode;
            model.pending = None;
            model.geometry = Some(target.geometry);
            model.mode = target.mode;
            model.metrics = Some(target.metrics);
            model.heights = Some(snapshot.index.clone());
            model.state = Some(match model.state.take() {
                None => ViewState::open(&diff.diff, model.file, target.mode),
                Some(state) if state.mode() != target.mode => state.with_mode(target.mode),
                Some(state) => state,
            });
            if reinstall {
                model.relayout.installed += 1;
                model.relayout.last_install_ns = Some(now_ns());
                model.relayout.mode_switches += u64::from(switched);
            }
            model.generation += 1;
            (model.generation, snapshot.index.total())
        };
        {
            let mut timeline = self.timeline.borrow_mut();
            if timeline.model_ns.is_none() {
                timeline.model_ns = Some(now_ns());
                timeline.open_generation = Some(generation);
                timeline.rows = Some(snapshot.index.len());
                timeline.document_height = Some(total);
            }
        }
        self.changed(generation, None);
        self.reload(total);
    }

    fn styles(&self, snapshot: &Snapshot) {
        let applied = {
            let mut model = self.model.borrow_mut();
            let Some(changed) = model.styles.apply(snapshot) else {
                return;
            };
            let rows = match model.shown() {
                Some(shown) => changed_rows(shown.body, model.mode, &changed),
                None => Vec::new(),
            };
            if !rows.is_empty() {
                model.generation += 1;
            }
            (model.generation, rows)
        };
        let (generation, rows) = applied;
        self.timeline.borrow_mut().snapshots.push(Applied {
            source: snapshot.source,
            request: snapshot.request,
            at_ns: now_ns(),
            generation,
            changed_rows: rows.iter().map(|range| range.len()).sum(),
            compute_ms: snapshot.compute.as_secs_f64() * 1e3,
        });
        if !rows.is_empty() {
            self.changed(generation, Some(rows));
        }
    }

    /// Ставит верхний край видимой области на положение якоря состояния.
    fn sync_scroll(&self) {
        let visible = self.viewport();
        let target = {
            let model = self.model.borrow();
            let Some(view) = model.viewport(visible.height) else {
                return;
            };
            model
                .state
                .as_ref()
                .and_then(|state| state.shown(&view))
                .map(|shown| shown.y)
        };
        if let (Some(y), Some(surface)) = (target, self.surface())
            && (y as f64 - visible.y).abs() >= 0.5
        {
            surface.scroll_to(y as f64);
        }
    }

    /// Запрос стилей видимых строк, если они изменились. Стили далеко от видимых забываются.
    fn request_visible(&self) {
        let visible = self.viewport();
        let request = {
            let mut model = self.model.borrow_mut();
            let mode = model.mode;
            let Some(shown) = model.shown() else {
                return;
            };
            let rows = shown
                .heights
                .visible(visible.y.max(0.0) as u64, visible.height.max(0.0) as u64);
            let lines = visible_lines(shown.body, mode, rows.clone());
            if model.last_visible.as_ref() == Some(&lines) {
                return;
            }
            let screen = rows.len().max(1) as u32;
            let keep = Range {
                start: rows.start.saturating_sub(STYLE_SCREENS * screen),
                end: rows.end.saturating_add(STYLE_SCREENS * screen),
            };
            if model.styles.len() > (4 * STYLE_SCREENS * screen) as usize {
                let around = model
                    .shown()
                    .map(|shown| visible_lines(shown.body, mode, keep));
                if let Some(around) = around {
                    model
                        .styles
                        .retain(|side, line| around.get(side).contains(line));
                }
            }
            model.last_visible = Some(lines.clone());
            lines
        };
        if let Err(stopped) = self.highlight.visible(request) {
            self.fail(format!("запрос подсветки не принят: {stopped:?}"));
        }
    }

    /// Клавиша вида. `true` — клавиша относится к diff, в том числе начало двухклавишной команды.
    pub fn key(&self, key: &KeyInput) -> bool {
        let action = self.decoder.borrow_mut().feed(key);
        match action {
            Some(Action::Open) => {
                self.open();
                true
            }
            Some(Action::Command(command)) => {
                self.command(command);
                true
            }
            None => self.decoder.borrow().is_pending(),
        }
    }

    /// Щелчок по координате документа `y`. `side` — половина «Рядом», в unified сторону даёт
    /// строка. `extend` — `Shift`-щелчок или протяжка.
    pub fn click(&self, y: f64, side: Option<Side>, extend: bool) {
        let command = {
            let model = self.model.borrow();
            let Some(shown) = model.shown() else {
                return;
            };
            let last = shown.heights.total().saturating_sub(1);
            let Some(hit) = shown.heights.row_at((y.max(0.0) as u64).min(last)) else {
                return;
            };
            if extend {
                Command::ExtendTo { row: hit.row }
            } else {
                let side = side.unwrap_or_else(|| match shown.body.row(model.mode, hit.row) {
                    Some(halves) if halves.new.is_none() => Side::Old,
                    _ => Side::New,
                });
                Command::Click { row: hit.row, side }
            }
        };
        self.command(command);
    }

    /// Прокрутка видом: колесо, полоса прокрутки или свой `scrollToPoint`.
    pub fn scrolled(&self) {
        if self.reloading.get() {
            return;
        }
        let visible = self.viewport();
        {
            let mut model = self.model.borrow_mut();
            let Some(state) = model.state.clone() else {
                return;
            };
            let Some(view) = model.viewport(visible.height) else {
                return;
            };
            let next = state.apply(Command::ScrollTo(visible.y.max(0.0) as u64), &view);
            model.state = Some(next);
        }
        self.request_visible();
    }

    /// Команда состояния вида: новое состояние, перерисовка строк курсора и выделения и
    /// прокрутка к якорю.
    pub fn command(&self, command: Command) {
        let visible = self.viewport();
        let outcome = {
            let mut model = self.model.borrow_mut();
            let mode = model.mode;
            let Some(state) = model.state.clone() else {
                return;
            };
            let Some(view) = model.viewport(visible.height) else {
                return;
            };
            let before = marked_rows(&state, &view, mode);
            let next = state.apply(command, &view);
            let after = marked_rows(&next, &view, mode);
            let y = next.shown(&view).map(|shown| shown.y);
            let mut rows = Vec::new();
            if before != after {
                for (cursor, selection) in [before, after] {
                    rows.extend(cursor);
                    rows.extend(selection.into_iter().flatten());
                }
            }
            model.state = Some(next);
            let rows = merge_rows(rows);
            if !rows.is_empty() {
                model.generation += 1;
            }
            (model.generation, rows, y)
        };
        let (generation, rows, y) = outcome;
        if !rows.is_empty() {
            self.changed(generation, Some(rows));
        }
        if let (Some(y), Some(surface)) = (y, self.surface())
            && (y as f64 - visible.y).abs() >= 0.5
        {
            surface.scroll_to(y as f64);
        }
        self.request_visible();
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use probe_diff_view::highlight::Sides;
    use probe_diff_view::model::parse;
    use probe_diff_view::worker::Cancel;

    use super::*;

    const DIFF: &str = "diff --git a/f.rs b/f.rs\n\
        index 1111111..2222222 100644\n\
        --- a/f.rs\n\
        +++ b/f.rs\n\
        @@ -1,4 +1,4 @@\n \
        a\n\
        -b\n\
        +c\n \
        d\n\
        -e\n\
        +f\n";

    fn diff() -> Diff {
        parse::parse(Arc::from(DIFF.as_bytes()), &Cancel::never()).unwrap()
    }

    #[test]
    fn unified_draws_new_styles_except_removed_lines() {
        assert_eq!(
            drawn_side(LineKind::Context, Mode::Unified, Side::Old),
            Side::New
        );
        assert_eq!(
            drawn_side(LineKind::Removed, Mode::Unified, Side::New),
            Side::Old
        );
        assert_eq!(
            drawn_side(LineKind::Added, Mode::Unified, Side::Old),
            Side::New
        );
        assert_eq!(
            drawn_side(LineKind::Context, Mode::SideBySide, Side::Old),
            Side::Old
        );
    }

    #[test]
    fn visible_lines_follow_drawn_sides() {
        let diff = diff();
        let body = diff.file(0).unwrap().text().unwrap();
        // unified: a(ctx 1/1), -b(old 2), +c(new 2), d(ctx 3/3), -e(old 4), +f(new 4).
        let lines = visible_lines(body, Mode::Unified, 0..6);
        assert_eq!(lines.old.lines().collect::<Vec<_>>(), vec![2, 4]);
        assert_eq!(lines.new.lines().collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        let lines = visible_lines(
            body,
            Mode::SideBySide,
            0..body.row_count(Mode::SideBySide) as u32,
        );
        assert_eq!(lines.old.lines().collect::<Vec<_>>(), vec![1, 2, 3, 4]);
        assert_eq!(lines.new.lines().collect::<Vec<_>>(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn changed_lines_map_to_drawn_rows() {
        let diff = diff();
        let body = diff.file(0).unwrap().text().unwrap();
        let changed = Sides {
            // Старая сторона контекстной строки 3 в unified не рисуется.
            old: vec![2, 3],
            new: vec![4],
        };
        assert_eq!(
            changed_rows(body, Mode::Unified, &changed),
            vec![1..2, 5..6]
        );
        let rows = changed_rows(body, Mode::SideBySide, &changed);
        // «Рядом»: a, b|c, d, e|f — строки 1, 2 и 3.
        assert_eq!(rows, vec![1..4]);
    }

    #[test]
    fn rows_merge_into_ranges() {
        assert_eq!(merge_rows(vec![5, 1, 2, 2, 9, 3]), vec![1..4, 5..6, 9..10]);
        assert!(merge_rows(Vec::new()).is_empty());
    }

    #[test]
    fn target_follows_width_not_height() {
        let diff = diff();
        let body = diff.file(0).unwrap().text().unwrap();
        let font = FontMetrics {
            char_width: 7.0,
            line_height: 15,
        };
        let visible = |width: f64, height: f64| Visible {
            y: 0.0,
            width,
            height,
        };
        let wide = Target::of(body, font, Mode::SideBySide, visible(1180.0, 700.0)).unwrap();
        assert_eq!(wide.mode, Mode::SideBySide);
        let narrower = Target::of(body, font, Mode::SideBySide, visible(1060.0, 700.0)).unwrap();
        assert_eq!(narrower.mode, Mode::SideBySide);
        assert_ne!(narrower.metrics, wide.metrics);
        // Высота внутри класса раскладку не меняет: индекс не перестраивается.
        let shorter = Target::of(body, font, Mode::SideBySide, visible(1180.0, 500.0)).unwrap();
        assert_eq!(shorter, wide);
        let narrow = Target::of(body, font, Mode::SideBySide, visible(600.0, 700.0)).unwrap();
        assert_eq!(narrow.mode, Mode::Unified);
        assert!(Target::of(body, font, Mode::Unified, visible(100.0, 700.0)).is_none());
    }

    #[test]
    fn max_line_takes_both_sides() {
        let diff = diff();
        let body = diff.file(0).unwrap().text().unwrap();
        assert_eq!(max_line(body), 5);
    }
}
