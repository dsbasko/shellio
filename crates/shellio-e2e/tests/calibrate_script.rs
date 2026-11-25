//! Отказы `scripts/conformance/calibrate.sh` до сборки и замера и цель `make calibrate`:
//! сценарий отдельным процессом на копии во временном дереве.

use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Копия сценария во временном дереве: корень сценарий выводит из своего места.
fn tree() -> Result<(tempfile::TempDir, PathBuf)> {
    let dir = tempfile::tempdir()?;
    let scripts = dir.path().join("scripts/conformance");
    std::fs::create_dir_all(&scripts)?;
    std::fs::create_dir_all(dir.path().join("conformance/calibration"))?;
    let script = scripts.join("calibrate.sh");
    std::fs::copy(repo().join("scripts/conformance/calibrate.sh"), &script)?;
    Ok((dir, script))
}

fn run(root: &Path, script: &Path, args: &[&str]) -> Result<(i32, String)> {
    let output = Command::new("/bin/bash")
        .arg(script)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("SHELLIO_CACHE_DIR", root.join("cache"))
        .output()?;
    Ok((
        output.status.code().ok_or("calibrate.sh убит сигналом")?,
        String::from_utf8(output.stderr)?,
    ))
}

/// Состоявшаяся калибровка не повторяется: запись на месте — отказ с кодом 1 до сборки, замера
/// и каталога прогона, запись не тронута.
#[test]
fn existing_record_refuses_a_repeat_before_any_work() -> Result<()> {
    let (dir, script) = tree()?;
    let record = dir.path().join("conformance/calibration/calibration.json");
    std::fs::write(&record, "{\"schema\":1}\n")?;
    let (code, stderr) = run(dir.path(), &script, &[])?;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("повтор запрещён"), "{stderr}");
    assert_eq!(std::fs::read_to_string(&record)?, "{\"schema\":1}\n");
    assert!(!dir.path().join("target").exists());
    Ok(())
}

/// Аргументов у сценария нет: лишний — код 2 до блокировки съёма.
#[test]
fn argument_is_a_usage_error_before_the_lock() -> Result<()> {
    let (dir, script) = tree()?;
    let (code, stderr) = run(dir.path(), &script, &["--force"])?;
    assert_eq!(code, 2, "{stderr}");
    assert!(!dir.path().join("cache").exists());
    Ok(())
}

/// `make calibrate` идёт под блокировкой набора ghostty (бандл собирается внутри) и вызывает
/// сценарий; готовая запись проверяется раньше оконного менеджера, блокировки набора и сборки.
#[test]
fn make_target_runs_the_script_under_the_ghostty_lock() -> Result<()> {
    let makefile = std::fs::read_to_string(repo().join("Makefile"))?;
    let locked = makefile
        .split("LOCKED_TARGETS :=")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .ok_or("в Makefile нет LOCKED_TARGETS")?;
    assert!(locked.split_whitespace().any(|name| name == "calibrate"));
    assert!(
        makefile.contains("locked-calibrate:\n\tCARGO=$(CARGO) scripts/conformance/calibrate.sh\n")
    );
    let script = std::fs::read_to_string(repo().join("scripts/conformance/calibrate.sh"))?;
    let at = |needle: &str| {
        script
            .find(needle)
            .ok_or(format!("в сценарии нет {needle:?}"))
    };
    let record = at("уже есть: после состоявшейся")?;
    let manager = at("предусловие: запущен оконный менеджер")?;
    let ghostty = at("нет блокировки набора ghostty")?;
    let bundle = at("scripts/bundle.sh\" >")?;
    assert!(record < manager && manager < ghostty && ghostty < bundle);
    Ok(())
}
