//! Кольцо квитанций UI (ADR-0007, «Квитанции UI» и «Уточнено в P1.6 (планирование)»): последние
//! завершения действий, исполненных главным потоком — клавишей маршрутизатора, пунктом меню или
//! кнопкой. Запись несёт имя действия, источник и исход; у `panel.open` ещё плагин, панель и
//! рабочую область. Свидетель «ровно один раз» для `ui.receipts`: данных команды в записи нет,
//! чтение кольцо не меняет.

use std::collections::VecDeque;

use shellio_control::{ReceiptSource, ReceiptView, ReceiptsView};
use shellio_core::{Command, CommandValue, CoreError, Reply, ReplyValue, WorkbenchId};

/// Сколько последних квитанций держит кольцо.
pub const CAPACITY: usize = 64;

/// Код записи, когда ядро не ответило на принятую команду.
pub const NO_REPLY: &str = "no_reply";

/// Код записи, когда действию не на что подействовать: окно скрыто или рабочая область не открыта.
pub const NOT_APPLICABLE: &str = "not_applicable";

/// Исполненное действие без его данных: имя, источник, а у `panel.open` — plugin и рабочая
/// область, куда шла команда.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub action: String,
    pub source: ReceiptSource,
    pub plugin: Option<String>,
    pub workbench: Option<WorkbenchId>,
}

impl Sent {
    /// Действие с именем keymap или именем команды ядра.
    pub fn action(action: &str, source: ReceiptSource) -> Sent {
        Sent {
            action: action.to_owned(),
            source,
            plugin: None,
            workbench: None,
        }
    }

    /// Команда `panel.open`; `None` — команда другая.
    pub fn opened(command: &Command, source: ReceiptSource) -> Option<Sent> {
        match command {
            Command::PanelOpen {
                plugin, workbench, ..
            } => Some(Sent {
                action: command.name().to_owned(),
                source,
                plugin: Some(plugin.clone()),
                workbench: *workbench,
            }),
            _ => None,
        }
    }

    /// Команда ядра действия `action`: у `panel.open` запись несёт плагин и рабочую область.
    pub fn command(action: &str, command: &Command, source: ReceiptSource) -> Sent {
        Self::opened(command, source).unwrap_or_else(|| Self::action(action, source))
    }
}

/// Чем завершилась квитанция команды.
#[derive(Clone, Copy, Debug)]
pub enum Ending<'a> {
    Reply(&'a Reply),
    /// Отказ ядра, включая ранний `busy` главного потока.
    Refused(&'a CoreError),
    NoReply,
    /// Действие без команды ядра выполнено.
    Done,
    /// Действию не на что подействовать, команда не подавалась.
    NotApplicable,
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
        let opening = sent.plugin.is_some();
        let mut receipt = ReceiptView {
            seq: self.last,
            action: sent.action,
            source: sent.source,
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
                // `panel.open` без панели в ответе — не ответ на неё.
                _ if opening => receipt.error = Some(NO_REPLY.to_owned()),
                _ => receipt.revision = Some(reply.revision),
            },
            Ending::Refused(error) => receipt.error = Some(error.code().as_str().to_owned()),
            Ending::NoReply => receipt.error = Some(NO_REPLY.to_owned()),
            Ending::Done => {}
            Ending::NotApplicable => receipt.error = Some(NOT_APPLICABLE.to_owned()),
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
            action: "panel.open".to_owned(),
            source: ReceiptSource::Menu,
            plugin: Some(plugin.to_owned()),
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
                action: "panel.open".into(),
                source: ReceiptSource::Menu,
                workbench: Some(workbench(2)),
                plugin: Some("demo-native".into()),
                outcome: Some(OpenOutcome::Moved),
                panel: Some(panel()),
                revision: Some(Revision(9)),
                error: None,
            }]
        );
    }

    #[test]
    fn the_record_past_capacity_evicts_the_oldest() {
        let mut ring = Ring::default();
        for _ in 0..=CAPACITY {
            ring.record(sent("terminal"), Ending::NoReply);
        }
        let view = ring.view(None);
        let last = CAPACITY as u64 + 1;
        assert_eq!(view.last, last);
        assert_eq!(view.receipts.len(), CAPACITY);
        assert_eq!(view.receipts.first().map(|r| r.seq), Some(2));
        assert_eq!(view.receipts.last().map(|r| r.seq), Some(last));
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
        let sent = Sent::opened(&command, ReceiptSource::Menu).unwrap();
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
            [
                "action",
                "outcome",
                "panel",
                "plugin",
                "revision",
                "seq",
                "source",
                "workbench"
            ]
        );
        assert!(!frame.to_string().contains("секрет"));
        let focus = Command::PanelFocus {
            panel: Some(panel()),
            workbench: None,
            direction: None,
        };
        assert_eq!(Sent::opened(&focus, ReceiptSource::Button), None);
        assert_eq!(
            Sent::command("panel.focus", &focus, ReceiptSource::Button),
            Sent::action("panel.focus", ReceiptSource::Button)
        );
    }

    #[test]
    fn an_action_records_its_name_source_and_outcome() {
        let mut ring = Ring::default();
        let done = Reply {
            revision: Revision(12),
            value: ReplyValue::Command(CommandValue::None),
        };
        let refused = CoreError::split_wrong_axis("stack");
        ring.record(
            Sent::action("ui.appearance.cycle", ReceiptSource::Key),
            Ending::Done,
        );
        ring.record(
            Sent::action("layout.toggle.navigator", ReceiptSource::Menu),
            Ending::Reply(&done),
        );
        ring.record(
            Sent::action("layout.split", ReceiptSource::Button),
            Ending::Refused(&refused),
        );
        ring.record(
            Sent::action("layout.toggle.side", ReceiptSource::Button),
            Ending::NotApplicable,
        );
        let frame = serde_json::to_value(ring.view(None)).unwrap();
        assert_eq!(
            frame["receipts"],
            json!([
                {"seq": 1, "action": "ui.appearance.cycle", "source": "key", "workbench": null},
                {
                    "seq": 2,
                    "action": "layout.toggle.navigator",
                    "source": "menu",
                    "workbench": null,
                    "revision": 12,
                },
                {
                    "seq": 3,
                    "action": "layout.split",
                    "source": "button",
                    "workbench": null,
                    "error": "invalid_argument",
                },
                {
                    "seq": 4,
                    "action": "layout.toggle.side",
                    "source": "button",
                    "workbench": null,
                    "error": NOT_APPLICABLE,
                },
            ])
        );
    }

    #[test]
    fn each_dispatch_is_one_record() {
        let mut ring = Ring::default();
        let before = ring.view(None).last;
        ring.record(
            Sent::action("layout.toggle.bottom", ReceiptSource::Key),
            Ending::Done,
        );
        let after = ring.view(Some(before));
        assert_eq!(after.last, before + 1);
        assert_eq!(after.receipts.len(), 1);
        assert_eq!(after.receipts[0].source, ReceiptSource::Key);
    }
}
