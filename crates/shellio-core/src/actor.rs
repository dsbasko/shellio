//! Актор состояния: единственный владелец `State`, команды выполняются строго по одной.

use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::change::Change;
use crate::clock::Clock;
use crate::command::PreparedCommand;
use crate::error::CoreError;
use crate::events::{EVENT_BUFFER, EventLog, EventsSince, Snapshot};
use crate::handle::StartError;
use crate::handler::{Ctx, Outcome, handle, startup};
use crate::ids::IdGen;
use crate::model::{Revision, State};
use crate::persist::{Persist, StorageThread};
use crate::query::{Query, StorageInfo, read};
use crate::tree::TreeScope;
use crate::wire::{Event, Reply};

/// Ёмкость очереди запросов ядра.
pub const QUEUE_CAPACITY: usize = 256;

/// Ёмкость канала событий подписчика. Отставший больше неё получает `Lagged`.
pub const SUBSCRIBER_CAPACITY: usize = 256;

pub(crate) type ReplySender = oneshot::Sender<Result<Reply, CoreError>>;

/// Запрос в очереди актора.
pub(crate) enum Request {
    Command {
        scope: TreeScope,
        command: PreparedCommand,
        reply: ReplySender,
    },
    Query {
        query: Query,
        reply: ReplySender,
    },
    Subscribe {
        reply: oneshot::Sender<(Snapshot, broadcast::Receiver<Event>)>,
    },
    EventsSince {
        cursor: Revision,
        reply: oneshot::Sender<Result<EventsSince, CoreError>>,
    },
    /// Принятое до него доводится до ответа, затем очередь закрывается.
    Shutdown,
}

pub(crate) struct Actor {
    state: Arc<State>,
    revision: Revision,
    storage_info: StorageInfo,
    log: EventLog,
    events: broadcast::Sender<Event>,
    storage: StorageThread,
    clock: Box<dyn Clock>,
    ids: Box<dyn IdGen>,
}

impl Actor {
    /// Загрузить состояние на потоке хранения и выполнить внутренние команды запуска.
    pub(crate) fn start(
        persist: Box<dyn Persist>,
        clock: Box<dyn Clock>,
        ids: Box<dyn IdGen>,
    ) -> Result<Self, StartError> {
        let (storage, loaded) = StorageThread::spawn(persist)?;
        let (events, _) = broadcast::channel(SUBSCRIBER_CAPACITY);
        let mut actor = Self {
            state: Arc::new(loaded.state),
            revision: loaded.revision,
            storage_info: loaded.storage,
            log: EventLog::new(EVENT_BUFFER),
            events,
            storage,
            clock,
            ids,
        };
        match actor.startup() {
            Ok(()) => Ok(actor),
            Err(error) => {
                actor.storage.stop_blocking();
                Err(StartError::Startup(error))
            }
        }
    }

    /// Загрузка с находками описывает базу, которую правили в обход ядра: курсор прошлого
    /// запуска на ревизии загрузки устарел. Тогда запуск записывает одну запись, пусть и без
    /// дельт, и не кладёт её в буфер: курсоры этого запуска больше ревизии загрузки, а все
    /// прежние получают снимок.
    fn startup(&mut self) -> Result<(), CoreError> {
        let mut ctx = Ctx::new(&*self.clock, &mut *self.ids);
        let changes = startup(&self.state, &mut ctx)?.map(|outcome| outcome.changes);
        if self.state.diagnostics.is_empty() {
            if let Some(changes) = changes {
                self.storage
                    .commit_blocking(self.revision, changes.clone())?;
                self.publish(changes);
            }
            return Ok(());
        }
        let changes = changes.unwrap_or_default();
        self.storage
            .commit_blocking(self.revision, changes.clone())?;
        Arc::make_mut(&mut self.state).apply_all(&changes);
        self.revision = self.revision.next();
        Ok(())
    }

    /// Цикл актора до `Shutdown` или до ухода всех `CoreHandle`.
    ///
    /// `Shutdown` закрывает очередь: новые запросы получают отказ, а уже стоящие в ней
    /// доводятся до ответа, после чего `recv` даёт `None`.
    pub(crate) async fn run(
        mut self,
        mut inbox: mpsc::Receiver<Request>,
        stopped: watch::Sender<bool>,
    ) {
        while let Some(request) = inbox.recv().await {
            match request {
                Request::Shutdown => inbox.close(),
                request => self.serve(request).await,
            }
        }
        let Self {
            storage, events, ..
        } = self;
        drop(events);
        storage.stop().await;
        stopped.send_replace(true);
    }

    // Отправка ответа не проверяется: брошенная квитанция команду не отменяет.
    async fn serve(&mut self, request: Request) {
        match request {
            Request::Command {
                scope,
                command,
                reply,
            } => {
                let result = self.command(scope, command).await;
                let _ = reply.send(result);
            }
            Request::Query { query, reply } => {
                let _ = reply.send(read(&self.state, self.revision, &self.storage_info, &query));
            }
            Request::Subscribe { reply } => {
                let _ = reply.send((self.snapshot(), self.events.subscribe()));
            }
            Request::EventsSince { cursor, reply } => {
                let _ = reply.send(self.events_since(cursor));
            }
            Request::Shutdown => {}
        }
    }

    async fn command(
        &mut self,
        scope: TreeScope,
        command: PreparedCommand,
    ) -> Result<Reply, CoreError> {
        command.check_scope(&self.state, scope)?;
        let mut ctx = Ctx::new(&*self.clock, &mut *self.ids);
        let Outcome { value, changes } = handle(&self.state, command, &mut ctx)?;
        if !changes.is_empty() {
            self.storage.commit(self.revision, changes.clone()).await?;
            self.publish(changes);
        }
        Ok(Reply {
            revision: self.revision,
            value: value.into(),
        })
    }

    /// Применить записанные дельты: состояние, ревизия, буфер и подписчики.
    fn publish(&mut self, changes: Vec<Change>) {
        Arc::make_mut(&mut self.state).apply_all(&changes);
        self.revision = self.revision.next();
        let event = Event {
            revision: self.revision,
            changes,
        };
        // Подписчиков может не быть: событие остаётся в буфере.
        let _ = self.events.send(event.clone());
        self.log.push(event);
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            revision: self.revision,
            state: Arc::clone(&self.state),
        }
    }

    fn events_since(&self, cursor: Revision) -> Result<EventsSince, CoreError> {
        if cursor > self.revision {
            return Err(CoreError::invalid_argument(
                "cursor",
                "cursor_ahead",
                format!(
                    "курсор {} больше текущей ревизии {}",
                    cursor.0, self.revision.0
                ),
            ));
        }
        Ok(match self.log.since(cursor, self.revision) {
            Some(events) => EventsSince::Events(events),
            None => EventsSince::Reset(self.snapshot()),
        })
    }
}
