//! Сторож сценария: отдельный поток, который по истечении срока пишет след последних событий в
//! `--out` и завершает процесс кодом ошибки исполнения, даже если главный поток не отвечает.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::EXIT_EXECUTION;
use super::events::SharedTrace;

use crate::results::WATCHDOG_FILE as TRACE_FILE;

/// Взведённый сторож. Снимается при уничтожении.
pub struct Watchdog {
    disarm: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

pub fn arm(
    scenario: &'static str,
    deadline: Duration,
    out: PathBuf,
    trace: SharedTrace,
) -> std::io::Result<Watchdog> {
    arm_with(scenario, deadline, out, trace, |code| {
        // SAFETY: `_exit` завершает процесс без обработчиков `atexit`, которые могли бы ждать
        // зависший главный поток.
        unsafe { libc::_exit(code) }
    })
}

fn arm_with(
    scenario: &'static str,
    deadline: Duration,
    out: PathBuf,
    trace: SharedTrace,
    exit: impl FnOnce(i32) + Send + 'static,
) -> std::io::Result<Watchdog> {
    let (disarm, signal) = mpsc::channel::<()>();
    let thread = thread::Builder::new()
        .name("probe-watchdog".to_owned())
        .spawn(move || {
            // Снятие сторожа закрывает канал; срабатывает только истечение срока.
            if signal.recv_timeout(deadline) != Err(RecvTimeoutError::Timeout) {
                return;
            }
            let lines = trace
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .lines();
            let text = render(scenario, deadline, &lines);
            let written = write_trace(&out, &text);
            let message = match written {
                Ok(path) => format!(
                    "сторож: сценарий {scenario} не закончился за {} с, след в {}\n",
                    deadline.as_secs_f64(),
                    path.display()
                ),
                Err(err) => format!(
                    "сторож: сценарий {scenario} не закончился за {} с, след не записан: {err}\n",
                    deadline.as_secs_f64()
                ),
            };
            write_stderr(&message);
            exit(i32::from(EXIT_EXECUTION));
        })?;
    Ok(Watchdog {
        disarm: Some(disarm),
        thread: Some(thread),
    })
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        drop(self.disarm.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn render(scenario: &str, deadline: Duration, lines: &[String]) -> String {
    let mut text = format!(
        "scenario={scenario}\nexecution=error\nreason=сторож: срок {} с истёк\ntrace:\n",
        deadline.as_secs_f64()
    );
    for line in lines {
        text.push_str("  ");
        text.push_str(line);
        text.push('\n');
    }
    text
}

fn write_trace(out: &Path, text: &str) -> std::io::Result<PathBuf> {
    let path = out.join(TRACE_FILE);
    fs::write(&path, text)?;
    Ok(path)
}

/// Пишет в stderr без блокировки потока Rust, которую мог занять главный поток.
fn write_stderr(message: &str) {
    let bytes = message.as_bytes();
    // SAFETY: буфер жив на время вызова, дескриптор 2 открыт у процесса.
    let _ = unsafe { libc::write(2, bytes.as_ptr().cast(), bytes.len()) };
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::scenario::events::{TraceRing, push_trace};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "probe-appkit-shell-watchdog-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn render_lists_trace() {
        let text = render(
            "native-menu",
            Duration::from_secs(2),
            &["a".into(), "b".into()],
        );
        assert!(text.starts_with("scenario=native-menu\nexecution=error\n"));
        assert!(text.contains("срок 2 с"));
        assert!(text.ends_with("trace:\n  a\n  b\n"));
    }

    #[test]
    fn disarm_stops_thread_without_exit() {
        let dir = temp_dir("disarm");
        let trace: SharedTrace = Arc::new(Mutex::new(TraceRing::default()));
        let (exited, seen) = mpsc::channel();
        let watchdog = arm_with(
            "launch",
            Duration::from_secs(600),
            dir.clone(),
            trace,
            move |code| {
                let _ = exited.send(code);
            },
        )
        .unwrap();
        drop(watchdog);
        assert!(seen.try_recv().is_err());
        assert!(!dir.join(TRACE_FILE).exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expiry_writes_trace_and_exits() {
        let dir = temp_dir("expiry");
        let trace: SharedTrace = Arc::new(Mutex::new(TraceRing::default()));
        push_trace(&trace, "post #1 keyDown Down".into());
        let (exited, seen) = mpsc::channel();
        let watchdog = arm_with(
            "launch",
            Duration::from_millis(1),
            dir.clone(),
            trace,
            move |code| {
                let _ = exited.send(code);
            },
        )
        .unwrap();
        assert_eq!(
            seen.recv_timeout(Duration::from_secs(30)),
            Ok(i32::from(EXIT_EXECUTION))
        );
        drop(watchdog);
        let text = fs::read_to_string(dir.join(TRACE_FILE)).unwrap();
        assert!(text.contains("post #1 keyDown Down"), "{text}");
        fs::remove_dir_all(dir).unwrap();
    }
}
