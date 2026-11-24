//! Порт хранения ядра и поток хранения, на котором он работает.

use std::error::Error;
use std::fmt;
use std::sync::mpsc as std_mpsc;
use std::thread::{self, JoinHandle};

use tokio::sync::oneshot;

use crate::change::Change;
use crate::error::CoreError;
use crate::handle::StartError;
use crate::model::{Revision, State};
use crate::query::StorageInfo;

/// Ошибка загрузки хранилища. `Core::start` возвращает её как есть.
pub type LoadError = Box<dyn Error + Send + Sync + 'static>;

/// Состояние, загруженное хранилищем.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Loaded {
    pub state: State,
    /// Ревизия последней записи.
    pub revision: Revision,
    pub storage: StorageInfo,
    /// Отпечаток реестра плагинов, с которым базу записал прежний запуск. `None` — отпечатка
    /// ещё нет.
    pub fingerprint: Option<String>,
}

/// Отказ записи. Ни один отказ не меняет ни базу, ни состояние ядра.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitError {
    /// Запись не удалась, повтор возможен: `storage_unavailable`.
    Unavailable(String),
    /// Хранилище разошлось с ядром, например в базу писал кто-то ещё: `storage_conflict`.
    Conflict(String),
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) | Self::Conflict(message) => f.write_str(message),
        }
    }
}

impl Error for CommitError {}

impl From<CommitError> for CoreError {
    fn from(error: CommitError) -> Self {
        match error {
            CommitError::Unavailable(message) => Self::storage_unavailable(message),
            CommitError::Conflict(message) => Self::storage_conflict(message),
        }
    }
}

/// Порт хранения. Блокирующий: ядро вызывает его только на своём потоке хранения.
pub trait Persist: Send {
    /// Загрузить состояние. Вызывается один раз до первой записи.
    fn load(&mut self) -> Result<Loaded, LoadError>;

    /// Записать дельты одной транзакцией: ревизия `base` становится `base + 1`.
    ///
    /// `fingerprint` — новый отпечаток реестра плагинов: его несёт только запись запуска и
    /// хранилище пишет его той же транзакцией. `None` — отпечаток прежний.
    fn commit(
        &mut self,
        base: Revision,
        changes: &[Change],
        fingerprint: Option<&str>,
    ) -> Result<(), CommitError>;
}

type CommitResult = Result<(), CommitError>;

/// Куда поток хранения отдаёт квитанцию записи.
enum Receipt {
    /// Актору, который ждёт её асинхронно.
    Async(oneshot::Sender<CommitResult>),
    /// Запуску ядра, который ждёт её, блокируя вызывающий поток.
    Blocking(std_mpsc::Sender<CommitResult>),
}

struct Job {
    base: Revision,
    changes: Vec<Change>,
    fingerprint: Option<String>,
    receipt: Receipt,
}

/// Отдельный поток с экземпляром [`Persist`]: записи идут строго по порядку и не занимают
/// потоки `tokio`.
pub(crate) struct StorageThread {
    jobs: Option<std_mpsc::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl StorageThread {
    /// Запустить поток хранения и дождаться загрузки состояния на нём.
    pub(crate) fn spawn(persist: Box<dyn Persist>) -> Result<(Self, Loaded), StartError> {
        let (jobs, inbox) = std_mpsc::channel();
        let (loaded_tx, loaded_rx) = std_mpsc::channel();
        let thread = thread::Builder::new()
            .name("shellio-storage".to_owned())
            .spawn(move || serve(persist, &loaded_tx, &inbox))
            .map_err(StartError::Spawn)?;
        let storage = Self {
            jobs: Some(jobs),
            thread: Some(thread),
        };
        match loaded_rx.recv() {
            Ok(Ok(loaded)) => Ok((storage, loaded)),
            Ok(Err(error)) => {
                storage.stop_blocking();
                Err(StartError::Load(error))
            }
            Err(_) => {
                storage.stop_blocking();
                Err(StartError::Load(
                    "поток хранения завершился до конца загрузки".into(),
                ))
            }
        }
    }

    /// Записать дельты и дождаться квитанции.
    pub(crate) async fn commit(&self, base: Revision, changes: Vec<Change>) -> CommitResult {
        let (receipt, done) = oneshot::channel();
        self.send(Job {
            base,
            changes,
            fingerprint: None,
            receipt: Receipt::Async(receipt),
        })?;
        done.await.map_err(|_| stopped())?
    }

    /// Записать дельты и новый отпечаток реестра, блокируя вызывающий поток до квитанции.
    /// Только для запуска ядра.
    pub(crate) fn commit_blocking(
        &self,
        base: Revision,
        changes: Vec<Change>,
        fingerprint: Option<String>,
    ) -> CommitResult {
        let (receipt, done) = std_mpsc::channel();
        self.send(Job {
            base,
            changes,
            fingerprint,
            receipt: Receipt::Blocking(receipt),
        })?;
        done.recv().map_err(|_| stopped())?
    }

    fn send(&self, job: Job) -> CommitResult {
        self.jobs
            .as_ref()
            .ok_or_else(stopped)?
            .send(job)
            .map_err(|_| stopped())
    }

    /// Закрыть поток хранения и дождаться его завершения вне потоков `tokio`.
    pub(crate) async fn stop(mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            // Паника потока хранения уже отдана отказами записи, здесь её нечем дополнить.
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }

    /// Закрыть поток хранения и дождаться его завершения, блокируя вызывающий поток.
    pub(crate) fn stop_blocking(mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn stopped() -> CommitError {
    CommitError::Unavailable("поток хранения остановлен".to_owned())
}

fn serve(
    mut persist: Box<dyn Persist>,
    loaded: &std_mpsc::Sender<Result<Loaded, LoadError>>,
    inbox: &std_mpsc::Receiver<Job>,
) {
    let result = persist.load();
    let failed = result.is_err();
    if loaded.send(result).is_err() || failed {
        return;
    }
    for job in inbox {
        let result = persist.commit(job.base, &job.changes, job.fingerprint.as_deref());
        // Получатель квитанции мог уйти: запись уже выполнена, сообщать некому.
        match job.receipt {
            Receipt::Async(receipt) => {
                let _ = receipt.send(result);
            }
            Receipt::Blocking(receipt) => {
                let _ = receipt.send(result);
            }
        }
    }
}

#[cfg(feature = "test-support")]
pub use support::{CommitBarrier, Faults, FaultyPersist, MemoryPersist};

#[cfg(feature = "test-support")]
mod support {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::sync::mpsc as std_mpsc;
    use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

    use tokio::sync::oneshot;

    use shellio_plugin_api::Registry;

    use super::{CommitError, LoadError, Loaded, Persist};
    use crate::change::Change;
    use crate::model::{Revision, State};
    use crate::plugin_host::fingerprint;
    use crate::query::StorageInfo;

    /// Хранилище в памяти. Клоны делят одни данные: тест держит клон и видит записанное
    /// ядром.
    #[derive(Clone, Debug, Default)]
    pub struct MemoryPersist {
        stored: Arc<Mutex<Stored>>,
    }

    #[derive(Debug, Default)]
    struct Stored {
        state: State,
        revision: Revision,
        fingerprint: Option<String>,
    }

    impl MemoryPersist {
        /// Условный путь базы в `query.status`.
        pub const STATE_DB: &'static str = ":memory:";

        /// Пустое хранилище с ревизией 0.
        pub fn new() -> Self {
            Self::default()
        }

        /// Хранилище с уже записанным состоянием `state` на ревизии `revision`, без отпечатка
        /// реестра: первый запуск на нём пишет запись запуска.
        pub fn with_state(state: State, revision: Revision) -> Self {
            Self {
                stored: Arc::new(Mutex::new(Stored {
                    state,
                    revision,
                    fingerprint: None,
                })),
            }
        }

        /// То же хранилище с отпечатком реестра `registry`, как после запуска с ним.
        #[must_use]
        pub fn written_by(self, registry: &Registry) -> Self {
            self.lock().fingerprint = Some(fingerprint(registry));
            self
        }

        /// Записанный отпечаток реестра.
        pub fn fingerprint(&self) -> Option<String> {
            self.lock().fingerprint.clone()
        }

        /// Записанное состояние.
        pub fn state(&self) -> State {
            self.lock().state.clone()
        }

        /// Ревизия последней записи.
        pub fn revision(&self) -> Revision {
            self.lock().revision
        }

        fn lock(&self) -> MutexGuard<'_, Stored> {
            self.stored.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    impl Persist for MemoryPersist {
        fn load(&mut self) -> Result<Loaded, LoadError> {
            let stored = self.lock();
            Ok(Loaded {
                state: stored.state.clone(),
                revision: stored.revision,
                storage: StorageInfo {
                    schema_version: 0,
                    state_db: PathBuf::from(Self::STATE_DB),
                },
                fingerprint: stored.fingerprint.clone(),
            })
        }

        fn commit(
            &mut self,
            base: Revision,
            changes: &[Change],
            fingerprint: Option<&str>,
        ) -> Result<(), CommitError> {
            let mut stored = self.lock();
            if stored.revision != base {
                return Err(CommitError::Conflict(format!(
                    "ревизия хранилища {} не совпадает с ревизией ядра {}",
                    stored.revision.0, base.0
                )));
            }
            stored.state.apply_all(changes);
            // Статус панели — производное значение: база его не хранит.
            stored.state.panel_status.clear();
            if let Some(fingerprint) = fingerprint {
                stored.fingerprint = Some(fingerprint.to_owned());
            }
            stored.revision = base.next();
            Ok(())
        }
    }

    /// Хранилище в памяти с отказами по плану теста: отказ загрузки, отказ записи по номеру
    /// вызова и барьер, на котором поток хранения ждёт теста.
    #[derive(Debug)]
    pub struct FaultyPersist {
        inner: MemoryPersist,
        faults: Faults,
    }

    impl FaultyPersist {
        /// Обёртка над `inner` и пульт её отказов.
        pub fn new(inner: MemoryPersist) -> (Self, Faults) {
            let faults = Faults::default();
            let persist = Self {
                inner,
                faults: faults.clone(),
            };
            (persist, faults)
        }
    }

    impl Persist for FaultyPersist {
        fn load(&mut self) -> Result<Loaded, LoadError> {
            if self.faults.lock().fail_load {
                return Err("отказ загрузки по плану теста".into());
            }
            self.inner.load()
        }

        fn commit(
            &mut self,
            base: Revision,
            changes: &[Change],
            fingerprint: Option<&str>,
        ) -> Result<(), CommitError> {
            let (call, fail, gate) = {
                let mut plan = self.faults.lock();
                plan.commits += 1;
                let call = plan.commits;
                (
                    call,
                    plan.fail_commits.remove(&call),
                    plan.barriers.remove(&call),
                )
            };
            if let Some(gate) = gate {
                let _ = gate.entered.send(());
                // Отпускает и `release`, и брошенный барьер.
                let _ = gate.release.recv();
            }
            if fail {
                return Err(CommitError::Unavailable(format!(
                    "отказ записи {call} по плану теста"
                )));
            }
            self.inner.commit(base, changes, fingerprint)
        }
    }

    impl Drop for FaultyPersist {
        fn drop(&mut self) {
            self.faults.lock().dropped = true;
        }
    }

    /// Пульт отказов [`FaultyPersist`]. Вызовы `commit` нумеруются с 1, запись запуска ядра
    /// тоже считается.
    #[derive(Clone, Debug, Default)]
    pub struct Faults {
        plan: Arc<Mutex<FaultPlan>>,
    }

    #[derive(Debug, Default)]
    struct FaultPlan {
        commits: u64,
        fail_load: bool,
        fail_commits: BTreeSet<u64>,
        barriers: BTreeMap<u64, Gate>,
        dropped: bool,
    }

    #[derive(Debug)]
    struct Gate {
        entered: oneshot::Sender<()>,
        release: std_mpsc::Receiver<()>,
    }

    impl Faults {
        fn lock(&self) -> MutexGuard<'_, FaultPlan> {
            self.plan.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Загрузка откажет.
        pub fn fail_load(&self) {
            self.lock().fail_load = true;
        }

        /// Число начатых вызовов `commit`.
        pub fn commits(&self) -> u64 {
            self.lock().commits
        }

        /// Вызов `commit` с номером `call` откажет с `storage_unavailable`.
        pub fn fail_commit(&self, call: u64) {
            self.lock().fail_commits.insert(call);
        }

        /// Следующий вызов `commit` откажет. Ставится, пока записей в пути нет.
        pub fn fail_next_commit(&self) {
            let mut plan = self.lock();
            let call = plan.commits + 1;
            plan.fail_commits.insert(call);
        }

        /// Вызов `commit` с номером `call` остановится на барьере до его отпускания.
        pub fn barrier(&self, call: u64) -> CommitBarrier {
            let (entered_tx, entered_rx) = oneshot::channel();
            let (release_tx, release_rx) = std_mpsc::channel();
            self.lock().barriers.insert(
                call,
                Gate {
                    entered: entered_tx,
                    release: release_rx,
                },
            );
            CommitBarrier {
                entered: Some(entered_rx),
                release: release_tx,
            }
        }

        /// Барьер на следующем вызове `commit`. Ставится, пока записей в пути нет.
        pub fn barrier_next_commit(&self) -> CommitBarrier {
            let call = self.commits() + 1;
            self.barrier(call)
        }

        /// Хранилище освобождено: поток хранения завершился.
        pub fn is_dropped(&self) -> bool {
            self.lock().dropped
        }
    }

    /// Барьер записи. Брошенный барьер отпускает запись.
    #[derive(Debug)]
    pub struct CommitBarrier {
        entered: Option<oneshot::Receiver<()>>,
        release: std_mpsc::Sender<()>,
    }

    impl CommitBarrier {
        /// Дождаться, пока поток хранения дойдёт до барьера.
        pub async fn entered(&mut self) {
            if let Some(entered) = self.entered.take() {
                let _ = entered.await;
            }
        }

        /// Отпустить запись.
        pub fn release(self) {
            let _ = self.release.send(());
        }
    }
}
