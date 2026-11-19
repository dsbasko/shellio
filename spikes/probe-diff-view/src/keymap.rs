//! Разбор клавиш вида diff в действия: команды состояния вида из ADR-0010 и `docs/ux.md`, две
//! клавиши у `gg`, `]c` и `[c`. Модуль не зависит от AppKit: вид передаёт
//! `charactersIgnoringModifiers` и модификаторы. Один декодер на сцену, его используют оба
//! варианта отрисовки.

use crate::model::state::Command;

/// Действие клавиши.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Command(Command),
    /// Клавиша открытия фикстуры, только для стенда пробы: `o`.
    Open,
}

/// Клавиша: `charactersIgnoringModifiers` и модификаторы.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyInput {
    pub chars: String,
    pub control: bool,
    pub command: bool,
    pub option: bool,
}

impl KeyInput {
    pub fn plain(chars: &str) -> KeyInput {
        KeyInput {
            chars: chars.to_owned(),
            control: false,
            command: false,
            option: false,
        }
    }

    pub fn control(chars: &str) -> KeyInput {
        KeyInput {
            control: true,
            ..KeyInput::plain(chars)
        }
    }
}

/// Первая клавиша двухклавишной команды.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prefix {
    G,
    RightBracket,
    LeftBracket,
}

#[derive(Debug, Default)]
pub struct Decoder {
    pending: Option<Prefix>,
}

impl Decoder {
    /// Действие клавиши. Незнакомая клавиша и клавиша, которая не продолжает начатую команду,
    /// сбрасывают начало.
    pub fn feed(&mut self, key: &KeyInput) -> Option<Action> {
        let pending = self.pending.take();
        if key.command || key.option {
            return None;
        }
        let chars = key.chars.as_str();
        if key.control {
            return match chars {
                "d" => Some(Action::Command(Command::HalfDown)),
                "u" => Some(Action::Command(Command::HalfUp)),
                "o" => Some(Action::Command(Command::JumpBack)),
                _ => None,
            };
        }
        match (pending, chars) {
            (Some(Prefix::G), "g") => return Some(Action::Command(Command::Top)),
            (Some(Prefix::RightBracket), "c") => {
                return Some(Action::Command(Command::NextChange));
            }
            (Some(Prefix::LeftBracket), "c") => {
                return Some(Action::Command(Command::PrevChange));
            }
            _ => {}
        }
        match chars {
            "j" => Some(Action::Command(Command::Down)),
            "k" => Some(Action::Command(Command::Up)),
            "G" => Some(Action::Command(Command::Bottom)),
            "v" => Some(Action::Command(Command::ToggleVisual)),
            "\u{1b}" => Some(Action::Command(Command::ClearSelection)),
            "o" => Some(Action::Open),
            "g" => {
                self.pending = Some(Prefix::G);
                None
            }
            "]" => {
                self.pending = Some(Prefix::RightBracket);
                None
            }
            "[" => {
                self.pending = Some(Prefix::LeftBracket);
                None
            }
            _ => None,
        }
    }

    /// Начата двухклавишная команда.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(decoder: &mut Decoder, keys: &[KeyInput]) -> Vec<Option<Action>> {
        keys.iter().map(|key| decoder.feed(key)).collect()
    }

    #[test]
    fn single_keys_map_to_commands() {
        let mut decoder = Decoder::default();
        for (key, command) in [
            (KeyInput::plain("j"), Command::Down),
            (KeyInput::plain("k"), Command::Up),
            (KeyInput::plain("G"), Command::Bottom),
            (KeyInput::plain("v"), Command::ToggleVisual),
            (KeyInput::plain("\u{1b}"), Command::ClearSelection),
            (KeyInput::control("d"), Command::HalfDown),
            (KeyInput::control("u"), Command::HalfUp),
            (KeyInput::control("o"), Command::JumpBack),
        ] {
            assert_eq!(
                decoder.feed(&key),
                Some(Action::Command(command)),
                "{key:?}"
            );
        }
        assert_eq!(decoder.feed(&KeyInput::plain("o")), Some(Action::Open));
    }

    #[test]
    fn two_key_commands() {
        let mut decoder = Decoder::default();
        assert_eq!(
            feed(&mut decoder, &[KeyInput::plain("g"), KeyInput::plain("g")]),
            vec![None, Some(Action::Command(Command::Top))]
        );
        assert_eq!(
            feed(&mut decoder, &[KeyInput::plain("]"), KeyInput::plain("c")]),
            vec![None, Some(Action::Command(Command::NextChange))]
        );
        assert_eq!(
            feed(&mut decoder, &[KeyInput::plain("["), KeyInput::plain("c")]),
            vec![None, Some(Action::Command(Command::PrevChange))]
        );
        assert!(!decoder.is_pending());
    }

    #[test]
    fn broken_prefix_is_dropped_and_the_key_counts_alone() {
        let mut decoder = Decoder::default();
        assert_eq!(
            feed(&mut decoder, &[KeyInput::plain("g"), KeyInput::plain("j")]),
            vec![None, Some(Action::Command(Command::Down))]
        );
        // `c` без префикса — черновик задачи 8, здесь не действие.
        assert_eq!(decoder.feed(&KeyInput::plain("c")), None);
        assert_eq!(
            feed(
                &mut decoder,
                &[KeyInput::plain("]"), KeyInput::control("d")]
            ),
            vec![None, Some(Action::Command(Command::HalfDown))]
        );
        assert_eq!(
            feed(&mut decoder, &[KeyInput::plain("g"), KeyInput::plain("x")]),
            vec![None, None]
        );
        assert_eq!(decoder.feed(&KeyInput::plain("g")), None);
        assert!(decoder.is_pending());
    }

    #[test]
    fn command_and_option_are_not_diff_keys() {
        let mut decoder = Decoder::default();
        let mut key = KeyInput::plain("j");
        key.command = true;
        assert_eq!(decoder.feed(&key), None);
        let mut key = KeyInput::plain("j");
        key.option = true;
        assert_eq!(decoder.feed(&key), None);
    }
}
