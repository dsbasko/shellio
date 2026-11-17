//! Модель сценария `apps-surface` (задача 19): клавиши синтетических событий по кодам клавиш
//! macOS, наборы клавиш частей и ожидаемые тексты. Модуль чистый и идёт в `make check`; водитель —
//! `crate::surface::apps`, события и источники ввода — `crate::surface::input`.
//!
//! Клавиша задаётся только кодом и модификаторами: символы события даёт текущий источник ввода
//! системы, а не стенд. Поэтому ввод латиницы идёт при раскладке ABC, а части `layout-ru` и
//! `dead-key` включают свой источник на время ввода.

use serde::{Deserialize, Serialize};

/// Модификаторы события.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mods {
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub control: bool,
    #[serde(default)]
    pub option: bool,
    #[serde(default)]
    pub command: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        shift: false,
        control: false,
        option: false,
        command: false,
    };
    pub const SHIFT: Mods = Mods {
        shift: true,
        ..Mods::NONE
    };
    pub const CONTROL: Mods = Mods {
        control: true,
        ..Mods::NONE
    };
    pub const OPTION: Mods = Mods {
        option: true,
        ..Mods::NONE
    };
}

/// Нажатие и отпускание клавиши по коду macOS (`kVK_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stroke {
    pub keycode: u16,
    #[serde(default)]
    pub mods: Mods,
}

impl Stroke {
    pub const fn key(keycode: u16) -> Stroke {
        Stroke {
            keycode,
            mods: Mods::NONE,
        }
    }

    pub const fn with(keycode: u16, mods: Mods) -> Stroke {
        Stroke { keycode, mods }
    }
}

pub const KEY_RETURN: u16 = 36;
pub const KEY_TAB: u16 = 48;
pub const KEY_SPACE: u16 = 49;
pub const KEY_ESCAPE: u16 = 53;
pub const KEY_UP: u16 = 126;
pub const KEY_E: u16 = 14;
pub const KEY_V: u16 = 9;
pub const KEY_W: u16 = 13;
pub const KEY_A: u16 = 0;
pub const KEY_X: u16 = 7;

/// Код клавиши символа ANSI-раскладки и нужен ли Shift.
fn ansi(ch: char) -> Option<(u16, bool)> {
    const LOWER: &str = "asdfhgzxcv\u{0}bqweryt123465=97-80]ou[ip\u{0}lj'k;\\,/nm.";
    const SHIFTED: &[(char, char)] = &[
        (':', ';'),
        ('!', '1'),
        ('@', '2'),
        ('#', '3'),
        ('$', '4'),
        ('%', '5'),
        ('^', '6'),
        ('&', '7'),
        ('*', '8'),
        ('(', '9'),
        (')', '0'),
        ('_', '-'),
        ('+', '='),
        ('<', ','),
        ('>', '.'),
        ('?', '/'),
        ('"', '\''),
        ('{', '['),
        ('}', ']'),
        ('|', '\\'),
    ];
    match ch {
        ' ' => return Some((KEY_SPACE, false)),
        '\r' | '\n' => return Some((KEY_RETURN, false)),
        '\t' => return Some((KEY_TAB, false)),
        '\u{1b}' => return Some((KEY_ESCAPE, false)),
        _ => {}
    }
    let (base, shift) = if ch.is_ascii_uppercase() {
        (ch.to_ascii_lowercase(), true)
    } else if let Some((_, base)) = SHIFTED.iter().find(|(shifted, _)| *shifted == ch) {
        (*base, true)
    } else {
        (ch, false)
    };
    if base == '\u{0}' {
        return None;
    }
    LOWER
        .chars()
        .position(|known| known == base)
        .and_then(|code| u16::try_from(code).ok())
        .map(|code| (code, shift))
}

/// Клавиши текста при раскладке ABC; символ вне ANSI-раскладки — ошибка стенда.
pub fn latin(text: &str) -> Result<Vec<Stroke>, String> {
    text.chars()
        .map(|ch| {
            ansi(ch)
                .map(|(keycode, shift)| Stroke {
                    keycode,
                    mods: if shift { Mods::SHIFT } else { Mods::NONE },
                })
                .ok_or_else(|| format!("символа {ch:?} нет в раскладке ABC стенда"))
        })
        .collect()
}

/// Ctrl-буква.
pub fn ctrl(letter: char) -> Result<Stroke, String> {
    ansi(letter)
        .map(|(keycode, _)| Stroke::with(keycode, Mods::CONTROL))
        .ok_or_else(|| format!("нет клавиши {letter:?}"))
}

/// Источники ввода частей (`kTISPropertyInputSourceID`).
pub const SOURCE_ABC: &str = "com.apple.keylayout.ABC";
pub const SOURCE_RUSSIAN: &str = "com.apple.keylayout.Russian";
pub const SOURCE_ABC_EXTENDED: &str = "com.apple.keylayout.USExtended";
/// Метод ввода японского и его режим: включается метод, выбирается режим.
pub const SOURCE_KOTOERI: &str = "com.apple.inputmethod.Kotoeri.RomajiTyping";
pub const SOURCE_JAPANESE: &str = "com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese";

/// `layout-ru`: коды клавиш g h b d t n.
pub const RU_KEYS: [u16; 6] = [5, 4, 11, 2, 17, 45];
pub const RU_TEXT: &str = "привет";
/// Те же коды при ABC.
pub const RU_LATIN: &str = "ghbdtn";

/// `dead-key`: Option-e, затем e.
pub const DEAD_KEYS: [Stroke; 2] = [Stroke::with(KEY_E, Mods::OPTION), Stroke::key(KEY_E)];
pub const DEAD_TEXT: &str = "é";

/// `ime`: ромадзи «nihonn», пробел — кандидат, Return — коммит; «ka» и Esc внутри composition.
pub const IME_ROMAJI: &str = "nihonn";
pub const IME_ESCAPED: &str = "ka";

/// Клавиши части `kitty` с модификаторами: имя и клавиша.
pub fn kitty_keys() -> Vec<(&'static str, Stroke)> {
    vec![
        ("ctrl-a", Stroke::with(KEY_A, Mods::CONTROL)),
        ("shift-enter", Stroke::with(KEY_RETURN, Mods::SHIFT)),
        ("ctrl-enter", Stroke::with(KEY_RETURN, Mods::CONTROL)),
        ("alt-x", Stroke::with(KEY_X, Mods::OPTION)),
        ("shift-tab", Stroke::with(KEY_TAB, Mods::SHIFT)),
        (
            "ctrl-shift-up",
            Stroke::with(
                KEY_UP,
                Mods {
                    shift: true,
                    control: true,
                    ..Mods::NONE
                },
            ),
        ),
        ("escape", Stroke::key(KEY_ESCAPE)),
    ]
}

/// Стек Kitty фикстуры части `kitty`: push 1, push 9 (disambiguate и все клавиши кодами).
pub const KITTY_PUSH: &[u8] = b"\x1b[>1u\x1b[>9u";

/// Ожидаемый файл `layout-ru` в контроле: те же коды при ABC, пробел, при «Русской».
pub fn ru_control_file() -> String {
    format!("{RU_LATIN} {RU_TEXT}\n")
}

/// Цифра `n % 10` ввода цикла `switch`.
pub fn digit(n: usize) -> char {
    char::from(b'0' + (n % 10) as u8)
}

/// Циклов части `switch`.
pub const SWITCH_CYCLES: usize = 10;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_text_maps_to_ansi_keycodes() {
        let strokes = latin(":set number\r").expect("клавиши");
        assert_eq!(strokes[0], Stroke::with(41, Mods::SHIFT));
        assert_eq!(strokes[1], Stroke::key(1));
        assert_eq!(strokes[4], Stroke::key(KEY_SPACE));
        assert_eq!(strokes.last(), Some(&Stroke::key(KEY_RETURN)));
        assert_eq!(latin("G").expect("G"), [Stroke::with(5, Mods::SHIFT)]);
        assert_eq!(
            latin("60").expect("цифры"),
            [Stroke::key(22), Stroke::key(29)]
        );
        assert_eq!(latin("-").expect("минус"), [Stroke::key(27)]);
        assert!(latin("п").is_err());
        assert_eq!(
            ctrl('w').expect("Ctrl-W"),
            Stroke::with(KEY_W, Mods::CONTROL)
        );
        assert_eq!(
            ctrl('v').expect("Ctrl-V"),
            Stroke::with(KEY_V, Mods::CONTROL)
        );
    }

    #[test]
    fn russian_keys_are_the_latin_word() {
        let strokes = latin(RU_LATIN).expect("клавиши");
        let codes: Vec<u16> = strokes.iter().map(|stroke| stroke.keycode).collect();
        assert_eq!(codes, RU_KEYS);
        assert_eq!(ru_control_file(), "ghbdtn привет\n");
        assert_eq!(latin("nihonn").expect("ромадзи")[0], Stroke::key(45));
    }

    #[test]
    fn kitty_keys_are_distinct_and_digits_cycle() {
        let keys = kitty_keys();
        let mut names: Vec<&str> = keys.iter().map(|(name, _)| *name).collect();
        names.dedup();
        assert_eq!(names.len(), keys.len());
        assert!(
            keys.iter()
                .all(|(_, stroke)| stroke.mods != Mods::NONE || stroke.keycode == KEY_ESCAPE)
        );
        assert_eq!(digit(0), '0');
        assert_eq!(digit(13), '3');
        let json = serde_json::to_string(&Stroke::key(KEY_E)).expect("json");
        let back: Stroke = serde_json::from_str(&json).expect("разбор");
        assert_eq!(back, Stroke::key(KEY_E));
        let short: Stroke = serde_json::from_str(r#"{"keycode":14}"#).expect("разбор");
        assert_eq!(short.mods, Mods::NONE);
    }
}
