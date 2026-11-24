//! Обёртка источника ввода `scripts/e2e/with-latin-input.sh` отдельным процессом с подставными
//! `osascript` и `defaults` в `PATH`: настоящий источник ввода тест не читает и не меняет.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const RUSSIAN: &str = "com.apple.keylayout.RussianWin";
const KOTOERI: &str = "com.apple.inputmethod.Kotoeri.RomajiTyping.Japanese";
const ABC: &str = "com.apple.keylayout.ABC";
const US: &str = "com.apple.keylayout.US";
const BRITISH_PC: &str = "com.apple.keylayout.British-PC";

/// Подставной `osascript`: текущий источник — файл `state`, включённые — `installed`, вызовы —
/// `calls`. Файл `fail-current` роняет чтение текущего, `refuse` называет ID, выбор которого
/// отказывает, `stuck` — ID, выбор которого проходит, но источник не меняет.
const OSASCRIPT: &str = r#"#!/bin/bash
set -u
dir="$(cd "$(dirname "$0")/.." && pwd)"
[[ "$1" == -l && "$2" == JavaScript && "$3" == -e ]] || { echo "неверный вызов: $*" >&2; exit 64; }
printf '%s' "$4" >"$dir/script"
mode="$5"
id="${6:-}"
echo "$mode${id:+ $id}" >>"$dir/calls"
case "$mode" in
  current)
    if [[ -e "$dir/fail-current" ]]; then
      echo "execution error: мост не ответил (-1)" >&2
      exit 1
    fi
    cat "$dir/state"
    ;;
  list) cat "$dir/installed" ;;
  select)
    grep -qxF "$id" "$dir/installed" || { echo "execution error: источник не включён" >&2; exit 1; }
    if [[ -f "$dir/refuse" && "$(cat "$dir/refuse")" == "$id" ]]; then
      echo "execution error: TISSelectInputSource: код -50" >&2
      exit 1
    fi
    if [[ -f "$dir/stuck" && "$(cat "$dir/stuck")" == "$id" ]]; then
      exit 0
    fi
    printf '%s\n' "$id" >"$dir/state"
    ;;
  *) exit 64 ;;
esac
"#;

/// Подставной `defaults`: настройка HIToolbox следует за подставным источником.
const DEFAULTS: &str = r#"#!/bin/bash
cat "$(cd "$(dirname "$0")/.." && pwd)/state"
"#;

/// Команда под обёрткой: пишет источник, который видит во время работы, и выходит с кодом.
const SEE: &str = r#"cat "$1" >"$2"; exit "$3""#;

struct Stand {
    dir: tempfile::TempDir,
}

impl Stand {
    fn new(current: &str, installed: &[&str]) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin)?;
        for (name, text) in [("osascript", OSASCRIPT), ("defaults", DEFAULTS)] {
            let file = bin.join(name);
            std::fs::write(&file, text)?;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::write(dir.path().join("state"), format!("{current}\n"))?;
        std::fs::write(
            dir.path().join("installed"),
            installed
                .iter()
                .map(|id| format!("{id}\n"))
                .collect::<String>(),
        )?;
        std::fs::write(dir.path().join("calls"), "")?;
        Ok(Self { dir })
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn text(&self, name: &str) -> Result<String> {
        Ok(std::fs::read_to_string(self.path(name))?
            .trim_end()
            .to_owned())
    }

    fn mark(&self, name: &str, text: &str) -> Result<()> {
        Ok(std::fs::write(self.path(name), text)?)
    }

    fn calls(&self) -> Result<Vec<String>> {
        Ok(self.text("calls")?.lines().map(str::to_owned).collect())
    }

    fn selects(&self) -> Result<Vec<String>> {
        Ok(self
            .calls()?
            .into_iter()
            .filter_map(|call| call.strip_prefix("select ").map(str::to_owned))
            .collect())
    }

    fn wrapper(&self, command: &[&str]) -> Command {
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/e2e/with-latin-input.sh");
        let mut wrapper = Command::new("/bin/bash");
        wrapper
            .arg(script)
            .args(command)
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.path("bin").display()),
            )
            .env("SHELLIO_INPUT_SOURCE_DIR", self.path("run"));
        wrapper
    }

    /// Обёртка с командой `SEE`, которая выходит с кодом `code`: код обёртки и её stderr.
    fn see(&self, code: &str) -> Result<(i32, String)> {
        let state = self.path("state");
        let seen = self.path("seen");
        self.run(&[
            "/bin/sh",
            "-c",
            SEE,
            "sh",
            &state.to_string_lossy(),
            &seen.to_string_lossy(),
            code,
        ])
    }

    fn run(&self, command: &[&str]) -> Result<(i32, String)> {
        let output = self.wrapper(command).output()?;
        Ok((
            output.status.code().ok_or("обёртка убита сигналом")?,
            String::from_utf8(output.stderr)?,
        ))
    }

    fn record(&self, name: &str) -> Result<String> {
        self.text(&format!("run/{name}"))
    }
}

/// Включённый латинский источник обёртка не трогает, код команды сохраняется.
#[test]
fn latin_source_is_left_alone() -> Result<()> {
    let stand = Stand::new(US, &[RUSSIAN, US])?;
    let (code, stderr) = stand.see("7")?;
    assert_eq!(code, 7, "{stderr}");
    assert_eq!(stand.selects()?, Vec::<String>::new());
    assert_eq!(stand.text("seen")?, US);
    assert_eq!(stand.text("state")?, US);
    assert_eq!(
        (stand.record("before")?, stand.record("after")?),
        (US.to_owned(), US.to_owned())
    );
    Ok(())
}

/// Другой источник заменяется первым установленным латинским на время команды и возвращается
/// точным исходным ID; код команды сохраняется и при штатном выходе, и при отказе команды.
#[test]
fn foreign_source_is_switched_for_the_command_and_restored() -> Result<()> {
    for command_code in [0, 7] {
        let stand = Stand::new(RUSSIAN, &[BRITISH_PC, RUSSIAN, KOTOERI, US])?;
        let (code, stderr) = stand.see(&command_code.to_string())?;
        assert_eq!(code, command_code, "{stderr}");
        assert_eq!(stand.text("seen")?, US, "источник во время команды");
        assert_eq!(stand.text("state")?, RUSSIAN, "источник после команды");
        assert_eq!(stand.selects()?, [US, RUSSIAN]);
        assert_eq!(stand.record("before")?, RUSSIAN);
        assert_eq!(stand.record("after")?, RUSSIAN);
        let script = stand.text("script")?;
        for name in [
            "ObjC.import('Carbon')",
            "TISCopyCurrentKeyboardInputSource",
            "TISCreateInputSourceList",
            "TISSelectInputSource",
            "kTISPropertyInputSourceID",
        ] {
            assert!(script.contains(name), "в сценарии моста нет {name}");
        }
    }
    Ok(())
}

/// Без установленного латинского источника команда не запускается: код 1, выбора не было.
#[test]
fn no_latin_source_fails_before_the_command() -> Result<()> {
    let stand = Stand::new(RUSSIAN, &[RUSSIAN, KOTOERI])?;
    let (code, stderr) = stand.see("0")?;
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("источник ввода не переключён: нет установленного латинского источника"),
        "{stderr}"
    );
    assert!(!stand.path("seen").exists(), "команда запускалась");
    assert_eq!(stand.selects()?, Vec::<String>::new());
    assert_eq!(stand.text("state")?, RUSSIAN);
    Ok(())
}

/// Отказ вызова моста при чтении текущего источника: код 1 до команды.
#[test]
fn bridge_failure_fails_before_the_command() -> Result<()> {
    let stand = Stand::new(RUSSIAN, &[RUSSIAN, ABC])?;
    stand.mark("fail-current", "")?;
    let (code, stderr) = stand.see("0")?;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("источник ввода не прочитан"), "{stderr}");
    assert!(!stand.path("seen").exists(), "команда запускалась");
    assert_eq!(stand.selects()?, Vec::<String>::new());
    assert!(!stand.path("run/before").exists());
    Ok(())
}

/// Выбор прошёл, но чтение показывает прежний источник: команда не запускается, код 1, исходный
/// источник выбирается обратно.
#[test]
fn unconfirmed_switch_fails_before_the_command() -> Result<()> {
    let stand = Stand::new(RUSSIAN, &[RUSSIAN, ABC])?;
    stand.mark("stuck", ABC)?;
    let (code, stderr) = stand.see("0")?;
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("источник ввода не переключён"), "{stderr}");
    assert!(!stand.path("seen").exists(), "команда запускалась");
    assert_eq!(stand.selects()?, [ABC, RUSSIAN]);
    assert_eq!(stand.text("state")?, RUSSIAN);
    assert_eq!(stand.record("after")?, RUSSIAN);
    Ok(())
}

/// Отказ возврата исходного источника — код 1, хотя команда вышла с кодом 0.
#[test]
fn failed_restore_is_an_error() -> Result<()> {
    let stand = Stand::new(RUSSIAN, &[RUSSIAN, ABC])?;
    stand.mark("refuse", RUSSIAN)?;
    let (code, stderr) = stand.see("0")?;
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("источник ввода не восстановлен"),
        "{stderr}"
    );
    assert_eq!(stand.text("seen")?, ABC);
    assert_eq!(stand.selects()?, [ABC, RUSSIAN]);
    assert_eq!(stand.record("before")?, RUSSIAN);
    assert_eq!(stand.record("after")?, ABC);
    Ok(())
}

/// `SIGTERM` обёртке во время команды: команда получает сигнал, исходный источник возвращён,
/// код — 143.
#[test]
fn term_during_the_command_restores_the_source() -> Result<()> {
    let stand = Stand::new(RUSSIAN, &[RUSSIAN, ABC])?;
    let ready = stand.path("ready");
    let hold = stand.path("hold");
    for fifo in [&ready, &hold] {
        let made = Command::new("/usr/bin/mkfifo").arg(fifo).status()?;
        assert!(made.success(), "mkfifo {}", fifo.display());
    }
    // Команда сообщает источник через FIFO и встаёт на открытии FIFO, в который никто не пишет.
    let mut wrapper = stand
        .wrapper(&[
            "/bin/sh",
            "-c",
            r#"cat "$1" >"$2"; exec cat "$3""#,
            "sh",
            &stand.path("state").to_string_lossy(),
            &ready.to_string_lossy(),
            &hold.to_string_lossy(),
        ])
        .stderr(Stdio::piped())
        .spawn()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::fs::read_to_string(ready));
    });
    let seen = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(seen) => seen?,
        Err(_) => {
            wrapper.kill()?;
            let output = wrapper.wait_with_output()?;
            return Err(format!(
                "команда под обёрткой не запустилась: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
    };
    assert_eq!(seen.trim_end(), ABC, "источник во время команды");

    let sent = Command::new("/bin/kill")
        .arg("-TERM")
        .arg(wrapper.id().to_string())
        .status()?;
    assert!(sent.success(), "kill -TERM обёртке");
    let output = wrapper.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(143), "{stderr}");
    assert_eq!(stand.text("state")?, RUSSIAN, "источник после сигнала");
    assert_eq!(stand.selects()?, [ABC, RUSSIAN]);
    assert_eq!(stand.record("before")?, RUSSIAN);
    assert_eq!(stand.record("after")?, RUSSIAN);
    Ok(())
}

/// Вызов без команды — код 2, мост не вызывается.
#[test]
fn missing_command_is_a_usage_error() -> Result<()> {
    let stand = Stand::new(RUSSIAN, &[RUSSIAN, ABC])?;
    let (code, stderr) = stand.run(&[])?;
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("usage:"), "{stderr}");
    assert_eq!(stand.calls()?, Vec::<String>::new());
    Ok(())
}
