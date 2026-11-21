//! JSON-петля транспортной границы для тестов: каждый запрос, ответ и событие `CoreHandle`
//! проходит через текст JSON и разбирается обратно, как через будущий сокет P1.2.
//!
//! Конверта запроса нет (он появится в P1.2): успех и отказ кодируются каждый своим типом,
//! а какая ветвь пришла, петля знает сама.

use std::error::Error;
use std::fmt;
use std::future::Future;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use crate::command::Command;
use crate::error::CoreError;
use crate::events::{EventsSince, Snapshot};
use crate::handle::CoreHandle;
use crate::model::Revision;
use crate::query::Query;
use crate::wire::{Event, Reply};

/// Результат вызова через петлю: внешний `Err` — сообщение не прошло JSON (дефект формата),
/// внутренний — ответ ядра, как у прямого вызова.
pub type Wired<T> = Result<Result<T, CoreError>, WireError>;

/// Сообщение не сериализовалось или не разобралось обратно.
#[derive(Debug)]
pub struct WireError {
    message: &'static str,
    json: Option<String>,
    source: serde_json::Error,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.json {
            None => write!(f, "{} не сериализуется: {}", self.message, self.source),
            Some(json) => write!(
                f,
                "{} не разбирается: {}: {json}",
                self.message, self.source
            ),
        }
    }
}

impl Error for WireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Значение после пути «текст JSON — разбор».
fn wire<T: Serialize + DeserializeOwned>(message: &'static str, value: &T) -> Result<T, WireError> {
    let json = serde_json::to_string(value).map_err(|source| WireError {
        message,
        json: None,
        source,
    })?;
    serde_json::from_str(&json).map_err(|source| WireError {
        message,
        json: Some(json),
        source,
    })
}

fn wire_result<T: Serialize + DeserializeOwned>(
    message: &'static str,
    result: Result<T, CoreError>,
) -> Wired<T> {
    Ok(match result {
        Ok(value) => Ok(wire(message, &value)?),
        Err(error) => Err(wire("ошибка", &error)?),
    })
}

/// `CoreHandle` за JSON-петлёй.
#[derive(Clone, Debug)]
pub struct Loopback {
    core: CoreHandle,
}

impl Loopback {
    pub fn new(core: CoreHandle) -> Self {
        Self { core }
    }

    /// [`CoreHandle::request`] через петлю: команда, ответ и ошибка проходят JSON.
    pub async fn request(&self, command: Command) -> Wired<Reply> {
        let command = wire("команда", &command)?;
        wire_result("ответ", self.core.request(command).await)
    }

    /// [`CoreHandle::query`] через петлю.
    pub async fn query(&self, query: Query) -> Wired<Reply> {
        let query = wire("чтение", &query)?;
        wire_result("ответ", self.core.query(query).await)
    }

    /// [`CoreHandle::subscribe`] через петлю: снимок и каждое событие проходят JSON.
    pub async fn subscribe(&self) -> Wired<(Snapshot, LoopbackEvents)> {
        Ok(match self.core.subscribe().await {
            Ok((snapshot, events)) => Ok((wire("снимок", &snapshot)?, LoopbackEvents { events })),
            Err(error) => Err(wire("ошибка", &error)?),
        })
    }

    /// [`CoreHandle::events_since`] через петлю.
    pub async fn events_since(&self, cursor: Revision) -> Wired<EventsSince> {
        let cursor = wire("курсор", &cursor)?;
        wire_result("события", self.core.events_since(cursor).await)
    }

    /// [`CoreHandle::shutdown`]: завершение через транспорт не идёт.
    pub fn shutdown(&self) -> impl Future<Output = ()> + Send + 'static {
        self.core.shutdown()
    }
}

/// Приёмник событий подписки, каждое событие которого проходит JSON.
#[derive(Debug)]
pub struct LoopbackEvents {
    events: broadcast::Receiver<Event>,
}

impl LoopbackEvents {
    /// Следующее событие, дождавшись его.
    pub async fn recv(&mut self) -> Result<Result<Event, RecvError>, WireError> {
        Ok(match self.events.recv().await {
            Ok(event) => Ok(wire("событие", &event)?),
            Err(error) => Err(error),
        })
    }

    /// Следующее событие без ожидания.
    pub fn try_recv(&mut self) -> Result<Result<Event, TryRecvError>, WireError> {
        Ok(match self.events.try_recv() {
            Ok(event) => Ok(wire("событие", &event)?),
            Err(error) => Err(error),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::runtime::Handle;

    use super::*;
    use crate::clock::{ManualClock, UnixMillis};
    use crate::handle::Core;
    use crate::ids::SequentialIds;
    use crate::persist::MemoryPersist;
    use crate::query::Query;
    use crate::root::FakeRootResolver;
    use crate::tree::TreeScope;

    #[test]
    fn wire_error_names_message_and_text() {
        let source = serde_json::from_str::<Revision>("\"x\"").unwrap_err();
        let error = WireError {
            message: "курсор",
            json: Some("\"x\"".to_owned()),
            source,
        };
        assert!(
            error.to_string().starts_with("курсор не разбирается: "),
            "{error}"
        );
        assert!(error.to_string().ends_with(": \"x\""), "{error}");
        assert!(error.source().is_some());
    }

    #[tokio::test]
    async fn loopback_passes_replies_errors_and_events() {
        let core = Core::start(
            &Handle::current(),
            Box::new(MemoryPersist::new()),
            Box::new(ManualClock::new(UnixMillis(1))),
            Box::new(SequentialIds::new()),
            Arc::new(FakeRootResolver::new()),
        )
        .unwrap();
        let wired = Loopback::new(core.clone());
        let (snapshot, mut events) = wired.subscribe().await.unwrap().unwrap();
        assert_eq!(snapshot, core.subscribe().await.unwrap().0);

        let created = Command::WorkspaceCreate {
            title: "Работа".into(),
        };
        let reply = wired.request(created.clone()).await.unwrap().unwrap();
        assert_eq!(reply.revision, Revision(2));
        let event = events.try_recv().unwrap().unwrap();
        assert_eq!(event.revision, Revision(2));
        assert!(matches!(
            events.try_recv().unwrap(),
            Err(TryRecvError::Empty)
        ));

        let refused = wired.request(created).await.unwrap().unwrap_err();
        assert_eq!(refused.code(), crate::error::ErrorCode::NameTaken);

        let tree = Query::Tree {
            scope: TreeScope::All,
        };
        assert_eq!(
            wired.query(tree.clone()).await.unwrap().unwrap(),
            core.query(tree).await.unwrap()
        );
        assert_eq!(
            wired.events_since(Revision(1)).await.unwrap().unwrap(),
            EventsSince::Events(vec![event])
        );

        wired.shutdown().await;
        assert_eq!(
            wired.subscribe().await.unwrap().unwrap_err(),
            CoreError::shutting_down()
        );
    }
}
