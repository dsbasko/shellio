//! Запуск ядра и `CoreHandle` — единственный вход в актор.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use shellio_layout::Viewport;
use shellio_plugin_api::Registry;
use tokio::runtime::Handle;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::actor::{Actor, QUEUE_CAPACITY, Request};
use crate::clock::Clock;
use crate::command::{Command, PreparedCommand};
use crate::error::CoreError;
use crate::events::{EventsSince, Snapshot};
use crate::ids::IdGen;
use crate::model::Revision;
use crate::persist::{LoadError, Persist};
use crate::query::Query;
use crate::root::RootResolver;
use crate::tree::TreeScope;
use crate::wire::{Event, Reply};

/// Квитанция ответа команды, поданной без ожидания.
pub type ReplyReceipt = oneshot::Receiver<Result<Reply, CoreError>>;

/// Ядро не запустилось.
#[derive(Debug)]
pub enum StartError {
    /// Поток хранения не создан.
    Spawn(std::io::Error),
    /// Загрузка не удалась. Ошибка хранилища как есть.
    Load(LoadError),
    /// Внутренняя команда запуска не выполнилась или не записалась.
    Startup(CoreError),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn(error) => write!(f, "поток хранения не создан: {error}"),
            Self::Load(error) => write!(f, "состояние не загружено: {error}"),
            Self::Startup(error) => write!(f, "запуск ядра не записан: {error}"),
        }
    }
}

impl Error for StartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spawn(error) => Some(error),
            Self::Load(error) => Some(&**error),
            Self::Startup(error) => Some(error),
        }
    }
}

/// Ядро состояния.
#[derive(Debug)]
pub struct Core;

impl Core {
    /// Загрузить состояние на потоке хранения, выполнить внутренние команды запуска и
    /// запустить актор на `runtime`. Блокирует вызывающий поток до конца загрузки и записи
    /// запуска, `runtime` для этого не нужен.
    ///
    /// `registry` — зарегистрированные плагины: по нему ядро считает статус панелей, `min_size`
    /// и отпечаток реестра, с которым сверяется отпечаток в хранилище.
    pub fn start(
        runtime: &Handle,
        persist: Box<dyn Persist>,
        clock: Box<dyn Clock>,
        ids: Box<dyn IdGen>,
        resolver: Arc<dyn RootResolver>,
        registry: Arc<Registry>,
    ) -> Result<CoreHandle, StartError> {
        let (viewport, viewport_rx) = watch::channel(None);
        let actor = Actor::start(persist, clock, ids, registry, viewport_rx)?;
        let (queue, inbox) = mpsc::channel(QUEUE_CAPACITY);
        let (stopped_tx, stopped) = watch::channel(false);
        runtime.spawn(actor.run(inbox, stopped_tx));
        Ok(CoreHandle {
            shared: Arc::new(Shared {
                queue,
                closing: AtomicBool::new(false),
                stopped,
                runtime: runtime.clone(),
                resolver,
                viewport,
            }),
        })
    }
}

/// Вход в ядро. Клонируется и передаётся между потоками.
#[derive(Clone)]
pub struct CoreHandle {
    shared: Arc<Shared>,
}

struct Shared {
    queue: mpsc::Sender<Request>,
    closing: AtomicBool,
    stopped: watch::Receiver<bool>,
    runtime: Handle,
    resolver: Arc<dyn RootResolver>,
    viewport: watch::Sender<Option<Viewport>>,
}

impl fmt::Debug for CoreHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CoreHandle")
            .field("closing", &self.shared.is_closing())
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    fn refuse_if_closing(&self) -> Result<(), CoreError> {
        if self.is_closing() {
            Err(CoreError::shutting_down())
        } else {
            Ok(())
        }
    }

    /// Подготовить команду. Ввод-вывод `project.add` идёт в `spawn_blocking`.
    async fn prepare(&self, command: Command) -> Result<PreparedCommand, CoreError> {
        if !command.needs_io() {
            return command.prepare(&*self.resolver);
        }
        let resolver = Arc::clone(&self.resolver);
        match self
            .runtime
            .spawn_blocking(move || command.prepare(&*resolver))
            .await
        {
            Ok(prepared) => prepared,
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(_) => Err(CoreError::shutting_down()),
        }
    }

    /// Занять место в очереди без ожидания.
    fn try_reserve(&self) -> Result<mpsc::Permit<'_, Request>, CoreError> {
        self.refuse_if_closing()?;
        self.queue.try_reserve().map_err(|error| match error {
            TrySendError::Full(()) => CoreError::busy(),
            TrySendError::Closed(()) => CoreError::shutting_down(),
        })
    }

    /// Поставить запрос в очередь, дождавшись места, и дождаться ответа.
    async fn call<T>(
        &self,
        request: impl FnOnce(oneshot::Sender<T>) -> Request,
    ) -> Result<T, CoreError> {
        self.refuse_if_closing()?;
        let (reply, receipt) = oneshot::channel();
        self.queue
            .send(request(reply))
            .await
            .map_err(|_| CoreError::shutting_down())?;
        receipt.await.map_err(|_| CoreError::shutting_down())
    }
}

impl CoreHandle {
    /// Выполнить команду в области всех воркспейсов. Равен [`Self::request_in`] с
    /// [`TreeScope::All`].
    pub async fn request(&self, command: Command) -> Result<Reply, CoreError> {
        self.request_in(TreeScope::All, command).await
    }

    /// Выполнить команду в области `scope`: ответ приходит после записи. Ждёт места в очереди.
    ///
    /// Принадлежность цели области проверяет актор на том же снимке, что и обработчик. Цель вне
    /// области — `not_found`, как у несуществующей записи. Прямой воркспейс вне области
    /// отвергается ещё до подготовки, без ввода-вывода.
    pub async fn request_in(&self, scope: TreeScope, command: Command) -> Result<Reply, CoreError> {
        self.shared.refuse_if_closing()?;
        let scope = command.in_scope(scope);
        scope.precheck()?;
        let command = self.shared.prepare(command).await?;
        self.shared
            .call(|reply| Request::Command {
                scope,
                command,
                reply,
            })
            .await?
    }

    /// Подать команду без ожидания в области всех воркспейсов. При полной очереди сразу
    /// `busy`, ответ — квитанцией.
    ///
    /// Команда с вводом-выводом (`project.add`) готовится задачей на runtime: она сразу
    /// получает квитанцию, а любой отказ, включая `busy` и `shutting_down`, приходит через
    /// неё. Брошенная квитанция принятую команду не отменяет.
    pub fn try_request(&self, command: Command) -> Result<ReplyReceipt, CoreError> {
        let (reply, receipt) = oneshot::channel();
        let scope = command.in_scope(TreeScope::All);
        if command.needs_io() {
            let shared = Arc::clone(&self.shared);
            self.shared.runtime.spawn(async move {
                let prepared = match shared.refuse_if_closing() {
                    Ok(()) => shared.prepare(command).await,
                    Err(error) => Err(error),
                };
                let permit = prepared.and_then(|command| Ok((command, shared.try_reserve()?)));
                match permit {
                    Ok((command, permit)) => permit.send(Request::Command {
                        scope,
                        command,
                        reply,
                    }),
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            });
            return Ok(receipt);
        }
        self.shared.refuse_if_closing()?;
        let command = command.prepare(&*self.shared.resolver)?;
        self.shared.try_reserve()?.send(Request::Command {
            scope,
            command,
            reply,
        });
        Ok(receipt)
    }

    /// Число запросов, которые стоят в очереди актора и ещё не взяты им. Тест гонки подтверждает
    /// им, что вторая команда ждёт, пока первая держит запись.
    #[cfg(feature = "test-support")]
    pub fn queued(&self) -> usize {
        self.shared.queue.max_capacity() - self.shared.queue.capacity()
    }

    /// Размер области содержимого окна (ADR-0007, «Внутренний вход размера окна»). Вызывает главный
    /// поток: вход не блокирует, не меняет ревизию и не порождает событий, его нет ни в протоколе,
    /// ни в `Command`. Команды и чтения, принятые актором после вызова, видят новый размер. `None` —
    /// окна нет: `layout.split` и `layout.resize` отказывают `no_ui`, `query.layout` отвечает без
    /// кадров. Повтор того же значения ничего не делает.
    pub fn set_viewport(&self, viewport: Option<Viewport>) {
        self.shared.viewport.send_if_modified(|current| {
            let changed = *current != viewport;
            *current = viewport;
            changed
        });
    }

    /// Выполнить чтение над текущим состоянием.
    pub async fn query(&self, query: Query) -> Result<Reply, CoreError> {
        self.shared
            .call(|reply| Request::Query { query, reply })
            .await?
    }

    /// Снимок с ревизией и приёмник событий начиная со следующей ревизии, без разрыва.
    pub async fn subscribe(&self) -> Result<(Snapshot, broadcast::Receiver<Event>), CoreError> {
        self.shared.call(|reply| Request::Subscribe { reply }).await
    }

    /// События после `cursor` или снимок, если буфер курсор не покрывает. Курсор больше
    /// текущей ревизии — `invalid_argument`.
    pub async fn events_since(&self, cursor: Revision) -> Result<EventsSince, CoreError> {
        self.shared
            .call(|reply| Request::EventsSince { cursor, reply })
            .await?
    }

    /// Завершить ядро. Запросы, поданные после вызова, получают `shutting_down`, даже пока
    /// future не опрошен. Принятые до него доводятся до ответа, поток хранения закрывается
    /// и присоединяется, затем future завершается.
    pub fn shutdown(&self) -> impl Future<Output = ()> + Send + 'static {
        self.shared.closing.store(true, Ordering::SeqCst);
        let shared = Arc::clone(&self.shared);
        async move {
            // Закрытая очередь значит, что актор уже завершается.
            let _ = shared.queue.send(Request::Shutdown).await;
            let mut stopped = shared.stopped.clone();
            // Ушедший отправитель тоже значит, что актор завершился.
            let _ = stopped.wait_for(|stopped| *stopped).await;
        }
    }
}
