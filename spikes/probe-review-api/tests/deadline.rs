//! `deadline.pl` — срок команд docker в `run.sh` и `gitlab/up.sh`: команда, глухая к SIGALRM (как
//! рантайм Go), обрывается по сроку с кодом 124; без истечения срока код и stdout команды
//! проходят без изменений.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

type TestResult = Result<(), String>;

const EXPIRED: i32 = 124;

fn helper() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("deadline.pl")
}

fn run(secs: &str, command: &[&str]) -> Result<(Output, Duration), String> {
    let started = Instant::now();
    let output = Command::new("/usr/bin/perl")
        .arg(helper())
        .arg(secs)
        .args(command)
        .output()
        .map_err(|e| format!("deadline.pl не запускается: {e}"))?;
    Ok((output, started.elapsed()))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn command_deaf_to_alarm_is_stopped_by_term() -> TestResult {
    let (output, elapsed) = run("1", &["/bin/bash", "-c", "trap '' ALRM; exec sleep 30"])?;
    assert_eq!(output.status.code(), Some(EXPIRED), "{}", stderr(&output));
    assert!(
        elapsed < Duration::from_secs(20),
        "срок не сработал: {elapsed:?}"
    );
    Ok(())
}

#[test]
fn command_deaf_to_term_is_killed() -> TestResult {
    let (output, elapsed) = run(
        "1",
        &["/bin/bash", "-c", "trap '' ALRM TERM; exec sleep 30"],
    )?;
    assert_eq!(output.status.code(), Some(EXPIRED), "{}", stderr(&output));
    assert!(
        elapsed < Duration::from_secs(20),
        "SIGKILL не сработал: {elapsed:?}"
    );
    Ok(())
}

/// Команда, вышедшая по SIGTERM с кодом 0, всё равно даёт код истёкшего срока: вызывающий не
/// принимает оборванную команду за успешную.
#[test]
fn clean_exit_after_term_is_still_expired() -> TestResult {
    let (output, _) = run(
        "1",
        &[
            "/bin/bash",
            "-c",
            "trap '' ALRM; sleep 30 >/dev/null 2>&1 & s=$!; trap 'kill $s; exit 0' TERM; wait $s",
        ],
    )?;
    assert_eq!(output.status.code(), Some(EXPIRED), "{}", stderr(&output));
    Ok(())
}

#[test]
fn code_and_stdout_pass_through_within_deadline() -> TestResult {
    let (output, _) = run("30", &["/bin/sh", "-c", "echo out; exit 3"])?;
    assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "out\n");
    let (output, _) = run("30", &["/bin/sh", "-c", "kill -9 $$"])?;
    assert_eq!(output.status.code(), Some(128 + 9), "{}", stderr(&output));
    let (output, _) = run("30", &["/nonexistent/command"])?;
    assert_eq!(output.status.code(), Some(127), "{}", stderr(&output));
    Ok(())
}

#[test]
fn zero_or_missing_deadline_is_refused() -> TestResult {
    for secs in ["0", "x", "-1"] {
        let (output, _) = run(secs, &["/usr/bin/true"])?;
        assert_ne!(output.status.code(), Some(0), "срок «{secs}» принят");
    }
    let output = Command::new("/usr/bin/perl")
        .arg(helper())
        .arg("5")
        .output()
        .map_err(|e| e.to_string())?;
    assert_ne!(
        output.status.code(),
        Some(0),
        "команда без аргументов принята"
    );
    Ok(())
}
