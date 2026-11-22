//! Гонка режима окна: настоящий `owner::start` с подделкой порта `Ui` в процессе теста.
//!
//! Режим окна задают аргументы процесса, взявшего блокировку, а не первый обработанный `launch`.
//! Подделка держит активацию собственного `launch` владельца на барьере, а второй запуск идёт
//! через `claim` с пересылкой `--gallery`, как у настоящего второго запуска. Порядки: второй запуск,
//! пока собственный `launch` стоит на барьере, и второй запуск после него. Режим читается
//! запросом `ui.window` и записью вызовов `become_owner`. Синхронизация — каналы, без ожидания по
//! времени: сроки ниже только ограничивают зависание.

// allow-unwrap-in-tests не распространяется на вспомогательные функции интеграционного теста.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::future::{Future, ready};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shellio::launch::{self, Mode, Run, WindowMode};
use shellio::owner::{
    self, ActivationHandoff, Claim, Handoff, HandoffOutcome, MirrorSink, Reopen,
    SECOND_LAUNCH_TIMEOUT, Second, Ui,
};
use shellio_control::{
    Activation, Address, Axis, AxisSource, Client, ColorSpaceView, Contrast, Launch, Lightness,
    Motion, OkValue, Rect, RefuseReason, RequestBody, Response, TokioClock, UiGate, UiRefusal,
    UiReply, UiRequest, WindowView,
};
use shellio_core::{AppearanceChoice, Delivery, Mirror, MirrorStep};
use shellio_store::Profile;
use tokio::sync::{oneshot, watch};

/// Срок ожиданий теста: ограничивает зависание, а не синхронизирует.
const HANG_GUARD: Duration = Duration::from_secs(30);

/// Барьер первой активации: сигнал входа и разрешение продолжить.
struct Barrier {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

/// Порт UI владельца без AppKit.
struct RaceUi {
    modes: Mutex<Vec<WindowMode>>,
    barrier: Mutex<Option<Barrier>>,
    /// Активация собственного `launch` вернулась.
    own_done: Mutex<Option<oneshot::Sender<()>>>,
    terminate: watch::Sender<bool>,
    exit: Mutex<Option<oneshot::Sender<u8>>>,
    mirror: Mutex<Option<Mirror>>,
}

impl MirrorSink for RaceUi {
    fn deliver(&self, delivery: Delivery) -> impl Future<Output = Option<MirrorStep>> + Send {
        let mut mirror = self.mirror.lock().unwrap();
        ready(Some(Mirror::deliver_to(&mut mirror, delivery)))
    }
}

impl Ui for RaceUi {
    fn ready(&self) -> impl Future<Output = ()> + Send {
        ready(())
    }

    fn become_owner(&self, window: WindowMode) -> impl Future<Output = Option<i64>> + Send {
        self.modes.lock().unwrap().push(window);
        ready(Some(1))
    }

    fn activate(&self) -> impl Future<Output = Activation> + Send {
        let barrier = self.barrier.lock().unwrap().take();
        let done = if barrier.is_some() {
            self.own_done.lock().unwrap().take()
        } else {
            None
        };
        async move {
            if let Some(Barrier { entered, release }) = barrier {
                let _ = entered.send(());
                let _ = release.await;
            }
            if let Some(done) = done {
                let _ = done.send(());
            }
            Activation::Requested
        }
    }

    fn ui_request(
        &self,
        request: UiRequest,
        gate: UiGate,
    ) -> impl Future<Output = Result<UiReply, UiRefusal>> + Send {
        let mode = self.modes.lock().unwrap().first().copied();
        let reply = match (request, mode) {
            (UiRequest::Window, Some(mode)) if gate.begin() => {
                Ok(UiReply::Window(Box::new(window_view(mode))))
            }
            _ => Err(UiRefusal::NoUi),
        };
        ready(reply)
    }

    fn hand_off_activation(&self, _pid: u32) -> impl Future<Output = Handoff> + Send {
        ready(NO_HANDOFF)
    }

    fn terminate_requested(&self) -> impl Future<Output = ()> + Send {
        let mut terminate = self.terminate.subscribe();
        async move {
            let _ = terminate.wait_for(|requested| *requested).await;
        }
    }

    fn finish_terminate(&self) {
        self.exit(0);
    }

    fn exit(&self, code: u8) {
        if let Some(exit) = self.exit.lock().unwrap().take() {
            let _ = exit.send(code);
        }
    }
}

const NO_HANDOFF: Handoff = Handoff {
    self_active: false,
    owner_found: false,
    activated: None,
    reopen: Reopen::NotRequested,
};

fn window_view(mode: WindowMode) -> WindowView {
    let zero = Rect {
        x: 0.0,
        y: 0.0,
        width: 0.0,
        height: 0.0,
    };
    WindowView {
        pid: std::process::id(),
        mode: match mode {
            WindowMode::Normal => shellio_control::WindowMode::Normal,
            WindowMode::Gallery => shellio_control::WindowMode::Gallery,
        },
        window_number: 1,
        frame: zero,
        backing_scale_factor: 1.0,
        visible: true,
        key: true,
        color_space: ColorSpaceView {
            name: None,
            srgb: true,
        },
        title: String::new(),
        appearance_choice: AppearanceChoice::System,
        lightness: Axis {
            value: Lightness::Light,
            source: AxisSource::System,
        },
        contrast: Axis {
            value: Contrast::Normal,
            source: AxisSource::System,
        },
        motion: Axis {
            value: Motion::Normal,
            source: AxisSource::System,
        },
        screen: None,
        maximum_frames_per_second: None,
        screens: Vec::new(),
        mirror_revision: None,
        generation: 0,
    }
}

/// Передача активации вторым запуском без UI.
struct NoHandoff;

impl ActivationHandoff for NoHandoff {
    fn hand_off(&self, _pid: u32) -> impl Future<Output = Handoff> + Send {
        ready(NO_HANDOFF)
    }

    fn outcome(&self, _pid: u32, _outcome: HandoffOutcome) {}
}

fn run_of(args: &[&str]) -> Run {
    match launch::mode(args.iter().map(OsString::from).collect()) {
        Mode::Run(run) => run,
        other => panic!("{args:?}: {other:?}"),
    }
}

/// Когда идёт второй запуск относительно собственного `launch` владельца.
#[derive(Clone, Copy, Debug)]
enum Order {
    /// Собственный `launch` стоит на барьере активации.
    WhileOwnHeld,
    /// Активация собственного `launch` вернулась.
    AfterOwn,
}

/// Владелец с аргументами `owner_args`, второй запуск с `second_args` в порядке `order`. Возвращает
/// режим из `ui.window`, все вызовы `become_owner` и отказы квитанции второго запуска.
fn race(
    owner_args: &[&str],
    second_args: &[&str],
    order: Order,
) -> (
    shellio_control::WindowMode,
    Vec<WindowMode>,
    Vec<RefuseReason>,
) {
    let dir = tempfile::Builder::new()
        .prefix("shellio-race-")
        .tempdir_in("/tmp")
        .unwrap();
    let root: PathBuf = dir.path().canonicalize().unwrap();
    let profile = Profile::isolated(root.join("state"));
    let (entered_tx, entered) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let (own_done_tx, own_done) = oneshot::channel();
    let (exit_tx, exit) = oneshot::channel();
    let ui = Arc::new(RaceUi {
        modes: Mutex::new(Vec::new()),
        barrier: Mutex::new(Some(Barrier {
            entered: entered_tx,
            release: release_rx,
        })),
        own_done: Mutex::new(Some(own_done_tx)),
        terminate: watch::Sender::new(false),
        exit: Mutex::new(Some(exit_tx)),
        mirror: Mutex::new(None),
    });
    let runtime = owner::start(Arc::clone(&ui), run_of(owner_args), Ok(profile.clone())).unwrap();

    let refused = runtime.block_on(async {
        guard(entered).await.unwrap();
        match order {
            Order::AfterOwn => {
                release.send(()).unwrap();
                guard(own_done).await.unwrap();
                second(&profile, &root, second_args).await
            }
            Order::WhileOwnHeld => {
                let refused = second(&profile, &root, second_args).await;
                release.send(()).unwrap();
                guard(own_done).await.unwrap();
                refused
            }
        }
    });
    let mode = runtime.block_on(window_mode(&profile));
    ui.terminate.send_replace(true);
    assert_eq!(runtime.block_on(guard(exit)).unwrap(), 0);
    let modes = ui.modes.lock().unwrap().clone();
    drop(runtime);
    drop(dir);
    (mode, modes, refused)
}

async fn guard<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(HANG_GUARD, future)
        .await
        .expect("ожидание теста зависло")
}

/// Второй запуск через `claim`: пересылка флага, как у настоящего второго запуска.
async fn second(profile: &Profile, cwd: &Path, args: &[&str]) -> Vec<RefuseReason> {
    let run = run_of(args);
    let own = Launch {
        args: run
            .args
            .iter()
            .map(|arg| arg.to_str().unwrap().to_owned())
            .collect(),
        cwd: cwd.to_owned(),
    };
    let forwarded = run.forwarded(&own);
    let claim = guard(owner::claim(
        profile,
        TokioClock,
        SECOND_LAUNCH_TIMEOUT,
        &forwarded,
        &NoHandoff,
    ))
    .await
    .unwrap();
    match claim {
        Claim::Second(Second::Delivered { receipt, .. }) => {
            assert_eq!(receipt.activation, Activation::Requested);
            receipt
                .refused
                .iter()
                .map(|refused| refused.reason)
                .collect()
        }
        Claim::Second(other) => panic!("второй запуск: {other:?}"),
        Claim::Owner(_) => panic!("второй запуск стал владельцем"),
    }
}

async fn window_mode(profile: &Profile) -> shellio_control::WindowMode {
    let client = Client::new(
        Address::Profile(profile.clone()),
        TokioClock,
        "race-test",
        HANG_GUARD,
    );
    match client.call(RequestBody::Ui(UiRequest::Window)).await {
        Ok(Response::Ok {
            value: OkValue::Ui(UiReply::Window(window)),
            ..
        }) => window.mode,
        other => panic!("ui.window: {other:?}"),
    }
}

/// Обычный владелец и второй запуск с `--gallery`: отказ в квитанции второго, режим владельца
/// обычный в обоих порядках.
#[test]
fn gallery_second_launch_does_not_change_normal_owner() {
    for order in [Order::WhileOwnHeld, Order::AfterOwn] {
        let (mode, modes, refused) = race(&[], &["--gallery"], order);
        assert_eq!(mode, shellio_control::WindowMode::Normal, "{order:?}");
        assert_eq!(modes, [WindowMode::Normal], "{order:?}");
        assert_eq!(refused, [RefuseReason::GalleryNotAtStart], "{order:?}");
    }
}

/// Владелец с `--gallery` и обычный второй запуск: режим остаётся галереей, отказов нет.
#[test]
fn plain_second_launch_keeps_gallery_owner() {
    for order in [Order::WhileOwnHeld, Order::AfterOwn] {
        let (mode, modes, refused) = race(&["--gallery"], &[], order);
        assert_eq!(mode, shellio_control::WindowMode::Gallery, "{order:?}");
        assert_eq!(modes, [WindowMode::Gallery], "{order:?}");
        assert!(refused.is_empty(), "{order:?}: {refused:?}");
    }
}
