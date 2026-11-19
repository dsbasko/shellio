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
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, OnceLock};

use dispatch2::DispatchQueue;
use serde::Serialize;

use probe_diff_view::access::{RowAx, describe_row};
use probe_diff_view::fixtures::Fixture;
use probe_diff_view::highlight::store::{Changed, StyleStore};
use probe_diff_view::highlight::{
    Event, HighlightWorker, LineSet, Observer, SideStyles, Sides, Snapshot, Source,
};
use probe_diff_view::keymap::{Action, Decoder, KeyInput};
use probe_diff_view::model::heights::{
    HeightIndex, HeightJob, HeightSnapshot, HeightWorker, Metrics,
};
use probe_diff_view::model::layout::{DiffGeometry, DiffLayout, Geometry, TextMetrics, Zone};
use probe_diff_view::model::state::{Anchor, Command, Selection, ThreadId, ViewState, Viewport};
use probe_diff_view::model::threads::{
    BlockItem, Draft, DraftError, Thread, ThreadAnchor, ThreadLayout, Threads,
};
use probe_diff_view::model::{
    Diff, DiffSnapshot, LineAddr, LineKind, Mode, ParseOutcome, ParseWorker, Side, TextBody,
};
use probe_diff_view::worker::Delivery;

use crate::blocks::{self, BlockFrame, Place};
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
    /// Высоты строк изменились (блоки тредов): высота документа `height`, строки `rows` с новым
    /// содержимым блоков. Текст строк прежний, всё ниже первой из них сдвинулось.
    fn heights_changed(&self, height: f64, rows: &[Range<u32>]);
    /// Поле черновика над блоком черновика или скрыто (`None`).
    fn place_draft(&self, place: Option<DraftPlace>);
    /// Изменилось то, что сообщают элементы доступности строк: вид шлёт уведомления.
    fn announce(&self, change: Announce);
}

/// Что изменилось для доступности.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Announce {
    /// Строка курсора: фокус доступности.
    pub focus: bool,
    /// Выбранные строки: выделение или, без него, строка курсора.
    pub selection: bool,
    /// Видимые строки: прокрутка или новое содержимое.
    pub layout: bool,
}

impl Announce {
    pub const ALL: Announce = Announce {
        focus: true,
        selection: true,
        layout: true,
    };

    pub fn is_empty(self) -> bool {
        !(self.focus || self.selection || self.layout)
    }

    pub fn merge(self, other: Announce) -> Announce {
        Announce {
            focus: self.focus || other.focus,
            selection: self.selection || other.selection,
            layout: self.layout || other.layout,
        }
    }
}

/// Где показать поле черновика: колонка блока черновика в координатах документа.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftPlace {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Текст модели в поле: при открытии черновика.
    pub text: Option<String>,
    /// Сделать поле first responder.
    pub focus: bool,
}

/// Исходы действий черновика и тредов.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct DraftStats {
    pub opened: u64,
    pub saved: u64,
    pub cancelled: u64,
    /// Черновик с текстом не уступил поле другому якорю.
    pub refused_busy: u64,
    /// Пустой черновик не сохранён.
    pub refused_empty: u64,
    pub toggled: u64,
    /// Перестановок блоков на главном потоке.
    pub block_updates: u64,
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

/// Применённый снимок вместе со стилями: сверка отметок открытия с эталоном.
#[derive(Clone, Debug)]
pub struct KeptSnapshot {
    pub applied: Applied,
    pub styles: Sides<SideStyles>,
}

/// Где встаёт верхний край открытого файла.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OpenAt {
    #[default]
    Top,
    /// Начало последнего ханка, прижатое к пределу прокрутки.
    LastHunk,
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
    /// Раскладка текста `body` в видимой области части зоны `zone`. `None` — ниже `min_size`.
    fn of(
        body: &TextBody,
        font: FontMetrics,
        requested: Mode,
        visible: Visible,
        zone: Zone,
    ) -> Option<Self> {
        let DiffLayout::Diff(geometry) =
            text_metrics(body, font).adapt(Geometry::new(zone, visible.width, visible.height))
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

/// Метрики текста `body` для `adapt`: клетка шрифта, цифры самого большого номера, разделитель.
fn text_metrics(body: &TextBody, font: FontMetrics) -> TextMetrics {
    TextMetrics {
        char_width: font.char_width,
        digits: TextMetrics::digits_of(max_line(body)),
        divider: DIVIDER,
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
    pub threads: Threads,
    /// Блоки установленного индекса высот: меняются вместе с ним.
    pub layout: ThreadLayout,
    /// Зона части, в которой стоит вид: её задаёт хост.
    pub zone: Zone,
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

    /// Раскладка блоков установленного индекса.
    pub fn place(&self) -> Option<Place<'_>> {
        let shown = self.shown()?;
        Some(Place {
            body: shown.body,
            mode: self.mode,
            geometry: shown.geometry,
            char_width: self.font.char_width,
            heights: shown.heights,
            layout: &self.layout,
            threads: &self.threads,
        })
    }

    /// Строка курсора.
    pub fn cursor_row(&self) -> Option<u32> {
        let shown = self.shown()?;
        let cursor = self.state.as_ref()?.cursor()?;
        shown
            .body
            .row_of_addr(self.mode, cursor.addr)
            .map(|located| located.index)
    }

    /// Что сообщает элемент доступности строки показа `row`.
    pub fn row_ax(&self, row: u32) -> Option<RowAx> {
        let shown = self.shown()?;
        let state = self.state.as_ref()?;
        describe_row(
            shown.diff,
            shown.body,
            self.mode,
            row,
            self.cursor_row(),
            state.selection(),
        )
    }

    fn expanded(&self) -> std::collections::BTreeSet<ThreadId> {
        self.state
            .as_ref()
            .map(|state| state.expanded().clone())
            .unwrap_or_default()
    }

    /// Блоки файла при переносе `metrics`.
    fn thread_layout(&self, metrics: Metrics) -> ThreadLayout {
        match self.diff.as_ref() {
            Some(diff) => self.threads.layout(
                &diff.diff,
                self.file,
                &self.expanded(),
                blocks::block_metrics(metrics),
            ),
            None => ThreadLayout::default(),
        }
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

/// Строки показа для перерисовки и объявление доступности при смене состояния `before` на
/// `after`. В «Рядом» курсор отмечен половиной своей стороны, поэтому переход курсора на другую
/// половину той же строки тоже её перерисовывает. Доступность описывает строку целиком, и такой
/// переход не объявляет.
fn repaint(
    before: &ViewState,
    after: &ViewState,
    view: &Viewport<'_>,
    mode: Mode,
) -> (Vec<Range<u32>>, Announce) {
    let marked = (
        marked_rows(before, view, mode),
        marked_rows(after, view, mode),
    );
    let moved =
        before.cursor().map(|cursor| cursor.addr) != after.cursor().map(|cursor| cursor.addr);
    let change = Announce {
        focus: marked.0.0 != marked.1.0,
        selection: marked.0 != marked.1,
        layout: false,
    };
    let mut rows = Vec::new();
    if marked.0 != marked.1 || moved {
        for (cursor, selection) in [marked.0, marked.1] {
            rows.extend(cursor);
            rows.extend(selection.into_iter().flatten());
        }
    }
    (merge_rows(rows), change)
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
    drafts: Cell<DraftStats>,
    open_at: Cell<OpenAt>,
    /// Применённые снимки текущего открытия, если сценарий их держит.
    kept: RefCell<Option<Vec<KeptSnapshot>>>,
    /// Запрос подсветки скомпилирован на потоке запросов: время компиляции или отказ.
    query: Arc<OnceLock<Result<f64, String>>>,
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
        let query = Arc::new(OnceLock::new());
        let observer: Observer = {
            let query = Arc::clone(&query);
            Arc::new(move |event: &Event| match event {
                Event::QueryCompiled { elapsed } => {
                    let _ = query.set(Ok(elapsed.as_secs_f64() * 1e3));
                }
                Event::QueryFailed { reason } => {
                    let _ = query.set(Err(reason.clone()));
                }
                _ => {}
            })
        };
        let highlight = HighlightWorker::spawn(
            move |snapshot| post(&tx, Msg::Styles(snapshot)),
            Some(observer),
        )
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
                threads: Threads::new(fixture.threads.clone()),
                layout: ThreadLayout::default(),
                zone: Zone::Center,
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
            drafts: Cell::new(DraftStats::default()),
            open_at: Cell::new(OpenAt::Top),
            kept: RefCell::new(None),
            query,
        });
        CURRENT.with(|current| *current.borrow_mut() = Rc::downgrade(&scene));
        Ok(scene)
    }

    pub fn attach(&self, surface: Rc<dyn Surface>) {
        *self.surface.borrow_mut() = Some(surface);
    }

    /// Где встаёт верхний край следующих открытий.
    pub fn set_open_at(&self, at: OpenAt) {
        self.open_at.set(at);
    }

    /// Держать применённые снимки каждого открытия со стилями.
    pub fn keep_snapshots(&self) {
        *self.kept.borrow_mut() = Some(Vec::new());
    }

    /// Применённые снимки текущего открытия; пусто, если сценарий их не держит.
    pub fn kept(&self) -> Vec<KeptSnapshot> {
        self.kept.borrow().clone().unwrap_or_default()
    }

    /// Компиляция запроса подсветки: `None` — ещё идёт.
    pub fn query_ready(&self) -> Option<Result<f64, String>> {
        self.query.get().cloned()
    }

    /// Строки сторон видимых строк показа, как их запрашивает подсветка.
    pub fn visible_lines(&self) -> Option<Sides<LineSet>> {
        let rows = self.visible_rows();
        let model = self.model.borrow();
        let shown = model.shown()?;
        Some(visible_lines(shown.body, model.mode, rows))
    }

    /// Путь активного файла открытого diff.
    pub fn file_path(&self) -> Option<String> {
        let model = self.model.borrow();
        let diff = &model.diff.as_ref()?.diff;
        Some(diff.file(model.file)?.path().to_owned())
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

    /// Ответил ли worker подсветки на последний запрос: он отвечает только последнему из
    /// накопившихся, поэтому ответ на него означает, что запросов в работе нет.
    pub fn highlight_settled(&self) -> bool {
        let requested = self.highlight.requested();
        self.timeline
            .borrow()
            .snapshots
            .last()
            .is_some_and(|applied| applied.request >= requested)
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

    pub fn selection(&self) -> Option<Selection> {
        self.model.borrow().state.as_ref()?.selection()
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
            // Стороны фикстуры — первого файла diff.
            model.file = 0;
            model.geometry = None;
            model.metrics = None;
            model.heights = None;
            model.state = None;
            model.pending = None;
            model.last_visible = None;
            model.threads = Threads::new(self.fixture.threads.clone());
            model.layout = ThreadLayout::default();
            model.generation += 1;
            model.generation
        };
        if let Some(surface) = self.surface() {
            surface.place_draft(None);
        }
        *self.timeline.borrow_mut() = Timeline {
            open_ns: Some(now),
            ..Timeline::default()
        };
        if let Some(kept) = self.kept.borrow_mut().as_mut() {
            kept.clear();
        }
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

    /// `]f` и `[f`: соседний файл открытого diff, без перехода по кругу.
    fn step_file(&self, forward: bool) {
        let target = {
            let model = self.model.borrow();
            let Some(diff) = model.diff.as_ref() else {
                return;
            };
            if forward {
                diff.diff.next_file(model.file)
            } else {
                diff.diff.prev_file(model.file)
            }
        };
        if let Some(file) = target {
            self.switch_file(file);
        }
    }

    /// Переход к файлу `file` открытого diff в том же виде. Разбор не повторяется: стороны файла
    /// уходят в подсветку, индекс высот строится заново. Треды и раскрытые треды остаются, их
    /// якорь содержит путь; черновик, выделение и возвраты сбрасываются.
    pub fn switch_file(&self, file: usize) {
        let now = now_ns();
        let switched = {
            let mut model = self.model.borrow_mut();
            match model.diff.clone() {
                None => Err("переход к файлу до разбора diff".to_owned()),
                Some(diff) => match diff.diff.file(file) {
                    None => Err(format!("в diff нет файла {file}")),
                    Some(entry) => match self.sides_of(file, entry.path()) {
                        Err(reason) => Err(reason),
                        Ok(sides) => {
                            let _ = model.threads.cancel_draft();
                            model.file = file;
                            model.geometry = None;
                            model.metrics = None;
                            model.heights = None;
                            model.pending = None;
                            model.state = model
                                .state
                                .take()
                                .map(|state| state.to_file(&diff.diff, file));
                            model.last_visible = None;
                            model.layout = ThreadLayout::default();
                            model.generation += 1;
                            Ok((model.generation, sides))
                        }
                    },
                },
            }
        };
        let (generation, sides) = match switched {
            Ok(switched) => switched,
            Err(reason) => {
                self.fail(reason);
                return;
            }
        };
        self.heights.cancel();
        self.hide_draft();
        *self.timeline.borrow_mut() = Timeline {
            open_ns: Some(now),
            parsed_ns: Some(now),
            ..Timeline::default()
        };
        if let Some(kept) = self.kept.borrow_mut().as_mut() {
            kept.clear();
        }
        match self.highlight.open(sides, Sides::default()) {
            Ok((document, _)) => self.model.borrow_mut().styles.reset(document),
            Err(stopped) => self.fail(format!("подсветка не принята: {stopped:?}")),
        }
        self.changed(generation, None);
        if let Some(surface) = self.surface() {
            surface.reload(0.0);
        }
        let visible = self.viewport();
        let target = {
            let model = self.model.borrow();
            model
                .diff
                .as_ref()
                .and_then(|diff| diff.diff.file(model.file))
                .and_then(|file| file.text())
                .map(|body| Target::of(body, model.font, model.requested, visible, model.zone))
        };
        match target {
            Some(Some(target)) => self.relayout_to(target),
            Some(None) => self.fail(format!(
                "часть зоны {} × {} ниже min_size",
                visible.width, visible.height
            )),
            None => self.fail(format!("у файла {file} нет текста")),
        }
    }

    /// Стороны файла `file` фикстуры. Путь сверяется с путём файла в разобранном diff.
    fn sides_of(&self, file: usize, path: &str) -> Result<Sides<Arc<[u8]>>, String> {
        match self.fixture.files.get(file) {
            Some(sides) if sides.path == path => Ok(Sides {
                old: Arc::clone(&sides.old),
                new: Arc::clone(&sides.new),
            }),
            Some(sides) => Err(format!(
                "файл {file} фикстуры — {}, а в diff — {path}",
                sides.path
            )),
            None if file == 0 && self.fixture.files.is_empty() => Ok(Sides {
                old: Arc::clone(&self.fixture.old),
                new: Arc::clone(&self.fixture.new),
            }),
            None => Err(format!("у фикстуры нет сторон файла {file} ({path})")),
        }
    }

    /// Закрытие вида: работа worker, привязанная к виду, отменяется.
    pub fn close(&self) {
        self.parse.cancel();
        self.heights.cancel();
        self.highlight.close();
        if let Some(surface) = self.surface() {
            surface.place_draft(None);
        }
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
            let Some(target) = Target::of(body, model.font, model.requested, visible, model.zone)
            else {
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
                blocks: Arc::from(model.thread_layout(target.metrics).blocks()),
            }
        };
        if let Err(stopped) = self.heights.submit(job) {
            self.fail(format!("индекс высот не принят: {stopped:?}"));
        }
    }

    /// Хост поставил вид в часть зоны `zone`: раскладка перестраивается, как при смене размера.
    pub fn set_zone(&self, zone: Zone) {
        self.model.borrow_mut().zone = zone;
        self.resized();
    }

    /// Раскладка открытого текста в части зоны `zone` размером `width × height`: по ней хост
    /// решает, показать вид или заглушку. `None` — текст ещё не открыт.
    pub fn layout_at(&self, zone: Zone, width: f64, height: f64) -> Option<DiffLayout> {
        let model = self.model.borrow();
        let diff = model.diff.as_ref()?;
        let body = diff.diff.file(model.file)?.text()?;
        Some(text_metrics(body, model.font).adapt(Geometry::new(zone, width, height)))
    }

    /// Запрошенный режим сменился без смены ширины: раскладка перестраивается, как при смене
    /// размера, якорь остаётся адресом и смещением. «Рядом» включается, только если ширины
    /// хватает на две стороны.
    pub fn request_mode(&self, mode: Mode) {
        self.model.borrow_mut().requested = mode;
        self.resized();
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
            let Some(target) = Target::of(body, model.font, model.requested, visible, model.zone)
            else {
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
        self.place_draft(None, false);
        self.request_visible();
        self.announce(Announce::ALL);
    }

    fn announce(&self, change: Announce) {
        if !change.is_empty()
            && let Some(surface) = self.surface()
        {
            surface.announce(change);
        }
    }

    fn heights_ready(&self, delivery: Delivery<Arc<HeightSnapshot>>) {
        if delivery.generation != self.heights.generation() {
            return;
        }
        let snapshot = delivery.value;
        let visible = self.viewport();
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
            let opened = model.state.is_none();
            let switched = reinstall && model.mode != target.mode;
            // Треды могли раскрыться или черновик открыться, пока шла перестройка.
            let layout = model.thread_layout(target.metrics);
            let mut index = snapshot.index.clone();
            if let Some(body) = diff.diff.file(model.file).and_then(|file| file.text()) {
                index.rebase_blocks(body, target.mode, &snapshot.blocks, &layout.blocks());
            }
            model.pending = None;
            model.geometry = Some(target.geometry);
            model.mode = target.mode;
            model.metrics = Some(target.metrics);
            model.layout = layout;
            model.heights = Some(index);
            model.state = Some(match model.state.take() {
                None => ViewState::open(&diff.diff, model.file, target.mode),
                Some(state) if state.mode() != target.mode => state.with_mode(target.mode),
                Some(state) => state,
            });
            if opened
                && self.open_at.get() == OpenAt::LastHunk
                && let Some(state) = model.state.take()
            {
                let state = match model.viewport(visible.height) {
                    Some(view) => state.at_last_hunk(&view),
                    None => state,
                };
                model.state = Some(state);
            }
            if reinstall {
                model.relayout.installed += 1;
                model.relayout.last_install_ns = Some(now_ns());
                model.relayout.mode_switches += u64::from(switched);
            }
            model.generation += 1;
            (
                model.generation,
                model.heights.as_ref().map_or(0, HeightIndex::total),
            )
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
        let applied = Applied {
            source: snapshot.source,
            request: snapshot.request,
            at_ns: now_ns(),
            generation,
            changed_rows: rows.iter().map(|range| range.len()).sum(),
            compute_ms: snapshot.compute.as_secs_f64() * 1e3,
        };
        if let Some(kept) = self.kept.borrow_mut().as_mut() {
            kept.push(KeptSnapshot {
                applied: applied.clone(),
                styles: snapshot.styles.clone(),
            });
        }
        self.timeline.borrow_mut().snapshots.push(applied);
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

    fn count(&self, update: impl FnOnce(&mut DraftStats)) {
        let mut stats = self.drafts.get();
        update(&mut stats);
        self.drafts.set(stats);
    }

    pub fn draft_stats(&self) -> DraftStats {
        self.drafts.get()
    }

    pub fn draft(&self) -> Option<Draft> {
        self.model.borrow().threads.draft().cloned()
    }

    pub fn threads(&self) -> Vec<Thread> {
        self.model.borrow().threads.threads().to_vec()
    }

    pub fn is_expanded(&self, id: ThreadId) -> bool {
        self.model
            .borrow()
            .state
            .as_ref()
            .is_some_and(|state| state.is_expanded(id))
    }

    /// Блок черновика в координатах документа.
    pub fn draft_frame(&self) -> Option<BlockFrame> {
        self.model.borrow().place()?.draft_frame()
    }

    /// Блоки под строкой показа в координатах документа.
    pub fn block_frames(&self, row: u32) -> Vec<BlockFrame> {
        self.model
            .borrow()
            .place()
            .map(|place| place.frames_at_row(row))
            .unwrap_or_default()
    }

    /// Высота блоков под строкой показа по установленному индексу.
    pub fn block_height(&self, row: u32) -> Option<u32> {
        self.model.borrow().shown()?.heights.block_height(row)
    }

    /// Высоты блоков треда при установленном переносе.
    pub fn thread_height(&self, id: ThreadId, expanded: bool) -> Option<u32> {
        let model = self.model.borrow();
        let metrics = blocks::block_metrics(model.metrics?);
        Some(metrics.thread_height(model.threads.get(id)?, expanded))
    }

    /// `c`: черновик на выделении или строке курсора. Выделение переходит в якорь черновика и
    /// снимается. Черновик с текстом на другом якоре поле не уступает.
    pub fn open_draft(&self) {
        let opened = {
            let mut model = self.model.borrow_mut();
            let Some(path) = model
                .diff
                .as_ref()
                .and_then(|diff| diff.diff.file(model.file))
                .map(|file| file.path().to_owned())
            else {
                return;
            };
            let Some(state) = model.state.clone() else {
                return;
            };
            let Some(cursor) = state.cursor() else {
                return;
            };
            let anchor = ThreadAnchor::from_view(path, cursor, state.selection());
            model.threads.open_draft(anchor)
        };
        match opened {
            Ok(()) => {
                self.count(|stats| stats.opened += 1);
                if self.selection().is_some() {
                    self.command(Command::ClearSelection);
                }
                self.relayout_blocks();
                let text = self.draft().map(|draft| draft.text().to_owned());
                self.place_draft(text, true);
            }
            Err(DraftError::Busy) => {
                self.count(|stats| stats.refused_busy += 1);
                self.place_draft(None, true);
            }
            Err(DraftError::NoDraft | DraftError::Empty) => {}
        }
    }

    /// Текст поля черновика после правки.
    pub fn draft_text(&self, text: String) {
        let _ = self.model.borrow_mut().threads.set_draft_text(text);
    }

    /// `Cmd-Enter` в поле: черновик становится раскрытым тредом, поле скрывается.
    pub fn save_draft(&self) {
        let saved = {
            let mut model = self.model.borrow_mut();
            let saved = model.threads.save_draft(blocks::AUTHOR);
            if let Ok(id) = saved
                && let Some(state) = model.state.take()
            {
                model.state = Some(if state.is_expanded(id) {
                    state
                } else {
                    state.toggle_thread(id)
                });
            }
            saved
        };
        match saved {
            Ok(_) => {
                self.count(|stats| stats.saved += 1);
                self.relayout_blocks();
                self.hide_draft();
            }
            Err(DraftError::Empty) => self.count(|stats| stats.refused_empty += 1),
            Err(DraftError::NoDraft | DraftError::Busy) => {}
        }
    }

    /// `Escape` в поле: черновик закрывается без треда.
    pub fn cancel_draft(&self) {
        let cancelled = self.model.borrow_mut().threads.cancel_draft();
        if cancelled.is_some() {
            self.count(|stats| stats.cancelled += 1);
            self.relayout_blocks();
        }
        self.hide_draft();
    }

    /// Раскрыть все свёрнутые треды тем же переходом состояния, что у [`Scene::toggle_thread`],
    /// но одной перестройкой блоков. Возвращает число раскрытых.
    pub fn expand_threads(&self) -> usize {
        let expanded = {
            let mut model = self.model.borrow_mut();
            let Some(state) = model.state.take() else {
                return 0;
            };
            let collapsed: Vec<ThreadId> = model
                .threads
                .threads()
                .iter()
                .map(|thread| thread.id)
                .filter(|&id| !state.is_expanded(id))
                .collect();
            model.state = Some(
                collapsed
                    .iter()
                    .fold(state, |state, &id| state.toggle_thread(id)),
            );
            collapsed.len()
        };
        if expanded > 0 {
            self.count(|stats| stats.toggled += expanded as u64);
            self.relayout_blocks();
        }
        expanded
    }

    /// Раскрыть или свернуть тред: высота его блока, якорь прокрутки прежний.
    pub fn toggle_thread(&self, id: ThreadId) {
        {
            let mut model = self.model.borrow_mut();
            let Some(state) = model.state.take() else {
                return;
            };
            model.state = Some(state.toggle_thread(id));
        }
        self.count(|stats| stats.toggled += 1);
        self.relayout_blocks();
    }

    /// Блоки после изменения тредов или черновика на главном потоке: высоты блоков правятся в
    /// установленном индексе, все строки ниже сдвигаются, поэтому объявляются все.
    fn relayout_blocks(&self) {
        let changed = {
            let mut model = self.model.borrow_mut();
            let (Some(diff), Some(metrics)) = (model.diff.clone(), model.metrics) else {
                return;
            };
            let Some(body) = diff.diff.file(model.file).and_then(|file| file.text()) else {
                return;
            };
            let layout = model.thread_layout(metrics);
            if layout == model.layout {
                return;
            }
            let mode = model.mode;
            let (before, after) = (model.layout.blocks(), layout.blocks());
            let mut rows: Vec<u32> = Vec::new();
            for entry in model.layout.entries().iter().chain(layout.entries()) {
                let kept =
                    model.layout.entries().contains(entry) && layout.entries().contains(entry);
                if !kept && let Some(row) = body.row_of_line(mode, entry.line) {
                    rows.push(row);
                }
            }
            let Some(heights) = model.heights.as_mut() else {
                return;
            };
            heights.rebase_blocks(body, mode, &before, &after);
            let total = heights.total();
            model.layout = layout;
            model.generation += 1;
            (model.generation, total, merge_rows(rows))
        };
        let (generation, total, rows) = changed;
        self.count(|stats| stats.block_updates += 1);
        self.changed(generation, None);
        self.reloading.set(true);
        if let Some(surface) = self.surface() {
            surface.heights_changed(total as f64, &rows);
        }
        self.sync_scroll();
        self.reloading.set(false);
        self.place_draft(None, false);
        self.request_visible();
        self.announce(Announce {
            layout: true,
            ..Announce::default()
        });
    }

    /// Поле черновика над блоком черновика, если он в этом файле, иначе скрыто. `text` — текст
    /// модели при открытии, `focus` — поле становится first responder.
    fn place_draft(&self, text: Option<String>, focus: bool) {
        let place = self.draft_frame().map(|frame| DraftPlace {
            x: frame.x,
            y: frame.y,
            width: frame.width,
            height: frame.height,
            text,
            focus,
        });
        if let Some(surface) = self.surface() {
            surface.place_draft(place);
        }
    }

    fn hide_draft(&self) {
        if let Some(surface) = self.surface() {
            surface.place_draft(None);
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
            Some(Action::Draft) => {
                self.open_draft();
                true
            }
            Some(Action::NextFile) => {
                self.step_file(true);
                true
            }
            Some(Action::PrevFile) => {
                self.step_file(false);
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
            // Протяжка и `Shift`-щелчок по блоку продолжают выделение, тред не переключают.
            if hit.in_block && !extend {
                let item = model
                    .place()
                    .and_then(|place| place.frame_at(hit.row, y))
                    .map(|frame| frame.entry.item);
                drop(model);
                if let Some(BlockItem::Thread(id)) = item {
                    self.toggle_thread(id);
                }
                return;
            }
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
        self.announce(Announce {
            layout: true,
            ..Announce::default()
        });
    }

    /// Команда состояния вида: новое состояние, перерисовка строк курсора и выделения и
    /// прокрутка к якорю.
    pub fn command(&self, command: Command) {
        let visible = self.viewport();
        let outcome = {
            let mut model = self.model.borrow_mut();
            let mode = model.mode;
            let Some(view) = model.viewport(visible.height) else {
                return;
            };
            let Some(state) = model.state.as_ref() else {
                return;
            };
            let next = state.clone().apply(command, &view);
            let (rows, change) = repaint(state, &next, &view, mode);
            let y = next.shown(&view).map(|shown| shown.y);
            model.state = Some(next);
            if !rows.is_empty() {
                model.generation += 1;
            }
            (model.generation, rows, y, change)
        };
        let (generation, rows, y, mut change) = outcome;
        if !rows.is_empty() {
            self.changed(generation, Some(rows));
        }
        if let (Some(y), Some(surface)) = (y, self.surface())
            && (y as f64 - visible.y).abs() >= 0.5
        {
            surface.scroll_to(y as f64);
            change.layout = true;
        }
        self.request_visible();
        self.announce(change);
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

    use probe_diff_view::fixtures::FixtureName;
    use probe_diff_view::highlight::Sides;
    use probe_diff_view::model::parse;
    use probe_diff_view::model::wrap::{TAB_WIDTH, Wrap};
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
    fn refused_save_keeps_view_state() {
        let fixture = probe_diff_view::fixtures::generate(FixtureName::Modified);
        let font = FontMetrics {
            char_width: 7.0,
            line_height: 15,
        };
        let scene = Scene::new(fixture, Mode::Unified, font).unwrap();
        let state = ViewState::open(&diff(), 0, Mode::Unified);
        scene.model.borrow_mut().state = Some(state.clone());

        scene.save_draft();
        assert_eq!(scene.model.borrow().state.as_ref(), Some(&state));

        let anchor = ThreadAnchor::at(
            "f.rs",
            LineAddr {
                side: Side::New,
                line: 2,
            },
        );
        scene.model.borrow_mut().threads.open_draft(anchor).unwrap();
        scene.draft_text(" \n".to_owned());
        scene.save_draft();
        assert_eq!(scene.model.borrow().state.as_ref(), Some(&state));
        assert_eq!(scene.draft_stats().refused_empty, 1);
        assert!(scene.draft().is_some());
    }

    #[test]
    fn cursor_half_switch_repaints_its_row() {
        let diff = diff();
        let body = diff.file(0).unwrap().text().unwrap();
        let metrics = Metrics {
            line_height: 15,
            wrap: Wrap::new(80, TAB_WIDTH),
        };
        let heights = probe_diff_view::model::heights::build(
            &diff,
            body,
            Mode::SideBySide,
            metrics,
            &[],
            &Cancel::never(),
        )
        .unwrap();
        let view = Viewport {
            body,
            heights: &heights,
            height: 600,
        };
        // «Рядом»: a, b|c, d, e|f — щелчок по старой половине строки 1, затем по новой.
        let old = ViewState::open(&diff, 0, Mode::SideBySide).apply(
            Command::Click {
                row: 1,
                side: Side::Old,
            },
            &view,
        );
        let new = old.clone().apply(
            Command::Click {
                row: 1,
                side: Side::New,
            },
            &view,
        );
        assert_ne!(old.cursor().unwrap().addr, new.cursor().unwrap().addr);
        let (rows, change) = repaint(&old, &new, &view, Mode::SideBySide);
        assert_eq!(rows, vec![1..2]);
        assert_eq!(change, Announce::default());

        let (rows, change) = repaint(&new, &new, &view, Mode::SideBySide);
        assert!(rows.is_empty());
        assert!(change.is_empty());

        let down = new.clone().apply(Command::Down, &view);
        let (rows, change) = repaint(&new, &down, &view, Mode::SideBySide);
        assert_eq!(rows, vec![1..3]);
        assert!(change.focus && change.selection);
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
        let wide = Target::of(
            body,
            font,
            Mode::SideBySide,
            visible(1180.0, 700.0),
            Zone::Center,
        )
        .unwrap();
        assert_eq!(wide.mode, Mode::SideBySide);
        let narrower = Target::of(
            body,
            font,
            Mode::SideBySide,
            visible(1060.0, 700.0),
            Zone::Center,
        )
        .unwrap();
        assert_eq!(narrower.mode, Mode::SideBySide);
        assert_ne!(narrower.metrics, wide.metrics);
        // Высота внутри класса раскладку не меняет: индекс не перестраивается.
        let shorter = Target::of(
            body,
            font,
            Mode::SideBySide,
            visible(1180.0, 500.0),
            Zone::Center,
        )
        .unwrap();
        assert_eq!(shorter, wide);
        let narrow = Target::of(
            body,
            font,
            Mode::SideBySide,
            visible(600.0, 700.0),
            Zone::Center,
        )
        .unwrap();
        assert_eq!(narrow.mode, Mode::Unified);
        assert!(
            Target::of(
                body,
                font,
                Mode::Unified,
                visible(100.0, 700.0),
                Zone::Center
            )
            .is_none()
        );
        // Боковая зона снимает запрос «Рядом» при той же ширине.
        let left = Target::of(
            body,
            font,
            Mode::SideBySide,
            visible(1180.0, 700.0),
            Zone::Left,
        )
        .unwrap();
        assert_eq!(left.mode, Mode::Unified);
    }

    #[test]
    fn max_line_takes_both_sides() {
        let diff = diff();
        let body = diff.file(0).unwrap().text().unwrap();
        assert_eq!(max_line(body), 5);
    }
}
