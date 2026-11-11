//! Сверяет набор ghostty, линкует его по `scripts/ghostty/link-libs` и отдаёт версию ghostty из штампа.
//!
//! Блокировку набора берут цели `make`, `build.rs` её не берёт. Прямой `cargo build` после смены SDK без
//! изменения отслеживаемых файлов сверку не повторяет, поэтому рабочий путь — цели `Makefile`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

fn main() {
    if let Err(message) = run() {
        eprintln!("error: {message}");
        process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let manifest_dir =
        env::var_os("CARGO_MANIFEST_DIR").ok_or("cargo не задал CARGO_MANIFEST_DIR")?;
    let root = fs::canonicalize(Path::new(&manifest_dir).join("../.."))
        .map_err(|err| format!("не удалось найти корень репозитория: {err}"))?;
    let scripts = root.join("scripts/ghostty");
    let vendor = vendor_root(&root)?;

    for path in [
        scripts.join("build.sh"),
        scripts.join("vendor.sh"),
        scripts.join("pin"),
        scripts.join("link-libs"),
        root.join("scripts/ghostty-patches"),
        vendor.join("stamp"),
        vendor.join("manifest"),
        vendor.join("lib/libghostty.a"),
        vendor.join("include/ghostty.h"),
    ] {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    for name in ["SHELLIO_GHOSTTY_VENDOR", "DEVELOPER_DIR", "SDKROOT"] {
        println!("cargo::rerun-if-env-changed={name}");
    }

    verify(&scripts, &vendor)?;
    link(&scripts, &vendor)?;
    let version = stamp_value(&vendor, "ghostty_version_string")?;
    println!("cargo::rustc-env=SHELLIO_GHOSTTY_VERSION={version}");
    Ok(())
}

// Корень набора определяется так же, как в vendor.sh, и передаётся ему явно: сверяется тот набор, что линкуется.
fn vendor_root(root: &Path) -> Result<PathBuf, String> {
    match env::var_os("SHELLIO_GHOSTTY_VENDOR") {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Ok(path)
            } else {
                Err(format!(
                    "SHELLIO_GHOSTTY_VENDOR должен быть абсолютным путём, а не {}",
                    path.display()
                ))
            }
        }
        _ => Ok(root.join("vendor/ghostty")),
    }
}

fn verify(scripts: &Path, vendor: &Path) -> Result<(), String> {
    let output = Command::new(scripts.join("vendor.sh"))
        .arg("--verify")
        .env("SHELLIO_GHOSTTY_VENDOR", vendor)
        .output()
        .map_err(|err| format!("не удалось запустить vendor.sh --verify: {err}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "набор ghostty {} не прошёл сверку ({}), выполните make ghostty\n{}",
        vendor.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end()
    ))
}

fn link(scripts: &Path, vendor: &Path) -> Result<(), String> {
    let list_path = scripts.join("link-libs");
    let list = fs::read_to_string(&list_path)
        .map_err(|err| format!("не удалось прочитать {}: {err}", list_path.display()))?;
    for line in list
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let Some((kind, name)) = line.split_once('=').filter(|(_, name)| !name.is_empty()) else {
            return Err(format!("строка link-libs без вида: {line}"));
        };
        match kind {
            "static" => {
                let lib_dir = vendor.join("lib");
                if !lib_dir.join(format!("lib{name}.a")).is_file() {
                    return Err(format!("в наборе {} нет lib/lib{name}.a", vendor.display()));
                }
                println!("cargo::rustc-link-search=native={}", lib_dir.display());
                // Архив в 140 МБ не копируется в rlib каждого профиля, а подключается при линковке бинаря.
                println!("cargo::rustc-link-lib=static:-bundle={name}");
            }
            "framework" | "dylib" => println!("cargo::rustc-link-lib={kind}={name}"),
            _ => return Err(format!("неизвестный вид {kind} в link-libs: {line}")),
        }
    }
    Ok(())
}

fn stamp_value(vendor: &Path, key: &str) -> Result<String, String> {
    let path = vendor.join("stamp");
    let stamp = fs::read_to_string(&path)
        .map_err(|err| format!("не удалось прочитать {}: {err}", path.display()))?;
    stamp
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("в штампе {} нет {key}", path.display()))
}
