//! `TreeList` на view-based `NSOutlineView`. Узлы — объекты Objective-C, которые строятся один
//! раз и принадлежат источнику данных: дерево отдаёт AppKit одни и те же объекты, поэтому
//! идентичность узла переживает перезагрузку. Перезагрузка возвращает раскрытие и выбор узлам,
//! а не номерам строк. Выбор рисует слой строки цветом `--sel`, рамку `--focus` строка получает,
//! когда фокус в навигаторе. Up и Down идут общим путём `VirtualList`, Left и Right раскрывает и
//! сворачивает сам `NSOutlineView`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAutoresizingMaskOptions, NSControl, NSControlTextEditingDelegate, NSEvent,
    NSFocusRingType, NSFont, NSOutlineView, NSOutlineViewDataSource, NSOutlineViewDelegate,
    NSResponder, NSScrollView, NSScrollerStyle, NSTableCellView, NSTableColumn,
    NSTableColumnResizingOptions, NSTableRowView, NSTableView, NSTableViewColumnAutoresizingStyle,
    NSTableViewStyle, NSTextField, NSUserInterfaceItemIdentification, NSView,
};
use objc2_foundation::{
    NSIndexSet, NSInteger, NSObject, NSObjectProtocol, NSRect, NSSize, NSString,
};

use crate::chrome::{color, label, rect};
use crate::fixtures::TreeSpec;
use crate::list;
use crate::scenario::counters::{Count, Live, counter};
use crate::tokens::{Rgba, Tokens};

counter!(OUTLINE, "ProbeTreeOutline");
counter!(SOURCE, "ProbeTreeSource");
counter!(NODE, "ProbeTreeNode");
counter!(CELL, "ProbeTreeCell");
counter!(ROW, "ProbeTreeRow");

const CELL_ID: &str = "probe.tree.cell";
const ROW_ID: &str = "probe.tree.row";
const COLUMN_ID: &str = "probe.tree.title";
/// Отступ уровня: `.tree.l1` прототипа сдвинут на 16 точек от корня.
pub const INDENT: f64 = 16.0;
const TEXT_INSET: f64 = 4.0;

pub struct NodeIvars {
    _live: Live,
    id: u64,
    title: String,
    children: Vec<Retained<TreeNode>>,
}

define_class!(
    /// Узел дерева. Детей держит родитель, ссылки на родителя нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTreeNode"]
    #[ivars = NodeIvars]
    pub struct TreeNode;
);

impl TreeNode {
    fn build(mtm: MainThreadMarker, spec: &TreeSpec) -> Retained<TreeNode> {
        let children = spec
            .children
            .iter()
            .map(|child| TreeNode::build(mtm, child))
            .collect();
        let this = TreeNode::alloc(mtm).set_ivars(NodeIvars {
            _live: NODE.live(),
            id: spec.id,
            title: spec.title.clone(),
            children,
        });
        // SAFETY: `init` — назначенный инициализатор NSObject.
        unsafe { msg_send![super(this), init] }
    }

    pub fn id(&self) -> u64 {
        self.ivars().id
    }

    pub fn title(&self) -> &str {
        &self.ivars().title
    }

    pub fn children(&self) -> &[Retained<TreeNode>] {
        &self.ivars().children
    }

    fn item(&self) -> &AnyObject {
        self
    }
}

fn node(item: Option<&AnyObject>) -> Option<&TreeNode> {
    item?.downcast_ref::<TreeNode>()
}

/// Счётчик узлов: создано и освобождено за процесс.
pub fn node_count() -> Count {
    NODE.count()
}

pub struct OutlineIvars {
    _live: Live,
    on_focus: RefCell<Option<Box<dyn Fn()>>>,
}

define_class!(
    /// Дерево навигатора. Набор по первым буквам выключен, пустой выбор запрещён.
    #[unsafe(super(NSOutlineView, NSTableView, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTreeOutline"]
    #[ivars = OutlineIvars]
    pub struct TreeOutline;

    impl TreeOutline {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !list::step_selection(self, event) {
                // SAFETY: `keyDown:` у NSOutlineView с событием клавиши.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
            }
        }

        /// Щелчок по подписи ячейки фокус дереву сам не передаёт, как и у списка.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
            // SAFETY: `mouseDown:` у NSOutlineView с событием мыши.
            let _: () = unsafe { msg_send![super(self), mouseDown: event] };
        }

        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: `becomeFirstResponder` у NSResponder без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted && let Some(on_focus) = self.ivars().on_focus.borrow().as_ref() {
                on_focus();
            }
            accepted
        }
    }
);

/// Живой вид дерева: число уменьшается, когда AppKit освобождает вид.
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

pub struct CellIvars {
    _live: Live,
    _tally: Tally,
}

define_class!(
    /// Вид ячейки с подписью; переиспользуется деревом.
    #[unsafe(super(NSTableCellView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTreeCell"]
    #[ivars = CellIvars]
    pub struct TreeCell;
);

#[derive(Clone, Copy)]
struct Paint {
    sel: Rgba,
    focus: Rgba,
    text: Rgba,
    text_2: Rgba,
    radius: f64,
}

pub struct RowIvars {
    _live: Live,
    _tally: Tally,
    paint: Paint,
    /// Фокус в навигаторе: общий для всех строк дерева.
    focused: Rc<Cell<bool>>,
}

define_class!(
    /// Вид строки: выбор — фон `--sel`, при фокусе в навигаторе ещё рамка `--focus`; подпись
    /// выбранной строки `--text`, остальных `--text-2`. Системная подсветка не рисуется.
    #[unsafe(super(NSTableRowView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTreeRow"]
    #[ivars = RowIvars]
    pub struct TreeRowView;

    impl TreeRowView {
        #[unsafe(method(setSelected:))]
        fn set_selected(&self, selected: bool) {
            // SAFETY: `setSelected:` у NSTableRowView с BOOL.
            let _: () = unsafe { msg_send![super(self), setSelected: selected] };
            self.paint();
        }

        #[unsafe(method(didAddSubview:))]
        fn did_add_subview(&self, subview: &NSView) {
            // SAFETY: `didAddSubview:` у NSView с добавленным видом.
            let _: () = unsafe { msg_send![super(self), didAddSubview: subview] };
            self.paint();
        }

        #[unsafe(method(drawSelectionInRect:))]
        fn draw_selection(&self, _rect: NSRect) {}

        /// Фон строки AppKit рисует в содержимое слоя поверх его `backgroundColor`, как у
        /// строки списка.
        #[unsafe(method(drawBackgroundInRect:))]
        fn draw_background(&self, _rect: NSRect) {}
    }
);

impl TreeRowView {
    fn paint(&self) {
        let ivars = self.ivars();
        let paint = ivars.paint;
        let selected = self.isSelected();
        if let Some(layer) = self.layer() {
            let background = selected.then(|| color(paint.sel, 1.0).CGColor());
            layer.setBackgroundColor(background.as_deref());
            layer.setCornerRadius(paint.radius);
            if selected && ivars.focused.get() {
                layer.setBorderWidth(1.0);
                layer.setBorderColor(Some(&color(paint.focus, 1.0).CGColor()));
            } else {
                layer.setBorderWidth(0.0);
                layer.setBorderColor(None);
            }
        }
        let ink = color(if selected { paint.text } else { paint.text_2 }, 1.0);
        for view in self.subviews() {
            if let Ok(cell) = view.downcast::<TreeCell>()
                // SAFETY: `textField` у NSTableCellView; поле задано при создании ячейки.
                && let Some(field) = unsafe { cell.textField() }
            {
                field.setTextColor(Some(&ink));
            }
        }
    }
}

pub struct SourceIvars {
    _live: Live,
    roots: RefCell<Vec<Retained<TreeNode>>>,
    paint: Paint,
    font_size: f64,
    row_height: f64,
    focused: Rc<Cell<bool>>,
    cells_created: Cell<usize>,
    rows_created: Cell<usize>,
    cells_live: Rc<Cell<usize>>,
    rows_live: Rc<Cell<usize>>,
}

define_class!(
    /// Источник данных и делегат дерева. Держит узлы, ссылок на виды не держит.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeTreeSource"]
    #[ivars = SourceIvars]
    pub struct TreeSource;

    unsafe impl NSObjectProtocol for TreeSource {}

    unsafe impl NSOutlineViewDataSource for TreeSource {
        #[unsafe(method(outlineView:numberOfChildrenOfItem:))]
        fn number_of_children(
            &self,
            _outline: &NSOutlineView,
            item: Option<&AnyObject>,
        ) -> NSInteger {
            match item {
                None => self.ivars().roots.borrow().len() as NSInteger,
                Some(_) => node(item).map_or(0, |node| node.children().len() as NSInteger),
            }
        }

        #[unsafe(method_id(outlineView:child:ofItem:))]
        fn child(
            &self,
            _outline: &NSOutlineView,
            index: NSInteger,
            item: Option<&AnyObject>,
        ) -> Option<Retained<AnyObject>> {
            self.child_of(index, item)
                .map(|child| Retained::into_super(Retained::into_super(child)))
        }

        #[unsafe(method(outlineView:isItemExpandable:))]
        fn is_expandable(&self, _outline: &NSOutlineView, item: &AnyObject) -> bool {
            node(Some(item)).is_some_and(|node| !node.children().is_empty())
        }
    }

    unsafe impl NSControlTextEditingDelegate for TreeSource {}

    unsafe impl NSOutlineViewDelegate for TreeSource {
        #[unsafe(method_id(outlineView:viewForTableColumn:item:))]
        fn view_for(
            &self,
            outline: &NSOutlineView,
            _column: Option<&NSTableColumn>,
            item: &AnyObject,
        ) -> Option<Retained<NSView>> {
            self.filled_cell(outline, item)
        }

        #[unsafe(method_id(outlineView:rowViewForItem:))]
        fn row_view_for(
            &self,
            outline: &NSOutlineView,
            _item: &AnyObject,
        ) -> Option<Retained<NSTableRowView>> {
            Some(Retained::into_super(self.row_view(outline)))
        }
    }
);

impl TreeSource {
    fn new(mtm: MainThreadMarker, tokens: &Tokens, spec: &[TreeSpec]) -> Retained<TreeSource> {
        let roots = spec.iter().map(|root| TreeNode::build(mtm, root)).collect();
        let this = TreeSource::alloc(mtm).set_ivars(SourceIvars {
            _live: SOURCE.live(),
            roots: RefCell::new(roots),
            paint: Paint {
                sel: tokens.sel,
                focus: tokens.focus,
                text: tokens.text,
                text_2: tokens.text_2,
                radius: tokens.r_row,
            },
            font_size: tokens.t_tab,
            row_height: tokens.row,
            focused: Rc::new(Cell::new(false)),
            cells_created: Cell::new(0),
            rows_created: Cell::new(0),
            cells_live: Rc::new(Cell::new(0)),
            rows_live: Rc::new(Cell::new(0)),
        });
        // SAFETY: `init` — назначенный инициализатор NSObject.
        unsafe { msg_send![super(this), init] }
    }

    fn child_of(&self, index: NSInteger, item: Option<&AnyObject>) -> Option<Retained<TreeNode>> {
        let index = usize::try_from(index).ok()?;
        match item {
            None => self.ivars().roots.borrow().get(index).cloned(),
            Some(_) => node(item)?.children().get(index).cloned(),
        }
    }

    fn filled_cell(&self, outline: &NSOutlineView, item: &AnyObject) -> Option<Retained<NSView>> {
        let title = node(Some(item))?.title().to_owned();
        let cell = self.cell(outline);
        // SAFETY: `textField` у NSTableCellView; поле задано при создании ячейки.
        if let Some(field) = unsafe { cell.textField() } {
            field.setStringValue(&NSString::from_str(&title));
        }
        Some(Retained::into_super(Retained::into_super(cell)))
    }

    fn cell(&self, outline: &NSOutlineView) -> Retained<TreeCell> {
        let identifier = NSString::from_str(CELL_ID);
        // SAFETY: владелец не нужен: ячейка создаётся кодом, а не из nib.
        let reused = unsafe { outline.makeViewWithIdentifier_owner(&identifier, None) };
        if let Some(cell) = reused.and_then(|view| view.downcast::<TreeCell>().ok()) {
            return cell;
        }
        let mtm = self.mtm();
        let ivars = self.ivars();
        ivars.cells_created.set(ivars.cells_created.get() + 1);
        let width = 200.0;
        let frame = rect(0.0, 0.0, width, ivars.row_height);
        let this = TreeCell::alloc(mtm).set_ivars(CellIvars {
            _live: CELL.live(),
            _tally: Tally::new(&ivars.cells_live),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let cell: Retained<TreeCell> = unsafe { msg_send![super(this), initWithFrame: frame] };
        cell.setIdentifier(Some(&identifier));
        let field: Retained<NSTextField> = label(
            mtm,
            "",
            ivars.paint.text_2,
            rect(
                TEXT_INSET,
                (ivars.row_height - 18.0) / 2.0,
                width - 2.0 * TEXT_INSET,
                18.0,
            ),
        );
        field.setFont(Some(&NSFont::systemFontOfSize(ivars.font_size)));
        field.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        cell.addSubview(&field);
        // SAFETY: поле — подвид ячейки и живёт вместе с ней.
        unsafe { cell.setTextField(Some(&field)) };
        cell
    }

    fn row_view(&self, outline: &NSOutlineView) -> Retained<TreeRowView> {
        let identifier = NSString::from_str(ROW_ID);
        // SAFETY: владелец не нужен: строка создаётся кодом, а не из nib.
        let reused = unsafe { outline.makeViewWithIdentifier_owner(&identifier, None) };
        if let Some(row) = reused.and_then(|view| view.downcast::<TreeRowView>().ok()) {
            return row;
        }
        let ivars = self.ivars();
        ivars.rows_created.set(ivars.rows_created.get() + 1);
        let frame = rect(0.0, 0.0, 200.0, ivars.row_height);
        let this = TreeRowView::alloc(self.mtm()).set_ivars(RowIvars {
            _live: ROW.live(),
            _tally: Tally::new(&ivars.rows_live),
            paint: ivars.paint,
            focused: Rc::clone(&ivars.focused),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let row: Retained<TreeRowView> = unsafe { msg_send![super(this), initWithFrame: frame] };
        row.setIdentifier(Some(&identifier));
        row.setWantsLayer(true);
        row
    }
}

/// Что AppKit сделал сам при `reloadData` и что вернуло дерево.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reload {
    /// Строка и узел выбора до перезагрузки.
    pub selected_before: Option<(isize, u64)>,
    /// Выбор AppKit сразу после `reloadData`, до возврата выбора узлу.
    pub appkit_selected: Option<(isize, Option<u64>)>,
    pub expanded_before: usize,
    /// Раскрытые узлы, которые AppKit сам оставил раскрытыми.
    pub appkit_kept_expanded: usize,
    /// Выбор после возврата узлу.
    pub selected_after: Option<(isize, u64)>,
}

/// Дерево: прокрутка, `NSOutlineView` и источник данных, которыми владеет навигатор.
pub struct TreeList {
    scroll: Retained<NSScrollView>,
    outline: Retained<TreeOutline>,
    source: Retained<TreeSource>,
}

impl TreeList {
    pub fn new(
        mtm: MainThreadMarker,
        tokens: &Tokens,
        frame: NSRect,
        spec: &[TreeSpec],
    ) -> TreeList {
        let source = TreeSource::new(mtm, tokens, spec);
        let this = TreeOutline::alloc(mtm).set_ivars(OutlineIvars {
            _live: OUTLINE.live(),
            on_focus: RefCell::new(None),
        });
        let outline_frame = rect(0.0, 0.0, frame.size.width, frame.size.height);
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        let outline: Retained<TreeOutline> =
            unsafe { msg_send![super(this), initWithFrame: outline_frame] };
        outline.setStyle(NSTableViewStyle::Plain);
        outline.setHeaderView(None);
        outline.setIntercellSpacing(NSSize::new(0.0, 0.0));
        outline.setUsesAutomaticRowHeights(false);
        outline.setRowHeight(tokens.row);
        outline.setAllowsTypeSelect(false);
        outline.setAllowsEmptySelection(false);
        outline.setAllowsMultipleSelection(false);
        outline.setUsesAlternatingRowBackgroundColors(false);
        outline.setBackgroundColor(&color(tokens.chrome, 1.0));
        outline.setFocusRingType(NSFocusRingType::None);
        outline.setIndentationPerLevel(INDENT);
        outline.setIndentationMarkerFollowsCell(true);
        outline.setAutoresizesOutlineColumn(false);
        outline.setColumnAutoresizingStyle(
            NSTableViewColumnAutoresizingStyle::UniformColumnAutoresizingStyle,
        );
        outline.setAccessibilityLabel(Some(&NSString::from_str("Навигатор")));
        let column = NSTableColumn::initWithIdentifier(
            NSTableColumn::alloc(mtm),
            &NSString::from_str(COLUMN_ID),
        );
        column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
        column.setWidth(frame.size.width);
        outline.addTableColumn(&column);
        // SAFETY: колонка добавлена в дерево и живёт вместе с ним. Источник дерево держит
        // слабо, владелец дерева держит его сам и отвязывает в `Drop` до освобождения.
        unsafe {
            outline.setOutlineTableColumn(Some(&column));
            outline.setDataSource(Some(ProtocolObject::from_ref(&*source)));
            outline.setDelegate(Some(ProtocolObject::from_ref(&*source)));
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
        scroll.setDocumentView(Some(&outline));
        outline.sizeLastColumnToFit();
        TreeList {
            scroll,
            outline,
            source,
        }
    }

    pub fn view(&self) -> &NSScrollView {
        &self.scroll
    }

    pub fn outline(&self) -> &TreeOutline {
        &self.outline
    }

    pub fn set_on_focus(&self, on_focus: Box<dyn Fn()>) {
        *self.outline.ivars().on_focus.borrow_mut() = Some(on_focus);
    }

    /// Корни в текущем порядке.
    pub fn roots(&self) -> Vec<Retained<TreeNode>> {
        self.source.ivars().roots.borrow().clone()
    }

    /// Узлов в модели.
    pub fn len(&self) -> usize {
        fn count(nodes: &[Retained<TreeNode>]) -> usize {
            nodes.iter().map(|node| 1 + count(node.children())).sum()
        }
        count(&self.source.ivars().roots.borrow())
    }

    pub fn node_at(&self, row: isize) -> Option<Retained<TreeNode>> {
        self.outline.itemAtRow(row)?.downcast::<TreeNode>().ok()
    }

    pub fn row_of(&self, node: &TreeNode) -> isize {
        // SAFETY: узел — объект этого дерева.
        unsafe { self.outline.rowForItem(Some(node.item())) }
    }

    pub fn is_expanded(&self, node: &TreeNode) -> bool {
        // SAFETY: узел — объект этого дерева.
        unsafe { self.outline.isItemExpanded(Some(node.item())) }
    }

    pub fn expand(&self, node: &TreeNode, children: bool) {
        // SAFETY: узел — объект этого дерева.
        unsafe {
            self.outline
                .expandItem_expandChildren(Some(node.item()), children)
        };
    }

    pub fn collapse(&self, node: &TreeNode, children: bool) {
        // SAFETY: узел — объект этого дерева.
        unsafe {
            self.outline
                .collapseItem_collapseChildren(Some(node.item()), children)
        };
    }

    /// Раскрытие всех корней без их потомков.
    pub fn expand_roots(&self) {
        for root in self.roots() {
            self.expand(&root, false);
        }
    }

    pub fn collapse_roots(&self) {
        for root in self.roots() {
            self.collapse(&root, false);
        }
    }

    pub fn select_row(&self, row: isize) {
        if let Ok(row) = usize::try_from(row) {
            self.outline
                .selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(row), false);
        }
    }

    pub fn selected_node(&self) -> Option<Retained<TreeNode>> {
        self.node_at(self.outline.selectedRow())
    }

    /// Вид выбранной строки, если он создан.
    pub fn selected_row_view(&self) -> Option<Retained<NSTableRowView>> {
        let row = self.outline.selectedRow();
        (row >= 0)
            .then(|| self.outline.rowViewAtRow_makeIfNecessary(row, false))
            .flatten()
    }

    /// Фокус в навигаторе: рамка у выбранной строки.
    pub fn set_focused(&self, focused: bool) {
        self.source.ivars().focused.set(focused);
        if let Some(row) = self
            .selected_row_view()
            .and_then(|row| row.downcast::<TreeRowView>().ok())
        {
            row.paint();
        }
    }

    /// Раскрытые узлы в прямом обходе: родитель раньше потомков.
    fn expanded_nodes(&self) -> Vec<Retained<TreeNode>> {
        fn walk(tree: &TreeList, nodes: &[Retained<TreeNode>], out: &mut Vec<Retained<TreeNode>>) {
            for node in nodes {
                if tree.is_expanded(node) {
                    out.push(node.clone());
                    walk(tree, node.children(), out);
                }
            }
        }
        let mut out = Vec::new();
        walk(self, &self.roots(), &mut out);
        out
    }

    /// `reloadData` с возвратом раскрытия и выбора узлам; выбранная строка видима после.
    pub fn reload(&self) -> Reload {
        let expanded = self.expanded_nodes();
        let selected = self.selected_node();
        let selected_before = selected
            .as_ref()
            .map(|node| (self.outline.selectedRow(), node.id()));
        self.outline.reloadData();
        let appkit_selected = {
            let row = self.outline.selectedRow();
            (row >= 0).then(|| (row, self.node_at(row).map(|node| node.id())))
        };
        let mut kept = 0;
        for node in &expanded {
            if self.is_expanded(node) {
                kept += 1;
            } else {
                self.expand(node, false);
            }
        }
        if let Some(node) = &selected {
            let row = self.row_of(node);
            if row >= 0 && self.outline.selectedRow() != row {
                self.select_row(row);
            }
            if row >= 0 {
                list::reveal(&self.outline, row);
            }
        }
        let selected_after = self
            .selected_node()
            .map(|node| (self.outline.selectedRow(), node.id()));
        Reload {
            selected_before,
            appkit_selected,
            expanded_before: expanded.len(),
            appkit_kept_expanded: kept,
            selected_after,
        }
    }

    /// Обратный порядок корней теми же объектами узлов, затем перезагрузка.
    pub fn reverse_roots(&self) -> Reload {
        self.source.ivars().roots.borrow_mut().reverse();
        self.reload()
    }

    /// Видов ячеек и строк, созданных этим деревом.
    pub fn created(&self) -> (usize, usize) {
        let ivars = self.source.ivars();
        (ivars.cells_created.get(), ivars.rows_created.get())
    }

    /// Живых видов ячеек и строк этого дерева.
    pub fn live(&self) -> (usize, usize) {
        let ivars = self.source.ivars();
        (ivars.cells_live.get(), ivars.rows_live.get())
    }
}

impl Drop for TreeList {
    fn drop(&mut self) {
        *self.outline.ivars().on_focus.borrow_mut() = None;
        // SAFETY: отвязка источника до его освобождения.
        unsafe {
            self.outline.setDataSource(None);
            self.outline.setDelegate(None);
        }
    }
}
