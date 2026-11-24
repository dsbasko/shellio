//! Актор состояния: единственный владелец `State`, команды выполняются строго по одной.

use std::sync::Arc;

use shellio_layout::Viewport;
use shellio_plugin_api::Registry;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::change::Change;
use crate::clock::Clock;
use crate::command::{CommandScope, PreparedCommand};
use crate::error::CoreError;
use crate::events::{EVENT_BUFFER, EventLog, EventsSince, Snapshot};
use crate::handle::StartError;
use crate::handler::{Ctx, Outcome, handle, startup};
use crate::ids::IdGen;
use crate::model::{Revision, State};
use crate::persist::{Persist, StorageThread};
use crate::plugin_host::{fingerprint, load_panels};
use crate::query::{Query, StorageInfo, read};
use crate::wire::{Event, Reply};

/// Ёмкость очереди запросов ядра.
pub const QUEUE_CAPACITY: usize = 256;

/// Ёмкость канала событий подписчика. Отставший больше неё получает `Lagged`.
pub const SUBSCRIBER_CAPACITY: usize = 256;

pub(crate) type ReplySender = oneshot::Sender<Result<Reply, CoreError>>;

/// Запрос в очереди актора.
pub(crate) enum Request {
    Command {
        scope: CommandScope,
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
    registry: Arc<Registry>,
    /// Размер окна от главного потока: читается на каждом запросе, ревизию не меняет.
    viewport: watch::Receiver<Option<Viewport>>,
}

impl Actor {
    /// Загрузить состояние на потоке хранения и выполнить внутренние команды запуска.
    pub(crate) fn start(
        persist: Box<dyn Persist>,
        clock: Box<dyn Clock>,
        ids: Box<dyn IdGen>,
        registry: Arc<Registry>,
        viewport: watch::Receiver<Option<Viewport>>,
    ) -> Result<Self, StartError> {
        let (storage, loaded) = StorageThread::spawn(persist)?;
        let (events, _) = broadcast::channel(SUBSCRIBER_CAPACITY);
        let mut state = loaded.state;
        // Статус — производное значение: что бы ни отдало хранилище, запуск считает его заново.
        state.panel_status.clear();
        let mut actor = Self {
            state: Arc::new(state),
            revision: loaded.revision,
            storage_info: loaded.storage,
            log: EventLog::new(EVENT_BUFFER),
            events,
            storage,
            clock,
            ids,
            registry,
            viewport,
        };
        match actor.startup(loaded.fingerprint.as_deref()) {
            Ok(()) => Ok(actor),
            Err(error) => {
                actor.storage.stop_blocking();
                Err(StartError::Startup(error))
            }
        }
    }

    /// Запись запуска: окно, недостающие раскладки, `init` и `migrate` данных панелей и новый
    /// отпечаток реестра `stored` — одной транзакцией. Статусы загруженных панелей ставятся в
    /// состояние без дельт, до первого снимка.
    ///
    /// Загрузка с находками описывает базу, которую правили в обход ядра, а другой отпечаток —
    /// реестр, по которому прежний владелец считал статусы иначе: курсор прошлого запуска на
    /// ревизии загрузки устарел. Тогда запуск записывает одну запись, пусть и без дельт, и не
    /// кладёт её в буфер: курсоры этого запуска больше ревизии загрузки, а все прежние получают
    /// снимок. Без находок, без смены отпечатка и без дельт записи нет.
    fn startup(&mut self, stored: Option<&str>) -> Result<(), CoreError> {
        let mut ctx = Ctx::new(&*self.clock, &mut *self.ids, &self.registry);
        let mut changes = startup(&self.state, &mut ctx)?
            .map(|outcome| outcome.changes)
            .unwrap_or_default();
        let panels = load_panels(&self.state, &self.registry);
        changes.extend(panels.changes);
        Arc::make_mut(&mut self.state).panel_status = panels.statuses;

        let fingerprint = fingerprint(&self.registry);
        let registry_changed = stored != Some(fingerprint.as_str());
        let stale_cursors = registry_changed || !self.state.diagnostics.is_empty();
        if changes.is_empty() && !stale_cursors {
            return Ok(());
        }
        self.storage.commit_blocking(
            self.revision,
            changes.clone(),
            registry_changed.then_some(fingerprint),
        )?;
        if stale_cursors {
            Arc::make_mut(&mut self.state).apply_all(&changes);
            self.revision = self.revision.next();
        } else {
            self.publish(changes);
        }
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
                let viewport = *self.viewport.borrow();
                let _ = reply.send(read(
                    &self.state,
                    &self.registry,
                    self.revision,
                    &self.storage_info,
                    viewport,
                    &query,
                ));
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
        scope: CommandScope,
        command: PreparedCommand,
    ) -> Result<Reply, CoreError> {
        scope.check(&self.state)?;
        let viewport = *self.viewport.borrow();
        let mut ctx =
            Ctx::new(&*self.clock, &mut *self.ids, &self.registry).with_viewport(viewport);
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
