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
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSControl, NSControlTextEditingDelegate, NSEvent, NSFocusRingType, NSFont,
    NSFontAttributeName, NSForegroundColorAttributeName, NSImageView, NSLineBreakMode, NSResponder,
    NSText, NSTextField, NSTextFieldDelegate, NSView, NSViewLayerContentsRedrawPolicy,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSNotification, NSPoint, NSRange, NSRect, NSSize, NSString,
};
use objc2_quartz_core::CALayer;
use shellio_theme::{AppTheme, Colors, Srgb8};

use crate::app::App;
use crate::chrome_bar::Frame;
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
/// Поле текста строки фильтра выше строки текста на точку сверху и снизу: кадр `<input>` в
/// паспорте `panel-filter` — 20 точек при межстрочном интервале 18.
pub const FILTER_INPUT_PAD_Y: f64 = 1.0;
/// Сторона значка строки фильтра и промежуток между ним и текстом (`.pfilter { gap: 6px }`).
pub const FILTER_ICON: f64 = 13.0;
pub const FILTER_GAP: f64 = 6.0;
/// Ширина текста строки фильтра (`.pfilter input { width: 170px }`).
pub const FILTER_INPUT_WIDTH: f64 = 170.0;
/// Ширина узла каретки: системный индикатор вставки шире точки.
const CARET_WIDTH: f64 = 2.0;
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
    /// Строка фильтра панели (`.pfilter` прототипа): значок перед текстом, подложка поднята над
    /// плиткой — тени токена и кольцо снаружи кадра вместо линии слоя.
    Filter,
    /// Поле запроса палитры (`.pal-in input` прототипа): без отступов, линии и подложки сверх
    /// заливки поверхности, кегль заголовка.
    Palette,
    /// Поле командной строки (`.cmdline input` прототипа): без отступов, линии и подложки сверх
    /// заливки поверхности, моноширинный шрифт мелкого кегля.
    Command,
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
    /// Подложка поднята: тени токена и кольцо снаружи кадра, линии слоя нет.
    pub raised: bool,
    /// У подложки есть линия и радиус.
    pub line: bool,
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
                raised: false,
                line: true,
            },
            TextFieldKind::Filter => FieldMetrics {
                pad_x: FIELD_PAD_X,
                pad_y: FILTER_PAD_Y,
                icon: FILTER_ICON,
                gap: FILTER_GAP,
                focus_line: false,
                raised: true,
                line: true,
            },
            TextFieldKind::Palette | TextFieldKind::Command => FieldMetrics {
                pad_x: 0.0,
                pad_y: 0.0,
                icon: 0.0,
                gap: 0.0,
                focus_line: false,
                raised: false,
                line: false,
            },
        }
    }

    /// Заливка подложки. Строка фильтра стоит в плитке панели, а там `--surface` прототипа —
    /// цвет панели (`.panel:not(.flat)`).
    pub const fn fill(self, colors: &Colors) -> Srgb8 {
        match self {
            TextFieldKind::Field | TextFieldKind::Palette | TextFieldKind::Command => {
                colors.surface
            }
            TextFieldKind::Filter => colors.panel,
        }
    }

    /// Кегль текста и плейсхолдера. Поле формы — кегль `.field`, строка фильтра наследует кегль
    /// текста панели: паспорт `panel-filter` несёт у `<input>` 13 точек.
    pub const fn font_size(self, theme: &AppTheme) -> f64 {
        match self {
            TextFieldKind::Field => theme.metrics.font.tab,
            TextFieldKind::Filter => theme.metrics.font.body,
            TextFieldKind::Palette => theme.metrics.font.title,
            TextFieldKind::Command => theme.metrics.font.small,
        }
    }

    /// Шрифт текста и плейсхолдера: у командной строки моноширинный (`font: var(--t-sm)
    /// var(--mono)`), у остальных системный.
    fn font(self, theme: &AppTheme) -> Retained<NSFont> {
        let size = self.font_size(theme);
        match self {
            TextFieldKind::Command => crate::font::mono(size),
            TextFieldKind::Field | TextFieldKind::Filter | TextFieldKind::Palette => {
                NSFont::systemFontOfSize(size)
            }
        }
    }

    /// Текст поля буквальный: запрос палитры и команда `:` не получают системных замен ввода —
    /// точки после двойного пробела, типографских кавычек и тире, замен текста и исправления
    /// написания. Поле формы и строка фильтра следуют настройкам системы.
    pub const fn literal(self) -> bool {
        match self {
            TextFieldKind::Palette | TextFieldKind::Command => true,
            TextFieldKind::Field | TextFieldKind::Filter => false,
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
/// высота — поле одной строки текста с отступами. Линия подложки лежит внутри этого размера.
pub fn filter_size(theme: &AppTheme) -> (f64, f64) {
    let metrics = TextFieldKind::Filter.metrics();
    (
        2.0 * metrics.pad_x + metrics.icon + metrics.gap + FILTER_INPUT_WIDTH,
        theme.metrics.line_height.body + 2.0 * (FILTER_INPUT_PAD_Y + metrics.pad_y),
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

    /// Поле запроса палитры с текстом `value` и плейсхолдером `placeholder`, вид
    /// [`TextFieldKind::Palette`]. `on_intent` — как у [`TextField::new`].
    pub fn palette(
        app: &App,
        theme: &AppTheme,
        value: &str,
        placeholder: &str,
        on_intent: impl Fn(TextFieldIntent) + 'static,
    ) -> TextField {
        let state = TextFieldState {
            value: value.to_owned(),
            placeholder: placeholder.to_owned(),
            editable: true,
        };
        TextField::of_kind(
            app,
            theme,
            TextFieldKind::Palette,
            state,
            placeholder,
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

    /// Отдаёт ввод окну, если его держит это поле. `true` — ввод был здесь.
    pub fn resign_focus(&self) -> bool {
        let field = self.field();
        let held = editor_is_first_responder(field);
        if held && let Some(window) = field.window() {
            window.makeFirstResponder(None);
        }
        held
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
            TextFieldKind::Field | TextFieldKind::Palette | TextFieldKind::Command => (
                Vec::new(),
                inspect::node(id, "TextField", parent, whole, enabled, Some(state)),
            ),
            TextFieldKind::Filter => {
                let mut bar = inspect::node(id, "FilterBar", parent, whole, None, None);
                if let Some(raised) = &whole.ivars().raised {
                    bar = bar.with_shadow_layers(&raised.shadows, &raised.ring);
                }
                let mut nodes = vec![bar];
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
        if matches!(self.kind(), TextFieldKind::Palette | TextFieldKind::Command) {
            // Поле палитры и командной строки сверяется с полем паспорта целиком: шрифт, цвет и
            // обрезка — у узла поля, подпись доступности остаётся его именем.
            let label = node.facts.label.take();
            node = node.with_text_of(field);
            node.facts.label = label;
            // Пустое поле показывает плейсхолдер тем же шрифтом: шрифт строки поиска есть и без
            // запроса, как у `<input>` паспорта.
            let style = node.style.get_or_insert_with(Default::default);
            if style.font.is_none() {
                style.font = field.font().as_deref().map(crate::style::font_facts);
            }
        }
        nodes.push(node);
        nodes
    }

    /// Точка вставки поля во время ввода — узел `id` без ключа сверки: прямоугольник каретки
    /// (выделения, если оно не пустое) в системе координат окна поля. Снимок маскирует его в
    /// сравнении стабильности: каретка мигает. Ввода нет — узла нет.
    pub fn caret(&self, id: &str, parent: &str) -> Option<InspectNode> {
        let field = self.field();
        let editor = field.currentEditor()?;
        let window = field.window()?;
        let range = editor.selectedRange();
        // SAFETY: field editor поля — `NSTextView`, он реализует
        // `firstRectForCharacterRange:actualRange:` протокола `NSTextInputClient`: аргументы —
        // `NSRange` и указатель на `NSRange`, который допускает NULL, результат — `NSRect` в
        // координатах экрана.
        let screen: NSRect = unsafe {
            msg_send![
                &*editor,
                firstRectForCharacterRange: range,
                actualRange: std::ptr::null_mut::<NSRange>()
            ]
        };
        let rect = window.convertRectFromScreen(screen);
        let width = rect.size.width.max(CARET_WIDTH);
        let x = rect.origin.x + (rect.size.width - width) / 2.0;
        Some(inspect::rect_node(
            id,
            "Caret",
            parent,
            inspect::object_id(&*editor),
            Frame::new(x, rect.origin.y, width, rect.size.height),
        ))
    }

    /// Поле командной строки с текстом `value` и плейсхолдером `placeholder`, вид
    /// [`TextFieldKind::Command`]. `on_intent` — как у [`TextField::new`].
    pub fn command(
        app: &App,
        theme: &AppTheme,
        value: &str,
        placeholder: &str,
        on_intent: impl Fn(TextFieldIntent) + 'static,
    ) -> TextField {
        let state = TextFieldState {
            value: value.to_owned(),
            placeholder: placeholder.to_owned(),
            editable: true,
        };
        TextField::of_kind(
            app,
            theme,
            TextFieldKind::Command,
            state,
            placeholder,
            on_intent,
        )
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
        field.setFont(Some(&self.kind().font(&theme)));
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
        let font = model.kind.font(&theme);
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

/// Ввод в поле начался или закончился: field editor стал первым ответчиком или перестал им быть.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldFocus {
    Began,
    Ended,
}

type FocusObserver = Rc<dyn Fn(FieldFocus)>;

thread_local! {
    /// Наблюдатель ввода в полях приложения: один на процесс.
    static FIELD_FOCUS: RefCell<Option<FocusObserver>> = const { RefCell::new(None) };
}

impl App {
    /// Ставит наблюдателя ввода в полях: его получает любое поле окна, включая поля тел плагинов.
    /// `handler` вызывается на главном потоке изнутри AppKit, в том числе изнутри
    /// `makeFirstResponder:` самого владельца, и не должен синхронно трогать окно. Новый
    /// наблюдатель заменяет прежнего.
    pub fn on_field_focus(&self, handler: impl Fn(FieldFocus) + 'static) {
        FIELD_FOCUS.with(|slot| *slot.borrow_mut() = Some(Rc::new(handler)));
    }
}

fn notify_focus(focus: FieldFocus) {
    let handler = FIELD_FOCUS.with(|slot| slot.borrow().clone());
    if let Some(handler) = handler {
        handler(focus);
    }
}

/// Редактор поля составляет текст в IME.
fn composing(editor: &AnyObject) -> bool {
    // SAFETY: редактор поля — `NSTextView`, он реализует `hasMarkedText` протокола
    // `NSTextInputClient`: метод без аргументов возвращает BOOL.
    unsafe { msg_send![editor, hasMarkedText] }
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

/// Замена системы «точка после двойного пробела»: второй пробел заменяет первый на `". "`.
/// Выключателя у текстового вида для неё нет: настройку `NSAutomaticPeriodSubstitutionEnabled`
/// читает весь процесс, поэтому буквальное поле распознаёт саму замену.
fn period_substitution(replaced: &str, replacement: &str) -> bool {
    replaced == " " && replacement == ". "
}

/// Текст редактора в диапазоне `range`; диапазон вне текста — пустая строка.
fn replaced_text(editor: &NSText, range: NSRange) -> String {
    let text = editor.string();
    match range.location.checked_add(range.length) {
        Some(end) if end <= text.length() => text.substringWithRange(range).to_string(),
        _ => String::new(),
    }
}

/// Ставит редактору поля системные замены ввода: типографские кавычки и тире, замены текста и
/// исправление написания. `follow_system` — как в настройках системы, иначе выключены. Field
/// editor окна общий у его полей, поэтому каждое поле настраивает его в начале своего ввода.
fn set_substitutions(editor: &NSText, follow_system: bool) {
    let checker = objc2::class!(NSSpellChecker);
    // SAFETY: классовые свойства `NSSpellChecker` без аргументов, результат BOOL.
    let (quotes, dashes, replacement, correction): (bool, bool, bool, bool) = unsafe {
        (
            msg_send![checker, isAutomaticQuoteSubstitutionEnabled],
            msg_send![checker, isAutomaticDashSubstitutionEnabled],
            msg_send![checker, isAutomaticTextReplacementEnabled],
            msg_send![checker, isAutomaticSpellingCorrectionEnabled],
        )
    };
    let (quotes, dashes, replacement, correction) = (
        follow_system && quotes,
        follow_system && dashes,
        follow_system && replacement,
        follow_system && correction,
    );
    // SAFETY: field editor поля — `NSTextView`, сеттеры свойств замен принимают BOOL.
    unsafe {
        let _: () = msg_send![editor, setAutomaticQuoteSubstitutionEnabled: quotes];
        let _: () = msg_send![editor, setAutomaticDashSubstitutionEnabled: dashes];
        let _: () = msg_send![editor, setAutomaticTextReplacementEnabled: replacement];
        let _: () = msg_send![editor, setAutomaticSpellingCorrectionEnabled: correction];
    }
}

/// Слои поднятой подложки под её содержимым по порядку записи `box-shadow`: две тени токена и
/// кольцо снаружи кадра. Слой тени несёт заливку подложки: она закрывает тень внутри кадра.
struct RaisedLayers {
    shadows: [Retained<CALayer>; 2],
    ring: Retained<CALayer>,
}

impl RaisedLayers {
    fn new() -> RaisedLayers {
        let raised = RaisedLayers {
            shadows: [CALayer::new(), CALayer::new()],
            ring: CALayer::new(),
        };
        for layer in raised.layers() {
            // Ниже слоёв поля и значка, в каком бы порядке AppKit их ни держал.
            layer.setZPosition(-1.0);
        }
        raised
    }

    fn layers(&self) -> impl Iterator<Item = &CALayer> {
        self.shadows
            .iter()
            .map(|layer| &**layer)
            .chain(std::iter::once(&*self.ring))
    }

    /// Тени — по кадру подложки, кольцо — на толщину линии шире с каждой стороны.
    fn place(&self, bounds: NSRect) {
        for layer in &self.shadows {
            layer.setFrame(bounds);
        }
        self.ring.setFrame(NSRect::new(
            NSPoint::new(bounds.origin.x - LINE_WIDTH, bounds.origin.y - LINE_WIDTH),
            NSSize::new(
                bounds.size.width + 2.0 * LINE_WIDTH,
                bounds.size.height + 2.0 * LINE_WIDTH,
            ),
        ));
    }
}

/// Красит подложку `layer` вида `kind` и слои поднятой подложки. `editing` — в поле идёт ввод.
fn paint(
    layer: &CALayer,
    raised: Option<&RaisedLayers>,
    kind: TextFieldKind,
    theme: &AppTheme,
    editing: bool,
) {
    let colors = &theme.colors;
    let fill = kind.fill(colors);
    let radius = theme.metrics.radius.control;
    layer::set_fill(layer, fill);
    match raised {
        Some(raised) => {
            layer::set_line(layer, None);
            layer::set_corner(layer, radius);
            for (under, shadow) in raised.shadows.iter().zip(theme.shadow) {
                layer::set_fill(under, fill);
                layer::set_corner(under, radius);
                layer::set_shadow(under, shadow);
            }
            layer::set_line(&raised.ring, Some((colors.line_2, LINE_WIDTH)));
            layer::set_corner(&raised.ring, radius + LINE_WIDTH);
        }
        None if !kind.metrics().line => {
            layer::set_line(layer, None);
            layer::set_radius(layer, 0.0);
        }
        None => {
            let line = if kind.metrics().focus_line && editing {
                (colors.accent_nav_line, FOCUS_LINE_WIDTH)
            } else {
                (colors.line_2, LINE_WIDTH)
            };
            layer::set_line(layer, Some(line));
            layer::set_radius(layer, radius);
        }
    }
}

struct BoxIvars {
    _live: Live,
    owner: Callback<FieldModel>,
    kind: TextFieldKind,
    /// Слои теней и кольца у поднятой подложки.
    raised: Option<RaisedLayers>,
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
            let ivars = self.ivars();
            let editing = ivars.field.currentEditor().is_some();
            if let Some(layer) = self.layer() {
                without_actions(|| {
                    paint(&layer, ivars.raised.as_ref(), ivars.kind, &theme, editing);
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
        let field = FieldControl::new(mtm, kind.literal());
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
            raised: kind.metrics().raised.then(RaisedLayers::new),
            icon,
            field,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<FieldBox> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        if let (Some(raised), Some(layer)) = (&view.ivars().raised, view.layer()) {
            for under in raised.layers() {
                layer.addSublayer(under);
            }
        }
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
            if let Some(raised) = &ivars.raised {
                raised.place(bounds);
            }
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
    /// [`TextFieldKind::literal`] вида поля.
    literal: bool,
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
                if let Some(editor) = self.currentEditor() {
                    set_substitutions(&editor, !self.ivars().literal);
                }
                self.redraw_box();
                notify_focus(FieldFocus::Began);
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
            notify_focus(FieldFocus::Ended);
        }

        // SAFETY: селектор, аргументы и результат — как у
        // `textView:shouldChangeTextInRange:replacementString:` протокола `NSTextViewDelegate`:
        // делегат field editor — само поле, редактор передаётся как `NSText`.
        #[unsafe(method(textView:shouldChangeTextInRange:replacementString:))]
        fn should_change_text(
            &self,
            editor: &NSText,
            range: NSRange,
            replacement: Option<&NSString>,
        ) -> bool {
            if self.ivars().literal
                && let Some(replacement) = replacement
                && period_substitution(&replaced_text(editor, range), &replacement.to_string())
            {
                // Вместо точки вставляется сам набранный пробел: повторный вход в этот метод
                // замены уже не несёт и идёт обычным путём.
                let space = NSString::from_str(" ");
                // SAFETY: field editor поля — `NSTextView`, он реализует
                // `insertText:replacementRange:` протокола `NSTextInputClient`: строка и
                // `NSRange` выделения внутри текста.
                let _: () = unsafe {
                    msg_send![editor, insertText: &*space, replacementRange: editor.selectedRange()]
                };
                false
            } else {
                // SAFETY: NSTextField реализует этот метод делегата field editor с теми же
                // аргументами.
                unsafe {
                    msg_send![
                        super(self),
                        textView: editor,
                        shouldChangeTextInRange: range,
                        replacementString: replacement
                    ]
                }
            }
        }
    }
);

impl FieldControl {
    fn new(mtm: MainThreadMarker, literal: bool) -> Retained<FieldControl> {
        let this = FieldControl::alloc(mtm).set_ivars(ControlIvars {
            _live: FIELD_CONTROL.live(),
            literal,
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
        fn do_command(&self, control: &NSControl, editor: &AnyObject, command: Sel) -> bool {
            let value = control.stringValue().to_string();
            let name = command.name().to_string_lossy();
            let handled = self
                .ivars()
                .owner
                .call(|model| model.command(&name, &value))
                .unwrap_or(false);
            // `Esc` после отказа от черновика отдаёт ввод окну: панель возвращается в `Navigation`
            // (ADR-0010). Составление текста в IME `Esc` отменяет само и ввод не отдаёт.
            if handled
                && name == "cancelOperation:"
                && !composing(editor)
                && let Some(window) = control.window()
            {
                window.makeFirstResponder(None);
            }
            handled
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
    fn only_palette_and_command_fields_are_literal() {
        assert!(TextFieldKind::Palette.literal());
        assert!(TextFieldKind::Command.literal());
        assert!(!TextFieldKind::Field.literal());
        assert!(!TextFieldKind::Filter.literal());
    }

    #[test]
    fn period_substitution_is_a_space_replaced_by_period_and_space() {
        assert!(period_substitution(" ", ". "));
        // Набранная точка и обычная вставка текста заменой не являются.
        assert!(!period_substitution("", ". "));
        assert!(!period_substitution("", " "));
        assert!(!period_substitution("a", ". "));
        assert!(!period_substitution(" ", "  "));
        assert!(!period_substitution("  ", ". "));
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
    fn palette_query_spans_the_whole_box_with_the_title_size() {
        let metrics = TextFieldKind::Palette.metrics();
        assert!(!metrics.line && !metrics.raised && !metrics.focus_line);
        assert_eq!(TextFieldKind::Palette.text_span(545.0), (0.0, 545.0));
        let theme = AppTheme::for_cell(shellio_theme::Cell::ALL[0]);
        assert_eq!(
            TextFieldKind::Palette.font_size(&theme),
            theme.metrics.font.title
        );
        assert_eq!(
            TextFieldKind::Palette.fill(&theme.colors),
            theme.colors.surface
        );
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
            assert_eq!(height, 26.0, "высота подложки в паспорте `panel-filter`");
            assert_eq!(
                height,
                theme.metrics.line_height.body + 2.0 * (FILTER_INPUT_PAD_Y + FILTER_PAD_Y)
            );
            assert_eq!(
                TextFieldKind::Filter.text_span(width),
                (FIELD_PAD_X + FILTER_ICON + FILTER_GAP, FILTER_INPUT_WIDTH)
            );
            assert_eq!(
                TextFieldKind::Filter.font_size(&theme),
                13.0,
                "кегль `<input>` в паспорте `panel-filter`"
            );
            assert_eq!(
                TextFieldKind::Field.font_size(&theme),
                12.5,
                "кегль `.field`"
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

    /// Подложка строки фильтра на слоях без вида: то, что отдаёт `ui.inspect`, против значений
    /// паспорта `panel-filter` — заливка панели, линии слоя нет, две тени токена и кольцо.
    #[test]
    fn raised_filter_bar_reports_panel_fill_no_line_and_three_shadows() {
        use shellio_theme::{Appearance, Cell as ThemeCell, Contrast};

        use crate::style::{self, Rgba, ShadowFacts};

        let rgb = |r, g, b, a| Rgba { r, g, b, a };
        let cases = [
            (
                (Appearance::Light, Contrast::Normal),
                rgb(255, 255, 255, 1.0),
                [rgb(20, 22, 30, 0.18), rgb(20, 22, 30, 0.10)],
                rgb(201, 196, 186, 1.0),
            ),
            (
                (Appearance::Light, Contrast::Increased),
                rgb(255, 255, 255, 1.0),
                [rgb(20, 22, 30, 0.18), rgb(20, 22, 30, 0.10)],
                rgb(111, 107, 98, 1.0),
            ),
            (
                (Appearance::Dark, Contrast::Normal),
                rgb(10, 9, 8, 1.0),
                [rgb(0, 0, 0, 0.55), rgb(0, 0, 0, 0.4)],
                rgb(64, 61, 57, 1.0),
            ),
            (
                (Appearance::Dark, Contrast::Increased),
                rgb(10, 9, 8, 1.0),
                [rgb(0, 0, 0, 0.55), rgb(0, 0, 0, 0.4)],
                rgb(127, 122, 112, 1.0),
            ),
        ];
        for ((appearance, contrast), fill, shadows, ring) in cases {
            let theme = AppTheme::for_cell(ThemeCell::new(appearance, contrast));
            let layer = CALayer::new();
            let raised = RaisedLayers::new();
            paint(&layer, Some(&raised), TextFieldKind::Filter, &theme, true);
            raised.place(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(205.0, 26.0),
            ));

            let bar = style::layer_style(&layer);
            assert_eq!(bar.fill, Some(fill), "{appearance:?} {contrast:?}");
            assert_eq!(bar.line_width, None);
            assert_eq!(bar.line_sides, []);
            assert_eq!(bar.radius, Some(6.0));
            assert!(!layer.masksToBounds());

            let mut got: Vec<ShadowFacts> = raised
                .shadows
                .iter()
                .filter_map(|layer| style::layer_shadow(layer))
                .collect();
            got.extend(style::ring_shadow(&raised.ring));
            let geometry: Vec<_> = got
                .iter()
                .map(|shadow| (shadow.x, shadow.y, shadow.blur, shadow.spread))
                .collect();
            assert_eq!(
                geometry,
                [
                    (0.0, 18.0, 48.0, 0.0),
                    (0.0, 2.0, 8.0, 0.0),
                    (0.0, 0.0, 0.0, 1.0)
                ]
            );
            for (shadow, want) in got.iter().zip(shadows.into_iter().chain([ring])) {
                assert_eq!(
                    (shadow.color.r, shadow.color.g, shadow.color.b),
                    (want.r, want.g, want.b)
                );
                assert!((shadow.color.a - want.a).abs() < 1e-3, "{shadow:?}");
            }
            let frame = raised.ring.frame();
            assert_eq!(
                (
                    frame.origin.x,
                    frame.origin.y,
                    frame.size.width,
                    frame.size.height
                ),
                (-1.0, -1.0, 207.0, 28.0)
            );
            assert_eq!(raised.ring.cornerRadius(), 7.0);
        }
    }

    #[test]
    fn form_field_keeps_surface_fill_layer_line_and_clipping() {
        let theme = AppTheme::for_cell(shellio_theme::Cell::ALL[0]);
        let layer = CALayer::new();
        paint(&layer, None, TextFieldKind::Field, &theme, false);
        let idle = crate::style::layer_style(&layer);
        assert_eq!(idle.line_width, Some(LINE_WIDTH));
        assert!(idle.shadow.is_empty());
        assert!(layer.masksToBounds());
        paint(&layer, None, TextFieldKind::Field, &theme, true);
        assert_eq!(
            crate::style::layer_style(&layer).line_width,
            Some(FOCUS_LINE_WIDTH)
        );
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
