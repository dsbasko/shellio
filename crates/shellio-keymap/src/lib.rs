//! Чистая модель клавиатуры (ADR-0010, «Уточнено в P1.6 (планирование)»): аккорды по физической
//! клавише, последовательности, контексты и их порядок, привязки по умолчанию, разбор
//! `keymap.toml`, автомат аккорда выхода, история переходов, подсказки строки статуса и чистые
//! модели наложений: выдача палитры, продолжения which-key, метки `f` и командная строка `:`.
//!
//! Имена действий здесь строки: их проверяет приложение по реестру плагинов и командам хоста.

#![forbid(unsafe_code)]

mod chord;
mod cmdline;
mod defaults;
mod exit_chord;
mod fuzzy;
mod hints;
mod jump_list;
mod keymap;
mod palette;
mod sequence;
mod status_hints;
mod toml;
mod which_key;

pub use chord::{Chord, ChordError, KeyCode, Mods};
pub use cmdline::{
    Cmdline, CmdlineCommand, Theme, parse as parse_cmdline, resolve as resolve_cmdline,
    unknown_message,
};
pub use defaults::{DefaultsError, KeyDefault, LIST_KEYS, PanelKeys};
pub use exit_chord::{Cancel, ExitChord, KeyDown};
pub use hints::{HINT_ALPHABET, HintLabels, HintStep, MAX_HINT_TARGETS, hint_labels};
pub use jump_list::{JUMP_DEPTH, JumpList};
pub use keymap::{Binding, Context, Diagnostic, DiagnosticKind, Keymap, Lookup, Mode, Source};
pub use palette::{
    Args, Axis, Candidate, Category, Group, PALETTE_LIMIT, Palette, PaletteRow, PaletteSource,
    PaletteView, ViewGroup,
};
pub use sequence::{MAX_SEQUENCE, Match, Overlap, Sequence, SequenceError, SequenceTree};
pub use status_hints::{Hint, HintQuery, PanelHint, StatusText, status_hints};
pub use toml::{ActionCheck, ActionError};
pub use which_key::{
    WHICH_KEY_LONG, WHICH_KEY_SHORT, WhichItem, WhichKey, group_title, which_key, which_key_delay,
};
