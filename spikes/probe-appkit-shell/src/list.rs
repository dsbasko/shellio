//! `VirtualList` на view-based `NSTableView`. Виды ячеек и строк создаёт источник данных и
//! отдаёт таблице на переиспользование по идентификатору. Источник данных и делегат —
//! `MainThreadOnly`, таблица держит их слабо, поэтому владелец списка держит их сам и
//! отвязывает при освобождении. Выбор рисует слой строки цветом `--sel`, системная подсветка
//! выключена. Таблица — first responder своей плитки: её фокус сообщается плитке, а клавиши
//! вкладок сначала предлагаются полосе. Up и Down таблица обрабатывает сама, без анимации.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSControl, NSControlTextEditingDelegate, NSEvent,
    NSEventModifierFlags, NSFocusRingType, NSFont, NSResponder, NSScrollView, NSScrollerStyle,
    NSTableCellView, NSTableColumn, NSTableColumnResizingOptions, NSTableRowView, NSTableView,
    NSTableViewColumnAutoresizingStyle, NSTableViewDataSource, NSTableViewDelegate,
    NSTableViewStyle, NSTextField, NSUserInterfaceItemIdentification, NSView,
};
use objc2_foundation::{
    NSIndexSet, NSInteger, NSMutableIndexSet, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString,
};

use crate::chrome::{color, label, rect};
use crate::fixtures::ListRow;
use crate::scenario::counters::{Live, counter};
use crate::tokens::{Rgba, Tokens};

counter!(TABLE, "ProbeListTable");
counter!(SOURCE, "ProbeListSource");
counter!(CELL, "ProbeListCell");
counter!(ROW, "ProbeListRow");

const CELL_ID: &str = "probe.list.cell";
const ROW_ID: &str = "probe.list.row";
const COLUMN_ID: &str = "probe.list.title";
/// Отступ подписи от левого края строки.
pub const TEXT_INSET: f64 = 12.0;

/// Вид списка: фон, высота строки, кегль и отступ подписи, радиус выбора.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListLook {
    pub background: Rgba,
    pub row_height: f64,
    pub font_size: f64,
    pub text_inset: f64,
    pub selection_radius: f64,
}

impl ListLook {
    /// Список плитки: фон `--panel`, строка `--row`, выбор во всю ширину.
    pub fn tile(tokens: &Tokens) -> ListLook {
        ListLook {
            background: tokens.panel,
            row_height: tokens.row,
            font_size: tokens.t_tab,
            text_inset: TEXT_INSET,
            selection_radius: 0.0,
        }
    }
}

/// Связь таблицы с плиткой: фокус и клавиши вкладок.
pub struct Hooks {
    pub on_focus: Box<dyn Fn()>,
    pub on_key: Box<dyn Fn(&NSEvent) -> bool>,
}

pub struct TableIvars {
    _live: Live,
    hooks: RefCell<Option<Hooks>>,
}

define_class!(
    /// Таблица списка. Набор по первым буквам выключен, чтобы клавиши вкладок не выбирали строки.
    #[unsafe(super(NSTableView, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeListTable"]
    #[ivars = TableIvars]
    pub struct ListTable;

    impl ListTable {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let handled = self
                .ivars()
                .hooks
                .borrow()
                .as_ref()
                .is_some_and(|hooks| (hooks.on_key)(event));
            if !handled && !step_selection(self, event) {
                // SAFETY: `keyDown:` у NSTableView с событием клавиши.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
            }
        }

        /// Щелчок по окну без key-статуса, например при открытой палитре, сразу выбирает строку.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        /// Щелчок по подписи ячейки фокус таблице сам не передаёт: окно отдаёт его только
        /// виду под точкой, а подпись first responder не принимает.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
            // SAFETY: `mouseDown:` у NSTableView с событием мыши.
            let _: () = unsafe { msg_send![super(self), mouseDown: event] };
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: `becomeFirstResponder` у NSResponder без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted && let Some(hooks) = self.ivars().hooks.borrow().as_ref() {
                (hooks.on_focus)();
            }
            accepted
        }
    }
);

/// Живой вид списка: число уменьшается, когда AppKit освобождает вид.
struct Tally(Rc<Cell<usize>>);

impl Tally {
    fn new(live: &Rc<Cell<usize>>) -> Tally {
        live.set(live.get() + 1);
        Tally(Rc::clone(live))
    }
}

impl Drop for Tally {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

const KEY_UP: u16 = 126;
const KEY_DOWN: u16 = 125;

/// Up и Down без модификаторов: соседняя строка и прокрутка к ней без анимации. Собственная
/// клавиатурная прокрутка AppKit анимирована и при нажатиях с частотой кадра перелетает
/// выбранную строку на тысячи строк. Тем же путём идут стрелки `TreeList`.
pub fn step_selection(table: &NSTableView, event: &NSEvent) -> bool {
    let step = match event.keyCode() {
        KEY_UP => -1,
        KEY_DOWN => 1,
        _ => return false,
    };
    let held = event.modifierFlags()
        & (NSEventModifierFlags::Shift
            | NSEventModifierFlags::Command
            | NSEventModifierFlags::Option
            | NSEventModifierFlags::Control);
    let rows = table.numberOfRows();
    if !held.is_empty() || rows == 0 {
        return false;
    }
    let current = table.selectedRow();
    let next = if current < 0 {
        0
    } else {
        (current + step).clamp(0, rows - 1)
    };
    table.selectRowIndexes_byExtendingSelection(
        &NSIndexSet::indexSetWithIndex(next as usize),
        false,
    );
    reveal(table, next);
    true
}

/// Прокрутка clip view к строке без анимации: строка встаёт к ближнему краю видимой части.
pub fn reveal(table: &NSTableView, row: NSInteger) {
    let Some(scroll) = table.enclosingScrollView() else {
        return;
    };
    let clip = scroll.contentView();
    let visible = clip.bounds();
    let target = table.rectOfRow(row);
    let top = visible.origin.y;
    let bottom = top + visible.size.height;
    let y = if target.origin.y < top {
        target.origin.y
    } else if target.origin.y + target.size.height > bottom {
        target.origin.y + target.size.height - visible.size.height
    } else {
        return;
    };
    clip.scrollToPoint(NSPoint::new(visible.origin.x, y));
    scroll.reflectScrolledClipView(&clip);
}

pub struct CellIvars {
    _live: Live,
    _tally: Tally,
}

define_class!(
    /// Вид ячейки с подписью; переиспользуется таблицей.
    #[unsafe(super(NSTableCellView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeListCell"]
    #[ivars = CellIvars]
    pub struct ListCell;
);

pub struct RowIvars {
    _live: Live,
    _tally: Tally,
    sel: Rgba,
    radius: f64,
}

define_class!(
    /// Вид строки: выбор — фон слоя цвета `--sel`, системная подсветка не рисуется.
    #[unsafe(super(NSTableRowView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeListRow"]
    #[ivars = RowIvars]
    pub struct ListRowView;

    impl ListRowView {
        #[unsafe(method(setSelected:))]
        fn set_selected(&self, selected: bool) {
            // SAFETY: `setSelected:` у NSTableRowView с BOOL.
            let _: () = unsafe { msg_send![super(self), setSelected: selected] };
            self.paint(selected);
        }

        #[unsafe(method(drawSelectionInRect:))]
        fn draw_selection(&self, _rect: NSRect) {}

        /// Фон строки AppKit рисует в содержимое слоя поверх его `backgroundColor`, и заливка
        /// выбора не видна.
        #[unsafe(method(drawBackgroundInRect:))]
        fn draw_background(&self, _rect: NSRect) {}
    }
);

impl ListRowView {
    fn paint(&self, selected: bool) {
        if let Some(layer) = self.layer() {
            let background = selected.then(|| color(self.ivars().sel, 1.0).CGColor());
            layer.setBackgroundColor(background.as_deref());
            layer.setCornerRadius(self.ivars().radius);
        }
    }
}

pub struct SourceIvars {
    _live: Live,
    rows: RefCell<Vec<ListRow>>,
    text: Rgba,
    sel: Rgba,
    look: ListLook,
    cells_created: Cell<usize>,
    rows_created: Cell<usize>,
    cells_live: Rc<Cell<usize>>,
    rows_live: Rc<Cell<usize>>,
}

define_class!(
    /// Источник данных и делегат таблицы. Ссылок на виды не держит.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeListSource"]
    #[ivars = SourceIvars]
    pub struct ListSource;

    unsafe impl NSObjectProtocol for ListSource {}

    unsafe impl NSTableViewDataSource for ListSource {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.ivars().rows.borrow().len() as NSInteger
        }
    }

    unsafe impl NSControlTextEditingDelegate for ListSource {}

    unsafe impl NSTableViewDelegate for ListSource {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for(
            &self,
            table: &NSTableView,
            _column: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<NSView>> {
            self.filled_cell(table, row)
        }

        #[unsafe(method_id(tableView:rowViewForRow:))]
        fn row_view_for(
            &self,
            table: &NSTableView,
            _row: NSInteger,
        ) -> Option<Retained<NSTableRowView>> {
            Some(Retained::into_super(self.row_view(table)))
        }
    }
);

impl ListSource {
    fn new(
        mtm: MainThreadMarker,
        tokens: &Tokens,
        rows: Vec<ListRow>,
        look: ListLook,
    ) -> Retained<ListSource> {
        let this = ListSource::alloc(mtm).set_ivars(SourceIvars {
            _live: SOURCE.live(),
            rows: RefCell::new(rows),
            text: tokens.text,
            sel: tokens.sel,
            look,
            cells_created: Cell::new(0),
            rows_created: Cell::new(0),
            cells_live: Rc::new(Cell::new(0)),
            rows_live: Rc::new(Cell::new(0)),
        });
        // SAFETY: `init` — назначенный инициализатор NSObject.
        unsafe { msg_send![super(this), init] }
    }

    fn filled_cell(&self, table: &NSTableView, row: NSInteger) -> Option<Retained<NSView>> {
        let title = self
            .ivars()
            .rows
            .borrow()
            .get(usize::try_from(row).ok()?)?
            .title
            .clone();
        let cell = self.cell(table);
        // SAFETY: `textField` у NSTableCellView; поле задано при создании ячейки.
        if let Some(field) = unsafe { cell.textField() } {
            field.setStringValue(&NSString::from_str(&title));
        }
        Some(Retained::into_super(Retained::into_super(cell)))
    }

    fn cell(&self, table: &NSTableView) -> Retained<ListCell> {
        let identifier = NSString::from_str(CELL_ID);
        // SAFETY: владелец не нужен: ячейка создаётся кодом, а не из nib.
        let reused = unsafe { table.makeViewWithIdentifier_owner(&identifier, None) };
        if let Some(cell) = reused.and_then(|view| view.downcast::<ListCell>().ok()) {
            return cell;
        }
        let mtm = self.mtm();
        let ivars = self.ivars();
        ivars.cells_created.set(ivars.cells_created.get() + 1);
        let look = ivars.look;
        let frame = rect(0.0, 0.0, 200.0, look.row_height);
        let this = ListCell::alloc(mtm).set_ivars(CellIvars {
            _live: CELL.live(),
            _tally: Tally::new(&ivars.cells_live),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let cell: Retained<ListCell> = unsafe { msg_send![super(this), initWithFrame: frame] };
        cell.setIdentifier(Some(&identifier));
        let field: Retained<NSTextField> = label(
            mtm,
            "",
            ivars.text,
            rect(
                look.text_inset,
                (look.row_height - 18.0) / 2.0,
                200.0 - 2.0 * look.text_inset,
                18.0,
            ),
        );
        field.setFont(Some(&NSFont::systemFontOfSize(look.font_size)));
        field.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        cell.addSubview(&field);
        // SAFETY: поле — подвид ячейки и живёт вместе с ней.
        unsafe { cell.setTextField(Some(&field)) };
        cell
    }

    fn row_view(&self, table: &NSTableView) -> Retained<ListRowView> {
        let identifier = NSString::from_str(ROW_ID);
        // SAFETY: владелец не нужен: строка создаётся кодом, а не из nib.
        let reused = unsafe { table.makeViewWithIdentifier_owner(&identifier, None) };
        if let Some(row) = reused.and_then(|view| view.downcast::<ListRowView>().ok()) {
            return row;
        }
        let ivars = self.ivars();
        ivars.rows_created.set(ivars.rows_created.get() + 1);
        let frame = rect(0.0, 0.0, 200.0, ivars.look.row_height);
        let this = ListRowView::alloc(self.mtm()).set_ivars(RowIvars {
            _live: ROW.live(),
            _tally: Tally::new(&ivars.rows_live),
            sel: ivars.sel,
            radius: ivars.look.selection_radius,
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let row: Retained<ListRowView> = unsafe { msg_send![super(this), initWithFrame: frame] };
        row.setIdentifier(Some(&identifier));
        row.setWantsLayer(true);
        row
    }
}

/// Список: прокрутка, таблица и источник данных, которыми владеет плитка.
pub struct VirtualList {
    scroll: Retained<NSScrollView>,
    table: Retained<ListTable>,
    source: Retained<ListSource>,
}

impl VirtualList {
    pub fn new(
        mtm: MainThreadMarker,
        tokens: &Tokens,
        frame: NSRect,
        rows: Vec<ListRow>,
        look: ListLook,
    ) -> VirtualList {
        let source = ListSource::new(mtm, tokens, rows, look);
        let this = ListTable::alloc(mtm).set_ivars(TableIvars {
            _live: TABLE.live(),
            hooks: RefCell::new(None),
        });
        let table_frame = rect(0.0, 0.0, frame.size.width, frame.size.height);
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let table: Retained<ListTable> =
            unsafe { msg_send![super(this), initWithFrame: table_frame] };
        table.setStyle(NSTableViewStyle::Plain);
        table.setHeaderView(None);
        table.setIntercellSpacing(NSSize::new(0.0, 0.0));
        table.setUsesAutomaticRowHeights(false);
        table.setRowHeight(look.row_height);
        table.setAllowsTypeSelect(false);
        table.setUsesAlternatingRowBackgroundColors(false);
        table.setBackgroundColor(&color(look.background, 1.0));
        table.setFocusRingType(NSFocusRingType::None);
        table.setColumnAutoresizingStyle(
            NSTableViewColumnAutoresizingStyle::UniformColumnAutoresizingStyle,
        );
        let column = NSTableColumn::initWithIdentifier(
            NSTableColumn::alloc(mtm),
            &NSString::from_str(COLUMN_ID),
        );
        column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
        column.setWidth(frame.size.width);
        table.addTableColumn(&column);
        // SAFETY: таблица держит источник слабо, владелец списка держит его сам и отвязывает в
        // `Drop` до освобождения.
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(&*source)));
            table.setDelegate(Some(ProtocolObject::from_ref(&*source)));
        }

        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        scroll.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        scroll.setDrawsBackground(false);
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(false);
        scroll.setAutohidesScrollers(true);
        scroll.setScrollerStyle(NSScrollerStyle::Overlay);
        scroll.setAutomaticallyAdjustsContentInsets(false);
        scroll.setDocumentView(Some(&table));
        table.sizeLastColumnToFit();
        VirtualList {
            scroll,
            table,
            source,
        }
    }

    pub fn view(&self) -> &NSScrollView {
        &self.scroll
    }

    pub fn table(&self) -> &ListTable {
        &self.table
    }

    pub fn set_hooks(&self, hooks: Hooks) {
        *self.table.ivars().hooks.borrow_mut() = Some(hooks);
    }

    pub fn len(&self) -> usize {
        self.source.ivars().rows.borrow().len()
    }

    /// Новый набор строк: выбор снимается, список встаёт в начало.
    pub fn set_rows(&self, rows: Vec<ListRow>) {
        *self.source.ivars().rows.borrow_mut() = rows;
        self.table
            .selectRowIndexes_byExtendingSelection(&NSIndexSet::new(), false);
        self.table.reloadData();
        self.table.scrollRowToVisible(0);
    }

    /// Выбранная строка; без выбора `None`.
    pub fn selected(&self) -> Option<usize> {
        usize::try_from(self.table.selectedRow()).ok()
    }

    /// Выбор строки и прокрутка к ней без анимации.
    pub fn select(&self, row: usize) {
        self.table
            .selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(row), false);
        reveal(&self.table, row as NSInteger);
    }

    /// Идентификатор строки модели.
    pub fn id(&self, index: usize) -> Option<u64> {
        self.source
            .ivars()
            .rows
            .borrow()
            .get(index)
            .map(|row| row.id)
    }

    /// Правка строк модели с перезагрузкой только их.
    pub fn update_rows(&self, indexes: &[usize], edit: impl Fn(&ListRow) -> ListRow) {
        let changed = NSMutableIndexSet::new();
        {
            let mut rows = self.source.ivars().rows.borrow_mut();
            for index in indexes {
                if let Some(row) = rows.get_mut(*index) {
                    *row = edit(row);
                    changed.addIndex(*index);
                }
            }
        }
        self.table
            .reloadDataForRowIndexes_columnIndexes(&changed, &NSIndexSet::indexSetWithIndex(0));
    }

    /// Подпись строки модели.
    pub fn title(&self, index: usize) -> Option<String> {
        self.source
            .ivars()
            .rows
            .borrow()
            .get(index)
            .map(|row| row.title.clone())
    }

    /// Видов ячеек и строк, созданных этим списком.
    pub fn created(&self) -> (usize, usize) {
        let ivars = self.source.ivars();
        (ivars.cells_created.get(), ivars.rows_created.get())
    }

    /// Живых видов ячеек и строк этого списка.
    pub fn live(&self) -> (usize, usize) {
        let ivars = self.source.ivars();
        (ivars.cells_live.get(), ivars.rows_live.get())
    }
}

impl Drop for VirtualList {
    fn drop(&mut self) {
        *self.table.ivars().hooks.borrow_mut() = None;
        // SAFETY: отвязка источника до его освобождения.
        unsafe {
            self.table.setDataSource(None);
            self.table.setDelegate(None);
        }
    }
}
