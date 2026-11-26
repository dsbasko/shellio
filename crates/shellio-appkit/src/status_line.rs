//! Строка статуса — нижняя строка окна (ADR-0024): плашка режима, затем набранный префикс,
//! сообщение или подсказки клавиш. Отказ команды раскладки из интерфейса показывается в ней
//! текстом ADR-0031, следующее действие пользователя его убирает.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityButtonRole, NSAccessibilityStaticTextRole, NSEvent, NSFont,
    NSFontWeightBold, NSFontWeightRegular, NSLineBreakMode, NSResponder, NSTextField, NSView,
    NSViewLayerContentsRedrawPolicy,
};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CALayer;
use shellio_theme::{AppTheme, KeyMode};

use crate::app::App;
use crate::chrome_bar::Frame;
use crate::counters::{Live, counter};
use crate::font;
use crate::inspect::{self, InspectNode};
use crate::layer::{self, without_actions};
use crate::own::{Callback, ComponentId, View};
use crate::piece::ns_rect;
use crate::runloop;
use crate::style;

counter!(STATUS_OWNER, Owner, "StatusLine");
counter!(STATUS_VIEW, Class, "ShellioStatusLine");

/// Отступ текста от края своего слота (`.sb-msg`, `.hints { padding: 0 12px }` прототипа).
const PAD: f64 = 12.0;
/// Высота строки текста.
const LINE: f64 = 16.0;
/// Межстрочный интервал сообщения и подписи подсказки: высота слота (`line-height` прототипа).
const MESSAGE_LINE: f64 = 18.0;
/// Ширина плашки режима (`.mode { width: 112px }` прототипа).
pub const MODE_WIDTH: f64 = 112.0;
/// Промежуток между подсказками (`.hints { gap: 14px }`).
const HINT_GAP: f64 = 14.0;
/// Промежуток между клавишами подсказки (`.hints .h { gap: 3px }`).
const KEY_GAP: f64 = 3.0;
/// От последней клавиши до подписи: промежуток подсказки и `em { margin-left: 4px }`.
const LABEL_GAP: f64 = 7.0;
/// Плашка клавиши — правило `kbd` прототипа: высота и наименьшая ширина 17, поля 4, линия 1,
/// радиус 4, моноширинный кегль 10,5.
const KEY_SIZE: f64 = 17.0;
const KEY_PAD: f64 = 4.0;
const KEY_RADIUS: f64 = 4.0;
const KEY_FONT: f64 = 10.5;

/// Слот сообщения в строке статуса `width`×`height` с линией `line` сверху, в точках строки с
/// началом сверху слева (`.sb-msg { padding: 0 12px }` прототипа): от правого края плашки режима
/// `left`, шириной в набор текста `text_width` с отступами, но не дальше края строки, по середине
/// высоты под линией.
pub fn message_frame(left: f64, width: f64, height: f64, line: f64, text_width: f64) -> Frame {
    Frame::new(
        left,
        line + (height - line - MESSAGE_LINE) / 2.0,
        (text_width.max(0.0) + 2.0 * PAD).min((width - left).max(0.0)),
        MESSAGE_LINE,
    )
}

/// Ширина плашки клавиши с набором подписи `text_width`.
pub fn key_width(text_width: f64) -> f64 {
    (text_width.max(0.0) + 2.0 * KEY_PAD + 2.0)
        .ceil()
        .max(KEY_SIZE)
}

/// Подсказка строки статуса: клавиши по одной и подпись.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusHint {
    pub keys: Vec<String>,
    pub label: String,
}

/// Кадры одной подсказки в точках строки с началом сверху слева.
#[derive(Clone, Debug, PartialEq)]
pub struct HintFrames {
    pub group: Frame,
    pub keys: Vec<Frame>,
    /// Место набора подписи.
    pub label: Frame,
}

/// Кадры подсказок после плашки режима в строке высотой `height` с линией `line` сверху: у каждой
/// подсказки ширины плашек клавиш и ширина набора подписи.
pub fn hint_frames(height: f64, line: f64, hints: &[(Vec<f64>, f64)]) -> Vec<HintFrames> {
    let under = height - line;
    let top = line + (under - MESSAGE_LINE) / 2.0;
    let key_top = line + (under - KEY_SIZE) / 2.0;
    let mut x = MODE_WIDTH + PAD;
    hints
        .iter()
        .map(|(keys, label)| {
            let start = x;
            let mut frames = Vec::with_capacity(keys.len());
            for (index, width) in keys.iter().enumerate() {
                if index > 0 {
                    x += KEY_GAP;
                }
                frames.push(Frame::new(x, key_top, *width, KEY_SIZE));
                x += width;
            }
            if !keys.is_empty() {
                x += LABEL_GAP;
            }
            let label = Frame::new(x, top, label.max(0.0), MESSAGE_LINE);
            x += label.width;
            let group = Frame::new(start, top, x - start, MESSAGE_LINE);
            x += HINT_GAP;
            HintFrames {
                group,
                keys: frames,
                label,
            }
        })
        .collect()
}

/// Подпись плашки режима.
pub fn mode_title(mode: KeyMode) -> &'static str {
    match mode {
        KeyMode::Navigation => "НАВИГАЦИЯ",
        KeyMode::Input => "ВВОД",
    }
}

struct StatusModel {
    id: ComponentId,
    theme: Cell<AppTheme>,
    /// Ширина плашки режима: [`MODE_WIDTH`] у строки окна, 0 у подписи в теле панели.
    left: f64,
    mode: Cell<KeyMode>,
    /// Плашка режима нажата указателем.
    pressed: Cell<bool>,
    on_mode: RefCell<Option<Rc<dyn Fn()>>>,
    message: RefCell<Option<String>>,
    /// Набранное начало последовательности: замещает сообщение и подсказки.
    prefix: RefCell<Option<String>>,
    hints: RefCell<Vec<StatusHint>>,
}

/// Виды одной подсказки.
struct HintViews {
    group: Retained<NSView>,
    keys: Vec<(Retained<NSView>, Retained<NSTextField>)>,
    label: Retained<NSTextField>,
}

/// Строка статуса на главном потоке. Освобождение убирает её из иерархии.
pub struct StatusLine {
    view: View<StatusView, StatusModel>,
}

impl StatusLine {
    /// Подпись без плашки режима: тот же компонент в теле панели.
    pub fn new(app: &App, theme: &AppTheme) -> StatusLine {
        StatusLine::build(app, theme, 0.0)
    }

    /// Строка статуса окна: слева плашка режима.
    pub fn with_mode(app: &App, theme: &AppTheme) -> StatusLine {
        StatusLine::build(app, theme, MODE_WIDTH)
    }

    fn build(app: &App, theme: &AppTheme, left: f64) -> StatusLine {
        let mtm = app.mtm();
        let model = StatusModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            left,
            mode: Cell::new(KeyMode::Navigation),
            pressed: Cell::new(false),
            on_mode: RefCell::new(None),
            message: RefCell::new(None),
            prefix: RefCell::new(None),
            hints: RefCell::new(Vec::new()),
        };
        let view = View::new(&STATUS_OWNER, model, "StatusLine.updateLayer", |owner| {
            StatusView::new(mtm, owner)
        });
        let line = StatusLine { view };
        line.view.view().ivars().mode_box.setHidden(left == 0.0);
        line.apply_theme();
        line
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    /// Текст сообщения; пустая строка — сообщения нет.
    pub fn message(&self) -> String {
        self.view
            .rust()
            .message
            .borrow()
            .clone()
            .unwrap_or_default()
    }

    /// Сообщение после плашки режима; `None` убирает его. Сообщение замещает подсказки.
    pub fn set_message(&self, message: Option<&str>) {
        let model = self.view.rust();
        if model.message.borrow().as_deref() == message {
            return;
        }
        *model.message.borrow_mut() = message.map(str::to_owned);
        self.show_text();
    }

    /// Нажатие плашки режима.
    pub fn on_mode(&self, handler: impl Fn() + 'static) {
        *self.view.rust().on_mode.borrow_mut() = Some(Rc::new(handler));
    }

    /// Режим на плашке, набранный префикс и подсказки. `true` — что-то изменилось.
    pub fn set_left(&self, mode: KeyMode, prefix: Option<&str>, hints: &[StatusHint]) -> bool {
        let model = self.view.rust();
        let view = self.view.view();
        let mut changed = false;
        if model.mode.get() != mode {
            model.mode.set(mode);
            let title = NSString::from_str(mode_title(mode));
            view.ivars().mode.setStringValue(&title);
            view.ivars().mode_box.setAccessibilityLabel(Some(&title));
            changed = true;
        }
        if model.prefix.borrow().as_deref() != prefix {
            *model.prefix.borrow_mut() = prefix.map(str::to_owned);
            changed = true;
        }
        if model.hints.borrow().as_slice() != hints {
            *model.hints.borrow_mut() = hints.to_vec();
            view.rebuild_hints(hints);
            changed = true;
        }
        if changed {
            self.show_text();
            self.apply_theme();
            view.place();
        }
        changed
    }

    /// Набранный префикс либо сообщение в текстовом слоте; подсказки видны, когда слот пуст.
    fn show_text(&self) {
        let model = self.view.rust();
        let view = self.view.view();
        let shown = model
            .prefix
            .borrow()
            .clone()
            .or_else(|| model.message.borrow().clone());
        let label = &view.ivars().text;
        let value = NSString::from_str(shown.as_deref().unwrap_or(""));
        label.setStringValue(&value);
        label.setAccessibilityLabel(Some(&value));
        label.setHidden(shown.is_none());
        for hint in view.ivars().hints.borrow().iter() {
            hint.group.setHidden(shown.is_some());
        }
        view.place();
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        let model = self.view.rust();
        if model.theme.get() != *theme {
            model.theme.set(*theme);
            self.apply_theme();
        }
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    /// Строка, плашка режима, текстовый слот и подсказки для `ui.inspect`. Слот — `<id>.message`
    /// либо `<id>.prefix`, когда показан набранный префикс: его подпись — текст, кадр —
    /// [`message_frame`], а не кадр поля: слот несёт отступы текста. Линию над строкой рисует
    /// отдельный слой. Плашка и подсказки несут свои ключи сверки.
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        let view = self.view.view();
        let model = self.view.rust();
        let parts = view.ivars();
        let text = &parts.text;
        let slot_id = if model.prefix.borrow().is_some() {
            format!("{id}.prefix")
        } else {
            format!("{id}.message")
        };
        let mut message = inspect::node(&slot_id, "StatusMessage", Some(id), text, None, None);
        let bounds = view.bounds().size;
        let text_width = text.font().map_or(0.0, |font| {
            style::typeset_width(&text.stringValue().to_string(), &font)
        });
        let slot = message_frame(
            model.left,
            bounds.width,
            bounds.height,
            model.theme.get().metrics.line,
            text_width,
        );
        message.facts.frame = inspect::frame_of(view.convertRect_toView(ns_rect(slot), None));
        let mut nodes = vec![
            inspect::node(id, "StatusLine", parent, view, None, None)
                .with_line_layer(&parts.line, &[style::Side::Top]),
        ];
        if model.left > 0.0 {
            nodes.push(
                inspect::node(
                    &format!("{id}.mode"),
                    "StatusMode",
                    Some(id),
                    &parts.mode_box,
                    Some(true),
                    None,
                )
                .with_ck(inspect::ck::STATUS_MODE)
                .with_text_of(&parts.mode),
            );
        }
        nodes.push(message);
        for (index, hint) in parts.hints.borrow().iter().enumerate() {
            let hint_id = format!("{id}.hint.{}", index + 1);
            let hint_ck = format!("{}.hint.{}", inspect::ck::STATUS, index + 1);
            nodes.push(
                inspect::node(&hint_id, "StatusHint", Some(id), &hint.group, None, None)
                    .with_ck(hint_ck.clone()),
            );
            for (key, (key_box, label)) in hint.keys.iter().enumerate() {
                nodes.push(
                    inspect::node(
                        &format!("{hint_id}.key.{}", key + 1),
                        "KeyBadge",
                        Some(&hint_id),
                        key_box,
                        None,
                        None,
                    )
                    .with_ck(format!("{hint_ck}.key.{}", key + 1))
                    .with_text_of(label),
                );
            }
            nodes.push(
                inspect::typeset_node(&format!("{hint_id}.label"), "Label", &hint_id, &hint.label)
                    .with_ck(format!("{hint_ck}.label")),
            );
        }
        nodes
    }

    fn apply_theme(&self) {
        let view = self.view.view();
        let theme = self.view.rust().theme.get();
        let parts = view.ivars();
        // SAFETY: веса шрифта — неизменяемые константы AppKit.
        let (regular, bold) = unsafe { (NSFontWeightRegular, NSFontWeightBold) };
        let small = NSFont::systemFontOfSize_weight(theme.metrics.font.small, regular);
        parts
            .text
            .setTextColor(Some(&layer::ns_color(theme.colors.text_2)));
        parts.text.setFont(Some(&small));
        let ink = match self.view.rust().mode.get() {
            KeyMode::Navigation => theme.colors.accent_nav_ink,
            KeyMode::Input => theme.colors.accent_input_ink,
        };
        parts.mode.setTextColor(Some(&layer::ns_color(ink)));
        parts.mode.setFont(Some(&NSFont::systemFontOfSize_weight(
            theme.metrics.font.caption,
            bold,
        )));
        for hint in parts.hints.borrow().iter() {
            hint.label
                .setTextColor(Some(&layer::ns_color(theme.colors.text_3)));
            hint.label.setFont(Some(&small));
            for (key_box, label) in &hint.keys {
                label.setTextColor(Some(&layer::ns_color(theme.colors.text_2)));
                label.setFont(Some(&font::mono(KEY_FONT)));
                if let Some(layer) = key_box.layer() {
                    without_actions(|| {
                        layer::set_fill(&layer, theme.colors.kbd);
                        layer::set_line(&layer, Some((theme.colors.line_2, 1.0)));
                        layer::set_radius(&layer, KEY_RADIUS);
                    });
                }
            }
        }
        view.setNeedsDisplay(true);
        view.place();
    }
}

struct StatusIvars {
    _live: Live,
    owner: Callback<StatusModel>,
    text: Retained<NSTextField>,
    /// Плашка режима (`.mode` прототипа) с подписью внутри.
    mode_box: Retained<NSView>,
    mode: Retained<NSTextField>,
    hints: RefCell<Vec<HintViews>>,
    /// Линия над строкой (`.status { border-top }` прототипа).
    line: Retained<CALayer>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioStatusLine"]
    #[ivars = StatusIvars]
    struct StatusView;

    impl StatusView {
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
            let Some((theme, mode, pressed)) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                (model.theme.get(), model.mode.get(), model.pressed.get())
            }) else {
                return;
            };
            let chip = match (pressed, mode) {
                (true, _) => theme.colors.chrome_press,
                (false, KeyMode::Navigation) => theme.colors.accent_nav,
                (false, KeyMode::Input) => theme.colors.accent_input,
            };
            if let Some(layer) = self.layer() {
                without_actions(|| {
                    layer::set_fill(&layer, theme.colors.chrome);
                    layer::set_fill(&self.ivars().line, theme.colors.line);
                    if let Some(mode) = self.ivars().mode_box.layer() {
                        layer::set_fill(&mode, chip);
                    }
                });
            }
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            if self.on_mode(event) {
                self.press(true);
            }
        }

        // SAFETY: селектор и аргумент — как у `mouseUp:` NSResponder.
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let pressed = self.ivars().owner.call(|model| model.pressed.get()) == Some(true);
            self.press(false);
            if pressed && self.on_mode(event) {
                let handler = self
                    .ivars()
                    .owner
                    .call(|model| model.on_mode.borrow().clone())
                    .flatten();
                if let Some(handler) = handler {
                    handler();
                }
            }
        }

        // SAFETY: селектор и результат — как у `layout` NSView.
        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), layout] };
            let (width, line) = self.ivars().owner.call(|model| {
                (self.bounds().size.width, model.theme.get().metrics.line)
            }).unwrap_or((0.0, 0.0));
            without_actions(|| {
                self.ivars().line.setFrame(NSRect::new(
                    NSPoint::ZERO,
                    NSSize::new(width, line),
                ));
            });
            self.place();
        }
    }
);

impl StatusView {
    fn new(mtm: MainThreadMarker, owner: Callback<StatusModel>) -> Retained<StatusView> {
        let text = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        text.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        text.setHidden(true);
        // SAFETY: роль доступности — неизменяемая константа AppKit.
        text.setAccessibilityRole(Some(unsafe { NSAccessibilityStaticTextRole }));
        let title = NSString::from_str(mode_title(KeyMode::Navigation));
        let mode = NSTextField::labelWithString(&title, mtm);
        let mode_box = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        mode_box.setWantsLayer(true);
        mode_box.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        mode_box.addSubview(&mode);
        mode_box.setAccessibilityElement(true);
        // SAFETY: роль доступности — неизменяемая константа AppKit.
        mode_box.setAccessibilityRole(Some(unsafe { NSAccessibilityButtonRole }));
        mode_box.setAccessibilityLabel(Some(&title));
        mode_box.setAccessibilityHelp(Some(&NSString::from_str("Переключить режим")));
        let this = StatusView::alloc(mtm).set_ivars(StatusIvars {
            _live: STATUS_VIEW.live(),
            owner,
            text,
            mode_box,
            mode,
            hints: RefCell::new(Vec::new()),
            line: CALayer::new(),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<StatusView> = unsafe {
            msg_send![super(this), initWithFrame: NSRect::new(NSPoint::ZERO, NSSize::new(2.0 * PAD, 2.0 * LINE))]
        };
        view.setWantsLayer(true);
        view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
        if let Some(layer) = view.layer() {
            layer.addSublayer(&view.ivars().line);
        }
        view.addSubview(&view.ivars().mode_box);
        view.addSubview(&view.ivars().text);
        view
    }

    /// Указатель события над плашкой режима.
    fn on_mode(&self, event: &NSEvent) -> bool {
        let point = self.convertPoint_fromView(event.locationInWindow(), None);
        let frame = self.ivars().mode_box.frame();
        point.x >= frame.origin.x
            && point.x < frame.origin.x + frame.size.width
            && point.y >= frame.origin.y
            && point.y < frame.origin.y + frame.size.height
    }

    fn press(&self, pressed: bool) {
        let changed = self
            .ivars()
            .owner
            .call(|model| model.pressed.replace(pressed) != pressed);
        if changed == Some(true) {
            self.setNeedsDisplay(true);
        }
    }

    /// Виды подсказок заново: прежние уходят из иерархии.
    fn rebuild_hints(&self, hints: &[StatusHint]) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let mut views = self.ivars().hints.borrow_mut();
        for hint in views.drain(..) {
            hint.group.removeFromSuperview();
        }
        for hint in hints {
            let group = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
            let keys = hint
                .keys
                .iter()
                .map(|key| {
                    let label = NSTextField::labelWithString(&NSString::from_str(key), mtm);
                    let key_box = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
                    key_box.setWantsLayer(true);
                    key_box.setLayerContentsRedrawPolicy(
                        NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay,
                    );
                    key_box.addSubview(&label);
                    group.addSubview(&key_box);
                    (key_box, label)
                })
                .collect();
            let label = NSTextField::labelWithString(&NSString::from_str(&hint.label), mtm);
            group.addSubview(&label);
            self.addSubview(&group);
            views.push(HintViews { group, keys, label });
        }
    }

    /// Плашка режима, текстовый слот и подсказки по размеру строки.
    fn place(&self) {
        let parts = self.ivars();
        let size = self.bounds().size;
        let (line, left) = parts
            .owner
            .call(|model| (model.theme.get().metrics.line, model.left))
            .unwrap_or((0.0, 0.0));
        let under = (size.height - line).max(0.0);
        let hints = parts.hints.borrow();
        let widths: Vec<(Vec<f64>, f64)> = hints
            .iter()
            .map(|hint| {
                let keys = hint
                    .keys
                    .iter()
                    .map(|(_, label)| key_width(label.intrinsicContentSize().width))
                    .collect();
                let label = hint.label.font().map_or(0.0, |font| {
                    style::typeset_width(&hint.label.stringValue().to_string(), &font)
                });
                (keys, label)
            })
            .collect();
        let frames = hint_frames(size.height, line, &widths);
        without_actions(|| {
            parts
                .mode_box
                .setFrame(Frame::new(0.0, line, left, under).rect());
            let mode = parts.mode.intrinsicContentSize();
            parts.mode.setFrame(
                Frame::new(
                    ((left - mode.width) / 2.0).round(),
                    ((under - mode.height) / 2.0).round(),
                    mode.width,
                    mode.height,
                )
                .rect(),
            );
            let text = parts.text.intrinsicContentSize();
            parts.text.setFrame(
                Frame::new(
                    left + PAD - style::LABEL_INSET,
                    line + ((under - text.height.max(LINE)) / 2.0).round(),
                    (size.width - left - 2.0 * PAD + 2.0 * style::LABEL_INSET).max(0.0),
                    text.height.max(LINE),
                )
                .rect(),
            );
            for (hint, frame) in hints.iter().zip(&frames) {
                hint.group.setFrame(frame.group.rect());
                for ((key_box, label), key) in hint.keys.iter().zip(&frame.keys) {
                    key_box.setFrame(
                        Frame::new(
                            key.x - frame.group.x,
                            key.y - frame.group.y,
                            key.width,
                            key.height,
                        )
                        .rect(),
                    );
                    let natural = label.intrinsicContentSize();
                    let width = natural.width + 2.0 * style::LABEL_INSET;
                    label.setFrame(
                        Frame::new(
                            ((key.width - width) / 2.0).round(),
                            ((key.height - natural.height) / 2.0).round(),
                            width,
                            natural.height,
                        )
                        .rect(),
                    );
                }
                let typeset = Frame::new(
                    frame.label.x - frame.group.x,
                    0.0,
                    frame.label.width,
                    frame.label.height,
                );
                hint.label
                    .setFrame(style::label_field_rect(&hint.group, typeset));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_slot_wraps_the_text_with_its_padding() {
        // Паспорт `status-split-no-room`: строка (0, 694, 1180×26), слот (0, 698,5, 509,59375×18).
        let slot = message_frame(0.0, 1180.0, 26.0, 1.0, 485.59375);
        assert_eq!(slot, Frame::new(0.0, 4.5, 509.59375, 18.0));
        assert_eq!(
            slot.y + slot.height / 2.0,
            1.0 + 25.0 / 2.0,
            "по середине высоты под линией"
        );
    }

    #[test]
    fn message_slot_is_never_wider_than_the_line() {
        let slot = message_frame(0.0, 300.0, 26.0, 1.0, 485.59375);
        assert_eq!((slot.x, slot.width), (0.0, 300.0));
        let empty = message_frame(0.0, 300.0, 26.0, 1.0, 0.0);
        assert_eq!(empty.width, 2.0 * PAD);
        assert_eq!(message_frame(0.0, 0.0, 26.0, 1.0, 40.0).width, 0.0);
        let after_mode = message_frame(MODE_WIDTH, 300.0, 26.0, 1.0, 485.59375);
        assert_eq!((after_mode.x, after_mode.width), (112.0, 188.0));
    }

    #[test]
    fn hints_follow_the_mode_plate_with_the_passport_frames() {
        // Паспорт `layout-all-zones`: строка (0, 694, 1180×26), клавиши 17×17 с 124 через 3,
        // подпись «панели» (208, 698,5, 42,023438×18), вторая подсказка с 264,023438.
        let keys = vec![17.0; 4];
        let frames = hint_frames(26.0, 1.0, &[(keys, 42.023438), (vec![17.0; 3], 47.796875)]);
        let first = &frames[0];
        assert_eq!(first.group, Frame::new(124.0, 4.5, 126.023438, 18.0));
        let xs: Vec<f64> = first.keys.iter().map(|key| key.x).collect();
        assert_eq!(xs, [124.0, 144.0, 164.0, 184.0]);
        assert!(
            first
                .keys
                .iter()
                .all(|key| key.y == 5.0 && key.height == 17.0)
        );
        assert_eq!(first.label, Frame::new(208.0, 4.5, 42.023438, 18.0));
        let second = &frames[1];
        assert!((second.group.x - 264.023438).abs() < 1e-9);
        assert!((second.label.x - 328.023438).abs() < 1e-9);
        assert!((second.group.width - 111.796875).abs() < 1e-9);
    }

    #[test]
    fn key_plate_is_never_narrower_than_its_height() {
        assert_eq!(key_width(6.3), 17.0);
        assert_eq!(key_width(0.0), 17.0);
        assert_eq!(key_width(12.6), 23.0);
    }

    #[test]
    fn mode_plate_names_the_mode() {
        assert_eq!(mode_title(KeyMode::Navigation), "НАВИГАЦИЯ");
        assert_eq!(mode_title(KeyMode::Input), "ВВОД");
    }
}
