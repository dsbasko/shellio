//! Снимок, кольцевой буфер событий и ответ `events_since`.

use std::collections::VecDeque;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::model::{Revision, State};
use crate::wire::Event;

/// Ёмкость кольцевого буфера событий.
pub const EVENT_BUFFER: usize = 1024;

/// Снимок состояния с его ревизией. Состояние разделяется с актором без копии, а в JSON
/// идёт значением.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub revision: Revision,
    #[serde(with = "shared_state")]
    pub state: Arc<State>,
}

/// Ответ `events_since`: события после курсора или снимок, если курсор вне буфера.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventsSince {
    /// События с ревизиями больше курсора, по порядку. Пусто — курсор равен текущей ревизии.
    Events(Vec<Event>),
    /// Курсор старше буфера или из прошлого запуска: текущий снимок вместо событий.
    Reset(Snapshot),
}

mod shared_state {
    use std::sync::Arc;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::model::State;

    pub(super) fn serialize<S: Serializer>(
        state: &Arc<State>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        State::serialize(state, serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<State>, D::Error> {
        State::deserialize(deserializer).map(Arc::new)
    }
}

/// Кольцевой буфер последних событий текущего запуска.
#[derive(Debug)]
pub(crate) struct EventLog {
    events: VecDeque<Event>,
    capacity: usize,
}

impl EventLog {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            events: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Добавить событие, вытеснив самое старое при полном буфере.
    pub(crate) fn push(&mut self, event: Event) {
        if self.events.len() == self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    /// События после `cursor` при текущей ревизии `current`, не больше которой `cursor`.
    /// `None` — буфер не покрывает курсор.
    ///
    /// Буфер покрывает курсоры от ревизии перед самым старым событием до текущей. Пустой
    /// буфер покрывает только текущую ревизию.
    pub(crate) fn since(&self, cursor: Revision, current: Revision) -> Option<Vec<Event>> {
        let oldest_cursor = self.events.front().map_or(current, |event| {
            Revision(event.revision.0.saturating_sub(1))
        });
        (cursor >= oldest_cursor).then(|| {
            self.events
                .iter()
                .filter(|event| event.revision > cursor)
                .cloned()
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{SequentialIds, WorkspaceId};
    use crate::model::Route;

    fn event(revision: u64) -> Event {
        let mut ids = SequentialIds::new();
        Event {
            revision: Revision(revision),
            changes: vec![crate::change::Change::WindowSet {
                active: WorkspaceId::generate(&mut ids),
                route: Route::WorkspaceHome,
            }],
        }
    }

    fn revisions(events: &[Event]) -> Vec<u64> {
        events.iter().map(|event| event.revision.0).collect()
    }

    /// Буфер на 3 события после записей 5..=9: в нём 7, 8, 9.
    fn filled() -> EventLog {
        let mut log = EventLog::new(3);
        for revision in 5..=9 {
            log.push(event(revision));
        }
        log
    }

    #[test]
    fn keeps_only_last_events() {
        let log = filled();
        assert_eq!(
            revisions(&log.since(Revision(6), Revision(9)).unwrap()),
            [7, 8, 9]
        );
    }

    #[test]
    fn cursor_inside_buffer_gets_later_events() {
        let log = filled();
        assert_eq!(
            revisions(&log.since(Revision(7), Revision(9)).unwrap()),
            [8, 9]
        );
        assert_eq!(log.since(Revision(9), Revision(9)), Some(Vec::new()));
    }

    #[test]
    fn cursor_before_buffer_is_not_covered() {
        assert_eq!(filled().since(Revision(5), Revision(9)), None);
        assert_eq!(filled().since(Revision(0), Revision(9)), None);
    }

    #[test]
    fn empty_buffer_covers_only_current_revision() {
        let log = EventLog::new(3);
        assert_eq!(log.since(Revision(4), Revision(4)), Some(Vec::new()));
        assert_eq!(log.since(Revision(3), Revision(4)), None);
    }

    #[test]
    fn snapshot_state_is_plain_json() {
        let snapshot = Snapshot {
            revision: Revision(2),
            state: Arc::new(State::default()),
        };
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["revision"], 2);
        assert_eq!(
            json["state"],
            serde_json::to_value(State::default()).unwrap()
        );
        assert_eq!(serde_json::from_value::<Snapshot>(json).unwrap(), snapshot);
    }
}
