//! Тесты режимов бинаря и логики маркера старта `run.sh` на временном файле блокировки.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "probe-resources-main-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn measure_exec(cache: &Path, dir: &Path, command: &[&str]) -> (i32, String) {
    let output = Command::new("/bin/bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("run.sh"))
        .arg("measure-exec")
        .arg(dir)
        .args(command)
        .env("SHELLIO_CACHE_DIR", cache)
        .env("SHELLIO_MEASURE_LOCK_TIMEOUT", "0")
        .output()
        .unwrap();
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    )
}

#[test]
fn start_marker_separates_busy_lock_from_command_code_75() {
    let base = temp_dir("lock");
    let cache = base.join("cache");
    let dir = base.join("unit");
    std::fs::create_dir_all(&cache).unwrap();

    assert_eq!(
        measure_exec(&cache, &dir, &["/bin/sh", "-c", "exit 75"]),
        (0, "exit:75".to_owned())
    );
    assert!(!dir.join(".measure-lock-start").exists());

    let mut holder = Command::new("/usr/bin/lockf")
        .arg("-k")
        .arg(cache.join("measure.lock"))
        .args(["/bin/sh", "-c", "echo held; read line"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(holder.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "held");

    assert_eq!(
        measure_exec(&cache, &dir, &["/usr/bin/true"]),
        (0, "lock-busy".to_owned())
    );
    assert!(!dir.join(".measure-lock-start").exists());

    holder.stdin.take().unwrap().write_all(b"\n").unwrap();
    assert!(holder.wait().unwrap().success());

    assert_eq!(
        measure_exec(&cache, &dir, &["/bin/sh", "-c", "exit 3"]),
        (0, "exit:3".to_owned())
    );
    std::fs::remove_dir_all(&base).unwrap();
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn unknown_modes_and_extra_arguments_are_refused() {
    assert!(dispatch(Vec::new()).is_err());
    assert!(dispatch(args(&["--bogus"])).is_err());
    assert!(dispatch(args(&["--list", "extra"])).is_err());
    assert!(dispatch(args(&["--scenario", "nope", "--out", "/nonexistent"])).is_err());
    assert!(dispatch(args(&["--fixture", "load"])).is_err());
    assert!(dispatch(args(&["--observer"])).is_err());
}

#[test]
fn record_mode_reports_retry_then_not_run() {
    let base = temp_dir("record");
    let out = base.join("smoke");
    std::fs::create_dir_all(&out).unwrap();
    let counters = base.join("not-started");
    let call = |attempt: &str| {
        dispatch(args(&[
            "--record",
            counters.to_str().unwrap(),
            "--unit",
            "smoke",
            "--attempt",
            attempt,
            "--out",
            out.to_str().unwrap(),
            "--scenario",
            "smoke",
        ]))
        .unwrap()
    };
    assert_eq!(call("noisy"), runstate::EXIT_NOT_STARTED);
    assert_eq!(call("noisy"), runstate::EXIT_NOT_STARTED);
    assert_eq!(call("lock-busy"), 0);
    let result: results::ResultFile = results::read_json(&out.join(results::RESULT_FILE)).unwrap();
    assert_eq!(
        result.cells[0].not_run_cause,
        Some(results::NotRunCause::LockBusy)
    );
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn fingerprint_mode_signals_mismatch_with_its_own_code() {
    let run = temp_dir("fingerprint");
    let call = |sha: &str| {
        dispatch(args(&[
            "--fingerprint",
            run.to_str().unwrap(),
            "--sha256",
            sha,
            "--profile",
            "release",
        ]))
        .unwrap()
    };
    assert_eq!(call("aa"), 0);
    assert_eq!(call("aa"), 0);
    assert_eq!(call("bb"), runstate::EXIT_BUILD_MISMATCH);
    std::fs::remove_dir_all(&run).unwrap();
}
