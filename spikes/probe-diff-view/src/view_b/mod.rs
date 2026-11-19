//! Вариант B: view-based `NSTableView` без заголовка с одной колонкой в `NSScrollView`. Строка
//! таблицы — строка показа или блоки тредов под ней ([`TableMap`]), её высота — из индекса высот
//! модели, поэтому координаты таблицы совпадают с координатами документа сцены. Ячеек нет: вид
//! строки ([`row::RowView`]) сам рисует фон, курсор, выделение и текст Core Text общим с A
//! модулем [`crate::paint`], системная подсветка выбора выключена. Источник данных и делегат
//! держит владелец и отвязывает в `Drop` (инвариант P0.2).
//!
//! Разница применяется так: `reloadData` — только при замене diff, новый перенос —
//! `noteHeightOfRowsWithIndexesChanged`, смена режима — `noteNumberOfRowsChanged`, треды и
//! черновик — вставка и удаление строк блоков. Изменения высот идут без анимации. Навигацию и
//! прокрутку к якорю ведёт сцена через `scrollToPoint` у clip view, родной навигацией таблицы
//! вид не пользуется (находка P0.2).

pub mod row;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{
    AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class,
    msg_send, sel,
};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityElement, NSAccessibilityFocusedUIElementChangedNotification,
    NSAccessibilityLayoutChangedNotification, NSAccessibilityPostNotification,
    NSAccessibilityRowRole, NSAccessibilitySelectedRowsChangedNotification, NSAnimationContext,
    NSColor, NSControl, NSControlTextEditingDelegate, NSEvent, NSEventModifierFlags,
    NSFocusRingType, NSResponder, NSScrollView, NSTableColumn, NSTableColumnResizingOptions,
    NSTableRowView, NSTableView, NSTableViewAnimationOptions, NSTableViewColumnAutoresizingStyle,
    NSTableViewDataSource, NSTableViewDelegate, NSTableViewGridLineStyle, NSTableViewRowSizeStyle,
    NSTableViewSelectionHighlightStyle, NSTableViewStyle, NSView,
    NSViewBoundsDidChangeNotification, NSViewFrameDidChangeNotification, NSWindowOrderingMode,
};
use objc2_foundation::{
    NSArray, NSIndexSet, NSInteger, NSMutableIndexSet, NSNotificationCenter, NSObject,
    NSObjectProtocol, NSPoint, NSRange, NSRect, NSSize, NSString,
};
use serde::Serialize;

use probe_diff_view::access::{BLOCK_LABEL, RowAx};
use probe_diff_view::keymap::KeyInput;
use probe_diff_view::model::Side;
use probe_diff_view::model::heights::HeightIndex;
use probe_diff_view::tokens::{Theme, ThemeKey};

use self::row::RowView;
use crate::draft_field::DraftField;
use crate::scenario::counters::{Live, counter};
use crate::scene::{Announce, DraftPlace, FontMetrics, Scene, Surface, Visible};
use crate::text::{ColorCache, Font};

counter!(TABLE_B, "ProbeDiffTableB");
counter!(SOURCE_B, "ProbeDiffTableSourceB");

const COLUMN_ID: &str = "probe.diff.b";
/// Записей журнала расхождений высоты таблицы с индексом высот.
const MISMATCH_LOG: usize = 8;

/// Что показывает строка таблицы: текст строки показа или блоки тредов под ней.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Item {
    Text(u32),
    Block(u32),
}

impl Item {
    /// Строка показа, к которой относится строка таблицы.
    pub fn row(self) -> u32 {
        match self {
            Item::Text(row) | Item::Block(row) => row,
        }
    }
}

/// Строки таблицы: текст каждой строки показа и, если у неё есть блоки тредов, одна строка
/// блоков сразу под ней. Все блоки одной строки показа стоят в одной строке таблицы, как в
/// индексе высот, где у строки одна высота блоков.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TableMap {
    rows: u32,
    /// Строки показа с блоками по возрастанию.
    blocks: Vec<u32>,
}

/// Разница строк блоков двух раскладок с одними строками показа: удалённые — номера строк
/// таблицы прежней раскладки, вставленные и оставшиеся — новой.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub removed: Vec<usize>,
    pub inserted: Vec<usize>,
    pub kept: Vec<usize>,
}

impl TableMap {
    #[cfg(test)]
    pub fn new(rows: u32, mut blocks: Vec<u32>) -> TableMap {
        blocks.retain(|&row| row < rows);
        blocks.sort_unstable();
        blocks.dedup();
        TableMap { rows, blocks }
    }

    /// Строки таблицы установленного индекса высот: блок есть у строки ненулевой высоты блоков.
    pub fn from_heights(heights: &HeightIndex) -> TableMap {
        let blocks = (0..heights.len())
            .filter(|&row| heights.block_height(row).is_some_and(|height| height > 0))
            .collect();
        TableMap {
            rows: heights.len(),
            blocks,
        }
    }

    pub fn len(&self) -> usize {
        self.rows as usize + self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn rows(&self) -> u32 {
        self.rows
    }

    pub fn blocks(&self) -> &[u32] {
        &self.blocks
    }

    /// Строка таблицы `j`-го блока.
    fn block_index(&self, j: usize) -> usize {
        self.blocks[j] as usize + j + 1
    }

    /// Что показывает строка таблицы `index`.
    pub fn item(&self, index: usize) -> Option<Item> {
        if index >= self.len() {
            return None;
        }
        let (mut low, mut high) = (0, self.blocks.len());
        while low < high {
            let middle = (low + high) / 2;
            if self.block_index(middle) < index {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if low < self.blocks.len() && self.block_index(low) == index {
            Some(Item::Block(self.blocks[low]))
        } else {
            Some(Item::Text((index - low) as u32))
        }
    }

    /// Строка таблицы `item`.
    pub fn index(&self, item: Item) -> Option<usize> {
        match item {
            Item::Text(row) => (row < self.rows)
                .then(|| row as usize + self.blocks.partition_point(|&block| block < row)),
            Item::Block(row) => self
                .blocks
                .binary_search(&row)
                .ok()
                .map(|j| self.block_index(j)),
        }
    }

    /// Высота строки таблицы по индексу высот.
    pub fn height(item: Item, heights: &HeightIndex) -> Option<u32> {
        match item {
            Item::Text(row) => heights.text_height(row),
            Item::Block(row) => heights.block_height(row),
        }
    }

    /// Верх строки таблицы в координатах документа по индексу высот.
    pub fn top(item: Item, heights: &HeightIndex) -> Option<u64> {
        let top = heights.top(item.row())?;
        match item {
            Item::Text(_) => Some(top),
            Item::Block(row) => Some(top + u64::from(heights.text_height(row)?)),
        }
    }

    /// Разница строк блоков до `next`, если строки показа те же.
    pub fn changes(&self, next: &TableMap) -> Option<Changes> {
        if self.rows != next.rows {
            return None;
        }
        let mut changes = Changes::default();
        for &row in &self.blocks {
            if next.blocks.binary_search(&row).is_err() {
                changes.removed.extend(self.index(Item::Block(row)));
            }
        }
        for &row in &next.blocks {
            let index = next.index(Item::Block(row));
            if self.blocks.binary_search(&row).is_ok() {
                changes.kept.extend(index);
            } else {
                changes.inserted.extend(index);
            }
        }
        Some(changes)
    }
}

/// Счётчики таблицы и видов строк для `result.json`.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct TableStats {
    pub reload_data: u64,
    pub note_heights: u64,
    pub note_rows: u64,
    pub inserted_rows: u64,
    pub removed_rows: u64,
    pub height_calls: u64,
    /// `heightOfRow` при занятой модели: таблица запомнила бы неверную высоту.
    pub height_failures: u64,
    pub row_views_created: u64,
    pub row_views_configured: u64,
    pub draws: u64,
    pub rows_drawn: u64,
    pub busy_draws: u64,
    pub max_draw_ms: f64,
    pub layouts_built: u64,
    /// Раскладки дальше 2 экранов от видимых строк показа.
    pub far_builds: u64,
    /// Подъёмов поля черновика над видами строк.
    pub draft_raises: u64,
    /// Оценка высот строк таблицы выключена.
    pub estimates_off: bool,
    /// Вызовов `heightOfRow` при измерении всех строк после разницы и самое долгое измерение.
    pub measure_calls: u64,
    pub max_measure_ms: f64,
    /// Высота таблицы не совпала с индексом высот после применения разницы.
    pub frame_mismatches: u64,
}

/// Какой diff и какой индекс высот загружены в таблицу.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Loaded {
    /// Поколение снимка diff и файл; `None` — diff не открыт.
    diff: Option<(u64, usize)>,
    /// Установок перестроенного индекса высот.
    installed: u64,
}

/// Общее для таблицы, источника данных и видов строк.
pub struct Shared {
    scene: RefCell<Weak<Scene>>,
    table: RefCell<Option<objc2::rc::Weak<Table>>>,
    map: RefCell<TableMap>,
    loaded: Cell<Loaded>,
    theme: ThemeKey,
    font: Font,
    colors: RefCell<ColorCache>,
    /// Версия раскладок строк: растёт при перезагрузке.
    epoch: Cell<u64>,
    stats: Cell<TableStats>,
    mismatches: RefCell<Vec<String>>,
    /// Одно поле черновика на таблицу, создаётся при первом черновике.
    draft: OnceCell<Retained<DraftField>>,
    /// Элементы доступности видимых строк таблицы по адресу.
    ax_rows: RefCell<HashMap<AxKey, Retained<NSAccessibilityElement>>>,
    /// Изменения доступности до отправки уведомлений.
    ax_pending: Cell<Announce>,
    ax_stats: Cell<AxStatsB>,
}

/// Ключ элемента доступности строки таблицы: адрес строки показа (номера строк сторон) и
/// признак строки блоков под ней.
type AxKey = (bool, (Option<u32>, Option<u32>));

/// Элементы строк и уведомления доступности варианта B.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct AxStatsB {
    /// Запросов строк (`accessibilityVisibleRows` и родственные, фокус).
    pub queries: u64,
    pub created: u64,
    /// Строк таблицы, получивших прежний элемент своего адреса.
    pub reused: u64,
    /// Элементов, отпущенных при уходе строки за экран.
    pub dropped: u64,
    /// Наибольшее число живых элементов и видимых строк таблицы в тот момент.
    pub max_live: usize,
    pub max_visible: usize,
    /// Видов строк, которые после переиспользования показывают не строку таблицы своего индекса.
    pub view_mismatches: u64,
    /// Запросов при занятой модели: строки не выданы.
    pub busy: u64,
    pub posted_focus: u64,
    pub posted_selection: u64,
    pub posted_layout: u64,
}

impl Shared {
    fn scene(&self) -> Option<Rc<Scene>> {
        self.scene.borrow().upgrade()
    }

    fn table(&self) -> Option<Retained<Table>> {
        self.table.borrow().as_ref()?.load()
    }

    fn count(&self, update: impl FnOnce(&mut TableStats)) {
        let mut stats = self.stats.get();
        update(&mut stats);
        self.stats.set(stats);
    }

    fn count_ax(&self, update: impl FnOnce(&mut AxStatsB)) {
        let mut stats = self.ax_stats.get();
        update(&mut stats);
        self.ax_stats.set(stats);
    }

    /// Строки таблицы и загруженное по модели; `None` — модель занята.
    fn current(&self) -> Option<(Loaded, TableMap)> {
        let scene = self.scene()?;
        let model = scene.model().try_borrow().ok()?;
        Some(match model.shown() {
            Some(shown) => (
                Loaded {
                    diff: model
                        .diff
                        .as_ref()
                        .map(|diff| (diff.generation, model.file)),
                    installed: model.relayout.installed,
                },
                TableMap::from_heights(shown.heights),
            ),
            None => (Loaded::default(), TableMap::default()),
        })
    }

    /// Высота строки таблицы по индексу высот. Таблица кэширует ответ, поэтому занятая модель
    /// считается отказом.
    fn height(&self, index: usize) -> f64 {
        self.count(|stats| stats.height_calls += 1);
        let fallback = f64::from(self.font.line_height);
        let Some(item) = self.map.borrow().item(index) else {
            return fallback;
        };
        let height = self.scene().and_then(|scene| {
            let model = scene.model().try_borrow().ok()?;
            TableMap::height(item, model.heights.as_ref()?)
        });
        match height {
            Some(height) if height > 0 => f64::from(height),
            _ => {
                self.count(|stats| stats.height_failures += 1);
                fallback
            }
        }
    }

    /// Поле черновика над видами строк: таблица добавляет виды строк поверх прежних подвидов.
    fn raise_draft(&self) {
        let (Some(field), Some(table)) = (self.draft.get(), self.table()) else {
            return;
        };
        if field.isHidden() {
            return;
        }
        let field_view: &NSView = field;
        let on_top = table
            .subviews()
            .lastObject()
            .is_some_and(|last| std::ptr::eq(&*last, field_view));
        if on_top {
            return;
        }
        let focused = field.is_focused();
        table.addSubview_positioned_relativeTo(field, NSWindowOrderingMode::Above, None);
        if focused && let Some(window) = table.window() {
            let responder: &NSResponder = field;
            window.makeFirstResponder(Some(responder));
        }
        self.count(|stats| stats.draft_raises += 1);
    }
}

pub struct TableIvars {
    _live: Live,
    shared: Rc<Shared>,
}

define_class!(
    /// Таблица варианта B: клавиши и мышь отдаёт сцене, наблюдает прокрутку и размер.
    #[unsafe(super(NSTableView, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeDiffTableB"]
    #[ivars = TableIvars]
    pub struct Table;

    impl Table {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !self.key(event) {
                // SAFETY: обычная передача клавиши таблице.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
            }
        }

        /// Родное отслеживание мыши таблицы выбрало бы строку: щелчок идёт в сцену.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.mouse(event, false);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.mouse(event, true);
        }

        #[unsafe(method(clipBoundsChanged:))]
        fn clip_bounds_changed(&self, _notification: &AnyObject) {
            if let Some(scene) = self.ivars().shared.scene() {
                scene.scrolled();
            }
        }

        #[unsafe(method(scrollFrameChanged:))]
        fn scroll_frame_changed(&self, _notification: &AnyObject) {
            if let Some(scene) = self.ivars().shared.scene() {
                scene.resized();
            }
        }

        /// Отправка накопленных уведомлений доступности: раз за проход цикла.
        #[unsafe(method(axFlush:))]
        fn ax_flush(&self, _argument: Option<&AnyObject>) {
            self.flush_accessibility();
        }

        /// Строки доступности — свои элементы видимых строк таблицы по адресу строки показа, а
        /// не приватные `NSTableRow` AppKit: те не берут подпись, текст и выбранность у вида
        /// строки (прогон `20261119-125516-13324`). Элемент не живёт в виде строки: с ним он
        /// после переиспользования вида описывал бы другую строку (прогон
        /// `20261119-130028-16537`). Все строки таблицы видами не представлены, поэтому и
        /// `AXRows` — только видимые.
        #[unsafe(method_id(accessibilityRows))]
        fn accessibility_rows(&self) -> Option<Retained<NSArray<AnyObject>>> {
            Some(NSArray::from_retained_slice(&self.ax_rows()))
        }

        #[unsafe(method_id(accessibilityVisibleRows))]
        fn accessibility_visible_rows(&self) -> Option<Retained<NSArray<AnyObject>>> {
            let rows = self.ax_rows();
            Some(NSArray::from_retained_slice(&rows))
        }

        /// Строки и поле черновика, если оно видно.
        #[unsafe(method_id(accessibilityChildren))]
        fn accessibility_children(&self) -> Option<Retained<NSArray<AnyObject>>> {
            let mut children = self.ax_rows();
            if let Some(field) = self.ivars().shared.draft.get()
                && !field.isHidden()
            {
                children.push(object(field.clone()));
            }
            Some(NSArray::from_retained_slice(&children))
        }

        #[unsafe(method_id(accessibilitySelectedRows))]
        fn accessibility_selected_rows(&self) -> Option<Retained<NSArray<AnyObject>>> {
            let rows: Vec<Retained<AnyObject>> = self
                .ax_elements()
                .into_iter()
                .filter(|(_, element)| element.isAccessibilitySelected())
                .map(|(_, element)| object(element))
                .collect();
            Some(NSArray::from_retained_slice(&rows))
        }

        /// Фокус доступности — элемент строки курсора, если она видна, иначе таблица.
        #[unsafe(method_id(accessibilityFocusedUIElement))]
        fn accessibility_focused(&self) -> Option<Retained<AnyObject>> {
            Some(self.ax_focused())
        }
    }
);

/// Объект доступности из вида.
fn object<T: Message>(value: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: любой объект Objective-C — `AnyObject`.
    unsafe { Retained::cast_unchecked(value) }
}

/// Подпись роли строки для VoiceOver, как у варианта A.
const ROW_ROLE_DESCRIPTION: &str = "строка diff";

/// Прямоугольник в неперевёрнутых координатах родителя высоты `height`: так AppKit читает
/// `accessibilityFrameInParentSpace`, хотя таблица перевёрнута.
fn unflipped(rect: NSRect, height: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(rect.origin.x, height - rect.origin.y - rect.size.height),
        rect.size,
    )
}

fn new_element(table: &Table) -> Retained<NSAccessibilityElement> {
    let element = NSAccessibilityElement::init(NSAccessibilityElement::alloc());
    // SAFETY: роль — константа AppKit, родитель — живая таблица; элементы держит `Shared`, его
    // держат таблица и её виды строк.
    unsafe {
        element.setAccessibilityRole(Some(NSAccessibilityRowRole));
        element.setAccessibilityParent(Some(table));
    }
    element.setAccessibilityRoleDescription(Some(&NSString::from_str(ROW_ROLE_DESCRIPTION)));
    element
}

/// Атрибуты элемента строки таблицы `index`, которая показывает `item` строки показа с
/// описанием `ax`.
fn update(element: &NSAccessibilityElement, item: Item, ax: &RowAx, index: usize, frame: NSRect) {
    let text_row = matches!(item, Item::Text(_));
    let label = if text_row {
        ax.label()
    } else {
        BLOCK_LABEL.to_owned()
    };
    element.setAccessibilityLabel(Some(&NSString::from_str(&label)));
    let text = text_row.then(|| NSString::from_str(&ax.text));
    // SAFETY: значение строки — NSString или nil.
    unsafe { element.setAccessibilityValue(text.as_deref().map(|text| text as &AnyObject)) };
    element.setAccessibilitySelected(text_row && ax.selected);
    element.setAccessibilityIndex(index as NSInteger);
    element.setAccessibilityFrameInParentSpace(frame);
}

impl Table {
    /// Виды строк видимых строк таблицы по порядку.
    fn row_views(&self) -> Vec<(Retained<RowView>, usize)> {
        let range: NSRange = self.rowsInRect(self.visibleRect());
        (range.location..range.location + range.length)
            .filter_map(|index| {
                let view = self.rowViewAtRow_makeIfNecessary(index as NSInteger, false)?;
                let view = view.downcast::<RowView>().ok()?;
                Some((view, index))
            })
            .collect()
    }

    /// Элементы доступности видимых строк таблицы с их индексами и атрибутами по модели. Элемент
    /// выдаётся по адресу строки показа: пока строка видна, клиент получает тот же элемент,
    /// ушедшая за экран строка свой элемент отпускает, другой адрес элемент не получает. Строку,
    /// которую показывает вид строки после переиспользования, элемент сверяет с картой строк
    /// таблицы по индексу.
    fn ax_elements(&self) -> Vec<(usize, Retained<NSAccessibilityElement>)> {
        let shared = &self.ivars().shared;
        let Some(scene) = shared.scene() else {
            return Vec::new();
        };
        let Ok(model) = scene.model().try_borrow() else {
            shared.count_ax(|stats| stats.busy += 1);
            return Vec::new();
        };
        let views = self.row_views();
        let map = shared.map.borrow();
        let height = self.bounds().size.height;
        let mut elements = shared.ax_rows.borrow_mut();
        let mut kept = HashMap::with_capacity(views.len());
        let mut out = Vec::with_capacity(views.len());
        let (mut created, mut reused, mut mismatches) = (0u64, 0u64, 0u64);
        for (view, index) in &views {
            let Some(item) = map.item(*index) else {
                continue;
            };
            if view.item() != Some(item) {
                mismatches += 1;
            }
            let Some(ax) = model.row_ax(item.row()) else {
                continue;
            };
            let key = (matches!(item, Item::Block(_)), ax.key());
            let element = match elements.remove(&key) {
                Some(element) => {
                    reused += 1;
                    element
                }
                None => {
                    created += 1;
                    new_element(self)
                }
            };
            let frame = unflipped(self.rectOfRow(*index as NSInteger), height);
            update(&element, item, &ax, *index, frame);
            out.push((*index, element.clone()));
            kept.insert(key, element);
        }
        let dropped = elements.len() as u64;
        *elements = kept;
        let live = elements.len();
        drop(elements);
        shared.count_ax(|stats| {
            stats.queries += 1;
            stats.created += created;
            stats.reused += reused;
            stats.dropped += dropped;
            stats.view_mismatches += mismatches;
            if live > stats.max_live {
                stats.max_live = live;
                stats.max_visible = views.len();
            }
        });
        out
    }

    fn ax_rows(&self) -> Vec<Retained<AnyObject>> {
        self.ax_elements()
            .into_iter()
            .map(|(_, element)| object(element))
            .collect()
    }

    /// Элемент строки курсора среди видимых или сама таблица.
    fn ax_focused(&self) -> Retained<AnyObject> {
        let shared = &self.ivars().shared;
        let index = shared
            .scene()
            .and_then(|scene| scene.model().try_borrow().ok()?.cursor_row())
            .and_then(|row| shared.map.borrow().index(Item::Text(row)));
        let element =
            index.and_then(|index| self.ax_elements().into_iter().find(|(at, _)| *at == index));
        match element {
            Some((_, element)) => object(element),
            None => object(self.retain()),
        }
    }

    fn flush_accessibility(&self) {
        let shared = &self.ivars().shared;
        let change = shared.ax_pending.replace(Announce::default());
        if change.is_empty() {
            return;
        }
        // Элементы строятся, только если клиент уже спрашивал строки, как у варианта A.
        let focused = if shared.ax_rows.borrow().is_empty() {
            object(self.retain())
        } else {
            self.ax_focused()
        };
        let target: &AnyObject = self;
        // SAFETY: таблица — объект доступности, имена уведомлений — константы AppKit.
        unsafe {
            if change.layout {
                NSAccessibilityPostNotification(target, NSAccessibilityLayoutChangedNotification);
            }
            if change.selection {
                NSAccessibilityPostNotification(
                    target,
                    NSAccessibilitySelectedRowsChangedNotification,
                );
            }
            if change.focus {
                NSAccessibilityPostNotification(
                    &focused,
                    NSAccessibilityFocusedUIElementChangedNotification,
                );
            }
        }
        shared.count_ax(|stats| {
            stats.posted_layout += u64::from(change.layout);
            stats.posted_selection += u64::from(change.selection);
            stats.posted_focus += u64::from(change.focus);
        });
    }

    fn key(&self, event: &NSEvent) -> bool {
        let Some(scene) = self.ivars().shared.scene() else {
            return false;
        };
        let chars = event
            .charactersIgnoringModifiers()
            .map(|chars| chars.to_string())
            .unwrap_or_default();
        let flags = event.modifierFlags();
        scene.key(&KeyInput {
            chars,
            control: flags.contains(NSEventModifierFlags::Control),
            command: flags.contains(NSEventModifierFlags::Command),
            option: flags.contains(NSEventModifierFlags::Option),
        })
    }

    /// Щелчок или протяжка по таблице или виду строки. Щелчок делает таблицу first responder:
    /// окно отдаёт фокус только виду под точкой (находка P0.2).
    pub fn mouse(&self, event: &NSEvent, dragged: bool) {
        if !dragged && let Some(window) = self.window() {
            let responder: &NSResponder = self;
            window.makeFirstResponder(Some(responder));
        }
        let extend = dragged || event.modifierFlags().contains(NSEventModifierFlags::Shift);
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        if let Some(scene) = self.ivars().shared.scene() {
            scene.click_at(point.x, point.y, extend);
        }
    }
}

pub struct SourceIvars {
    _live: Live,
    shared: Rc<Shared>,
}

define_class!(
    /// Источник данных и делегат таблицы. Таблица держит его слабо.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeDiffTableSourceB"]
    #[ivars = SourceIvars]
    pub struct Source;

    unsafe impl NSObjectProtocol for Source {}

    unsafe impl NSTableViewDataSource for Source {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _table: &NSTableView) -> NSInteger {
            self.ivars().shared.map.borrow().len() as NSInteger
        }
    }

    unsafe impl NSControlTextEditingDelegate for Source {}

    unsafe impl NSTableViewDelegate for Source {
        #[unsafe(method(tableView:heightOfRow:))]
        fn height_of_row(&self, _table: &NSTableView, row: NSInteger) -> f64 {
            self.ivars()
                .shared
                .height(usize::try_from(row).unwrap_or(usize::MAX))
        }

        /// Ячеек нет: строку рисует вид строки.
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for(
            &self,
            _table: &NSTableView,
            _column: Option<&NSTableColumn>,
            _row: NSInteger,
        ) -> Option<Retained<NSView>> {
            None
        }

        #[unsafe(method_id(tableView:rowViewForRow:))]
        fn row_view_for(
            &self,
            table: &NSTableView,
            row: NSInteger,
        ) -> Option<Retained<NSTableRowView>> {
            let shared = &self.ivars().shared;
            let view = RowView::dequeue(self.mtm(), table, shared);
            let item = usize::try_from(row)
                .ok()
                .and_then(|row| shared.map.borrow().item(row));
            view.configure(item);
            view.setNeedsDisplay(true);
            Some(Retained::into_super(view))
        }

        #[unsafe(method(tableView:shouldSelectRow:))]
        fn should_select_row(&self, _table: &NSTableView, _row: NSInteger) -> bool {
            false
        }

        #[unsafe(method(tableView:didAddRowView:forRow:))]
        fn did_add_row_view(&self, _table: &NSTableView, _view: &NSTableRowView, _row: NSInteger) {
            self.ivars().shared.raise_draft();
        }
    }
);

impl Source {
    fn new(mtm: MainThreadMarker, shared: Rc<Shared>) -> Retained<Source> {
        let this = Source::alloc(mtm).set_ivars(SourceIvars {
            _live: SOURCE_B.live(),
            shared,
        });
        // SAFETY: `init` — назначенный инициализатор NSObject.
        unsafe { msg_send![super(this), init] }
    }
}

/// Выключает оценку высот строк. На macOS 27 таблица по умолчанию спрашивает `heightOfRow`
/// только у части строк (около 300) и ставит остальные по средней высоте, и строки таблицы
/// расходятся с индексом высот. Публичного выключателя нет: это приватный
/// `_setEstimatesRowHeights:`, обход AppKit. `false` — выключателя нет или он не сработал.
fn disable_estimates(table: &NSTableView) -> bool {
    if !table.respondsToSelector(sel!(_setEstimatesRowHeights:))
        || !table.respondsToSelector(sel!(_estimatesRowHeights))
    {
        return false;
    }
    // SAFETY: оба селектора есть у таблицы (проверено выше): сеттер с BOOL и геттер BOOL.
    unsafe {
        let _: () = msg_send![table, _setEstimatesRowHeights: false];
        let estimates: bool = msg_send![table, _estimatesRowHeights];
        !estimates
    }
}

/// Изменение высот таблицы без анимации.
fn without_animation(change: impl FnOnce()) {
    NSAnimationContext::beginGrouping();
    NSAnimationContext::currentContext().setDuration(0.0);
    change();
    NSAnimationContext::endGrouping();
}

fn index_set(indexes: &[usize]) -> Retained<NSIndexSet> {
    let set = NSMutableIndexSet::new();
    for &index in indexes {
        set.addIndex(index);
    }
    Retained::into_super(set)
}

/// Вид варианта B для сцены: прокрутка, таблица и её источник данных.
pub struct ViewB {
    mtm: MainThreadMarker,
    scroll: Retained<NSScrollView>,
    table: Retained<Table>,
    /// Таблица держит источник данных слабо: его держит вид.
    _source: Retained<Source>,
    shared: Rc<Shared>,
}

impl ViewB {
    /// Прокрутка во весь `frame` с пустой таблицей. Привязка к сцене — [`ViewB::bind`].
    pub fn new(mtm: MainThreadMarker, frame: NSRect, theme: ThemeKey) -> Rc<ViewB> {
        let shared = Rc::new(Shared {
            scene: RefCell::new(Weak::new()),
            table: RefCell::new(None),
            map: RefCell::new(TableMap::default()),
            loaded: Cell::new(Loaded::default()),
            theme,
            font: Font::monospaced(),
            colors: RefCell::new(ColorCache::default()),
            epoch: Cell::new(1),
            stats: Cell::new(TableStats::default()),
            mismatches: RefCell::new(Vec::new()),
            draft: OnceCell::new(),
            ax_rows: RefCell::new(HashMap::new()),
            ax_pending: Cell::new(Announce::default()),
            ax_stats: Cell::new(AxStatsB::default()),
        });
        let source = Source::new(mtm, Rc::clone(&shared));
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(false);
        scroll.setDrawsBackground(true);
        let [r, g, b, a] = Theme::of(theme).background.components();
        let background = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a);
        scroll.setBackgroundColor(&background);
        let content = scroll.contentSize();
        let this = Table::alloc(mtm).set_ivars(TableIvars {
            _live: TABLE_B.live(),
            shared: Rc::clone(&shared),
        });
        // SAFETY: назначенный инициализатор NSView с прямоугольником.
        let table: Retained<Table> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::new(NSPoint::ZERO, content)] };
        table.setStyle(NSTableViewStyle::Plain);
        table.setHeaderView(None);
        table.setIntercellSpacing(NSSize::new(0.0, 0.0));
        table.setUsesAutomaticRowHeights(false);
        table.setRowSizeStyle(NSTableViewRowSizeStyle::Custom);
        table.setGridStyleMask(NSTableViewGridLineStyle::empty());
        table.setSelectionHighlightStyle(NSTableViewSelectionHighlightStyle::None);
        table.setAllowsTypeSelect(false);
        table.setAllowsEmptySelection(true);
        table.setAllowsMultipleSelection(false);
        table.setAllowsColumnReordering(false);
        table.setAllowsColumnResizing(false);
        table.setUsesAlternatingRowBackgroundColors(false);
        table.setBackgroundColor(&background);
        table.setFocusRingType(NSFocusRingType::None);
        table.setColumnAutoresizingStyle(
            NSTableViewColumnAutoresizingStyle::UniformColumnAutoresizingStyle,
        );
        let column = NSTableColumn::initWithIdentifier(
            NSTableColumn::alloc(mtm),
            &NSString::from_str(COLUMN_ID),
        );
        column.setResizingMask(NSTableColumnResizingOptions::AutoresizingMask);
        column.setWidth(content.width);
        table.addTableColumn(&column);
        // SAFETY: таблица держит источник слабо, владелец держит его сам и отвязывает в `Drop`
        // до освобождения.
        unsafe {
            table.setDataSource(Some(ProtocolObject::from_ref(&*source)));
            table.setDelegate(Some(ProtocolObject::from_ref(&*source)));
        }
        scroll.setDocumentView(Some(&table));
        table.sizeLastColumnToFit();
        // `setDelegate:` снова включает оценку высот: выключатель — после него.
        let estimates_off = disable_estimates(&table);
        shared.count(|stats| stats.estimates_off = estimates_off);
        *shared.table.borrow_mut() = Some(objc2::rc::Weak::from_retained(&table));
        let clip = scroll.contentView();
        clip.setPostsBoundsChangedNotifications(true);
        scroll.setPostsFrameChangedNotifications(true);
        // SAFETY: наблюдатель — таблица, она отвечает на `clipBoundsChanged:` и
        // `scrollFrameChanged:`; снимается в `Drop` до освобождения таблицы.
        unsafe {
            let center = NSNotificationCenter::defaultCenter();
            center.addObserver_selector_name_object(
                &table,
                sel!(clipBoundsChanged:),
                Some(NSViewBoundsDidChangeNotification),
                Some(&clip),
            );
            center.addObserver_selector_name_object(
                &table,
                sel!(scrollFrameChanged:),
                Some(NSViewFrameDidChangeNotification),
                Some(&scroll),
            );
        }
        Rc::new(ViewB {
            mtm,
            scroll,
            table,
            _source: source,
            shared,
        })
    }

    pub fn bind(&self, scene: &Rc<Scene>) {
        *self.shared.scene.borrow_mut() = Rc::downgrade(scene);
    }

    pub fn font(&self) -> FontMetrics {
        FontMetrics {
            char_width: self.shared.font.char_width,
            line_height: self.shared.font.line_height,
        }
    }

    pub fn scroll_view(&self) -> &NSScrollView {
        &self.scroll
    }

    pub fn responder(&self) -> &NSResponder {
        &self.table
    }

    pub fn document_frame(&self) -> NSRect {
        self.table.frame()
    }

    pub fn stats(&self) -> TableStats {
        self.shared.stats.get()
    }

    pub fn ax_stats(&self) -> AxStatsB {
        self.shared.ax_stats.get()
    }

    pub fn mismatches(&self) -> Vec<String> {
        self.shared.mismatches.borrow().clone()
    }

    pub fn map(&self) -> TableMap {
        self.shared.map.borrow().clone()
    }

    pub fn viewport_y(&self) -> f64 {
        self.scroll.contentView().bounds().origin.y
    }

    /// Видов строк, которые таблица держит сейчас, включая подготовленные вне видимой части.
    pub fn available_row_views(&self) -> usize {
        let count = Cell::new(0usize);
        self.each_row_view(|_, _| count.set(count.get() + 1));
        count.get()
    }

    /// Строки таблицы, которые пересекает видимая область.
    pub fn visible_table_rows(&self) -> Range<usize> {
        let range: NSRange = self.table.rowsInRect(self.table.visibleRect());
        range.location..range.location + range.length
    }

    /// Прямоугольник строки таблицы в её координатах.
    pub fn table_rect(&self, index: usize) -> NSRect {
        self.table.rectOfRow(index as NSInteger)
    }

    /// Прямоугольник текста строки показа по таблице.
    pub fn row_rect(&self, row: u32) -> Option<NSRect> {
        let index = self.shared.map.borrow().index(Item::Text(row))?;
        Some(self.table_rect(index))
    }

    /// Прямоугольник половины `side` строки показа в «Рядом».
    pub fn half_rect(&self, row: u32, side: Side) -> Option<NSRect> {
        let geometry = self.shared.scene()?.geometry()?;
        let area = self.row_rect(row)?;
        Some(NSRect::new(
            NSPoint::new(geometry.half_origin(side), area.origin.y),
            NSSize::new(geometry.half_width(), area.size.height),
        ))
    }

    pub fn to_window(&self, area: NSRect) -> NSRect {
        self.table.convertRect_toView(area, None)
    }

    /// Строки таблицы против индекса высот у видимых строк таблицы: верх и высота строки, рамки
    /// видов строк, число строк таблицы. Число проверенных строк и расхождения.
    pub fn geometry(&self) -> (usize, Vec<String>) {
        let Some(scene) = self.shared.scene() else {
            return (0, vec!["нет сцены".to_owned()]);
        };
        let rows = self.visible_table_rows();
        let mut errors = Vec::new();
        {
            let Ok(model) = scene.model().try_borrow() else {
                return (0, vec!["модель занята".to_owned()]);
            };
            let Some(heights) = model.heights.as_ref() else {
                return (0, vec!["нет индекса высот".to_owned()]);
            };
            let map = self.shared.map.borrow();
            let count = self.table.numberOfRows();
            if map.is_empty() {
                errors.push("таблица без строк".to_owned());
            }
            if usize::try_from(count).ok() != Some(map.len()) {
                errors.push(format!("строк таблицы {count}, по индексу {}", map.len()));
            }
            for index in rows.clone() {
                let Some(item) = map.item(index) else {
                    errors.push(format!("строка таблицы {index} вне раскладки"));
                    continue;
                };
                let rect = self.table_rect(index);
                let top = TableMap::top(item, heights).map(|top| top as f64);
                let height = TableMap::height(item, heights).map(f64::from);
                let off = |value: Option<f64>, actual: f64| {
                    value.is_none_or(|value| (value - actual).abs() > 0.5)
                };
                if off(top, rect.origin.y) || off(height, rect.size.height) {
                    errors.push(format!(
                        "{item:?} (строка таблицы {index}): таблица y {} высота {}, индекс {top:?} {height:?}",
                        rect.origin.y, rect.size.height
                    ));
                }
            }
        }
        self.each_row_view(|view, index| {
            let frame = view.frame();
            let rect = self.table_rect(index);
            if (frame.origin.y - rect.origin.y).abs() > 0.5
                || (frame.size.height - rect.size.height).abs() > 0.5
            {
                errors.push(format!(
                    "вид строки {index}: рамка y {} высота {}, строка таблицы y {} высота {}",
                    frame.origin.y, frame.size.height, rect.origin.y, rect.size.height
                ));
            }
        });
        (rows.len(), errors)
    }

    /// Рамка поля черновика, если оно видно поверх видов строк: в центре поля попадание даёт
    /// само поле.
    pub fn draft_shown(&self) -> Option<NSRect> {
        let field = self.shared.draft.get()?;
        if field.isHidden() {
            return None;
        }
        let frame = field.frame();
        let center = NSPoint::new(
            frame.origin.x + frame.size.width / 2.0,
            frame.origin.y + frame.size.height / 2.0,
        );
        // Надвид таблицы — clip view прокрутки: `hitTest:` ждёт точку в его координатах.
        let clip = self.scroll.contentView();
        let point = self.table.convertPoint_toView(center, Some(&clip));
        let hit = self.table.hitTest(point)?;
        let field_view: &NSView = field;
        hit.isDescendantOf(field_view).then_some(frame)
    }

    pub fn draft_focused(&self) -> bool {
        self.shared
            .draft
            .get()
            .is_some_and(|field| field.is_focused())
    }

    pub fn document_focused(&self) -> bool {
        let responder: &NSResponder = &self.table;
        self.table
            .window()
            .and_then(|window| window.firstResponder())
            .is_some_and(|first| std::ptr::eq(&*first, responder))
    }

    fn each_row_view(&self, mut visit: impl FnMut(&RowView, usize)) {
        row::each_row_view(&self.table, |view, index| visit(&view, index));
    }

    /// Виды строк по новым строкам таблицы: что показывают, и перерисовка.
    fn refresh_rows(&self) {
        let map = self.shared.map.borrow().clone();
        self.each_row_view(|view, index| {
            view.configure(map.item(index));
            view.setNeedsDisplay(true);
        });
    }

    /// Высота таблицы после разницы равна индексу высот (или видимой области, если документ
    /// короче). Прямоугольник последней строки заставляет таблицу без оценки высот
    /// ([`disable_estimates`]) измерить все строки сразу. Иначе — запись расхождения.
    fn settle(&self, reason: &str) {
        let rows = self.table.numberOfRows();
        if rows > 0 {
            let start = crate::scenario::sampler::uptime_ns().unwrap_or(0);
            let calls = self.shared.stats.get().height_calls;
            let _ = self.table.rectOfRow(rows - 1);
            self.table.tile();
            let ms = crate::scenario::sampler::uptime_ns()
                .unwrap_or(0)
                .saturating_sub(start) as f64
                / 1e6;
            self.shared.count(|stats| {
                stats.measure_calls += stats.height_calls - calls;
                stats.max_measure_ms = stats.max_measure_ms.max(ms);
            });
        }
        let expected = self
            .shared
            .scene()
            .and_then(|scene| scene.scroll_limit())
            .map_or(0.0, |(total, _)| total as f64);
        let clip = self.scroll.contentView().bounds().size.height;
        let matches = |height: f64| {
            (height - expected).abs() <= 0.5 || (expected < clip && (height - clip).abs() <= 0.5)
        };
        if matches(self.table.frame().size.height) {
            return;
        }
        self.table.tile();
        let height = self.table.frame().size.height;
        if !matches(height) {
            self.shared.count(|stats| stats.frame_mismatches += 1);
            let mut log = self.shared.mismatches.borrow_mut();
            if log.len() < MISMATCH_LOG {
                log.push(format!(
                    "{reason}: высота таблицы {height}, индекс высот {expected}, видимая область {clip}"
                ));
            }
        }
    }
}

impl Surface for ViewB {
    fn reload(&self, _height: f64) {
        let Some((loaded, next)) = self.shared.current() else {
            self.shared.count(|stats| stats.height_failures += 1);
            return;
        };
        let previous = self.shared.map.replace(next.clone());
        let before = self.shared.loaded.replace(loaded);
        let epoch = &self.shared.epoch;
        epoch.set(epoch.get() + 1);
        if loaded.diff != before.diff {
            self.table.reloadData();
            self.shared.count(|stats| stats.reload_data += 1);
        } else {
            // Смена режима меняет и число строк, и высоты: `noteNumberOfRowsChanged` запомненные
            // высоты оставшихся строк не сбрасывает.
            let rows = previous != next;
            let heights = loaded.installed != before.installed;
            if rows {
                without_animation(|| self.table.noteNumberOfRowsChanged());
                self.shared.count(|stats| stats.note_rows += 1);
            }
            if heights {
                let all = NSIndexSet::indexSetWithIndexesInRange(NSRange::new(0, next.len()));
                without_animation(|| self.table.noteHeightOfRowsWithIndexesChanged(&all));
                self.shared.count(|stats| stats.note_heights += 1);
            }
        }
        self.refresh_rows();
        self.settle("reload");
    }

    fn invalidate(&self, rows: &[Range<u32>]) {
        self.each_row_view(|view, _| {
            if let Some(Item::Text(row)) = view.item()
                && rows.iter().any(|range| range.contains(&row))
            {
                view.forget_text();
                view.setNeedsDisplay(true);
            }
        });
    }

    fn scroll_to(&self, y: f64) {
        let clip = self.scroll.contentView();
        clip.scrollToPoint(NSPoint::new(0.0, y));
        self.scroll.reflectScrolledClipView(&clip);
    }

    /// Видимая область — границы clip view: ширина таблицы при смене размера прокрутки может
    /// ещё быть прежней.
    fn viewport(&self) -> Visible {
        let bounds = self.scroll.contentView().bounds();
        Visible {
            y: bounds.origin.y,
            width: bounds.size.width,
            height: bounds.size.height,
        }
    }

    fn heights_changed(&self, _height: f64, _rows: &[Range<u32>]) {
        let Some((loaded, next)) = self.shared.current() else {
            self.shared.count(|stats| stats.height_failures += 1);
            return;
        };
        let previous = self.shared.map.replace(next.clone());
        self.shared.loaded.set(loaded);
        match previous.changes(&next) {
            Some(changes) => {
                without_animation(|| {
                    self.table.beginUpdates();
                    if !changes.removed.is_empty() {
                        self.table.removeRowsAtIndexes_withAnimation(
                            &index_set(&changes.removed),
                            NSTableViewAnimationOptions::empty(),
                        );
                    }
                    if !changes.inserted.is_empty() {
                        self.table.insertRowsAtIndexes_withAnimation(
                            &index_set(&changes.inserted),
                            NSTableViewAnimationOptions::empty(),
                        );
                    }
                    self.table.endUpdates();
                    if !changes.kept.is_empty() {
                        self.table
                            .noteHeightOfRowsWithIndexesChanged(&index_set(&changes.kept));
                    }
                });
                self.shared.count(|stats| {
                    stats.removed_rows += changes.removed.len() as u64;
                    stats.inserted_rows += changes.inserted.len() as u64;
                    stats.note_heights += u64::from(!changes.kept.is_empty());
                });
            }
            None => {
                without_animation(|| self.table.noteNumberOfRowsChanged());
                self.shared.count(|stats| stats.note_rows += 1);
            }
        }
        self.refresh_rows();
        self.settle("heights_changed");
    }

    fn place_draft(&self, place: Option<DraftPlace>) {
        let Some(place) = place else {
            if let Some(field) = self.shared.draft.get() {
                let focused = field.is_focused();
                field.setHidden(true);
                if focused && let Some(window) = self.table.window() {
                    let responder: &NSResponder = &self.table;
                    window.makeFirstResponder(Some(responder));
                }
            }
            return;
        };
        let field = self.shared.draft.get_or_init(|| {
            let field = DraftField::new(
                self.mtm,
                Theme::of(self.shared.theme),
                crate::text::FONT_NAME,
                crate::text::FONT_SIZE,
            );
            if let Some(scene) = self.shared.scene() {
                field.bind(&scene);
            }
            self.table.addSubview(&field);
            field
        });
        field.setFrame(NSRect::new(
            NSPoint::new(place.x, place.y),
            NSSize::new(place.width, place.height),
        ));
        if let Some(text) = place.text {
            field.setString(&NSString::from_str(&text));
        }
        field.setHidden(false);
        self.shared.raise_draft();
        if place.focus
            && let Some(window) = self.table.window()
        {
            let responder: &NSResponder = field;
            window.makeFirstResponder(Some(responder));
        }
    }

    fn announce(&self, change: Announce) {
        let before = self.shared.ax_pending.get();
        self.shared.ax_pending.set(before.merge(change));
        if before.is_empty() {
            // SAFETY: таблица отвечает на `axFlush:`; отложенный вызов держит её до вызова и
            // снимается в `Drop`.
            unsafe {
                let _: () = msg_send![
                    &*self.table,
                    performSelector: sel!(axFlush:),
                    withObject: None::<&AnyObject>,
                    afterDelay: 0.0f64
                ];
            }
        }
    }
}

impl Drop for ViewB {
    fn drop(&mut self) {
        // SAFETY: снимаются наблюдатель, поставленный в `new`, и отложенная отправка уведомлений,
        // источник данных отвязывается до его освобождения.
        unsafe {
            NSNotificationCenter::defaultCenter().removeObserver(&self.table);
            let _: () = msg_send![
                NSObject::class(),
                cancelPreviousPerformRequestsWithTarget: &*self.table
            ];
            self.table.setDataSource(None);
            self.table.setDelegate(None);
        }
        *self.shared.scene.borrow_mut() = Weak::new();
        if let Some(field) = self.shared.draft.get() {
            field.unbind();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Строки таблицы по порядку, как их видит таблица.
    fn items(map: &TableMap) -> Vec<Item> {
        (0..map.len()).filter_map(|index| map.item(index)).collect()
    }

    /// Эталон: текст каждой строки показа и блок под ней, если он есть.
    fn reference(rows: u32, blocks: &[u32]) -> Vec<Item> {
        let mut items = Vec::new();
        for row in 0..rows {
            items.push(Item::Text(row));
            if blocks.contains(&row) {
                items.push(Item::Block(row));
            }
        }
        items
    }

    #[test]
    fn table_rows_follow_display_rows_and_blocks() {
        let map = TableMap::new(8, vec![5, 0, 7, 5, 9]);
        assert_eq!(map.blocks(), &[0, 5, 7]);
        assert_eq!(map.len(), 11);
        assert_eq!(items(&map), reference(8, &[0, 5, 7]));
        for (index, item) in items(&map).into_iter().enumerate() {
            assert_eq!(map.index(item), Some(index), "{item:?}");
        }
        assert_eq!(map.item(11), None);
        assert_eq!(map.index(Item::Text(8)), None);
        assert_eq!(map.index(Item::Block(1)), None);
        assert!(TableMap::default().is_empty());
        assert_eq!(TableMap::default().item(0), None);
    }

    #[test]
    fn table_rows_match_a_pseudo_random_layout() {
        let mut seed = 0x9e37_79b9_u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..50 {
            let rows = next() % 400 + 1;
            let blocks: Vec<u32> = (0..rows).filter(|_| next() % 7 == 0).collect();
            let map = TableMap::new(rows, blocks.clone());
            assert_eq!(items(&map), reference(rows, &blocks));
        }
    }

    #[test]
    fn table_heights_sum_to_the_height_index() {
        let text = vec![14, 28, 14, 42, 14, 14];
        let block = vec![0, 30, 0, 0, 0, 50];
        let heights = HeightIndex::from_heights(text, block);
        let map = TableMap::from_heights(&heights);
        assert_eq!(map.blocks(), &[1, 5]);
        let mut y = 0u64;
        for index in 0..map.len() {
            let item = map.item(index).unwrap();
            assert_eq!(TableMap::top(item, &heights), Some(y), "{item:?}");
            y += u64::from(TableMap::height(item, &heights).unwrap());
        }
        assert_eq!(y, heights.total());
    }

    /// Применяет разницу как таблица: удаление по прежним номерам, вставка по новым.
    fn apply(before: &TableMap, changes: &Changes, after: &TableMap) -> Vec<Item> {
        let mut rows = items(before);
        for &index in changes.removed.iter().rev() {
            rows.remove(index);
        }
        for &index in &changes.inserted {
            rows.insert(index, after.item(index).unwrap());
        }
        rows
    }

    #[test]
    fn block_changes_turn_one_table_into_the_other() {
        let before = TableMap::new(10, vec![1, 4, 8]);
        let after = TableMap::new(10, vec![0, 4, 6, 9]);
        let changes = before.changes(&after).unwrap();
        assert_eq!(changes.removed, vec![2, 11]);
        assert_eq!(changes.inserted, vec![1, 9, 13]);
        assert_eq!(changes.kept, vec![6]);
        assert_eq!(apply(&before, &changes, &after), items(&after));
        let same = before.changes(&before).unwrap();
        assert!(same.removed.is_empty() && same.inserted.is_empty());
        assert_eq!(same.kept, vec![2, 6, 11]);
        assert_eq!(before.changes(&TableMap::new(11, vec![])), None);
    }
}
