//! Перевод событий клавиатуры и мыши AppKit в данные libghostty без AppKit: модификаторы,
//! логическое событие клавиши [`KeyInput`] и его сборка из события и текста, накопленного
//! `insertText` во время `interpretKeyEvents`, владельцы нажатых клавиш, кодирование прокрутки.
//!
//! Правила сборки повторяют встраивание Ghostty для macOS (`SurfaceView_AppKit.swift`,
//! `NSEvent+Extension.swift`), это MIT: управляющий символ события заменяется символом без
//! `Ctrl` — его кодирует ghostty по физической клавише; функциональные клавиши из частной области
//! Unicode текста не дают; текст, начинающийся с управляющего символа, не передаётся.

use std::ffi::CString;

use serde::Serialize;
use shellio_sys_ghostty as sys;

/// Код клавиши текстового события без физической клавиши. Код 0 на macOS — клавиша `A`, поэтому
/// берётся код, которого нет в таблице `src/input/keycodes.zig`: ghostty видит `unidentified`.
pub const NO_PHYSICAL_KEY: u32 = u32::MAX;

/// Биты `NSEventModifierFlags` и аппаратные биты сторон (`NX_DEVICE*`).
pub mod flags {
    pub const CAPS_LOCK: u64 = 1 << 16;
    pub const SHIFT: u64 = 1 << 17;
    pub const CONTROL: u64 = 1 << 18;
    pub const OPTION: u64 = 1 << 19;
    pub const COMMAND: u64 = 1 << 20;
    pub const RIGHT_SHIFT: u64 = 0x04;
    pub const RIGHT_COMMAND: u64 = 0x10;
    pub const RIGHT_OPTION: u64 = 0x40;
    pub const RIGHT_CONTROL: u64 = 0x2000;
}

/// Коды клавиш macOS, которые различает сборка.
pub mod code {
    pub const LEFT: u16 = 123;
    pub const RIGHT: u16 = 124;
    pub const DOWN: u16 = 125;
    pub const UP: u16 = 126;
}

const MODS_SHIFT: u32 = sys::ghostty_input_mods_e_GHOSTTY_MODS_SHIFT;
const MODS_CTRL: u32 = sys::ghostty_input_mods_e_GHOSTTY_MODS_CTRL;
const MODS_ALT: u32 = sys::ghostty_input_mods_e_GHOSTTY_MODS_ALT;
const MODS_SUPER: u32 = sys::ghostty_input_mods_e_GHOSTTY_MODS_SUPER;
const MODS_CAPS: u32 = sys::ghostty_input_mods_e_GHOSTTY_MODS_CAPS;

/// Модификаторы ghostty по флагам события, со сторонами.
pub fn mods_from_flags(raw: u64) -> u32 {
    let pairs = [
        (flags::SHIFT, MODS_SHIFT),
        (flags::CONTROL, MODS_CTRL),
        (flags::OPTION, MODS_ALT),
        (flags::COMMAND, MODS_SUPER),
        (flags::CAPS_LOCK, MODS_CAPS),
        (
            flags::RIGHT_SHIFT,
            sys::ghostty_input_mods_e_GHOSTTY_MODS_SHIFT_RIGHT,
        ),
        (
            flags::RIGHT_CONTROL,
            sys::ghostty_input_mods_e_GHOSTTY_MODS_CTRL_RIGHT,
        ),
        (
            flags::RIGHT_OPTION,
            sys::ghostty_input_mods_e_GHOSTTY_MODS_ALT_RIGHT,
        ),
        (
            flags::RIGHT_COMMAND,
            sys::ghostty_input_mods_e_GHOSTTY_MODS_SUPER_RIGHT,
        ),
    ];
    pairs
        .into_iter()
        .filter(|(flag, _)| raw & flag != 0)
        .fold(0, |mods, (_, bit)| mods | bit)
}

/// Флаги события для перевода символов: биты `Shift`, `Control`, `Option` и `Command` берутся из
/// модификаторов перевода ghostty (`macos-option-as-alt`), остальные — из события: скрытые биты
/// нужны мёртвым клавишам.
pub fn translation_flags(event_flags: u64, translation_mods: u32) -> u64 {
    [
        (flags::SHIFT, MODS_SHIFT),
        (flags::CONTROL, MODS_CTRL),
        (flags::OPTION, MODS_ALT),
        (flags::COMMAND, MODS_SUPER),
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
pub fn consumed_mods(translation_flags: u64) -> u32 {
    mods_from_flags(
        translation_flags
            & !(flags::CONTROL | flags::COMMAND | flags::RIGHT_CONTROL | flags::RIGHT_COMMAND),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Press,
    Repeat,
    Release,
}

impl Action {
    pub fn raw(self) -> sys::ghostty_input_action_e {
        match self {
            Action::Press => sys::ghostty_input_action_e_GHOSTTY_ACTION_PRESS,
            Action::Repeat => sys::ghostty_input_action_e_GHOSTTY_ACTION_REPEAT,
            Action::Release => sys::ghostty_input_action_e_GHOSTTY_ACTION_RELEASE,
        }
    }
}

/// Логическое событие клавиши в собственных данных Rust; текст живёт в `String`, а не в
/// заимствованной строке C.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct KeyInput {
    pub action: Action,
    pub mods: u32,
    pub consumed_mods: u32,
    pub keycode: u32,
    pub text: Option<String>,
    pub unshifted_codepoint: u32,
    pub composing: bool,
}

impl KeyInput {
    /// Текстовое событие без физической клавиши: коммит IME вне `keyDown` или вместе с ним.
    pub fn committed(action: Action, text: &str) -> KeyInput {
        KeyInput {
            action,
            mods: 0,
            consumed_mods: 0,
            keycode: NO_PHYSICAL_KEY,
            text: Some(text.to_owned()),
            unshifted_codepoint: 0,
            composing: false,
        }
    }

    /// Вызывает `f` со структурой C, текст которой заимствован у копии на время вызова.
    pub fn with_raw<R>(&self, f: impl FnOnce(sys::ghostty_input_key_s) -> R) -> Result<R, String> {
        let text = match &self.text {
            Some(text) => Some(
                CString::new(text.as_str())
                    .map_err(|_| "текст клавиши: внутренний нулевой байт".to_owned())?,
            ),
            None => None,
        };
        let raw = sys::ghostty_input_key_s {
            action: self.action.raw(),
            mods: self.mods,
            consumed_mods: self.consumed_mods,
            keycode: self.keycode,
            text: text.as_ref().map_or(std::ptr::null(), |text| text.as_ptr()),
            unshifted_codepoint: self.unshifted_codepoint,
            composing: self.composing,
        };
        let result = f(raw);
        drop(text);
        Ok(result)
    }
}

/// Что прочитано у события клавиши AppKit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventKey {
    pub key_code: u16,
    pub flags: u64,
    /// Флаги события перевода (см. [`translation_flags`]).
    pub translation_flags: u64,
    /// `characters` события перевода.
    pub characters: Option<String>,
    /// `charactersByApplyingModifiers:` события перевода без `Control`.
    pub without_control: Option<String>,
    /// `charactersByApplyingModifiers:` без модификаторов: источник `unshifted_codepoint`.
    pub unmodified: Option<String>,
    pub repeat: bool,
}

impl EventKey {
    fn input(&self, action: Action, text: Option<&str>, composing: bool) -> KeyInput {
        KeyInput {
            action,
            mods: mods_from_flags(self.flags),
            consumed_mods: consumed_mods(self.translation_flags),
            keycode: u32::from(self.key_code),
            text: text.and_then(key_event_text).map(str::to_owned),
            unshifted_codepoint: first_scalar(self.unmodified.as_deref()),
            composing,
        }
    }

    pub fn press_action(&self) -> Action {
        if self.repeat {
            Action::Repeat
        } else {
            Action::Press
        }
    }

    /// Событие нажатия с текстом события, как его видит `ghostty_surface_key_is_binding`.
    pub fn binding_probe(&self) -> KeyInput {
        self.input(Action::Press, self.characters.as_deref(), false)
    }
}

fn first_scalar(text: Option<&str>) -> u32 {
    text.and_then(|text| text.chars().next())
        .map_or(0, u32::from)
}

/// Текст события для ghostty: одиночный управляющий символ заменяется символом без `Control`,
/// символ функциональной клавиши (U+F700–U+F8FF) текста не даёт.
pub fn ghostty_characters(
    characters: Option<&str>,
    without_control: Option<&str>,
) -> Option<String> {
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

/// Текст для `ghostty_input_key_s`: пустой и начинающийся с управляющего символа ASCII не
/// передаётся, такие клавиши ghostty кодирует сам.
pub fn key_event_text(text: &str) -> Option<&str> {
    let first = text.chars().next()?;
    if u32::from(first) < 0x20 || first == '\u{7f}' {
        return None;
    }
    Some(text)
}

/// Одиночный управляющий символ во время composition принадлежит IME, а не терминалу.
pub fn suppressed_while_composing(text: Option<&str>, composing: bool) -> bool {
    let Some(text) = text else {
        return false;
    };
    let mut scalars = text.chars();
    composing
        && matches!((scalars.next(), scalars.next()), (Some(scalar), None) if u32::from(scalar) < 0x20)
}

/// После коммита preedit клавишей она сама повторяется, только если влияет на терминал: стрелки,
/// кроме простой стрелки влево (корейские IME сами оставляют курсор на месте).
fn replays_after_commit(event: &EventKey) -> bool {
    match event.key_code {
        code::DOWN | code::RIGHT | code::UP => true,
        code::LEFT => {
            event.flags & (flags::SHIFT | flags::CONTROL | flags::OPTION | flags::COMMAND) != 0
        }
        _ => false,
    }
}

/// Что известно после `interpretKeyEvents` в `keyDown`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KeyDown {
    pub event: EventKey,
    /// Текст, накопленный `insertText` за время `interpretKeyEvents`.
    pub accumulated: Vec<String>,
    pub marked_before: bool,
    pub marked_after: bool,
}

/// События, которые `keyDown` отправляет в `ghostty_surface_key`, по порядку.
pub fn plan_key_down(down: &KeyDown) -> Vec<KeyInput> {
    let event = &down.event;
    let action = event.press_action();
    let composing = down.marked_after || down.marked_before;
    let mut out = Vec::new();
    if down.marked_before && !down.accumulated.is_empty() {
        for text in &down.accumulated {
            if !suppressed_while_composing(Some(text), composing) {
                out.push(KeyInput::committed(action, text));
            }
        }
        if replays_after_commit(event) {
            out.push(event.input(action, None, false));
        }
        return out;
    }
    if !down.accumulated.is_empty() {
        for text in &down.accumulated {
            if !suppressed_while_composing(Some(text), composing) {
                out.push(event.input(action, Some(text), false));
            }
        }
        return out;
    }
    if suppressed_while_composing(event.characters.as_deref(), composing) {
        return out;
    }
    let text = ghostty_characters(
        event.characters.as_deref(),
        event.without_control.as_deref(),
    );
    out.push(event.input(action, text.as_deref(), composing));
    out
}

/// Отпускание клавиши.
pub fn key_up(event: &EventKey) -> KeyInput {
    event.input(Action::Release, None, false)
}

/// Бит модификатора клавиши `flagsChanged` и аппаратный бит её правой стороны.
fn modifier_of(key_code: u16) -> Option<(u32, Option<u64>)> {
    Some(match key_code {
        0x39 => (MODS_CAPS, None),
        0x38 => (MODS_SHIFT, None),
        0x3C => (MODS_SHIFT, Some(flags::RIGHT_SHIFT)),
        0x3B => (MODS_CTRL, None),
        0x3E => (MODS_CTRL, Some(flags::RIGHT_CONTROL)),
        0x3A => (MODS_ALT, None),
        0x3D => (MODS_ALT, Some(flags::RIGHT_OPTION)),
        0x37 => (MODS_SUPER, None),
        0x36 => (MODS_SUPER, Some(flags::RIGHT_COMMAND)),
        _ => return None,
    })
}

/// Событие клавиши модификатора: нажатие, если её бит стоит и нажата именно эта сторона, иначе
/// отпускание. `None` — не модификатор.
pub fn flags_changed(event: &EventKey) -> Option<KeyInput> {
    let (bit, right) = modifier_of(event.key_code)?;
    let mods = mods_from_flags(event.flags);
    let side = right.is_none_or(|mask| event.flags & mask != 0);
    let action = if mods & bit != 0 && side {
        Action::Press
    } else {
        Action::Release
    };
    Some(KeyInput {
        action,
        mods,
        consumed_mods: consumed_mods(event.flags),
        keycode: u32::from(event.key_code),
        text: None,
        unshifted_codepoint: 0,
        composing: false,
    })
}

/// Владельцы нажатых клавиш: получатель, съевший `keyDown`, забирает его `keyUp` и автоповтор,
/// даже если фокус ушёл. Ключ — `keyCode`.
#[derive(Debug)]
pub struct Owners<T> {
    held: Vec<(u16, T)>,
}

impl<T> Default for Owners<T> {
    fn default() -> Self {
        Owners { held: Vec::new() }
    }
}

impl<T> Owners<T> {
    /// Новое нажатие: прежний владелец того же кода заменяется.
    pub fn claim(&mut self, code: u16, owner: T) {
        self.held.retain(|(held, _)| *held != code);
        self.held.push((code, owner));
    }

    pub fn owner(&self, code: u16) -> Option<&T> {
        self.held
            .iter()
            .find(|(held, _)| *held == code)
            .map(|(_, owner)| owner)
    }

    /// Отпускание снимает владельца.
    pub fn release(&mut self, code: u16) -> Option<T> {
        let index = self.held.iter().position(|(held, _)| *held == code)?;
        Some(self.held.remove(index).1)
    }

    /// Владельцы, для которых `gone` истинно, снимаются: получатель исчез.
    pub fn forget(&mut self, gone: impl Fn(&T) -> bool) {
        self.held.retain(|(_, owner)| !gone(owner));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.held.len()
    }
}

/// Кнопка мыши ghostty по `buttonNumber` AppKit.
pub fn mouse_button(number: isize) -> sys::ghostty_input_mouse_button_e {
    match number {
        0 => sys::ghostty_input_mouse_button_e_GHOSTTY_MOUSE_LEFT,
        1 => sys::ghostty_input_mouse_button_e_GHOSTTY_MOUSE_RIGHT,
        2 => sys::ghostty_input_mouse_button_e_GHOSTTY_MOUSE_MIDDLE,
        3..=10 => sys::ghostty_input_mouse_button_e_GHOSTTY_MOUSE_FOUR + (number as u32 - 3),
        _ => sys::ghostty_input_mouse_button_e_GHOSTTY_MOUSE_UNKNOWN,
    }
}

/// Упакованные модификаторы прокрутки (`ScrollMods` в `src/input/mouse.zig`): бит 0 — точная
/// прокрутка, биты 1–3 — фаза инерции. `phase` — биты `NSEventPhase`.
pub fn scroll_mods(precise: bool, phase: u64) -> sys::ghostty_input_scroll_mods_t {
    let momentum = match phase {
        0x1 => 1,
        0x2 => 2,
        0x4 => 3,
        0x8 => 4,
        0x10 => 5,
        0x20 => 6,
        _ => 0,
    };
    i32::from(precise) | (momentum << 1)
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
            unmodified: Some(characters.to_lowercase()),
            repeat: false,
        }
    }

    #[test]
    fn modifiers_translate_with_sides() {
        assert_eq!(mods_from_flags(0), 0);
        assert_eq!(
            mods_from_flags(flags::SHIFT | flags::COMMAND),
            MODS_SHIFT | MODS_SUPER
        );
        assert_eq!(
            mods_from_flags(flags::CONTROL | flags::RIGHT_CONTROL),
            MODS_CTRL | sys::ghostty_input_mods_e_GHOSTTY_MODS_CTRL_RIGHT
        );
        assert_eq!(
            mods_from_flags(flags::OPTION | flags::CAPS_LOCK),
            MODS_ALT | MODS_CAPS
        );
        assert_eq!(
            consumed_mods(flags::SHIFT | flags::CONTROL | flags::COMMAND | flags::RIGHT_COMMAND),
            MODS_SHIFT,
            "Control и Command в получении текста не участвуют"
        );
    }

    #[test]
    fn translation_flags_follow_ghostty_and_keep_hidden_bits() {
        let hidden = 0x100;
        let event = flags::OPTION | flags::SHIFT | hidden;
        assert_eq!(
            translation_flags(event, MODS_SHIFT),
            flags::SHIFT | hidden,
            "option-as-alt снимает Option"
        );
        assert_eq!(translation_flags(event, MODS_SHIFT | MODS_ALT), event);
        assert_eq!(
            translation_flags(hidden, MODS_CTRL),
            flags::CONTROL | hidden
        );
    }

    #[test]
    fn characters_filter_control_and_function_keys() {
        assert_eq!(
            ghostty_characters(Some("a"), Some("a")).as_deref(),
            Some("a")
        );
        assert_eq!(
            ghostty_characters(Some("\u{3}"), Some("c")).as_deref(),
            Some("c"),
            "Ctrl-C: символ без Control"
        );
        assert_eq!(ghostty_characters(Some("\u{f702}"), None), None, "стрелка");
        assert_eq!(ghostty_characters(None, Some("x")), None);
        assert_eq!(
            ghostty_characters(Some("ab"), None).as_deref(),
            Some("ab"),
            "несколько символов не фильтруются"
        );
        assert_eq!(key_event_text(""), None);
        assert_eq!(key_event_text("\u{1b}"), None);
        assert_eq!(key_event_text("\u{7f}"), None);
        assert_eq!(key_event_text("ж"), Some("ж"));
    }

    #[test]
    fn plain_key_down_uses_event_characters() {
        let down = KeyDown {
            event: event(0, 0, "a"),
            ..KeyDown::default()
        };
        let plan = plan_key_down(&down);
        assert_eq!(
            plan,
            [KeyInput {
                action: Action::Press,
                mods: 0,
                consumed_mods: 0,
                keycode: 0,
                text: Some("a".to_owned()),
                unshifted_codepoint: u32::from('a'),
                composing: false,
            }]
        );
        let ctrl = KeyDown {
            event: EventKey {
                without_control: Some("c".to_owned()),
                ..event(8, flags::CONTROL, "\u{3}")
            },
            ..KeyDown::default()
        };
        let plan = plan_key_down(&ctrl);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].text.as_deref(), Some("c"));
        assert_eq!((plan[0].mods, plan[0].consumed_mods), (MODS_CTRL, 0));
        let arrow = KeyDown {
            event: event(code::LEFT, 0, "\u{f702}"),
            ..KeyDown::default()
        };
        assert_eq!(plan_key_down(&arrow)[0].text, None);
        let esc = KeyDown {
            event: event(53, 0, "\u{1b}"),
            ..KeyDown::default()
        };
        assert_eq!(plan_key_down(&esc)[0].text, None, "Esc кодирует ghostty");
    }

    #[test]
    fn accumulated_text_goes_in_one_event_per_insert() {
        let down = KeyDown {
            event: event(41, 0, ";"),
            accumulated: vec!["ж".to_owned()],
            ..KeyDown::default()
        };
        let plan = plan_key_down(&down);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].text.as_deref(), Some("ж"));
        assert_eq!(plan[0].keycode, 41, "физическая клавиша сохраняется");
        let repeat = KeyDown {
            event: EventKey {
                repeat: true,
                ..event(0, 0, "a")
            },
            accumulated: vec!["a".to_owned()],
            ..KeyDown::default()
        };
        assert_eq!(plan_key_down(&repeat)[0].action, Action::Repeat);
        let control = KeyDown {
            event: event(4, flags::CONTROL, "\u{8}"),
            accumulated: vec!["\u{8}".to_owned()],
            ..KeyDown::default()
        };
        let plan = plan_key_down(&control);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].text, None, "управляющий текст ghostty кодирует сам");
    }

    #[test]
    fn composition_commit_and_cancel() {
        // Коммит preedit клавишей Return: текст уходит без физической клавиши, сама клавиша не
        // повторяется.
        let commit = KeyDown {
            event: event(36, 0, "\r"),
            accumulated: vec!["か".to_owned()],
            marked_before: true,
            marked_after: false,
        };
        assert_eq!(
            plan_key_down(&commit),
            [KeyInput::committed(Action::Press, "か")]
        );
        // Коммит стрелкой вниз: стрелка повторяется после текста.
        let arrow = KeyDown {
            event: event(code::DOWN, 0, "\u{f701}"),
            accumulated: vec!["か".to_owned()],
            marked_before: true,
            marked_after: false,
        };
        let plan = plan_key_down(&arrow);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[1].keycode, u32::from(code::DOWN));
        assert!(!plan[1].composing);
        // Простая стрелка влево не повторяется, со Shift — повторяется.
        let left = KeyDown {
            event: event(code::LEFT, 0, "\u{f702}"),
            ..arrow.clone()
        };
        assert_eq!(plan_key_down(&left).len(), 1);
        let shift_left = KeyDown {
            event: event(code::LEFT, flags::SHIFT, "\u{f702}"),
            ..arrow.clone()
        };
        assert_eq!(plan_key_down(&shift_left).len(), 2);
        // Esc при marked text: composition снята, управляющий символ терминалу не уходит.
        let cancel = KeyDown {
            event: event(53, 0, "\u{1b}"),
            accumulated: Vec::new(),
            marked_before: true,
            marked_after: false,
        };
        assert!(plan_key_down(&cancel).is_empty());
        // Буква во время composition без коммита — событие с признаком composing.
        let composing = KeyDown {
            event: event(40, 0, "k"),
            accumulated: Vec::new(),
            marked_before: false,
            marked_after: true,
        };
        let plan = plan_key_down(&composing);
        assert_eq!(plan.len(), 1);
        assert!(plan[0].composing);
    }

    #[test]
    fn key_up_and_flags_changed() {
        let up = key_up(&event(0, flags::SHIFT, "A"));
        assert_eq!(up.action, Action::Release);
        assert_eq!(up.text, None);
        assert_eq!(up.mods, MODS_SHIFT);
        let press = flags_changed(&event(0x38, flags::SHIFT, "")).unwrap();
        assert_eq!((press.action, press.mods), (Action::Press, MODS_SHIFT));
        let release = flags_changed(&event(0x38, 0, "")).unwrap();
        assert_eq!(release.action, Action::Release);
        // Правый Shift отпущен при нажатом левом: бит Shift стоит, но сторона не та.
        let right_up = flags_changed(&event(0x3C, flags::SHIFT, "")).unwrap();
        assert_eq!(right_up.action, Action::Release);
        let right_down =
            flags_changed(&event(0x3C, flags::SHIFT | flags::RIGHT_SHIFT, "")).unwrap();
        assert_eq!(right_down.action, Action::Press);
        assert_eq!(flags_changed(&event(0, 0, "")), None);
    }

    #[test]
    fn raw_key_borrows_text_for_the_call() {
        let input = KeyInput::committed(Action::Press, "жук");
        let seen = input
            .with_raw(|raw| {
                assert_eq!(raw.keycode, NO_PHYSICAL_KEY);
                assert_eq!(raw.action, sys::ghostty_input_action_e_GHOSTTY_ACTION_PRESS);
                // SAFETY: текст жив на время замыкания.
                unsafe { std::ffi::CStr::from_ptr(raw.text) }
                    .to_str()
                    .map(str::to_owned)
            })
            .unwrap();
        assert_eq!(seen.as_deref(), Ok("жук"));
        let without = KeyInput {
            text: None,
            ..input.clone()
        };
        assert!(without.with_raw(|raw| raw.text.is_null()).unwrap());
        let nul = KeyInput {
            text: Some("a\0b".to_owned()),
            ..input
        };
        assert!(nul.with_raw(|_| ()).is_err());
    }

    #[test]
    fn owners_claim_release_and_forget() {
        let mut owners = Owners::default();
        owners.claim(0, "A");
        owners.claim(1, "B");
        assert_eq!(owners.owner(0), Some(&"A"));
        owners.claim(0, "C");
        assert_eq!(owners.owner(0), Some(&"C"), "новое нажатие заменяет");
        assert_eq!(owners.len(), 2);
        assert_eq!(owners.release(0), Some("C"));
        assert_eq!(owners.release(0), None);
        owners.forget(|owner| *owner == "B");
        assert_eq!(owners.owner(1), None);
        assert_eq!(owners.len(), 0);
    }

    #[test]
    fn mouse_buttons_and_scroll_packing() {
        assert_eq!(
            mouse_button(0),
            sys::ghostty_input_mouse_button_e_GHOSTTY_MOUSE_LEFT
        );
        assert_eq!(
            mouse_button(2),
            sys::ghostty_input_mouse_button_e_GHOSTTY_MOUSE_MIDDLE
        );
        assert_eq!(
            mouse_button(3),
            sys::ghostty_input_mouse_button_e_GHOSTTY_MOUSE_FOUR
        );
        assert_eq!(
            mouse_button(10),
            sys::ghostty_input_mouse_button_e_GHOSTTY_MOUSE_ELEVEN
        );
        assert_eq!(
            mouse_button(11),
            sys::ghostty_input_mouse_button_e_GHOSTTY_MOUSE_UNKNOWN
        );
        assert_eq!(scroll_mods(false, 0), 0);
        assert_eq!(scroll_mods(true, 0), 1);
        assert_eq!(scroll_mods(true, 0x4), 1 | (3 << 1), "changed");
        assert_eq!(scroll_mods(false, 0x20), 6 << 1, "may begin");
    }
}
