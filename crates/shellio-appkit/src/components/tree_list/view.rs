//! Вид `TreeList` на главном потоке.
//!
//! Дерево — view-based `NSOutlineView` в `NSScrollView` без заголовка и фона: под строками видна
//! подложка. Узлы — объекты Objective-C, по одному на id на всю жизнь дерева: детей держит
//! родитель, корни и узлы по id держит Rust-часть (P0.2, инвариант 7). Поэтому раскрытие и выбор
//! переживают перезагрузку детей, а прежние узлы живут до конца применения операций. Источник
//! данных и делегат — один объект, его держит владелец и снимает в `Drop` (инвариант 1).
//!
//! Раскрытие и выбор принадлежат модели. Системный треугольник раскрытия скрыт (его цвет не из
//! токенов, P0.2, находка 5), свой треугольник рисуется токеном `text_3` и отдаёт намерение
//! `toggle`. Щелчок по строке, Up, Down, Left и Right отдают намерения, дерево меняется только по
//! новому состоянию.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSBezierPath, NSColor, NSControl, NSControlTextEditingDelegate, NSEvent,
    NSFocusRingType, NSFont, NSFontWeightRegular, NSLineBreakMode, NSOutlineView,
    NSOutlineViewDataSource, NSOutlineViewDelegate, NSResponder, NSScrollView, NSTableCellView,
    NSTableColumn, NSTableColumnResizingOptions, NSTableRowView, NSTableView,
    NSTableViewGridLineStyle, NSTableViewSelectionHighlightStyle, NSTableViewStyle, NSTextField,
    NSUserInterfaceItemIdentification, NSView,
};
use objc2_foundation::{NSIndexSet, NSInteger, NSPoint, NSRect, NSSize, NSString};
use shellio_theme::AppTheme;

use super::model::{
    self, TreeListError, TreeListIntent, TreeListKey, TreeListState, TreeNode, TreeOp,
};
use crate::app::App;
use crate::components::virtual_list::model::{reveal, visible_rows};
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode, NodeState, Transition, TransitionPath};
use crate::layer;
use crate::own::{Callback, ComponentId, View};
use crate::runloop;

counter!(TREE_OWNER, Owner, "TreeList");
counter!(TREE_OUTLINE, Class, "ShellioTreeOutline");
counter!(TREE_SOURCE, Class, "ShellioTreeSource");
counter!(TREE_ITEM, Class, "ShellioTreeItem");
counter!(TREE_ROW, Class, "ShellioTreeRow");
counter!(TREE_CELL, Class, "ShellioTreeCell");
counter!(TREE_DISCLOSURE, Class, "ShellioTreeDisclosure");

/// Идентификаторы переиспользования вида строки и вида ячейки.
const ROW_ID: &str = "shellio.tree.row";
const CELL_ID: &str = "shellio.tree.cell";
const COLUMN_ID: &str = "shellio.tree.column";
/// Сдвиг уровня: `.tree.l1` прототипа сдвинут на 16 точек от корня.
const INDENT: f64 = 16.0;
/// Сторона области треугольника и сторона самого треугольника.
const DISCLOSURE: f64 = 16.0;
const TRIANGLE: f64 = 7.0;
/// Отступ текста от треугольника и правый отступ строки.
const TEXT_GAP: f64 = 2.0;
const TEXT_INSET: f64 = 6.0;

/// Видимая строка дерева в `ui.inspect`: значения читаются у `NSOutlineView`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRowFacts {
    pub index: usize,
    pub id: String,
    pub text: String,
    pub level: usize,
    pub expandable: bool,
    pub expanded: bool,
}

/// Состояние дерева для `ui.inspect`. Выбор и раскрытие читаются у `NSOutlineView`.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeListFacts {
    pub label: String,
    /// Строк у `NSOutlineView`.
    pub count: usize,
    /// Узлов во всём дереве.
    pub nodes: usize,
    /// Узел выбранной строки.
    pub selected: Option<String>,
    /// Раскрытые узлы в прямом порядке, включая прокрученные за край.
    pub expanded: Vec<String>,
    /// Смещение прокрутки в точках и его предел.
    pub offset: f64,
    pub max_offset: f64,
    /// Дерево — first responder окна.
    pub keyboard_focus: bool,
    /// Созданные виды строк и ячеек за жизнь дерева.
    pub row_views_created: u64,
    pub cell_views_created: u64,
    /// Видимые строки по порядку.
    pub visible: Vec<TreeRowFacts>,
}

/// Rust-часть дерева. Виды и источник читают её через слабую ссылку.
struct TreeModel {
    id: ComponentId,
    /// Общие цвета: виды строк и ячеек читают их при отрисовке и выдаче.
    theme: Cell<AppTheme>,
    /// Растёт со сменой ячейки: вид строки с прежней эпохой перекрашивается при выдаче.
    epoch: Cell<u64>,
    /// Показанное состояние; `None` — дерево ещё не наполнялось.
    shown: RefCell<Option<TreeListState>>,
    /// Объекты корней; детей держат родители.
    roots: RefCell<Vec<Retained<TreeItem>>>,
    /// Объекты всех узлов по id: один объект на id на всю жизнь дерева.
    items: RefCell<HashMap<String, Retained<TreeItem>>>,
    row_views: Cell<u64>,
    cell_views: Cell<u64>,
    on_intent: Box<dyn Fn(TreeListIntent)>,
}

impl TreeModel {
    fn item(&self, id: &str) -> Option<Retained<TreeItem>> {
        self.items.borrow().get(id).cloned()
    }

    fn key(&self, key: TreeListKey) {
        let intent = self
            .shown
            .borrow()
            .as_ref()
            .and_then(|state| model::key_intent(state, key));
        if let Some(intent) = intent {
            (self.on_intent)(intent);
        }
    }

    /// Объекты узлов приводятся к `roots`: узел с прежним id остаётся тем же объектом, новый
    /// создаётся. Возвращает прежние объекты по id: они живут, пока вид не применит операции.
    fn graft(
        &self,
        mtm: MainThreadMarker,
        roots: &[TreeNode],
    ) -> HashMap<String, Retained<TreeItem>> {
        let old = self.items.take();
        let mut items = HashMap::with_capacity(old.len());
        let grafted = roots
            .iter()
            .map(|node| graft_node(mtm, node, &old, &mut items))
            .collect();
        self.roots.replace(grafted);
        self.items.replace(items);
        old
    }
}

fn graft_node(
    mtm: MainThreadMarker,
    node: &TreeNode,
    old: &HashMap<String, Retained<TreeItem>>,
    items: &mut HashMap<String, Retained<TreeItem>>,
) -> Retained<TreeItem> {
    let item = old
        .get(&node.id)
        .cloned()
        .unwrap_or_else(|| TreeItem::new(mtm, &node.id));
    if *item.ivars().text.borrow() != node.text {
        item.ivars().text.replace(node.text.clone());
    }
    let children = node
        .children
        .iter()
        .map(|child| graft_node(mtm, child, old, items))
        .collect();
    item.ivars().children.replace(children);
    items.insert(node.id.clone(), item.clone());
    item
}

/// Дерево на главном потоке. Освобождение снимает источник и делегата и убирает вид из иерархии.
pub struct TreeList {
    view: View<NSScrollView, TreeModel>,
    outline: Retained<TreeOutline>,
}

impl TreeList {
    /// Дерево с состоянием `state`. `on_intent` вызывается на главном потоке изнутри AppKit и не
    /// должен синхронно менять дерево.
    pub fn new(
        app: &App,
        theme: &AppTheme,
        state: TreeListState,
        on_intent: impl Fn(TreeListIntent) + 'static,
    ) -> Result<TreeList, TreeListError> {
        model::check(&state)?;
        let mtm = app.mtm();
        let model = TreeModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            epoch: Cell::new(0),
            shown: RefCell::new(None),
            roots: RefCell::new(Vec::new()),
            items: RefCell::new(HashMap::new()),
            row_views: Cell::new(0),
            cell_views: Cell::new(0),
            on_intent: Box::new(on_intent),
        };
        let mut outline_slot = None;
        let mut view = View::new(&TREE_OWNER, model, "TreeList.outline", |owner| {
            let outline = TreeOutline::new(mtm, owner, theme);
            let scroll = NSScrollView::new(mtm);
            scroll.setWantsLayer(true);
            scroll.setDrawsBackground(false);
            scroll.setHasVerticalScroller(true);
            scroll.setHasHorizontalScroller(false);
            scroll.setAutohidesScrollers(true);
            scroll.contentView().setDrawsBackground(false);
            scroll.setDocumentView(Some(&outline));
            outline_slot = Some(outline);
            scroll
        });
        let Some(outline) = outline_slot else {
            unreachable!("View::new вызывает построение вида ровно один раз");
        };
        let source = TreeSource::new(mtm, view.callback("TreeList.source"));
        // SAFETY: источник реализует методы протоколов с их сигнатурами; его держит владелец до
        // снятия в шаге отвязки.
        unsafe {
            outline.setDataSource(Some(ProtocolObject::from_ref(&*source)));
            outline.setDelegate(Some(ProtocolObject::from_ref(&*source)));
        }
        let detached = outline.clone();
        view.on_detach(move || {
            // SAFETY: снятие источника и делегата до их освобождения.
            unsafe {
                detached.setDelegate(None);
                detached.setDataSource(None);
            }
            drop(source);
        });
        let tree = TreeList { view, outline };
        tree.set_state(state)?;
        Ok(tree)
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn state(&self) -> TreeListState {
        self.view.rust().shown.borrow().clone().unwrap_or_default()
    }

    /// Новое состояние: свёртка по прежним узлам, затем узлы приводятся к новому дереву,
    /// перезагружаются изменённые дети, раскрытие и выбор восстанавливаются по id.
    pub fn set_state(&self, state: TreeListState) -> Result<(), TreeListError> {
        model::check(&state)?;
        let model = self.view.rust();
        let ops = model::diff(model.shown.borrow().as_ref(), &state);
        let outline: &NSOutlineView = &self.outline;
        let mtm = self.outline.mtm();
        // Те же корни — те же объекты узлов: смена раскрытия или выбора их не перестраивает.
        let regraft = !model
            .shown
            .borrow()
            .as_ref()
            .is_some_and(|shown| Rc::ptr_eq(&shown.roots, &state.roots));
        let mut previous = None;
        let mut disclosures = false;
        for op in ops {
            if regraft
                && previous.is_none()
                && !matches!(op, TreeOp::Label(_) | TreeOp::Collapse(_))
            {
                previous = Some(model.graft(mtm, &state.roots));
            }
            match op {
                TreeOp::Label(label) => {
                    outline.setAccessibilityLabel(Some(&NSString::from_str(&label)));
                }
                TreeOp::Fill => outline.reloadData(),
                TreeOp::Collapse(ids) => {
                    for id in ids {
                        let Some(item) = model.item(&id) else {
                            continue;
                        };
                        // SAFETY: узел — объект источника, живой до конца применения операций.
                        unsafe {
                            if outline.isItemExpanded(Some(&item)) {
                                outline.collapseItem_collapseChildren(Some(&item), true);
                            }
                        }
                    }
                    disclosures = true;
                }
                TreeOp::Reload(keys) => {
                    for key in keys {
                        let item = key.as_deref().and_then(|id| model.item(id));
                        if key.is_some() && item.is_none() {
                            continue;
                        }
                        let item: Option<&AnyObject> = item.as_deref().map(|item| &***item);
                        // SAFETY: узел — объект источника, `None` — корень.
                        unsafe { outline.reloadItem_reloadChildren(item, true) };
                    }
                    disclosures = true;
                }
                TreeOp::Text(ids) => {
                    for id in ids {
                        if let Some(item) = model.item(&id) {
                            // SAFETY: узел — объект источника.
                            unsafe { outline.reloadItem(Some(&item)) };
                        }
                    }
                }
                TreeOp::Expand(ids) => {
                    for id in ids {
                        let Some(item) = model.item(&id) else {
                            continue;
                        };
                        // SAFETY: узел — объект источника; родитель раскрыт раньше ребёнка.
                        unsafe {
                            if !outline.isItemExpanded(Some(&item)) {
                                outline.expandItem(Some(&item));
                            }
                        }
                    }
                    disclosures = true;
                }
                TreeOp::Select { id, reveal } => {
                    let row = id
                        .as_deref()
                        .and_then(|id| model.item(id))
                        // SAFETY: узел — объект источника.
                        .map(|item| unsafe { outline.rowForItem(Some(&item)) })
                        .filter(|row| *row >= 0);
                    match row {
                        Some(row) => {
                            let rows = NSIndexSet::indexSetWithIndex(row.unsigned_abs());
                            outline.selectRowIndexes_byExtendingSelection(&rows, false);
                            if reveal {
                                self.reveal(row.unsigned_abs());
                            }
                        }
                        // SAFETY: `deselectAll:` без отправителя на главном потоке.
                        None => unsafe { outline.deselectAll(None) },
                    }
                }
            }
        }
        if regraft && previous.is_none() {
            previous = Some(model.graft(mtm, &state.roots));
        }
        model.shown.replace(Some(state));
        if disclosures {
            self.refresh_disclosures();
        }
        // Прежние узлы больше не нужны виду: удалённые из дерева освобождаются здесь.
        drop(previous);
        Ok(())
    }

    /// Новая ячейка темы: видимые и подготовленные виды перекрашиваются, виды из очереди
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
        self.outline.enumerateAvailableRowViewsUsingBlock(&block);
    }

    /// Треугольники видимых и подготовленных строк приводятся к раскрытию `NSOutlineView`.
    fn refresh_disclosures(&self) {
        let outline = self.outline.clone();
        let block = RcBlock::new(move |row: NonNull<NSTableRowView>, index: NSInteger| {
            // SAFETY: AppKit передаёт живой вид строки на время вызова блока.
            let row = unsafe { row.as_ref() };
            let expanded = outline
                .itemAtRow(index)
                // SAFETY: узел, который дерево само отдало для этой строки.
                .is_some_and(|item| unsafe { outline.isItemExpanded(Some(&item)) });
            for view in row.subviews().iter() {
                if let Some(cell) = view.downcast_ref::<TreeCell>() {
                    cell.set_expanded(expanded);
                }
            }
        });
        self.outline.enumerateAvailableRowViewsUsingBlock(&block);
    }

    /// Отдаёт дереву клавиатурный фокус: Up, Down, Left и Right меняют выбор и раскрытие.
    pub fn focus(&self) -> bool {
        let outline: &NSView = &self.outline;
        outline
            .window()
            .is_some_and(|window| window.makeFirstResponder(Some(outline)))
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    /// Созданные виды строк за жизнь дерева.
    pub fn row_views_created(&self) -> u64 {
        self.view.rust().row_views.get()
    }

    /// Дерево и его видимые строки для `ui.inspect`: дерево (`<id>`) с ролью и именем и кадром
    /// видимой части, строки (`<id>.row.<id узла>`) с выбором, раскрытием и текстом ячейки.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        let scroll = self.view.view();
        let outline: &NSView = &self.outline;
        let facts = self.facts();
        let rows: Vec<TreeRowFacts> = facts.visible.clone();
        let mut tree = inspect::node(
            id,
            "TreeList",
            parent,
            outline,
            None,
            Some(NodeState::TreeList(Box::new(facts))),
        );
        tree.facts.frame = inspect::frame_of(scroll.convertRect_toView(scroll.bounds(), None));
        let mut nodes = vec![tree];
        for row in rows {
            let Ok(index) = NSInteger::try_from(row.index) else {
                break;
            };
            let Some(row_view) = self.outline.rowViewAtRow_makeIfNecessary(index, false) else {
                continue;
            };
            let mut node = inspect::node(
                &format!("{id}.row.{}", row.id),
                "TreeRow",
                Some(id),
                &row_view,
                None,
                None,
            );
            let shown = row_view.visibleRect();
            node.facts.frame = inspect::frame_of(row_view.convertRect_toView(shown, None));
            // Внешний обход видит строку элементом AXRow дерева: имя строки у него — текст ячейки.
            node.facts.role = Some("AXRow".to_owned());
            node.facts.selected = Some(row_view.isSelected());
            node.facts.expanded = row.expandable.then_some(row.expanded);
            node.facts.label = Some(row.text);
            nodes.push(node);
        }
        nodes
    }

    fn facts(&self) -> TreeListFacts {
        let model = self.view.rust();
        let outline: &NSOutlineView = &self.outline;
        let clip = self.view.view().contentView();
        let bounds = clip.bounds();
        let count = usize::try_from(outline.numberOfRows()).unwrap_or(0);
        let content = count as f64 * outline.rowHeight();
        let selected = usize::try_from(outline.selectedRow())
            .ok()
            .and_then(|row| self.item_at(row))
            .map(|item| item.ivars().id.clone());
        let mut expanded = Vec::new();
        let roots = model.roots.borrow().clone();
        let mut stack: Vec<Retained<TreeItem>> = roots.into_iter().rev().collect();
        while let Some(item) = stack.pop() {
            let children = item.ivars().children.borrow().clone();
            // SAFETY: узел — объект источника.
            if !children.is_empty() && unsafe { outline.isItemExpanded(Some(&item)) } {
                expanded.push(item.ivars().id.clone());
                stack.extend(children.into_iter().rev());
            }
        }
        let label = model
            .shown
            .borrow()
            .as_ref()
            .map(|state| state.label.clone())
            .unwrap_or_default();
        TreeListFacts {
            label,
            count,
            nodes: model.items.borrow().len(),
            selected,
            expanded,
            offset: bounds.origin.y,
            max_offset: (content - bounds.size.height).max(0.0),
            keyboard_focus: self.outline.is_key_focus(),
            row_views_created: model.row_views.get(),
            cell_views_created: model.cell_views.get(),
            visible: self
                .visible_range(count)
                .filter_map(|index| self.row_facts(index))
                .collect(),
        }
    }

    fn item_at(&self, row: usize) -> Option<Retained<TreeItem>> {
        let row = NSInteger::try_from(row).ok()?;
        self.outline
            .itemAtRow(row)
            .and_then(|item| item.downcast::<TreeItem>().ok())
    }

    fn row_facts(&self, index: usize) -> Option<TreeRowFacts> {
        let item = self.item_at(index)?;
        let row = NSInteger::try_from(index).ok()?;
        let outline: &NSOutlineView = &self.outline;
        // Текст — то, что показывает ячейка строки: чтение доказывает, что вид получил новый текст.
        let shown = self
            .outline
            .rowViewAtRow_makeIfNecessary(row, false)
            .and_then(|row_view| {
                row_view.subviews().iter().find_map(|view| {
                    view.downcast_ref::<TreeCell>()
                        .map(|cell| cell.ivars().label.stringValue().to_string())
                })
            });
        Some(TreeRowFacts {
            index,
            id: item.ivars().id.clone(),
            text: shown.unwrap_or_else(|| item.ivars().text.borrow().clone()),
            level: usize::try_from(outline.levelForRow(row)).unwrap_or(0),
            // SAFETY: узел, который дерево само отдало для этой строки.
            expandable: unsafe { outline.isExpandable(Some(&item)) },
            // SAFETY: то же.
            expanded: unsafe { outline.isItemExpanded(Some(&item)) },
        })
    }

    fn visible_range(&self, count: usize) -> Range<usize> {
        let clip = self.view.view().contentView();
        let bounds = clip.bounds();
        visible_rows(
            self.outline.rowHeight(),
            bounds.origin.y,
            bounds.size.height,
            count,
        )
    }

    fn reveal(&self, index: usize) {
        let scroll = self.view.view();
        let clip = scroll.contentView();
        let bounds = clip.bounds();
        if let Some(offset) = reveal(
            index,
            self.outline.rowHeight(),
            bounds.origin.y,
            bounds.size.height,
        ) {
            clip.scrollToPoint(NSPoint::new(0.0, offset));
            scroll.reflectScrolledClipView(&clip);
        }
    }
}

/// Вид строки получает новые цвета: перерисовка заливки, текст и треугольник ячейки.
fn repaint_row(row: &NSTableRowView, theme: &AppTheme, epoch: u64) {
    if let Some(row) = row.downcast_ref::<TreeRow>() {
        row.ivars().epoch.set(epoch);
    }
    row.setNeedsDisplay(true);
    for view in row.subviews().iter() {
        if let Some(cell) = view.downcast_ref::<TreeCell>() {
            cell.paint(theme);
        }
    }
}

struct ItemIvars {
    _live: Live,
    id: String,
    text: RefCell<String>,
    /// Дети узла: их держит родитель, ссылки на родителя нет.
    children: RefCell<Vec<Retained<TreeItem>>>,
}

define_class!(
    // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTreeItem"]
    #[ivars = ItemIvars]
    struct TreeItem;
);

impl TreeItem {
    fn new(mtm: MainThreadMarker, id: &str) -> Retained<TreeItem> {
        let this = TreeItem::alloc(mtm).set_ivars(ItemIvars {
            _live: TREE_ITEM.live(),
            id: id.to_owned(),
            text: RefCell::new(String::new()),
            children: RefCell::new(Vec::new()),
        });
        // SAFETY: `init` у NSObject, ivars уже заданы.
        unsafe { msg_send![super(this), init] }
    }
}

fn tree_item(item: Option<&AnyObject>) -> Option<&TreeItem> {
    item?.downcast_ref::<TreeItem>()
}

struct OutlineIvars {
    _live: Live,
    owner: Callback<TreeModel>,
}

define_class!(
    // SAFETY: NSOutlineView не требует от подкласса ничего сверх переопределённых ниже методов,
    // `Drop` у класса нет.
    #[unsafe(super(NSOutlineView, NSTableView, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTreeOutline"]
    #[ivars = OutlineIvars]
    struct TreeOutline;

    impl TreeOutline {
        // SAFETY: селектор и аргумент — как у `keyDown:` NSResponder.
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let Some(key) = tree_key(event) else {
                // SAFETY: `keyDown:` у NSOutlineView с тем же событием.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
                return;
            };
            self.ivars().owner.call(|model| model.key(key));
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. Щелчок по подписи строки
        // сам дерево first responder не делает (P0.2, находка 4).
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                window.makeFirstResponder(Some(self));
            }
            // SAFETY: `mouseDown:` у NSOutlineView с тем же событием.
            let _: () = unsafe { msg_send![super(self), mouseDown: event] };
        }

        // SAFETY: селектор, аргумент и результат — как у `frameOfOutlineCellAtRow:` NSOutlineView.
        // Пустой кадр скрывает системный треугольник раскрытия: его рисует ячейка токеном.
        #[unsafe(method(frameOfOutlineCellAtRow:))]
        fn frame_of_outline_cell(&self, _row: NSInteger) -> NSRect {
            NSRect::ZERO
        }
    }
);

impl TreeOutline {
    fn new(mtm: MainThreadMarker, owner: Callback<TreeModel>, theme: &AppTheme) -> Retained<Self> {
        let this = TreeOutline::alloc(mtm).set_ivars(OutlineIvars {
            _live: TREE_OUTLINE.live(),
            owner,
        });
        // SAFETY: назначенный инициализатор NSOutlineView, ivars уже заданы.
        let outline: Retained<TreeOutline> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        let column = NSTableColumn::initWithIdentifier(
            NSTableColumn::alloc(mtm),
            &NSString::from_str(COLUMN_ID),
        );
        column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
        outline.addTableColumn(&column);
        // SAFETY: колонка добавлена в это дерево и живёт вместе с ним.
        unsafe { outline.setOutlineTableColumn(Some(&column)) };
        outline.setHeaderView(None);
        outline.setStyle(NSTableViewStyle::Plain);
        outline.setRowHeight(theme.metrics.row);
        outline.setIntercellSpacing(NSSize::new(0.0, 0.0));
        outline.setIndentationPerLevel(INDENT);
        outline.setIndentationMarkerFollowsCell(false);
        outline.setAutoresizesOutlineColumn(false);
        outline.setBackgroundColor(&NSColor::clearColor());
        outline.setGridStyleMask(NSTableViewGridLineStyle::GridNone);
        outline.setSelectionHighlightStyle(NSTableViewSelectionHighlightStyle::Regular);
        outline.setFocusRingType(NSFocusRingType::None);
        outline.setAllowsEmptySelection(true);
        outline.setAllowsMultipleSelection(false);
        outline.setUsesAlternatingRowBackgroundColors(false);
        outline
    }

    fn is_key_focus(&self) -> bool {
        self.window()
            .and_then(|window| window.firstResponder())
            .is_some_and(|responder| {
                std::ptr::eq(
                    std::ptr::from_ref::<NSResponder>(&responder).cast::<u8>(),
                    std::ptr::from_ref::<TreeOutline>(self).cast::<u8>(),
                )
            })
    }
}

fn tree_key(event: &NSEvent) -> Option<TreeListKey> {
    match event.keyCode() {
        126 => Some(TreeListKey::Up),
        125 => Some(TreeListKey::Down),
        123 => Some(TreeListKey::Left),
        124 => Some(TreeListKey::Right),
        36 | 76 => Some(TreeListKey::Enter),
        _ => None,
    }
}

struct SourceIvars {
    _live: Live,
    owner: Callback<TreeModel>,
}

define_class!(
    // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTreeSource"]
    #[ivars = SourceIvars]
    struct TreeSource;

    // SAFETY: протокол NSObject реализует суперкласс NSObject.
    unsafe impl NSObjectProtocol for TreeSource {}

    // SAFETY: методы ниже совпадают с объявлениями протокола.
    unsafe impl NSOutlineViewDataSource for TreeSource {
        // SAFETY: селектор, аргументы и результат — как у `outlineView:numberOfChildrenOfItem:`.
        #[unsafe(method(outlineView:numberOfChildrenOfItem:))]
        fn number_of_children(
            &self,
            _outline: &NSOutlineView,
            item: Option<&AnyObject>,
        ) -> NSInteger {
            let count = match item {
                None => self
                    .ivars()
                    .owner
                    .call(|model| model.roots.borrow().len())
                    .unwrap_or(0),
                Some(_) => tree_item(item).map_or(0, |item| item.ivars().children.borrow().len()),
            };
            NSInteger::try_from(count).unwrap_or(NSInteger::MAX)
        }

        // SAFETY: селектор, аргументы и результат — как у `outlineView:child:ofItem:`.
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

        // SAFETY: селектор, аргументы и результат — как у `outlineView:isItemExpandable:`.
        #[unsafe(method(outlineView:isItemExpandable:))]
        fn is_expandable(&self, _outline: &NSOutlineView, item: &AnyObject) -> bool {
            tree_item(Some(item)).is_some_and(|item| !item.ivars().children.borrow().is_empty())
        }
    }

    // SAFETY: протокол требует только необязательные методы.
    unsafe impl NSControlTextEditingDelegate for TreeSource {}

    // SAFETY: методы ниже совпадают с объявлениями протокола.
    unsafe impl NSOutlineViewDelegate for TreeSource {
        // SAFETY: селектор, аргументы и результат — как у `outlineView:viewForTableColumn:item:`.
        #[unsafe(method_id(outlineView:viewForTableColumn:item:))]
        fn view_for(
            &self,
            outline: &NSOutlineView,
            _column: Option<&NSTableColumn>,
            item: &AnyObject,
        ) -> Option<Retained<NSView>> {
            self.cell_for(outline, item)
        }

        // SAFETY: селектор, аргументы и результат — как у `outlineView:rowViewForItem:`.
        #[unsafe(method_id(outlineView:rowViewForItem:))]
        fn row_view_for(
            &self,
            outline: &NSOutlineView,
            _item: &AnyObject,
        ) -> Option<Retained<NSTableRowView>> {
            self.row_for(outline)
        }

        // SAFETY: селектор и аргументы — как у `outlineView:didAddRowView:forRow:`. Вид строки из
        // очереди переиспользования с прежней эпохой цветов перекрашивается.
        #[unsafe(method(outlineView:didAddRowView:forRow:))]
        fn did_add_row_view(
            &self,
            _outline: &NSOutlineView,
            row: &NSTableRowView,
            _index: NSInteger,
        ) {
            self.ivars().owner.call(|model| {
                let stale = row
                    .downcast_ref::<TreeRow>()
                    .is_some_and(|row| row.ivars().epoch.get() != model.epoch.get());
                if stale {
                    repaint_row(row, &model.theme.get(), model.epoch.get());
                }
            });
        }

        // SAFETY: селектор, аргументы и результат — как у `outlineView:shouldSelectItem:`. Выбор
        // принадлежит модели: щелчок отдаёт намерение, дерево меняется по новому состоянию.
        #[unsafe(method(outlineView:shouldSelectItem:))]
        fn should_select_item(&self, _outline: &NSOutlineView, item: &AnyObject) -> bool {
            if let Some(item) = tree_item(Some(item)) {
                let id = item.ivars().id.clone();
                self.ivars()
                    .owner
                    .call(|model| (model.on_intent)(TreeListIntent::Select { id }));
            }
            false
        }
    }
);

impl TreeSource {
    fn new(mtm: MainThreadMarker, owner: Callback<TreeModel>) -> Retained<TreeSource> {
        let this = TreeSource::alloc(mtm).set_ivars(SourceIvars {
            _live: TREE_SOURCE.live(),
            owner,
        });
        // SAFETY: `init` у NSObject, ivars уже заданы.
        unsafe { msg_send![super(this), init] }
    }

    /// Ребёнок `index` узла `item`; `None` у узла — корень.
    fn child_of(&self, index: NSInteger, item: Option<&AnyObject>) -> Option<Retained<TreeItem>> {
        let index = usize::try_from(index).ok()?;
        match item {
            None => self
                .ivars()
                .owner
                .call(|model| model.roots.borrow().get(index).cloned())
                .flatten(),
            Some(_) => {
                tree_item(item).and_then(|item| item.ivars().children.borrow().get(index).cloned())
            }
        }
    }

    /// Вид ячейки узла: из очереди переиспользования или новый.
    fn cell_for(&self, outline: &NSOutlineView, item: &AnyObject) -> Option<Retained<NSView>> {
        let item = tree_item(Some(item))?;
        let owner = self.ivars().owner.clone();
        self.ivars().owner.call(|model| {
            let cell = reuse::<TreeCell>(outline, CELL_ID).unwrap_or_else(|| {
                model.cell_views.set(model.cell_views.get() + 1);
                TreeCell::new(outline.mtm(), owner)
            });
            let ivars = item.ivars();
            // SAFETY: узел, который дерево само отдало для этой строки.
            let expanded = unsafe { outline.isItemExpanded(Some(item)) };
            cell.show(
                &ivars.id,
                &ivars.text.borrow(),
                !ivars.children.borrow().is_empty(),
                expanded,
                &model.theme.get(),
            );
            Retained::into_super(Retained::into_super(cell))
        })
    }

    /// Вид строки: из очереди переиспользования или новый, новый считается.
    fn row_for(&self, outline: &NSOutlineView) -> Option<Retained<NSTableRowView>> {
        self.ivars().owner.call(|model| {
            let row = reuse::<TreeRow>(outline, ROW_ID).unwrap_or_else(|| {
                model.row_views.set(model.row_views.get() + 1);
                TreeRow::new(outline.mtm(), self.ivars().owner.clone(), model.epoch.get())
            });
            Retained::into_super(row)
        })
    }
}

/// Вид из очереди переиспользования дерева по идентификатору.
fn reuse<T: objc2::DowncastTarget>(
    outline: &NSOutlineView,
    identifier: &str,
) -> Option<Retained<T>> {
    // SAFETY: владельца у вида нет (`None`), дерево отдаёт вид из своей очереди или `nil`.
    let view =
        unsafe { outline.makeViewWithIdentifier_owner(&NSString::from_str(identifier), None) };
    view.and_then(|view| view.downcast::<T>().ok())
}

struct RowIvars {
    _live: Live,
    owner: Callback<TreeModel>,
    /// Эпоха цветов, с которой вид последний раз перекрашен.
    epoch: Cell<u64>,
}

define_class!(
    // SAFETY: NSTableRowView не требует от подкласса ничего сверх переопределённых ниже методов,
    // `Drop` у класса нет.
    #[unsafe(super(NSTableRowView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTreeRow"]
    #[ivars = RowIvars]
    struct TreeRow;

    impl TreeRow {
        // SAFETY: селектор и аргумент — как у `drawRect:` NSView.
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

impl TreeRow {
    fn new(mtm: MainThreadMarker, owner: Callback<TreeModel>, epoch: u64) -> Retained<TreeRow> {
        let this = TreeRow::alloc(mtm).set_ivars(RowIvars {
            _live: TREE_ROW.live(),
            owner,
            epoch: Cell::new(epoch),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let row: Retained<TreeRow> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        row.setIdentifier(Some(&NSString::from_str(ROW_ID)));
        row
    }
}

struct CellIvars {
    _live: Live,
    label: Retained<NSTextField>,
    disclosure: Retained<TreeDisclosure>,
}

define_class!(
    // SAFETY: NSTableCellView не требует от подкласса ничего сверх переопределённых ниже методов,
    // `Drop` у класса нет.
    #[unsafe(super(NSTableCellView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTreeCell"]
    #[ivars = CellIvars]
    struct TreeCell;

    impl TreeCell {
        // SAFETY: селектор и результат — как у `layout` NSView.
        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), layout] };
            let bounds = self.bounds();
            let ivars = self.ivars();
            ivars.disclosure.setFrame(NSRect::new(
                NSPoint::new(0.0, ((bounds.size.height - DISCLOSURE) / 2.0).floor()),
                NSSize::new(DISCLOSURE, DISCLOSURE),
            ));
            let height = ivars.label.fittingSize().height.ceil();
            let left = DISCLOSURE + TEXT_GAP;
            ivars.label.setFrame(NSRect::new(
                NSPoint::new(left, ((bounds.size.height - height) / 2.0).floor()),
                NSSize::new((bounds.size.width - left - TEXT_INSET).max(0.0), height),
            ));
        }
    }
);

impl TreeCell {
    fn new(mtm: MainThreadMarker, owner: Callback<TreeModel>) -> Retained<TreeCell> {
        let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        label.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        let disclosure = TreeDisclosure::new(mtm, owner);
        let this = TreeCell::alloc(mtm).set_ivars(CellIvars {
            _live: TREE_CELL.live(),
            label,
            disclosure,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let cell: Retained<TreeCell> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        let ivars = cell.ivars();
        cell.addSubview(&ivars.disclosure);
        cell.addSubview(&ivars.label);
        // SAFETY: подпись — подвид ячейки, ячейка держит её в ivars.
        unsafe { cell.setTextField(Some(&ivars.label)) };
        cell.setIdentifier(Some(&NSString::from_str(CELL_ID)));
        cell
    }

    fn show(&self, id: &str, text: &str, expandable: bool, expanded: bool, theme: &AppTheme) {
        let ivars = self.ivars();
        ivars.label.setStringValue(&NSString::from_str(text));
        let disclosure = ivars.disclosure.ivars();
        disclosure.id.replace(id.to_owned());
        disclosure.expandable.set(expandable);
        disclosure.expanded.set(expanded);
        ivars.disclosure.setNeedsDisplay(true);
        self.paint(theme);
        self.setNeedsLayout(true);
    }

    fn set_expanded(&self, expanded: bool) {
        let disclosure = &self.ivars().disclosure;
        if disclosure.ivars().expanded.replace(expanded) != expanded {
            disclosure.setNeedsDisplay(true);
        }
    }

    fn paint(&self, theme: &AppTheme) {
        let ivars = self.ivars();
        ivars
            .label
            .setTextColor(Some(&layer::ns_color(theme.colors.text)));
        // SAFETY: вес шрифта — неизменяемая константа AppKit.
        ivars.label.setFont(Some(&NSFont::systemFontOfSize_weight(
            theme.metrics.font.body,
            unsafe { NSFontWeightRegular },
        )));
        ivars.disclosure.setNeedsDisplay(true);
    }
}

struct DisclosureIvars {
    _live: Live,
    owner: Callback<TreeModel>,
    /// Узел строки, которую показывает ячейка.
    id: RefCell<String>,
    expandable: Cell<bool>,
    expanded: Cell<bool>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTreeDisclosure"]
    #[ivars = DisclosureIvars]
    struct TreeDisclosure;

    impl TreeDisclosure {
        // SAFETY: селектор и аргумент — как у `drawRect:` NSView. Треугольник вправо у свёрнутого
        // узла и вниз у раскрытого, цвет — токен `text_3`.
        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            let ivars = self.ivars();
            if !ivars.expandable.get() {
                return;
            }
            let Some(theme) = ivars.owner.call(|model| model.theme.get()) else {
                return;
            };
            let bounds = self.bounds();
            let (cx, cy) = (
                bounds.origin.x + bounds.size.width / 2.0,
                bounds.origin.y + bounds.size.height / 2.0,
            );
            let half = TRIANGLE / 2.0;
            let path = NSBezierPath::bezierPath();
            if ivars.expanded.get() {
                path.moveToPoint(NSPoint::new(cx - half, cy + half / 2.0));
                path.lineToPoint(NSPoint::new(cx + half, cy + half / 2.0));
                path.lineToPoint(NSPoint::new(cx, cy - half));
            } else {
                path.moveToPoint(NSPoint::new(cx - half / 2.0, cy - half));
                path.lineToPoint(NSPoint::new(cx - half / 2.0, cy + half));
                path.lineToPoint(NSPoint::new(cx + half, cy));
            }
            path.closePath();
            layer::ns_color(theme.colors.text_3).setFill();
            path.fill();
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. Щелчок по треугольнику
        // отдаёт `toggle` и строку не выбирает.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            let ivars = self.ivars();
            if !ivars.expandable.get() {
                return;
            }
            let intent = TreeListIntent::Toggle {
                id: ivars.id.borrow().clone(),
                expand: !ivars.expanded.get(),
            };
            ivars.owner.call(|model| (model.on_intent)(intent));
        }
    }
);

impl TreeDisclosure {
    fn new(mtm: MainThreadMarker, owner: Callback<TreeModel>) -> Retained<TreeDisclosure> {
        let this = TreeDisclosure::alloc(mtm).set_ivars(DisclosureIvars {
            _live: TREE_DISCLOSURE.live(),
            owner,
            id: RefCell::new(String::new()),
            expandable: Cell::new(false),
            expanded: Cell::new(false),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
    }
}
