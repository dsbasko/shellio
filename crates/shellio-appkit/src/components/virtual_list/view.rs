//! Вид `VirtualList` на главном потоке.
//!
//! Таблица — view-based `NSTableView` в `NSScrollView` без заголовка и фона: под строками видна
//! подложка. Источник данных и делегат — один объект, его держит владелец и снимает в `Drop`
//! (инвариант 1 P0.2). Строки читаются из Rust-части через слабую ссылку, источник видов строк не
//! держит, а цвета темы общие для всех видов, включая виды из очереди переиспользования
//! (инвариант 8): вид строки, выданный повторно после смены ячейки, перерисовывается.
//!
//! Выбор принадлежит модели. Щелчок и Up/Down отдают намерение `select`, таблица меняет выбор
//! только по новому состоянию. `Escape` отдаёт намерение `cancel` и дальше по цепочке ответчиков
//! не идёт. Up и Down обрабатывает сам список, прокрутка к выбору идёт через
//! clip view (P0.2, находка 4). Заливку выбора рисует свой вид строки токеном `selection`: фон
//! `NSTableRowView` лёг бы поверх цвета слоя.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ops::Range;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSColor, NSControl, NSControlTextEditingDelegate, NSEvent, NSFocusRingType,
    NSFont, NSFontWeightRegular, NSLineBreakMode, NSResponder, NSScrollView, NSTableCellView,
    NSTableColumn, NSTableColumnResizingOptions, NSTableRowView, NSTableView,
    NSTableViewAnimationOptions, NSTableViewDataSource, NSTableViewDelegate,
    NSTableViewGridLineStyle, NSTableViewSelectionHighlightStyle, NSTableViewStyle, NSTextField,
    NSUserInterfaceItemIdentification, NSView,
};
use objc2_foundation::{
    NSIndexSet, NSInteger, NSMutableIndexSet, NSPoint, NSRange, NSRect, NSRunLoop,
    NSRunLoopCommonModes, NSSize, NSString,
};
use objc2_quartz_core::CADisplayLink;
use shellio_theme::AppTheme;

use super::model::{
    self, KeyPress, ListError, ListIntent, ListKey, ListOp, ListState, PressLog, RowDiff,
};
use crate::app::App;
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode, NodeState, Transition, TransitionPath};
use crate::layer;
use crate::own::{Callback, ComponentId, View};
use crate::runloop;

counter!(LIST_OWNER, Owner, "VirtualList");
counter!(LIST_TABLE, Class, "ShellioListTable");
counter!(LIST_SOURCE, Class, "ShellioListSource");
counter!(LIST_ROW, Class, "ShellioListRow");
counter!(LIST_CELL, Class, "ShellioListCell");
counter!(LIST_TICKER, Class, "ShellioListTicker");

/// Сколько последних тактов `CADisplayLink` держит список: больше 4 минут на 120 Гц, серия задержки
/// с проверками между нажатиями идёт меньше минуты.
const TICKS: usize = 32_768;

/// Идентификаторы переиспользования вида строки и вида ячейки.
const ROW_ID: &str = "shellio.list.row";
const CELL_ID: &str = "shellio.list.cell";
const COLUMN_ID: &str = "shellio.list.column";
/// Отступ текста строки от её краёв.
const TEXT_INSET: f64 = 10.0;

/// Видимая строка в `ui.inspect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRowFacts {
    pub index: usize,
    pub id: String,
    pub text: String,
}

/// Состояние списка для `ui.inspect`.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualListFacts {
    pub label: String,
    pub count: usize,
    pub selected: Option<String>,
    /// Смещение прокрутки в точках и его предел.
    pub offset: f64,
    pub max_offset: f64,
    /// Таблица — first responder окна.
    pub keyboard_focus: bool,
    /// Созданные виды строк и ячеек за жизнь списка.
    pub row_views_created: u64,
    pub cell_views_created: u64,
    /// Видимые строки по порядку.
    pub visible: Vec<ListRowFacts>,
}

/// Rust-часть списка. Виды и источник читают её через слабую ссылку.
struct ListModel {
    id: ComponentId,
    /// Общие цвета: виды строк и ячеек читают их при отрисовке и выдаче.
    theme: Cell<AppTheme>,
    /// Растёт со сменой ячейки: вид строки с прежней эпохой перерисовывается при выдаче.
    epoch: Cell<u64>,
    /// Показанное состояние; `None` — таблица ещё не наполнялась.
    shown: RefCell<Option<ListState>>,
    row_views: Cell<u64>,
    cell_views: Cell<u64>,
    on_intent: Box<dyn Fn(ListIntent)>,
    /// Замер задержки клавиш включён: без него список не ведёт журнал нажатий и не заводит
    /// `CADisplayLink`.
    latency_probe: Cell<bool>,
    /// Такты `CADisplayLink` во время серии. Сам display link будит главный цикл каждый кадр и
    /// меняет то, что мерит, поэтому по умолчанию серия идёт без него.
    latency_ticks: Cell<bool>,
    /// Нажатия Up и Down серии задержки: серия начинается, когда таблица получает фокус.
    presses: RefCell<PressLog>,
    /// Такты `CADisplayLink` окна за серию, секунды с загрузки.
    ticks: RefCell<VecDeque<f64>>,
    /// Такты идут только при включённых замере и тактах и пока таблица держит фокус: в простое
    /// цикл они не будят.
    link: Rc<RefCell<Option<Retained<CADisplayLink>>>>,
}

/// Метки нажатия до обработчика.
struct Stamp {
    event: f64,
    wake: Option<f64>,
    entry: f64,
}

impl ListModel {
    fn rows_len(&self) -> usize {
        self.shown
            .borrow()
            .as_ref()
            .map_or(0, |state| state.rows.len())
    }

    fn row_text(&self, index: usize) -> Option<String> {
        self.shown
            .borrow()
            .as_ref()
            .and_then(|state| state.rows.get(index))
            .map(|row| row.text.clone())
    }

    fn row_id(&self, index: usize) -> Option<String> {
        self.shown
            .borrow()
            .as_ref()
            .and_then(|state| state.rows.get(index))
            .map(|row| row.id.clone())
    }

    fn key(&self, key: ListKey, stamp: &Stamp) {
        let intent = self
            .shown
            .borrow()
            .as_ref()
            .and_then(|state| model::key_intent(state, key));
        let Some(intent) = intent else {
            return;
        };
        let target = match (&intent, key) {
            (ListIntent::Select { id }, ListKey::Up | ListKey::Down) => Some(id.clone()),
            _ => None,
        };
        (self.on_intent)(intent);
        if let Some(target) = target.filter(|_| self.latency_probe.get()) {
            self.presses.borrow_mut().press(KeyPress {
                key,
                event: stamp.event,
                wake: stamp.wake,
                entry: stamp.entry,
                handled: runloop::uptime(),
                target,
                applied: None,
                generation: None,
            });
        }
    }

    fn stop_ticks(&self) {
        if let Some(link) = self.link.borrow_mut().take() {
            link.invalidate();
        }
    }
}

/// Список на главном потоке. Освобождение снимает источник и делегата и убирает вид из иерархии.
pub struct VirtualList {
    view: View<NSScrollView, ListModel>,
    table: Retained<ListTable>,
}

impl VirtualList {
    /// Список с состоянием `state`. `on_intent` вызывается на главном потоке изнутри AppKit и не
    /// должен синхронно менять список.
    pub fn new(
        app: &App,
        theme: &AppTheme,
        state: ListState,
        on_intent: impl Fn(ListIntent) + 'static,
    ) -> Result<VirtualList, ListError> {
        model::check(&state)?;
        let mtm = app.mtm();
        let model = ListModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            epoch: Cell::new(0),
            shown: RefCell::new(None),
            row_views: Cell::new(0),
            cell_views: Cell::new(0),
            on_intent: Box::new(on_intent),
            latency_probe: Cell::new(false),
            latency_ticks: Cell::new(false),
            presses: RefCell::new(PressLog::default()),
            ticks: RefCell::new(VecDeque::new()),
            link: Rc::new(RefCell::new(None)),
        };
        let mut table_slot = None;
        let mut view = View::new(&LIST_OWNER, model, "VirtualList.table", |owner| {
            let table = ListTable::new(mtm, owner, theme);
            let scroll = NSScrollView::new(mtm);
            scroll.setWantsLayer(true);
            scroll.setDrawsBackground(false);
            scroll.setHasVerticalScroller(true);
            scroll.setHasHorizontalScroller(false);
            scroll.setAutohidesScrollers(true);
            scroll.contentView().setDrawsBackground(false);
            scroll.setDocumentView(Some(&table));
            table_slot = Some(table);
            scroll
        });
        let Some(table) = table_slot else {
            unreachable!("View::new вызывает построение вида ровно один раз");
        };
        let source = ListSource::new(mtm, view.callback("VirtualList.source"));
        // SAFETY: источник реализует методы протоколов с их сигнатурами; его держит владелец до
        // снятия в шаге отвязки.
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(&*source)));
            table.setDelegate(Some(ProtocolObject::from_ref(&*source)));
        }
        let link = Rc::clone(&view.rust().link);
        let detached = table.clone();
        view.on_detach(move || {
            if let Some(link) = link.borrow_mut().take() {
                link.invalidate();
            }
            // SAFETY: снятие источника и делегата до их освобождения.
            unsafe {
                detached.setDelegate(None);
                detached.setDataSource(None);
            }
            drop(source);
        });
        let list = VirtualList { view, table };
        list.set_state(state)?;
        Ok(list)
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn state(&self) -> ListState {
        self.view.rust().shown.borrow().clone().unwrap_or_default()
    }

    /// Новое состояние: первое наполнение — полный `reloadData`, замена набора — диапазонами
    /// строк, выбор прокручивается в видимую часть через clip view.
    pub fn set_state(&self, state: ListState) -> Result<(), ListError> {
        model::check(&state)?;
        let model = self.view.rust();
        let ops = model::diff(model.shown.borrow().as_ref(), &state);
        model.shown.replace(Some(state));
        let table = &*self.table;
        for op in ops {
            match op {
                ListOp::Label(label) => {
                    table.setAccessibilityLabel(Some(&NSString::from_str(&label)));
                }
                ListOp::Fill => table.reloadData(),
                ListOp::Rows(diff) => apply_rows(table, &diff),
                ListOp::Select(index) => {
                    let selected = model
                        .shown
                        .borrow()
                        .as_ref()
                        .and_then(|state| state.selected.clone());
                    match index {
                        Some(index) => {
                            let rows = NSIndexSet::indexSetWithIndex(index);
                            table.selectRowIndexes_byExtendingSelection(&rows, false);
                        }
                        // SAFETY: `deselectAll:` без отправителя на главном потоке.
                        None => unsafe { table.deselectAll(None) },
                    }
                    if let Some(index) = index {
                        self.reveal(index);
                    }
                    model
                        .presses
                        .borrow_mut()
                        .applied(selected.as_deref(), runloop::uptime());
                }
            }
        }
        Ok(())
    }

    /// Включает замер задержки клавиш: со следующего получения фокуса список ведёт журнал нажатий
    /// Up и Down, а при [`VirtualList::set_latency_ticks`] — и такты `CADisplayLink`, пока фокус у
    /// него. Без включения замер не идёт.
    pub fn enable_latency_probe(&self) {
        self.view.rust().latency_probe.set(true);
    }

    /// Такты `CADisplayLink` во время серии задержки. Действует со следующего получения фокуса.
    pub fn set_latency_ticks(&self, enabled: bool) {
        self.view.rust().latency_ticks.set(enabled);
    }

    /// Окно опубликовало поколение: нажатия, чей выбор уже применён, получают его.
    pub fn set_generation(&self, generation: u64) {
        self.view.rust().presses.borrow_mut().stamp(generation);
    }

    /// Нажатия текущей серии задержки.
    pub fn key_presses(&self) -> Vec<KeyPress> {
        self.view.rust().presses.borrow().presses()
    }

    /// Такты `CADisplayLink` текущей серии.
    pub fn ticks(&self) -> Vec<f64> {
        self.view.rust().ticks.borrow().iter().copied().collect()
    }

    /// Новая ячейка темы: видимые и подготовленные виды перерисовываются, виды из очереди
    /// переиспользования — при выдаче. Возвращает смену для журнала анимаций.
    pub fn set_theme(&self, theme: &AppTheme) -> Transition {
        let before = self.animations();
        self.repaint(theme);
        Transition {
            path: TransitionPath::Instant,
            added: inspect::added_animations(&before, &self.animations()),
        }
    }

    fn animations(&self) -> Vec<(usize, String, usize)> {
        self.view
            .view()
            .layer()
            .map(|layer| inspect::animation_set(&layer))
            .unwrap_or_default()
    }

    fn repaint(&self, theme: &AppTheme) {
        let model = self.view.rust();
        if model.theme.get() == *theme {
            return;
        }
        model.theme.set(*theme);
        model.epoch.set(model.epoch.get() + 1);
        let epoch = model.epoch.get();
        let theme = *theme;
        let block = RcBlock::new(move |row: NonNull<NSTableRowView>, _index: NSInteger| {
            // SAFETY: AppKit передаёт живой вид строки на время вызова блока.
            let row = unsafe { row.as_ref() };
            repaint_row(row, &theme, epoch);
        });
        self.table.enumerateAvailableRowViewsUsingBlock(&block);
    }

    /// Отдаёт таблице клавиатурный фокус: Up и Down меняют выбор.
    pub fn focus(&self) -> bool {
        let table: &NSView = &self.table;
        table
            .window()
            .is_some_and(|window| window.makeFirstResponder(Some(table)))
    }

    /// Таблица — first responder окна.
    pub fn has_focus(&self) -> bool {
        self.table.is_key_focus()
    }

    /// Прячет список или показывает его снова. Скрытый список first responder не отдаёт: ввод
    /// переводит владелец до скрытия.
    pub fn set_hidden(&self, hidden: bool) {
        let view = self.view.view();
        if view.isHidden() != hidden {
            layer::without_actions(|| view.setHidden(hidden));
        }
    }

    /// Прокрутка: строка `index` встаёт к верхнему краю, в пределах прокрутки.
    pub fn scroll_to_row(&self, index: usize) {
        let row = self.table.rowHeight();
        let clip = self.view.view().contentView();
        let visible = clip.bounds().size.height;
        let offset = model::scroll_to(index, row, visible, self.view.rust().rows_len());
        self.scroll_clip(offset);
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    /// Список и его видимые строки для `ui.inspect`: список (`<id>`) с ролью и именем таблицы и
    /// кадром видимой части, строки (`<id>.row.<id строки>`) с выбором и текстом.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        let scroll = self.view.view();
        let table: &NSView = &self.table;
        let facts = self.facts();
        let visible: Vec<usize> = facts.visible.iter().map(|row| row.index).collect();
        let mut list = inspect::node(
            id,
            "VirtualList",
            parent,
            table,
            None,
            Some(NodeState::VirtualList(Box::new(facts))),
        );
        list.facts.frame = inspect::frame_of(scroll.convertRect_toView(scroll.bounds(), None));
        let mut nodes = vec![list];
        let model = self.view.rust();
        for index in visible {
            let Ok(row) = NSInteger::try_from(index) else {
                break;
            };
            let Some(row_view) = self.table.rowViewAtRow_makeIfNecessary(row, false) else {
                continue;
            };
            let Some(row_id) = model.row_id(index) else {
                continue;
            };
            // Имя строки — текст, который показывает её ячейка, а не строка модели: так чтение
            // доказывает, что переиспользованный вид получил новый текст.
            let text = row_view.subviews().iter().find_map(|view| {
                view.downcast_ref::<ListCell>()
                    .map(|cell| cell.ivars().label.stringValue().to_string())
            });
            let mut node = inspect::node(
                &format!("{id}.row.{row_id}"),
                "ListRow",
                Some(id),
                &row_view,
                None,
                None,
            );
            let shown = row_view.visibleRect();
            node.facts.frame = inspect::frame_of(row_view.convertRect_toView(shown, None));
            // Внешний обход видит строку элементом AXRow таблицы, а не её видом строки: имя строки
            // у него — текст ячейки.
            node.facts.role = Some("AXRow".to_owned());
            node.facts.selected = Some(row_view.isSelected());
            node.facts.label = text;
            nodes.push(node);
        }
        nodes
    }

    /// Видимые строки по номерам.
    pub fn visible_rows(&self) -> Vec<ListRowFacts> {
        let model = self.view.rust();
        self.visible_range()
            .filter_map(|index| {
                Some(ListRowFacts {
                    index,
                    id: model.row_id(index)?,
                    text: model.row_text(index)?,
                })
            })
            .collect()
    }

    /// Созданные виды строк за жизнь списка.
    pub fn row_views_created(&self) -> u64 {
        self.view.rust().row_views.get()
    }

    fn facts(&self) -> VirtualListFacts {
        let model = self.view.rust();
        let clip = self.view.view().contentView();
        let bounds = clip.bounds();
        let state = self.state();
        let content = state.rows.len() as f64 * self.table.rowHeight();
        // Выбор читается у таблицы, а не у модели: модель и контрол могут разойтись.
        let selected = usize::try_from(self.table.selectedRow())
            .ok()
            .and_then(|row| model.row_id(row));
        VirtualListFacts {
            label: state.label.clone(),
            count: state.rows.len(),
            selected,
            offset: bounds.origin.y,
            max_offset: (content - bounds.size.height).max(0.0),
            keyboard_focus: self.table.is_key_focus(),
            row_views_created: model.row_views.get(),
            cell_views_created: model.cell_views.get(),
            visible: self.visible_rows(),
        }
    }

    fn visible_range(&self) -> Range<usize> {
        let clip = self.view.view().contentView();
        let bounds = clip.bounds();
        model::visible_rows(
            self.table.rowHeight(),
            bounds.origin.y,
            bounds.size.height,
            self.view.rust().rows_len(),
        )
    }

    fn reveal(&self, index: usize) {
        let clip = self.view.view().contentView();
        let bounds = clip.bounds();
        if let Some(offset) = model::reveal(
            index,
            self.table.rowHeight(),
            bounds.origin.y,
            bounds.size.height,
        ) {
            self.scroll_clip(offset);
        }
    }

    fn scroll_clip(&self, offset: f64) {
        let scroll = self.view.view();
        let clip = scroll.contentView();
        clip.scrollToPoint(NSPoint::new(0.0, offset));
        scroll.reflectScrolledClipView(&clip);
    }
}

/// Замена набора: удаление по прежним номерам и вставка по новым в одном блоке обновления, затем
/// перезагрузка сопоставленных строк по новым номерам. Виды строк при этом переиспользуются.
fn apply_rows(table: &NSTableView, diff: &RowDiff) {
    if !diff.remove.is_empty() || !diff.insert.is_empty() {
        table.beginUpdates();
        if !diff.remove.is_empty() {
            table.removeRowsAtIndexes_withAnimation(
                &index_set(&diff.remove),
                NSTableViewAnimationOptions::empty(),
            );
        }
        if !diff.insert.is_empty() {
            table.insertRowsAtIndexes_withAnimation(
                &index_set(&diff.insert),
                NSTableViewAnimationOptions::empty(),
            );
        }
        table.endUpdates();
    }
    if !diff.reload.is_empty() {
        let columns = NSIndexSet::indexSetWithIndex(0);
        table.reloadDataForRowIndexes_columnIndexes(&index_set(&diff.reload), &columns);
    }
}

fn index_set(ranges: &[Range<usize>]) -> Retained<NSIndexSet> {
    let set = NSMutableIndexSet::new();
    for range in ranges {
        set.addIndexesInRange(NSRange::new(range.start, range.len()));
    }
    Retained::into_super(set)
}

/// Вид строки получает новые цвета: перерисовка заливки и цвет текста ячейки.
fn repaint_row(row: &NSTableRowView, theme: &AppTheme, epoch: u64) {
    if let Some(row) = row.downcast_ref::<ListRow>() {
        row.ivars().epoch.set(epoch);
    }
    row.setNeedsDisplay(true);
    for view in row.subviews().iter() {
        if let Some(cell) = view.downcast_ref::<ListCell>() {
            cell.paint(theme);
        }
    }
}

struct TableIvars {
    _live: Live,
    owner: Callback<ListModel>,
}

define_class!(
    // SAFETY: NSTableView не требует от подкласса ничего сверх переопределённых ниже методов,
    // `Drop` у класса нет.
    #[unsafe(super(NSTableView, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioListTable"]
    #[ivars = TableIvars]
    struct ListTable;

    impl ListTable {
        // SAFETY: селектор и аргумент — как у `keyDown:` NSResponder.
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let Some(key) = list_key(event) else {
                // SAFETY: `keyDown:` у NSTableView с тем же событием.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
                return;
            };
            let stamp = Stamp {
                event: event.timestamp(),
                wake: runloop::last_wake(),
                entry: runloop::uptime(),
            };
            self.ivars().owner.call(|model| model.key(key, &stamp));
        }

        // SAFETY: селектор и результат — как у `becomeFirstResponder` NSResponder. При включённом
        // замере фокус начинает серию задержки: журнал нажатий и тактов сбрасывается, такты идут
        // до потери фокуса.
        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: `becomeFirstResponder` у NSTableView без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted {
                self.start_ticks();
            }
            accepted
        }

        // SAFETY: селектор и результат — как у `resignFirstResponder` NSResponder.
        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            // SAFETY: `resignFirstResponder` у NSTableView без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if accepted {
                self.ivars().owner.call(ListModel::stop_ticks);
            }
            accepted
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. Щелчок по подписи строки
        // сам таблицу first responder не делает (P0.2, находка 4): подпись его не принимает.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
            // SAFETY: `mouseDown:` у NSTableView с тем же событием.
            let _: () = unsafe { msg_send![super(self), mouseDown: event] };
        }
    }
);

impl ListTable {
    fn new(mtm: MainThreadMarker, owner: Callback<ListModel>, theme: &AppTheme) -> Retained<Self> {
        let this = ListTable::alloc(mtm).set_ivars(TableIvars {
            _live: LIST_TABLE.live(),
            owner,
        });
        // SAFETY: назначенный инициализатор NSTableView, ivars уже заданы.
        let table: Retained<ListTable> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        let column = NSTableColumn::initWithIdentifier(
            NSTableColumn::alloc(mtm),
            &NSString::from_str(COLUMN_ID),
        );
        column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
        table.addTableColumn(&column);
        table.setHeaderView(None);
        table.setStyle(NSTableViewStyle::Plain);
        table.setRowHeight(theme.metrics.row);
        table.setIntercellSpacing(NSSize::new(0.0, 0.0));
        table.setBackgroundColor(&NSColor::clearColor());
        table.setGridStyleMask(NSTableViewGridLineStyle::GridNone);
        table.setSelectionHighlightStyle(NSTableViewSelectionHighlightStyle::Regular);
        table.setFocusRingType(NSFocusRingType::None);
        table.setAllowsEmptySelection(true);
        table.setAllowsMultipleSelection(false);
        table.setUsesAlternatingRowBackgroundColors(false);
        table
    }

    fn start_ticks(&self) {
        let owner = self.ivars().owner.clone();
        self.ivars().owner.call(|model| {
            if !model.latency_probe.get() {
                return;
            }
            model.stop_ticks();
            model.presses.borrow_mut().clear();
            model.ticks.borrow_mut().clear();
            if !model.latency_ticks.get() {
                return;
            }
            let ticker = ListTicker::new(self.mtm(), owner);
            // SAFETY: цель — объект с методом `tick:` для `CADisplayLink`; ссылка держит цель до
            // `invalidate`, цель достаёт список только слабой ссылкой.
            let link = unsafe { self.displayLinkWithTarget_selector(&ticker, sel!(tick:)) };
            // SAFETY: главный цикл и общий режим — константы Foundation, ссылка создана на главном
            // потоке.
            unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
            model.link.replace(Some(link));
        });
    }

    fn is_key_focus(&self) -> bool {
        self.window()
            .and_then(|window| window.firstResponder())
            .is_some_and(|responder| {
                std::ptr::eq(
                    std::ptr::from_ref::<NSResponder>(&responder).cast::<u8>(),
                    std::ptr::from_ref::<ListTable>(self).cast::<u8>(),
                )
            })
    }
}

fn list_key(event: &NSEvent) -> Option<ListKey> {
    match event.keyCode() {
        126 => Some(ListKey::Up),
        125 => Some(ListKey::Down),
        36 | 76 => Some(ListKey::Enter),
        53 => Some(ListKey::Escape),
        _ => None,
    }
}

struct TickerIvars {
    _live: Live,
    owner: Callback<ListModel>,
}

define_class!(
    // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioListTicker"]
    #[ivars = TickerIvars]
    struct ListTicker;

    impl ListTicker {
        // SAFETY: селектор и аргумент — как у цели `CADisplayLink`: метод с одним аргументом-ссылкой.
        #[unsafe(method(tick:))]
        fn tick(&self, link: &CADisplayLink) {
            let at = link.timestamp();
            self.ivars().owner.call(|model| {
                let mut ticks = model.ticks.borrow_mut();
                if ticks.len() == TICKS {
                    ticks.pop_front();
                }
                ticks.push_back(at);
            });
        }
    }
);

impl ListTicker {
    fn new(mtm: MainThreadMarker, owner: Callback<ListModel>) -> Retained<ListTicker> {
        let this = ListTicker::alloc(mtm).set_ivars(TickerIvars {
            _live: LIST_TICKER.live(),
            owner,
        });
        // SAFETY: `init` у NSObject, ivars уже заданы.
        unsafe { msg_send![super(this), init] }
    }
}

struct SourceIvars {
    _live: Live,
    owner: Callback<ListModel>,
}

define_class!(
    // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioListSource"]
    #[ivars = SourceIvars]
    struct ListSource;

    // SAFETY: протокол NSObject реализует суперкласс NSObject.
    unsafe impl NSObjectProtocol for ListSource {}

    // SAFETY: методы ниже совпадают с объявлениями протокола.
    unsafe impl NSTableViewDataSource for ListSource {
        // SAFETY: селектор, аргумент и результат — как у `numberOfRowsInTableView:`.
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.ivars()
                .owner
                .call(|model| NSInteger::try_from(model.rows_len()).unwrap_or(NSInteger::MAX))
                .unwrap_or(0)
        }
    }

    // SAFETY: протокол требует только необязательные методы.
    unsafe impl NSControlTextEditingDelegate for ListSource {}

    // SAFETY: методы ниже совпадают с объявлениями протокола.
    unsafe impl NSTableViewDelegate for ListSource {
        // SAFETY: селектор, аргументы и результат — как у `tableView:viewForTableColumn:row:`.
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for(
            &self,
            table: &NSTableView,
            _column: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<NSView>> {
            self.cell_for(table, row)
        }

        // SAFETY: селектор, аргументы и результат — как у `tableView:rowViewForRow:`.
        #[unsafe(method_id(tableView:rowViewForRow:))]
        fn row_view_for(
            &self,
            table: &NSTableView,
            _row: NSInteger,
        ) -> Option<Retained<NSTableRowView>> {
            self.row_for(table)
        }

        // SAFETY: селектор и аргументы — как у `tableView:didAddRowView:forRow:`. Вид строки из
        // очереди переиспользования с прежней эпохой цветов перерисовывается.
        #[unsafe(method(tableView:didAddRowView:forRow:))]
        fn did_add_row_view(&self, _table: &NSTableView, row: &NSTableRowView, _index: NSInteger) {
            self.ivars().owner.call(|model| {
                let stale = row
                    .downcast_ref::<ListRow>()
                    .is_some_and(|row| row.ivars().epoch.get() != model.epoch.get());
                if stale {
                    repaint_row(row, &model.theme.get(), model.epoch.get());
                }
            });
        }

        // SAFETY: селектор, аргументы и результат — как у `tableView:shouldSelectRow:`. Выбор
        // принадлежит модели: щелчок отдаёт намерение, таблица меняется по новому состоянию.
        #[unsafe(method(tableView:shouldSelectRow:))]
        fn should_select_row(&self, _table: &NSTableView, row: NSInteger) -> bool {
            if let Ok(index) = usize::try_from(row) {
                self.ivars().owner.call(|model| {
                    if let Some(id) = model.row_id(index) {
                        (model.on_intent)(ListIntent::Select { id });
                    }
                });
            }
            false
        }
    }
);

impl ListSource {
    fn new(mtm: MainThreadMarker, owner: Callback<ListModel>) -> Retained<ListSource> {
        let this = ListSource::alloc(mtm).set_ivars(SourceIvars {
            _live: LIST_SOURCE.live(),
            owner,
        });
        // SAFETY: `init` у NSObject, ivars уже заданы.
        unsafe { msg_send![super(this), init] }
    }

    /// Вид ячейки строки `row`: из очереди переиспользования или новый.
    fn cell_for(&self, table: &NSTableView, row: NSInteger) -> Option<Retained<NSView>> {
        let index = usize::try_from(row).ok()?;
        self.ivars()
            .owner
            .call(|model| {
                let text = model.row_text(index)?;
                let cell = reuse::<ListCell>(table, CELL_ID).unwrap_or_else(|| {
                    model.cell_views.set(model.cell_views.get() + 1);
                    ListCell::new(table.mtm())
                });
                cell.show(&text, &model.theme.get());
                Some(Retained::into_super(Retained::into_super(cell)))
            })
            .flatten()
    }

    /// Вид строки: из очереди переиспользования или новый, новый считается.
    fn row_for(&self, table: &NSTableView) -> Option<Retained<NSTableRowView>> {
        self.ivars().owner.call(|model| {
            let row = reuse::<ListRow>(table, ROW_ID).unwrap_or_else(|| {
                model.row_views.set(model.row_views.get() + 1);
                ListRow::new(table.mtm(), self.ivars().owner.clone(), model.epoch.get())
            });
            Retained::into_super(row)
        })
    }
}

/// Вид из очереди переиспользования таблицы по идентификатору.
fn reuse<T: objc2::DowncastTarget>(table: &NSTableView, identifier: &str) -> Option<Retained<T>> {
    // SAFETY: владельца у вида нет (`None`), таблица отдаёт вид из своей очереди или `nil`.
    let view = unsafe { table.makeViewWithIdentifier_owner(&NSString::from_str(identifier), None) };
    view.and_then(|view| view.downcast::<T>().ok())
}

struct RowIvars {
    _live: Live,
    owner: Callback<ListModel>,
    /// Эпоха цветов, с которой вид последний раз перекрашен.
    epoch: Cell<u64>,
}

define_class!(
    // SAFETY: NSTableRowView не требует от подкласса ничего сверх переопределённых ниже методов,
    // `Drop` у класса нет.
    #[unsafe(super(NSTableRowView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioListRow"]
    #[ivars = RowIvars]
    struct ListRow;

    impl ListRow {
        // SAFETY: селектор и аргумент — как у `drawRect:` NSView. Отметка отрисовки несёт
        // поколение состояния, которое строка показывает.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            // SAFETY: `drawRect:` у NSTableRowView с той же областью.
            let _: () = unsafe { msg_send![super(self), drawRect: dirty] };
            self.ivars().owner.call(|model| runloop::mark_draw(model.id));
        }

        // SAFETY: селектор и аргумент — как у `drawBackgroundInRect:` NSTableRowView: фона у строки
        // нет, под ней видна подложка.
        #[unsafe(method(drawBackgroundInRect:))]
        fn draw_background(&self, _dirty: NSRect) {}

        // SAFETY: селектор и аргумент — как у `drawSelectionInRect:` NSTableRowView.
        #[unsafe(method(drawSelectionInRect:))]
        fn draw_selection(&self, _dirty: NSRect) {
            let Some(theme) = self.ivars().owner.call(|model| model.theme.get()) else {
                return;
            };
            layer::ns_color(theme.colors.selection).setFill();
            objc2_app_kit::NSRectFill(self.bounds());
        }
    }
);

impl ListRow {
    fn new(mtm: MainThreadMarker, owner: Callback<ListModel>, epoch: u64) -> Retained<ListRow> {
        let this = ListRow::alloc(mtm).set_ivars(RowIvars {
            _live: LIST_ROW.live(),
            owner,
            epoch: Cell::new(epoch),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let row: Retained<ListRow> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        row.setIdentifier(Some(&NSString::from_str(ROW_ID)));
        row
    }
}

struct CellIvars {
    _live: Live,
    label: Retained<NSTextField>,
}

define_class!(
    // SAFETY: NSTableCellView не требует от подкласса ничего сверх переопределённых ниже методов,
    // `Drop` у класса нет.
    #[unsafe(super(NSTableCellView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioListCell"]
    #[ivars = CellIvars]
    struct ListCell;

    impl ListCell {
        // SAFETY: селектор и результат — как у `layout` NSView.
        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), layout] };
            let bounds = self.bounds();
            let label = &self.ivars().label;
            let height = label.fittingSize().height.ceil();
            label.setFrame(NSRect::new(
                NSPoint::new(TEXT_INSET, ((bounds.size.height - height) / 2.0).floor()),
                NSSize::new((bounds.size.width - 2.0 * TEXT_INSET).max(0.0), height),
            ));
        }
    }
);

impl ListCell {
    fn new(mtm: MainThreadMarker) -> Retained<ListCell> {
        let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        let this = ListCell::alloc(mtm).set_ivars(CellIvars {
            _live: LIST_CELL.live(),
            label,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let cell: Retained<ListCell> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        let label = &cell.ivars().label;
        cell.addSubview(label);
        // SAFETY: подпись — подвид ячейки, ячейка держит её в ivars.
        unsafe { cell.setTextField(Some(label)) };
        cell.setIdentifier(Some(&NSString::from_str(CELL_ID)));
        cell
    }

    fn show(&self, text: &str, theme: &AppTheme) {
        let label = &self.ivars().label;
        label.setStringValue(&NSString::from_str(text));
        self.paint(theme);
        self.setNeedsLayout(true);
    }

    fn paint(&self, theme: &AppTheme) {
        let label = &self.ivars().label;
        label.setTextColor(Some(&layer::ns_color(theme.colors.text)));
        // SAFETY: вес шрифта — неизменяемая константа AppKit.
        label.setFont(Some(&NSFont::systemFontOfSize_weight(
            theme.metrics.font.body,
            unsafe { NSFontWeightRegular },
        )));
    }
}
