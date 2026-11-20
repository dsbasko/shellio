//! Фикстуры процессов: режим `--fixture <режим>` того же исполняемого файла (план P0.8,
//! «Solution Overview», «Фикстуры»).
//!
//! Фикстура подключается к сокету стенда по пути из `--channel` и шлёт события
//! ([`receipts::Event`]), каждое сначала в свой файл в каталоге `--out`. Долгоживущая фикстура
//! сообщает `ready` и ждёт команд: квитанции на границе с меткой и выхода. Она выходит и сама —
//! по закрытию канала и по собственному сроку `--lifetime-s`, потому что программа хоста
//! (своя сессия), сирота двойного `fork` и ребёнок `login` уходят из группы процессов стенда и
//! сигнал группе до них не доходит. Дети получают остаток срока родителя.
//!
//! Режимы ([`Mode`]): `idle`; `load --percent P` — нагрузка с фиксированным темпом, один рабочий
//! поток до 100% одного ядра, два выше; `memory --mib N` — выделение с касанием страниц;
//! `tree --children C --grandchildren G [--double-fork K]`; `quick-exit`; `double-fork` —
//! промежуточный процесс, который запускает сироту `idle` и выходит без ожидания; `host
//! --program tree|login` — программа запускается лидером своей сессии и группы (`setsid` до
//! `exec`), хост шлёт запись владения; `login-child` — ребёнок `/usr/bin/login`, запущенного
//! командной строкой поверхности Ghostty ([`login_argv`]).
//!
//! Дети запускаются тем же исполняемым файлом через `spawn`, а не сырым `fork` многопоточного
//! процесса. Ключ ребёнка читается после `spawn` и до `waitpid`, родитель собирает каждого
//! ребёнка, кроме сироты.

pub mod bench;
pub mod receipts;

use std::ffi::CStr;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::attrib::OwnerRecord;
use crate::background::uptime_ns;
use crate::proc;
use crate::snapshot::Key;
use receipts::{
    Boundary, Command, Event, ExitReason, FoundKey, KeySource, Log, Mode, Process as ProcessInfo,
    Receipt, Request, Role, Rusage, Spawned, WaitStatus,
};

pub const EXIT_USAGE: i32 = 2;
pub const DEFAULT_LIFETIME: Duration = Duration::from_secs(1800);
pub const MAX_LIFETIME: Duration = Duration::from_secs(3600);
/// Период темпа нагрузки.
pub const PACE_PERIOD: Duration = Duration::from_millis(10);
pub const MAX_PERCENT: f64 = 200.0;
pub const MAX_MIB: u64 = 4096;
pub const MAX_CHILDREN: u32 = 64;
pub const LOGIN: &str = "/usr/bin/login";
/// Шагов `xorshift` между чтениями часов потока в занятой фазе.
const BUSY_STEPS: u32 = 512;

/// Программа хоста.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Program {
    Tree,
    Login,
}

impl Program {
    pub fn name(self) -> &'static str {
        match self {
            Program::Tree => "tree",
            Program::Login => "login",
        }
    }

    pub fn from_name(name: &str) -> Option<Program> {
        [Program::Tree, Program::Login]
            .into_iter()
            .find(|program| program.name() == name)
    }
}

/// Что запустить: режим, имя и параметры режима.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub mode: Mode,
    /// Имя фикстуры: `[A-Za-z0-9._/-]`, дети получают `<имя>/<суффикс>`.
    pub name: String,
    pub percent: Option<f64>,
    pub mib: Option<u64>,
    pub children: u32,
    pub grandchildren: u32,
    pub double_fork: u32,
    pub program: Program,
}

impl Spec {
    pub fn new(mode: Mode, name: impl Into<String>) -> Spec {
        Spec {
            mode,
            name: name.into(),
            percent: None,
            mib: None,
            children: 0,
            grandchildren: 0,
            double_fork: 0,
            program: Program::Tree,
        }
    }

    /// Аргументы после исполняемого файла.
    pub fn argv(&self, channel: &str, out: &str, lifetime: Duration) -> Vec<String> {
        let mut argv = vec![
            "--fixture".to_owned(),
            self.mode.name().to_owned(),
            "--channel".to_owned(),
            channel.to_owned(),
            "--name".to_owned(),
            self.name.clone(),
            "--out".to_owned(),
            out.to_owned(),
            "--lifetime-s".to_owned(),
            lifetime.as_secs().max(1).to_string(),
        ];
        let mut push = |flag: &str, value: String| {
            argv.push(flag.to_owned());
            argv.push(value);
        };
        if let Some(percent) = self.percent {
            push("--percent", percent.to_string());
        }
        if let Some(mib) = self.mib {
            push("--mib", mib.to_string());
        }
        if self.children > 0 {
            push("--children", self.children.to_string());
        }
        if self.grandchildren > 0 {
            push("--grandchildren", self.grandchildren.to_string());
        }
        if self.double_fork > 0 {
            push("--double-fork", self.double_fork.to_string());
        }
        if self.mode == Mode::Host {
            push("--program", self.program.name().to_owned());
        }
        argv
    }

    fn validate(&self) -> Result<(), String> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        {
            return Err(format!(
                "имя «{}»: нужно непустое из [A-Za-z0-9._/-]",
                self.name
            ));
        }
        match (self.mode, self.percent) {
            (Mode::Load, None) => return Err("load: нужен --percent".to_owned()),
            (Mode::Load, Some(percent)) => {
                Pace::new(percent)?;
            }
            (_, Some(_)) => return Err("--percent только у load".to_owned()),
            (_, None) => {}
        }
        match (self.mode, self.mib) {
            (Mode::Memory, None) => return Err("memory: нужен --mib".to_owned()),
            (Mode::Memory, Some(mib)) if mib == 0 || mib > MAX_MIB => {
                return Err(format!("--mib {mib}: нужно 1..={MAX_MIB}"));
            }
            (Mode::Memory, Some(_)) | (_, None) => {}
            (_, Some(_)) => return Err("--mib только у memory".to_owned()),
        }
        let tree_like = matches!(self.mode, Mode::Tree | Mode::Host);
        for (flag, value) in [
            ("--children", self.children),
            ("--grandchildren", self.grandchildren),
            ("--double-fork", self.double_fork),
        ] {
            if value > MAX_CHILDREN {
                return Err(format!("{flag} {value}: больше {MAX_CHILDREN}"));
            }
            if value > 0 && !tree_like {
                return Err(format!("{flag} только у tree и host"));
            }
        }
        if self.mode == Mode::Host
            && self.program == Program::Login
            && (self.children > 0 || self.grandchildren > 0 || self.double_fork > 0)
        {
            return Err("host --program login: дерево под login не задаётся".to_owned());
        }
        Ok(())
    }
}

/// Аргументы процесса фикстуры.
#[derive(Clone, Debug, PartialEq)]
pub struct Args {
    pub spec: Spec,
    pub channel: String,
    pub out: String,
    pub lifetime: Duration,
}

/// Разбор аргументов после `--fixture`: первым идёт режим.
pub fn parse(mut rest: Vec<String>) -> Result<Args, String> {
    if rest.is_empty() || rest[0].starts_with("--") {
        return Err("--fixture: нужен режим".to_owned());
    }
    let mode_name = rest.remove(0);
    let mode = Mode::from_name(&mode_name)
        .ok_or_else(|| format!("неизвестный режим фикстуры «{mode_name}»"))?;
    let mut spec = Spec::new(mode, String::new());
    let mut channel = None;
    let mut out = None;
    let mut lifetime = DEFAULT_LIFETIME;
    let mut program = None;
    let mut iter = rest.into_iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .ok_or_else(|| format!("{flag}: нужно значение"))?;
        let number = |value: &str| -> Result<u64, String> {
            value
                .parse::<u64>()
                .map_err(|_| format!("{flag} «{value}»: нужно целое"))
        };
        let count = |value: &str| -> Result<u32, String> {
            u32::try_from(number(value)?).map_err(|_| format!("{flag} «{value}»: велико"))
        };
        match flag.as_str() {
            "--channel" => channel = Some(value),
            "--name" => spec.name = value,
            "--out" => out = Some(value),
            "--lifetime-s" => {
                let secs = number(&value)?;
                if secs == 0 || secs > MAX_LIFETIME.as_secs() {
                    return Err(format!(
                        "--lifetime-s {secs}: нужно 1..={}",
                        MAX_LIFETIME.as_secs()
                    ));
                }
                lifetime = Duration::from_secs(secs);
            }
            "--percent" => {
                let percent: f64 = value
                    .parse()
                    .map_err(|_| format!("--percent «{value}»: нужно число"))?;
                spec.percent = Some(percent);
            }
            "--mib" => spec.mib = Some(number(&value)?),
            "--children" => spec.children = count(&value)?,
            "--grandchildren" => spec.grandchildren = count(&value)?,
            "--double-fork" => spec.double_fork = count(&value)?,
            "--program" => {
                program = Some(
                    Program::from_name(&value)
                        .ok_or_else(|| format!("--program «{value}»: нужно tree или login"))?,
                );
            }
            other => return Err(format!("неизвестный флаг фикстуры «{other}»")),
        }
    }
    match (mode, program) {
        (Mode::Host, Some(program)) => spec.program = program,
        (Mode::Host, None) => return Err("host: нужен --program".to_owned()),
        (_, Some(_)) => return Err("--program только у host".to_owned()),
        (_, None) => {}
    }
    spec.validate()?;
    Ok(Args {
        spec,
        channel: channel.ok_or("нужен --channel")?,
        out: out.ok_or("нужен --out")?,
        lifetime,
    })
}

/// Темп нагрузки: число рабочих потоков и доля ядра на поток.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pace {
    pub workers: u32,
    /// Доля одного ядра на поток, `(0, 1]`.
    pub share: f64,
}

impl Pace {
    /// Один поток до 100% одного ядра, два выше; предел — 200%.
    pub fn new(percent: f64) -> Result<Pace, String> {
        if !percent.is_finite() || percent <= 0.0 || percent > MAX_PERCENT {
            return Err(format!("--percent {percent}: нужно (0, {MAX_PERCENT}]"));
        }
        let workers = if percent <= 100.0 { 1 } else { 2 };
        Ok(Pace {
            workers,
            share: percent / 100.0 / f64::from(workers),
        })
    }

    /// CPU потока на период, нс.
    pub fn quota_ns(self, period_ns: u64) -> u64 {
        (self.share * period_ns as f64).round() as u64
    }
}

/// Номер следующего периода после периода `index`: период, идущий в момент `elapsed_ns`,
/// доигрывается без ожидания, пропущенные целиком не догоняются, расписание не сдвигается.
pub fn next_period(index: u64, elapsed_ns: u64, period_ns: u64) -> u64 {
    (elapsed_ns / period_ns.max(1)).max(index + 1)
}

/// Одинарные кавычки POSIX-оболочки: `'` внутри — `'\''`.
pub fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// Командная строка поверхности Ghostty на macOS для команды оболочки
/// (`src/termio/Exec.zig:1844–1965` закреплённой ревизии): `login [-q] -flp <user> /bin/bash
/// --noprofile --norc -c "exec -l <команда>"`, `-q` — при `~/.hushlogin`. Команда — слова,
/// каждое в одинарных кавычках.
pub fn login_argv(user: &str, hush: bool, command: &[String]) -> Vec<String> {
    let mut argv = vec![LOGIN.to_owned()];
    if hush {
        argv.push("-q".to_owned());
    }
    let words: Vec<String> = command.iter().map(|word| shell_quote(word)).collect();
    argv.extend(
        [
            "-flp",
            user,
            "/bin/bash",
            "--noprofile",
            "--norc",
            "-c",
            &format!("exec -l {}", words.join(" ")),
        ]
        .map(str::to_owned),
    );
    argv
}

/// Пользователь и домашний каталог из `getpwuid_r(getuid())`, как у Ghostty.
pub fn passwd() -> Result<(String, PathBuf), String> {
    // SAFETY: getuid без указателей.
    let uid = unsafe { libc::getuid() };
    let mut buffer = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: passwd — POD, нули допустимы.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: entry, buffer и result — действительные выходные буферы, длина buffer передана.
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() || entry.pw_name.is_null() || entry.pw_dir.is_null() {
        return Err(format!("getpwuid_r({uid}): код {rc}"));
    }
    // SAFETY: getpwuid_r заполнил pw_name и pw_dir строками с нулём внутри buffer.
    let (name, dir) = unsafe { (CStr::from_ptr(entry.pw_name), CStr::from_ptr(entry.pw_dir)) };
    Ok((
        name.to_string_lossy().into_owned(),
        PathBuf::from(dir.to_string_lossy().into_owned()),
    ))
}

/// Точка входа режима `--fixture`.
pub fn main(rest: Vec<String>) -> i32 {
    let args = match parse(rest) {
        Ok(args) => args,
        Err(err) => {
            eprintln!("probe-resources --fixture: {err}");
            return EXIT_USAGE;
        }
    };
    let name = args.spec.name.clone();
    match run(args) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("probe-resources --fixture {name}: {err}");
            1
        }
    }
}

/// Файл и канал фикстуры: событие сначала в файл, затем в канал.
struct Reporter {
    state: Mutex<ReporterState>,
}

struct ReporterState {
    stream: UnixStream,
    dir: PathBuf,
    file: String,
    log: Log,
    seq: u64,
    /// После `exit` события не пишутся: поздний `child-exit` сборщика не нарушает порядок.
    closed: bool,
    /// Канал отказал при записи: стенда нет.
    broken: bool,
}

impl Reporter {
    fn new(stream: UnixStream, out: &Path, spec: &Spec) -> Result<Reporter, String> {
        let dir = out.join(receipts::LOG_DIR);
        std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        Ok(Reporter {
            state: Mutex::new(ReporterState {
                stream,
                dir,
                file: receipts::log_file_name(&spec.name),
                log: Log {
                    schema: receipts::LOG_SCHEMA,
                    name: spec.name.clone(),
                    mode: spec.mode,
                    events: Vec::new(),
                },
                seq: 0,
                closed: false,
                broken: false,
            }),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ReporterState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Пишет событие. Отказ файла — ошибка фикстуры; отказ канала запоминается, фикстура выходит
    /// по закрытию канала.
    fn send(&self, event: Event) -> Result<(), String> {
        let mut state = self.lock();
        state.write(event)
    }

    /// Квитанция с очередным номером: граница берётся под замком, номер и порядок файла совпадают.
    fn receipt(
        &self,
        spec: &Spec,
        label: &str,
        id: Option<u64>,
        allocated_bytes: u64,
        workers: u32,
    ) -> Result<Receipt, String> {
        let mut state = self.lock();
        let boundary = boundary()?;
        state.seq += 1;
        let receipt = Receipt {
            seq: state.seq,
            name: spec.name.clone(),
            mode: spec.mode,
            label: label.to_owned(),
            process: process_info(),
            boundary,
            allocated_bytes,
            workers,
        };
        let event = if label == "ready" && id.is_none() {
            Event::Ready {
                receipt: receipt.clone(),
            }
        } else {
            Event::Receipt {
                id,
                receipt: receipt.clone(),
            }
        };
        state.write(event)?;
        Ok(receipt)
    }

    fn close(&self, reason: ExitReason) -> Result<(), String> {
        let mut state = self.lock();
        let result = state.write(Event::Exit { reason });
        state.closed = true;
        let _ = state.stream.shutdown(std::net::Shutdown::Write);
        result
    }
}

impl ReporterState {
    fn write(&mut self, event: Event) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        let frame = receipts::encode(&event)?;
        self.log.events.push(event);
        let bytes = serde_json::to_vec_pretty(&self.log)
            .map_err(|err| format!("сериализация файла фикстуры: {err}"))?;
        crate::results::write_atomic(&self.dir, &self.file, &bytes)?;
        if !self.broken && self.stream.write_all(&frame).is_err() {
            self.broken = true;
        }
        Ok(())
    }
}

fn process_info() -> ProcessInfo {
    let pid = std::process::id() as i32;
    // SAFETY: getpgrp, getuid и geteuid без указателей.
    let (pgid, uid, euid) = unsafe { (libc::getpgrp(), libc::getuid(), libc::geteuid()) };
    ProcessInfo {
        pid,
        ppid: std::os::unix::process::parent_id() as i32,
        pgid,
        sid: proc::session(pid),
        uid,
        euid,
        key: proc::bsd_info(pid).map(|info| Key {
            pid,
            start: info.start,
        }),
        argv0: std::env::args_os()
            .next()
            .map(|arg| arg.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

/// `getrusage(RUSAGE_SELF)`, затем собственный `proc_pid_rusage`, между отметками.
fn boundary() -> Result<Boundary, String> {
    let t_begin_ns = uptime_ns();
    let (user_ns, system_ns) = proc::units::self_getrusage()?;
    let own = proc::usage(std::process::id() as i32);
    let t_end_ns = uptime_ns();
    Ok(Boundary {
        t_begin_ns,
        rusage: Rusage { user_ns, system_ns },
        own,
        t_end_ns,
    })
}

/// Ключ своего ребёнка до `waitpid`: `proc_pidinfo`, при отказе — запись списка.
pub fn child_key(pid: i32) -> Result<FoundKey, String> {
    let pidinfo = match proc::bsd_info(pid) {
        Ok(info) => {
            return Ok(FoundKey {
                key: Key {
                    pid,
                    start: info.start,
                },
                source: KeySource::PidInfo,
            });
        }
        Err(error) => error,
    };
    let listed = proc::list_all()?
        .into_iter()
        .find(|listed| listed.pid == pid)
        .ok_or_else(|| format!("PID {pid} нет в списке; proc_pidinfo: {pidinfo}"))?;
    Ok(FoundKey {
        key: Key::of(&listed),
        source: KeySource::List { pidinfo },
    })
}

fn wait_status(status: std::process::ExitStatus) -> WaitStatus {
    WaitStatus {
        code: status.code(),
        signal: status.signal(),
    }
}

/// Остановка рабочих потоков нагрузки.
struct Stop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl Stop {
    fn new() -> Arc<Stop> {
        Arc::new(Stop {
            stopped: Mutex::new(false),
            wake: Condvar::new(),
        })
    }

    fn stop(&self) {
        let mut stopped = match self.stopped.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *stopped = true;
        self.wake.notify_all();
    }

    /// Ждёт до `until` или остановки; `true` — остановлено.
    fn wait_until(&self, until: Instant) -> bool {
        let mut stopped = match self.stopped.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        loop {
            if *stopped {
                return true;
            }
            let now = Instant::now();
            if now >= until {
                return false;
            }
            stopped = match self.wake.wait_timeout(stopped, until - now) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }
}

fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: ts — действительный выходной буфер.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000_000 + ts.tv_nsec as u64
}

/// Рабочий поток: в каждом периоде расписания `t0 + k × период` потратить квоту CPU потока
/// (не дольше конца периода), затем ждать начала следующего периода или остановки.
fn worker(pace: Pace, stop: Arc<Stop>) {
    let period_ns = PACE_PERIOD.as_nanos() as u64;
    let quota = pace.quota_ns(period_ns);
    let start = Instant::now();
    let mut index: u64 = 0;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    loop {
        let period_end = start + PACE_PERIOD * (index + 1) as u32;
        let cpu_start = thread_cpu_ns();
        loop {
            for _ in 0..BUSY_STEPS {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
            }
            std::hint::black_box(x);
            if thread_cpu_ns().saturating_sub(cpu_start) >= quota || Instant::now() >= period_end {
                break;
            }
        }
        let elapsed = start.elapsed().as_nanos() as u64;
        index = next_period(index, elapsed, period_ns);
        let next = start + Duration::from_nanos(index * period_ns);
        if stop.wait_until(next) {
            return;
        }
    }
}

/// Выделение `mib` МиБ: запись нулей во все байты и метки на каждой странице.
fn touch(mib: u64) -> Result<Vec<u8>, String> {
    let bytes = usize::try_from(mib << 20).map_err(|_| format!("--mib {mib}: велико"))?;
    // SAFETY: sysconf без указателей.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page = usize::try_from(page)
        .ok()
        .filter(|page| *page > 0)
        .ok_or_else(|| format!("sysconf(_SC_PAGESIZE): {page}"))?;
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(bytes)
        .map_err(|err| format!("выделение {mib} МиБ: {err}"))?;
    buffer.resize(bytes, 0u8);
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    for offset in (0..bytes).step_by(page) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        buffer[offset] = (x as u8) | 1;
    }
    std::hint::black_box(&buffer);
    Ok(buffer)
}

/// Запущенные дети: сборщики шлют `child-exit`.
struct Children<'a> {
    args: &'a Args,
    reporter: &'a Arc<Reporter>,
    deadline: Instant,
}

impl Children<'_> {
    fn remaining(&self) -> Duration {
        self.deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_secs(1))
    }

    fn fixture_argv(&self, spec: &Spec) -> Vec<String> {
        // Остаток срока с округлением вверх: ребёнок не переживает родителя больше чем на секунду.
        let remaining = self.remaining();
        let secs = remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0);
        spec.argv(
            &self.args.channel,
            &self.args.out,
            Duration::from_secs(secs),
        )
    }

    /// Запускает ребёнка, шлёт `spawned` с ключом и при `reap` ставит сборщик.
    fn spawn(
        &self,
        name: String,
        role: Role,
        program: &str,
        argv: Vec<String>,
        setsid: bool,
        reap: bool,
    ) -> Result<FoundKey, String> {
        let mut command = Process::new(program);
        command
            .args(&argv)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if setsid {
            // SAFETY: setsid асинхронно-безопасен; замыкание не выделяет память и не берёт замков.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let child: Child = command
            .spawn()
            .map_err(|err| format!("запуск {program} ({name}): {err}"))?;
        let pid = child.id() as i32;
        let key = child_key(pid);
        let mut full_argv = vec![program.to_owned()];
        full_argv.extend(argv);
        self.reporter.send(Event::Spawned {
            child: Spawned {
                name: name.clone(),
                role,
                pid,
                key: key.clone(),
                argv: full_argv,
            },
        })?;
        if reap {
            let reporter = Arc::clone(self.reporter);
            let mut child = child;
            thread::spawn(move || {
                if let Ok(status) = child.wait() {
                    let _ = reporter.send(Event::ChildExit {
                        pid,
                        status: wait_status(status),
                    });
                }
            });
        }
        key.map_err(|err| format!("ключ {name}: {err}"))
    }

    fn spawn_fixture(
        &self,
        spec: Spec,
        role: Role,
        setsid: bool,
        reap: bool,
    ) -> Result<FoundKey, String> {
        let exe = std::env::current_exe().map_err(|err| format!("current_exe: {err}"))?;
        let exe = exe
            .to_str()
            .ok_or_else(|| format!("путь {} не UTF-8", exe.display()))?
            .to_owned();
        let argv = self.fixture_argv(&spec);
        self.spawn(spec.name, role, &exe, argv, setsid, reap)
    }

    /// Дети узла дерева и промежуточные процессы двойного `fork`.
    fn tree(&self, spec: &Spec) -> Result<(), String> {
        for index in 1..=spec.children {
            let mut child = Spec::new(Mode::Tree, format!("{}/c{index}", spec.name));
            child.children = spec.grandchildren;
            self.spawn_fixture(child, Role::Child, false, true)?;
        }
        for index in 1..=spec.double_fork {
            let middle = Spec::new(Mode::DoubleFork, format!("{}/d{index}", spec.name));
            self.spawn_fixture(middle, Role::Middle, false, true)?;
        }
        Ok(())
    }

    /// Программа хоста — лидер своей сессии и группы; затем запись владения.
    fn host(&self, spec: &Spec) -> Result<(), String> {
        let program_name = format!("{}/p", spec.name);
        let program = match spec.program {
            Program::Tree => {
                let mut program = Spec::new(Mode::Tree, program_name);
                program.children = spec.children;
                program.grandchildren = spec.grandchildren;
                program.double_fork = spec.double_fork;
                self.spawn_fixture(program, Role::Program, true, true)?
            }
            Program::Login => {
                let (user, home) = passwd()?;
                let hush = home.join(".hushlogin").exists();
                let exe = std::env::current_exe().map_err(|err| format!("current_exe: {err}"))?;
                let exe = exe
                    .to_str()
                    .ok_or_else(|| format!("путь {} не UTF-8", exe.display()))?
                    .to_owned();
                let child = Spec::new(Mode::LoginChild, format!("{program_name}/c"));
                let mut command = vec![exe];
                command.extend(self.fixture_argv(&child));
                let mut argv = login_argv(&user, hush, &command);
                let login = argv.remove(0);
                self.spawn(program_name, Role::Login, &login, argv, true, true)?
            }
        };
        let own = proc::bsd_info(std::process::id() as i32)
            .map_err(|err| format!("ключ хоста: proc_pidinfo: {err}"))?;
        self.reporter.send(Event::Owner {
            record: OwnerRecord {
                host: Key {
                    pid: std::process::id() as i32,
                    start: own.start,
                },
                programs: vec![program.key],
                group: program.key,
                session: program.key,
            },
        })
    }
}

/// Команды канала и его закрытие.
enum Incoming {
    Request(Request),
    Garbage(String),
    Closed,
}

fn read_channel(stream: UnixStream, tx: mpsc::Sender<Incoming>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let incoming = match reader.read_line(&mut line) {
            Ok(0) | Err(_) => Incoming::Closed,
            Ok(_) => match receipts::decode_request(&line) {
                Ok(request) => Incoming::Request(request),
                Err(err) => Incoming::Garbage(err),
            },
        };
        let closed = matches!(incoming, Incoming::Closed);
        if tx.send(incoming).is_err() || closed {
            return;
        }
    }
}

fn run(args: Args) -> Result<(), String> {
    let deadline = Instant::now() + args.lifetime;
    let spec = args.spec.clone();
    let stream = UnixStream::connect(&args.channel)
        .map_err(|err| format!("подключение к {}: {err}", args.channel))?;
    let reader = stream.try_clone().map_err(|err| format!("канал: {err}"))?;
    let reporter = Arc::new(Reporter::new(stream, Path::new(&args.out), &spec)?);
    reporter.send(Event::Hello {
        name: spec.name.clone(),
        mode: spec.mode,
        pid: std::process::id() as i32,
    })?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || read_channel(reader, tx));

    let result = serve(&args, &spec, &reporter, &rx, deadline);
    let reason = match &result {
        Ok(reason) => *reason,
        Err(err) => {
            let _ = reporter.send(Event::Error {
                id: None,
                message: err.clone(),
            });
            ExitReason::Error
        }
    };
    let closed = reporter.close(reason);
    result.and(closed)
}

/// Подготовка режима, `ready` и команды до выхода.
fn serve(
    args: &Args,
    spec: &Spec,
    reporter: &Arc<Reporter>,
    rx: &mpsc::Receiver<Incoming>,
    deadline: Instant,
) -> Result<ExitReason, String> {
    let children = Children {
        args,
        reporter,
        deadline,
    };
    let stop = Stop::new();
    let mut workers = Vec::new();
    let mut held: Vec<u8> = Vec::new();
    let mut pace_workers = 0;
    match spec.mode {
        Mode::Idle | Mode::LoginChild | Mode::QuickExit => {}
        Mode::Load => {
            let pace = Pace::new(spec.percent.unwrap_or_default())?;
            pace_workers = pace.workers;
            for _ in 0..pace.workers {
                let stop = Arc::clone(&stop);
                workers.push(thread::spawn(move || worker(pace, stop)));
            }
        }
        Mode::Memory => {
            reporter.receipt(spec, "before", None, 0, 0)?;
            held = touch(spec.mib.unwrap_or_default())?;
        }
        Mode::Tree => children.tree(spec)?,
        Mode::DoubleFork => {
            let orphan = Spec::new(Mode::Idle, format!("{}/o", spec.name));
            children.spawn_fixture(orphan, Role::Orphan, false, false)?;
        }
        Mode::Host => children.host(spec)?,
    }
    let allocated = held.len() as u64;
    reporter.receipt(spec, "ready", None, allocated, pace_workers)?;
    if !spec.mode.long_lived() {
        return Ok(ExitReason::Done);
    }

    let reason = loop {
        let now = Instant::now();
        if now >= deadline {
            break ExitReason::Lifetime;
        }
        match rx.recv_timeout(deadline - now) {
            Ok(Incoming::Request(Request { id, command })) => match command {
                Command::Receipt { label } => {
                    if let Err(err) =
                        reporter.receipt(spec, &label, Some(id), allocated, pace_workers)
                    {
                        reporter.send(Event::Error {
                            id: Some(id),
                            message: err,
                        })?;
                    }
                }
                Command::Exit => break ExitReason::Command,
            },
            Ok(Incoming::Garbage(message)) => reporter.send(Event::Error { id: None, message })?,
            Ok(Incoming::Closed) | Err(RecvTimeoutError::Disconnected) => {
                break ExitReason::ChannelClosed;
            }
            Err(RecvTimeoutError::Timeout) => break ExitReason::Lifetime,
        }
    };
    stop.stop();
    for handle in workers {
        let _ = handle.join();
    }
    std::hint::black_box(&held);
    Ok(reason)
}

#[cfg(test)]
mod tests;
