//! Порт хранения ядра на `state.db`.

use shellio_core::{Change, CommitError, LoadError, Loaded, Persist, Revision, StorageInfo};

use crate::open::Database;
use crate::{load, write};

/// Хранилище ядра на `state.db`: построчная загрузка с диагностикой и запись дельт одной
/// транзакцией с ревизией. Базу открывает один процесс: в продукте после блокировки P1.2.
#[derive(Debug)]
pub struct SqlitePersist {
    db: Database,
    #[cfg(feature = "test-support")]
    fault: CommitFault,
}

impl SqlitePersist {
    /// Хранилище над открытой и мигрированной базой.
    pub fn new(db: Database) -> Self {
        Self {
            db,
            #[cfg(feature = "test-support")]
            fault: CommitFault::default(),
        }
    }

    /// Пульт крючка сбоя записи. Клон пульта остаётся у теста, когда хранилище уходит в ядро.
    #[cfg(feature = "test-support")]
    pub fn commit_fault(&self) -> CommitFault {
        self.fault.clone()
    }
}

impl Persist for SqlitePersist {
    fn load(&mut self) -> Result<Loaded, LoadError> {
        let load::Loaded {
            state,
            revision,
            fingerprint,
        } = load::load(self.db.conn(), self.db.path())?;
        Ok(Loaded {
            state,
            revision,
            storage: StorageInfo {
                schema_version: self.db.schema_version(),
                state_db: self.db.path().to_path_buf(),
            },
            fingerprint,
        })
    }

    fn commit(
        &mut self,
        base: Revision,
        changes: &[Change],
        fingerprint: Option<&str>,
    ) -> Result<(), CommitError> {
        #[cfg(feature = "test-support")]
        let stop_after = self.fault.take();
        #[cfg(not(feature = "test-support"))]
        let stop_after = None;
        write::commit(self.db.conn_mut(), base, changes, fingerprint, stop_after)
    }
}

/// Тестовый крючок сбоя записи [`SqlitePersist`]: прерывает следующую запись посреди
/// транзакции. Клоны делят одно состояние.
#[cfg(feature = "test-support")]
#[derive(Clone, Debug, Default)]
pub struct CommitFault {
    armed: std::sync::Arc<std::sync::Mutex<Option<usize>>>,
}

#[cfg(feature = "test-support")]
impl CommitFault {
    /// Следующая запись выполнит `statements` SQL-операторов дельт, отпечатка реестра и ревизии и
    /// откажет `storage_unavailable` с сообщением «тестовый сбой: …», не дойдя до `COMMIT`.
    /// Крючок снимает любая следующая запись: та, в которой операторов не больше
    /// `statements`, проходит.
    pub fn fail_next_commit_after(&self, statements: usize) {
        *self.lock() = Some(statements);
    }

    /// Снять крючок для начинающейся записи.
    fn take(&self) -> Option<usize> {
        self.lock().take()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<usize>> {
        self.armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
