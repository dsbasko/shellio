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
    NSFontAttributeName, NSForegroundColorAttributeName, NSLineBreakMode, NSResponder, NSText,
    NSTextField, NSTextFieldDelegate, NSView, NSViewLayerContentsRedrawPolicy,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSNotification, NSPoint, NSRect, NSSize, NSString,
};
use shellio_theme::AppTheme;

use crate::app::App;
use crate::counters::{Live, counter};
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
        let mtm = app.mtm();
        let model = FieldModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            shown: RefCell::new(TextFieldState::default()),
            on_intent: Box::new(on_intent),
        };
        let mut view = View::new(&FIELD_OWNER, model, "TextField.updateLayer", |owner| {
            FieldBox::new(mtm, owner)
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
    pub fn inspect(&self, id: &'static str, parent: Option<&'static str>) -> Vec<InspectNode> {
        let field = self.field();
        let state = NodeState::TextField {
            state: self.state(),
            editing: self.is_editing(),
        };
        let mut node = inspect::node(
            id,
            "TextField",
            parent,
            self.view.view(),
            Some(field.isEnabled()),
            Some(state),
        );
        if let Some(cell) = field.cell() {
            node.facts.role = inspect::text(cell.accessibilityRole());
            node.facts.label = inspect::text(cell.accessibilityLabel())
                .or_else(|| inspect::text(cell.accessibilityTitle()));
            node.facts.help = inspect::text(cell.accessibilityHelp());
        }
        node.facts.first_responder = editor_is_first_responder(field);
        vec![node]
    }

    /// Подложка поля для размещения в иерархии.
    #[expect(dead_code, reason = "поле размещает галерея в задаче 8 плана P1.3")]
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
            let line = if self.ivars().field.currentEditor().is_some() {
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
    fn new(mtm: MainThreadMarker, owner: Callback<FieldModel>) -> Retained<FieldBox> {
        let field = FieldControl::new(mtm);
        let this = FieldBox::alloc(mtm).set_ivars(BoxIvars {
            _live: FIELD_BOX.live(),
            owner,
            field,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<FieldBox> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        view.addSubview(&view.ivars().field);
        view
    }

    /// Поле по середине высоты подложки с горизонтальными отступами.
    fn place(&self) {
        let field = &self.ivars().field;
        let bounds = self.bounds();
        let height = field.intrinsicContentSize().height.ceil();
        let frame = NSRect::new(
            NSPoint::new(FIELD_PAD_X, ((bounds.size.height - height) / 2.0).floor()),
            NSSize::new((bounds.size.width - 2.0 * FIELD_PAD_X).max(0.0), height),
        );
        without_actions(|| field.setFrame(frame));
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
