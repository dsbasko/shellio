//! `SplitView` — сплит раскладки на `NSSplitViewController` с обычными `NSSplitViewItem`
//! (ADR-0009, ADR-0023): режимы сайдбара и инспектора не используются, разделитель нулевой
//! толщины, промежутки между плитками лежат внутри элементов. Начало и длину каждого элемента и его
//! свёртку задаёт владелец ([`SplitView::apply`]) по геометрии модели, собственный расчёт AppKit
//! кадры не выбирает.
//!
//! Правило «Разделитель»: полоса захвата — собственный вид в промежутке перед следующим элементом,
//! у конечного края содержимого элемента. Нажатие на ней начинает перетаскивание, отпускание
//! заканчивает его одним намерением [`SplitDrag`] с итоговым положением. Пока оно идёт,
//! [`SplitView::apply`] положения не ставит, а за мышью идут края содержимого двух соседних
//! элементов: конечный край элемента до разделителя и начальный край элемента после него. Положения
//! элементов ставит только владелец; собственная установка кадров намерений не порождает, их даёт
//! только полоса. Разделитель без полосы не перетаскивается.
//!
//! Собственное перетаскивание `NSSplitView` выключено (действующий прямоугольник разделителя пуст):
//! при элементах контроллера и разделителе нулевой толщины оно зацикливает проход
//! `updateConstraints` окна, и AppKit бросает исключение. `setPosition:ofDividerAtIndex:`, пока
//! нажата кнопка, положение не меняет, поэтому за мышью идёт содержимое, а не элементы (находки
//! задачи 9 P1.4).
//!
//! [`Pane`] — содержимое элемента сплита и контейнер раскладки: дети держат отступы от его краёв,
//! поэтому при перетаскивании разделителя плитки идут за краем элемента.

use std::cell::{Cell, RefCell};

use objc2::rc::{self, Retained};
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSCursor, NSEvent, NSResponder, NSSplitView, NSSplitViewController,
    NSSplitViewItem, NSView, NSViewController, NSWindowOrderingMode,
};
use objc2_foundation::{NSInteger, NSRect, NSSize};

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
counter!(GRIP_VIEW, Class, "ShellioSplitGrip");

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
    /// Ширина полосы захвата разделителя `i` у конечного края элемента `i`; 0 — разделитель не
    /// перетаскивается. Полоса двигает границу с ближайшим несвёрнутым элементом после `i`
    /// (`next_shown`), свёрнутые элементы между ними не участвуют.
    pub grips: Vec<f64>,
}

/// Ближайший несвёрнутый элемент после разделителя `divider`; `None` — его нет.
fn next_shown(slots: &[SplitSlot], divider: usize) -> Option<usize> {
    slots
        .iter()
        .enumerate()
        .skip(divider + 1)
        .find(|(_, slot)| !slot.collapsed)
        .map(|(index, _)| index)
}

/// Конец перетаскивания разделителя пользователем: новое начало ближайшего несвёрнутого элемента
/// после `divider` вдоль оси сплита.
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

impl Insets {
    /// Отступы, сдвинутые перетаскиванием разделителя: начальный край вдоль оси `axis` на `lead`,
    /// конечный на `trail`; положительный сдвиг — к концу оси.
    pub fn shifted(self, axis: SplitDirection, lead: f64, trail: f64) -> Insets {
        match axis {
            SplitDirection::Columns => Insets {
                left: self.left + lead,
                right: self.right - trail,
                ..self
            },
            SplitDirection::Rows => Insets {
                top: self.top + lead,
                bottom: self.bottom - trail,
                ..self
            },
        }
    }
}

/// Конец перетаскивания разделителя `divider` с началом следующего элемента `start` на `delta`
/// точек. `None` — сдвиг меньше половины точки: размер не меняется.
pub fn drag_end(divider: usize, start: f64, delta: f64) -> Option<SplitDrag> {
    (delta.abs() >= 0.5).then_some(SplitDrag {
        divider,
        position: start + delta,
    })
}

/// Rust-часть сплита. Виды читают её через слабую ссылку.
struct SplitModel {
    id: ComponentId,
    direction: SplitDirection,
    /// Идёт перетаскивание разделителя пользователем.
    dragging: Cell<bool>,
    on_drag: Box<dyn Fn(SplitDrag)>,
}

/// Левая кнопка мыши сейчас нажата.
fn left_button_down() -> bool {
    NSEvent::pressedMouseButtons() & 1 != 0
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
            dragging: Cell::new(false),
            on_drag: Box::new(on_drag),
        };
        let view = View::new(&SPLIT_OWNER, model, "SplitView.view", |owner| {
            ShellioSplitView::new(mtm, owner)
        });
        let split = view.view();
        split.setVertical(direction == SplitDirection::Columns);
        let controller = ShellioSplitController::new(mtm);
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

    /// Ставит элементы по `spec`: число элементов, свёртку, начала и полосы захвата; сдвиг
    /// содержимого перетаскиванием снимается. Во время перетаскивания пользователем ничего не делает
    /// и отвечает `false`. Перетаскивание, отпускание которого полоса не получила (кнопка уже
    /// отпущена), сначала сбрасывается без намерения.
    pub fn apply(&self, spec: &SplitSpec) -> bool {
        let model = self.view.rust();
        if model.dragging.get() {
            if left_button_down() {
                return false;
            }
            model.dragging.set(false);
        }
        self.set_count(spec.slots.len());
        without_actions(|| {
            let entries = self.entries.borrow();
            for (index, (entry, slot)) in entries.iter().zip(&spec.slots).enumerate() {
                if entry.item.isCollapsed() != slot.collapsed {
                    entry.item.setCollapsed(slot.collapsed);
                }
                let next = next_shown(&spec.slots, index);
                let grip = next.map_or(0.0, |_| spec.grips.get(index).copied().unwrap_or(0.0));
                entry.pane.view.set_shift(0.0, 0.0);
                entry.pane.set_grip(
                    self.view.view(),
                    index,
                    next.unwrap_or(index + 1),
                    model.direction,
                    grip,
                );
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
        let direction = self.view.rust().direction;
        while entries.len() < count {
            let pane = Pane::with_mtm(mtm);
            pane.view.ivars().axis.set(direction);
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

    /// Точка события вдоль оси сплита в его точках.
    fn along(&self, event: &NSEvent) -> f64 {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        match self.direction() {
            SplitDirection::Columns => point.x,
            SplitDirection::Rows => point.y,
        }
    }

    /// Содержимое элемента `divider` и следующего показанного элемента `next` идёт за мышью на
    /// `delta` точек.
    fn shift(&self, divider: usize, next: usize, delta: f64) {
        let panes = self.arrangedSubviews();
        let pane = |index: usize| {
            (index < panes.count())
                .then(|| panes.objectAtIndex(index))
                .and_then(|view| view.downcast::<PaneView>().ok())
        };
        without_actions(|| {
            if let Some(before) = pane(divider) {
                before.set_shift(before.ivars().lead.get(), delta);
            }
            if let Some(after) = pane(next) {
                after.set_shift(delta, after.ivars().trail.get());
            }
        });
    }

    fn direction(&self) -> SplitDirection {
        if self.isVertical() {
            SplitDirection::Columns
        } else {
            SplitDirection::Rows
        }
    }

    /// Начало и длина элементов вдоль оси.
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
        // реализует NSSplitViewController. Пустой прямоугольник выключает собственное
        // перетаскивание разделителя: его ведёт полоса захвата.
        #[unsafe(method(splitView:effectiveRect:forDrawnRect:ofDividerAtIndex:))]
        fn effective_rect(
            &self,
            _split_view: &NSSplitView,
            _proposed: NSRect,
            _drawn: NSRect,
            _index: NSInteger,
        ) -> NSRect {
            NSRect::ZERO
        }
    }
);

impl ShellioSplitController {
    fn new(mtm: MainThreadMarker) -> Retained<ShellioSplitController> {
        let this = ShellioSplitController::alloc(mtm).set_ivars(ControllerIvars {
            _live: SPLIT_CONTROLLER.live(),
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
            if let Some(grip) = self.view.ivars().grip.borrow().as_ref() {
                pane.addSubview_positioned_relativeTo(grip, NSWindowOrderingMode::Above, None);
            }
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
        self.view.relayout();
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

    /// Полоса захвата разделителя `divider` сплита `split` шириной `width` у конечного края
    /// содержимого элемента, поверх детей; 0 — полосы нет. `next` — следующий показанный элемент,
    /// с которым полоса двигает границу.
    fn set_grip(
        &self,
        split: &ShellioSplitView,
        divider: usize,
        next: usize,
        direction: SplitDirection,
        width: f64,
    ) {
        {
            let mut slot = self.view.ivars().grip.borrow_mut();
            if width <= 0.0 {
                if let Some(grip) = slot.as_ref() {
                    grip.setHidden(true);
                }
                return;
            }
            let grip = slot.get_or_insert_with(|| {
                let grip = SplitGrip::new(self.view.mtm(), split, divider, direction);
                self.view.addSubview(&grip);
                grip
            });
            grip.ivars().width.set(width);
            grip.ivars().next.set(next);
            grip.setHidden(false);
        }
        self.view.place_grip();
    }

    /// Контейнер для `ui.inspect`.
    pub fn inspect(&self, id: &str, class: &'static str, parent: Option<&str>) -> InspectNode {
        inspect::node(id, class, parent, &self.view, None, None)
    }
}

struct PaneIvars {
    _live: Live,
    children: RefCell<Vec<(rc::Weak<NSView>, Insets)>>,
    /// Ось сплита, в элементе которого стоит контейнер.
    axis: Cell<SplitDirection>,
    /// Сдвиг начального и конечного края детей вдоль оси, пока тянется разделитель.
    lead: Cell<f64>,
    trail: Cell<f64>,
    /// Полоса захвата разделителя после этого элемента.
    grip: RefCell<Option<Retained<SplitGrip>>>,
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
            axis: Cell::new(SplitDirection::Columns),
            lead: Cell::new(0.0),
            trail: Cell::new(0.0),
            grip: RefCell::new(None),
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

    /// Сдвиг краёв детей вдоль оси.
    fn set_shift(&self, lead: f64, trail: f64) {
        let parts = self.ivars();
        if parts.lead.replace(lead) != lead || parts.trail.replace(trail) != trail {
            self.relayout();
        }
    }

    /// Дети встают по своим отступам со сдвигом перетаскивания; дети, переставленные в другой вид,
    /// забываются.
    fn relayout(&self) {
        let size = self.bounds().size;
        let (axis, lead, trail) = {
            let parts = self.ivars();
            (parts.axis.get(), parts.lead.get(), parts.trail.get())
        };
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
            let insets = insets.shifted(axis, lead, trail);
            let frame = ns_rect(insets.frame_in(size.width, size.height));
            if child.frame() != frame {
                child.setFrame(frame);
            }
        }
        self.place_grip();
    }

    /// Полоса захвата у конечного края вдоль оси сплита.
    fn place_grip(&self) {
        let size = self.bounds().size;
        let grip = self.ivars().grip.borrow();
        let Some(grip) = grip.as_ref() else {
            return;
        };
        let parts = grip.ivars();
        let frame = grip_frame(parts.direction, parts.width.get(), size.width, size.height);
        let frame = ns_rect(frame);
        if grip.frame() != frame {
            grip.setFrame(frame);
        }
    }
}

/// Кадр полосы захвата шириной `width` у конечного края содержимого `width_of`×`height_of`.
fn grip_frame(direction: SplitDirection, width: f64, width_of: f64, height_of: f64) -> Frame {
    let width = width.min(match direction {
        SplitDirection::Columns => width_of,
        SplitDirection::Rows => height_of,
    });
    match direction {
        SplitDirection::Columns => Frame::new(width_of - width, 0.0, width, height_of),
        SplitDirection::Rows => Frame::new(0.0, height_of - width, width_of, width),
    }
}

/// Перетаскивание полосы: точка нажатия вдоль оси, следующий показанный элемент и его начало,
/// границы сдвига — до начала элемента полосы и до конца следующего показанного.
#[derive(Clone, Copy)]
struct GripDrag {
    at: f64,
    next: usize,
    start: f64,
    min: f64,
    max: f64,
}

impl GripDrag {
    fn delta(self, along: f64) -> f64 {
        (along - self.at).clamp(self.min, self.max)
    }
}

struct GripIvars {
    _live: Live,
    split: rc::Weak<ShellioSplitView>,
    divider: usize,
    /// Следующий показанный элемент по последней установке сплита.
    next: Cell<usize>,
    direction: SplitDirection,
    width: Cell<f64>,
    drag: Cell<Option<GripDrag>>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioSplitGrip"]
    #[ivars = GripIvars]
    struct SplitGrip;

    impl SplitGrip {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор, аргумент и результат — как у `acceptsFirstMouse:` NSView.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `mouseDownCanMoveWindow` NSView.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        // SAFETY: селектор — как у `resetCursorRects` NSView.
        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let cursor = match self.ivars().direction {
                SplitDirection::Columns => NSCursor::columnResizeCursor(),
                SplitDirection::Rows => NSCursor::rowResizeCursor(),
            };
            self.addCursorRect_cursor(self.bounds(), &cursor);
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let parts = self.ivars();
            let Some(split) = parts.split.load() else {
                return;
            };
            let spans = split.spans();
            let next = parts.next.get();
            let (Some(&(previous, _)), Some(&(start, length))) =
                (spans.get(parts.divider), spans.get(next))
            else {
                return;
            };
            let end = start + length;
            parts.drag.set(Some(GripDrag {
                at: split.along(event),
                next,
                start,
                min: previous - start,
                max: end - start,
            }));
            split.ivars().owner.call(|model| model.dragging.set(true));
        }

        // SAFETY: селектор и аргумент — как у `mouseDragged:` NSResponder.
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let parts = self.ivars();
            if let (Some(split), Some(drag)) = (parts.split.load(), parts.drag.get()) {
                split.shift(parts.divider, drag.next, drag.delta(split.along(event)));
            }
        }

        // SAFETY: селектор и аргумент — как у `mouseUp:` NSResponder.
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let parts = self.ivars();
            let (Some(split), Some(drag)) = (parts.split.load(), parts.drag.take()) else {
                return;
            };
            let delta = drag.delta(split.along(event));
            let end = drag_end(parts.divider, drag.start, delta);
            if end.is_none() {
                split.shift(parts.divider, drag.next, 0.0);
            }
            split.ivars().owner.call(|model| {
                model.dragging.set(false);
                if let Some(end) = end {
                    (model.on_drag)(end);
                }
            });
        }
    }
);

impl SplitGrip {
    fn new(
        mtm: MainThreadMarker,
        split: &ShellioSplitView,
        divider: usize,
        direction: SplitDirection,
    ) -> Retained<SplitGrip> {
        let this = SplitGrip::alloc(mtm).set_ivars(GripIvars {
            _live: GRIP_VIEW.live(),
            split: rc::Weak::new(split),
            divider,
            next: Cell::new(divider + 1),
            direction,
            width: Cell::new(0.0),
            drag: Cell::new(None),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] }
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
    fn drag_end_names_the_divider_and_its_new_position() {
        assert_eq!(
            drag_end(1, 614.0, 36.0),
            Some(SplitDrag {
                divider: 1,
                position: 650.0
            })
        );
        assert_eq!(
            drag_end(0, 472.0, -80.0),
            Some(SplitDrag {
                divider: 0,
                position: 392.0
            })
        );
        assert_eq!(
            drag_end(0, 300.0, 0.2),
            None,
            "сдвиг меньше половины точки — не перетаскивание"
        );
    }

    #[test]
    fn shift_moves_only_the_edges_along_the_axis() {
        let insets = Insets {
            left: 6.0,
            top: 6.0,
            right: 6.0,
            bottom: 8.0,
        };
        assert_eq!(
            insets.shifted(SplitDirection::Columns, 0.0, -40.0),
            Insets {
                right: 46.0,
                ..insets
            },
            "конечный край элемента до разделителя идёт влево"
        );
        assert_eq!(
            insets.shifted(SplitDirection::Columns, -40.0, 0.0),
            Insets {
                left: -34.0,
                ..insets
            },
            "начальный край элемента после разделителя идёт влево"
        );
        assert_eq!(
            insets.shifted(SplitDirection::Rows, 12.0, 30.0),
            Insets {
                top: 18.0,
                bottom: -22.0,
                ..insets
            }
        );
        assert_eq!(insets.shifted(SplitDirection::Rows, 0.0, 0.0), insets);
    }

    #[test]
    fn grip_moves_the_boundary_with_the_next_shown_slot() {
        let slot = |collapsed: bool| SplitSlot {
            start: 0.0,
            length: 0.0,
            collapsed,
        };
        // Колонка навигатора, плитка левой зоны, пустой столбец центра, правая зона.
        let slots = [slot(false), slot(false), slot(true), slot(false)];
        assert_eq!(next_shown(&slots, 0), Some(1));
        assert_eq!(
            next_shown(&slots, 1),
            Some(3),
            "свёрнутый столбец пропускается"
        );
        assert_eq!(next_shown(&slots, 2), Some(3));
        let slots = [slot(false), slot(false), slot(false), slot(true)];
        assert_eq!(
            next_shown(&slots, 2),
            None,
            "после столбца ничего не показано"
        );
        assert_eq!(next_shown(&slots, 3), None);
    }

    #[test]
    fn grip_lies_at_the_trailing_edge_along_the_axis() {
        assert_eq!(
            grip_frame(SplitDirection::Columns, 6.0, 472.0, 600.0),
            Frame::new(466.0, 0.0, 6.0, 600.0)
        );
        assert_eq!(
            grip_frame(SplitDirection::Rows, 6.0, 900.0, 300.0),
            Frame::new(0.0, 294.0, 900.0, 6.0)
        );
        assert_eq!(
            grip_frame(SplitDirection::Columns, 6.0, 4.0, 10.0),
            Frame::new(0.0, 0.0, 4.0, 10.0),
            "не шире содержимого"
        );
    }
}
