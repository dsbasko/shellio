//! Токены из login keychain только через `/usr/bin/security find-generic-password -w`.
//!
//! Элемент создан с доверенным приложением `/usr/bin/security` (план P0.7, «Предусловия»):
//! пересобранный бинарь пробы меняет подпись, и прямое чтение через Security.framework вызвало бы
//! диалог ACL. Вызов идёт со сроком 10 с: диалог всё же появился — процесс `security` снимается,
//! а клетки аккаунта получают `not_run` «Keychain ждёт диалога».
//!
//! Токен приходит только из stdout `security` и сразу оборачивается в [`Secret`]: в аргументах
//! процесса его нет (там сервис и аккаунт), ошибки и `Debug` его не несут.

use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::transport::http::Secret;

pub const SECURITY: &str = "/usr/bin/security";
pub const DEADLINE: Duration = Duration::from_secs(10);
/// Код `security`, когда элемента нет (`errSecItemNotFound`): наблюдён на этой машине.
pub const ITEM_NOT_FOUND: i32 = 44;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenError {
    /// Элемента нет в keychain.
    Missing { account: String },
    /// `security` не ответил за срок: ждёт диалога доступа. Процесс снят.
    Timeout { account: String },
    /// `security` не запустился, завершился другим кодом или вернул пустой токен.
    Failed { account: String, detail: String },
}

impl TokenError {
    pub fn account(&self) -> &str {
        match self {
            TokenError::Missing { account }
            | TokenError::Timeout { account }
            | TokenError::Failed { account, .. } => account,
        }
    }

    /// Причина `not_run` клеток аккаунта.
    pub fn not_run_reason(&self) -> String {
        match self {
            TokenError::Missing { account } => format!("нет токена {account}"),
            TokenError::Timeout { .. } => "Keychain ждёт диалога".to_owned(),
            TokenError::Failed { account, detail } => {
                format!("токен {account} не прочитан: {detail}")
            }
        }
    }
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.not_run_reason())
    }
}

impl std::error::Error for TokenError {}

/// Чтение токенов через `security`. Программа и срок — параметры для тестов на подставной
/// команде.
#[derive(Clone, Debug)]
pub struct Keychain {
    program: PathBuf,
    deadline: Duration,
}

impl Keychain {
    pub fn system() -> Keychain {
        Keychain::with_program(Path::new(SECURITY), DEADLINE)
    }

    pub fn with_program(program: &Path, deadline: Duration) -> Keychain {
        Keychain {
            program: program.to_path_buf(),
            deadline,
        }
    }

    /// Токен элемента `service`/`account`.
    pub fn token(&self, service: &str, account: &str) -> Result<Secret, TokenError> {
        let failed = |detail: String| TokenError::Failed {
            account: account.to_owned(),
            detail,
        };
        let mut child = Command::new(&self.program)
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| failed(format!("security не запущен: {}", err.kind())))?;
        let stdout = read_in_thread(child.stdout.take());
        let stderr = read_in_thread(child.stderr.take());
        let started = Instant::now();
        let left = |started: Instant| self.deadline.saturating_sub(started.elapsed());
        let out = stdout.recv_timeout(left(started));
        let err = out
            .as_ref()
            .ok()
            .map(|_| stderr.recv_timeout(left(started)));
        let (out, err) = match (out, err) {
            (Ok(out), Some(Ok(err))) => (out, err),
            _ => {
                kill(&mut child);
                return Err(TokenError::Timeout {
                    account: account.to_owned(),
                });
            }
        };
        let status = child
            .wait()
            .map_err(|err| failed(format!("ожидание security: {}", err.kind())))?;
        let out = out.map_err(|err| failed(format!("stdout security: {err}")))?;
        let err = err.unwrap_or_default();
        match status.code() {
            Some(0) => {}
            Some(ITEM_NOT_FOUND) => {
                return Err(TokenError::Missing {
                    account: account.to_owned(),
                });
            }
            Some(code) => {
                return Err(failed(format!(
                    "security завершился кодом {code}: {}",
                    first_line(&err)
                )));
            }
            None => return Err(failed("security снят сигналом".to_owned())),
        }
        let text = String::from_utf8(out).map_err(|_| failed("токен не UTF-8".to_owned()))?;
        let token = text.strip_suffix('\n').unwrap_or(&text);
        if token.is_empty() || token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(failed("пустой токен или пробел в нём".to_owned()));
        }
        Ok(Secret::new(token))
    }
}

/// Поток читает канал до конца и отдаёт байты. Поток не ждут после снятия процесса: внук,
/// унаследовавший канал, держал бы его открытым.
fn read_in_thread<R: Read + Send + 'static>(
    pipe: Option<R>,
) -> mpsc::Receiver<Result<Vec<u8>, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = match pipe {
            Some(mut pipe) => pipe
                .read_to_end(&mut bytes)
                .map(|_| bytes)
                .map_err(|err| err.kind().to_string()),
            None => Err("канал не открыт".to_owned()),
        };
        let _ = tx.send(result);
    });
    rx
}

fn kill(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn first_line(text: &[u8]) -> String {
    String::from_utf8_lossy(text)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Подставная команда `security`: сценарий оболочки в каталоге теста.
    fn stub(dir: &Path, body: &str) -> Result<PathBuf, String> {
        let path = dir.join("security");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        Ok(path)
    }

    fn keychain(program: &Path) -> Keychain {
        Keychain::with_program(program, Duration::from_secs(10))
    }

    #[test]
    fn reads_token_from_stdout_and_passes_no_secret_in_arguments() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let args = dir.path().join("args.txt");
        let program = stub(
            dir.path(),
            &format!(
                "printf '%s\\n' \"$@\" > '{}'\nprintf 'ghp_stubtoken\\n'",
                args.display()
            ),
        )?;
        let token = keychain(&program)
            .token("svc", "github-author")
            .map_err(|e| e.to_string())?;
        assert_eq!(token.expose(), "ghp_stubtoken");
        assert!(!format!("{token:?}").contains("ghp_stubtoken"));
        let recorded = std::fs::read_to_string(&args).map_err(|e| e.to_string())?;
        assert_eq!(
            recorded.lines().collect::<Vec<_>>(),
            [
                "find-generic-password",
                "-s",
                "svc",
                "-a",
                "github-author",
                "-w"
            ]
        );
        assert!(!recorded.contains("ghp_"));
        Ok(())
    }

    #[test]
    fn missing_item_is_not_run_with_account() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let program = stub(
            dir.path(),
            "echo 'security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.' >&2\nexit 44",
        )?;
        let err = keychain(&program).token("svc", "gitlab-reviewer");
        assert_eq!(
            err,
            Err(TokenError::Missing {
                account: "gitlab-reviewer".to_owned()
            })
        );
        assert_eq!(
            err.err().map(|e| e.not_run_reason()).as_deref(),
            Some("нет токена gitlab-reviewer")
        );
        Ok(())
    }

    #[test]
    fn deadline_kills_waiting_security() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let fifo = dir.path().join("dialog");
        let made = Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .map_err(|e| e.to_string())?;
        assert!(made.success());
        let pid = dir.path().join("pid");
        // Открытие FIFO без писателя блокирует процесс, как диалог доступа: `exec` оставляет
        // один процесс, и снятие по сроку не оставляет внука с каналом stdout.
        let program = stub(
            dir.path(),
            &format!(
                "echo $$ > '{}'\nexec /bin/cat '{}'",
                pid.display(),
                fifo.display()
            ),
        )?;
        let started = Instant::now();
        let err = Keychain::with_program(&program, Duration::from_secs(1))
            .token("svc", "github-reviewer");
        assert_eq!(
            err,
            Err(TokenError::Timeout {
                account: "github-reviewer".to_owned()
            })
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(
            err.err().map(|e| e.not_run_reason()).as_deref(),
            Some("Keychain ждёт диалога")
        );
        // Файла нет только если процесс снят раньше, чем оболочка дошла до `echo`.
        if let Ok(pid) = std::fs::read_to_string(&pid) {
            let alive = Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .map_err(|e| e.to_string())?;
            assert!(!alive.success(), "процесс security {pid} не снят");
        }
        Ok(())
    }

    #[test]
    fn other_failures_carry_no_token() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let program = stub(
            dir.path(),
            "printf 'ghp_leak\\n'\necho 'security: denied' >&2\nexit 51",
        )?;
        let err = keychain(&program)
            .token("svc", "github-author")
            .err()
            .ok_or("ожидалась ошибка")?;
        assert!(matches!(err, TokenError::Failed { .. }), "{err:?}");
        assert!(err.not_run_reason().contains("кодом 51"));
        assert!(!format!("{err:?} {err}").contains("ghp_leak"));

        let empty = stub(dir.path(), "printf '\\n'")?;
        assert!(matches!(
            keychain(&empty).token("svc", "a"),
            Err(TokenError::Failed { .. })
        ));
        let absent = keychain(&dir.path().join("no-such-security")).token("svc", "a");
        assert!(matches!(absent, Err(TokenError::Failed { .. })));
        Ok(())
    }
}
