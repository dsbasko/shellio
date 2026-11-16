//! Без функции `vt` ничего не ищет, не генерирует и не линкует: `make check` идёт без набора VT
//! (ADR-0035). С `vt` сверяет набор `libghostty-vt` пробы через `scripts/vt.sh --verify` и линкует
//! только его. Привязки лежат в `src/vt/bindings.rs`, их генерирует `run.sh bindings`.
//!
//! Блокировку набора берёт `run.sh`, `build.rs` её не берёт. Набор собирает только
//! `scripts/vt.sh ensure`; здесь сети нет, расхождение даёт ошибку сверки.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if env::var_os("CARGO_FEATURE_VT").is_none() {
        return;
    }
    if let Err(message) = link_vt() {
        eprintln!("error: {message}");
        process::exit(1);
    }
}

fn link_vt() -> Result<(), String> {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").ok_or("cargo не задал CARGO_MANIFEST_DIR")?,
    );
    let root = manifest_dir
        .join("../..")
        .canonicalize()
        .map_err(|err| format!("не удалось найти корень репозитория: {err}"))?;
    let set = sets_dir(&root)?.join("vt");

    for path in [
        manifest_dir.join("scripts/vt.sh"),
        manifest_dir.join("scripts/set.sh"),
        root.join("scripts/ghostty/pin"),
        root.join("scripts/ghostty-patches"),
        set.join("stamp"),
        set.join("manifest"),
        set.join("lib/libghostty-vt.a"),
    ] {
        println!("cargo::rerun-if-changed={}", path.display());
    }
    for name in [
        "SHELLIO_PROBE_SETS_DIR",
        "SHELLIO_CACHE_DIR",
        "DEVELOPER_DIR",
        "SDKROOT",
    ] {
        println!("cargo::rerun-if-env-changed={name}");
    }

    verify(&manifest_dir.join("scripts/vt.sh"))?;
    let lib_dir = set.join("lib");
    if !lib_dir.join("libghostty-vt.a").is_file() {
        return Err(format!(
            "в наборе {} нет lib/libghostty-vt.a",
            set.display()
        ));
    }
    println!("cargo::rustc-link-search=native={}", lib_dir.display());
    // Архив не копируется в rlib, а подключается при линковке бинаря и тестов.
    println!("cargo::rustc-link-lib=static:-bundle=ghostty-vt");
    Ok(())
}

// Каталог наборов определяется так же, как в vt.sh: сверяется тот набор, что линкуется.
fn sets_dir(root: &Path) -> Result<PathBuf, String> {
    match env::var_os("SHELLIO_PROBE_SETS_DIR") {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Ok(path)
            } else {
                Err(format!(
                    "SHELLIO_PROBE_SETS_DIR должен быть абсолютным путём, а не {}",
                    path.display()
                ))
            }
        }
        _ => Ok(root.join("target/probes/session-survival")),
    }
}

fn verify(script: &Path) -> Result<(), String> {
    let output = Command::new(script)
        .arg("--verify")
        .output()
        .map_err(|err| format!("не удалось запустить {} --verify: {err}", script.display()))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "набор libghostty-vt пробы не прошёл сверку ({}), выполните make probe-session\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end()
    ))
}
