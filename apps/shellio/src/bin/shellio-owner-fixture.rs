//! Тестовый владелец без AppKit: тот же `shellio::owner`, UI без окна и цикла. Главный поток
//! ждёт кода выхода от задачи runtime. Активация пишет строку в stdout. Завершается только
//! сигналами. В бандл не входит.
//!
//! Передача активации вторым запуском сразу возвращает [`NO_UI_HANDOFF`]. С
//! `SHELLIO_FIXTURE_HANDOFF=hang` она не завершается никогда: тест проверяет её срок.

use std::future::{Future, pending, ready};
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};

use shellio::owner::{self, Handoff, Reopen, Ui};
use shellio_control::Activation;
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
}

impl Ui for NoUi {
    fn ready(&self) -> impl Future<Output = ()> + Send {
        ready(())
    }

    fn become_owner(&self) -> impl Future<Output = ()> + Send {
        ready(())
    }

    fn activate(&self) -> impl Future<Output = Activation> + Send {
        owner::say(ACTIVATION_LINE);
        ready(Activation::NoUi)
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

fn main() {
    let (exit, code) = channel();
    let profile = Profile::from_process_env();
    let hang_handoff = std::env::var_os(HANDOFF_VAR).is_some_and(|value| value == "hang");
    let args = std::env::args_os().skip(1).collect();
    let ui = NoUi { exit, hang_handoff };
    let _runtime = match owner::start(Arc::new(ui), args, profile) {
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
