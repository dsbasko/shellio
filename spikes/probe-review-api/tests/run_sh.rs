//! `run.sh` без сборки, docker и настоящей блокировки: разбор осей до блокировки, маркер старта
//! под замерной блокировкой и код «не начат». Блокировка — во временном `SHELLIO_CACHE_DIR`,
//! поэтому тесты идут и под внешним захватом настоящего `measure.lock` рубежа.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use probe_review_api::known_pairs;
use probe_review_api::results::Column;

const EXIT_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_NOT_STARTED: i32 = 4;

type TestResult = Result<(), String>;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("run.sh")
}

fn run(cache: &Path, envs: &[(&str, &str)], args: &[&str]) -> Result<Output, String> {
    let mut command = Command::new(script());
    command
        .args(args)
        .env("SHELLIO_CACHE_DIR", cache)
        .env("CARGO", "/nonexistent/cargo")
        .env("PROBE_REVIEW_DOCKER", "/nonexistent/docker")
        .env_remove("HOSTS")
        .env_remove("SERIES");
    for (key, value) in envs {
        command.env(key, value);
    }
    command
        .output()
        .map_err(|e| format!("run.sh не запускается: {e}"))
}

fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn tempdir() -> Result<tempfile::TempDir, String> {
    tempfile::tempdir().map_err(|e| e.to_string())
}

#[test]
fn unknown_scenario_is_refused_before_lock() -> TestResult {
    let cache = tempdir()?;
    let output = run(cache.path(), &[], &["fake", "bogus"])?;
    assert_eq!(code(&output), Some(EXIT_USAGE), "{}", stderr(&output));
    assert!(stderr(&output).contains("bogus"));
    assert!(!cache.path().join("measure.lock").exists());
    Ok(())
}

#[test]
fn empty_host_intersection_is_refused_before_lock() -> TestResult {
    let cache = tempdir()?;
    let output = run(cache.path(), &[("HOSTS", "github_cloud")], &["fake"])?;
    assert_eq!(code(&output), Some(EXIT_USAGE), "{}", stderr(&output));
    assert!(stderr(&output).contains("пустое пересечение"));
    let output = run(cache.path(), &[("HOSTS", "mars")], &["fake"])?;
    assert_eq!(code(&output), Some(EXIT_USAGE), "{}", stderr(&output));
    assert!(!cache.path().join("measure.lock").exists());
    Ok(())
}

#[test]
fn live_without_scenarios_is_refused_before_lock() -> TestResult {
    let cache = tempdir()?;
    let output = run(cache.path(), &[], &["live"])?;
    assert_eq!(code(&output), Some(EXIT_USAGE), "{}", stderr(&output));
    assert!(stderr(&output).contains("пустое пересечение"));
    assert!(!cache.path().join("measure.lock").exists());
    Ok(())
}

#[test]
fn invalid_series_is_refused_before_lock() -> TestResult {
    let cache = tempdir()?;
    let output = run(cache.path(), &[("SERIES", "a b/c")], &["fake"])?;
    assert_eq!(code(&output), Some(EXIT_USAGE), "{}", stderr(&output));
    assert!(!cache.path().join("measure.lock").exists());
    Ok(())
}

#[test]
fn command_under_lock_keeps_its_code() -> TestResult {
    let cache = tempdir()?;
    let marker = cache.path().join("ran");
    let marker_text = marker.to_string_lossy().into_owned();
    let output = run(
        cache.path(),
        &[],
        &["with-lock", "/usr/bin/touch", marker_text.as_str()],
    )?;
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert!(marker.exists());
    assert!(cache.path().join("measure.lock").exists());
    let output = run(cache.path(), &[], &["with-lock", "/bin/sh", "-c", "exit 7"])?;
    assert_eq!(code(&output), Some(7), "{}", stderr(&output));
    Ok(())
}

#[test]
fn code_75_of_the_command_is_execution_error_not_not_started() -> TestResult {
    let cache = tempdir()?;
    for inner in ["exit 75", "exit 4"] {
        let output = run(cache.path(), &[], &["with-lock", "/bin/sh", "-c", inner])?;
        assert_eq!(
            code(&output),
            Some(EXIT_ERROR),
            "{inner}: {}",
            stderr(&output)
        );
        assert!(stderr(&output).contains("ошибка исполнения"));
    }
    Ok(())
}

/// Держит блокировку отдельным `lockf`, пока открыт его stdin; готовность — строка в stdout.
struct Holder {
    child: std::process::Child,
}

impl Holder {
    fn hold(lock: &Path) -> Result<Holder, String> {
        let mut child = Command::new("/usr/bin/lockf")
            .arg("-k")
            .arg(lock)
            .args(["/bin/sh", "-c", "echo held; read _"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("lockf не запускается: {e}"))?;
        let stdout = child.stdout.take().ok_or("нет stdout у lockf")?;
        let mut line = String::new();
        BufReader::new(stdout)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if line.trim() != "held" {
            return Err(format!("держатель блокировки ответил «{line}»"));
        }
        Ok(Holder { child })
    }

    fn release(mut self) -> TestResult {
        if let Some(mut stdin) = self.child.stdin.take() {
            let _ = stdin.write_all(b"\n");
        }
        let status = self.child.wait().map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("держатель блокировки завершился {status}"));
        }
        Ok(())
    }
}

#[test]
fn busy_lock_gives_not_started_without_running_the_command() -> TestResult {
    let cache = tempdir()?;
    let lock = cache.path().join("measure.lock");
    let holder = Holder::hold(&lock)?;
    let marker = cache.path().join("ran");
    let marker_text = marker.to_string_lossy().into_owned();
    let output = run(
        cache.path(),
        &[("PROBE_REVIEW_LOCK_TIMEOUT", "0")],
        &["with-lock", "/usr/bin/touch", marker_text.as_str()],
    );
    holder.release()?;
    let output = output?;
    assert_eq!(code(&output), Some(EXIT_NOT_STARTED), "{}", stderr(&output));
    assert!(stderr(&output).contains("не начат"));
    assert!(!marker.exists(), "команда выполнилась без блокировки");
    Ok(())
}

/// Списки сценариев `run.sh` совпадают с каталогом бинаря: `run.sh` проверяет оси до сборки.
#[test]
fn run_sh_axes_match_scenario_catalog() -> TestResult {
    let text = std::fs::read_to_string(script()).map_err(|e| e.to_string())?;
    let value = |name: &str| -> Result<Vec<String>, String> {
        let prefix = format!("{name}=\"");
        let line = text
            .lines()
            .find(|l| l.starts_with(&prefix))
            .ok_or_else(|| format!("в run.sh нет {name}"))?;
        Ok(line[prefix.len()..]
            .trim_end_matches('"')
            .split_whitespace()
            .map(str::to_owned)
            .collect())
    };
    let scenarios_of = |columns: &[Column]| -> Vec<String> {
        let mut names: Vec<String> = columns
            .iter()
            .flat_map(|c| known_pairs(Some(*c)))
            .map(|p| p.scenario)
            .collect();
        names.sort();
        names.dedup();
        names
    };
    let sorted = |mut v: Vec<String>| {
        v.sort();
        v
    };
    assert_eq!(sorted(value("FAKE_HOSTS")?), vec!["fake".to_owned()]);
    assert_eq!(
        sorted(value("FAKE_SCENARIOS")?),
        scenarios_of(&[Column::Fake])
    );
    let live = [
        Column::GithubCloud,
        Column::GitlabCloud,
        Column::GitlabCe,
        Column::Ghes,
    ];
    let mut live_names: Vec<String> = live.iter().map(|c| c.name().to_owned()).collect();
    live_names.sort();
    assert_eq!(sorted(value("LIVE_HOSTS")?), live_names);
    assert_eq!(sorted(value("LIVE_SCENARIOS")?), scenarios_of(&live));
    Ok(())
}
