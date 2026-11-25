//! Чистая модель клавиатуры (ADR-0010, «Уточнено в P1.6 (планирование)»): аккорды по физической
//! клавише, последовательности, контексты и их порядок, привязки по умолчанию, разбор
//! `keymap.toml`, автомат аккорда выхода, история переходов и подсказки строки статуса.
//!
//! Имена действий здесь строки: их проверяет приложение по реестру плагинов и командам хоста.

#![forbid(unsafe_code)]

mod chord;
mod defaults;
mod exit_chord;
mod hints;
mod jump_list;
mod keymap;
mod sequence;
mod toml;

pub use chord::{Chord, ChordError, KeyCode, Mods};
pub use defaults::{DefaultsError, KeyDefault, LIST_KEYS, PanelKeys};
pub use exit_chord::{Cancel, ExitChord, KeyDown};
pub use hints::{Hint, HintQuery, PanelHint, StatusText, status_hints};
pub use jump_list::{JUMP_DEPTH, JumpList};
pub use keymap::{Binding, Context, Diagnostic, DiagnosticKind, Keymap, Lookup, Mode, Source};
pub use sequence::{MAX_SEQUENCE, Match, Overlap, Sequence, SequenceError, SequenceTree};
pub use toml::{ActionCheck, ActionError};
