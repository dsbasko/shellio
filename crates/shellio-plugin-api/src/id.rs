//! Plugin id панели (ADR-0007, ADR-0008).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Строковый plugin id: от 1 до [`PluginId::MAX_CHARS`] символов из `[a-z0-9-]` (ADR-0007).
/// По нему хост находит манифест, модель и фабрику вида плагина.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PluginId(String);

/// Причина отказа в plugin id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginIdError {
    Empty,
    /// Символ вне `[a-z0-9-]`.
    InvalidChar {
        ch: char,
    },
    /// Длиннее [`PluginId::MAX_CHARS`] символов.
    TooLong {
        chars: usize,
    },
}

impl fmt::Display for PluginIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("plugin id пустой"),
            Self::InvalidChar { ch } => write!(
                f,
                "plugin id содержит символ U+{:04X} вне [a-z0-9-]",
                u32::from(*ch)
            ),
            Self::TooLong { chars } => write!(
                f,
                "plugin id длиной {chars} символов длиннее {} символов",
                PluginId::MAX_CHARS
            ),
        }
    }
}

impl std::error::Error for PluginIdError {}

impl PluginId {
    /// Наибольшая длина plugin id в символах.
    pub const MAX_CHARS: usize = 64;

    /// Plugin id из строки как есть, без обрезки пробелов.
    pub fn new(raw: &str) -> Result<Self, PluginIdError> {
        if raw.is_empty() {
            return Err(PluginIdError::Empty);
        }
        if let Some(ch) = raw
            .chars()
            .find(|ch| !matches!(ch, 'a'..='z' | '0'..='9' | '-'))
        {
            return Err(PluginIdError::InvalidChar { ch });
        }
        if raw.len() > Self::MAX_CHARS {
            return Err(PluginIdError::TooLong { chars: raw.len() });
        }
        Ok(Self(raw.to_owned()))
    }

    /// Plugin id как строка.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for PluginId {
    type Error = PluginIdError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(&raw)
    }
}

impl From<PluginId> for String {
    fn from(id: PluginId) -> Self {
        id.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_id_follows_wire_rule() {
        let longest = "a".repeat(PluginId::MAX_CHARS);
        for ok in [
            "navigator",
            "terminal",
            "a",
            "git-diff",
            "x9",
            longest.as_str(),
        ] {
            assert_eq!(
                PluginId::new(ok).map(|id| id.to_string()),
                Ok(ok.to_owned())
            );
        }
        assert_eq!(PluginId::new(""), Err(PluginIdError::Empty));
        assert_eq!(
            PluginId::new(&"a".repeat(65)),
            Err(PluginIdError::TooLong { chars: 65 })
        );
        for (raw, ch) in [
            ("Terminal", 'T'),
            ("git_diff", '_'),
            (" terminal", ' '),
            ("терминал", 'т'),
            ("a.b", '.'),
        ] {
            assert_eq!(
                PluginId::new(raw),
                Err(PluginIdError::InvalidChar { ch }),
                "{raw}"
            );
        }
    }

    #[test]
    fn plugin_id_is_checked_when_parsed() {
        let id: PluginId = serde_json::from_str("\"terminal\"").unwrap();
        assert_eq!(id.as_str(), "terminal");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"terminal\"");
        assert!(serde_json::from_str::<PluginId>("\"Terminal\"").is_err());
        assert!(serde_json::from_str::<PluginId>("\"\"").is_err());
    }
}
