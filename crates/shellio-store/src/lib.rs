//! Хранилище shellio поверх SQLite.

#![forbid(unsafe_code)]

mod migrate;
mod open;
mod profile;

pub use migrate::{BackupError, Migration, Migrator};
pub use open::{Database, StoreError};
pub use profile::{Profile, ProfileError, STATE_DIR_VAR};

/// Тестовая поддержка хранилища, доступная только с функцией `test-support`.
#[cfg(feature = "test-support")]
pub mod test_support {}

#[cfg(test)]
mod tests {
    #[test]
    fn test_support_enabled_without_flags() {
        const {
            assert!(
                cfg!(feature = "test-support"),
                "функция test-support не подключена dev-зависимостью crate на себя"
            )
        };
    }
}
