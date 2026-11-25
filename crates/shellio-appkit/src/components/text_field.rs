//! `TextField` — однострочное поле ввода на `NSTextField` (ADR-0002, таблица «Компоненты» плана
//! P1.3): значение, плейсхолдер и редактируемость, намерения `changed`, `submitted` и `cancelled`.
//!
//! Чистая часть — состояние, разница состояний в операции и перевод команд field editor в
//! намерения; её тестирует `make check`. Вид — подложка поля на токенах (`.field` прототипа:
//! заливка `surface`, линия `line-2`, у поля в фокусе — линия `nav-line` в две точки) и системный
//! `NSTextField` внутри без собственной рамки и кольца фокуса. Ввод, выделение и IME — системные:
//! во время ввода first responder — field editor поля, а элемент доступности поля — его ячейка
//! (находка 2 P0.2).

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSControl, NSControlTextEditingDelegate, NSEvent, NSFocusRingType, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSImageView, NSLineBreakMode, NSResponder,
    NSText, NSTextField, NSTextFieldDelegate, NSView, NSViewLayerContentsRedrawPolicy,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSNotification, NSPoint, NSRect, NSSize, NSString,
};
use shellio_theme::AppTheme;

use crate::app::App;
use crate::counters::{Live, counter};
use crate::icon;
use crate::inspect::{self, InspectNode, NodeState};
use crate::layer::{self, without_actions};
use crate::own::{Callback, ComponentId, View};
use crate::runloop;
use crate::surface::LINE_WIDTH;

counter!(FIELD_OWNER, Owner, "TextField");
counter!(FIELD_BOX, Class, "ShellioTextFieldBox");
counter!(FIELD_CONTROL, Class, "ShellioTextField");
counter!(FIELD_DELEGATE, Class, "ShellioTextFieldDelegate");

/// Внутренний отступ текста от линии поля по горизонтали и по вертикали
/// (`.field { padding: 6px 8px }`).
pub const FIELD_PAD_X: f64 = 8.0;
pub const FIELD_PAD_Y: f64 = 6.0;
/// Линия поля в фокусе: граница и кольцо `box-shadow: 0 0 0 1px` прототипа внутри рамки.
pub const FOCUS_LINE_WIDTH: f64 = 2.0;

/// Отступ текста строки фильтра от её края по вертикали (`.pfilter { padding: 3px 8px }`).
pub const FILTER_PAD_Y: f64 = 3.0;
/// Сторона значка строки фильтра и промежуток между ним и текстом (`.pfilter { gap: 6px }`).
pub const FILTER_ICON: f64 = 13.0;
pub const FILTER_GAP: f64 = 6.0;
/// Ширина текста строки фильтра (`.pfilter input { width: 170px }`).
pub const FILTER_INPUT_WIDTH: f64 = 170.0;
/// Плейсхолдер и имя строки фильтра для доступности.
pub const FILTER_PLACEHOLDER: &str = "Фильтр";
/// Имя значка строки фильтра в наборе прототипа.
const FILTER_ICON_NAME: &str = "filter";

/// Вид поля: одна подложка и один `NSTextField`, разные отступы и состав.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextFieldKind {
    /// Поле формы (`.field` прототипа).
    #[default]
    Field,
    /// Строка фильтра панели (`.pfilter` прототипа): значок перед текстом, линия подложки в
    /// фокусе прежняя.
    Filter,
}

/// Размеры вида поля.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldMetrics {
    pub pad_x: f64,
    pub pad_y: f64,
    /// Сторона значка перед текстом; ноль — значка нет.
    pub icon: f64,
    /// Промежуток между значком и текстом.
    pub gap: f64,
    /// Линия подложки в фокусе меняется на линию навигации.
    pub focus_line: bool,
}

impl TextFieldKind {
    pub const fn metrics(self) -> FieldMetrics {
        match self {
            TextFieldKind::Field => FieldMetrics {
                pad_x: FIELD_PAD_X,
                pad_y: FIELD_PAD_Y,
                icon: 0.0,
                gap: 0.0,
                focus_line: true,
            },
            TextFieldKind::Filter => FieldMetrics {
                pad_x: FIELD_PAD_X,
                pad_y: FILTER_PAD_Y,
                icon: FILTER_ICON,
                gap: FILTER_GAP,
                focus_line: false,
            },
        }
    }

    /// Начало и ширина текста в подложке шириной `width`.
    pub fn text_span(self, width: f64) -> (f64, f64) {
        let metrics = self.metrics();
        let start = metrics.pad_x + metrics.icon + metrics.gap;
        (start, (width - start - metrics.pad_x).max(0.0))
    }
}

/// Размер строки фильтра: отступы, значок, промежуток и текст шириной [`FILTER_INPUT_WIDTH`];
/// высота — одна строка текста с отступами. Линия подложки лежит внутри этого размера.
pub fn filter_size(theme: &AppTheme) -> (f64, f64) {
    let metrics = TextFieldKind::Filter.metrics();
    (
        2.0 * metrics.pad_x + metrics.icon + metrics.gap + FILTER_INPUT_WIDTH,
        theme.metrics.line_height.body + 2.0 * metrics.pad_y,
    )
}

/// Состояние поля.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextFieldState {
    pub value: String,
    pub placeholder: String,
    pub editable: bool,
}

/// Операция над видом поля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextFieldOp {
    Value(String),
    Placeholder(String),
    Editable(bool),
}

/// Намерение поля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextFieldIntent {
    /// Пользователь изменил текст: новое значение.
    Changed(String),
    /// `Enter`: значение на момент нажатия.
    Submitted(String),
    /// `Escape`.
    Cancelled,
}

impl TextFieldState {
    /// Операции, которые переводят вид из `self` в `next`. Одинаковое значение операции не даёт:
    /// эхо владельца после `changed` не трогает field editor и каретку.
    pub fn diff(&self, next: &TextFieldState) -> Vec<TextFieldOp> {
        let mut ops = Vec::new();
        if self.value != next.value {
            ops.push(TextFieldOp::Value(next.value.clone()));
        }
        if self.placeholder != next.placeholder {
            ops.push(TextFieldOp::Placeholder(next.placeholder.clone()));
        }
        if self.editable != next.editable {
            ops.push(TextFieldOp::Editable(next.editable));
        }
        ops
    }

    /// Пользователь изменил текст, и вид уже показывает `value`. Состояние следует виду;
    /// намерение `changed` — только если значение действительно другое.
    pub fn edited(&mut self, value: &str) -> Option<TextFieldIntent> {
        if self.value == value {
            return None;
        }
        value.clone_into(&mut self.value);
        Some(TextFieldIntent::Changed(self.value.clone()))
    }

    /// Команда field editor: `Enter` — `insertNewline:`, `Escape` — `cancelOperation:`. `None` —
    /// команду выполняет AppKit. Нередактируемое поле намерений не даёт.
    pub fn command(&self, command: &str) -> Option<TextFieldIntent> {
        if !self.editable {
            return None;
        }
        match command {
            "insertNewline:" => Some(TextFieldIntent::Submitted(self.value.clone())),
            "cancelOperation:" => Some(TextFieldIntent::Cancelled),
            _ => None,
        }
    }
}

/// Высота поля с одной строкой текста, отступами и линией.
pub fn preferred_height(theme: &AppTheme) -> f64 {
    theme.metrics.line_height.body + 2.0 * (FIELD_PAD_Y + LINE_WIDTH)
}

/// Rust-часть поля: состояние, которое показывает вид, тема и получатель намерений.
struct FieldModel {
    id: ComponentId,
    kind: TextFieldKind,
    theme: Cell<AppTheme>,
    shown: RefCell<TextFieldState>,
    on_intent: Box<dyn Fn(TextFieldIntent)>,
}

impl FieldModel {
    fn user_edit(&self, value: &str) {
        let intent = self.shown.borrow_mut().edited(value);
        if let Some(intent) = intent {
            (self.on_intent)(intent);
        }
    }

    /// `true` — команда обработана и AppKit её не выполняет.
    fn command(&self, command: &str, value: &str) -> bool {
        let intent = {
            let mut shown = self.shown.borrow_mut();
            // Значение на момент команды — то, что видит пользователь.
            value.clone_into(&mut shown.value);
            shown.command(command)
        };
        match intent {
            Some(intent) => {
                (self.on_intent)(intent);
                true
            }
            None => false,
        }
    }
}

/// Поле ввода на главном потоке. Освобождение снимает делегата и убирает вид из иерархии.
pub struct TextField {
    view: View<FieldBox, FieldModel>,
}

impl TextField {
    /// `label` — имя поля для доступности. `on_intent` вызывается на главном потоке изнутри
    /// AppKit и не должен синхронно освобождать поле.
    pub fn new(
        app: &App,
        theme: &AppTheme,
        state: TextFieldState,
        label: &str,
        on_intent: impl Fn(TextFieldIntent) + 'static,
    ) -> TextField {
        TextField::of_kind(app, theme, TextFieldKind::Field, state, label, on_intent)
    }

    /// Строка фильтра панели с текстом `value`: редактируемое поле вида
    /// [`TextFieldKind::Filter`], размер — [`filter_size`]. `on_intent` — как у [`TextField::new`].
    pub fn filter(
        app: &App,
        theme: &AppTheme,
        value: &str,
        on_intent: impl Fn(TextFieldIntent) + 'static,
    ) -> TextField {
        let state = TextFieldState {
            value: value.to_owned(),
            placeholder: FILTER_PLACEHOLDER.to_owned(),
            editable: true,
        };
        TextField::of_kind(
            app,
            theme,
            TextFieldKind::Filter,
            state,
            FILTER_PLACEHOLDER,
            on_intent,
        )
    }

    fn of_kind(
        app: &App,
        theme: &AppTheme,
        kind: TextFieldKind,
        state: TextFieldState,
        label: &str,
        on_intent: impl Fn(TextFieldIntent) + 'static,
    ) -> TextField {
        let mtm = app.mtm();
        let model = FieldModel {
            id: ComponentId::next(),
            kind,
            theme: Cell::new(*theme),
            shown: RefCell::new(TextFieldState::default()),
            on_intent: Box::new(on_intent),
        };
        let mut view = View::new(&FIELD_OWNER, model, "TextField.updateLayer", |owner| {
            FieldBox::new(mtm, owner, kind)
        });
        let field = view.view().ivars().field.clone();
        let delegate = FieldDelegate::new(mtm, view.callback("TextField.delegate"));
        // SAFETY: делегат реализует только методы протокола; его держит владелец до снятия.
        unsafe { field.setDelegate(Some(ProtocolObject::from_ref(&*delegate))) };
        if let Some(cell) = field.cell() {
            cell.setAccessibilityLabel(Some(&NSString::from_str(label)));
        }
        view.on_detach(move || {
            // SAFETY: снятие делегата до его освобождения.
            unsafe { field.setDelegate(None) };
            drop(delegate);
        });
        let text_field = TextField { view };
        text_field.apply_theme();
        text_field.set_state(state);
        text_field
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn kind(&self) -> TextFieldKind {
        self.view.rust().kind
    }

    /// Прячет или показывает поле без неявных анимаций. Скрытое поле ввод не держит.
    pub fn set_hidden(&self, hidden: bool) {
        let view = self.view.view();
        if view.isHidden() == hidden {
            return;
        }
        if hidden && self.is_editing() {
            let field = self.field();
            field.abortEditing();
            // Прерванный ввод отбрасывает текст field editor: поле получает значение состояния.
            let value = NSString::from_str(&self.view.rust().shown.borrow().value);
            field.setStringValue(&value);
        }
        without_actions(|| view.setHidden(hidden));
    }

    /// Состояние, которое показывает вид, включая ввод пользователя.
    pub fn state(&self) -> TextFieldState {
        self.view.rust().shown.borrow().clone()
    }

    /// Новое состояние: вид получает только операции разницы.
    pub fn set_state(&self, next: TextFieldState) {
        let model = self.view.rust();
        let ops = model.shown.borrow().diff(&next);
        *model.shown.borrow_mut() = next;
        for op in &ops {
            self.apply(op);
        }
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        let model = self.view.rust();
        if model.theme.get() == *theme {
            return;
        }
        model.theme.set(*theme);
        self.apply_theme();
    }

    /// Ставит фокус в поле: first responder становится field editor поля. `false` — поле не в
    /// окне или окно отказало.
    pub fn focus(&self) -> bool {
        let field = self.field();
        field
            .window()
            .is_some_and(|window| window.makeFirstResponder(Some(field)))
    }

    /// Идёт ли ввод: у поля есть field editor.
    pub fn is_editing(&self) -> bool {
        self.field().currentEditor().is_some()
    }

    /// Поле для `ui.inspect`: кадр подложки, AX-роль и имя ячейки, first responder — field editor
    /// поля во время ввода, состояние.
    ///
    /// Строка фильтра — три узла: подложка `<id>` класса `FilterBar`, значок `<id>.icon` и текст
    /// `<id>.input` класса `FilterInput` с состоянием поля; AX и first responder несёт текст.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        let field = self.field();
        let state = NodeState::TextField {
            state: self.state(),
            editing: self.is_editing(),
        };
        let enabled = Some(field.isEnabled());
        let whole = self.view.view();
        let (mut nodes, mut node) = match self.kind() {
            TextFieldKind::Field => (
                Vec::new(),
                inspect::node(id, "TextField", parent, whole, enabled, Some(state)),
            ),
            TextFieldKind::Filter => {
                let mut nodes = vec![inspect::node(id, "FilterBar", parent, whole, None, None)];
                if let Some(icon) = &whole.ivars().icon {
                    let color = icon
                        .contentTintColor()
                        .and_then(|color| crate::style::rgba(&color));
                    nodes.push(
                        inspect::node(&format!("{id}.icon"), "Icon", Some(id), icon, None, None)
                            .with_icon(FILTER_ICON_NAME, FILTER_ICON, color),
                    );
                }
                let input = inspect::node(
                    &format!("{id}.input"),
                    "FilterInput",
                    Some(id),
                    field,
                    enabled,
                    Some(state),
                );
                (nodes, input)
            }
        };
        if let Some(cell) = field.cell() {
            node.facts.role = inspect::text(cell.accessibilityRole());
            node.facts.label = inspect::text(cell.accessibilityLabel())
                .or_else(|| inspect::text(cell.accessibilityTitle()));
            node.facts.help = inspect::text(cell.accessibilityHelp());
        }
        node.facts.first_responder = editor_is_first_responder(field);
        nodes.push(node);
        nodes
    }

    /// Подложка поля для размещения в иерархии.
    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    fn field(&self) -> &FieldControl {
        &self.view.view().ivars().field
    }

    fn apply(&self, op: &TextFieldOp) {
        let field = self.field();
        match op {
            TextFieldOp::Value(value) => {
                let value = NSString::from_str(value);
                // Значение во время ввода меняется в field editor: `setStringValue` прервал бы ввод
                // и снял фокус.
                match field.currentEditor() {
                    Some(editor) => editor.setString(&value),
                    None => field.setStringValue(&value),
                }
            }
            TextFieldOp::Placeholder(_) => self.apply_placeholder(),
            TextFieldOp::Editable(editable) => {
                if !editable && field.currentEditor().is_some() {
                    field.abortEditing();
                    // Прерванный ввод отбрасывает текст field editor, а в нём уже значение
                    // состояния: новое от владельца или введённое пользователем.
                    let value = NSString::from_str(&self.view.rust().shown.borrow().value);
                    field.setStringValue(&value);
                }
                field.setEditable(*editable);
                field.setSelectable(*editable);
                self.view.view().setNeedsDisplay(true);
            }
        }
    }

    fn apply_theme(&self) {
        let theme = self.view.rust().theme.get();
        let field = self.field();
        field.setTextColor(Some(&layer::ns_color(theme.colors.text)));
        field.setFont(Some(&NSFont::systemFontOfSize(theme.metrics.font.tab)));
        self.apply_placeholder();
        let view = self.view.view();
        if let Some(icon) = &view.ivars().icon {
            icon.setContentTintColor(Some(&layer::ns_color(theme.colors.text_3)));
        }
        view.setNeedsDisplay(true);
        view.setNeedsLayout(true);
    }

    fn apply_placeholder(&self) {
        let model = self.view.rust();
        let theme = model.theme.get();
        let text = NSString::from_str(&model.shown.borrow().placeholder);
        let color = layer::ns_color(theme.colors.text_3);
        let font = NSFont::systemFontOfSize(theme.metrics.font.tab);
        // SAFETY: ключи атрибутов — неизменяемые строки AppKit, живущие весь процесс.
        let keys = unsafe { [NSForegroundColorAttributeName, NSFontAttributeName] };
        let values: [&AnyObject; 2] = [color.as_ref(), font.as_ref()];
        let attributes = NSDictionary::from_slices(&keys, &values);
        // SAFETY: значения словаря — те, что требуют ключи: `NSColor` и `NSFont`.
        let placeholder = unsafe {
            NSAttributedString::initWithString_attributes(
                NSAttributedString::alloc(),
                &text,
                Some(&attributes),
            )
        };
        self.field()
            .setPlaceholderAttributedString(Some(&placeholder));
    }
}

/// Окно поля держит field editor поля first responder.
fn editor_is_first_responder(field: &NSTextField) -> bool {
    let (Some(editor), Some(window)) = (field.currentEditor(), field.window()) else {
        return false;
    };
    window.firstResponder().is_some_and(|responder| {
        std::ptr::eq(
            std::ptr::from_ref::<NSResponder>(&responder).cast::<u8>(),
            std::ptr::from_ref::<NSText>(&editor).cast::<u8>(),
        )
    })
}

struct BoxIvars {
    _live: Live,
    owner: Callback<FieldModel>,
    kind: TextFieldKind,
    /// Значок перед текстом у вида со значком.
    icon: Option<Retained<NSImageView>>,
    field: Retained<FieldControl>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTextFieldBox"]
    #[ivars = BoxIvars]
    struct FieldBox;

    impl FieldBox {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `wantsUpdateLayer` NSView.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `updateLayer` NSView.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let Some(theme) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                model.theme.get()
            }) else {
                return;
            };
            let colors = &theme.colors;
            let focus_line = self.ivars().kind.metrics().focus_line;
            let line = if focus_line && self.ivars().field.currentEditor().is_some() {
                (colors.accent_nav_line, FOCUS_LINE_WIDTH)
            } else {
                (colors.line_2, LINE_WIDTH)
            };
            if let Some(layer) = self.layer() {
                without_actions(|| {
                    layer::set_fill(&layer, colors.surface);
                    layer::set_line(&layer, Some(line));
                    layer::set_radius(&layer, theme.metrics.radius.control);
                });
            }
        }

        // SAFETY: селектор и результат — как у `layout` NSView.
        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), layout] };
            self.place();
        }

        // SAFETY: селектор и аргумент — как у `resizeSubviewsWithOldSize:` NSView: новый размер
        // подложки от владельца ставит поле на место без ожидания прохода раскладки.
        #[unsafe(method(resizeSubviewsWithOldSize:))]
        fn resize_subviews_with_old_size(&self, old_size: NSSize) {
            // SAFETY: `resizeSubviewsWithOldSize:` у NSView с тем же размером.
            let _: () = unsafe { msg_send![super(self), resizeSubviewsWithOldSize: old_size] };
            self.place();
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. Нажатие в отступ подложки
        // ставит фокус в поле.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            let field = &self.ivars().field;
            if field.isEditable()
                && let Some(window) = self.window()
            {
                window.makeFirstResponder(Some(field));
            }
        }
    }
);

impl FieldBox {
    fn new(
        mtm: MainThreadMarker,
        owner: Callback<FieldModel>,
        kind: TextFieldKind,
    ) -> Retained<FieldBox> {
        let field = FieldControl::new(mtm);
        let size = kind.metrics().icon;
        let icon = (size > 0.0).then(|| {
            let view = NSImageView::initWithFrame(NSImageView::alloc(mtm), NSRect::ZERO);
            let description = NSString::from_str(FILTER_PLACEHOLDER);
            view.setImage(icon::image(FILTER_ICON_NAME, size, &description).as_deref());
            view
        });
        let this = FieldBox::alloc(mtm).set_ivars(BoxIvars {
            _live: FIELD_BOX.live(),
            owner,
            kind,
            icon,
            field,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<FieldBox> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        view.addSubview(&view.ivars().field);
        if let Some(icon) = &view.ivars().icon {
            view.addSubview(icon);
        }
        view
    }

    /// Поле по середине высоты подложки с горизонтальными отступами, значок — перед ним.
    fn place(&self) {
        let ivars = self.ivars();
        let field = &ivars.field;
        let bounds = self.bounds();
        let metrics = ivars.kind.metrics();
        let (start, width) = ivars.kind.text_span(bounds.size.width);
        let height = field.intrinsicContentSize().height.ceil();
        let frame = NSRect::new(
            NSPoint::new(start, ((bounds.size.height - height) / 2.0).floor()),
            NSSize::new(width, height),
        );
        without_actions(|| {
            field.setFrame(frame);
            if let Some(icon) = &ivars.icon {
                icon.setFrame(NSRect::new(
                    NSPoint::new(
                        metrics.pad_x,
                        ((bounds.size.height - metrics.icon) / 2.0).floor(),
                    ),
                    NSSize::new(metrics.icon, metrics.icon),
                ));
            }
        });
    }
}

struct ControlIvars {
    _live: Live,
}

define_class!(
    // SAFETY: NSTextField не требует от подкласса ничего сверх переопределённых ниже методов,
    // `Drop` у класса нет.
    #[unsafe(super(NSTextField, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTextField"]
    #[ivars = ControlIvars]
    struct FieldControl;

    impl FieldControl {
        // SAFETY: селектор и результат — как у `becomeFirstResponder` NSResponder. Начало ввода
        // перерисовывает линию подложки.
        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: `becomeFirstResponder` у NSTextField без аргументов, результат BOOL.
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            if accepted {
                self.redraw_box();
            }
            accepted
        }

        // SAFETY: селектор и аргумент — как у `textDidEndEditing:` NSTextField. Конец ввода
        // перерисовывает линию подложки.
        #[unsafe(method(textDidEndEditing:))]
        fn text_did_end_editing(&self, notification: &NSNotification) {
            // SAFETY: `textDidEndEditing:` у NSTextField с тем же уведомлением.
            let _: () = unsafe { msg_send![super(self), textDidEndEditing: notification] };
            self.redraw_box();
        }
    }
);

impl FieldControl {
    fn new(mtm: MainThreadMarker) -> Retained<FieldControl> {
        let this = FieldControl::alloc(mtm).set_ivars(ControlIvars {
            _live: FIELD_CONTROL.live(),
        });
        // SAFETY: назначенный инициализатор NSTextField (через NSControl), ivars уже заданы.
        let field: Retained<FieldControl> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        // Вид начинает с `TextFieldState::default()`: первое состояние владельца доходит до него
        // разницей, а `NSTextField` из `initWithFrame:` редактируем и выделяем.
        field.setEditable(false);
        field.setSelectable(false);
        field.setBezeled(false);
        field.setBordered(false);
        field.setDrawsBackground(false);
        field.setFocusRingType(NSFocusRingType::None);
        field.setUsesSingleLineMode(true);
        field.setLineBreakMode(NSLineBreakMode::ByClipping);
        if let Some(cell) = field.cell() {
            cell.setScrollable(true);
            cell.setWraps(false);
        }
        field
    }

    fn redraw_box(&self) {
        // SAFETY: `superview` у поля, которое держит подложка.
        if let Some(parent) = unsafe { self.superview() } {
            parent.setNeedsDisplay(true);
        }
    }
}

struct DelegateIvars {
    _live: Live,
    owner: Callback<FieldModel>,
}

define_class!(
    // SAFETY: NSObject не требует от подкласса ничего сверх `init`, `Drop` у класса нет.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTextFieldDelegate"]
    #[ivars = DelegateIvars]
    struct FieldDelegate;

    // SAFETY: протокол NSObject реализует суперкласс NSObject.
    unsafe impl NSObjectProtocol for FieldDelegate {}

    // SAFETY: протокол требует только необязательные методы, ниже они совпадают с его объявлениями.
    unsafe impl NSControlTextEditingDelegate for FieldDelegate {
        // SAFETY: селектор и аргумент — как у `controlTextDidChange:` протокола.
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, notification: &NSNotification) {
            let Some(control) = notification
                .object()
                .and_then(|object| object.downcast::<NSControl>().ok())
            else {
                return;
            };
            let value = control.stringValue().to_string();
            self.ivars().owner.call(|model| model.user_edit(&value));
        }

        // SAFETY: селектор, аргументы и результат — как у
        // `control:textView:doCommandBySelector:` протокола; field editor передаётся как объект.
        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn do_command(&self, control: &NSControl, _editor: &AnyObject, command: Sel) -> bool {
            let value = control.stringValue().to_string();
            let command = command.name().to_string_lossy();
            self.ivars()
                .owner
                .call(|model| model.command(&command, &value))
                .unwrap_or(false)
        }
    }

    // SAFETY: протокол требует только необязательные методы.
    unsafe impl NSTextFieldDelegate for FieldDelegate {}
);

impl FieldDelegate {
    fn new(mtm: MainThreadMarker, owner: Callback<FieldModel>) -> Retained<FieldDelegate> {
        let this = FieldDelegate::alloc(mtm).set_ivars(DelegateIvars {
            _live: FIELD_DELEGATE.live(),
            owner,
        });
        // SAFETY: `init` у NSObject без аргументов, ivars уже заданы.
        unsafe { msg_send![super(this), init] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(value: &str, placeholder: &str, editable: bool) -> TextFieldState {
        TextFieldState {
            value: value.to_owned(),
            placeholder: placeholder.to_owned(),
            editable,
        }
    }

    #[test]
    fn diff_names_only_changed_parts() {
        let before = state("a", "Поиск", true);
        assert!(before.diff(&before).is_empty());
        assert_eq!(
            before.diff(&state("ab", "Поиск", true)),
            [TextFieldOp::Value("ab".into())]
        );
        assert_eq!(
            before.diff(&state("a", "Фильтр", true)),
            [TextFieldOp::Placeholder("Фильтр".into())]
        );
        assert_eq!(
            before.diff(&state("", "", false)),
            [
                TextFieldOp::Value(String::new()),
                TextFieldOp::Placeholder(String::new()),
                TextFieldOp::Editable(false),
            ]
        );
    }

    #[test]
    fn owner_echo_after_typing_gives_no_operations() {
        let mut shown = state("shell", "", true);
        assert_eq!(
            shown.edited("shellio"),
            Some(TextFieldIntent::Changed("shellio".into()))
        );
        let echo = state("shellio", "", true);
        assert!(
            shown.diff(&echo).is_empty(),
            "владелец вернул то же значение: field editor не трогается"
        );
        assert_eq!(
            shown.edited("shellio"),
            None,
            "то же значение — без намерения"
        );
    }

    #[test]
    fn owner_value_during_typing_is_one_operation() {
        let mut shown = state("", "", true);
        let _ = shown.edited("abc");
        assert_eq!(
            shown.diff(&state("ABC", "", true)),
            [TextFieldOp::Value("ABC".into())]
        );
    }

    #[test]
    fn enter_submits_current_value_and_escape_cancels() {
        let shown = state("shellio", "", true);
        assert_eq!(
            shown.command("insertNewline:"),
            Some(TextFieldIntent::Submitted("shellio".into()))
        );
        assert_eq!(
            shown.command("cancelOperation:"),
            Some(TextFieldIntent::Cancelled)
        );
        for other in ["insertTab:", "moveLeft:", "deleteBackward:", "complete:"] {
            assert_eq!(shown.command(other), None, "{other} выполняет AppKit");
        }
    }

    #[test]
    fn view_starts_read_only_like_the_default_state() {
        // `FieldControl::new` начинает нередактируемым: первое состояние доходит разницей от
        // `default()`, и нередактируемое поле операции `Editable` не получает.
        assert!(!TextFieldState::default().editable);
        assert!(
            TextFieldState::default()
                .diff(&state("", "", false))
                .is_empty()
        );
        assert_eq!(
            TextFieldState::default().diff(&state("", "", true)),
            [TextFieldOp::Editable(true)]
        );
    }

    #[test]
    fn read_only_field_has_no_intents() {
        let shown = state("x", "", false);
        assert_eq!(shown.command("insertNewline:"), None);
        assert_eq!(shown.command("cancelOperation:"), None);
    }

    #[test]
    fn form_field_keeps_its_span_and_focus_line() {
        let metrics = TextFieldKind::Field.metrics();
        assert_eq!((metrics.pad_x, metrics.pad_y), (FIELD_PAD_X, FIELD_PAD_Y));
        assert!(metrics.focus_line);
        assert_eq!(
            TextFieldKind::Field.text_span(200.0),
            (FIELD_PAD_X, 200.0 - 2.0 * FIELD_PAD_X)
        );
        assert_eq!(TextFieldKind::default(), TextFieldKind::Field);
    }

    #[test]
    fn filter_bar_puts_text_after_the_icon_and_keeps_the_line() {
        let metrics = TextFieldKind::Filter.metrics();
        assert!(!metrics.focus_line, "линия строки фильтра в фокусе прежняя");
        assert_eq!((metrics.icon, metrics.gap), (FILTER_ICON, FILTER_GAP));
        for cell in shellio_theme::Cell::ALL {
            let theme = AppTheme::for_cell(cell);
            let (width, height) = filter_size(&theme);
            assert_eq!(width, 205.0);
            assert_eq!(height, theme.metrics.line_height.body + 2.0 * FILTER_PAD_Y);
            assert_eq!(
                TextFieldKind::Filter.text_span(width),
                (FIELD_PAD_X + FILTER_ICON + FILTER_GAP, FILTER_INPUT_WIDTH)
            );
        }
        assert_eq!(
            TextFieldKind::Filter.text_span(10.0).1,
            0.0,
            "узкая строка текста не показывает"
        );
        assert!(icon::image(FILTER_ICON_NAME, FILTER_ICON, &NSString::from_str("")).is_some());
        assert!(shellio_theme::icons::shapes(FILTER_ICON_NAME).is_some());
    }

    #[test]
    fn preferred_height_fits_one_line_with_padding_and_line() {
        for cell in shellio_theme::Cell::ALL {
            let theme = AppTheme::for_cell(cell);
            assert_eq!(
                preferred_height(&theme),
                theme.metrics.line_height.body + 2.0 * FIELD_PAD_Y + 2.0 * LINE_WIDTH
            );
        }
    }
}
