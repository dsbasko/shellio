//! Клетка E, TCC (план P0.5, разделы «TCC» и «Предусловия»): фикстура в
//! `~/Documents/shellio-probe-update/`, защищённое чтение с атрибуцией, разовый режим
//! `probe-update-app --tcc`, запись шага выдачи владельца `tcc-grant.json` и её проверка, поиск окна
//! запроса TCC в списке окон.
//!
//! Защищённое чтение делают только бандл пробы (GUI) и нагрузка его хоста. Раннер, `run.sh` и
//! тесты путь фикстуры только вычисляют: процесс из терминала, тронувший `~/Documents`, вызвал бы
//! запрос TCC от имени терминала. Фикстуру создаёт разрешённый бандл в шаге выдачи (`prepare`).
//!
//! Атрибуция и отказ от ответственности — приватные функции libSystem
//! `responsibility_get_pid_responsible_for_pid` и `responsibility_spawnattrs_setdisclaim`. Они
//! ищутся `dlsym` при вызове: их отсутствие — `not_run` атрибуции или варианта, а не ошибка сборки.

use std::ffi::{CStr, c_void};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::generation::{self, BUNDLE_ID, DENIED_ID, Layout};
use crate::scenario::smoke::WindowEntry;
use crate::{EXIT_EXECUTION, EXIT_USAGE, PROBE_NAME, say};

/// Категория TCC клетки; другие категории не проверяются.
pub const SERVICE: &str = "SystemPolicyDocumentsFolder";
/// Каталог фикстуры относительно домашнего каталога.
pub const FIXTURE_DIR: &str = "Documents/shellio-probe-update";
pub const FIXTURE_FILE: &str = "fixture.txt";
pub const FIXTURE_TEXT: &str = "shellio probe-update-lifecycle TCC fixture\n";
/// Запись шага выдачи в `target/probes/update-lifecycle/`.
pub const GRANT_FILE: &str = "tcc-grant.json";
/// Журнал команд `tccutil` цели выдачи рядом с `tcc-grant.json`; его пишет `run.sh tcc-grant`.
pub const TCCUTIL_LOG: &str = "tccutil.log";
pub const GRANT_SCHEMA: u32 = 1;
/// Причина `not_run` клеток E без действительной выдачи (второй план, «Предусловия»).
pub const NO_GRANT: &str = "выдача владельца не выполнена или устарела";

/// Путь фикстуры в домашнем каталоге `home`.
pub fn fixture_path(home: &Path) -> PathBuf {
    home.join(FIXTURE_DIR).join(FIXTURE_FILE)
}

/// Идентификаторы, разрешения которых сбрасывает цель выдачи: бандлы обоих графов, их хосты и
/// нагрузки.
pub fn reset_identifiers() -> Vec<String> {
    [BUNDLE_ID, DENIED_ID]
        .into_iter()
        .flat_map(|base| {
            [
                base.to_owned(),
                format!("{base}.host"),
                format!("{base}.fixture"),
            ]
        })
        .collect()
}

// --- чтение ---

/// Итог защищённого чтения.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadOutcome {
    pub ok: bool,
    pub errno: Option<i32>,
    pub error: Option<String>,
    /// SHA-256 прочитанного содержимого.
    pub sha256: Option<String>,
}

impl ReadOutcome {
    pub fn from_result(result: io::Result<Vec<u8>>) -> ReadOutcome {
        match result {
            Ok(bytes) => ReadOutcome {
                ok: true,
                errno: None,
                error: None,
                sha256: crate::keychain::sha256_hex(&bytes),
            },
            Err(err) => ReadOutcome {
                ok: false,
                errno: err.raw_os_error(),
                error: Some(err.to_string()),
                sha256: None,
            },
        }
    }

    /// Отказ TCC: `EPERM`.
    pub fn denied(&self) -> bool {
        !self.ok && self.errno == Some(libc::EPERM)
    }

    /// Чтение удалось, и содержимое — текст фикстуры.
    pub fn read_fixture(&self) -> bool {
        self.ok && self.sha256 == crate::keychain::sha256_hex(FIXTURE_TEXT.as_bytes())
    }

    pub fn describe(&self) -> String {
        if self.ok {
            format!(
                "прочитано, sha256 {}",
                self.sha256.as_deref().unwrap_or("?")
            )
        } else {
            format!(
                "отказ errno {} ({})",
                self.errno.map_or("?".to_owned(), |errno| errno.to_string()),
                self.error.as_deref().unwrap_or("?")
            )
        }
    }
}

/// Защищённое чтение фикстуры.
pub fn read(path: &Path) -> ReadOutcome {
    ReadOutcome::from_result(fs::read(path))
}

/// Каталог и файл фикстуры, если их нет. Только для разрешённого бандла в шаге выдачи.
pub fn prepare(path: &Path) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{}: нет каталога", path.display()))?;
    fs::create_dir_all(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => io::Write::write_all(&mut file, FIXTURE_TEXT.as_bytes())
            .map_err(|err| format!("{}: {err}", path.display())),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

// --- атрибуция и disclaim ---

fn symbol(name: &CStr) -> Option<*mut c_void> {
    // SAFETY: имя — строка C; RTLD_DEFAULT ищет по всем загруженным образам.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!found.is_null()).then_some(found)
}

/// Ответственный процесс `pid` по `responsibility_get_pid_responsible_for_pid`.
pub fn responsible_pid(pid: i32) -> Result<i32, String> {
    let found = symbol(c"responsibility_get_pid_responsible_for_pid")
        .ok_or("нет функции responsibility_get_pid_responsible_for_pid")?;
    // SAFETY: сигнатура `pid_t responsibility_get_pid_responsible_for_pid(pid_t)`.
    let call: extern "C" fn(libc::pid_t) -> libc::pid_t = unsafe { std::mem::transmute(found) };
    match call(pid) {
        responsible if responsible > 0 => Ok(responsible),
        other => Err(format!(
            "responsibility_get_pid_responsible_for_pid({pid}) = {other}"
        )),
    }
}

/// Есть ли `responsibility_spawnattrs_setdisclaim`.
pub fn disclaim_available() -> bool {
    symbol(c"responsibility_spawnattrs_setdisclaim").is_some()
}

/// `responsibility_spawnattrs_setdisclaim(attr, 1)`.
///
/// # Safety
/// `attr` — инициализированный `posix_spawnattr_t`.
pub unsafe fn set_disclaim(attr: *mut libc::posix_spawnattr_t) -> io::Result<()> {
    let found = symbol(c"responsibility_spawnattrs_setdisclaim").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "нет функции responsibility_spawnattrs_setdisclaim",
        )
    })?;
    // SAFETY: сигнатура `int responsibility_spawnattrs_setdisclaim(posix_spawnattr_t *, int)`,
    // `attr` инициализирован по контракту функции.
    let call: extern "C" fn(*mut libc::posix_spawnattr_t, libc::c_int) -> libc::c_int =
        unsafe { std::mem::transmute(found) };
    match call(attr, 1) {
        0 => Ok(()),
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

// --- строка нагрузки ---

/// Ответ нагрузки на `read <путь>`: `tcc <seq> ok sha256=<…> responsible=<pid|->` или
/// `tcc <seq> error errno=<n> responsible=<pid|->`.
pub fn reply(seq: u64, outcome: &ReadOutcome, responsible: Option<i32>) -> String {
    let responsible = responsible.map_or("-".to_owned(), |pid| pid.to_string());
    if outcome.ok {
        format!(
            "tcc {seq} ok sha256={} responsible={responsible}",
            outcome.sha256.as_deref().unwrap_or("-")
        )
    } else {
        format!(
            "tcc {seq} error errno={} responsible={responsible}",
            outcome
                .errno
                .map_or("-".to_owned(), |errno| errno.to_string())
        )
    }
}

/// Разобранный ответ нагрузки.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reply {
    pub seq: u64,
    pub ok: bool,
    pub sha256: Option<String>,
    pub errno: Option<i32>,
    pub responsible: Option<i32>,
}

pub fn parse_reply(text: &str) -> Option<Reply> {
    let mut words = text.split_whitespace();
    if words.next()? != "tcc" {
        return None;
    }
    let seq = words.next()?.parse().ok()?;
    let ok = match words.next()? {
        "ok" => true,
        "error" => false,
        _ => return None,
    };
    let mut reply = Reply {
        seq,
        ok,
        sha256: None,
        errno: None,
        responsible: None,
    };
    for word in words {
        let (key, value) = word.split_once('=')?;
        match key {
            "sha256" if value != "-" => reply.sha256 = Some(value.to_owned()),
            "errno" if value != "-" => reply.errno = Some(value.parse().ok()?),
            "responsible" if value != "-" => reply.responsible = Some(value.parse().ok()?),
            "sha256" | "errno" | "responsible" => {}
            _ => return None,
        }
    }
    Some(reply)
}

// --- разовый режим probe-update-app --tcc ---

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// Создать фикстуру, если её нет, и прочитать: разрешённый бандл в шаге выдачи.
    Prepare,
    Read,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpArgs {
    pub op: Op,
    pub fixture: PathBuf,
    pub out: PathBuf,
    pub window: bool,
}

pub const USAGE: &str = "\
       probe-update-app --tcc prepare|read --fixture <файл> --out <отчёт.json> [--window]";

impl OpArgs {
    pub fn parse(args: &[String]) -> Result<OpArgs, String> {
        let (mut op, mut fixture, mut out, mut window) = (None, None, None, false);
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            if flag == "--window" {
                window = true;
                continue;
            }
            let value = rest.next().ok_or_else(|| format!("{flag}: нет значения"))?;
            let slot = match flag.as_str() {
                "--tcc" => &mut op,
                "--fixture" => &mut fixture,
                "--out" => &mut out,
                other => return Err(format!("--tcc: неизвестный аргумент «{other}»")),
            };
            if slot.replace(value.clone()).is_some() {
                return Err(format!("{flag} задан дважды"));
            }
        }
        let op = match op.as_deref() {
            Some("prepare") => Op::Prepare,
            Some("read") => Op::Read,
            other => return Err(format!("--tcc {other:?}: ожидается prepare или read")),
        };
        let absolute = |flag: &str, value: Option<String>| -> Result<PathBuf, String> {
            let path = PathBuf::from(value.ok_or_else(|| format!("нет {flag}"))?);
            if path.is_absolute() {
                Ok(path)
            } else {
                Err(format!("{flag} {}: нужен абсолютный путь", path.display()))
            }
        };
        Ok(OpArgs {
            op,
            fixture: absolute("--fixture", fixture)?,
            out: absolute("--out", out)?,
            window,
        })
    }
}

/// Отчёт разового режима.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpReport {
    pub op: Op,
    pub pid: i32,
    pub generation: u32,
    pub executable: Option<String>,
    pub cdhash: Option<String>,
    pub fixture: PathBuf,
    pub prepare_error: Option<String>,
    pub read: ReadOutcome,
    pub responsible: Option<i32>,
    pub responsible_error: Option<String>,
}

pub fn perform(op: &OpArgs) -> OpReport {
    let pid = std::process::id() as i32;
    let prepare_error = match op.op {
        Op::Prepare => prepare(&op.fixture).err(),
        Op::Read => None,
    };
    let read = read(&op.fixture);
    let (responsible, responsible_error) = match responsible_pid(pid) {
        Ok(responsible) => (Some(responsible), None),
        Err(reason) => (None, Some(reason)),
    };
    OpReport {
        op: op.op,
        pid,
        generation: crate::GENERATION,
        executable: crate::sys::process_path(pid)
            .ok()
            .map(|path| path.display().to_string()),
        cdhash: crate::sys::process_cdhash(pid).ok(),
        fixture: op.fixture.clone(),
        prepare_error,
        read,
        responsible,
        responsible_error,
    }
}

pub fn write_report(path: &Path, report: &OpReport) -> Result<(), String> {
    let text = serde_json::to_string_pretty(report).map_err(|err| format!("отчёт: {err}"))?;
    fs::write(path, text + "\n").map_err(|err| format!("{}: {err}", path.display()))
}

pub fn read_report(path: &Path) -> Result<OpReport, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))
}

// --- выдача владельца ---

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Бандл пробы: владелец нажал «Разрешить».
    Allowed,
    /// Бандл-контроль `.denied`: владелец нажал «Не разрешать».
    Denied,
}

impl Role {
    pub const ALL: [Role; 2] = [Role::Allowed, Role::Denied];

    pub fn name(self) -> &'static str {
        match self {
            Role::Allowed => "allowed",
            Role::Denied => "denied",
        }
    }

    /// Бандл раскладки, который открывает цель выдачи.
    pub fn bundle(self) -> &'static str {
        match self {
            Role::Allowed => "gen-1",
            Role::Denied => "denied",
        }
    }

    pub fn identifier(self) -> &'static str {
        match self {
            Role::Allowed => BUNDLE_ID,
            Role::Denied => DENIED_ID,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantBundle {
    pub role: Role,
    pub identifier: String,
    pub app: PathBuf,
    /// Designated requirement из `codesign -d -r-`: TCC сверяет его, а не CDHash.
    pub requirement: String,
    /// CDHash для сведения.
    pub cdhash: String,
    /// Код выхода `open -n -W`.
    pub open_code: Option<i32>,
    /// Отчёт бандла; `None` — бандл его не записал.
    pub report: Option<OpReport>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub schema: u32,
    pub service: String,
    pub created_ms: u64,
    pub fixture: PathBuf,
    pub bundles: Vec<GrantBundle>,
}

/// Текущая подпись бандла роли для сверки с записью выдачи.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Current {
    pub role: Role,
    pub identifier: String,
    pub requirement: String,
}

/// `tcc-grant.json`: `Ok(None)` — файла нет.
pub fn read_grant(path: &Path) -> Result<Option<Grant>, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|err| format!("{}: {err}", path.display())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

/// Действительна ли выдача: файл есть, designated requirement бандлов совпадают с текущими,
/// разрешённый бандл прочитал фикстуру, контроль получил `EPERM`. `Err` — причина `not_run`.
pub fn check_grant(grant: Option<&Grant>, current: &[Current]) -> Result<(), String> {
    let grant = grant.ok_or_else(|| format!("{NO_GRANT}: нет {GRANT_FILE}"))?;
    if grant.schema != GRANT_SCHEMA || grant.service != SERVICE {
        return Err(format!(
            "{NO_GRANT}: схема {} и категория {}, ожидаются {GRANT_SCHEMA} и {SERVICE}",
            grant.schema, grant.service
        ));
    }
    for role in Role::ALL {
        let recorded: Vec<&GrantBundle> = grant
            .bundles
            .iter()
            .filter(|bundle| bundle.role == role)
            .collect();
        let [recorded] = recorded[..] else {
            return Err(format!(
                "{NO_GRANT}: бандл {} записан {} раз",
                role.name(),
                recorded.len()
            ));
        };
        let now = current
            .iter()
            .find(|now| now.role == role)
            .ok_or_else(|| format!("{NO_GRANT}: нет текущей подписи {}", role.name()))?;
        if recorded.identifier != now.identifier || recorded.requirement != now.requirement {
            return Err(format!(
                "{NO_GRANT}: designated requirement {} изменился: записано «{}», сейчас «{}»",
                role.name(),
                recorded.requirement,
                now.requirement
            ));
        }
        let report = recorded
            .report
            .as_ref()
            .ok_or_else(|| format!("{NO_GRANT}: бандл {} не записал отчёт чтения", role.name()))?;
        let expected = match role {
            Role::Allowed => report.read.read_fixture(),
            Role::Denied => report.read.denied(),
        };
        if !expected {
            return Err(format!(
                "{NO_GRANT}: наблюдённый результат выдачи {}: {}, ожидается {}",
                role.name(),
                report.read.describe(),
                match role {
                    Role::Allowed => "чтение фикстуры",
                    Role::Denied => "EPERM",
                }
            ));
        }
    }
    Ok(())
}

/// Текущие подписи бандлов ролей по `codesign -d -r-`.
pub fn current_signatures(
    layout: &Layout,
    runs: &mut Vec<generation::Run>,
) -> Result<Vec<Current>, String> {
    Role::ALL
        .into_iter()
        .map(|role| {
            let entry = layout.get(role.bundle())?;
            let requirement = generation::designated(&entry.app, runs)?
                .ok_or_else(|| format!("нет designated requirement у {}", entry.app.display()))?;
            Ok(Current {
                role,
                identifier: role.identifier().to_owned(),
                requirement,
            })
        })
        .collect()
}

// --- окно запроса TCC ---

/// Окна, появившиеся после снимка `before` у процессов вне `own`: кандидаты окна запроса TCC.
/// Имя процесса окна не закрепляется: на macOS 27 его не наблюдали до шага владельца.
pub fn new_windows(before: &[WindowEntry], after: &[WindowEntry], own: &[i64]) -> Vec<WindowEntry> {
    after
        .iter()
        .filter(|window| {
            !own.contains(&window.pid)
                && !before
                    .iter()
                    .any(|seen| seen.number == window.number && seen.pid == window.pid)
        })
        .cloned()
        .collect()
}

// --- цель выдачи: probe-update --tcc-grant ---

const OPEN: &str = "/usr/bin/open";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Аргументы `open` разового режима бандла `app`.
pub fn open_args(app: &Path, op: Op, fixture: &Path, out: &Path) -> Vec<String> {
    vec![
        "-n".to_owned(),
        "-W".to_owned(),
        app.display().to_string(),
        "--args".to_owned(),
        "--tcc".to_owned(),
        match op {
            Op::Prepare => "prepare",
            Op::Read => "read",
        }
        .to_owned(),
        "--fixture".to_owned(),
        fixture.display().to_string(),
        "--out".to_owned(),
        out.display().to_string(),
        "--window".to_owned(),
    ]
}

/// Шаг выдачи владельца: `probe-update --tcc-grant <каталог бандлов> <tcc-grant.json>`. Открывает
/// разрешённый бандл (`prepare`), затем контроль (`read`) через `open -n -W` и пишет запись
/// выдачи. `tccutil` здесь не вызывается: сброс до шага делает `run.sh tcc-grant` со своим
/// журналом. Ненулевой код — только если запись не сделана; итог выдачи проверяет `tcc`.
pub fn grant_main(args: &[String]) -> ExitCode {
    let [bundles, out] = args else {
        say(&format!(
            "{PROBE_NAME}: --tcc-grant <каталог бандлов> <tcc-grant.json>"
        ));
        return ExitCode::from(EXIT_USAGE);
    };
    match grant(Path::new(bundles), Path::new(out)) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            say(&format!("{PROBE_NAME}: --tcc-grant: {reason}"));
            ExitCode::from(EXIT_EXECUTION)
        }
    }
}

fn grant(bundles: &Path, out: &Path) -> Result<String, String> {
    let home = std::env::var_os("HOME").ok_or("нет HOME")?;
    let fixture = fixture_path(Path::new(&home));
    let layout = Layout::read(bundles)?;
    let dir = out
        .parent()
        .ok_or_else(|| format!("{}: нет каталога", out.display()))?;
    let work = dir.join(format!(".tcc-grant-{}", std::process::id()));
    fs::create_dir_all(&work).map_err(|err| format!("{}: {err}", work.display()))?;
    let mut runs = Vec::new();
    let mut recorded = Vec::new();
    let mut lines = Vec::new();
    for role in Role::ALL {
        let entry = layout.get(role.bundle())?;
        let requirement = generation::designated(&entry.app, &mut runs)?
            .ok_or_else(|| format!("нет designated requirement у {}", entry.app.display()))?;
        let report_path = work.join(format!("{}.json", role.name()));
        let op = match role {
            Role::Allowed => Op::Prepare,
            Role::Denied => Op::Read,
        };
        let opened = crate::spawn::output(
            Path::new(OPEN),
            &open_args(&entry.app, op, &fixture, &report_path),
        )
        .map_err(|err| format!("open: {err}"))?;
        let report = read_report(&report_path).ok();
        lines.push(format!(
            "{}: {} — {}",
            role.name(),
            entry.app.display(),
            report
                .as_ref()
                .map_or("отчёта нет".to_owned(), |report| report
                    .read
                    .describe())
        ));
        recorded.push(GrantBundle {
            role,
            identifier: role.identifier().to_owned(),
            app: entry.app.clone(),
            requirement,
            cdhash: entry.cdhash.clone(),
            open_code: opened.exit.code,
            report,
        });
    }
    let _ = fs::remove_dir_all(&work);
    let grant = Grant {
        schema: GRANT_SCHEMA,
        service: SERVICE.to_owned(),
        created_ms: now_ms(),
        fixture,
        bundles: recorded,
    };
    let text = serde_json::to_string_pretty(&grant).map_err(|err| format!("запись: {err}"))?;
    fs::write(out, text + "\n").map_err(|err| format!("{}: {err}", out.display()))?;
    let verdict = check_grant(Some(&grant), &current_from(&grant))
        .map_or_else(|reason| reason, |()| "выдача действительна".to_owned());
    lines.push(format!("{}: {verdict}", out.display()));
    Ok(lines.join("\n"))
}

/// Подписи из самой записи: проверка наблюдённых результатов сразу после шага.
fn current_from(grant: &Grant) -> Vec<Current> {
    grant
        .bundles
        .iter()
        .map(|bundle| Current {
            role: bundle.role,
            identifier: bundle.identifier.clone(),
            requirement: bundle.requirement.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REQ: &str = "identifier \"io.github.dsbasko.shellio.probe.update-lifecycle\" and certificate leaf = H\"0123456789abcdef0123456789abcdef01234567\"";
    const REQ_DENIED: &str = "identifier \"io.github.dsbasko.shellio.probe.update-lifecycle.denied\" and certificate leaf = H\"0123456789abcdef0123456789abcdef01234567\"";

    fn ok_read() -> ReadOutcome {
        ReadOutcome::from_result(Ok(FIXTURE_TEXT.as_bytes().to_vec()))
    }

    fn eperm() -> ReadOutcome {
        ReadOutcome::from_result(Err(io::Error::from_raw_os_error(libc::EPERM)))
    }

    fn report(read: ReadOutcome) -> OpReport {
        OpReport {
            op: Op::Read,
            pid: 10,
            generation: 1,
            executable: None,
            cdhash: None,
            fixture: PathBuf::from("/Users/u/Documents/shellio-probe-update/fixture.txt"),
            prepare_error: None,
            read,
            responsible: Some(10),
            responsible_error: None,
        }
    }

    fn bundle(role: Role, requirement: &str, read: Option<ReadOutcome>) -> GrantBundle {
        GrantBundle {
            role,
            identifier: role.identifier().to_owned(),
            app: PathBuf::from("/tmp/bundle/x.app"),
            requirement: requirement.to_owned(),
            cdhash: "0".repeat(40),
            open_code: Some(0),
            report: read.map(report),
        }
    }

    fn grant(allowed: Option<ReadOutcome>, denied: Option<ReadOutcome>) -> Grant {
        Grant {
            schema: GRANT_SCHEMA,
            service: SERVICE.to_owned(),
            created_ms: 1,
            fixture: PathBuf::from("/Users/u/Documents/shellio-probe-update/fixture.txt"),
            bundles: vec![
                bundle(Role::Allowed, REQ, allowed),
                bundle(Role::Denied, REQ_DENIED, denied),
            ],
        }
    }

    fn current() -> Vec<Current> {
        vec![
            Current {
                role: Role::Allowed,
                identifier: BUNDLE_ID.to_owned(),
                requirement: REQ.to_owned(),
            },
            Current {
                role: Role::Denied,
                identifier: DENIED_ID.to_owned(),
                requirement: REQ_DENIED.to_owned(),
            },
        ]
    }

    #[test]
    fn missing_grant_is_not_valid() {
        let reason = check_grant(None, &current()).unwrap_err();
        assert!(reason.starts_with(NO_GRANT));
        assert!(reason.contains(GRANT_FILE));
        let dir = std::env::temp_dir().join(format!("psu-tcc-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_grant(&dir.join(GRANT_FILE)), Ok(None));
        fs::write(dir.join(GRANT_FILE), "{").unwrap();
        assert!(read_grant(&dir.join(GRANT_FILE)).is_err());
        let valid = grant(Some(ok_read()), Some(eperm()));
        fs::write(dir.join(GRANT_FILE), serde_json::to_string(&valid).unwrap()).unwrap();
        assert_eq!(read_grant(&dir.join(GRANT_FILE)), Ok(Some(valid)));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn valid_grant_matches_requirements_and_observations() {
        assert_eq!(
            check_grant(Some(&grant(Some(ok_read()), Some(eperm()))), &current()),
            Ok(())
        );
    }

    #[test]
    fn changed_designated_requirement_makes_grant_stale() {
        let mut now = current();
        now[1].requirement = REQ_DENIED.replace("0123456789abcdef", "fedcba9876543210");
        let reason = check_grant(Some(&grant(Some(ok_read()), Some(eperm()))), &now).unwrap_err();
        assert!(reason.contains("designated requirement denied"), "{reason}");
        let mut twice = grant(Some(ok_read()), Some(eperm()));
        twice.bundles.push(twice.bundles[0].clone());
        assert!(
            check_grant(Some(&twice), &current())
                .unwrap_err()
                .contains("записан 2 раз")
        );
    }

    #[test]
    fn observed_results_must_match_owner_answers() {
        let both_read = check_grant(Some(&grant(Some(ok_read()), Some(ok_read()))), &current());
        assert!(both_read.unwrap_err().contains("ожидается EPERM"));
        let allowed_denied = check_grant(Some(&grant(Some(eperm()), Some(eperm()))), &current());
        assert!(
            allowed_denied
                .unwrap_err()
                .contains("ожидается чтение фикстуры")
        );
        let wrong_text = ReadOutcome::from_result(Ok(b"other".to_vec()));
        assert!(check_grant(Some(&grant(Some(wrong_text), Some(eperm()))), &current()).is_err());
        let enoent = ReadOutcome::from_result(Err(io::Error::from_raw_os_error(libc::ENOENT)));
        assert!(check_grant(Some(&grant(Some(ok_read()), Some(enoent))), &current()).is_err());
        let no_report = check_grant(Some(&grant(None, Some(eperm()))), &current());
        assert!(no_report.unwrap_err().contains("не записал отчёт"));
    }

    #[test]
    fn replies_round_trip() {
        let ok = reply(3, &ok_read(), Some(42));
        let parsed = parse_reply(&ok).unwrap();
        assert!(parsed.ok);
        assert_eq!(parsed.seq, 3);
        assert_eq!(parsed.responsible, Some(42));
        assert_eq!(parsed.sha256, ok_read().sha256);
        let denied = parse_reply(&reply(4, &eperm(), None)).unwrap();
        assert_eq!(
            denied,
            Reply {
                seq: 4,
                ok: false,
                sha256: None,
                errno: Some(libc::EPERM),
                responsible: None,
            }
        );
        assert_eq!(parse_reply("ack 1 ping"), None);
        assert_eq!(parse_reply("tcc 1 maybe"), None);
        assert_eq!(parse_reply("tcc 1 ok extra=1"), None);
    }

    #[test]
    fn op_args_parse() {
        let args: Vec<String> = [
            "--tcc",
            "prepare",
            "--fixture",
            "/Users/u/Documents/shellio-probe-update/fixture.txt",
            "--out",
            "/tmp/r.json",
            "--window",
        ]
        .map(str::to_owned)
        .to_vec();
        let op = OpArgs::parse(&args).unwrap();
        assert_eq!(op.op, Op::Prepare);
        assert!(op.window);
        assert_eq!(
            open_args(Path::new("/tmp/a.app"), op.op, &op.fixture, &op.out)[4..],
            args[..]
        );
        assert!(OpArgs::parse(&args[..4]).is_err());
        let mut relative = args.clone();
        relative[5] = "r.json".to_owned();
        assert!(OpArgs::parse(&relative).is_err());
        let mut bad = args.clone();
        bad[1] = "write".to_owned();
        assert!(OpArgs::parse(&bad).is_err());
    }

    #[test]
    fn fixture_paths_and_reset_list() {
        assert_eq!(
            fixture_path(Path::new("/Users/u")),
            PathBuf::from("/Users/u/Documents/shellio-probe-update/fixture.txt")
        );
        let ids = reset_identifiers();
        assert_eq!(ids.len(), 6);
        assert!(ids.contains(&format!("{DENIED_ID}.fixture")));
        assert!(ids.contains(&format!("{BUNDLE_ID}.host")));
    }

    #[test]
    fn new_windows_ignore_known_and_own() {
        let window = |number, pid| WindowEntry {
            number,
            pid,
            layer: 0,
            owner: "x".to_owned(),
            bounds: [0.0; 4],
        };
        let before = [window(1, 100), window(2, 200)];
        let after = [
            window(1, 100),
            window(2, 200),
            window(3, 300),
            window(4, 400),
        ];
        let found = new_windows(&before, &after, &[400]);
        assert_eq!(found, [window(3, 300)]);
    }

    #[test]
    fn prepare_creates_fixture_once() {
        let dir = std::env::temp_dir().join(format!("psu-tcc-prep-{}", std::process::id()));
        let path = dir.join("sub").join(FIXTURE_FILE);
        prepare(&path).unwrap();
        assert!(read(&path).read_fixture());
        fs::write(&path, "kept").unwrap();
        prepare(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "kept");
        let missing = read(&dir.join("none"));
        assert!(!missing.ok && !missing.denied());
        fs::remove_dir_all(&dir).unwrap();
    }
}
