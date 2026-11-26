//! Тестовый владелец без AppKit: тот же `shellio::owner`, UI без окна и цикла. Главный поток
//! ждёт кода выхода от задачи runtime. Активация пишет строку в stdout. Завершается только
//! сигналами. В бандл не входит.
//!
//! Передача активации вторым запуском сразу возвращает [`NO_UI_HANDOFF`]. С
//! `SHELLIO_FIXTURE_HANDOFF=hang` она не завершается никогда: тест проверяет её срок.
//!
//! Аргументы разбираются как у `shellio`: `--gallery` выбирает режим владельца и требует явного
//! профиля, окна у фикстуры нет. Зеркало состояния ведётся без UI, запросы `ui` получают
//! `no_ui`.

use std::future::{Future, pending, ready};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

use shellio::keymap_service::{KeymapPort, KeymapSnapshot};
use shellio::launch::{self, Mode, WindowMode};
use shellio::owner::{self, Handoff, MirrorSink, Reopen, Ui};
use shellio_control::{Activation, Exit, UiGate, UiRefusal, UiReply, UiRequest};
use shellio_core::{CoreHandle, Delivery, Mirror, MirrorStep};
use shellio_store::Profile;

/// Строка активации в stdout владельца.
const ACTIVATION_LINE: &str = "shellio-owner-fixture: активация без UI";

/// Переменная, задерживающая передачу активации.
const HANDOFF_VAR: &str = "SHELLIO_FIXTURE_HANDOFF";

/// Факты передачи без UI: активным быть нечему, приложения владельца нет.
const NO_UI_HANDOFF: Handoff = Handoff {
    self_active: false,
    owner_found: false,
    activated: None,
    reopen: Reopen::NotRequested,
};

struct NoUi {
    exit: Sender<u8>,
    /// Передача активации не завершается.
    hang_handoff: bool,
    mirror: Mutex<Option<Mirror>>,
}

impl MirrorSink for NoUi {
    fn deliver(&self, delivery: Delivery) -> impl Future<Output = Option<MirrorStep>> + Send {
        let mut mirror = self
            .mirror
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        ready(Some(Mirror::deliver_to(&mut mirror, delivery)))
    }
}

impl Ui for NoUi {
    fn ready(&self) -> impl Future<Output = ()> + Send {
        ready(())
    }

    fn become_owner(&self, _window: WindowMode) -> impl Future<Output = Option<i64>> + Send {
        ready(None)
    }

    fn attach_core(&self, _core: CoreHandle) -> impl Future<Output = ()> + Send {
        ready(())
    }

    fn keymap(&self, _snapshot: Arc<KeymapSnapshot>) -> impl Future<Output = ()> + Send {
        ready(())
    }

    fn keymap_port(&self, _port: KeymapPort) -> impl Future<Output = ()> + Send {
        ready(())
    }

    fn activate(&self) -> impl Future<Output = Activation> + Send {
        owner::say(ACTIVATION_LINE);
        ready(Activation::NoUi)
    }

    fn ui_request(
        &self,
        _request: UiRequest,
        _gate: UiGate,
    ) -> impl Future<Output = Result<UiReply, UiRefusal>> + Send {
        ready(Err(UiRefusal::NoUi))
    }

    fn hand_off_activation(&self, _pid: u32) -> impl Future<Output = Handoff> + Send {
        let hang = self.hang_handoff;
        async move {
            if hang {
                pending::<()>().await;
            }
            NO_UI_HANDOFF
        }
    }

    fn terminate_requested(&self) -> impl Future<Output = ()> + Send {
        pending()
    }

    fn finish_terminate(&self) {
        self.exit(0);
    }

    fn exit(&self, code: u8) {
        // Приёмник ждёт в главном потоке до выхода процесса.
        let _ = self.exit.send(code);
    }
}

/// Ошибка использования: сообщение и код 2.
fn usage(message: impl std::fmt::Display) -> ! {
    owner::say_err(format_args!("shellio-owner-fixture: {message}"));
    owner::exit_process(Exit::Usage.code());
}

fn main() {
    let run = match launch::mode(std::env::args_os().skip(1).collect()) {
        Mode::Run(run) => run,
        Mode::SelfCheck => usage("самопроверки у фикстуры нет"),
        Mode::Usage(message) => usage(message),
    };
    let profile = Profile::from_process_env();
    if let Some(refusal) = launch::gallery_refusal(run.window, &profile) {
        usage(refusal);
    }
    let (exit, code) = channel();
    let hang_handoff = std::env::var_os(HANDOFF_VAR).is_some_and(|value| value == "hang");
    let ui = NoUi {
        exit,
        hang_handoff,
        mirror: Mutex::new(None),
    };
    let _runtime = match owner::start(Arc::new(ui), run, profile) {
        Ok(runtime) => runtime,
        Err(error) => {
            owner::say_err(format_args!(
                "shellio-owner-fixture: runtime не запущен: {error}"
            ));
            owner::exit_process(1);
        }
    };
    // Отправитель живёт в задаче runtime до её конца; закрытый канал без кода — отказ.
    owner::exit_process(code.recv().unwrap_or(1));
}
