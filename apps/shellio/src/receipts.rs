//! Кольцо квитанций UI (ADR-0007, «Квитанции UI»): последние завершения `panel.open`, поданных
//! главным потоком — пунктом меню «Открыть панель» и отправителем вида плагина. Свидетель доставки
//! команды для `ui.receipts`: данных команды в записи нет, чтение кольцо не меняет.

use std::collections::VecDeque;

use shellio_control::{ReceiptView, ReceiptsView};
use shellio_core::{Command, CommandValue, CoreError, Reply, ReplyValue, WorkbenchId};

/// Сколько последних квитанций держит кольцо.
pub const CAPACITY: usize = 16;

/// Код записи, когда ядро не ответило на принятую команду.
pub const NO_REPLY: &str = "no_reply";

/// Поданная команда `panel.open` без её данных: plugin и рабочая область, куда она шла.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub plugin: String,
    pub workbench: Option<WorkbenchId>,
}

impl Sent {
    /// `None` — команда не `panel.open`: в кольцо она не пишется.
    pub fn of(command: &Command) -> Option<Sent> {
        match command {
            Command::PanelOpen {
                plugin, workbench, ..
            } => Some(Sent {
                plugin: plugin.clone(),
                workbench: *workbench,
            }),
            _ => None,
        }
    }
}

/// Чем завершилась квитанция команды.
#[derive(Clone, Copy, Debug)]
pub enum Ending<'a> {
    Reply(&'a Reply),
    /// Отказ ядра, включая ранний `busy` главного потока.
    Refused(&'a CoreError),
    NoReply,
}

/// Кольцо квитанций процесса. Номера идут с 1 и не повторяются.
#[derive(Debug, Default)]
pub struct Ring {
    last: u64,
    items: VecDeque<ReceiptView>,
}

impl Ring {
    /// Записать завершение команды `sent`; при переполнении вытесняется самая старая запись.
    /// Ответ — номер записи.
    pub fn record(&mut self, sent: Sent, ending: Ending<'_>) -> u64 {
        self.last += 1;
        let mut receipt = ReceiptView {
            seq: self.last,
            workbench: sent.workbench,
            plugin: sent.plugin,
            outcome: None,
            panel: None,
            revision: None,
            error: None,
        };
        match ending {
            Ending::Reply(reply) => match reply.value {
                ReplyValue::Command(CommandValue::Opened {
                    panel,
                    workbench,
                    outcome,
                }) => {
                    receipt.workbench = Some(workbench);
                    receipt.outcome = Some(outcome);
                    receipt.panel = Some(panel);
                    receipt.revision = Some(reply.revision);
                }
                _ => receipt.error = Some(NO_REPLY.to_owned()),
            },
            Ending::Refused(error) => receipt.error = Some(error.code().as_str().to_owned()),
            Ending::NoReply => receipt.error = Some(NO_REPLY.to_owned()),
        }
        if self.items.len() == CAPACITY {
            self.items.pop_front();
        }
        self.items.push_back(receipt);
        self.last
    }

    /// Записи с номером больше `after` по возрастанию номера и номер последней записанной.
    pub fn view(&self, after: Option<u64>) -> ReceiptsView {
        let after = after.unwrap_or(0);
        ReceiptsView {
            last: self.last,
            receipts: self
                .items
                .iter()
                .filter(|receipt| receipt.seq > after)
                .cloned()
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shellio_core::{OpenOutcome, PanelId, Revision};
    use shellio_layout::Zone;

    use super::*;

    fn workbench(n: u8) -> WorkbenchId {
        format!("019a8f2e-0000-7000-8000-0000000000{n:02x}")
            .parse()
            .unwrap()
    }

    fn panel() -> PanelId {
        "019a8f2e-0000-7000-8000-000000000a07".parse().unwrap()
    }

    fn sent(plugin: &str) -> Sent {
        Sent {
            plugin: plugin.to_owned(),
            workbench: Some(workbench(1)),
        }
    }

    fn opened(revision: u64, outcome: OpenOutcome) -> Reply {
        Reply {
            revision: Revision(revision),
            value: ReplyValue::Command(CommandValue::Opened {
                panel: panel(),
                workbench: workbench(2),
                outcome,
            }),
        }
    }

    #[test]
    fn numbers_start_at_one_and_grow_by_one() {
        let mut ring = Ring::default();
        assert_eq!(ring.view(None), ReceiptsView::default());
        let reply = opened(5, OpenOutcome::Created);
        assert_eq!(ring.record(sent("demo-list"), Ending::Reply(&reply)), 1);
        assert_eq!(ring.record(sent("terminal"), Ending::NoReply), 2);
        let view = ring.view(None);
        assert_eq!(view.last, 2);
        assert_eq!(
            view.receipts.iter().map(|r| r.seq).collect::<Vec<_>>(),
            [1, 2]
        );
    }

    #[test]
    fn success_carries_panel_revision_and_the_workbench_of_the_reply() {
        let mut ring = Ring::default();
        let reply = opened(9, OpenOutcome::Moved);
        ring.record(sent("demo-native"), Ending::Reply(&reply));
        let view = ring.view(None);
        assert_eq!(
            view.receipts,
            [ReceiptView {
                seq: 1,
                workbench: Some(workbench(2)),
                plugin: "demo-native".into(),
                outcome: Some(OpenOutcome::Moved),
                panel: Some(panel()),
                revision: Some(Revision(9)),
                error: None,
            }]
        );
    }

    #[test]
    fn the_seventeenth_record_evicts_the_oldest() {
        let mut ring = Ring::default();
        for _ in 0..=CAPACITY {
            ring.record(sent("terminal"), Ending::NoReply);
        }
        let view = ring.view(None);
        assert_eq!(view.last, 17);
        assert_eq!(view.receipts.len(), CAPACITY);
        assert_eq!(view.receipts.first().map(|r| r.seq), Some(2));
        assert_eq!(view.receipts.last().map(|r| r.seq), Some(17));
    }

    #[test]
    fn after_keeps_only_later_records() {
        let mut ring = Ring::default();
        for _ in 0..4 {
            ring.record(sent("terminal"), Ending::NoReply);
        }
        let seqs = |after| {
            ring.view(after)
                .receipts
                .iter()
                .map(|r| r.seq)
                .collect::<Vec<_>>()
        };
        assert_eq!(seqs(Some(2)), [3, 4]);
        assert_eq!(seqs(Some(0)), [1, 2, 3, 4]);
        assert!(seqs(Some(4)).is_empty());
        assert!(seqs(Some(u64::MAX)).is_empty());
        assert_eq!(
            ring.view(Some(4)).last,
            4,
            "номер последней не зависит от after"
        );
    }

    #[test]
    fn reading_changes_nothing() {
        let mut ring = Ring::default();
        ring.record(sent("terminal"), Ending::NoReply);
        let first = ring.view(None);
        let filtered = ring.view(Some(1));
        assert!(filtered.receipts.is_empty());
        assert_eq!(ring.view(None), first, "чтение записей не убирает");
        assert_eq!(ring.record(sent("terminal"), Ending::NoReply), 2);
    }

    #[test]
    fn busy_and_core_refusal_differ_by_code() {
        let mut ring = Ring::default();
        let busy = CoreError::busy();
        let unknown = CoreError::plugin_unknown("plugin", "gone");
        ring.record(sent("terminal"), Ending::Refused(&busy));
        ring.record(sent("gone"), Ending::Refused(&unknown));
        ring.record(sent("terminal"), Ending::NoReply);
        let codes: Vec<_> = ring
            .view(None)
            .receipts
            .into_iter()
            .map(|receipt| {
                assert_eq!(
                    (receipt.outcome, receipt.panel, receipt.revision),
                    (None, None, None)
                );
                receipt.error
            })
            .collect();
        assert_eq!(
            codes,
            [
                Some("busy".to_owned()),
                Some("invalid_argument".to_owned()),
                Some(NO_REPLY.to_owned())
            ]
        );
    }

    #[test]
    fn only_panel_open_is_recorded_and_its_data_stay_out() {
        let command = Command::PanelOpen {
            plugin: "demo-list".into(),
            workbench: Some(workbench(1)),
            zone: Some(Zone::Bottom),
            args: Some(json!({"key": "секрет"})),
        };
        let sent = Sent::of(&command).unwrap();
        assert_eq!(sent, self::sent("demo-list"));
        let mut ring = Ring::default();
        let reply = opened(3, OpenOutcome::Created);
        ring.record(sent, Ending::Reply(&reply));
        let frame = serde_json::to_value(ring.view(None)).unwrap();
        let record = frame["receipts"][0].as_object().unwrap();
        let mut keys: Vec<&str> = record.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["outcome", "panel", "plugin", "revision", "seq", "workbench"]
        );
        assert!(!frame.to_string().contains("секрет"));
        let focus = Command::PanelFocus {
            panel: Some(panel()),
            workbench: None,
            direction: None,
        };
        assert_eq!(Sent::of(&focus), None);
    }
}
