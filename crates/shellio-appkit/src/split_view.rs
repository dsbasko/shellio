//! `SplitView` — сплит раскладки на `NSSplitViewController` с обычными `NSSplitViewItem`
//! (ADR-0009, ADR-0023): режимы сайдбара и инспектора не используются, разделитель нулевой
//! толщины, промежутки между плитками лежат внутри элементов. Начало и длину каждого элемента и его
//! свёртку задаёт владелец ([`SplitView::apply`]) по геометрии модели, собственный расчёт AppKit
//! кадры не выбирает.
//!
//! Правило «Разделитель»: перетаскивание пользователем идёт внутри `mouseDown:` вида сплита. Пока
//! оно идёт, [`SplitView::apply`] кадры не ставит, а в конце уходит одно намерение [`SplitDrag`] с
//! итоговым положением. Собственная установка кадров идёт под флагом повторного входа и намерений
//! не порождает. Полоса захвата разделителя — промежуток перед следующим элементом; разделитель
//! без полосы не перетаскивается.
//!
//! [`Pane`] — содержимое элемента сплита и контейнер раскладки: дети держат отступы от его краёв,
//! поэтому при перетаскивании разделителя плитки идут за краем элемента.

use std::cell::{Cell, OnceCell, RefCell};

use objc2::rc::{self, Retained};
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSEvent, NSResponder, NSSplitView, NSSplitViewController,
    NSSplitViewItem, NSView, NSViewController,
};
use objc2_foundation::{NSInteger, NSPoint, NSRect, NSSize};

use crate::app::App;
use crate::chrome_bar::Frame;
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode};
use crate::layer::without_actions;
use crate::own::{Callback, ComponentId, View};
use crate::piece::{Piece, ns_rect};

counter!(SPLIT_OWNER, Owner, "SplitView");
counter!(SPLIT_VIEW, Class, "ShellioSplitView");
counter!(SPLIT_CONTROLLER, Class, "ShellioSplitController");
counter!(PANE_VIEW, Class, "ShellioPane");

/// Приоритет удержания элементов: при изменении размера окна AppKit сначала меняет гибкий элемент,
/// затем владелец всё равно ставит кадры модели.
const HOLDING_FIXED: f32 = 260.0;

/// Направление элементов сплита.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SplitDirection {
    /// Элементы рядом, разделители вертикальные.
    Columns,
    /// Элементы один над другим, разделители горизонтальные.
    Rows,
}

/// Элемент сплита вдоль его оси в точках сплита.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitSlot {
    pub start: f64,
    pub length: f64,
    /// Свёрнутый элемент: скрытая или пустая зона.
    pub collapsed: bool,
}

/// Элементы сплита и полосы захвата его разделителей.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SplitSpec {
    pub slots: Vec<SplitSlot>,
    /// Ширина полосы захвата разделителя `i` перед началом элемента `i + 1`; 0 — разделитель не
    /// перетаскивается.
    pub grips: Vec<f64>,
}

/// Конец перетаскивания разделителя пользователем: новое начало элемента `divider + 1` вдоль оси
/// сплита.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitDrag {
    pub divider: usize,
    pub position: f64,
}

/// Отступы ребёнка [`Pane`] от его краёв.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Insets {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl Insets {
    pub const ZERO: Insets = Insets {
        left: 0.0,
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
    };

    /// Кадр ребёнка в контейнере размера `width`×`height`; размер не меньше нуля.
    pub fn frame_in(self, width: f64, height: f64) -> Frame {
        Frame::new(
            self.left,
            self.top,
            (width - self.left - self.right).max(0.0),
            (height - self.top - self.bottom).max(0.0),
        )
    }
}

/// Какой разделитель сдвинулся за перетаскивание: первое начало элемента, изменившееся больше чем
/// на четверть точки. `None` — разделитель остался на месте.
pub fn moved_divider(before: &[f64], after: &[f64]) -> Option<SplitDrag> {
    before
        .iter()
        .zip(after)
        .enumerate()
        .skip(1)
        .find(|(_, (old, new))| (*new - *old).abs() > 0.25)
        .map(|(index, (_, new))| SplitDrag {
            divider: index - 1,
            position: *new,
        })
}

/// Rust-часть сплита. Виды и контроллер читают её через слабую ссылку.
struct SplitModel {
    id: ComponentId,
    direction: SplitDirection,
    grips: RefCell<Vec<f64>>,
    /// Владелец ставит кадры: события AppKit намерений не порождают.
    applying: Cell<bool>,
    /// Идёт перетаскивание разделителя пользователем.
    dragging: Cell<bool>,
    on_drag: Box<dyn Fn(SplitDrag)>,
}

struct Entry {
    item: Retained<NSSplitViewItem>,
    _controller: Retained<NSViewController>,
    pane: Pane,
}

/// Сплит на главном потоке. Освобождение убирает его вид из иерархии.
pub struct SplitView {
    view: View<ShellioSplitView, SplitModel>,
    /// Вид контроллера в иерархии: его загрузка ставит элементы в сплит.
    host: Retained<NSView>,
    controller: Retained<ShellioSplitController>,
    entries: RefCell<Vec<Entry>>,
}

impl SplitView {
    /// Пустой сплит. `on_drag` вызывается на главном потоке изнутри AppKit в конце перетаскивания и
    /// не должен синхронно менять сплит.
    pub fn new(
        app: &App,
        direction: SplitDirection,
        on_drag: impl Fn(SplitDrag) + 'static,
    ) -> SplitView {
        let mtm = app.mtm();
        let model = SplitModel {
            id: ComponentId::next(),
            direction,
            grips: RefCell::new(Vec::new()),
            applying: Cell::new(false),
            dragging: Cell::new(false),
            on_drag: Box::new(on_drag),
        };
        let view = View::new(&SPLIT_OWNER, model, "SplitView.view", |owner| {
            ShellioSplitView::new(mtm, owner)
        });
        let split = view.view();
        split.setVertical(direction == SplitDirection::Columns);
        let controller = ShellioSplitController::new(mtm, view.callback("SplitView.controller"));
        controller.setSplitView(split);
        let host = controller.view();
        SplitView {
            view,
            host,
            controller,
            entries: RefCell::new(Vec::new()),
        }
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    /// Число элементов.
    pub fn len(&self) -> usize {
        self.entries.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.borrow().is_empty()
    }

    /// Идёт перетаскивание разделителя пользователем.
    pub fn is_dragging(&self) -> bool {
        self.view.rust().dragging.get()
    }

    /// Содержимое элемента `index`.
    pub fn with_pane<R>(&self, index: usize, f: impl FnOnce(&Pane) -> R) -> Option<R> {
        self.entries.borrow().get(index).map(|entry| f(&entry.pane))
    }

    /// Ставит элементы по `spec`: число элементов, свёртку и начала. Во время перетаскивания
    /// пользователем ничего не делает и отвечает `false`.
    pub fn apply(&self, spec: &SplitSpec) -> bool {
        let model = self.view.rust();
        if model.dragging.get() {
            return false;
        }
        model.grips.replace(spec.grips.clone());
        self.set_count(spec.slots.len());
        model.applying.set(true);
        without_actions(|| {
            let entries = self.entries.borrow();
            for (entry, slot) in entries.iter().zip(&spec.slots) {
                if entry.item.isCollapsed() != slot.collapsed {
                    entry.item.setCollapsed(slot.collapsed);
                }
            }
            drop(entries);
            let split = self.view.view();
            let dividers = spec.slots.len().saturating_sub(1);
            // Разделитель не заходит за соседа: два прохода в обе стороны ставят любой порядок.
            for _ in 0..2 {
                for index in (0..dividers).chain((0..dividers).rev()) {
                    let position = spec.slots[index + 1].start;
                    let index = NSInteger::try_from(index).unwrap_or(NSInteger::MAX);
                    split.setPosition_ofDividerAtIndex(position, index);
                }
            }
            split.layoutSubtreeIfNeeded();
        });
        model.applying.set(false);
        true
    }

    /// Начало и длина каждого элемента вдоль оси, как их поставил AppKit.
    pub fn slots(&self) -> Vec<(f64, f64)> {
        self.view.view().spans()
    }

    /// Прячет или показывает сплит без неявных анимаций.
    pub fn set_hidden(&self, hidden: bool) {
        if self.host.isHidden() != hidden {
            without_actions(|| self.host.setHidden(hidden));
        }
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        &self.host
    }

    /// Сплит для `ui.inspect`.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        vec![inspect::node(
            id,
            "SplitView",
            parent,
            self.view.view(),
            None,
            None,
        )]
    }

    fn set_count(&self, count: usize) {
        let mut entries = self.entries.borrow_mut();
        let mtm = self.view.view().mtm();
        while entries.len() < count {
            let pane = Pane::with_mtm(mtm);
            let controller = NSViewController::new(mtm);
            controller.setView(pane.ns_view());
            let item = NSSplitViewItem::splitViewItemWithViewController(&controller);
            item.setMinimumThickness(0.0);
            item.setCanCollapse(true);
            item.setCanCollapseFromWindowResize(false);
            item.setHoldingPriority(HOLDING_FIXED);
            self.controller.addSplitViewItem(&item);
            entries.push(Entry {
                item,
                _controller: controller,
                pane,
            });
        }
        while entries.len() > count {
            if let Some(entry) = entries.pop() {
                self.controller.removeSplitViewItem(&entry.item);
            }
        }
    }
}

impl Drop for SplitView {
    fn drop(&mut self) {
        for entry in self.entries.borrow_mut().drain(..).rev() {
            self.controller.removeSplitViewItem(&entry.item);
        }
        self.host.removeFromSuperview();
    }
}

struct SplitIvars {
    _live: Live,
    owner: Callback<SplitModel>,
}

define_class!(
    // SAFETY: NSSplitView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop`
    // у класса нет.
    #[unsafe(super(NSSplitView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioSplitView"]
    #[ivars = SplitIvars]
    struct ShellioSplitView;

    impl ShellioSplitView {
        // SAFETY: селектор и результат — как у `dividerThickness` NSSplitView.
        #[unsafe(method(dividerThickness))]
        fn divider_thickness(&self) -> f64 {
            0.0
        }

        // SAFETY: селектор и аргумент — как у `drawDividerInRect:` NSSplitView. Разделитель нулевой
        // толщины не рисуется.
        #[unsafe(method(drawDividerInRect:))]
        fn draw_divider_in_rect(&self, _rect: NSRect) {}

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. NSSplitView отслеживает
        // перетаскивание разделителя внутри вызова суперкласса и возвращается после отпускания.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let before = self.starts();
            self.ivars().owner.call(|model| model.dragging.set(true));
            // SAFETY: `mouseDown:` суперкласса с тем же событием.
            let () = unsafe { msg_send![super(self), mouseDown: event] };
            let after = self.starts();
            self.ivars().owner.call(|model| {
                model.dragging.set(false);
                if let Some(drag) = moved_divider(&before, &after) {
                    (model.on_drag)(drag);
                }
            });
        }
    }
);

impl ShellioSplitView {
    fn new(mtm: MainThreadMarker, owner: Callback<SplitModel>) -> Retained<ShellioSplitView> {
        let this = ShellioSplitView::alloc(mtm).set_ivars(SplitIvars {
            _live: SPLIT_VIEW.live(),
            owner,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<ShellioSplitView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view
    }

    fn direction(&self) -> SplitDirection {
        if self.isVertical() {
            SplitDirection::Columns
        } else {
            SplitDirection::Rows
        }
    }

    /// Начала элементов вдоль оси.
    fn starts(&self) -> Vec<f64> {
        self.spans().into_iter().map(|(start, _)| start).collect()
    }

    fn spans(&self) -> Vec<(f64, f64)> {
        let columns = self.direction() == SplitDirection::Columns;
        self.arrangedSubviews()
            .iter()
            .map(|view| {
                let frame = view.frame();
                if columns {
                    (frame.origin.x, frame.size.width)
                } else {
                    (frame.origin.y, frame.size.height)
                }
            })
            .collect()
    }
}

struct ControllerIvars {
    _live: Live,
    owner: OnceCell<Callback<SplitModel>>,
}

define_class!(
    // SAFETY: NSSplitViewController не требует от подкласса ничего сверх переопределённых ниже
    // методов, `Drop` у класса нет.
    #[unsafe(super(NSSplitViewController, NSViewController, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioSplitController"]
    #[ivars = ControllerIvars]
    struct ShellioSplitController;

    impl ShellioSplitController {
        // SAFETY: селектор, аргументы и результат — как у метода делегата сплита, который
        // реализует NSSplitViewController. Полоса захвата — промежуток перед следующим элементом.
        #[unsafe(method(splitView:effectiveRect:forDrawnRect:ofDividerAtIndex:))]
        fn effective_rect(
            &self,
            split_view: &NSSplitView,
            proposed: NSRect,
            drawn: NSRect,
            index: NSInteger,
        ) -> NSRect {
            let grip = self.ivars().owner.get().and_then(|owner| {
                owner.call(|model| {
                    let grip = usize::try_from(index)
                        .ok()
                        .and_then(|index| model.grips.borrow().get(index).copied())
                        .unwrap_or(0.0);
                    (grip, model.direction)
                })
            });
            match grip {
                Some((grip, _)) if grip <= 0.0 => NSRect::ZERO,
                Some((grip, SplitDirection::Columns)) => NSRect::new(
                    NSPoint::new(drawn.origin.x - grip, drawn.origin.y),
                    NSSize::new(grip + drawn.size.width, drawn.size.height),
                ),
                Some((grip, SplitDirection::Rows)) => NSRect::new(
                    NSPoint::new(drawn.origin.x, drawn.origin.y - grip),
                    NSSize::new(drawn.size.width, grip + drawn.size.height),
                ),
                // SAFETY: тот же метод суперкласса с теми же аргументами.
                None => unsafe {
                    msg_send![
                        super(self),
                        splitView: split_view,
                        effectiveRect: proposed,
                        forDrawnRect: drawn,
                        ofDividerAtIndex: index
                    ]
                },
            }
        }
    }
);

impl ShellioSplitController {
    fn new(mtm: MainThreadMarker, owner: Callback<SplitModel>) -> Retained<ShellioSplitController> {
        let this = ShellioSplitController::alloc(mtm).set_ivars(ControllerIvars {
            _live: SPLIT_CONTROLLER.live(),
            owner: OnceCell::from(owner),
        });
        // SAFETY: `init` у NSViewController без аргументов, ivars уже заданы.
        unsafe { msg_send![super(this), init] }
    }
}

/// Содержимое элемента сплита или контейнер: дети стоят с отступами от его краёв и идут за ними
/// при любом изменении размера.
pub struct Pane {
    view: Retained<PaneView>,
}

impl Pane {
    pub fn new(app: &App) -> Pane {
        Pane::with_mtm(app.mtm())
    }

    fn with_mtm(mtm: MainThreadMarker) -> Pane {
        Pane {
            view: PaneView::new(mtm),
        }
    }

    /// Ставит компонент в контейнер с отступами `insets` от его краёв. Компонент из другого
    /// родителя переставляется без пересоздания.
    pub fn place<'a>(&self, piece: impl Into<Piece<'a>>, insets: Insets) {
        let piece = piece.into();
        let child = piece.ns_view();
        let pane: &NSView = &self.view;
        // SAFETY: `superview` у живого вида только читается.
        let parent = unsafe { child.superview() };
        if !parent.is_some_and(|parent| std::ptr::eq(&*parent, pane)) {
            pane.addSubview(child);
        }
        child.setAutoresizingMask(NSAutoresizingMaskOptions::ViewNotSizable);
        {
            let mut children = self.view.ivars().children.borrow_mut();
            children.retain(|(known, _)| {
                known
                    .load()
                    .is_some_and(|known| !std::ptr::eq(&*known, child))
            });
            children.push((rc::Weak::new(child), insets));
        }
        let size = pane.bounds().size;
        child.setFrame(ns_rect(insets.frame_in(size.width, size.height)));
    }

    /// Прячет или показывает контейнер без неявных анимаций.
    pub fn set_hidden(&self, hidden: bool) {
        if self.view.isHidden() != hidden {
            without_actions(|| self.view.setHidden(hidden));
        }
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        &self.view
    }

    /// Контейнер для `ui.inspect`.
    pub fn inspect(&self, id: &str, class: &'static str, parent: Option<&str>) -> InspectNode {
        inspect::node(id, class, parent, &self.view, None, None)
    }
}

struct PaneIvars {
    _live: Live,
    children: RefCell<Vec<(rc::Weak<NSView>, Insets)>>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioPane"]
    #[ivars = PaneIvars]
    struct PaneView;

    impl PaneView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и аргумент — как у `setFrameSize:` NSView.
        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            // SAFETY: `setFrameSize:` суперкласса с тем же размером.
            let () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.relayout();
        }

        // SAFETY: селектор — как у `layout` NSView.
        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` суперкласса.
            let () = unsafe { msg_send![super(self), layout] };
            self.relayout();
        }
    }
);

impl PaneView {
    fn new(mtm: MainThreadMarker) -> Retained<PaneView> {
        let this = PaneView::alloc(mtm).set_ivars(PaneIvars {
            _live: PANE_VIEW.live(),
            children: RefCell::new(Vec::new()),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<PaneView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        view
    }

    /// Дети встают по своим отступам; дети, переставленные в другой вид, забываются.
    fn relayout(&self) {
        let size = self.bounds().size;
        let pane: &NSView = self;
        let placed: Vec<(Retained<NSView>, Insets)> = {
            let mut children = self.ivars().children.borrow_mut();
            children.retain(|(child, _)| {
                child.load().is_some_and(|child| {
                    // SAFETY: `superview` у живого вида только читается.
                    let parent = unsafe { child.superview() };
                    parent.is_some_and(|parent| std::ptr::eq(&*parent, pane))
                })
            });
            children
                .iter()
                .filter_map(|(child, insets)| child.load().map(|child| (child, *insets)))
                .collect()
        };
        for (child, insets) in placed {
            let frame = ns_rect(insets.frame_in(size.width, size.height));
            if child.frame() != frame {
                child.setFrame(frame);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insets_keep_edges_and_never_go_negative() {
        let insets = Insets {
            left: 6.0,
            top: 6.0,
            right: 6.0,
            bottom: 8.0,
        };
        assert_eq!(
            insets.frame_in(100.0, 50.0),
            Frame::new(6.0, 6.0, 88.0, 36.0)
        );
        assert_eq!(insets.frame_in(4.0, 4.0), Frame::new(6.0, 6.0, 0.0, 0.0));
        assert_eq!(
            Insets::ZERO.frame_in(7.0, 9.0),
            Frame::new(0.0, 0.0, 7.0, 9.0)
        );
    }

    #[test]
    fn moved_divider_names_the_first_shifted_start() {
        assert_eq!(
            moved_divider(&[0.0, 236.0, 614.0], &[0.0, 236.0, 614.0]),
            None
        );
        assert_eq!(
            moved_divider(&[0.0, 236.0, 614.0], &[0.0, 236.0, 650.0]),
            Some(SplitDrag {
                divider: 1,
                position: 650.0
            })
        );
        assert_eq!(
            moved_divider(&[0.0, 300.0], &[0.0, 300.2]),
            None,
            "дробный сдвиг меньше четверти точки — не перетаскивание"
        );
        assert_eq!(moved_divider(&[], &[]), None);
    }
}
