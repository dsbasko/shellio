//! Вид поверхности с клавиатурой для прохода F4 (задача 19): упрощённая копия пути клавиатуры
//! P0.3 (`spikes/probe-ghostty-surface/src/view.rs` и `keys.rs`): `keyDown` → `interpretKeyEvents`
//! → `NSTextInputClient` (`insertText`, `setMarkedText`, `unmarkText`) → `ghostty_surface_key`.
//! Правила сборки события повторяют встраивание Ghostty для macOS (MIT), как в P0.3.
//!
//! Журнал ввода вида — события, вызовы клиента ввода и отправленные клавиши — водитель забирает
//! после каждой команды: по нему стенд отличает событие, не прошедшее через источник ввода
//! (`stand`), от потери в поверхности (`surface`).

use std::cell::{Cell, RefCell};
use std::ffi::CString;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSEvent, NSEventModifierFlags, NSEventType, NSResponder, NSTextInputClient, NSView,
};
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSObject, NSObjectProtocol, NSPoint,
    NSRange, NSRangePointer, NSRect, NSSize, NSString, NSUInteger,
};
use shellio_sys_ghostty as sys;

/// Биты `NSEventModifierFlags`.
mod flags {
    pub const CAPS_LOCK: u64 = 1 << 16;
    pub const SHIFT: u64 = 1 << 17;
    pub const CONTROL: u64 = 1 << 18;
    pub const OPTION: u64 = 1 << 19;
    pub const COMMAND: u64 = 1 << 20;
}

/// Код клавиши текста без физической клавиши: ghostty видит `unidentified`.
const NO_PHYSICAL_KEY: u32 = u32::MAX;
const NOT_FOUND: NSUInteger = NSUInteger::MAX >> 1;

fn mods_from_flags(raw: u64) -> u32 {
    [
        (flags::SHIFT, sys::ghostty_input_mods_e_GHOSTTY_MODS_SHIFT),
        (flags::CONTROL, sys::ghostty_input_mods_e_GHOSTTY_MODS_CTRL),
        (flags::OPTION, sys::ghostty_input_mods_e_GHOSTTY_MODS_ALT),
        (flags::COMMAND, sys::ghostty_input_mods_e_GHOSTTY_MODS_SUPER),
        (
            flags::CAPS_LOCK,
            sys::ghostty_input_mods_e_GHOSTTY_MODS_CAPS,
        ),
    ]
    .into_iter()
    .filter(|(flag, _)| raw & flag != 0)
    .fold(0, |mods, (_, bit)| mods | bit)
}

/// Флаги перевода символов: `Shift`, `Control`, `Option`, `Command` — по модификаторам перевода
/// ghostty (`macos-option-as-alt`), остальные — из события: скрытые биты нужны мёртвым клавишам.
fn translation_flags(event_flags: u64, translation_mods: u32) -> u64 {
    [
        (flags::SHIFT, sys::ghostty_input_mods_e_GHOSTTY_MODS_SHIFT),
        (flags::CONTROL, sys::ghostty_input_mods_e_GHOSTTY_MODS_CTRL),
        (flags::OPTION, sys::ghostty_input_mods_e_GHOSTTY_MODS_ALT),
        (flags::COMMAND, sys::ghostty_input_mods_e_GHOSTTY_MODS_SUPER),
    ]
    .into_iter()
    .fold(event_flags, |raw, (flag, bit)| {
        if translation_mods & bit != 0 {
            raw | flag
        } else {
            raw & !flag
        }
    })
}

/// Модификаторы, участвовавшие в получении текста: все, кроме `Control` и `Command`.
fn consumed_mods(translation_flags: u64) -> u32 {
    mods_from_flags(translation_flags & !(flags::CONTROL | flags::COMMAND))
}

/// Текст события для ghostty: одиночный управляющий символ заменяется символом без `Control`,
/// символ функциональной клавиши (U+F700–U+F8FF) текста не даёт.
fn ghostty_characters(characters: Option<&str>, without_control: Option<&str>) -> Option<String> {
    let characters = characters?;
    let mut scalars = characters.chars();
    if let (Some(scalar), None) = (scalars.next(), scalars.next()) {
        if u32::from(scalar) < 0x20 {
            return without_control.map(str::to_owned);
        }
        if (0xF700..=0xF8FF).contains(&u32::from(scalar)) {
            return None;
        }
    }
    Some(characters.to_owned())
}

/// Пустой текст и текст с управляющим символом в начале не передаются: такие клавиши ghostty
/// кодирует сам.
fn key_event_text(text: &str) -> Option<&str> {
    let first = text.chars().next()?;
    (u32::from(first) >= 0x20 && first != '\u{7f}').then_some(text)
}

/// Одиночный управляющий символ во время composition принадлежит IME.
fn suppressed_while_composing(text: Option<&str>, composing: bool) -> bool {
    let Some(text) = text else {
        return false;
    };
    let mut scalars = text.chars();
    composing
        && matches!((scalars.next(), scalars.next()), (Some(scalar), None) if u32::from(scalar) < 0x20)
}

/// Логическое событие клавиши в собственных данных.
#[derive(Clone, Debug, PartialEq, Eq)]
struct KeyInput {
    press: bool,
    mods: u32,
    consumed_mods: u32,
    keycode: u32,
    text: Option<String>,
    unshifted_codepoint: u32,
    composing: bool,
}

impl KeyInput {
    fn committed(text: &str) -> KeyInput {
        KeyInput {
            press: true,
            mods: 0,
            consumed_mods: 0,
            keycode: NO_PHYSICAL_KEY,
            text: Some(text.to_owned()),
            unshifted_codepoint: 0,
            composing: false,
        }
    }

    fn describe(&self) -> String {
        format!(
            "{} {} mods={} text={:?}{}",
            if self.press { "press" } else { "release" },
            if self.keycode == NO_PHYSICAL_KEY {
                "-".to_owned()
            } else {
                self.keycode.to_string()
            },
            self.mods,
            self.text.as_deref().unwrap_or(""),
            if self.composing { " composing" } else { "" }
        )
    }
}

/// Что прочитано у события клавиши.
#[derive(Clone, Debug, Default)]
struct EventKey {
    key_code: u16,
    flags: u64,
    translation_flags: u64,
    characters: Option<String>,
    without_control: Option<String>,
    unmodified: Option<String>,
}

impl EventKey {
    fn input(&self, press: bool, text: Option<&str>, composing: bool) -> KeyInput {
        KeyInput {
            press,
            mods: mods_from_flags(self.flags),
            consumed_mods: consumed_mods(self.translation_flags),
            keycode: u32::from(self.key_code),
            text: text.and_then(key_event_text).map(str::to_owned),
            unshifted_codepoint: self
                .unmodified
                .as_deref()
                .and_then(|text| text.chars().next())
                .map_or(0, u32::from),
            composing,
        }
    }
}

/// События `keyDown` в `ghostty_surface_key` по порядку: текст, накопленный `insertText` за
/// `interpretKeyEvents`, иначе символы события.
fn plan_key_down(
    event: &EventKey,
    accumulated: &[String],
    marked_before: bool,
    marked_after: bool,
) -> Vec<KeyInput> {
    let composing = marked_before || marked_after;
    if marked_before && !accumulated.is_empty() {
        return accumulated
            .iter()
            .filter(|text| !suppressed_while_composing(Some(text), composing))
            .map(|text| KeyInput::committed(text))
            .collect();
    }
    if !accumulated.is_empty() {
        return accumulated
            .iter()
            .filter(|text| !suppressed_while_composing(Some(text), composing))
            .map(|text| event.input(true, Some(text), false))
            .collect();
    }
    if suppressed_while_composing(event.characters.as_deref(), composing) {
        return Vec::new();
    }
    let text = ghostty_characters(
        event.characters.as_deref(),
        event.without_control.as_deref(),
    );
    vec![event.input(true, text.as_deref(), composing)]
}

pub struct KeyIvars {
    /// Поверхность вида; 0 — нет. Снимается до освобождения поверхности.
    surface: Cell<usize>,
    marked: RefCell<String>,
    /// Текст `insertText` во время `interpretKeyEvents`; `None` — вне `keyDown`.
    accumulator: RefCell<Option<Vec<String>>>,
    /// Коды клавиш, нажатие которых ушло в ghostty физической клавишей.
    sent_down: RefCell<Vec<u16>>,
    journal: RefCell<Vec<String>>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PssKeyView"]
    #[ivars = KeyIvars]
    pub struct KeyView;

    impl KeyView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.on_key_down(event);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.on_key_up(event);
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, _event: &NSEvent) {}
    }

    unsafe impl NSObjectProtocol for KeyView {}

    unsafe impl NSTextInputClient for KeyView {
        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &AnyObject, _replacement: NSRange) {
            self.on_insert_text(string);
        }

        #[unsafe(method(doCommandBySelector:))]
        fn do_command(&self, selector: Sel) {
            self.note(format!("doCommand {}", selector.name().to_string_lossy()));
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(&self, string: &AnyObject, _selected: NSRange, _replacement: NSRange) {
            self.on_set_marked(string);
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            self.note("unmarkText".to_owned());
            self.set_marked(String::new());
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            match self.marked_len() {
                0 => NSRange::new(NOT_FOUND, 0),
                len => NSRange::new(len, 0),
            }
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            match self.marked_len() {
                0 => NSRange::new(NOT_FOUND, 0),
                len => NSRange::new(0, len),
            }
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            self.marked_len() > 0
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        fn attributed_substring(
            &self,
            _range: NSRange,
            _actual: NSRangePointer,
        ) -> Option<Retained<NSAttributedString>> {
            None
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        fn first_rect(&self, _range: NSRange, _actual: NSRangePointer) -> NSRect {
            NSRect::new(NSPoint::new(100.0, 100.0), NSSize::new(1.0, 20.0))
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index(&self, _point: NSPoint) -> NSUInteger {
            NOT_FOUND
        }
    }
);

impl KeyView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<KeyView> {
        let this = KeyView::alloc(mtm).set_ivars(KeyIvars {
            surface: Cell::new(0),
            marked: RefCell::new(String::new()),
            accumulator: RefCell::new(None),
            sent_down: RefCell::new(Vec::new()),
            journal: RefCell::new(Vec::new()),
        });
        // SAFETY: `initWithFrame:` — назначенный инициализатор NSView.
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Поверхность, в которую вид отправляет клавиши; `None` снимает её до освобождения.
    pub fn set_surface(&self, surface: Option<sys::ghostty_surface_t>) {
        self.ivars()
            .surface
            .set(surface.map_or(0, |raw| raw as usize));
    }

    fn surface(&self) -> Option<sys::ghostty_surface_t> {
        match self.ivars().surface.get() {
            0 => None,
            raw => Some(raw as sys::ghostty_surface_t),
        }
    }

    /// Журнал ввода с прошлого забора.
    pub fn take_journal(&self) -> Vec<String> {
        std::mem::take(&mut *self.ivars().journal.borrow_mut())
    }

    fn note(&self, line: String) {
        self.ivars().journal.borrow_mut().push(line);
    }

    fn marked_len(&self) -> NSUInteger {
        self.ivars().marked.borrow().encode_utf16().count()
    }

    fn set_marked(&self, text: String) {
        let changed = *self.ivars().marked.borrow() != text;
        *self.ivars().marked.borrow_mut() = text;
        if changed && self.ivars().accumulator.borrow().is_none() {
            self.sync_preedit();
        }
    }

    fn sync_preedit(&self) {
        let Some(surface) = self.surface() else {
            return;
        };
        let text = self.ivars().marked.borrow().clone();
        // SAFETY: поверхность жива, пока вид её держит; байты живут на время вызова.
        unsafe { sys::ghostty_surface_preedit(surface, text.as_ptr().cast(), text.len()) };
    }

    fn send(&self, input: &KeyInput) {
        let Some(surface) = self.surface() else {
            self.note(format!("dropped {} (нет поверхности)", input.describe()));
            return;
        };
        let Ok(text) = input.text.as_deref().map(CString::new).transpose() else {
            self.note(format!("dropped {} (нулевой байт)", input.describe()));
            return;
        };
        let raw = sys::ghostty_input_key_s {
            action: if input.press {
                sys::ghostty_input_action_e_GHOSTTY_ACTION_PRESS
            } else {
                sys::ghostty_input_action_e_GHOSTTY_ACTION_RELEASE
            },
            mods: input.mods,
            consumed_mods: input.consumed_mods,
            keycode: input.keycode,
            text: text.as_ref().map_or(std::ptr::null(), |text| text.as_ptr()),
            unshifted_codepoint: input.unshifted_codepoint,
            composing: input.composing,
        };
        // SAFETY: поверхность жива; текст — строка с нулём, живая до конца функции.
        let handled = unsafe { sys::ghostty_surface_key(surface, raw) };
        self.note(format!("sent {} handled={handled}", input.describe()));
    }

    fn event_key(
        &self,
        event: &NSEvent,
        translation: &NSEvent,
        translation_flags: u64,
    ) -> EventKey {
        let without_control = NSEventModifierFlags((translation_flags & !flags::CONTROL) as usize);
        EventKey {
            key_code: event.keyCode(),
            flags: event.modifierFlags().0 as u64,
            translation_flags,
            characters: translation.characters().map(|text| text.to_string()),
            without_control: translation
                .charactersByApplyingModifiers(without_control)
                .map(|text| text.to_string()),
            unmodified: event
                .charactersByApplyingModifiers(NSEventModifierFlags::empty())
                .map(|text| text.to_string()),
        }
    }

    fn on_key_down(&self, event: &NSEvent) {
        let code = event.keyCode();
        self.note(format!(
            "keyDown {code} flags={:#x} chars={:?}",
            event.modifierFlags().0,
            event
                .characters()
                .map(|text| text.to_string())
                .unwrap_or_default()
        ));
        let Some(surface) = self.surface() else {
            self.note("dropped keyDown (нет поверхности)".to_owned());
            return;
        };
        let flags = event.modifierFlags().0 as u64;
        // SAFETY: поверхность жива, вызов на главном потоке.
        let translation_mods =
            unsafe { sys::ghostty_surface_key_translation_mods(surface, mods_from_flags(flags)) };
        let translation_flags = translation_flags(flags, translation_mods);
        // Модификаторы перевода расходятся с событием только при `macos-option-as-alt`, которого
        // в конфигурации пробы нет; копия события с другими символами здесь не строится.
        if translation_flags != flags {
            self.note(format!(
                "keyDown {code}: флаги перевода {translation_flags:#x} не равны флагам события"
            ));
        }
        let translation = event.retain();
        let event_key = self.event_key(event, &translation, translation_flags);
        let marked_before = self.marked_len() > 0;
        *self.ivars().accumulator.borrow_mut() = Some(Vec::new());
        self.interpretKeyEvents(&NSArray::from_retained_slice(&[translation]));
        let accumulated = self
            .ivars()
            .accumulator
            .borrow_mut()
            .take()
            .unwrap_or_default();
        let marked_after = self.marked_len() > 0;
        if marked_before || marked_after {
            self.sync_preedit();
        }
        let plan = plan_key_down(&event_key, &accumulated, marked_before, marked_after);
        if plan.is_empty() {
            self.note(format!("keyDown {code}: съедено composition"));
        }
        let mut physical = false;
        for input in &plan {
            physical |= input.keycode != NO_PHYSICAL_KEY;
            self.send(input);
        }
        let mut sent_down = self.ivars().sent_down.borrow_mut();
        sent_down.retain(|held| *held != code);
        if physical {
            sent_down.push(code);
        }
    }

    fn on_key_up(&self, event: &NSEvent) {
        if event.r#type() != NSEventType::KeyUp {
            return;
        }
        let code = event.keyCode();
        let sent = {
            let mut sent_down = self.ivars().sent_down.borrow_mut();
            let before = sent_down.len();
            sent_down.retain(|held| *held != code);
            sent_down.len() != before
        };
        if !sent {
            return;
        }
        let flags = event.modifierFlags().0 as u64;
        let event_key = self.event_key(event, event, flags);
        self.send(&event_key.input(false, None, false));
    }

    fn on_insert_text(&self, string: &AnyObject) {
        let text = text_of(string);
        self.note(format!("insertText {text:?}"));
        self.set_marked(String::new());
        {
            let mut accumulator = self.ivars().accumulator.borrow_mut();
            if let Some(list) = accumulator.as_mut() {
                if !text.is_empty() {
                    list.push(text);
                }
                return;
            }
        }
        if !text.is_empty() {
            self.send(&KeyInput::committed(&text));
        }
    }

    fn on_set_marked(&self, string: &AnyObject) {
        let text = text_of(string);
        self.note(format!("setMarkedText {text:?}"));
        self.set_marked(text);
    }
}

/// Текст `insertText:` или `setMarkedText:` (`NSString` или `NSAttributedString`).
fn text_of(string: &AnyObject) -> String {
    if let Some(attributed) = string.downcast_ref::<NSAttributedString>() {
        return attributed.string().to_string();
    }
    string
        .downcast_ref::<NSString>()
        .map(NSString::to_string)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(code: u16, flags: u64, characters: &str) -> EventKey {
        EventKey {
            key_code: code,
            flags,
            translation_flags: flags,
            characters: Some(characters.to_owned()),
            without_control: Some(characters.to_owned()),
            unmodified: Some(characters.to_owned()),
        }
    }

    #[test]
    fn key_down_sends_accumulated_text_or_event_characters() {
        let g = event(5, 0, "п");
        let plan = plan_key_down(&g, &["п".to_owned()], false, false);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].text.as_deref(), Some("п"));
        assert_eq!(plan[0].keycode, 5);

        let enter = event(36, flags::SHIFT, "\r");
        let plan = plan_key_down(&enter, &[], false, false);
        assert_eq!(plan[0].text, None, "управляющий символ кодирует ghostty");
        assert_eq!(plan[0].mods, sys::ghostty_input_mods_e_GHOSTTY_MODS_SHIFT);

        let dead = event(14, flags::OPTION, "");
        assert_eq!(plan_key_down(&dead, &[], false, true)[0].text, None);
        assert!(plan_key_down(&dead, &[], false, true)[0].composing);

        let commit = plan_key_down(&event(36, 0, "\r"), &["日本".to_owned()], true, false);
        assert_eq!(commit, [KeyInput::committed("日本")]);
        assert!(plan_key_down(&event(53, 0, "\u{1b}"), &[], true, true).is_empty());
    }

    #[test]
    fn modifiers_and_translation_flags() {
        assert_eq!(
            mods_from_flags(flags::CONTROL | flags::SHIFT),
            sys::ghostty_input_mods_e_GHOSTTY_MODS_CTRL
                | sys::ghostty_input_mods_e_GHOSTTY_MODS_SHIFT
        );
        assert_eq!(translation_flags(flags::OPTION | 0x100, 0), 0x100);
        assert_eq!(
            consumed_mods(flags::CONTROL | flags::SHIFT),
            sys::ghostty_input_mods_e_GHOSTTY_MODS_SHIFT
        );
        assert_eq!(
            ghostty_characters(Some("\u{1}"), Some("a")),
            Some("a".to_owned())
        );
        assert_eq!(ghostty_characters(Some("\u{f700}"), None), None);
    }
}
