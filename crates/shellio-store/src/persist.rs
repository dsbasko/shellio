//! Порт хранения ядра на `state.db`.

use shellio_core::{Change, CommitError, LoadError, Loaded, Persist, Revision, StorageInfo};

use crate::open::Database;
use crate::{load, write};

/// Хранилище ядра на `state.db`: построчная загрузка с диагностикой и запись дельт одной
/// транзакцией с ревизией. Базу открывает один процесс: в продукте после блокировки P1.2.
#[derive(Debug)]
pub struct SqlitePersist {
    db: Database,
}

impl SqlitePersist {
    /// Хранилище над открытой и мигрированной базой.
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

impl Persist for SqlitePersist {
    fn load(&mut self) -> Result<Loaded, LoadError> {
        let (state, revision) = load::load(self.db.conn(), self.db.path())?;
        Ok(Loaded {
            state,
            revision,
            storage: StorageInfo {
                schema_version: self.db.schema_version(),
                state_db: self.db.path().to_path_buf(),
            },
        })
    }

    fn commit(&mut self, base: Revision, changes: &[Change]) -> Result<(), CommitError> {
        write::commit(self.db.conn_mut(), base, changes)
    }
}
