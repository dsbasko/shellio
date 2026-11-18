//! Сценарий `keychain` (задача 14): клетка-наблюдение F «Keychain». Вопрос — читает ли N+1 секрет
//! N без запроса, если item создан N с доступом только для собственного кода.
//!
//! Стенд:
//! - временная файловая keychain `~/Library/Keychains/psu-f-<nonce>.keychain-db` со случайным
//!   паролем, разблокирована, в список поиска не добавляется и удаляется в любом исходе (после
//!   `SIGKILL` процесса сценария — внешним запуском, [`keychain::sweep`]);
//! - копии бандлов в `<state>/kc/`: установленный путь `install/probe-update.app` с поколением 1,
//!   копии поколения 2, кандидата `foreign` и бандла `.denied`. N+1 и `foreign` встают на
//!   установленный путь обменом `RENAME_SWAP`, как при обновлении;
//! - каждая операция — отдельный процесс `probe-update-app --keychain … --window` через
//!   `open -n -W` с итогом в `keychain/ops/<операция>.json`.
//!
//! Ход: `.denied` и GUI N создают свои item; отрицательный контроль — чтение item `.denied`
//! процессом N каждым способом запрета взаимодействия по порядку [`NoUi::ALL`], пока один не даст
//! отказ без окна; базовое чтение N своего item; чтение N+1 и `foreign`. До и после чтений —
//! `security dump-keychain -a`, до, во время и после клетки — список поиска, keychain по умолчанию
//! и `scripts/signing/requirement.txt`.
//!
//! Части — наблюдения клетки F: `interaction-control`, `next-read`, `foreign-read`,
//! `item-unchanged` и `own-read`. Ошибка исполнения — только сломанный стенд: keychain не создана,
//! запись или базовое чтение N не прошли, операцию выполнил не тот код, вывод `security` не
//! разобран, окружение keychain изменилось, окно запроса осталось на экране.

use std::fs::{self, File};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use objc2_core_graphics::CGEvent;
use serde::Serialize;
use serde_json::json;

use crate::generation::{self, BundleEntry, Layout, Run};
use crate::keychain::{
    self, ACCOUNT_DENIED, ACCOUNT_N, Class, DumpItem, NoUi, Op, OpArgs, OpReport,
    PARTITIONED_VERSION, SECURITY, SERVICE,
};
use crate::kq::{Event, Queue};
use crate::launch;
use crate::results::{Cell, Diagnostics, Environment, ProcessRecord, Report, Requirement, Status};
use crate::{spawn, sys};

use super::reject::install;
use super::smoke::{WindowEntry, window_list};
use super::{PartSpec, stand_inputs};

pub const DEADLINE: Duration = Duration::from_secs(420);
pub const CELL: &str = "F";
pub const DIR: &str = "keychain";
pub const STEPS_ARTIFACT: &str = "keychain/steps.json";
pub const ENVIRONMENT_ARTIFACT: &str = "keychain/environment.json";
pub const DUMPS_DIR: &str = "keychain/dumps";
/// Срок одной операции: запуск через LaunchServices, окно и вызовы Security.
pub const OP_DEADLINE: Duration = Duration::from_secs(30);
/// Ожидание выхода `open -W` после `SIGKILL` зависшей операции.
pub const KILL_WAIT: Duration = Duration::from_secs(10);
/// Срок одного вызова `security`.
pub const SECURITY_DEADLINE: Duration = Duration::from_secs(20);
/// Владельцы окон запроса keychain и авторизации.
pub const DIALOG_OWNERS: [&str; 2] = ["SecurityAgent", "coreautha"];
const KEY_ESCAPE: u16 = 53;
const OPEN: &str = "/usr/bin/open";
/// Requirement продукта: временная keychain и временная идентичность `foreign` его не меняют.
const REQUIREMENT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/signing/requirement.txt"
);
const COMPLETION: &str = "итог операции в срок 30 с; иначе окно запроса по списку окон, SIGKILL операции и запасной Escape";

pub const OWN: PartSpec = PartSpec {
    name: "own-read",
    stimulus: "GUI N (поколение 1 на установленном пути) через open -n с --window: SecItemAdd item n с доступом только для своего кода во временную keychain (kSecUseKeychain), затем чтение им же с запрещённым взаимодействием (kSecMatchSearchList)",
    oracle: "запись errSecSuccess; чтение: атрибуты и данные errSecSuccess, SHA-256 значения совпадает, окна запроса нет; операции выполнил код поколения 1 (CDHash процесса из ядра)",
};
pub const CONTROL: PartSpec = PartSpec {
    name: "interaction-control",
    stimulus: "бандл .denied через open -n создаёт item denied с доступом только для своего кода; GUI N читает его с запрещённым взаимодействием: сначала process-flag (SecKeychainSetUserInteractionAllowed(false)), при неудаче query-fail (kSecUseAuthenticationUIFail)",
    oracle: "item виден в поиске (атрибуты errSecSuccess), данные — отказ доступа (errSecInteractionNotAllowed или другой отказ), окна запроса нет; первый такой способ — способ чтений N+1 и foreign",
};
pub const NEXT: PartSpec = PartSpec {
    name: "next-read",
    stimulus: "установленный путь обменян RENAME_SWAP на копию поколения 2; N+1 через open -n с --window читает item n способом контроля",
    oracle: "атрибуты и данные errSecSuccess, SHA-256 значения совпадает, окна запроса нет, операцию выполнил код поколения 2 на установленном пути; keychain версии 512, у item n есть partition list",
};
pub const FOREIGN: PartSpec = PartSpec {
    name: "foreign-read",
    stimulus: "установленный путь обменян RENAME_SWAP на копию foreign (тот же код поколения 2, другой сертификат); чтение item n способом контроля",
    oracle: "item виден в поиске (атрибуты errSecSuccess), данные не прочитаны молча: отказ доступа или окно запроса",
};
pub const ITEM: PartSpec = PartSpec {
    name: "item-unchanged",
    stimulus: "security dump-keychain -a временной keychain после записи, после базового чтения N, после чтения N+1 и после чтения foreign",
    oracle: "item n во всех снимках с теми же атрибутами, ACL и partition list; partition list есть",
};

fn cell(spec: &PartSpec, status: Status, reason: String, artifacts: &[&str]) -> Cell {
    Cell {
        id: CELL.to_owned(),
        part: Some(spec.name.to_owned()),
        requirement: Requirement::Observation,
        status,
        stimulus: spec.stimulus.to_owned(),
        oracle: spec.oracle.to_owned(),
        completion: COMPLETION.to_owned(),
        reason,
        artifacts: artifacts
            .iter()
            .map(|artifact| (*artifact).to_owned())
            .collect(),
    }
}

// --- окна запроса ---

/// Окна запроса keychain на экране.
pub fn dialogs(windows: &[WindowEntry]) -> Vec<WindowEntry> {
    windows
        .iter()
        .filter(|window| DIALOG_OWNERS.contains(&window.owner.as_str()))
        .cloned()
        .collect()
}

fn dialogs_now() -> Vec<WindowEntry> {
    dialogs(&window_list())
}

fn press_escape(pid: i32) -> Result<(), String> {
    for down in [true, false] {
        let event = CGEvent::new_keyboard_event(None, KEY_ESCAPE, down)
            .ok_or("CGEventCreateKeyboardEvent вернул NULL")?;
        CGEvent::post_to_pid(pid, Some(&event));
    }
    Ok(())
}

/// Окно запроса, появившееся во время операции, и его закрытие.
#[derive(Clone, Debug, Serialize)]
pub struct Dialog {
    pub at_deadline: Vec<WindowEntry>,
    pub killed: Vec<ProcessRecord>,
    pub after_kill: Vec<WindowEntry>,
    pub escape_to: Vec<i64>,
    pub after_escape: Vec<WindowEntry>,
}

// --- запуск с ограниченным ожиданием ---

/// Запуск с выводом в файлы и ожиданием выхода до `deadline`. `None` — процесс ещё жив.
fn spawn_bounded(
    program: &Path,
    args: &[String],
    stdout: &Path,
    stderr: &Path,
    deadline: Instant,
) -> Result<(i32, Option<spawn::Exit>), String> {
    let out = File::create(stdout).map_err(|err| format!("{}: {err}", stdout.display()))?;
    let err = File::create(stderr).map_err(|err| format!("{}: {err}", stderr.display()))?;
    let pid = spawn::spawn(spawn::Spec {
        program,
        args,
        env: None,
        stdio: [None, Some(OwnedFd::from(out)), Some(OwnedFd::from(err))],
        setsid: false,
        disclaim: false,
    })
    .map_err(|err| format!("запуск {}: {err}", program.display()))?;
    let queue = Queue::new()?;
    if queue.watch_exit(pid)? {
        loop {
            match queue.wait(deadline)? {
                Event::Exited(exited) if exited == pid => break,
                Event::TimedOut => return Ok((pid, None)),
                _ => {}
            }
        }
    }
    let exit = spawn::wait(pid).map_err(|err| format!("waitpid {pid}: {err}"))?;
    Ok((pid, Some(exit)))
}

fn kill_and_reap(pid: i32) -> Option<spawn::Exit> {
    // SAFETY: сигнал собственному потомку по PID, памяти вызов не касается.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    spawn::wait(pid).ok()
}

// --- контекст стенда ---

struct Stand {
    state: PathBuf,
    out: PathBuf,
    nonce: String,
    keychain: PathBuf,
    runs: Vec<Run>,
    ops: Vec<OpRun>,
    /// Пароль временной keychain: только в аргументах `security`, в артефакты не попадает.
    password: String,
}

/// Запуск разовой операции.
#[derive(Clone, Debug, Serialize)]
pub struct OpRun {
    pub name: String,
    pub bundle: String,
    pub app: String,
    pub args: Vec<String>,
    pub open_exit: Option<String>,
    pub report: Option<OpReport>,
    pub dialog: Option<Dialog>,
    /// Почему операция не запускалась.
    pub skipped: Option<String>,
    pub elapsed_ms: u64,
}

impl Stand {
    fn kc_dir(&self) -> PathBuf {
        self.state.join("kc")
    }

    fn ops_dir(&self) -> PathBuf {
        self.out.join(DIR).join("ops")
    }

    /// `security` с ограниченным сроком; пароль в записанной команде заменён.
    fn security(&mut self, args: &[String]) -> Result<Run, String> {
        let kc = self.kc_dir();
        let (stdout, stderr) = (kc.join("security.stdout"), kc.join("security.stderr"));
        let deadline = Instant::now() + SECURITY_DEADLINE;
        let (pid, exit) = spawn_bounded(Path::new(SECURITY), args, &stdout, &stderr, deadline)?;
        let mut command = vec![SECURITY.to_owned()];
        command.extend(args.iter().map(|arg| {
            if arg == &self.password {
                "<пароль>".to_owned()
            } else {
                arg.clone()
            }
        }));
        let exit = match exit {
            Some(exit) => exit,
            None => {
                kill_and_reap(pid);
                return Err(format!(
                    "стенд: {} не вышел за {} с",
                    command.join(" "),
                    SECURITY_DEADLINE.as_secs()
                ));
            }
        };
        let read = |path: &Path| fs::read_to_string(path).unwrap_or_default();
        let run = Run {
            command,
            code: exit.code,
            stdout: read(&stdout),
            stderr: read(&stderr),
        };
        let _ = fs::remove_file(&stdout);
        let _ = fs::remove_file(&stderr);
        self.runs.push(run.clone());
        Ok(run)
    }

    fn security_ok(&mut self, args: &[&str]) -> Result<Run, String> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let run = self.security(&args)?;
        if run.code == Some(0) {
            Ok(run)
        } else {
            Err(format!(
                "стенд: {}: код {:?}; {}",
                run.command.join(" "),
                run.code,
                run.stderr.trim()
            ))
        }
    }

    fn snapshot(&mut self, when: &'static str) -> Result<EnvSnapshot, String> {
        let list = self.security_ok(&["list-keychains", "-d", "user"])?;
        let default = self.security_ok(&["default-keychain", "-d", "user"])?;
        let requirement = fs::read(REQUIREMENT).map_err(|err| format!("{REQUIREMENT}: {err}"))?;
        Ok(EnvSnapshot {
            when,
            search_list: keychain::parse_keychain_list(&list.stdout),
            default_keychain: keychain::parse_keychain_list(&default.stdout),
            requirement_sha256: keychain::sha256_hex(&requirement).unwrap_or_default(),
        })
    }

    fn dump(&mut self, name: &'static str) -> Result<Dump, String> {
        let path = self.keychain.display().to_string();
        let run = self.security_ok(&["dump-keychain", "-a", &path])?;
        let file = self.out.join(DUMPS_DIR).join(format!("{name}.txt"));
        fs::write(&file, &run.stdout).map_err(|err| format!("{}: {err}", file.display()))?;
        let items = keychain::parse_dump(&run.stdout)
            .map_err(|err| format!("стенд: разбор dump-keychain {name}: {err}"))?;
        Ok(Dump {
            name,
            n: keychain::find_item(&items, SERVICE, ACCOUNT_N).cloned(),
            denied: keychain::find_item(&items, SERVICE, ACCOUNT_DENIED).cloned(),
            items: items.len(),
        })
    }

    /// Операция через `open -n -W` с ограниченным сроком. Окно запроса на экране до операции —
    /// операция не запускается.
    fn op(
        &mut self,
        name: &str,
        entry: &BundleEntry,
        app: &Path,
        op: OpArgs,
    ) -> Result<OpRun, String> {
        let started = Instant::now();
        let mut run = OpRun {
            name: name.to_owned(),
            bundle: entry.name.clone(),
            app: app.display().to_string(),
            args: op.to_args(),
            open_exit: None,
            report: None,
            dialog: None,
            skipped: None,
            elapsed_ms: 0,
        };
        let before = dialogs_now();
        if !before.is_empty() {
            run.skipped = Some(format!(
                "окно запроса на экране до операции: {}",
                describe_windows(&before)
            ));
            self.ops.push(run.clone());
            return Ok(run);
        }
        let ops = self.ops_dir();
        let mut args = vec![
            "-n".to_owned(),
            "-W".to_owned(),
            "--stdout".to_owned(),
            ops.join(format!("{name}.app.stdout")).display().to_string(),
            "--stderr".to_owned(),
            ops.join(format!("{name}.app.stderr")).display().to_string(),
            app.display().to_string(),
            "--args".to_owned(),
        ];
        args.extend(op.to_args());
        let (pid, exit) = spawn_bounded(
            Path::new(OPEN),
            &args,
            &ops.join(format!("{name}.open.stdout")),
            &ops.join(format!("{name}.open.stderr")),
            started + OP_DEADLINE,
        )?;
        let exit = match exit {
            Some(exit) => Some(exit),
            None => {
                let at_deadline = dialogs_now();
                let killed = launch::find_by_token(&op.tag);
                for process in &killed {
                    // SAFETY: сигнал процессу операции, найденному по её метке.
                    unsafe { libc::kill(process.pid, libc::SIGKILL) };
                }
                let alive = launch::wait_exit(&[pid], KILL_WAIT);
                let exit = if alive.is_empty() {
                    spawn::wait(pid).ok()
                } else {
                    kill_and_reap(pid)
                };
                if at_deadline.is_empty() {
                    run.open_exit = exit.map(|exit| exit.to_string());
                    self.ops.push(run.clone());
                    return Err(format!(
                        "стенд: операция {name} не закончилась за {} с, окна запроса нет",
                        OP_DEADLINE.as_secs()
                    ));
                }
                let after_kill = dialogs_now();
                let mut escape_to: Vec<i64> = after_kill.iter().map(|window| window.pid).collect();
                escape_to.sort_unstable();
                escape_to.dedup();
                for pid in &escape_to {
                    press_escape(*pid as i32)?;
                }
                run.dialog = Some(Dialog {
                    at_deadline,
                    killed,
                    after_kill,
                    escape_to,
                    after_escape: dialogs_now(),
                });
                exit
            }
        };
        run.open_exit = exit.map(|exit| exit.to_string());
        if run.dialog.is_none() {
            run.report = Some(keychain::read_report(&op.out).map_err(|err| {
                format!(
                    "стенд: операция {name}: нет итога ({err}), open {:?}",
                    run.open_exit
                )
            })?);
        }
        run.elapsed_ms = started.elapsed().as_millis() as u64;
        self.ops.push(run.clone());
        Ok(run)
    }

    fn op_args(
        &self,
        name: &str,
        op: Op,
        account: &str,
        no_ui: NoUi,
        secret: Option<PathBuf>,
    ) -> OpArgs {
        OpArgs {
            op,
            keychain: self.keychain.clone(),
            account: account.to_owned(),
            no_ui,
            secret_file: secret,
            out: self.ops_dir().join(format!("{name}.json")),
            tag: format!("{}.kc-{name}", self.nonce),
            window: true,
        }
    }
}

/// Список поиска, keychain по умолчанию и `requirement.txt` в один момент клетки.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EnvSnapshot {
    pub when: &'static str,
    pub search_list: Vec<String>,
    pub default_keychain: Vec<String>,
    pub requirement_sha256: String,
}

/// Расхождения снимков с первым; пусто — окружение keychain прежнее.
pub fn env_changes(snapshots: &[EnvSnapshot]) -> Vec<String> {
    let Some(first) = snapshots.first() else {
        return vec!["нет снимков окружения".to_owned()];
    };
    let mut changes = Vec::new();
    for snapshot in &snapshots[1..] {
        if snapshot.search_list != first.search_list {
            changes.push(format!("список поиска keychain {}", snapshot.when));
        }
        if snapshot.default_keychain != first.default_keychain {
            changes.push(format!("keychain по умолчанию {}", snapshot.when));
        }
        if snapshot.requirement_sha256 != first.requirement_sha256 {
            changes.push(format!("scripts/signing/requirement.txt {}", snapshot.when));
        }
    }
    changes
}

/// Снимок `security dump-keychain -a`: item клетки.
#[derive(Clone, Debug, Serialize)]
pub struct Dump {
    pub name: &'static str,
    pub items: usize,
    pub n: Option<DumpItem>,
    pub denied: Option<DumpItem>,
}

// --- выводы частей ---

/// Что наблюдено в операции чтения.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seen {
    /// Значение прочитано без окна.
    Read { sha256: String },
    /// Отказ доступа или взаимодействия.
    Refused { status: i32 },
    /// Item не найден в поиске.
    NotFound { status: i32 },
    /// Появилось окно запроса.
    Dialog,
    /// Операция не запускалась.
    Skipped(String),
    /// Другая ошибка.
    Other(String),
}

impl Seen {
    pub fn describe(&self) -> String {
        match self {
            Seen::Read { .. } => "прочитано без окна".to_owned(),
            Seen::Refused { status } => format!("отказ {}", keychain::status_name(*status)),
            Seen::NotFound { status } => {
                format!(
                    "item не найден в поиске ({})",
                    keychain::status_name(*status)
                )
            }
            Seen::Dialog => "окно запроса".to_owned(),
            Seen::Skipped(reason) => format!("не запускалась: {reason}"),
            Seen::Other(reason) => reason.clone(),
        }
    }
}

/// Итог чтения по запуску операции: сначала атрибуты (виден ли item), затем данные.
pub fn seen(run: &OpRun) -> Seen {
    if let Some(reason) = &run.skipped {
        return Seen::Skipped(reason.clone());
    }
    if run.dialog.is_some() {
        return Seen::Dialog;
    }
    let Some(report) = &run.report else {
        return Seen::Other("нет итога операции".to_owned());
    };
    if let Some(error) = &report.error {
        return Seen::Other(error.clone());
    }
    let by_class = |status: i32| match keychain::classify(status) {
        Class::NotFound => Seen::NotFound { status },
        Class::Refused => Seen::Refused { status },
        _ => Seen::Other(keychain::status_name(status)),
    };
    match (report.attributes_status, report.data_status) {
        (Some(0), Some(0)) => match &report.value_sha256 {
            Some(sha256) => Seen::Read {
                sha256: sha256.clone(),
            },
            None => Seen::Other("errSecSuccess без значения".to_owned()),
        },
        (Some(0), Some(status)) => by_class(status),
        (Some(status), _) => by_class(status),
        (None, _) => Seen::Other("нет запроса атрибутов".to_owned()),
    }
}

/// Отрицательный контроль: первый способ с отказом без окна.
pub fn control_verdict(tries: &[(NoUi, Seen)]) -> (Option<NoUi>, Status, String) {
    let tried = tries
        .iter()
        .map(|(no_ui, seen)| format!("{}: {}", no_ui.name(), seen.describe()))
        .collect::<Vec<_>>()
        .join("; ");
    match tries
        .iter()
        .find(|(_, seen)| matches!(seen, Seen::Refused { .. }))
    {
        Some((no_ui, _)) => (
            Some(*no_ui),
            Status::Pass,
            format!(
                "способ запрета взаимодействия — {}: чтение item .denied процессом N отказано без окна ({tried})",
                no_ui.name()
            ),
        ),
        None => (
            None,
            Status::NotRun,
            format!("способ запрета взаимодействия не подтверждён контролем: {tried}"),
        ),
    }
}

/// Условия, без которых чтение N+1 не засчитывается.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextFacts {
    pub control: Option<NoUi>,
    pub version: Option<u32>,
    pub partitions: Option<Vec<String>>,
    pub expected_sha256: String,
}

pub fn next_verdict(facts: &NextFacts, seen: &Seen, evidence: &str) -> (Status, String) {
    if facts.control.is_none() {
        return (
            Status::NotRun,
            "без подтверждённого контролем способа запрета взаимодействия чтение N+1 не засчитывается".to_owned(),
        );
    }
    if facts.version != Some(PARTITIONED_VERSION) || facts.partitions.is_none() {
        return (
            Status::NotRun,
            format!(
                "keychain версии {:?} без partition list у item n: проверка partition ID не наблюдаема",
                facts.version
            ),
        );
    }
    match seen {
        Seen::Read { sha256 } if *sha256 == facts.expected_sha256 => (
            Status::Pass,
            format!("N+1 прочитал секрет N молча, SHA-256 совпадает; {evidence}"),
        ),
        Seen::Read { .. } => (
            Status::Fail,
            format!("N+1 прочитал другое значение (SHA-256 не совпадает); {evidence}"),
        ),
        Seen::Skipped(reason) => (
            Status::NotRun,
            format!("чтение N+1 не запускалось: {reason}"),
        ),
        other => (
            Status::Fail,
            format!(
                "N+1 не прочитал секрет N молча: {}; {evidence}",
                other.describe()
            ),
        ),
    }
}

pub fn foreign_verdict(control: Option<NoUi>, seen: &Seen) -> (Status, String) {
    match seen {
        Seen::Read { .. } => (Status::Fail, "foreign прочитал item n молча".to_owned()),
        Seen::NotFound { .. } => (
            Status::NotRun,
            format!("{}: отказ по подписи не наблюдён", seen.describe()),
        ),
        Seen::Skipped(reason) => (Status::NotRun, format!("не запускалось: {reason}")),
        Seen::Other(reason) => (Status::NotRun, format!("отказ не по доступу: {reason}")),
        Seen::Refused { .. } | Seen::Dialog if control.is_some() => (
            Status::Pass,
            format!("foreign не прочитал item n молча: {}", seen.describe()),
        ),
        Seen::Refused { .. } | Seen::Dialog => (
            Status::NotRun,
            format!(
                "{}, но способ запрета взаимодействия не подтверждён контролем",
                seen.describe()
            ),
        ),
    }
}

/// Item n во всех снимках тот же.
pub fn item_verdict(dumps: &[Dump]) -> (Status, String) {
    let Some(first) = dumps.first() else {
        return (Status::Fail, "нет снимков dump-keychain".to_owned());
    };
    let missing: Vec<&str> = dumps
        .iter()
        .filter(|dump| dump.n.is_none())
        .map(|dump| dump.name)
        .collect();
    if !missing.is_empty() {
        return (
            Status::Fail,
            format!("item n нет в снимках: {}", missing.join(", ")),
        );
    }
    let Some(base) = first.n.as_ref() else {
        return (Status::Fail, "item n нет в первом снимке".to_owned());
    };
    let changed: Vec<&str> = dumps
        .iter()
        .filter(|dump| {
            dump.n.as_ref().is_some_and(|item| {
                item.stored_access() != base.stored_access() || item.attributes != base.attributes
            })
        })
        .map(|dump| dump.name)
        .collect();
    // Отметка приложения ACL — проверка кода, который лежит по его пути в момент снимка: после
    // обмена на foreign она меняется, хотя item тот же.
    let marks = dumps
        .iter()
        .filter_map(|dump| {
            dump.n
                .as_ref()
                .map(|item| format!("{} [{}]", dump.name, item.app_marks().join(", ")))
        })
        .collect::<Vec<_>>()
        .join("; ");
    if !changed.is_empty() {
        return (
            Status::Fail,
            format!(
                "ACL, partition list или атрибуты item n изменились в снимках: {}",
                changed.join(", ")
            ),
        );
    }
    match base.partitions() {
        Some(partitions) => (
            Status::Pass,
            format!(
                "item n одинаков в {} снимках: записей ACL {}, partition list [{}], keychain версии {:?}; отметки проверки пути приложения ACL: {marks}",
                dumps.len(),
                base.access.len(),
                partitions.join(", "),
                base.version
            ),
        ),
        None => (
            Status::NotRun,
            format!(
                "ACL item n одинаков в {} снимках, но partition list не наблюдён (keychain версии {:?})",
                dumps.len(),
                base.version
            ),
        ),
    }
}

fn describe_windows(windows: &[WindowEntry]) -> String {
    windows
        .iter()
        .map(|window| format!("{} pid {} окно {}", window.owner, window.pid, window.number))
        .collect::<Vec<_>>()
        .join(", ")
}

// --- сценарий ---

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let (state, nonce, layout) = stand_inputs(environment)?;
    let home = environment
        .process
        .env
        .get("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or("предусловие: нет HOME")?;
    let dir = out.join(DIR);
    for sub in [dir.clone(), dir.join("ops"), out.join(DUMPS_DIR)] {
        fs::create_dir_all(&sub).map_err(|err| format!("{}: {err}", sub.display()))?;
    }
    let mut stand = Stand {
        state: state.clone(),
        out: out.to_path_buf(),
        keychain: keychain::keychain_path(&home, &nonce),
        nonce,
        runs: Vec::new(),
        ops: Vec::new(),
        password: format!("kc-{}", sys::random_id()),
    };
    fs::create_dir(stand.kc_dir()).map_err(|err| format!("{}: {err}", stand.kc_dir().display()))?;
    let mut snapshots = Vec::new();
    let mut dumps = Vec::new();
    let walked = walk(&mut stand, &layout, &mut snapshots, &mut dumps);
    let removed = remove_keychain(&mut stand);
    let after = stand.snapshot("после удаления keychain");
    if let Ok(snapshot) = &after {
        snapshots.push(snapshot.clone());
    }
    let lingering = dialogs_now();
    let _ = fs::remove_dir_all(stand.kc_dir());
    let changes = env_changes(&snapshots);
    let steps = json!({
        "keychain": stand.keychain.display().to_string(),
        "ops": stand.ops,
        "dumps": dumps,
        "security": stand.runs,
        "removed": removed,
        "lingering_dialogs": lingering,
        "walk_error": walked.as_ref().err(),
    });
    let written = crate::results::write_json(&dir, "steps.json", &steps).and_then(|_| {
        crate::results::write_json(
            &dir,
            "environment.json",
            &json!({ "snapshots": snapshots, "changes": changes }),
        )
    });
    let (cells, notes) = walked?;
    written?;
    removed?;
    after?;
    if !changes.is_empty() {
        return Err(format!(
            "стенд: временная keychain изменила окружение: {}",
            changes.join("; ")
        ));
    }
    if !lingering.is_empty() {
        return Err(format!(
            "стенд: окно запроса осталось на экране в конце сценария: {}",
            describe_windows(&lingering)
        ));
    }
    Ok(Report {
        cells,
        diagnostics: Diagnostics {
            notes,
            partial: None,
        },
    })
}

fn remove_keychain(stand: &mut Stand) -> Result<(), String> {
    if !stand.keychain.exists() {
        return Ok(());
    }
    let path = stand.keychain.display().to_string();
    let deleted = stand.security_ok(&["delete-keychain", &path]).map(|_| ());
    if stand.keychain.exists() {
        let _ = fs::remove_file(&stand.keychain);
        return Err(format!(
            "стенд: security delete-keychain не удалил {path}: {deleted:?}"
        ));
    }
    deleted
}

/// Значение item: случайные байты в файле с правами 0600 только на время записи.
fn secret_file(stand: &Stand, name: &str) -> Result<(PathBuf, String), String> {
    let value = format!("{}{}", sys::random_id(), sys::random_id());
    let path = stand.kc_dir().join(format!("secret-{name}"));
    fs::write(&path, value.as_bytes()).map_err(|err| format!("{}: {err}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|err| format!("{}: {err}", path.display()))?;
    Ok((
        path,
        keychain::sha256_hex(value.as_bytes()).unwrap_or_default(),
    ))
}

/// Операцию выполнил ожидаемый код с ожидаемого пути.
fn check_identity<'a>(
    run: &'a OpRun,
    entry: &BundleEntry,
    app: &Path,
) -> Result<&'a OpReport, String> {
    let report = run
        .report
        .as_ref()
        .ok_or_else(|| format!("стенд: операция {}: нет итога", run.name))?;
    let expected = fs::canonicalize(generation::Part::App.path(app))
        .map_err(|err| format!("{}: {err}", app.display()))?;
    let executable = report
        .executable
        .as_deref()
        .map(PathBuf::from)
        .and_then(|path| fs::canonicalize(path).ok());
    let mut problems = Vec::new();
    if report.cdhash.as_deref() != Some(entry.cdhash.as_str()) {
        problems.push(format!(
            "CDHash процесса {:?}, ожидается {} ({})",
            report.cdhash, entry.cdhash, entry.name
        ));
    }
    if report.generation != entry.generation {
        problems.push(format!(
            "поколение {}, ожидается {}",
            report.generation, entry.generation
        ));
    }
    if executable.as_ref() != Some(&expected) {
        problems.push(format!(
            "исполняемый файл {:?}, ожидается {}",
            report.executable,
            expected.display()
        ));
    }
    if !report.window {
        problems.push("операция без окна".to_owned());
    }
    if problems.is_empty() {
        Ok(report)
    } else {
        Err(format!(
            "стенд: операцию {} выполнил не тот код: {}",
            run.name,
            problems.join("; ")
        ))
    }
}

/// Запись item: `errSecSuccess` и ожидаемый код, иначе ошибка исполнения стенда.
fn add_item(
    stand: &mut Stand,
    name: &str,
    entry: &BundleEntry,
    app: &Path,
    account: &str,
) -> Result<String, String> {
    let (secret, sha256) = secret_file(stand, account)?;
    let args = stand.op_args(
        name,
        Op::Add,
        account,
        NoUi::ProcessFlag,
        Some(secret.clone()),
    );
    let run = stand.op(name, entry, app, args);
    let _ = fs::remove_file(&secret);
    let run = run?;
    if let Some(reason) = run.skipped.as_ref() {
        return Err(format!("стенд: запись {name} не запускалась: {reason}"));
    }
    if run.dialog.is_some() {
        return Err(format!("стенд: запись {name} показала окно запроса"));
    }
    let report = check_identity(&run, entry, app)?;
    if report.add_status != Some(0) || report.value_sha256.as_deref() != Some(sha256.as_str()) {
        return Err(format!(
            "стенд: запись {name}: {}; ошибка {:?}",
            report
                .add_status
                .map_or_else(|| "не выполнена".to_owned(), keychain::status_name),
            report.error
        ));
    }
    Ok(sha256)
}

fn read_item(
    stand: &mut Stand,
    name: &str,
    entry: &BundleEntry,
    app: &Path,
    account: &str,
    no_ui: NoUi,
) -> Result<(OpRun, Seen), String> {
    let args = stand.op_args(name, Op::Read, account, no_ui, None);
    let run = stand.op(name, entry, app, args)?;
    if run.report.is_some() {
        check_identity(&run, entry, app)?;
    }
    let seen = seen(&run);
    Ok((run, seen))
}

fn copy_bundle(entry: &BundleEntry, dest: &Path) -> Result<(), String> {
    install(&entry.app, dest)
}

type Walked = (Vec<Cell>, Vec<String>);

fn walk(
    stand: &mut Stand,
    layout: &Layout,
    snapshots: &mut Vec<EnvSnapshot>,
    dumps: &mut Vec<Dump>,
) -> Result<Walked, String> {
    let before = dialogs_now();
    if !before.is_empty() {
        return Err(format!(
            "предусловие: окно запроса на экране до клетки: {}",
            describe_windows(&before)
        ));
    }
    let (gen1, gen2, foreign, denied) = (
        layout.get("gen-1")?,
        layout.get("gen-2")?,
        layout.get("foreign")?,
        layout.get("denied")?,
    );
    let kc = stand.kc_dir();
    let installed = kc.join("install").join(generation::APP_NAME);
    let next = kc.join("next").join(generation::APP_NAME);
    let other = kc.join("foreign").join(generation::APP_NAME);
    let denied_app = kc.join("denied").join(
        denied
            .app
            .file_name()
            .ok_or("стенд: у бандла .denied нет имени")?,
    );
    copy_bundle(gen1, &installed)?;
    copy_bundle(gen2, &next)?;
    copy_bundle(foreign, &other)?;
    copy_bundle(denied, &denied_app)?;

    snapshots.push(stand.snapshot("до клетки")?);
    if stand.keychain.exists() {
        return Err(format!(
            "стенд: временная keychain {} уже есть",
            stand.keychain.display()
        ));
    }
    let path = stand.keychain.display().to_string();
    let password = stand.password.clone();
    stand.security_ok(&["create-keychain", "-p", &password, &path])?;
    stand.security_ok(&["unlock-keychain", "-p", &password, &path])?;
    stand.security_ok(&["set-keychain-settings", &path])?;
    snapshots.push(stand.snapshot("во время клетки")?);

    let started = Instant::now();
    add_item(stand, "denied-add", denied, &denied_app, ACCOUNT_DENIED)?;
    let expected = add_item(stand, "n-add", gen1, &installed, ACCOUNT_N)?;
    dumps.push(stand.dump("after-add")?);

    let mut tries = Vec::new();
    for no_ui in NoUi::ALL {
        let (_, seen) = read_item(
            stand,
            &format!("control-{}", no_ui.name()),
            gen1,
            &installed,
            ACCOUNT_DENIED,
            no_ui,
        )?;
        let stop = !matches!(seen, Seen::Dialog | Seen::Other(_));
        tries.push((no_ui, seen));
        if stop {
            break;
        }
    }
    let (method, control_status, control_reason) = control_verdict(&tries);

    let base_ui = method.unwrap_or(NoUi::ProcessFlag);
    let (_, own) = read_item(stand, "n-read", gen1, &installed, ACCOUNT_N, base_ui)?;
    if own
        != (Seen::Read {
            sha256: expected.clone(),
        })
    {
        return Err(format!(
            "стенд: базовая проверка: N не читает свой item ({}, способ {})",
            own.describe(),
            base_ui.name()
        ));
    }
    dumps.push(stand.dump("before-next")?);

    sys::swap_paths(&installed, &next).map_err(|err| format!("стенд: RENAME_SWAP N+1: {err}"))?;
    let read_ui = method.unwrap_or(NoUi::ProcessFlag);
    let (_, next_seen) = read_item(stand, "next-read", gen2, &installed, ACCOUNT_N, read_ui)?;
    dumps.push(stand.dump("after-next")?);

    sys::swap_paths(&installed, &other)
        .map_err(|err| format!("стенд: RENAME_SWAP foreign: {err}"))?;
    let (_, foreign_seen) = read_item(
        stand,
        "foreign-read",
        foreign,
        &installed,
        ACCOUNT_N,
        read_ui,
    )?;
    dumps.push(stand.dump("after-foreign")?);
    let elapsed = started.elapsed();

    let base_item = dumps.first().and_then(|dump| dump.n.clone());
    let facts = NextFacts {
        control: method,
        version: base_item.as_ref().and_then(|item| item.version),
        partitions: base_item.as_ref().and_then(DumpItem::partitions),
        expected_sha256: expected.clone(),
    };
    let evidence = next_evidence(stand, base_item.as_ref(), gen1, gen2)?;
    let (next_status, next_reason) = next_verdict(&facts, &next_seen, &evidence);
    let (foreign_status, foreign_reason) = foreign_verdict(method, &foreign_seen);
    let (item_status, item_reason) = item_verdict(dumps);

    // Итог операции есть, только если она дошла до конца: у пропущенной и снятой его нет.
    let ops_dir = stand.ops_dir();
    let ops = |names: &[String]| -> Vec<String> {
        names
            .iter()
            .filter(|name| ops_dir.join(format!("{name}.json")).is_file())
            .map(|name| format!("{DIR}/ops/{name}.json"))
            .collect()
    };
    let with = |extra: Vec<String>| -> Vec<String> {
        let mut all = vec![STEPS_ARTIFACT.to_owned()];
        all.extend(extra);
        all
    };
    let names =
        |list: &[&str]| -> Vec<String> { list.iter().map(|name| (*name).to_owned()).collect() };
    let mut control_names = names(&["denied-add"]);
    control_names.extend(
        tries
            .iter()
            .map(|(no_ui, _)| format!("control-{}", no_ui.name())),
    );
    let own_artifacts = with(ops(&names(&["n-add", "n-read"])));
    let control_artifacts = with(ops(&control_names));
    let next_artifacts = with(ops(&names(&["next-read"])));
    let foreign_artifacts = with(ops(&names(&["foreign-read"])));
    let item_artifacts = with(
        dumps
            .iter()
            .map(|dump| format!("{DUMPS_DIR}/{}.txt", dump.name))
            .collect(),
    );
    fn refs(list: &[String]) -> Vec<&str> {
        list.iter().map(String::as_str).collect()
    }
    let cells = vec![
        cell(
            &OWN,
            Status::Pass,
            format!(
                "N записал и прочитал свой item без окна способом {}",
                base_ui.name()
            ),
            &refs(&own_artifacts),
        ),
        cell(
            &CONTROL,
            control_status,
            control_reason,
            &refs(&control_artifacts),
        ),
        cell(&NEXT, next_status, next_reason, &refs(&next_artifacts)),
        cell(
            &FOREIGN,
            foreign_status,
            foreign_reason,
            &refs(&foreign_artifacts),
        ),
        cell(&ITEM, item_status, item_reason, &refs(&item_artifacts)),
    ];
    let notes = vec![
        format!(
            "временная keychain {} (версия {:?}); способ запрета взаимодействия — {}",
            stand.keychain.display(),
            facts.version,
            method.map_or("не подтверждён", NoUi::name)
        ),
        format!(
            "операций {}, от первой записи до последнего снимка {} мс; срок операции {} с",
            stand.ops.len(),
            elapsed.as_millis(),
            OP_DEADLINE.as_secs()
        ),
    ];
    Ok((cells, notes))
}

/// Доказательство к чтению N+1: partition list и ACL item n против CDHash и требования N+1.
fn next_evidence(
    stand: &mut Stand,
    item: Option<&DumpItem>,
    gen1: &BundleEntry,
    gen2: &BundleEntry,
) -> Result<String, String> {
    let Some(item) = item else {
        return Ok("item n в снимке нет".to_owned());
    };
    let partitions = item.partitions();
    let has = |cdhash: &str| {
        partitions
            .as_ref()
            .is_some_and(|ids| ids.iter().any(|id| id == &format!("cdhash:{cdhash}")))
    };
    let mark = |present: bool| if present { "есть" } else { "нет" };
    let mut parts = vec![format!(
        "partition list [{}]: cdhash N {} — {}, cdhash N+1 {} — {}",
        partitions
            .as_deref()
            .map_or_else(|| "нет".to_owned(), |ids| ids.join(", ")),
        gen1.cdhash,
        mark(has(&gen1.cdhash)),
        gen2.cdhash,
        mark(has(&gen2.cdhash)),
    )];
    for requirement in item.decrypt_requirements().unwrap_or_default() {
        let verdict = generation::verify(&gen2.app, &requirement, &mut stand.runs)?;
        parts.push(format!(
            "N+1 против требования ACL «{requirement}»: {}",
            verdict.name()
        ));
    }
    Ok(parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::{AclEntry, OpReport};
    use std::collections::BTreeMap;

    fn report(attributes: Option<i32>, data: Option<i32>, sha: Option<&str>) -> OpReport {
        OpReport {
            op: Op::Read,
            account: ACCOUNT_N.to_owned(),
            no_ui: NoUi::ProcessFlag,
            pid: 10,
            generation: 2,
            cdhash: Some("a".repeat(40)),
            executable: Some("/x".to_owned()),
            window: true,
            interaction_before: Some(true),
            interaction_after: Some(false),
            set_interaction_status: Some(0),
            open_status: 0,
            keychain_status: Some(7),
            add_status: None,
            attributes_status: attributes,
            data_status: data,
            value_sha256: sha.map(str::to_owned),
            value_len: sha.map(|_| 64),
            elapsed_ms: 5,
            error: None,
        }
    }

    fn op_run(report: Option<OpReport>) -> OpRun {
        OpRun {
            name: "next-read".to_owned(),
            bundle: "gen-2".to_owned(),
            app: "/s/kc/install/probe-update.app".to_owned(),
            args: Vec::new(),
            open_exit: Some("код 0".to_owned()),
            report,
            dialog: None,
            skipped: None,
            elapsed_ms: 100,
        }
    }

    fn dialog() -> Dialog {
        Dialog {
            at_deadline: vec![window("SecurityAgent")],
            killed: Vec::new(),
            after_kill: Vec::new(),
            escape_to: Vec::new(),
            after_escape: Vec::new(),
        }
    }

    fn window(owner: &str) -> WindowEntry {
        WindowEntry {
            number: 7,
            pid: 99,
            layer: 0,
            owner: owner.to_owned(),
            bounds: [0.0, 0.0, 10.0, 10.0],
        }
    }

    #[test]
    fn seen_separates_not_found_from_refusal() {
        let sha = "b".repeat(64);
        assert_eq!(
            seen(&op_run(Some(report(Some(0), Some(0), Some(&sha))))),
            Seen::Read { sha256: sha }
        );
        assert_eq!(
            seen(&op_run(Some(report(Some(0), Some(-25308), None)))),
            Seen::Refused { status: -25308 }
        );
        assert_eq!(
            seen(&op_run(Some(report(Some(-25300), Some(-25300), None)))),
            Seen::NotFound { status: -25300 }
        );
        assert!(matches!(
            seen(&op_run(Some(report(Some(0), Some(-50), None)))),
            Seen::Other(_)
        ));
        assert!(matches!(
            seen(&op_run(Some(report(Some(0), Some(0), None)))),
            Seen::Other(_)
        ));
        let mut with_dialog = op_run(None);
        with_dialog.dialog = Some(dialog());
        assert_eq!(seen(&with_dialog), Seen::Dialog);
        let mut skipped = op_run(None);
        skipped.skipped = Some("окно".to_owned());
        assert!(matches!(seen(&skipped), Seen::Skipped(_)));
        let mut failed = report(None, None, None);
        failed.error = Some("SecKeychainOpen: errSecNoSuchKeychain".to_owned());
        assert!(matches!(seen(&op_run(Some(failed))), Seen::Other(_)));
    }

    #[test]
    fn control_takes_first_silent_refusal() {
        let (method, status, _) =
            control_verdict(&[(NoUi::ProcessFlag, Seen::Refused { status: -25308 })]);
        assert_eq!((method, status), (Some(NoUi::ProcessFlag), Status::Pass));
        let (method, status, _) = control_verdict(&[
            (NoUi::ProcessFlag, Seen::Dialog),
            (NoUi::QueryFail, Seen::Refused { status: -25293 }),
        ]);
        assert_eq!((method, status), (Some(NoUi::QueryFail), Status::Pass));
        for spoiled in [
            Seen::Read {
                sha256: "x".to_owned(),
            },
            Seen::NotFound { status: -25300 },
            Seen::Dialog,
            Seen::Other("errSecParam".to_owned()),
        ] {
            let (method, status, reason) = control_verdict(&[(NoUi::ProcessFlag, spoiled)]);
            assert_eq!((method, status), (None, Status::NotRun));
            assert!(reason.contains("process-flag"), "{reason}");
        }
    }

    fn facts() -> NextFacts {
        NextFacts {
            control: Some(NoUi::ProcessFlag),
            version: Some(PARTITIONED_VERSION),
            partitions: Some(vec!["cdhash:aa".to_owned()]),
            expected_sha256: "s".repeat(64),
        }
    }

    #[test]
    fn next_read_counts_only_with_control_and_partitions() {
        let silent = Seen::Read {
            sha256: "s".repeat(64),
        };
        assert_eq!(next_verdict(&facts(), &silent, "e").0, Status::Pass);
        let mut no_control = facts();
        no_control.control = None;
        assert_eq!(next_verdict(&no_control, &silent, "e").0, Status::NotRun);
        let mut old = facts();
        old.version = Some(256);
        assert_eq!(next_verdict(&old, &silent, "e").0, Status::NotRun);
        let mut unpartitioned = facts();
        unpartitioned.partitions = None;
        assert_eq!(next_verdict(&unpartitioned, &silent, "e").0, Status::NotRun);
        for negative in [
            Seen::Read {
                sha256: "t".repeat(64),
            },
            Seen::Refused { status: -25308 },
            Seen::Dialog,
            Seen::NotFound { status: -25300 },
            Seen::Other("errSecParam".to_owned()),
        ] {
            assert_eq!(
                next_verdict(&facts(), &negative, "e").0,
                Status::Fail,
                "{negative:?}"
            );
        }
        assert_eq!(
            next_verdict(&facts(), &Seen::Skipped("окно".to_owned()), "e").0,
            Status::NotRun
        );
    }

    #[test]
    fn foreign_must_not_read_silently() {
        let control = Some(NoUi::ProcessFlag);
        assert_eq!(
            foreign_verdict(control, &Seen::Refused { status: -25308 }).0,
            Status::Pass
        );
        assert_eq!(foreign_verdict(control, &Seen::Dialog).0, Status::Pass);
        assert_eq!(
            foreign_verdict(
                control,
                &Seen::Read {
                    sha256: "s".to_owned()
                }
            )
            .0,
            Status::Fail
        );
        assert_eq!(
            foreign_verdict(
                None,
                &Seen::Read {
                    sha256: "s".to_owned()
                }
            )
            .0,
            Status::Fail
        );
        assert_eq!(
            foreign_verdict(None, &Seen::Refused { status: -25308 }).0,
            Status::NotRun
        );
        assert_eq!(
            foreign_verdict(control, &Seen::NotFound { status: -25300 }).0,
            Status::NotRun
        );
        assert_eq!(
            foreign_verdict(control, &Seen::Other("errSecParam".to_owned())).0,
            Status::NotRun
        );
    }

    fn item(partitions: Option<&str>, mdat: &str) -> DumpItem {
        let mut access = vec![AclEntry {
            index: 0,
            authorizations: vec!["decrypt".to_owned()],
            flags: vec!["don't-require-password".to_owned()],
            description: Some("n".to_owned()),
            applications: Some(Vec::new()),
        }];
        if let Some(ids) = partitions {
            access.push(AclEntry {
                index: 1,
                authorizations: vec!["partition_id".to_owned()],
                flags: Vec::new(),
                description: Some(ids.to_owned()),
                applications: None,
            });
        }
        DumpItem {
            keychain: "/k".to_owned(),
            version: Some(PARTITIONED_VERSION),
            class: Some("genp".to_owned()),
            attributes: BTreeMap::from([("mdat".to_owned(), mdat.to_owned())]),
            access,
            unparsed: Vec::new(),
        }
    }

    fn dump(name: &'static str, n: Option<DumpItem>) -> Dump {
        Dump {
            name,
            items: 2,
            n,
            denied: None,
        }
    }

    #[test]
    fn item_must_stay_the_same_with_partitions() {
        let same = [
            dump("a", Some(item(Some("cdhash:aa"), "1"))),
            dump("b", Some(item(Some("cdhash:aa"), "1"))),
        ];
        let (status, reason) = item_verdict(&same);
        assert_eq!(status, Status::Pass);
        assert!(reason.contains("cdhash:aa"), "{reason}");
        let changed = [
            dump("a", Some(item(Some("cdhash:aa"), "1"))),
            dump("b", Some(item(Some("cdhash:aa, cdhash:bb"), "1"))),
        ];
        assert_eq!(item_verdict(&changed).0, Status::Fail);
        let touched = [
            dump("a", Some(item(Some("cdhash:aa"), "1"))),
            dump("b", Some(item(Some("cdhash:aa"), "2"))),
        ];
        assert_eq!(item_verdict(&touched).0, Status::Fail);
        let mut marked = item(Some("cdhash:aa"), "1");
        marked.access[0].applications = Some(vec![keychain::AclApp {
            path: "/s/kc/install/probe-update.app".to_owned(),
            mark: Some("OK".to_owned()),
            requirement: Some("identifier \"x\"".to_owned()),
        }]);
        let mut remarked = marked.clone();
        if let Some(apps) = remarked.access[0].applications.as_mut() {
            apps[0].mark = Some("status -2147415734".to_owned());
        }
        let (status, reason) =
            item_verdict(&[dump("a", Some(marked.clone())), dump("b", Some(remarked))]);
        assert_eq!(status, Status::Pass, "отметка пути — не состояние item");
        assert!(reason.contains("status -2147415734"), "{reason}");
        let mut other_app = marked.clone();
        if let Some(apps) = other_app.access[0].applications.as_mut() {
            apps[0].requirement = Some("identifier \"y\"".to_owned());
        }
        assert_eq!(
            item_verdict(&[dump("a", Some(marked)), dump("b", Some(other_app))]).0,
            Status::Fail
        );
        let gone = [
            dump("a", Some(item(Some("cdhash:aa"), "1"))),
            dump("b", None),
        ];
        assert_eq!(item_verdict(&gone).0, Status::Fail);
        let unobserved = [
            dump("a", Some(item(None, "1"))),
            dump("b", Some(item(None, "1"))),
        ];
        assert_eq!(item_verdict(&unobserved).0, Status::NotRun);
        assert_eq!(item_verdict(&[]).0, Status::Fail);
    }

    #[test]
    fn environment_changes_name_the_moment() {
        let base = EnvSnapshot {
            when: "до клетки",
            search_list: vec!["/k/login.keychain-db".to_owned()],
            default_keychain: vec!["/k/login.keychain-db".to_owned()],
            requirement_sha256: "a".repeat(64),
        };
        assert!(env_changes(&[base.clone(), base.clone()]).is_empty());
        let mut during = base.clone();
        during.when = "во время клетки";
        during.search_list.push("/k/psu-f-1.keychain-db".to_owned());
        let mut after = base.clone();
        after.when = "после удаления keychain";
        after.requirement_sha256 = "b".repeat(64);
        after.default_keychain = vec!["/k/psu-f-1.keychain-db".to_owned()];
        assert_eq!(
            env_changes(&[base, during, after]),
            vec![
                "список поиска keychain во время клетки".to_owned(),
                "keychain по умолчанию после удаления keychain".to_owned(),
                "scripts/signing/requirement.txt после удаления keychain".to_owned(),
            ]
        );
        assert!(!env_changes(&[]).is_empty());
    }

    #[test]
    fn dialogs_are_security_agent_windows() {
        let windows = [
            window("Finder"),
            window("SecurityAgent"),
            window("coreautha"),
        ];
        assert_eq!(dialogs(&windows).len(), 2);
        assert!(dialogs(&[window("probe-update-app")]).is_empty());
    }

    #[test]
    fn observation_cells_have_no_required_parts() {
        let pass = cell(&NEXT, Status::Pass, "r".to_owned(), &[STEPS_ARTIFACT]);
        assert_eq!(pass.requirement, Requirement::Observation);
        assert!(pass.problems().is_empty(), "{:?}", pass.problems());
        let not_run = cell(&NEXT, Status::NotRun, "r".to_owned(), &[]);
        assert!(not_run.problems().is_empty(), "{:?}", not_run.problems());
    }
}
