//! Вариант A: один `NSView` — document view в `NSScrollView`, высота документа из индекса высот
//! модели. `drawRect:` рисует только строки в грязном прямоугольнике, обрезанном до видимой
//! области с запасом в экран: фон добавления и удаления, гуттер с номерами, курсор и выделение,
//! текст Core Text по визуальным строкам модели, блоки тредов. Строку и блоки рисует общий с
//! вариантом B модуль [`crate::paint`], раскладка живёт в ограниченном кэше вида
//! ([`layout_cache`]). Доменного состояния вид не хранит: он читает модель сцены, клавиши и мышь
//! отдаёт её обработчикам, прокрутку к якорю ведёт `scrollToPoint` у clip view.

pub mod ax;
pub mod layout_cache;

use std::cell::{Cell, OnceCell, RefCell};
use std::ops::Range;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel,
};
use objc2_app_kit::{
    NSAccessibilityTableRole, NSColor, NSEvent, NSEventModifierFlags, NSGraphicsContext,
    NSResponder, NSScrollView, NSView, NSViewBoundsDidChangeNotification,
    NSViewFrameDidChangeNotification,
};
use objc2_foundation::{
    NSArray, NSCopying, NSNotificationCenter, NSObject, NSPoint, NSRect, NSSize, NSString,
};
use serde::Serialize;

use probe_diff_view::keymap::KeyInput;
use probe_diff_view::model::{Mode, Side};
use probe_diff_view::tokens::{Theme, ThemeKey};

use self::layout_cache::{Counters, LayoutCache};
use crate::draft_field::DraftField;
use crate::paint::{self, BlockLayout, Half, Painter, RowLayout, Store, fill, flip_text};
use crate::results::Rect;
use crate::scenario::counters::{Live, counter};
use crate::scenario::drawmark::{self, Mark};
use crate::scenario::sampler;
use crate::scene::{Announce, DraftPlace, Scene, Surface, Visible};
use crate::text::{ColorCache, Font};

counter!(DOCUMENT_VIEW_A, "ProbeDiffDocumentViewA");

/// Грязных прямоугольников в журнале наблюдения.
const DIRTY_LOG: usize = 24;

/// Грязный прямоугольник `drawRect:` и что вид нарисовал.
#[derive(Clone, Debug, Serialize)]
pub struct DirtyRecord {
    pub dirty: Rect,
    pub visible: Rect,
    pub rows: Range<u32>,
    pub draw_ms: f64,
}

/// Счётчики отрисовки варианта A.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct DrawStats {
    pub draws: u64,
    pub rows_drawn: u64,
    pub max_rows_per_draw: u32,
    /// Проходы, где грязный прямоугольник выходил за видимую область с запасом.
    pub clipped_draws: u64,
    /// Проходы, когда модель была занята: строки не нарисованы.
    pub busy_draws: u64,
    pub max_draw_ms: f64,
}

pub struct DocumentIvars {
    _live: Live,
    scene: RefCell<Weak<Scene>>,
    theme: Cell<ThemeKey>,
    font: Font,
    cache: RefCell<LayoutCache<RowLayout>>,
    blocks: RefCell<LayoutCache<BlockLayout>>,
    epoch: Cell<u64>,
    colors: RefCell<ColorCache>,
    dirty_log: RefCell<Vec<DirtyRecord>>,
    stats: Cell<DrawStats>,
    ax: ax::AxRows,
}

/// Журнал контракта: полоса текста и блоков строки показа, как их нарисовали `paint_row` и
/// `paint_blocks` по индексу высот, установленному на момент `drawRect:`.
fn record_row(model: &crate::scene::Model, row: u32) {
    use crate::scenario::anchor::Drawn;
    let Some(shown) = model.shown() else {
        return;
    };
    if let (Some(top), Some(height)) = (shown.heights.top(row), shown.heights.text_height(row)) {
        drawmark::drawn_text(Drawn {
            generation: model.generation,
            row,
            top: top as f64,
            height: f64::from(height),
        });
    }
    let frames = model
        .place()
        .map(|place| place.frames_at_row(row))
        .unwrap_or_default();
    let top = frames.iter().map(|frame| frame.y).reduce(f64::min);
    let bottom = frames
        .iter()
        .map(|frame| frame.y + frame.height)
        .reduce(f64::max);
    if let (Some(top), Some(bottom)) = (top, bottom) {
        drawmark::drawn_block(Drawn {
            generation: model.generation,
            row,
            top,
            height: bottom - top,
        });
    }
}

fn rect(value: NSRect) -> Rect {
    Rect {
        x: value.origin.x,
        y: value.origin.y,
        width: value.size.width,
        height: value.size.height,
    }
}

/// Полоса `[y, y + height)` прямоугольника `area`, обрезанная до `bounds` по вертикали.
fn clip_vertical(area: NSRect, top: f64, bottom: f64) -> NSRect {
    let y = area.origin.y.max(top);
    let end = (area.origin.y + area.size.height).min(bottom);
    NSRect::new(
        NSPoint::new(area.origin.x, y),
        NSSize::new(area.size.width, (end - y).max(0.0)),
    )
}

define_class!(
    /// Document view варианта A.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeDiffDocumentViewA"]
    #[ivars = DocumentIvars]
    pub struct DocumentView;

    impl DocumentView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            self.draw(dirty);
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !self.key(event) {
                // SAFETY: обычная передача клавиши дальше по цепочке ответчиков.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(window) = self.window() {
                let responder: &NSResponder = self;
                window.makeFirstResponder(Some(responder));
            }
            let shift = event.modifierFlags().contains(NSEventModifierFlags::Shift);
            self.click(event, shift);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.click(event, true);
        }

        #[unsafe(method(clipBoundsChanged:))]
        fn clip_bounds_changed(&self, _notification: &AnyObject) {
            if let Some(scene) = self.scene() {
                scene.scrolled();
            }
        }

        #[unsafe(method(scrollFrameChanged:))]
        fn scroll_frame_changed(&self, _notification: &AnyObject) {
            self.fit_width();
            if let Some(scene) = self.scene() {
                scene.resized();
            }
        }

        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            true
        }

        #[unsafe(method_id(accessibilityRole))]
        fn accessibility_role(&self) -> Option<Retained<NSString>> {
            // SAFETY: неизменяемая константа AppKit.
            Some(unsafe { NSAccessibilityTableRole }.copy())
        }

        #[unsafe(method_id(accessibilityLabel))]
        fn accessibility_label(&self) -> Option<Retained<NSString>> {
            Some(NSString::from_str(ax::TABLE_LABEL))
        }

        /// Строки и подвиды AppKit (поле черновика).
        #[unsafe(method_id(accessibilityChildren))]
        fn accessibility_children(&self) -> Option<Retained<NSArray<AnyObject>>> {
            let mut children = self.ax_rows();
            // SAFETY: `accessibilityChildren` у NSView без аргументов.
            let base: Option<Retained<NSArray<AnyObject>>> =
                unsafe { msg_send![super(self), accessibilityChildren] };
            children.extend(base.iter().flat_map(|base| base.iter()));
            Some(NSArray::from_retained_slice(&children))
        }

        #[unsafe(method_id(accessibilityRows))]
        fn accessibility_rows(&self) -> Option<Retained<NSArray<AnyObject>>> {
            Some(NSArray::from_retained_slice(&self.ax_rows()))
        }

        #[unsafe(method_id(accessibilityVisibleRows))]
        fn accessibility_visible_rows(&self) -> Option<Retained<NSArray<AnyObject>>> {
            Some(NSArray::from_retained_slice(&self.ax_rows()))
        }

        #[unsafe(method_id(accessibilitySelectedRows))]
        fn accessibility_selected_rows(&self) -> Option<Retained<NSArray<AnyObject>>> {
            let rows: Vec<Retained<AnyObject>> = self
                .ivars()
                .ax
                .selected(&self.ax_rows_typed());
            Some(NSArray::from_retained_slice(&rows))
        }

        /// Фокус доступности — элемент строки курсора, если она видна, иначе контейнер.
        #[unsafe(method_id(accessibilityFocusedUIElement))]
        fn accessibility_focused(&self) -> Option<Retained<AnyObject>> {
            let _ = self.ax_rows_typed();
            Some(self.ax_focused())
        }

        /// Отправка накопленных уведомлений доступности: раз за проход цикла.
        #[unsafe(method(axFlush:))]
        fn ax_flush(&self, _argument: Option<&AnyObject>) {
            self.flush_accessibility();
        }
    }
);

impl DocumentView {
    fn new(mtm: MainThreadMarker, frame: NSRect, font: Font, theme: ThemeKey) -> Retained<Self> {
        let capacity = layout_cache::capacity(frame.size.height, font.line_height, 1);
        let this = DocumentView::alloc(mtm).set_ivars(DocumentIvars {
            _live: DOCUMENT_VIEW_A.live(),
            scene: RefCell::new(Weak::new()),
            theme: Cell::new(theme),
            font,
            cache: RefCell::new(LayoutCache::new(capacity)),
            blocks: RefCell::new(LayoutCache::new(capacity)),
            epoch: Cell::new(1),
            colors: RefCell::new(ColorCache::default()),
            dirty_log: RefCell::new(Vec::new()),
            stats: Cell::new(DrawStats::default()),
            ax: ax::AxRows::default(),
        });
        // SAFETY: назначенный инициализатор NSView с прямоугольником.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn scene(&self) -> Option<Rc<Scene>> {
        self.ivars().scene.borrow().upgrade()
    }

    /// Ширина документа по ширине прокрутки до вопроса сцене о видимой области: видимая
    /// область — пересечение clip view с документом, и узкий документ занизил бы её ширину.
    fn fit_width(&self) {
        let Some(scroll) = self.enclosingScrollView() else {
            return;
        };
        let width = scroll.contentSize().width;
        let frame = self.frame();
        if (frame.size.width - width).abs() >= 0.5 {
            self.setFrameSize(NSSize::new(width, frame.size.height));
        }
    }

    fn key(&self, event: &NSEvent) -> bool {
        let Some(scene) = self.scene() else {
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

    fn click(&self, event: &NSEvent, extend: bool) {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        if let Some(scene) = self.scene() {
            scene.click_at(point.x, point.y, extend);
        }
    }

    fn theme(&self) -> &'static Theme {
        Theme::of(self.ivars().theme.get())
    }

    fn draw(&self, dirty: NSRect) {
        let start = sampler::uptime_ns().unwrap_or(0);
        let Some(graphics) = NSGraphicsContext::currentContext() else {
            return;
        };
        let context = graphics.CGContext();
        let theme = self.theme();
        let visible = self.visibleRect();
        let margin = visible.size.height;
        let area = clip_vertical(
            dirty,
            visible.origin.y - margin,
            visible.origin.y + visible.size.height + margin,
        );
        fill(&context, theme.background, area);
        let mut stats = self.ivars().stats.get();
        stats.draws += 1;
        if area.size.height < dirty.size.height {
            stats.clipped_draws += 1;
        }
        let Some(scene) = self.scene() else {
            self.ivars().stats.set(stats);
            return;
        };
        let Ok(model) = scene.model().try_borrow() else {
            stats.busy_draws += 1;
            self.ivars().stats.set(stats);
            return;
        };
        let rows = match model.shown() {
            Some(shown) => {
                let rows = shown.heights.visible(
                    area.origin.y.max(0.0) as u64,
                    area.size.height.max(0.0) as u64,
                );
                let seen = shown.heights.visible(
                    visible.origin.y.max(0.0) as u64,
                    visible.size.height.max(0.0) as u64,
                );
                let halves = match model.mode {
                    Mode::Unified => 1,
                    Mode::SideBySide => 2,
                };
                let capacity =
                    layout_cache::capacity(visible.size.height, shown.metrics.line_height, halves);
                let ivars = self.ivars();
                ivars.cache.borrow_mut().set_capacity(capacity);
                ivars.blocks.borrow_mut().set_capacity(capacity);
                flip_text(&context);
                let width = visible.size.width.max(self.bounds().size.width);
                let mut colors = ivars.colors.borrow_mut();
                let mut painter = Painter {
                    context: &context,
                    theme,
                    font: &ivars.font,
                    colors: &mut colors,
                    epoch: ivars.epoch.get(),
                };
                let mut cache = ivars.cache.borrow_mut();
                let mut blocks = ivars.blocks.borrow_mut();
                for row in rows.clone() {
                    paint::paint_row(
                        &mut painter,
                        &model,
                        row,
                        width,
                        &mut Cached {
                            cache: &mut cache,
                            seen: &seen,
                        },
                    );
                    paint::paint_blocks(
                        &mut painter,
                        &model,
                        row,
                        width,
                        &mut Cached {
                            cache: &mut blocks,
                            seen: &seen,
                        },
                    );
                    if drawmark::recording() {
                        record_row(&model, row);
                    }
                }
                rows
            }
            None => 0..0,
        };
        let generation = model.generation;
        drop(model);
        let end = sampler::uptime_ns().unwrap_or(0);
        let draw_ms = end.saturating_sub(start) as f64 / 1e6;
        stats.rows_drawn += u64::from(rows.end - rows.start);
        stats.max_rows_per_draw = stats.max_rows_per_draw.max(rows.end - rows.start);
        stats.max_draw_ms = stats.max_draw_ms.max(draw_ms);
        self.ivars().stats.set(stats);
        {
            let mut log = self.ivars().dirty_log.borrow_mut();
            if log.len() < DIRTY_LOG {
                log.push(DirtyRecord {
                    dirty: rect(dirty),
                    visible: rect(visible),
                    rows: rows.clone(),
                    draw_ms,
                });
            }
        }
        drawmark::mark(Mark {
            generation,
            rows: rows.start as usize..rows.end as usize,
            draw_start_ns: start,
            draw_end_ns: end,
        });
    }

    fn counters(&self) -> Counters {
        self.ivars().cache.borrow().counters()
    }

    /// Элементы видимых строк с атрибутами по модели.
    fn ax_rows_typed(&self) -> Vec<Retained<objc2_app_kit::NSAccessibilityElement>> {
        let Some(scene) = self.scene() else {
            return Vec::new();
        };
        let Ok(model) = scene.model().try_borrow() else {
            return Vec::new();
        };
        let Some(shown) = model.shown() else {
            return Vec::new();
        };
        let visible = self.visibleRect();
        let rows = shown.heights.visible(
            visible.origin.y.max(0.0) as u64,
            visible.size.height.max(0.0) as u64,
        );
        self.ivars().ax.visible(self.mtm(), self, &model, rows)
    }

    fn ax_rows(&self) -> Vec<Retained<AnyObject>> {
        self.ax_rows_typed()
            .into_iter()
            .map(|element| Retained::into_super(Retained::into_super(element)))
            .collect()
    }

    /// Элемент строки курсора среди выданных или сам контейнер.
    fn ax_focused(&self) -> Retained<AnyObject> {
        let row = self
            .scene()
            .and_then(|scene| scene.model().try_borrow().ok()?.cursor_row());
        match row.and_then(|row| self.ivars().ax.element_of(row)) {
            Some(element) => Retained::into_super(Retained::into_super(element)),
            None => Retained::into_super(Retained::into_super(Retained::into_super(
                Retained::into_super(self.retain()),
            ))),
        }
    }

    fn flush_accessibility(&self) {
        let ax = &self.ivars().ax;
        let change = ax.take_pending();
        if change.is_empty() {
            return;
        }
        // Атрибуты обновляются, только если клиент уже спрашивал строки.
        if ax.active() {
            let _ = self.ax_rows_typed();
        }
        let focused = self.ax_focused();
        ax.post(change, self, &focused);
    }

    fn block_counters(&self) -> Counters {
        self.ivars().blocks.borrow().counters()
    }
}

/// Раскладки варианта A — записи ограниченного кэша вида.
struct Cached<'a, V> {
    cache: &'a mut LayoutCache<V>,
    seen: &'a Range<u32>,
}

impl<V> Store<V> for Cached<'_, V> {
    fn with(
        &mut self,
        row: u32,
        half: Half,
        version: u64,
        build: &mut dyn FnMut() -> V,
        draw: &mut dyn FnMut(&V),
    ) {
        draw(
            self.cache
                .get_or_build(row, half, version, self.seen, build),
        );
    }
}

/// Вид варианта A для сцены: прокрутка и document view.
pub struct ViewA {
    mtm: MainThreadMarker,
    theme: ThemeKey,
    scroll: Retained<NSScrollView>,
    document: Retained<DocumentView>,
    /// Одно поле черновика на вид, создаётся при первом черновике.
    draft: OnceCell<Retained<DraftField>>,
}

impl ViewA {
    /// Прокрутка во весь `frame` с пустым document view. Привязка к сцене — [`ViewA::bind`].
    pub fn new(mtm: MainThreadMarker, frame: NSRect, theme: ThemeKey) -> Rc<ViewA> {
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), frame);
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(false);
        scroll.setDrawsBackground(true);
        let [r, g, b, a] = Theme::of(theme).background.components();
        scroll.setBackgroundColor(&NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a));
        let content = scroll.contentSize();
        let document = DocumentView::new(
            mtm,
            NSRect::new(NSPoint::ZERO, content),
            Font::monospaced(),
            theme,
        );
        scroll.setDocumentView(Some(&document));
        let clip = scroll.contentView();
        clip.setPostsBoundsChangedNotifications(true);
        scroll.setPostsFrameChangedNotifications(true);
        // SAFETY: наблюдатель — document view, он отвечает на `clipBoundsChanged:` и
        // `scrollFrameChanged:`; снимается в `Drop` до освобождения вида.
        unsafe {
            let center = NSNotificationCenter::defaultCenter();
            center.addObserver_selector_name_object(
                &document,
                sel!(clipBoundsChanged:),
                Some(NSViewBoundsDidChangeNotification),
                Some(&clip),
            );
            center.addObserver_selector_name_object(
                &document,
                sel!(scrollFrameChanged:),
                Some(NSViewFrameDidChangeNotification),
                Some(&scroll),
            );
        }
        Rc::new(ViewA {
            mtm,
            theme,
            scroll,
            document,
            draft: OnceCell::new(),
        })
    }

    pub fn bind(&self, scene: &Rc<Scene>) {
        *self.document.ivars().scene.borrow_mut() = Rc::downgrade(scene);
    }

    pub fn font(&self) -> crate::scene::FontMetrics {
        let font = &self.document.ivars().font;
        crate::scene::FontMetrics {
            char_width: font.char_width,
            line_height: font.line_height,
        }
    }

    pub fn scroll_view(&self) -> &NSScrollView {
        &self.scroll
    }

    pub fn responder(&self) -> &NSResponder {
        &self.document
    }

    pub fn document_frame(&self) -> NSRect {
        self.document.frame()
    }

    pub fn cache_counters(&self) -> Counters {
        self.document.counters()
    }

    pub fn block_cache_counters(&self) -> Counters {
        self.document.block_counters()
    }

    /// Рамка поля черновика в документе, если оно видно.
    pub fn draft_shown(&self) -> Option<NSRect> {
        let field = self.draft.get()?;
        (!field.isHidden()).then(|| field.frame())
    }

    pub fn draft_focused(&self) -> bool {
        self.draft.get().is_some_and(|field| field.is_focused())
    }

    pub fn document_focused(&self) -> bool {
        let responder: &NSResponder = &self.document;
        self.document
            .window()
            .and_then(|window| window.firstResponder())
            .is_some_and(|first| std::ptr::eq(&*first, responder))
    }

    pub fn cache_len(&self) -> (usize, usize) {
        let cache = self.document.ivars().cache.borrow();
        (cache.len(), cache.capacity())
    }

    pub fn cache_rows(&self) -> Vec<u32> {
        self.document.ivars().cache.borrow().rows().collect()
    }

    pub fn draw_stats(&self) -> DrawStats {
        self.document.ivars().stats.get()
    }

    pub fn ax_stats(&self) -> ax::AxStats {
        self.document.ivars().ax.stats()
    }

    pub fn dirty_log(&self) -> Vec<DirtyRecord> {
        self.document.ivars().dirty_log.borrow().clone()
    }

    /// Верхний край видимой области документа.
    pub fn viewport_y(&self) -> f64 {
        self.scroll.documentVisibleRect().origin.y
    }

    /// Прямоугольник текста строки показа в координатах документа.
    pub fn row_rect(&self, row: u32) -> Option<NSRect> {
        let scene = self.document.scene()?;
        let model = scene.model().try_borrow().ok()?;
        let shown = model.shown()?;
        let top = shown.heights.top(row)? as f64;
        let height = f64::from(shown.heights.text_height(row)?);
        Some(NSRect::new(
            NSPoint::new(0.0, top),
            NSSize::new(self.document.bounds().size.width, height),
        ))
    }

    /// Прямоугольник половины `side` строки показа в «Рядом» в координатах документа.
    pub fn half_rect(&self, row: u32, side: Side) -> Option<NSRect> {
        let scene = self.document.scene()?;
        let geometry = scene.geometry()?;
        let area = self.row_rect(row)?;
        Some(NSRect::new(
            NSPoint::new(geometry.half_origin(side), area.origin.y),
            NSSize::new(geometry.half_width(), area.size.height),
        ))
    }

    /// Прямоугольник документа в координатах окна.
    pub fn to_window(&self, area: NSRect) -> NSRect {
        self.document.convertRect_toView(area, None)
    }

    /// Прямоугольник строк в координатах документа по индексу высот модели.
    fn rows_rect(&self, rows: &Range<u32>) -> Option<NSRect> {
        let scene = self.document.scene()?;
        let model = scene.model().try_borrow().ok()?;
        let shown = model.shown()?;
        let top = shown.heights.top(rows.start)? as f64;
        let bottom = shown.heights.top(rows.end.min(shown.heights.len()))? as f64;
        Some(NSRect::new(
            NSPoint::new(0.0, top),
            NSSize::new(self.document.bounds().size.width, bottom - top),
        ))
    }
}

impl Surface for ViewA {
    fn reload(&self, height: f64) {
        let content = self.scroll.contentSize();
        self.document
            .setFrameSize(NSSize::new(content.width, height.max(content.height)));
        self.document.ivars().cache.borrow_mut().clear();
        self.document.ivars().blocks.borrow_mut().clear();
        let epoch = &self.document.ivars().epoch;
        epoch.set(epoch.get() + 1);
        self.document.setNeedsDisplay(true);
    }

    fn invalidate(&self, rows: &[Range<u32>]) {
        for range in rows {
            self.document
                .ivars()
                .cache
                .borrow_mut()
                .invalidate(range.clone());
            if let Some(area) = self.rows_rect(range) {
                self.document.setNeedsDisplayInRect(area);
            }
        }
    }

    fn scroll_to(&self, y: f64) {
        let clip = self.scroll.contentView();
        clip.scrollToPoint(NSPoint::new(0.0, y));
        self.scroll.reflectScrolledClipView(&clip);
    }

    fn viewport(&self) -> Visible {
        let visible = self.scroll.documentVisibleRect();
        Visible {
            y: visible.origin.y,
            width: visible.size.width,
            height: visible.size.height,
        }
    }

    fn heights_changed(&self, height: f64, rows: &[Range<u32>]) {
        let content = self.scroll.contentSize();
        self.document
            .setFrameSize(NSSize::new(content.width, height.max(content.height)));
        let mut blocks = self.document.ivars().blocks.borrow_mut();
        for range in rows {
            blocks.invalidate(range.clone());
        }
        drop(blocks);
        self.document.setNeedsDisplay(true);
    }

    fn place_draft(&self, place: Option<DraftPlace>) {
        let Some(place) = place else {
            if let Some(field) = self.draft.get() {
                let focused = field.is_focused();
                field.setHidden(true);
                if focused && let Some(window) = self.document.window() {
                    let responder: &NSResponder = &self.document;
                    window.makeFirstResponder(Some(responder));
                }
            }
            return;
        };
        let field = self.draft.get_or_init(|| {
            let field = DraftField::new(
                self.mtm,
                Theme::of(self.theme),
                crate::text::FONT_NAME,
                crate::text::FONT_SIZE,
            );
            if let Some(scene) = self.document.scene() {
                field.bind(&scene);
            }
            self.document.addSubview(&field);
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
        if place.focus
            && let Some(window) = self.document.window()
        {
            let responder: &NSResponder = field;
            window.makeFirstResponder(Some(responder));
        }
    }

    fn announce(&self, change: Announce) {
        if self.document.ivars().ax.defer(change) {
            // SAFETY: document view отвечает на `axFlush:`; отложенный вызов держит его до вызова и
            // снимается в `Drop`.
            unsafe {
                let _: () = msg_send![
                    &*self.document,
                    performSelector: sel!(axFlush:),
                    withObject: None::<&AnyObject>,
                    afterDelay: 0.0f64
                ];
            }
        }
    }
}

impl Drop for ViewA {
    fn drop(&mut self) {
        // SAFETY: снимаются наблюдатель, поставленный в `new`, и отложенная отправка уведомлений
        // доступности.
        unsafe {
            NSNotificationCenter::defaultCenter().removeObserver(&self.document);
            let _: () = msg_send![
                NSObject::class(),
                cancelPreviousPerformRequestsWithTarget: &*self.document
            ];
        }
        *self.document.ivars().scene.borrow_mut() = Weak::new();
        if let Some(field) = self.draft.get() {
            field.unbind();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertical_clip_keeps_width() {
        let area = NSRect::new(NSPoint::new(3.0, -100.0), NSSize::new(50.0, 1000.0));
        let clipped = clip_vertical(area, 0.0, 300.0);
        assert_eq!(clipped.origin, NSPoint::new(3.0, 0.0));
        assert_eq!(clipped.size, NSSize::new(50.0, 300.0));
        let outside = clip_vertical(area, 2000.0, 2100.0);
        assert_eq!(outside.size.height, 0.0);
    }
}
