//! Порт [`Ui`] владельца над `shellio-appkit`: главный поток получает работу только через
//! [`MainThread`], ответы возвращаются каналами, делегат лишь отмечает события. Окно владельца,
//! зеркало и обработчик действий интерфейса живут на главном потоке в [`crate::window_ui`].

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use shellio::launch::WindowMode;
use shellio::owner::{self, Handoff, MirrorSink, Reopen, Ui};
use shellio_appkit::{App, Delegate, MainThread, Reopened, Transfer};
use shellio_control::{Activation, UiGate, UiRefusal, UiReply, UiRequest};
use shellio_core::{CoreHandle, Delivery, MirrorStep};
use tokio::sync::{oneshot, watch};

use crate::window_ui;

/// Сколько второй запуск ждёт собственной активности, прежде чем просить LaunchServices. Своя
/// активность передаётся по PID и верна при любом числе владельцев, LaunchServices выбирает
/// экземпляр бандла сам, поэтому идёт только без неё. Ожидание начинается после готовности
/// приложения, поэтому на ответ LaunchServices остаётся не больше 500 мс срока передачи (1 с от
/// `hello`).
const OWN_ACTIVITY_WAIT: Duration = Duration::from_millis(500);

/// UI владельца на `NSApplication`.
pub struct AppUi {
    main: MainThread,
    ready: watch::Sender<bool>,
    /// Приложение стало активным после начала текущей передачи: [`hand_off`] сбрасывает флаг
    /// перед запросом переднего плана.
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

    /// Шаг передачи на главном потоке через [`awaited`]: шаг, ответа которого к его очереди уже
    /// не ждут, не выполняется.
    async fn step<T: Send + 'static>(
        &self,
        work: impl FnOnce(&App, oneshot::Sender<T>) + Send + 'static,
    ) -> Option<T> {
        let main = self.main;
        awaited(move |job| main.exec(move |app| job(app)), work).await
    }
}

/// Задача очереди главного потока над целью `A`.
type Job<A> = Box<dyn FnOnce(&A) + Send>;

/// Поставить `work` в очередь `enqueue` и дождаться ответа, который `work` отправляет в канал.
/// Срок передачи отбрасывает future вместе с приёмником, поэтому шаг, дошедший до очереди после
/// срока, не выполняется и после отправки `launch` передний план не трогает. `None` — ответа не
/// будет.
async fn awaited<A, T: Send + 'static>(
    enqueue: impl FnOnce(Job<A>),
    work: impl FnOnce(&A, oneshot::Sender<T>) + Send + 'static,
) -> Option<T> {
    let (reply, answer) = oneshot::channel();
    enqueue(Box::new(move |target| {
        if !reply.is_closed() {
            work(target, reply);
        }
    }));
    answer.await.ok()
}

/// Поставить `work` в очередь `enqueue` под отметкой начала `gate` и дождаться результата. Работа
/// выполняется, только если [`UiGate::begin`] ответил `true`: срок сервера, отменивший её в очереди,
/// не даёт ей начаться позже. `None` — результата не будет.
async fn gated<A, T: Send + 'static>(
    enqueue: impl FnOnce(Job<A>),
    gate: UiGate,
    work: impl FnOnce(&A) -> T + Send + 'static,
) -> Option<T> {
    let (reply, answer) = oneshot::channel();
    enqueue(Box::new(move |target| {
        if gate.begin() {
            let _ = reply.send(work(target));
        }
    }));
    answer.await.ok()
}

/// Дождаться, пока флаг станет `true`. Отправитель живёт в [`AppUi`], поэтому канал не
/// закрывается, пока ждут.
async fn latch(flag: &watch::Sender<bool>) {
    let mut receiver = flag.subscribe();
    let _ = receiver.wait_for(|set| *set).await;
}

impl MirrorSink for AppUi {
    fn deliver(&self, delivery: Delivery) -> impl Future<Output = Option<MirrorStep>> + Send {
        self.on_main(move |app| window_ui::deliver(app, delivery))
    }
}

impl Ui for AppUi {
    fn ready(&self) -> impl Future<Output = ()> + Send {
        latch(&self.ready)
    }

    async fn become_owner(&self, window: WindowMode) -> Option<i64> {
        // Окно — только после роли «владелец»: до неё процесс мог оказаться вторым запуском.
        self.on_main(move |app| {
            if !app.become_regular() {
                owner::say_err("shellio: AppKit отказал в политике .regular, активации не будет");
            }
            window_ui::create(app, window)
        })
        .await
    }

    async fn attach_core(&self, core: CoreHandle) {
        let runtime = tokio::runtime::Handle::current();
        self.on_main(move |app| window_ui::attach_core(app, core, runtime))
            .await;
    }

    async fn activate(&self) -> Activation {
        // Правило «Первый передний план», вариант 2: окно `makeKeyAndOrderFront`, кооперативный
        // `activate()`, затем некооперативный запрос. Пересланный `launch` второго запуска показывает
        // окно так же.
        let shown = self.on_main(|app| {
            window_ui::show(app);
            app.activate();
            app.activate_ignoring_others();
        });
        match shown.await {
            Some(()) => Activation::Requested,
            // Главный поток не выполнил вызов: UI уже нет.
            None => Activation::NoUi,
        }
    }

    async fn ui_request(&self, request: UiRequest, gate: UiGate) -> Result<UiReply, UiRefusal> {
        let main = self.main;
        gated(
            move |job| main.exec(move |app| job(app)),
            gate,
            move |app| window_ui::request(app, request),
        )
        .await
        // Главный поток не выполнил работу: UI уже нет.
        .unwrap_or(Err(UiRefusal::NoUi))
    }

    async fn hand_off_activation(&self, pid: u32) -> Handoff {
        // Передача идёт от запущенного приложения: до `applicationDidFinishLaunching` его ещё нет.
        latch(&self.ready).await;
        // Окно закрывается и тогда, когда срок передачи в `claim` отбросит этот future.
        let _window = ServiceWindow(self.main);
        hand_off(self, &self.became_active, pid).await
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

/// Шаги передачи активации на главном потоке. Порядок и ожидания — в [`hand_off`].
trait Steps: Sync {
    /// Показать служебное окно, попросить передний план себе и сказать, активно ли приложение.
    /// `None` — главный поток не выполнил шаг: UI уже нет.
    fn claim_front(&self) -> impl Future<Output = Option<bool>> + Send;

    /// Попросить LaunchServices открыть бандл владельца `pid` с активацией и дождаться ответа.
    fn reopen(&self, pid: u32) -> impl Future<Output = Reopen> + Send;

    /// Передать активацию владельцу `pid`. `None` — главный поток не выполнил шаг.
    fn transfer(&self, pid: u32) -> impl Future<Output = Option<Transfer>> + Send;

    /// Строка в stderr второго запуска.
    fn say(&self, line: String);
}

impl Steps for AppUi {
    async fn claim_front(&self) -> Option<bool> {
        self.step(|app, reply| {
            app.show_service_window();
            app.request_activation();
            let _ = reply.send(app.is_active());
        })
        .await
    }

    async fn reopen(&self, pid: u32) -> Reopen {
        // LaunchServices выбирает экземпляр бандла сам и может вывести другого владельца, поэтому
        // запрос, не дождавшийся очереди до срока, не уходит.
        let reopened = self
            .step(move |app, reply| {
                app.reopen_bundle_of(pid, move |reopened| {
                    let _ = reply.send(reopened);
                });
            })
            .await;
        reopen(reopened)
    }

    async fn transfer(&self, pid: u32) -> Option<Transfer> {
        // Без шага, а выполняется и после срока: в очередь он попадает после запроса переднего
        // плана себе, и пропуск оставил бы второй запуск на переднем плане без уступки. Обращён он
        // только к владельцу по PID, поэтому поздний шаг может вывести вперёд лишь его.
        self.on_main(move |app| app.hand_off_activation_to(pid))
            .await
    }

    fn say(&self, line: String) {
        owner::say_err(line);
    }
}

/// Передать активацию владельцу `pid`: передний план себе, собственная активность не дольше
/// [`OWN_ACTIVITY_WAIT`], без неё — LaunchServices, затем передача. `became_active` ставит делегат.
async fn hand_off(steps: &impl Steps, became_active: &watch::Sender<bool>, pid: u32) -> Handoff {
    // Активность прежней попытки `claim` уже уступлена. Полученную до запроса покажет `claim_front`.
    became_active.send_replace(false);
    let Some(active) = steps.claim_front().await else {
        return handoff(None, Reopen::NotRequested);
    };
    let own = active
        || tokio::time::timeout(OWN_ACTIVITY_WAIT, latch(became_active))
            .await
            .is_ok();
    let reopen = if own {
        Reopen::NotRequested
    } else {
        // Строка до ожидания ответа: срок передачи может отбросить его, а ушедший запрос
        // LaunchServices не отменяется.
        steps.say(owner::reopen_start_line(pid));
        steps.reopen(pid).await
    };
    // Передать можно только свою активность. Не пришла и LaunchServices не открыл владельца —
    // ожидание обрывает срок передачи в `claim`, и `launch` уходит без неё.
    if !own && reopen != Reopen::Opened(pid) {
        steps.say(owner::reopen_line(pid, reopen));
        latch(became_active).await;
    }
    handoff(steps.transfer(pid).await, reopen)
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

    fn reopen(&self, app: &App, _has_visible_windows: bool) {
        window_ui::reopen(app);
    }

    fn terminate_requested(&self, _app: &App) {
        self.ui.terminate.send_replace(true);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::time::Instant;

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
    fn own_activity_wait_is_shorter_than_handoff_by_300ms() {
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

    /// Шаг, который ждут, выполняется в очереди и возвращает ответ.
    #[tokio::test]
    async fn awaited_step_answers_from_the_queue() {
        let answer = awaited(
            |job: Job<u32>| job(&5),
            |target, reply| {
                let _ = reply.send(*target + 1);
            },
        )
        .await;
        assert_eq!(answer, Some(6));
    }

    /// Срок передачи истёк, пока шаг стоял в очереди главного потока: дошедший до очереди шаг не
    /// выполняется.
    #[tokio::test(start_paused = true)]
    async fn step_still_queued_at_the_handoff_deadline_does_not_run() {
        let queue: Mutex<Vec<Job<()>>> = Mutex::new(Vec::new());
        let ran = Arc::new(Mutex::new(false));
        let work_ran = Arc::clone(&ran);
        let step = awaited(
            |job| queue.lock().unwrap().push(job),
            move |(), reply| {
                *work_ran.lock().unwrap() = true;
                let _ = reply.send(());
            },
        );
        assert!(
            tokio::time::timeout(owner::HANDOFF_TIMEOUT, step)
                .await
                .is_err()
        );
        let jobs = std::mem::take(&mut *queue.lock().unwrap());
        assert_eq!(jobs.len(), 1);
        for job in jobs {
            job(&());
        }
        assert!(!*ran.lock().unwrap());
    }

    /// Работа, отменённая сроком сервера, пока стояла в очереди главного потока, не выполняется.
    #[tokio::test]
    async fn gated_work_cancelled_in_the_queue_does_not_run() {
        let queue: Mutex<Vec<Job<()>>> = Mutex::new(Vec::new());
        let ran = Arc::new(Mutex::new(false));
        let work_ran = Arc::clone(&ran);
        let gate = UiGate::new();
        let mut pending = std::pin::pin!(gated(
            |job| queue.lock().unwrap().push(job),
            gate.clone(),
            move |()| {
                *work_ran.lock().unwrap() = true;
            },
        ));
        // Один опрос ставит работу в очередь и оставляет ожидание ответа.
        tokio::select! {
            biased;
            answer = &mut pending => panic!("ответ до выполнения очереди: {answer:?}"),
            () = std::future::ready(()) => {}
        }
        assert!(gate.cancel(), "работа ещё в очереди");
        let jobs = std::mem::take(&mut *queue.lock().unwrap());
        assert_eq!(jobs.len(), 1);
        for job in jobs {
            job(&());
        }
        assert_eq!(pending.await, None);
        assert!(!*ran.lock().unwrap());
    }

    /// Начатая работа доходит до ответа, и срок её уже не отменяет.
    #[tokio::test]
    async fn gated_work_started_answers_and_cannot_be_cancelled() {
        let gate = UiGate::new();
        let answer = gated(|job: Job<u32>| job(&41), gate.clone(), |target| *target + 1).await;
        assert_eq!(answer, Some(42));
        assert!(!gate.cancel());
    }

    const PID: u32 = 7;

    /// Когда поддельное приложение получает собственную активность событием делегата.
    #[derive(Clone, Copy)]
    enum Event {
        Never,
        AfterClaim,
        AfterReopen,
    }

    /// Шаги передачи без AppKit: ответы заданы заранее, вызовы и строки stderr записываются по
    /// порядку.
    struct FakeSteps {
        became_active: watch::Sender<bool>,
        /// Ответ `claim_front`: активно ли приложение, `None` — UI нет.
        front: Option<bool>,
        event: Event,
        /// Ответ LaunchServices, `None` — ответа нет.
        reopen: Option<Reopen>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeSteps {
        fn new(front: Option<bool>, event: Event, reopen: Option<Reopen>) -> Self {
            Self {
                became_active: watch::Sender::new(false),
                front,
                event,
                reopen,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        /// Передача под сроком 60 с виртуального времени; `None` — не завершилась.
        async fn run(&self) -> Option<Handoff> {
            tokio::time::timeout(
                Duration::from_secs(60),
                hand_off(self, &self.became_active, PID),
            )
            .await
            .ok()
        }
    }

    impl Steps for FakeSteps {
        fn claim_front(&self) -> impl Future<Output = Option<bool>> + Send {
            self.record("claim_front".to_owned());
            if matches!(self.event, Event::AfterClaim) {
                self.became_active.send_replace(true);
            }
            std::future::ready(self.front)
        }

        fn reopen(&self, pid: u32) -> impl Future<Output = Reopen> + Send {
            self.record(format!("reopen {pid}"));
            if matches!(self.event, Event::AfterReopen) {
                self.became_active.send_replace(true);
            }
            let answer = self.reopen;
            async move {
                match answer {
                    Some(answer) => answer,
                    None => std::future::pending().await,
                }
            }
        }

        fn transfer(&self, pid: u32) -> impl Future<Output = Option<Transfer>> + Send {
            self.record(format!("transfer {pid}"));
            std::future::ready(Some(Transfer {
                self_active: true,
                owner_found: true,
                activated: Some(true),
            }))
        }

        fn say(&self, line: String) {
            self.record(line);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn active_launch_hands_off_without_launch_services() {
        let steps = FakeSteps::new(Some(true), Event::Never, None);
        let facts = steps.run().await.unwrap();
        assert_eq!(facts.reopen, Reopen::NotRequested);
        assert_eq!(steps.calls(), ["claim_front", "transfer 7"]);
    }

    #[tokio::test(start_paused = true)]
    async fn activity_after_the_request_skips_launch_services() {
        let steps = FakeSteps::new(Some(false), Event::AfterClaim, None);
        let started = Instant::now();
        let facts = steps.run().await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(facts.reopen, Reopen::NotRequested);
        assert_eq!(steps.calls(), ["claim_front", "transfer 7"]);
    }

    /// Флаг активности прежней попытки `claim` не считается: та активность уже уступлена.
    /// LaunchServices, открывший владельца, даёт передачу без ожидания.
    #[tokio::test(start_paused = true)]
    async fn activity_of_an_earlier_attempt_does_not_count() {
        let steps = FakeSteps::new(Some(false), Event::Never, Some(Reopen::Opened(PID)));
        steps.became_active.send_replace(true);
        let started = Instant::now();
        let facts = steps.run().await.unwrap();
        assert_eq!(started.elapsed(), OWN_ACTIVITY_WAIT);
        assert_eq!(facts.reopen, Reopen::Opened(PID));
        let start = owner::reopen_start_line(PID);
        assert_eq!(
            steps.calls(),
            ["claim_front", start.as_str(), "reopen 7", "transfer 7"]
        );
    }

    /// Строка о LaunchServices печатается до ответа: срок передачи, отбросивший ожидание, её не
    /// скрывает.
    #[tokio::test(start_paused = true)]
    async fn launch_services_request_is_reported_before_its_answer() {
        let steps = FakeSteps::new(Some(false), Event::Never, None);
        assert_eq!(steps.run().await, None);
        let start = owner::reopen_start_line(PID);
        assert_eq!(steps.calls(), ["claim_front", start.as_str(), "reopen 7"]);
    }

    /// LaunchServices открыл не владельца или не открыл ничего: строка ответа, затем ожидание
    /// собственной активности без срока внутри передачи.
    #[tokio::test(start_paused = true)]
    async fn launch_services_answer_other_than_owner_waits_for_own_activity() {
        let start = owner::reopen_start_line(PID);
        for answer in [
            Reopen::Opened(9),
            Reopen::Failed(Some(-600)),
            Reopen::NotRequested,
        ] {
            let line = owner::reopen_line(PID, answer);
            let steps = FakeSteps::new(Some(false), Event::Never, Some(answer));
            assert_eq!(steps.run().await, None, "{answer:?}");
            assert_eq!(
                steps.calls(),
                ["claim_front", start.as_str(), "reopen 7", line.as_str()]
            );

            let steps = FakeSteps::new(Some(false), Event::AfterReopen, Some(answer));
            assert_eq!(steps.run().await.unwrap().reopen, answer);
            assert_eq!(
                steps.calls(),
                [
                    "claim_front",
                    start.as_str(),
                    "reopen 7",
                    line.as_str(),
                    "transfer 7"
                ]
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn handoff_without_ui_stops_at_the_request() {
        let steps = FakeSteps::new(None, Event::Never, None);
        assert_eq!(steps.run().await, Some(handoff(None, Reopen::NotRequested)));
        assert_eq!(steps.calls(), ["claim_front"]);
    }
}
