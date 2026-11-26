//! Локальный монитор событий приложения для маршрутизатора клавиш (ADR-0010, «Уточнено в P1.6
//! (планирование)») и снимок того, кто держит ввод в ключевом окне.
//!
//! Монитор срабатывает до `sendEvent:` и до эквивалентов меню. Вложенные циклы слежения (раскрытое
//! меню, перетаскивание) его обходят. Снимок описывает окно до доставки события: смену первого
//! ответчика, которую вызовет само событие, он не видит.

use std::ptr::NonNull;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{Message, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSButton, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSResponder,
    NSText,
};
use objc2_foundation::{NSNotFound, NSObjectProtocol, NSRange, NSString};

use crate::App;

/// Имя класса полосы вкладок: `Space` и `Enter` при её фокусе подтверждают вкладку.
const TAB_STRIP_CLASS: &str = "ShellioTabStrip";

/// Модификаторы аккорда. CapsLock, `Fn` и признак цифрового блока в аккорд не входят.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyMods {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

/// Нажатие клавиши. Значение хранит исходное событие: автомат аккорда выхода держит его до второго
/// аккорда и возвращает получателю.
#[derive(Debug, Clone)]
pub struct KeyStroke {
    key_code: u16,
    mods: KeyMods,
    repeat: bool,
    event: Retained<NSEvent>,
}

impl KeyStroke {
    /// Виртуальный код физической клавиши.
    pub fn key_code(&self) -> u16 {
        self.key_code
    }

    pub fn mods(&self) -> KeyMods {
        self.mods
    }

    /// Автоповтор удержанной клавиши.
    pub fn repeat(&self) -> bool {
        self.repeat
    }

    /// Символы события по действующему источнику ввода: при кириллической раскладке они не совпадают
    /// с именем клавиши.
    pub fn characters(&self) -> Option<String> {
        self.event.characters().map(|text| text.to_string())
    }

    /// Возвращает пару нажатия и отпускания первому ответчику ключевого окна напрямую: мимо
    /// очереди событий, монитора и эквивалентов меню. Отпускание строится по нажатию: исходное
    /// монитор поглотил. `false` — ключевого окна или ответчика нет, пара не доставлена.
    pub fn replay(&self, app: &App) -> bool {
        let Some(responder) = NSApplication::sharedApplication(app.mtm())
            .keyWindow()
            .and_then(|window| window.firstResponder())
        else {
            return false;
        };
        let down = &self.event;
        responder.keyDown(down);
        let characters = down.characters().unwrap_or_default();
        let ignoring = down.charactersIgnoringModifiers().unwrap_or_default();
        let up = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            NSEventType::KeyUp,
            down.locationInWindow(),
            down.modifierFlags(),
            down.timestamp(),
            down.windowNumber(),
            None,
            &characters,
            &ignoring,
            false,
            down.keyCode(),
        );
        if let Some(up) = up {
            responder.keyUp(&up);
        }
        true
    }
}

/// Событие, которое видит монитор.
#[derive(Debug, Clone)]
pub enum MonitorEvent {
    KeyDown(KeyStroke),
    KeyUp {
        key_code: u16,
    },
    /// Смена модификаторов: не клавиша.
    FlagsChanged,
    /// Нажатие любой кнопки мыши в окне с номером `window` (0 — вне окон приложения).
    MouseDown {
        window: i64,
    },
    ScrollWheel,
}

/// Ответ монитору. Поглотить можно нажатие и отпускание клавиши и нажатие мыши: остальные события
/// идут AppKit при любом ответе.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Событие идёт AppKit без изменений.
    Pass,
    /// Событие до AppKit не доходит.
    Swallow,
}

/// Кто держит ввод в ключевом окне.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyResponder {
    /// Ключевого окна у приложения нет.
    None,
    /// Вид без собственных клавиш маршрутизатора: тело панели, список, само окно.
    Other,
    /// Редактор поля.
    Field,
    /// Полоса вкладок с клавиатурным фокусом.
    TabStrip,
    /// Кнопка с кольцом фокуса.
    Button,
}

/// Снимок ввода ключевого окна в момент события.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyFocus {
    pub responder: KeyResponder,
    /// Первый ответчик составляет текст в IME (`hasMarkedText`).
    pub composing: bool,
}

/// Установленный монитор. Снимается вместе со значением.
#[derive(Debug)]
pub struct KeyMonitor {
    token: Retained<AnyObject>,
}

impl KeyMonitor {
    /// Ставит монитор нажатий и отпусканий клавиш, смены модификаторов, нажатий мыши и прокрутки.
    /// `handler` вызывается на главном потоке до доставки события и не должен ждать. `None` —
    /// AppKit монитор не поставил.
    ///
    /// Монитор держит `handler` до снятия: окно и виды в него сильной ссылкой не попадают.
    pub fn install(
        app: &App,
        handler: impl Fn(&App, MonitorEvent) -> Verdict + 'static,
    ) -> Option<Self> {
        let app = *app;
        let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
            // SAFETY: AppKit передаёт монитору живое событие на время вызова.
            let seen = unsafe { event.as_ref() };
            let Some(kind) = kind(seen.r#type()) else {
                return event.as_ptr();
            };
            let verdict = handler(&app, read(kind, seen));
            if verdict == Verdict::Swallow && kind.swallowable() {
                std::ptr::null_mut()
            } else {
                event.as_ptr()
            }
        });
        // SAFETY: блок принимает событие и возвращает его же или nil, как требует монитор; AppKit
        // копирует блок и держит его до `removeMonitor:`.
        let token =
            unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask(), &block) }?;
        Some(Self { token })
    }
}

impl Drop for KeyMonitor {
    fn drop(&mut self) {
        // SAFETY: `token` — объект, который вернул `addLocalMonitorForEventsMatchingMask:handler:`,
        // и снимается он один раз.
        unsafe { NSEvent::removeMonitor(&self.token) };
    }
}

impl App {
    /// Кто держит ввод в ключевом окне приложения прямо сейчас.
    pub fn key_focus(&self) -> KeyFocus {
        let Some(window) = NSApplication::sharedApplication(self.mtm()).keyWindow() else {
            return KeyFocus {
                responder: KeyResponder::None,
                composing: false,
            };
        };
        match window.firstResponder() {
            Some(responder) => KeyFocus {
                responder: responder_kind(&responder),
                composing: composing(&responder),
            },
            None => KeyFocus {
                responder: KeyResponder::Other,
                composing: false,
            },
        }
    }
}

impl App {
    /// Снять составление IME у первого ответчика ключевого окна без коммита (ADR-0010, «Уточнено
    /// в P0.3»): размеченный текст заменяется пустым, метод ввода сбрасывает своё состояние.
    /// `true` — составление было и снято.
    pub fn discard_composition(&self) -> bool {
        let Some(window) = NSApplication::sharedApplication(self.mtm()).keyWindow() else {
            return false;
        };
        let Some(responder) = window.firstResponder() else {
            return false;
        };
        if !composing(&responder)
            || !responder.respondsToSelector(sel!(setMarkedText:selectedRange:replacementRange:))
            || !responder.respondsToSelector(sel!(unmarkText))
        {
            return false;
        }
        let empty = NSString::new();
        let none = NSRange::new(NSNotFound as usize, 0);
        // SAFETY: ответчик с размеченным текстом реализует `NSTextInputClient`: оба метода есть
        // (проверено выше), аргументы — строка и диапазоны по протоколу.
        unsafe {
            let _: () = msg_send![
                &*responder,
                setMarkedText: &*empty,
                selectedRange: NSRange::new(0, 0),
                replacementRange: none
            ];
            let _: () = msg_send![&*responder, unmarkText];
        }
        if responder.respondsToSelector(sel!(inputContext)) {
            // SAFETY: `inputContext` без аргументов возвращает `NSTextInputContext` или nil, у
            // контекста есть `discardMarkedText`.
            unsafe {
                let context: Option<Retained<AnyObject>> = msg_send![&*responder, inputContext];
                if let Some(context) = context {
                    let _: () = msg_send![&*context, discardMarkedText];
                }
            }
        }
        true
    }
}

fn responder_kind(responder: &NSResponder) -> KeyResponder {
    if responder
        .downcast_ref::<NSText>()
        .is_some_and(NSText::isFieldEditor)
    {
        return KeyResponder::Field;
    }
    if responder.class().name().to_bytes() == TAB_STRIP_CLASS.as_bytes() {
        return KeyResponder::TabStrip;
    }
    if responder.downcast_ref::<NSButton>().is_some() {
        return KeyResponder::Button;
    }
    KeyResponder::Other
}

fn composing(responder: &NSResponder) -> bool {
    if !responder.respondsToSelector(sel!(hasMarkedText)) {
        return false;
    }
    // SAFETY: ответчик реализует `hasMarkedText` протокола `NSTextInputClient`: метод без
    // аргументов возвращает BOOL.
    unsafe { msg_send![responder, hasMarkedText] }
}

/// Вид события для маршрутизатора.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    KeyDown,
    KeyUp,
    FlagsChanged,
    MouseDown,
    ScrollWheel,
}

impl Kind {
    fn swallowable(self) -> bool {
        match self {
            Kind::KeyDown | Kind::KeyUp | Kind::MouseDown => true,
            Kind::FlagsChanged | Kind::ScrollWheel => false,
        }
    }
}

fn kind(event: NSEventType) -> Option<Kind> {
    if event == NSEventType::KeyDown {
        Some(Kind::KeyDown)
    } else if event == NSEventType::KeyUp {
        Some(Kind::KeyUp)
    } else if event == NSEventType::FlagsChanged {
        Some(Kind::FlagsChanged)
    } else if event == NSEventType::LeftMouseDown
        || event == NSEventType::RightMouseDown
        || event == NSEventType::OtherMouseDown
    {
        Some(Kind::MouseDown)
    } else if event == NSEventType::ScrollWheel {
        Some(Kind::ScrollWheel)
    } else {
        None
    }
}

fn mask() -> NSEventMask {
    NSEventMask::KeyDown
        | NSEventMask::KeyUp
        | NSEventMask::FlagsChanged
        | NSEventMask::LeftMouseDown
        | NSEventMask::RightMouseDown
        | NSEventMask::OtherMouseDown
        | NSEventMask::ScrollWheel
}

pub(crate) fn mods(flags: NSEventModifierFlags) -> KeyMods {
    KeyMods {
        cmd: flags.contains(NSEventModifierFlags::Command),
        ctrl: flags.contains(NSEventModifierFlags::Control),
        alt: flags.contains(NSEventModifierFlags::Option),
        shift: flags.contains(NSEventModifierFlags::Shift),
    }
}

/// Свойства клавиши читаются только у событий клавиши: у `flagsChanged` и событий мыши `keyCode`,
/// `isARepeat` и `characters` дают исключение Objective-C.
fn read(kind: Kind, event: &NSEvent) -> MonitorEvent {
    match kind {
        Kind::KeyDown => MonitorEvent::KeyDown(KeyStroke {
            key_code: event.keyCode(),
            mods: mods(event.modifierFlags()),
            repeat: event.isARepeat(),
            event: event.retain(),
        }),
        Kind::KeyUp => MonitorEvent::KeyUp {
            key_code: event.keyCode(),
        },
        Kind::FlagsChanged => MonitorEvent::FlagsChanged,
        Kind::MouseDown => MonitorEvent::MouseDown {
            window: event.windowNumber() as i64,
        },
        Kind::ScrollWheel => MonitorEvent::ScrollWheel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_types_map_to_router_kinds() {
        assert_eq!(kind(NSEventType::KeyDown), Some(Kind::KeyDown));
        assert_eq!(kind(NSEventType::KeyUp), Some(Kind::KeyUp));
        assert_eq!(kind(NSEventType::FlagsChanged), Some(Kind::FlagsChanged));
        assert_eq!(kind(NSEventType::LeftMouseDown), Some(Kind::MouseDown));
        assert_eq!(kind(NSEventType::RightMouseDown), Some(Kind::MouseDown));
        assert_eq!(kind(NSEventType::OtherMouseDown), Some(Kind::MouseDown));
        assert_eq!(kind(NSEventType::ScrollWheel), Some(Kind::ScrollWheel));
        assert_eq!(kind(NSEventType::LeftMouseUp), None);
        assert_eq!(kind(NSEventType::MouseMoved), None);
        assert_eq!(kind(NSEventType::ApplicationDefined), None);
    }

    #[test]
    fn mask_covers_exactly_the_mapped_event_types() {
        let mask = mask();
        for event in [
            NSEventType::KeyDown,
            NSEventType::KeyUp,
            NSEventType::FlagsChanged,
            NSEventType::LeftMouseDown,
            NSEventType::RightMouseDown,
            NSEventType::OtherMouseDown,
            NSEventType::ScrollWheel,
            NSEventType::LeftMouseUp,
            NSEventType::MouseMoved,
            NSEventType::LeftMouseDragged,
            NSEventType::ApplicationDefined,
        ] {
            let bit = NSEventMask(1 << event.0);
            assert_eq!(mask.contains(bit), kind(event).is_some(), "{event:?}");
        }
    }

    #[test]
    fn key_and_mouse_down_events_can_be_swallowed() {
        assert!(Kind::KeyDown.swallowable());
        assert!(Kind::KeyUp.swallowable());
        assert!(
            Kind::MouseDown.swallowable(),
            "щелчок вне палитры не доходит до слоя под ней"
        );
        assert!(!Kind::FlagsChanged.swallowable());
        assert!(!Kind::ScrollWheel.swallowable());
    }

    #[test]
    fn modifiers_ignore_caps_lock_function_and_numeric_pad() {
        let noise = NSEventModifierFlags::CapsLock
            | NSEventModifierFlags::Function
            | NSEventModifierFlags::NumericPad;
        assert_eq!(mods(noise), KeyMods::default());
        assert_eq!(
            mods(noise | NSEventModifierFlags::Control | NSEventModifierFlags::Shift),
            KeyMods {
                cmd: false,
                ctrl: true,
                alt: false,
                shift: true,
            }
        );
        assert_eq!(
            mods(NSEventModifierFlags::Command | NSEventModifierFlags::Option),
            KeyMods {
                cmd: true,
                ctrl: false,
                alt: true,
                shift: false,
            }
        );
    }
}
