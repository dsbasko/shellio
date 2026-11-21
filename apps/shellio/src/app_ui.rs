//! Порт [`Ui`] владельца над `shellio-appkit`: главный поток получает работу только через
//! [`MainThread`], ответы возвращаются каналами, делегат лишь отмечает события.

use std::future::Future;
use std::sync::Arc;

use shellio::owner::{self, Ui};
use shellio_appkit::{App, Delegate, MainThread};
use shellio_control::Activation;
use tokio::sync::{oneshot, watch};

/// UI владельца на `NSApplication`.
pub struct AppUi {
    main: MainThread,
    ready: watch::Sender<bool>,
    terminate: watch::Sender<bool>,
}

impl AppUi {
    pub fn new() -> Self {
        Self {
            main: MainThread::new(),
            ready: watch::Sender::new(false),
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

    async fn yield_activation(&self, pid: u32) {
        // Уступка идёт от запущенного приложения: до `applicationDidFinishLaunching` его ещё нет.
        latch(&self.ready).await;
        if self.on_main(move |app| app.yield_activation_to(pid)).await == Some(false) {
            owner::say_err(format_args!(
                "shellio: приложение владельца pid {pid} не найдено, активация не уступлена"
            ));
        }
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

/// Делегат приложения: отмечает готовность и запрос завершения и сразу возвращается.
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

    fn terminate_requested(&self, _app: &App) {
        self.ui.terminate.send_replace(true);
    }
}
