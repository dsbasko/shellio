//! Вид строки таблицы варианта B. Ячеек у таблицы нет: вид строки сам рисует свою строку
//! таблицы — текст строки показа или блоки тредов под ней — общим модулем [`crate::paint`] в
//! координатах документа, сдвинув контекст на своё начало в таблице. Раскладку Core Text он
//! держит сам и строит при отрисовке. Фон и выбор AppKit не рисует. В конце `drawRect:` вид
//! ставит отметку `drawmark` с поколением модели и строкой показа; вид строки блоков отмечает ту
//! же строку показа, что и вид её текста.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;

use block2::StackBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSEvent, NSGraphicsContext, NSResponder, NSTableRowView, NSTableView,
    NSUserInterfaceItemIdentification, NSView,
};
use objc2_core_graphics::CGContext;
use objc2_foundation::{NSInteger, NSObject, NSPoint, NSRect, NSSize, NSString};

use probe_diff_view::tokens::Theme;

use super::{Item, Shared};
use crate::paint::{
    self, BlockLayout, FAR_SCREENS, Half, Painter, RowLayout, Store, distance, fill, flip_text,
};
use crate::scenario::counters::{Live, counter};
use crate::scenario::drawmark::{self, Mark};
use crate::scenario::sampler;

counter!(ROW_VIEW_B, "ProbeDiffRowViewB");

const ROW_ID: &str = "probe.diff.b.row";

pub struct RowIvars {
    _live: Live,
    shared: Rc<Shared>,
    item: Cell<Option<Item>>,
    text: RefCell<Vec<(Half, u64, RowLayout)>>,
    blocks: RefCell<Vec<(Half, u64, BlockLayout)>>,
}

define_class!(
    /// Вид строки: рисует свою строку таблицы, щелчок отдаёт таблице.
    #[unsafe(super(NSTableRowView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeDiffRowViewB"]
    #[ivars = RowIvars]
    pub struct RowView;

    impl RowView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(isOpaque))]
        fn is_opaque(&self) -> bool {
            true
        }

        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            false
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, dirty: NSRect) {
            self.draw(dirty);
        }

        #[unsafe(method(drawBackgroundInRect:))]
        fn draw_background(&self, _rect: NSRect) {}

        #[unsafe(method(drawSelectionInRect:))]
        fn draw_selection(&self, _rect: NSRect) {}

        #[unsafe(method(drawSeparatorInRect:))]
        fn draw_separator(&self, _rect: NSRect) {}

        /// Ширина строки меняется вместе с таблицей: содержимое рисуется заново.
        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            let before = self.frame().size;
            // SAFETY: `setFrameSize:` у NSView с размером.
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            if before != size {
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if let Some(table) = self.ivars().shared.table() {
                table.mouse(event, false);
            }
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            if let Some(table) = self.ivars().shared.table() {
                table.mouse(event, true);
            }
        }
    }
);

/// Раскладки, которые держит вид строки: по половине на запись с версией.
struct Slots<'a, V> {
    slots: &'a mut Vec<(Half, u64, V)>,
    built: &'a mut u64,
}

impl<V> Store<V> for Slots<'_, V> {
    fn with(
        &mut self,
        _row: u32,
        half: Half,
        version: u64,
        build: &mut dyn FnMut() -> V,
        draw: &mut dyn FnMut(&V),
    ) {
        let found = self.slots.iter().position(|slot| slot.0 == half);
        let index = match found {
            Some(index) if self.slots[index].1 == version => index,
            _ => {
                if let Some(index) = found {
                    self.slots.remove(index);
                }
                self.slots.push((half, version, build()));
                *self.built += 1;
                self.slots.len() - 1
            }
        };
        draw(&self.slots[index].2);
    }
}

/// Каждый вид строки варианта B, который таблица держит сейчас, с его строкой таблицы.
pub fn each_row_view(table: &NSTableView, mut visit: impl FnMut(Retained<RowView>, usize)) {
    let found: RefCell<Vec<(Retained<RowView>, usize)>> = RefCell::new(Vec::new());
    let block = StackBlock::new(|view: NonNull<NSTableRowView>, index: NSInteger| {
        // SAFETY: таблица передаёт живой вид строки на время вызова.
        let view: &NSTableRowView = unsafe { view.as_ref() };
        let object: &AnyObject = view;
        if let (Some(view), Ok(index)) = (object.downcast_ref::<RowView>(), usize::try_from(index))
        {
            found.borrow_mut().push((view.retain(), index));
        }
    });
    table.enumerateAvailableRowViewsUsingBlock(&block);
    for (view, index) in found.into_inner() {
        visit(view, index);
    }
}

impl RowView {
    /// Вид строки из очереди переиспользования таблицы или новый.
    pub(super) fn dequeue(
        mtm: MainThreadMarker,
        table: &NSTableView,
        shared: &Rc<Shared>,
    ) -> Retained<RowView> {
        let identifier = NSString::from_str(ROW_ID);
        // SAFETY: владелец не нужен: вид строки создаётся кодом, а не из nib.
        let reused = unsafe { table.makeViewWithIdentifier_owner(&identifier, None) };
        if let Some(view) = reused.and_then(|view| view.downcast::<RowView>().ok()) {
            return view;
        }
        shared.count(|stats| stats.row_views_created += 1);
        let this = RowView::alloc(mtm).set_ivars(RowIvars {
            _live: ROW_VIEW_B.live(),
            shared: Rc::clone(shared),
            item: Cell::new(None),
            text: RefCell::new(Vec::new()),
            blocks: RefCell::new(Vec::new()),
        });
        // SAFETY: назначенный инициализатор NSView с прямоугольником.
        let view: Retained<RowView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setIdentifier(Some(&identifier));
        view
    }

    pub fn item(&self) -> Option<Item> {
        self.ivars().item.get()
    }

    /// Новая строка таблицы: прежние раскладки не годятся.
    pub(super) fn configure(&self, item: Option<Item>) {
        let ivars = self.ivars();
        ivars.shared.count(|stats| stats.row_views_configured += 1);
        if ivars.item.replace(item) != item {
            ivars.text.borrow_mut().clear();
            ivars.blocks.borrow_mut().clear();
        }
    }

    /// Текст строки изменился: стили, курсор или выделение.
    pub(super) fn forget_text(&self) {
        self.ivars().text.borrow_mut().clear();
    }

    fn draw(&self, _dirty: NSRect) {
        let start = sampler::uptime_ns().unwrap_or(0);
        let Some(graphics) = NSGraphicsContext::currentContext() else {
            return;
        };
        let context = graphics.CGContext();
        let ivars = self.ivars();
        let shared = &ivars.shared;
        let theme = Theme::of(shared.theme);
        fill(&context, theme.background, self.bounds());
        shared.count(|stats| stats.draws += 1);
        let (Some(scene), Some(table), Some(item)) =
            (shared.scene(), shared.table(), ivars.item.get())
        else {
            return;
        };
        let Ok(model) = scene.model().try_borrow() else {
            shared.count(|stats| stats.busy_draws += 1);
            return;
        };
        let Some(shown) = model.shown() else {
            return;
        };
        let table_view: &NSView = &table;
        let origin = self.convertPoint_toView(NSPoint::ZERO, Some(table_view));
        let visible = table.visibleRect();
        let seen = shown.heights.visible(
            visible.origin.y.max(0.0) as u64,
            visible.size.height.max(0.0) as u64,
        );
        let width = table.bounds().size.width;
        CGContext::save_g_state(Some(&context));
        CGContext::translate_ctm(Some(&context), -origin.x, -origin.y);
        flip_text(&context);
        let mut built = 0u64;
        {
            let mut colors = shared.colors.borrow_mut();
            let mut painter = Painter {
                context: &context,
                theme,
                font: &shared.font,
                colors: &mut colors,
                epoch: shared.epoch.get(),
            };
            match item {
                Item::Text(row) => paint::paint_row(
                    &mut painter,
                    &model,
                    row,
                    width,
                    &mut Slots {
                        slots: &mut ivars.text.borrow_mut(),
                        built: &mut built,
                    },
                ),
                Item::Block(row) => paint::paint_blocks(
                    &mut painter,
                    &model,
                    row,
                    width,
                    &mut Slots {
                        slots: &mut ivars.blocks.borrow_mut(),
                        built: &mut built,
                    },
                ),
            }
        }
        CGContext::restore_g_state(Some(&context));
        let generation = model.generation;
        drop(model);
        if drawmark::recording() {
            // B отдаёт рамку вида строки в координатах таблицы: её ставит сама таблица.
            let frame = self.frame();
            let drawn = crate::scenario::anchor::Drawn {
                generation,
                row: item.row(),
                top: frame.origin.y,
                height: frame.size.height,
            };
            match item {
                Item::Text(_) => drawmark::drawn_text(drawn),
                Item::Block(_) => drawmark::drawn_block(drawn),
            }
        }
        let row = item.row();
        let screen = seen.end.saturating_sub(seen.start).max(1);
        let far = distance(row, &seen) > FAR_SCREENS * screen;
        let end = sampler::uptime_ns().unwrap_or(0);
        let draw_ms = end.saturating_sub(start) as f64 / 1e6;
        shared.count(|stats| {
            stats.rows_drawn += 1;
            stats.layouts_built += built;
            stats.far_builds += if far { built } else { 0 };
            stats.max_draw_ms = stats.max_draw_ms.max(draw_ms);
        });
        drawmark::mark(Mark {
            generation,
            rows: row as usize..row as usize + 1,
            draw_start_ns: start,
            draw_end_ns: end,
        });
    }
}
