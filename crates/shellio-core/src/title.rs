//! Имена воркспейсов, проектов и рабочих областей.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Проверенное имя: без пробелов по краям, не пустое, не длиннее [`Title::MAX_CHARS`] символов
/// и без управляющих символов.
///
/// Равенство точное. Уникальность имён сравнивает [`Title::folded`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Title(String);

/// Причина отказа в имени.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TitleError {
    /// После обрезки пробелов ничего не осталось.
    Empty,
    /// Длиннее [`Title::MAX_CHARS`] символов.
    TooLong { chars: usize },
    /// Есть управляющий символ.
    ControlChar { ch: char },
}

impl fmt::Display for TitleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("имя пустое"),
            Self::TooLong { chars } => write!(
                f,
                "имя длиной {chars} символов длиннее {} символов",
                Title::MAX_CHARS
            ),
            Self::ControlChar { ch } => {
                write!(
                    f,
                    "имя содержит управляющий символ U+{:04X}",
                    u32::from(*ch)
                )
            }
        }
    }
}

impl std::error::Error for TitleError {}

impl TitleError {
    /// Стабильная причина отказа: её несут `invalid_argument` и диагностика загрузки.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Empty => "title_empty",
            Self::TooLong { .. } => "title_too_long",
            Self::ControlChar { .. } => "title_control_char",
        }
    }
}

impl Title {
    /// Наибольшая длина имени в символах (`chars`).
    pub const MAX_CHARS: usize = 200;

    /// Имя из ввода пользователя: пробелы по краям обрезаются, затем значение проверяется.
    pub fn new(raw: &str) -> Result<Self, TitleError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(TitleError::Empty);
        }
        if let Some(ch) = trimmed.chars().find(|ch| ch.is_control()) {
            return Err(TitleError::ControlChar { ch });
        }
        let chars = trimmed.chars().count();
        if chars > Self::MAX_CHARS {
            return Err(TitleError::TooLong { chars });
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// Имя как строка.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Ключ уникальности: имя в нижнем регистре.
    pub fn folded(&self) -> String {
        self.0.to_lowercase()
    }

    /// Совпадают ли имена без учёта регистра.
    pub fn same_name(&self, other: &Title) -> bool {
        self.folded() == other.folded()
    }
}

impl fmt::Display for Title {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Title {
    type Error = TitleError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(&raw)
    }
}

impl From<Title> for String {
    fn from(title: Title) -> Self {
        title.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_are_trimmed() {
        let title = Title::new(" \t Работа \n").unwrap();
        assert_eq!(title.as_str(), "Работа");
        assert_eq!(Title::new("\u{3000}x\u{a0}").unwrap().as_str(), "x");
        assert_eq!(Title::new("a  b").unwrap().as_str(), "a  b");
    }

    #[test]
    fn empty_is_rejected() {
        assert_eq!(Title::new(""), Err(TitleError::Empty));
        assert_eq!(Title::new("   \t\n "), Err(TitleError::Empty));
    }

    #[test]
    fn length_is_counted_in_chars() {
        let max = "ж".repeat(Title::MAX_CHARS);
        assert_eq!(max.len(), 2 * Title::MAX_CHARS);
        assert_eq!(Title::new(&max).unwrap().as_str(), max);
        let padded = format!("  {max}  ");
        assert_eq!(Title::new(&padded).unwrap().as_str(), max);

        let over = "ж".repeat(Title::MAX_CHARS + 1);
        assert_eq!(
            Title::new(&over),
            Err(TitleError::TooLong {
                chars: Title::MAX_CHARS + 1
            })
        );
    }

    #[test]
    fn control_chars_are_rejected() {
        for (raw, ch) in [
            ("a\tb", '\t'),
            ("a\nb", '\n'),
            ("a\u{0}b", '\u{0}'),
            ("a\u{1b}[31m", '\u{1b}'),
            ("a\u{7f}", '\u{7f}'),
            ("a\u{85}b", '\u{85}'),
        ] {
            assert_eq!(
                Title::new(raw),
                Err(TitleError::ControlChar { ch }),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn comparison_ignores_case_for_cyrillic_and_latin() {
        let cyrillic = Title::new("Работа").unwrap();
        assert!(cyrillic.same_name(&Title::new("РАБОТА").unwrap()));
        assert!(cyrillic.same_name(&Title::new("  работа ").unwrap()));
        assert!(
            Title::new("Ёлка")
                .unwrap()
                .same_name(&Title::new("ёЛКА").unwrap())
        );
        assert!(
            Title::new("Shellio")
                .unwrap()
                .same_name(&Title::new("sHELLIO").unwrap())
        );
        assert!(!cyrillic.same_name(&Title::new("Работы").unwrap()));
        assert!(
            !Title::new("e")
                .unwrap()
                .same_name(&Title::new("е").unwrap())
        );
    }

    #[test]
    fn equality_is_exact() {
        assert_ne!(Title::new("Работа").unwrap(), Title::new("работа").unwrap());
        assert_eq!(
            Title::new(" Работа").unwrap(),
            Title::new("Работа ").unwrap()
        );
    }

    #[test]
    fn serde_is_a_validated_string() {
        let title = Title::new("Основной").unwrap();
        assert_eq!(serde_json::to_string(&title).unwrap(), "\"Основной\"");
        assert_eq!(
            serde_json::from_str::<Title>("\" Основной \"").unwrap(),
            title
        );
        assert!(serde_json::from_str::<Title>("\"  \"").is_err());
        assert!(serde_json::from_str::<Title>("\"a\\u0007\"").is_err());
    }

    #[test]
    fn errors_explain_themselves() {
        assert_eq!(TitleError::Empty.to_string(), "имя пустое");
        assert!(
            TitleError::ControlChar { ch: '\u{7}' }
                .to_string()
                .contains("U+0007")
        );
        assert!(
            TitleError::TooLong { chars: 201 }
                .to_string()
                .contains("201")
        );
    }
}
