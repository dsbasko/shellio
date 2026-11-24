//! Запросы тела к хосту: команда своей панели и открытие панели любого плагина.

use std::fmt;
use std::rc::Rc;

use serde_json::Value;
use shellio_plugin_api::Refusal;

/// Запрос тела. Хост превращает его в `panel.command` панели тела или в `panel.open` с её рабочей
/// областью.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelRequest {
    /// Действие манифеста своего плагина: полное имя и `args` объектом JSON.
    Command { name: String, args: Value },
    /// Открыть панель плагина `plugin` с контекстом `args`.
    Open { plugin: String, args: Option<Value> },
}

/// Исход запроса на главном потоке: успех или отказ, ранний `busy` — тоже отказ. Хост вызывает его,
/// только если тело живо и запрос у тела последний.
pub type Done = Box<dyn FnOnce(Result<(), Refusal>)>;

/// Отправитель запросов тела. Отправка не ждёт актора.
#[derive(Clone)]
pub struct PanelSink {
    send: Rc<dyn Fn(PanelRequest, Done)>,
}

impl PanelSink {
    /// Отправитель хоста: `send` получает запрос и исход на главном потоке.
    pub fn new(send: impl Fn(PanelRequest, Done) + 'static) -> Self {
        Self {
            send: Rc::new(send),
        }
    }

    pub fn send(&self, request: PanelRequest, done: Done) {
        (self.send)(request, done);
    }
}

impl fmt::Debug for PanelSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PanelSink").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;

    #[test]
    fn sink_hands_the_request_and_its_outcome_to_the_host() {
        let inbox = Rc::new(RefCell::new(Vec::<(PanelRequest, Done)>::new()));
        let sink = PanelSink::new({
            let inbox = Rc::clone(&inbox);
            move |request, done| inbox.borrow_mut().push((request, done))
        });
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let record = |outcomes: &Rc<RefCell<Vec<Result<(), String>>>>| -> Done {
            let outcomes = Rc::clone(outcomes);
            Box::new(move |outcome| {
                outcomes
                    .borrow_mut()
                    .push(outcome.map_err(|refusal| refusal.reason));
            })
        };

        let command = PanelRequest::Command {
            name: "demo.set".into(),
            args: json!({"value": "a"}),
        };
        let open = PanelRequest::Open {
            plugin: "terminal".into(),
            args: None,
        };
        sink.send(command.clone(), record(&outcomes));
        sink.clone().send(open.clone(), record(&outcomes));
        assert!(outcomes.borrow().is_empty(), "отправка не ждёт исхода");

        let mut sent = inbox.borrow_mut().drain(..).collect::<Vec<_>>();
        assert_eq!(sent.len(), 2);
        let (second, refuse) = sent.remove(1);
        let (first, accept) = sent.remove(0);
        assert_eq!((first, second), (command, open));
        accept(Ok(()));
        refuse(Err(Refusal::new("plugin", "busy", "ядро занято")));
        assert_eq!(*outcomes.borrow(), [Ok(()), Err("busy".to_owned())]);
    }
}
