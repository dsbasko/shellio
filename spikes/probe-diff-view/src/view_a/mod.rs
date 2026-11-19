//! Вариант A: один `NSView` — document view в `NSScrollView`, высота документа из индекса высот
//! модели. `drawRect:` рисует только строки в грязном прямоугольнике, обрезанном до видимой
//! области с запасом в экран: фон добавления и удаления, гуттер с номерами, курсор и выделение,
//! текст Core Text по визуальным строкам модели. Раскладка строк живёт в ограниченном кэше
//! ([`layout_cache`]). Доменного состояния вид не хранит: он читает модель сцены, клавиши и мышь
//! отдаёт её обработчикам, прокрутку к якорю ведёт `scrollToPoint` у clip view.

pub mod layout_cache;

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSColor, NSEvent, NSEventModifierFlags, NSGraphicsContext, NSResponder, NSScrollView, NSView,
    NSViewBoundsDidChangeNotification,
};
use objc2_core_foundation::{CFRetained, CGAffineTransform};
use objc2_core_graphics::CGContext;
use objc2_core_text::CTLine;
use objc2_foundation::{NSNotificationCenter, NSPoint, NSRect, NSSize};
use serde::Serialize;

use probe_diff_view::keymap::KeyInput;
use probe_diff_view::model::layout::Gutter;
use probe_diff_view::model::wrap;
use probe_diff_view::model::{LineKind, Mode, Side};
use probe_diff_view::tokens::{Rgb, Theme, ThemeKey};

use self::layout_cache::{Counters, Half, LayoutCache};
use crate::results::Rect;
use crate::scenario::counters::{Live, counter};
use crate::scenario::drawmark::{self, Mark};
use crate::scenario::sampler;
use crate::scene::{Model, Scene, Surface, Visible, drawn_side};
use crate::text::{ColorCache, Font};

counter!(DOCUMENT_VIEW_A, "ProbeDiffDocumentViewA");

/// Грязных прямоугольников в журнале наблюдения.
const DIRTY_LOG: usize = 24;

/// Раскладка одной строки показа: гуттер и визуальные строки текста.
pub struct RowLayout {
    gutter: CFRetained<CTLine>,
    segments: Vec<CFRetained<CTLine>>,
}

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
    epoch: Cell<u64>,
    colors: RefCell<ColorCache>,
    dirty_log: RefCell<Vec<DirtyRecord>>,
    stats: Cell<DrawStats>,
}

fn rect(value: NSRect) -> Rect {
    Rect {
        x: value.origin.x,
        y: value.origin.y,
        width: value.size.width,
        height: value.size.height,
    }
}

fn fill(context: &CGContext, color: Rgb, area: NSRect) {
    let [r, g, b, a] = color.components();
    CGContext::set_rgb_fill_color(Some(context), r, g, b, a);
    CGContext::fill_rect(Some(context), area);
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
            epoch: Cell::new(1),
            colors: RefCell::new(ColorCache::default()),
            dirty_log: RefCell::new(Vec::new()),
            stats: Cell::new(DrawStats::default()),
        });
        // SAFETY: назначенный инициализатор NSView с прямоугольником.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    fn scene(&self) -> Option<Rc<Scene>> {
        self.ivars().scene.borrow().upgrade()
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
            scene.click(point.y, None, extend);
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
                let capacity =
                    layout_cache::capacity(visible.size.height, shown.metrics.line_height, 1);
                self.ivars().cache.borrow_mut().set_capacity(capacity);
                CGContext::set_text_matrix(
                    Some(&context),
                    CGAffineTransform {
                        a: 1.0,
                        b: 0.0,
                        c: 0.0,
                        d: -1.0,
                        tx: 0.0,
                        ty: 0.0,
                    },
                );
                for row in rows.clone() {
                    self.paint_row(
                        &context,
                        &model,
                        row,
                        &seen,
                        visible.size.width.max(self.bounds().size.width),
                    );
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

    /// Строка показа unified: фон по виду строки, гуттер с номерами и знаком, визуальные строки
    /// текста. Курсор отмечен фоном гуттера, выделение — фоном текста.
    fn paint_row(
        &self,
        context: &CGContext,
        model: &Model,
        row: u32,
        seen: &Range<u32>,
        width: f64,
    ) {
        let Some(shown) = model.shown() else {
            return;
        };
        let mode = model.mode;
        let (Some(top), Some(text_height)) =
            (shown.heights.top(row), shown.heights.text_height(row))
        else {
            return;
        };
        let Some(index) = shown.body.row(mode, row).and_then(|halves| halves.first()) else {
            return;
        };
        let Some(line) = shown.body.line(index) else {
            return;
        };
        let theme = self.theme();
        let font = &self.ivars().font;
        let top = top as f64;
        let height = f64::from(text_height);
        let gutter_width = f64::from(shown.geometry.gutter_cells(mode)) * font.char_width;
        let kind = line.kind();
        let state = model.state.as_ref();
        let cursor = state
            .and_then(|state| state.cursor())
            .and_then(|cursor| shown.body.row_of_addr(mode, cursor.addr))
            .is_some_and(|located| located.index == row);
        let selected = state
            .and_then(|state| state.selection())
            .is_some_and(|selection| {
                line.addr(selection.side)
                    .is_some_and(|addr| selection.contains(addr))
            });
        let (text_fill, gutter_fill) = match kind {
            LineKind::Context => (theme.background, theme.gutter_background),
            LineKind::Added => (theme.added_background, theme.added_gutter),
            LineKind::Removed => (theme.removed_background, theme.removed_gutter),
        };
        let text_fill = if selected {
            theme.selection
        } else if cursor && kind == LineKind::Context {
            theme.cursor_line
        } else {
            text_fill
        };
        let gutter_fill = if cursor { theme.selection } else { gutter_fill };
        fill(
            context,
            gutter_fill,
            NSRect::new(NSPoint::new(0.0, top), NSSize::new(gutter_width, height)),
        );
        fill(
            context,
            text_fill,
            NSRect::new(
                NSPoint::new(gutter_width, top),
                NSSize::new((width - gutter_width).max(0.0), height),
            ),
        );
        if row > 0 && shown.body.starts_hunk(mode, row) {
            fill(
                context,
                theme.separator,
                NSRect::new(NSPoint::new(0.0, top), NSSize::new(width, 1.0)),
            );
        }
        let text = shown.diff.line_text(line);
        let side = drawn_side(kind, mode, Side::New);
        let runs = line
            .line(side)
            .map_or(&[][..], |number| model.styles.runs(side, number));
        let epoch = self.ivars().epoch.get();
        let digits = shown.geometry.gutter_cells(Mode::Unified);
        let mut cache = self.ivars().cache.borrow_mut();
        let mut colors = self.ivars().colors.borrow_mut();
        let layout = cache.get_or_build(row, Half::Unified, epoch, seen, || {
            let gutter = gutter_text(
                shown.geometry.gutter,
                digits,
                line.line(Side::Old),
                line.line(Side::New),
                kind,
            );
            let gutter_units: Vec<u16> = gutter.encode_utf16().collect();
            RowLayout {
                gutter: font.line(
                    &gutter_units,
                    &[(0..gutter_units.len(), theme.gutter_text)],
                    &mut colors,
                ),
                segments: wrap::segments(text, shown.metrics.wrap)
                    .into_iter()
                    .map(|segment| {
                        font.segment_line(
                            text,
                            segment,
                            shown.metrics.wrap,
                            runs,
                            theme,
                            &mut colors,
                        )
                    })
                    .collect(),
            }
        });
        let line_height = f64::from(shown.metrics.line_height);
        let baseline = |visual: usize| top + visual as f64 * line_height + font.ascent;
        CGContext::set_text_position(Some(context), 0.0, baseline(0));
        // SAFETY: строка Core Text и контекст живы на время вызова.
        unsafe { layout.gutter.draw(context) };
        for (visual, segment) in layout.segments.iter().enumerate() {
            CGContext::set_text_position(Some(context), gutter_width, baseline(visual));
            // SAFETY: строка Core Text и контекст живы на время вызова.
            unsafe { segment.draw(context) };
        }
    }

    fn counters(&self) -> Counters {
        self.ivars().cache.borrow().counters()
    }
}

/// Текст гуттера unified: номера сторон по своим колонкам и знак. Колонок номеров — по
/// [`Gutter`], ширина — `cells` клеток.
pub fn gutter_text(
    gutter: Gutter,
    cells: u32,
    old: Option<u32>,
    new: Option<u32>,
    kind: LineKind,
) -> String {
    let sign = match kind {
        LineKind::Context => ' ',
        LineKind::Added => '+',
        LineKind::Removed => '-',
    };
    let numbers = match gutter {
        Gutter::Both => 2,
        Gutter::Single => 1,
    };
    // Клетки знака и пробела после него; остальное — колонки номеров с пробелом после каждой.
    let digits = (cells.saturating_sub(2) / numbers).saturating_sub(1) as usize;
    let number = |value: Option<u32>| {
        value.map_or_else(|| " ".repeat(digits), |value| format!("{value:>digits$}"))
    };
    match gutter {
        Gutter::Both => format!("{} {} {sign} ", number(old), number(new)),
        Gutter::Single => format!("{} {sign} ", number(new.or(old))),
    }
}

/// Вид варианта A для сцены: прокрутка и document view.
pub struct ViewA {
    scroll: Retained<NSScrollView>,
    document: Retained<DocumentView>,
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
        // SAFETY: наблюдатель — document view, он отвечает на `clipBoundsChanged:`; снимается в
        // `Drop` до освобождения вида.
        unsafe {
            NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                &document,
                sel!(clipBoundsChanged:),
                Some(NSViewBoundsDidChangeNotification),
                Some(&clip),
            );
        }
        Rc::new(ViewA { scroll, document })
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
}

impl Drop for ViewA {
    fn drop(&mut self) {
        // SAFETY: снимается наблюдатель, поставленный в `new`.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&self.document) };
        *self.document.ivars().scene.borrow_mut() = Weak::new();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gutter_text_fills_its_cells() {
        // 6 цифр: две колонки по 7 клеток и знак с пробелом — 16 клеток.
        let text = gutter_text(Gutter::Both, 16, Some(12), Some(345_678), LineKind::Context);
        assert_eq!(text, "    12 345678   ");
        assert_eq!(text.chars().count(), 16);
        let text = gutter_text(Gutter::Both, 16, None, Some(7), LineKind::Added);
        assert_eq!(text, "            7 + ");
        let text = gutter_text(Gutter::Both, 16, Some(9), None, LineKind::Removed);
        assert_eq!(text, "     9        - ");
        let text = gutter_text(Gutter::Single, 9, Some(9), None, LineKind::Removed);
        assert_eq!(text, "     9 - ");
    }

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
