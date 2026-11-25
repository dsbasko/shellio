//! Аккорд: физическая клавиша и модификаторы. Имена — как в прототипе: `Ctrl-w`, `Cmd-Alt-0`,
//! `Space`, `H` для `Shift-h`, знаки по раскладке US.

use std::fmt;

/// Виртуальный код клавиши macOS: положение клавиши, а не символ раскладки.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyCode(pub u16);

impl KeyCode {
    /// Клавиша-модификатор: её нажатие приходит как `flagsChanged`, аккордом она не бывает.
    pub fn is_modifier(self) -> bool {
        (0x36..=0x3F).contains(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Mods {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    pub const NONE: Self = Self {
        cmd: false,
        ctrl: false,
        alt: false,
        shift: false,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Chord {
    pub key: KeyCode,
    pub mods: Mods,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChordError(pub String);

impl fmt::Display for ChordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "неразборный аккорд «{}»", self.0)
    }
}

impl std::error::Error for ChordError {}

/// Клавиши со знаком раскладки ANSI US: код, знак без `Shift`, знак с `Shift`.
const PRINTABLE: [(u16, char, char); 47] = [
    (0x00, 'a', 'A'),
    (0x0B, 'b', 'B'),
    (0x08, 'c', 'C'),
    (0x02, 'd', 'D'),
    (0x0E, 'e', 'E'),
    (0x03, 'f', 'F'),
    (0x05, 'g', 'G'),
    (0x04, 'h', 'H'),
    (0x22, 'i', 'I'),
    (0x26, 'j', 'J'),
    (0x28, 'k', 'K'),
    (0x25, 'l', 'L'),
    (0x2E, 'm', 'M'),
    (0x2D, 'n', 'N'),
    (0x1F, 'o', 'O'),
    (0x23, 'p', 'P'),
    (0x0C, 'q', 'Q'),
    (0x0F, 'r', 'R'),
    (0x01, 's', 'S'),
    (0x11, 't', 'T'),
    (0x20, 'u', 'U'),
    (0x09, 'v', 'V'),
    (0x0D, 'w', 'W'),
    (0x07, 'x', 'X'),
    (0x10, 'y', 'Y'),
    (0x06, 'z', 'Z'),
    (0x12, '1', '!'),
    (0x13, '2', '@'),
    (0x14, '3', '#'),
    (0x15, '4', '$'),
    (0x17, '5', '%'),
    (0x16, '6', '^'),
    (0x1A, '7', '&'),
    (0x1C, '8', '*'),
    (0x19, '9', '('),
    (0x1D, '0', ')'),
    (0x29, ';', ':'),
    (0x2C, '/', '?'),
    (0x2B, ',', '<'),
    (0x2F, '.', '>'),
    (0x21, '[', '{'),
    (0x1E, ']', '}'),
    (0x2A, '\\', '|'),
    (0x1B, '-', '_'),
    (0x18, '=', '+'),
    (0x27, '\'', '"'),
    (0x32, '`', '~'),
];

/// Клавиши с именем: `Shift` у них пишется явно.
const NAMED: [(u16, &str); 26] = [
    (0x31, "Space"),
    (0x24, "Enter"),
    (0x35, "Esc"),
    (0x30, "Tab"),
    (0x33, "Backspace"),
    (0x75, "Delete"),
    (0x7E, "Up"),
    (0x7D, "Down"),
    (0x7B, "Left"),
    (0x7C, "Right"),
    (0x73, "Home"),
    (0x77, "End"),
    (0x74, "PageUp"),
    (0x79, "PageDown"),
    (0x7A, "F1"),
    (0x78, "F2"),
    (0x63, "F3"),
    (0x76, "F4"),
    (0x60, "F5"),
    (0x61, "F6"),
    (0x62, "F7"),
    (0x64, "F8"),
    (0x65, "F9"),
    (0x6D, "F10"),
    (0x67, "F11"),
    (0x6F, "F12"),
];

const PREFIXES: [&str; 4] = ["Cmd-", "Ctrl-", "Alt-", "Shift-"];

impl Chord {
    pub const fn new(key: KeyCode, mods: Mods) -> Self {
        Self { key, mods }
    }

    /// Аккорд по имени. Модификаторы — приставки `Cmd-`, `Ctrl-`, `Alt-`, `Shift-`; заглавная
    /// буква и знак верхнего регистра несут `Shift` сами.
    pub fn parse(name: &str) -> Result<Self, ChordError> {
        let fail = || ChordError(name.to_owned());
        let mut mods = Mods::NONE;
        let mut rest = name;
        'prefixes: loop {
            for (index, prefix) in PREFIXES.iter().enumerate() {
                let Some(tail) = rest.strip_prefix(prefix) else {
                    continue;
                };
                // «Ctrl--» — это `Ctrl` и клавиша «-»: приставка без клавиши за ней не приставка.
                if tail.is_empty() {
                    continue;
                }
                let flag = match index {
                    0 => &mut mods.cmd,
                    1 => &mut mods.ctrl,
                    2 => &mut mods.alt,
                    _ => &mut mods.shift,
                };
                if *flag {
                    return Err(fail());
                }
                *flag = true;
                rest = tail;
                continue 'prefixes;
            }
            break;
        }

        if let Some(&(code, _)) = NAMED.iter().find(|(_, named)| *named == rest) {
            return Ok(Self::new(KeyCode(code), mods));
        }
        let mut chars = rest.chars();
        let (Some(sign), None) = (chars.next(), chars.next()) else {
            return Err(fail());
        };
        for &(code, plain, shifted) in &PRINTABLE {
            if sign == plain {
                return Ok(Self::new(KeyCode(code), mods));
            }
            if sign == shifted {
                mods.shift = true;
                return Ok(Self::new(KeyCode(code), mods));
            }
        }
        Err(fail())
    }

    /// Печатаемая клавиша без `Cmd` и `Ctrl`: в поле ввода она принадлежит полю.
    pub fn types_text(self) -> bool {
        !self.mods.cmd
            && !self.mods.ctrl
            && (self.key.0 == 0x31 || PRINTABLE.iter().any(|&(code, ..)| code == self.key.0))
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Mods {
            cmd,
            ctrl,
            alt,
            shift,
        } = self.mods;
        for (on, prefix) in [(cmd, "Cmd-"), (ctrl, "Ctrl-"), (alt, "Alt-")] {
            if on {
                f.write_str(prefix)?;
            }
        }
        if let Some(&(_, plain, shifted)) = PRINTABLE.iter().find(|&&(code, ..)| code == self.key.0)
        {
            return write!(f, "{}", if shift { shifted } else { plain });
        }
        if shift {
            f.write_str("Shift-")?;
        }
        match NAMED.iter().find(|&&(code, _)| code == self.key.0) {
            Some(&(_, named)) => f.write_str(named),
            None => write!(f, "#{}", self.key.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(name: &str) -> Chord {
        Chord::parse(name).unwrap()
    }

    #[test]
    fn every_table_key_round_trips_with_every_modifier_set() {
        let codes = PRINTABLE
            .iter()
            .map(|&(code, ..)| code)
            .chain(NAMED.iter().map(|&(code, _)| code));
        for code in codes {
            for bits in 0..16u8 {
                let mods = Mods {
                    cmd: bits & 1 != 0,
                    ctrl: bits & 2 != 0,
                    alt: bits & 4 != 0,
                    shift: bits & 8 != 0,
                };
                let chord = Chord::new(KeyCode(code), mods);
                assert_eq!(Chord::parse(&chord.to_string()), Ok(chord), "{chord}");
            }
        }
    }

    #[test]
    fn table_codes_are_unique_and_never_modifiers() {
        let mut codes: Vec<u16> = PRINTABLE
            .iter()
            .map(|&(code, ..)| code)
            .chain(NAMED.iter().map(|&(code, _)| code))
            .collect();
        assert!(codes.iter().all(|&code| !KeyCode(code).is_modifier()));
        let total = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), total);
        for code in [0x37, 0x38, 0x39, 0x3A, 0x3B, 0x36, 0x3C, 0x3D, 0x3E, 0x3F] {
            assert!(KeyCode(code).is_modifier());
        }
    }

    #[test]
    fn prototype_names_map_to_macos_key_codes() {
        let ctrl = Mods {
            ctrl: true,
            ..Mods::NONE
        };
        let shift = Mods {
            shift: true,
            ..Mods::NONE
        };
        assert_eq!(chord("Ctrl-w"), Chord::new(KeyCode(0x0D), ctrl));
        assert_eq!(chord("Ctrl-\\"), Chord::new(KeyCode(0x2A), ctrl));
        assert_eq!(chord("h"), Chord::new(KeyCode(0x04), Mods::NONE));
        assert_eq!(chord("H"), Chord::new(KeyCode(0x04), shift));
        assert_eq!(chord("Shift-h"), chord("H"));
        assert_eq!(chord("Space"), Chord::new(KeyCode(0x31), Mods::NONE));
        assert_eq!(chord("Enter"), Chord::new(KeyCode(0x24), Mods::NONE));
        assert_eq!(chord("Esc"), Chord::new(KeyCode(0x35), Mods::NONE));
        assert_eq!(chord("Tab"), Chord::new(KeyCode(0x30), Mods::NONE));
        assert_eq!(chord("/"), Chord::new(KeyCode(0x2C), Mods::NONE));
        assert_eq!(chord(":"), Chord::new(KeyCode(0x29), shift));
        assert_eq!(chord("<"), Chord::new(KeyCode(0x2B), shift));
        assert_eq!(chord(">"), Chord::new(KeyCode(0x2F), shift));
        assert_eq!(chord("+"), Chord::new(KeyCode(0x18), shift));
        assert_eq!(chord("="), Chord::new(KeyCode(0x18), Mods::NONE));
        assert_eq!(chord("-"), Chord::new(KeyCode(0x1B), Mods::NONE));
        assert_eq!(chord("Ctrl--"), Chord::new(KeyCode(0x1B), ctrl));
        assert_eq!(
            chord("Cmd-Alt-0"),
            Chord::new(
                KeyCode(0x1D),
                Mods {
                    cmd: true,
                    alt: true,
                    ..Mods::NONE
                }
            )
        );
        assert_eq!(chord("Cmd-Alt-t").to_string(), "Cmd-Alt-t");
        assert_eq!(chord("Shift-Tab").to_string(), "Shift-Tab");
        assert_eq!(chord("Alt-Cmd-0").to_string(), "Cmd-Alt-0");
        assert_eq!(chord("Shift-;").to_string(), ":");
    }

    #[test]
    fn unknown_names_are_refused() {
        for name in [
            "",
            "Ctrl-",
            "Ctrl-Ctrl-w",
            "Hyper-w",
            "ab",
            "space",
            "й",
            "<leader>",
        ] {
            assert_eq!(Chord::parse(name), Err(ChordError(name.to_owned())));
        }
    }

    /// Синтетическое событие при русской раскладке: код клавиши прежний, символы другие.
    struct KeyEvent {
        code: u16,
        mods: Mods,
        characters: &'static str,
    }

    #[test]
    fn matching_uses_the_key_code_under_a_cyrillic_layout() {
        let events = [
            (
                KeyEvent {
                    code: 0x26,
                    mods: Mods::NONE,
                    characters: "о",
                },
                "j",
            ),
            (
                KeyEvent {
                    code: 0x25,
                    mods: Mods::NONE,
                    characters: "д",
                },
                "l",
            ),
            (
                KeyEvent {
                    code: 0x22,
                    mods: Mods::NONE,
                    characters: "ш",
                },
                "i",
            ),
            (
                KeyEvent {
                    code: 0x0D,
                    mods: Mods {
                        ctrl: true,
                        ..Mods::NONE
                    },
                    characters: "ц",
                },
                "Ctrl-w",
            ),
        ];
        for (event, name) in events {
            assert!(Chord::parse(event.characters).is_err());
            assert_eq!(Chord::new(KeyCode(event.code), event.mods), chord(name));
        }
    }

    #[test]
    fn text_keys_are_printable_without_cmd_and_ctrl() {
        assert!(chord("j").types_text());
        assert!(chord("J").types_text());
        assert!(chord("Space").types_text());
        assert!(chord("Alt-/").types_text());
        assert!(!chord("Ctrl-w").types_text());
        assert!(!chord("Cmd-k").types_text());
        assert!(!chord("Esc").types_text());
        assert!(!chord("Enter").types_text());
    }
}
