//! Порт [`Ui`] владельца над `shellio-appkit`: главный поток получает работу только через
//! [`MainThread`], ответы возвращаются каналами, делегат лишь отмечает события.

use std::future::Future;
use std::sync::Arc;

use shellio::owner::{self, Handoff, Ui};
use shellio_appkit::{App, Delegate, MainThread, Transfer};
use shellio_control::Activation;
use tokio::sync::{oneshot, watch};

/// UI владельца на `NSApplication`.
pub struct AppUi {
    main: MainThread,
    ready: watch::Sender<bool>,
    /// Приложение хотя бы раз стало активным. Флаг, а не событие: активность, которую
    /// LaunchServices дал до начала передачи, не теряется.
    became_active: watch::Sender<bool>,
    terminate: watch::Sender<bool>,
}

impl AppUi {
    pub fn new() -> Self {
        Self {
            main: MainThread::new(),
            ready: watch::Sender::new(false),
            became_active: watch::Sender::new(false),
            terminate: watch::Sender::new(false),
        }
    }

    /// Выполнить `work` на главном потоке и дождаться его ответа. `None` — замыкание
    /// отброшено, не выполнившись.
    async fn on_main<T: Send + 'static>(
        &self,
        work: impl FnOnce(&App) -> T + Send + 'static,
    ) -> Option<T> {
        let (reply, answer) = oneshot::channel();
        self.main.exec(move |app| {
            let _ = reply.send(work(app));
        });
        answer.await.ok()
    }
}

/// Дождаться, пока флаг станет `true`. Отправитель живёт в [`AppUi`], поэтому канал не
/// закрывается, пока ждут.
async fn latch(flag: &watch::Sender<bool>) {
    let mut receiver = flag.subscribe();
    let _ = receiver.wait_for(|set| *set).await;
}

impl Ui for AppUi {
    fn ready(&self) -> impl Future<Output = ()> + Send {
        latch(&self.ready)
    }

    async fn become_owner(&self) {
        if self.on_main(App::become_regular).await == Some(false) {
            owner::say_err("shellio: AppKit отказал в политике .regular, активации не будет");
        }
    }

    async fn activate(&self) -> Activation {
        match self.on_main(App::activate).await {
            Some(()) => Activation::Requested,
            // Главный поток не выполнил вызов: UI уже нет.
            None => Activation::NoUi,
        }
    }

    async fn hand_off_activation(&self, pid: u32) -> Handoff {
        // Передача идёт от запущенного приложения: до `applicationDidFinishLaunching` его ещё нет.
        latch(&self.ready).await;
        let Some(active) = self
            .on_main(|app| {
                app.request_activation();
                app.is_active()
            })
            .await
        else {
            return handoff(None);
        };
        // Передать можно только свою активность. Не пришла — ожидание обрывает срок передачи
        // в `claim`, и `launch` уходит без неё.
        if !active {
            latch(&self.became_active).await;
        }
        handoff(
            self.on_main(move |app| app.hand_off_activation_to(pid))
                .await,
        )
    }

    fn terminate_requested(&self) -> impl Future<Output = ()> + Send {
        latch(&self.terminate)
    }

    fn finish_terminate(&self) {
        owner::flush_output();
        self.main.exec(App::finish_terminate);
    }

    fn exit(&self, code: u8) {
        // Выход на главном потоке: цикл AppKit не продолжает работу посреди `exit`.
        self.main.exec(move |_| owner::exit_process(code));
    }
}

/// Факты передачи для порта. `None` — главный поток не выполнил передачу: UI уже нет.
fn handoff(transfer: Option<Transfer>) -> Handoff {
    match transfer {
        Some(Transfer {
            self_active,
            owner_found,
            activated,
        }) => Handoff {
            self_active,
            owner_found,
            activated,
        },
        None => Handoff {
            self_active: false,
            owner_found: false,
            activated: None,
        },
    }
}

/// Делегат приложения: отмечает готовность, активность и запрос завершения и сразу
/// возвращается.
pub struct AppDelegate {
    ui: Arc<AppUi>,
}

impl AppDelegate {
    pub fn new(ui: Arc<AppUi>) -> Self {
        Self { ui }
    }
}

impl Delegate for AppDelegate {
    fn did_finish_launching(&self, _app: &App) {
        self.ui.ready.send_replace(true);
    }

    fn did_become_active(&self, _app: &App) {
        self.ui.became_active.send_replace(true);
    }

    fn terminate_requested(&self, _app: &App) {
        self.ui.terminate.send_replace(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_keeps_every_fact_of_the_transfer() {
        let transfer = |self_active, owner_found, activated| Transfer {
            self_active,
            owner_found,
            activated,
        };
        assert_eq!(
            handoff(Some(transfer(true, true, Some(true)))),
            Handoff {
                self_active: true,
                owner_found: true,
                activated: Some(true),
            }
        );
        assert_eq!(
            handoff(Some(transfer(false, true, Some(false)))),
            Handoff {
                self_active: false,
                owner_found: true,
                activated: Some(false),
            }
        );
        assert_eq!(
            handoff(Some(transfer(true, false, None))),
            Handoff {
                self_active: true,
                owner_found: false,
                activated: None,
            }
        );
    }

    #[test]
    fn handoff_without_main_thread_reports_nothing_done() {
        let facts = handoff(None);
        assert_eq!(
            facts,
            Handoff {
                self_active: false,
                owner_found: false,
                activated: None,
            }
        );
        assert_eq!(
            owner::handoff_line(7, owner::HandoffOutcome::Done(facts)),
            "shellio: передача активации pid 7: второй запуск активен — нет, приложение владельца \
             найдено — нет, активация владельца — не запрашивалась"
        );
    }
}
