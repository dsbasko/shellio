//! Автомат аккорда выхода (ADR-0010, «Уточнено в P0.3» и «Аккорд выхода»): первое нажатие
//! переводит в `Navigation` без передачи, второе подряд возвращает прежнему получателю исходную
//! пару нажатия и отпускания. Окна времени нет.

use crate::chord::{Chord, KeyCode};

/// Что снимает ожидание второго аккорда. Повтора после снятия нет, режим остаётся `Navigation`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cancel {
    /// Сменился первый ответчик.
    Responder,
    /// Сменилась панель с фокусом.
    Focus,
    MouseDown,
    ScrollWheel,
    /// Открылось наложение.
    Overlay,
    /// Приложение перестало быть активным.
    Resigned,
}

/// Решение автомата о нажатии клавиши.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyDown<R, E> {
    /// Автомат нажатие не трогает: его разбирает маршрутизатор.
    Pass,
    /// Нажатие съедено: автоповтор первого аккорда.
    Swallow,
    /// Второй аккорд: получателю идёт сохранённая пара, режим возвращается в `Input`.
    Replay { receiver: R, event: E },
}

#[derive(Debug, PartialEq, Eq)]
struct Armed<R, E> {
    receiver: R,
    event: E,
    chord: Chord,
}

/// `R` — получатель ввода, `E` — исходное событие нажатия. Аккорд берётся из привязки
/// `mode.navigation` контекста `input`, автомат помнит тот, которым его взвели.
#[derive(Debug, PartialEq, Eq)]
pub struct ExitChord<R, E> {
    armed: Option<Armed<R, E>>,
    /// Клавиша, отпускание которой съедается: нажатие получателю не доставлялось.
    swallow_up: Option<KeyCode>,
}

impl<R, E> Default for ExitChord<R, E> {
    fn default() -> Self {
        Self {
            armed: None,
            swallow_up: None,
        }
    }
}

impl<R: PartialEq, E> ExitChord<R, E> {
    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// Получатель, которому вернётся пара при втором аккорде.
    pub fn receiver(&self) -> Option<&R> {
        self.armed.as_ref().map(|armed| &armed.receiver)
    }

    /// Первый аккорд в `Input`: событие получателю не передаётся, маршрутизатор переводит панель
    /// в `Navigation`. Автоповтор автомат не взводит: `false`, нажатие съедается как прежде.
    pub fn arm(&mut self, receiver: R, event: E, chord: Chord, repeat: bool) -> bool {
        if repeat {
            return false;
        }
        self.swallow_up = Some(chord.key);
        self.armed = Some(Armed {
            receiver,
            event,
            chord,
        });
        true
    }

    /// Нажатие клавиши, не модификатора. Другая клавиша и её автоповтор ожидание снимают и идут
    /// маршрутизатору.
    pub fn key_down(&mut self, chord: Chord, repeat: bool) -> KeyDown<R, E> {
        let Some(armed) = self.armed.take() else {
            return KeyDown::Pass;
        };
        if chord != armed.chord {
            return KeyDown::Pass;
        }
        if repeat {
            self.armed = Some(armed);
            return KeyDown::Swallow;
        }
        self.swallow_up = Some(chord.key);
        KeyDown::Replay {
            receiver: armed.receiver,
            event: armed.event,
        }
    }

    /// Отпускание клавиши: `true` — съедено. Отпускание обоих аккордов выхода получателю не
    /// идёт: его пару доставляет повтор. Ожидание отпускание не снимает.
    pub fn key_up(&mut self, key: KeyCode) -> bool {
        if self.swallow_up == Some(key) {
            self.swallow_up = None;
            return true;
        }
        false
    }

    /// Смена модификаторов ожидание не снимает: удержанный `Ctrl` и модификатор, нажатый между
    /// аккордами, — не клавиша.
    pub fn flags_changed(&self) {}

    /// `true`, если ожидание было и снято.
    pub fn cancel(&mut self, _cause: Cancel) -> bool {
        self.armed.take().is_some()
    }

    /// Получатель закрыт: его ожидание снимается, чужое остаётся.
    pub fn receiver_closed(&mut self, receiver: &R) -> bool {
        if self.receiver() == Some(receiver) {
            self.armed = None;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Machine = ExitChord<&'static str, u32>;

    fn chord(name: &str) -> Chord {
        Chord::parse(name).unwrap()
    }

    fn exit() -> Chord {
        chord("Ctrl-\\")
    }

    fn armed() -> Machine {
        let mut machine = Machine::default();
        assert!(machine.arm("surface", 7, exit(), false));
        machine
    }

    #[test]
    fn idle_machine_passes_everything() {
        let mut machine = Machine::default();
        assert!(!machine.is_armed());
        assert_eq!(machine.key_down(exit(), false), KeyDown::Pass);
        assert_eq!(machine.key_down(chord("j"), false), KeyDown::Pass);
        assert!(!machine.key_up(exit().key));
        assert!(!machine.cancel(Cancel::MouseDown));
    }

    #[test]
    fn second_chord_replays_the_saved_pair_once() {
        let mut machine = armed();
        assert_eq!(machine.receiver(), Some(&"surface"));
        // Отпускание первого аккорда съедено: получатель его нажатия не видел.
        assert!(machine.key_up(exit().key));
        assert!(!machine.key_up(exit().key));
        assert!(machine.is_armed());

        assert_eq!(
            machine.key_down(exit(), false),
            KeyDown::Replay {
                receiver: "surface",
                event: 7
            }
        );
        assert!(!machine.is_armed());
        // Физическое отпускание второго аккорда тоже съедено: пару доставил повтор.
        assert!(machine.key_up(exit().key));
        assert!(!machine.key_up(exit().key));
        // Третий аккорд — уже обычное нажатие.
        assert_eq!(machine.key_down(exit(), false), KeyDown::Pass);
    }

    #[test]
    fn autorepeat_of_the_first_chord_is_swallowed_and_never_arms() {
        let mut machine = Machine::default();
        assert!(!machine.arm("surface", 7, exit(), true));
        assert!(!machine.is_armed());

        let mut machine = armed();
        assert_eq!(machine.key_down(exit(), true), KeyDown::Swallow);
        assert_eq!(machine.key_down(exit(), true), KeyDown::Swallow);
        assert!(machine.is_armed());
        assert!(matches!(
            machine.key_down(exit(), false),
            KeyDown::Replay { .. }
        ));
    }

    #[test]
    fn modifiers_and_key_up_keep_waiting() {
        let mut machine = armed();
        machine.flags_changed();
        assert!(!machine.key_up(chord("j").key));
        assert!(machine.is_armed());
        assert!(matches!(
            machine.key_down(exit(), false),
            KeyDown::Replay { .. }
        ));
    }

    #[test]
    fn another_key_and_its_autorepeat_cancel_without_replay() {
        for repeat in [false, true] {
            let mut machine = armed();
            assert_eq!(machine.key_down(chord("j"), repeat), KeyDown::Pass);
            assert!(!machine.is_armed());
            assert_eq!(machine.key_down(exit(), false), KeyDown::Pass);
        }
        // Та же клавиша с другими модификаторами — другая клавиша.
        let mut machine = armed();
        assert_eq!(machine.key_down(chord("\\"), false), KeyDown::Pass);
        assert!(!machine.is_armed());
    }

    #[test]
    fn every_cancel_cause_disarms_without_replay() {
        for cause in [
            Cancel::Responder,
            Cancel::Focus,
            Cancel::MouseDown,
            Cancel::ScrollWheel,
            Cancel::Overlay,
            Cancel::Resigned,
        ] {
            let mut machine = armed();
            assert!(machine.cancel(cause), "{cause:?}");
            assert!(!machine.is_armed());
            assert!(!machine.cancel(cause));
            assert_eq!(machine.key_down(exit(), false), KeyDown::Pass);
            // Отпускание первого аккорда после снятия всё равно съедается.
            assert!(machine.key_up(exit().key));
        }
    }

    #[test]
    fn closing_the_receiver_disarms_only_its_own_wait() {
        let mut machine = armed();
        assert!(!machine.receiver_closed(&"other"));
        assert!(machine.is_armed());
        assert!(machine.receiver_closed(&"surface"));
        assert_eq!(machine.key_down(exit(), false), KeyDown::Pass);
    }

    #[test]
    fn machine_follows_the_bound_chord() {
        let custom = chord("Ctrl-]");
        let mut machine = Machine::default();
        assert!(machine.arm("surface", 9, custom, false));
        assert_eq!(machine.key_down(exit(), false), KeyDown::Pass);
        assert!(machine.arm("surface", 9, custom, false));
        assert_eq!(
            machine.key_down(custom, false),
            KeyDown::Replay {
                receiver: "surface",
                event: 9
            }
        );
    }
}
