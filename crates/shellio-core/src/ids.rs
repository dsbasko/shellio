//! Идентификаторы сущностей и их генераторы.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

/// Длина канонической строки UUID с дефисами.
const CANONICAL_LEN: usize = 36;

/// Строка не является канонической записью UUID с дефисами.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdParseError {
    input: String,
}

impl IdParseError {
    /// Отвергнутая строка.
    pub fn input(&self) -> &str {
        &self.input
    }
}

impl fmt::Display for IdParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "«{}» не является идентификатором: нужна каноническая строка UUID с дефисами",
            self.input
        )
    }
}

impl std::error::Error for IdParseError {}

fn parse_canonical(input: &str) -> Result<Uuid, IdParseError> {
    let error = || IdParseError {
        input: input.to_owned(),
    };
    // `Uuid::try_parse` принимает и формы без дефисов, в фигурных скобках и `urn:uuid:`:
    // у канонической формы и только у неё длина 36.
    if input.len() != CANONICAL_LEN {
        return Err(error());
    }
    Uuid::try_parse(input).map_err(|_| error())
}

/// Источник новых UUID. Продукт выдаёт UUIDv7, тесты — последовательные значения.
pub trait IdGen: Send {
    /// Следующий ещё не выданный UUID.
    fn next_uuid(&mut self) -> Uuid;
}

/// Продуктовый генератор UUIDv7.
#[derive(Debug, Default)]
pub struct SystemIds;

impl IdGen for SystemIds {
    fn next_uuid(&mut self) -> Uuid {
        Uuid::now_v7()
    }
}

/// Последовательный генератор для тестов: 1, 2, 3, … в младших битах UUID.
#[cfg(feature = "test-support")]
#[derive(Debug, Default)]
pub struct SequentialIds {
    issued: u128,
}

#[cfg(feature = "test-support")]
impl SequentialIds {
    /// Генератор, первым выдающий UUID со значением 1.
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg(feature = "test-support")]
impl IdGen for SequentialIds {
    fn next_uuid(&mut self) -> Uuid {
        self.issued += 1;
        Uuid::from_u128(self.issued)
    }
}

macro_rules! entity_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Uuid);

        impl $name {
            /// Новый идентификатор из генератора.
            pub fn generate(ids: &mut dyn IdGen) -> Self {
                Self(ids.next_uuid())
            }

            /// Идентификатор из 16 байт хранилища.
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(Uuid::from_bytes(bytes))
            }

            /// 16 байт для хранилища.
            pub const fn as_bytes(&self) -> &[u8; 16] {
                self.0.as_bytes()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.hyphenated(), f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(input: &str) -> Result<Self, Self::Err> {
                parse_canonical(input).map(Self)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

entity_id!(
    /// Идентификатор воркспейса.
    WorkspaceId
);
entity_id!(
    /// Идентификатор проекта.
    ProjectId
);
entity_id!(
    /// Идентификатор корня проекта.
    ProjectRootId
);
entity_id!(
    /// Идентификатор рабочей области.
    WorkbenchId
);
entity_id!(
    /// Идентификатор панели. Записи панелей появляются в P1.4.
    PanelId
);
entity_id!(
    /// Идентификатор сессии. Записи сессий появляются в P2.1.
    SessionId
);
entity_id!(
    /// Идентификатор агента. Записи агентов появляются в P2.5.
    AgentId
);

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const LOWER: &str = "0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5a6";
    const UPPER: &str = "0192F3A4-5B6C-7D8E-9FA0-B1C2D3E4F5A6";

    #[test]
    fn canonical_string_parses_in_any_case_and_prints_lowercase() {
        let lower: WorkspaceId = LOWER.parse().unwrap();
        let upper: WorkspaceId = UPPER.parse().unwrap();
        let mixed: WorkspaceId = "0192f3A4-5B6c-7d8E-9fa0-B1C2d3e4F5a6".parse().unwrap();
        assert_eq!(lower, upper);
        assert_eq!(lower, mixed);
        assert_eq!(upper.to_string(), LOWER);
        assert_eq!(format!("{upper:?}"), format!("WorkspaceId({LOWER})"));
    }

    #[test]
    fn version_is_not_checked() {
        let v4: ProjectId = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
        assert_eq!(v4.to_string(), "550e8400-e29b-41d4-a716-446655440000");
        let nil: ProjectId = "00000000-0000-0000-0000-000000000000".parse().unwrap();
        assert_eq!(nil.as_bytes(), &[0; 16]);
    }

    #[test]
    fn non_canonical_strings_are_rejected() {
        let rejected = [
            "",
            "0192f3a45b6c7d8e9fa0b1c2d3e4f5a6",
            "{0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5a6}",
            "urn:uuid:0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5a6",
            "0192f3a45-b6c-7d8e-9fa0-b1c2d3e4f5a6",
            "0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5a",
            "0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5a67",
            "0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5ag",
            " 0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5a",
            "0192f3a4_5b6c_7d8e_9fa0_b1c2d3e4f5a6",
            "0192f3a4-5b6c-7d8e-9fa0-b1c2d3e4f5аб",
        ];
        for input in rejected {
            let error = input.parse::<WorkbenchId>().unwrap_err();
            assert_eq!(error.input(), input);
            assert!(error.to_string().contains(input), "{error}");
            let json = serde_json::to_string(input).unwrap();
            assert!(
                serde_json::from_str::<WorkbenchId>(&json).is_err(),
                "serde принял {input:?}"
            );
        }
    }

    #[test]
    fn serde_uses_canonical_lowercase_string() {
        let id: AgentId = UPPER.parse().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{LOWER}\""));
        let back: AgentId = serde_json::from_str(&format!("\"{UPPER}\"")).unwrap();
        assert_eq!(back, id);
        assert!(serde_json::from_str::<AgentId>("42").is_err());
        assert!(serde_json::from_slice::<AgentId>(json.as_bytes()).is_ok());
    }

    #[test]
    fn ids_work_as_json_map_keys() {
        let id: SessionId = LOWER.parse().unwrap();
        let map = std::collections::BTreeMap::from([(id, 1)]);
        let json = serde_json::to_string(&map).unwrap();
        assert_eq!(json, format!("{{\"{LOWER}\":1}}"));
        let back: std::collections::BTreeMap<SessionId, i32> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, map);
    }

    #[test]
    fn bytes_round_trip() {
        let id: PanelId = LOWER.parse().unwrap();
        assert_eq!(PanelId::from_bytes(*id.as_bytes()), id);
    }

    #[test]
    fn sequential_generator_is_unique_and_ordered() {
        let mut ids = SequentialIds::new();
        let issued: Vec<ProjectRootId> = (0..1000)
            .map(|_| ProjectRootId::generate(&mut ids))
            .collect();
        let unique: BTreeSet<_> = issued.iter().copied().collect();
        assert_eq!(unique.len(), issued.len());
        assert_eq!(
            issued[0].to_string(),
            "00000000-0000-0000-0000-000000000001"
        );
        assert!(issued.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn sequential_generators_with_same_start_agree() {
        let mut a = SequentialIds::new();
        let mut b = SequentialIds::new();
        for _ in 0..10 {
            assert_eq!(WorkspaceId::generate(&mut a), WorkspaceId::generate(&mut b));
        }
    }

    #[test]
    fn system_generator_issues_v7() {
        let mut ids = SystemIds;
        let first = WorkspaceId::generate(&mut ids);
        let second = WorkspaceId::generate(&mut ids);
        assert_ne!(first, second);
        assert_eq!(Uuid::from_bytes(*first.as_bytes()).get_version_num(), 7);
    }
}
