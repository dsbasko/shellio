//! Порт [`Ui`] владельца над `shellio-appkit`: главный поток получает работу только через
//! [`MainThread`], ответы возвращаются каналами, делегат лишь отмечает события.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use shellio::owner::{self, Handoff, Reopen, Ui};
use shellio_appkit::{App, Delegate, MainThread, Reopened, Transfer};
use shellio_control::Activation;
use tokio::sync::{oneshot, watch};

/// Сколько второй запуск ждёт собственной активности, прежде чем просить LaunchServices. Своя
/// активность передаётся по PID и верна при любом числе владельцев, LaunchServices выбирает
/// экземпляр бандла сам, поэтому идёт только без неё. Остаток срока передачи (1 с от `hello`)
/// оставлен на ответ LaunchServices.
const OWN_ACTIVITY_WAIT: Duration = Duration::from_millis(500);

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

    /// Просит LaunchServices открыть бандл владельца `pid` с активацией и ждёт ответа.
    async fn reopen(&self, pid: u32) -> Reopen {
        let (reply, answer) = oneshot::channel();
        self.main.exec(move |app| {
            app.reopen_bundle_of(pid, move |reopened| {
                let _ = reply.send(reopened);
            });
        });
        reopen(answer.await.ok())
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
        // Окно закрывается и тогда, когда срок передачи в `claim` отбросит этот future.
        let _window = ServiceWindow(self.main);
        let Some(active) = self
            .on_main(|app| {
                app.show_service_window();
                app.request_activation();
                app.is_active()
            })
            .await
        else {
            return handoff(None, Reopen::NotRequested);
        };
        let own = active
            || tokio::time::timeout(OWN_ACTIVITY_WAIT, latch(&self.became_active))
                .await
                .is_ok();
        let reopen = if own {
            Reopen::NotRequested
        } else {
            self.reopen(pid).await
        };
        // Передать можно только свою активность. Не пришла и LaunchServices не открыл владельца —
        // ожидание обрывает срок передачи в `claim`, и `launch` уходит без неё.
        if !own && reopen != Reopen::Opened(pid) {
            owner::say_err(owner::reopen_line(pid, reopen));
            latch(&self.became_active).await;
        }
        handoff(
            self.on_main(move |app| app.hand_off_activation_to(pid))
                .await,
            reopen,
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

/// Служебное окно второго запуска на время передачи: закрывается при выходе из неё.
struct ServiceWindow(MainThread);

impl Drop for ServiceWindow {
    fn drop(&mut self) {
        self.0.exec(App::close_service_window);
    }
}

/// Факты передачи для порта. `None` — главный поток не выполнил передачу: UI уже нет.
fn handoff(transfer: Option<Transfer>, reopen: Reopen) -> Handoff {
    match transfer {
        Some(Transfer {
            self_active,
            owner_found,
            activated,
        }) => Handoff {
            self_active,
            owner_found,
            activated,
            reopen,
        },
        None => Handoff {
            self_active: false,
            owner_found: false,
            activated: None,
            reopen,
        },
    }
}

/// Ответ LaunchServices для порта. `None` — ответа не будет: UI уже нет.
fn reopen(reopened: Option<Reopened>) -> Reopen {
    match reopened {
        Some(Reopened::Opened(pid)) => {
            u32::try_from(pid).map_or(Reopen::Failed(None), Reopen::Opened)
        }
        Some(Reopened::Failed(code)) => {
            Reopen::Failed(code.and_then(|code| i64::try_from(code).ok()))
        }
        Some(Reopened::NoBundle) | None => Reopen::NotRequested,
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
            handoff(Some(transfer(true, true, Some(true))), Reopen::Opened(7)),
            Handoff {
                self_active: true,
                owner_found: true,
                activated: Some(true),
                reopen: Reopen::Opened(7),
            }
        );
        assert_eq!(
            handoff(
                Some(transfer(false, true, Some(false))),
                Reopen::Failed(Some(-600))
            ),
            Handoff {
                self_active: false,
                owner_found: true,
                activated: Some(false),
                reopen: Reopen::Failed(Some(-600)),
            }
        );
        assert_eq!(
            handoff(Some(transfer(true, false, None)), Reopen::NotRequested),
            Handoff {
                self_active: true,
                owner_found: false,
                activated: None,
                reopen: Reopen::NotRequested,
            }
        );
    }

    #[test]
    fn handoff_without_main_thread_reports_nothing_done() {
        let facts = handoff(None, Reopen::NotRequested);
        assert_eq!(
            facts,
            Handoff {
                self_active: false,
                owner_found: false,
                activated: None,
                reopen: Reopen::NotRequested,
            }
        );
        assert_eq!(
            owner::handoff_line(7, owner::HandoffOutcome::Done(facts)),
            "shellio: передача активации pid 7: второй запуск активен — нет, приложение владельца \
             найдено — нет, активация владельца — не запрашивалась, LaunchServices — не \
             запрашивался"
        );
    }

    #[test]
    fn own_activity_wait_leaves_time_for_launch_services_within_the_handoff() {
        assert!(OWN_ACTIVITY_WAIT < owner::HANDOFF_TIMEOUT);
        assert!(owner::HANDOFF_TIMEOUT - OWN_ACTIVITY_WAIT >= Duration::from_millis(300));
    }

    #[test]
    fn reopen_keeps_the_answer_of_launch_services() {
        assert_eq!(reopen(Some(Reopened::Opened(42))), Reopen::Opened(42));
        assert_eq!(reopen(Some(Reopened::Opened(-1))), Reopen::Failed(None));
        assert_eq!(
            reopen(Some(Reopened::Failed(Some(-10814)))),
            Reopen::Failed(Some(-10814))
        );
        assert_eq!(reopen(Some(Reopened::Failed(None))), Reopen::Failed(None));
        assert_eq!(reopen(Some(Reopened::NoBundle)), Reopen::NotRequested);
        assert_eq!(reopen(None), Reopen::NotRequested);
    }
}
