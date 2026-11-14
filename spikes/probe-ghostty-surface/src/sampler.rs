//! Общий сэмплер процесса пробы вне главного потока: `proc_pid_rusage` процесса и его потомков
//! (оболочка, обёртка `login`, фикстура), CPU процесса по монотонному времени `CLOCK_UPTIME_RAW`.
//!
//! Главный поток только ставит запрос и забирает ответ. Поток сэмплера ждёт на канале запросов с
//! тайм-аутом, поэтому окно замера CPU никого не будит: показание в начале и в конце. После
//! каждого ответа вызывается `notify`, им сценарий будит главный поток.
//!
//! `ri_user_time` и `ri_system_time` приходят в тиках `mach_absolute_time`, а на Apple Silicon тик
//! не равен наносекунде. Тики переводятся через `mach_timebase_info`, и каждое показание несёт
//! `getrusage(RUSAGE_SELF)` для сверки перевода.

use std::ffi::c_int;
use std::mem::MaybeUninit;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Serialize;

use crate::clock;

pub const MIB: f64 = 1_048_576.0;
pub const MB: f64 = 1_000_000.0;
/// Шаг показаний в серии до устоявшейся памяти.
pub const SETTLE_PACE: Duration = Duration::from_millis(100);
/// Серия устоялась, когда память процесса и потомков не менялась столько. Временную память
/// драйвера Metal отпускает через 1,1–1,4 с (калибровка задачи 11), окно её покрывает.
pub const SETTLE_WINDOW_NS: u64 = 1_500_000_000;
/// Предел серии от запроса.
pub const SETTLE_CAP_NS: u64 = 6_000_000_000;
/// Изменение меньше этого — шум аллокатора, а не событие.
pub const SETTLE_EPSILON: u64 = 1024 * 1024;
const CHILD_CAPACITY: usize = 256;
const MAX_DESCENDANTS: usize = 512;

pub fn mib(bytes: f64) -> f64 {
    bytes / MIB
}

pub fn mb(bytes: f64) -> f64 {
    bytes / MB
}

/// Показание одного процесса.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Usage {
    pub pid: i32,
    pub ppid: i32,
    pub program: String,
    /// `ri_user_time + ri_system_time`, нс.
    pub cpu_ns: u64,
    /// `ri_phys_footprint`, байты.
    pub footprint: u64,
    pub idle_wakeups: u64,
    pub interrupt_wakeups: u64,
}

/// Потомок, чьё показание не прочитано, например `login` с setuid.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Unreadable {
    pub pid: i32,
    pub ppid: i32,
    pub program: String,
    pub error: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Reading {
    pub t_ns: u64,
    pub process: Usage,
    /// `getrusage(RUSAGE_SELF)`, нс: сверка перевода тиков.
    pub rusage_cpu_ns: u64,
    pub children: Vec<Usage>,
    pub unreadable: Vec<Unreadable>,
}

impl Reading {
    pub fn children_footprint(&self) -> u64 {
        self.children.iter().map(|child| child.footprint).sum()
    }

    pub fn child(&self, pid: i32) -> Option<&Usage> {
        self.children.iter().find(|child| child.pid == pid)
    }
}

/// `100 × Δcpu / Δt` без деления на ядра; `None` без прошедшего времени или при убывании CPU.
pub fn cpu_percent(cpu_from_ns: u64, cpu_to_ns: u64, t_from_ns: u64, t_to_ns: u64) -> Option<f64> {
    let elapsed = t_to_ns.checked_sub(t_from_ns).filter(|ns| *ns > 0)?;
    let cpu = cpu_to_ns.checked_sub(cpu_from_ns)?;
    Some(100.0 * cpu as f64 / elapsed as f64)
}

/// События в секунду; `None` без прошедшего времени или при убывании счётчика.
pub fn per_second(from: u64, to: u64, t_from_ns: u64, t_to_ns: u64) -> Option<f64> {
    let elapsed = t_to_ns.checked_sub(t_from_ns).filter(|ns| *ns > 0)?;
    Some(to.checked_sub(from)? as f64 * 1e9 / elapsed as f64)
}

/// Первый прирост `M(1) − M(0)`, байты.
pub fn first_increment(m0: u64, m1: u64) -> i64 {
    m1 as i64 - m0 as i64
}

/// Маргинальный прирост `(M(N) − M(1)) / (N − 1)`, байты; `None` при `N < 2`.
pub fn marginal_increment(m1: u64, mn: u64, n: usize) -> Option<f64> {
    per_surface(m1, mn, n.checked_sub(1)?)
}

/// `(to − from) / surfaces`, байты на поверхность; `None` без поверхностей.
pub fn per_surface(from: u64, to: u64, surfaces: usize) -> Option<f64> {
    (surfaces > 0).then(|| (to as f64 - from as f64) / surfaces as f64)
}

/// Окно замера CPU: показания в начале и в конце.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Window {
    pub start: Reading,
    pub end: Reading,
}

impl Window {
    pub fn duration_ms(&self) -> f64 {
        clock::interval_ms(self.start.t_ns, self.end.t_ns)
    }

    pub fn cpu_percent(&self) -> Option<f64> {
        cpu_percent(
            self.start.process.cpu_ns,
            self.end.process.cpu_ns,
            self.start.t_ns,
            self.end.t_ns,
        )
    }

    /// CPU тех же точек по `getrusage`, для сверки перевода тиков.
    pub fn rusage_percent(&self) -> Option<f64> {
        cpu_percent(
            self.start.rusage_cpu_ns,
            self.end.rusage_cpu_ns,
            self.start.t_ns,
            self.end.t_ns,
        )
    }

    /// CPU потомков, которые есть в обоих показаниях, суммарно.
    pub fn children_cpu_percent(&self) -> Option<f64> {
        let (from, to) = self
            .end
            .children
            .iter()
            .filter_map(|end| Some((self.start.child(end.pid)?.cpu_ns, end.cpu_ns)))
            .fold((0u64, 0u64), |(from, to), (a, b)| (from + a, to + b.max(a)));
        cpu_percent(from, to, self.start.t_ns, self.end.t_ns)
    }

    /// Пробуждения процесса (простой пакета и прерывания) в секунду.
    pub fn wakeups_per_s(&self) -> Option<f64> {
        let count = |usage: &Usage| usage.idle_wakeups.saturating_add(usage.interrupt_wakeups);
        per_second(
            count(&self.start.process),
            count(&self.end.process),
            self.start.t_ns,
            self.end.t_ns,
        )
    }

    /// Отношение CPU по `proc_pid_rusage` к CPU по `getrusage`; `None`, если `getrusage` насчитал
    /// меньше `min_ns`: его шаг — микросекунда, и малые окна сверку не несут.
    pub fn units_ratio(&self, min_ns: u64) -> Option<f64> {
        let rusage = self
            .end
            .rusage_cpu_ns
            .checked_sub(self.start.rusage_cpu_ns)
            .filter(|ns| *ns >= min_ns)?;
        let proc = self
            .end
            .process
            .cpu_ns
            .checked_sub(self.start.process.cpu_ns)?;
        Some(proc as f64 / rusage as f64)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    Pending,
    Settled,
    Capped,
}

fn close(a: &Reading, b: &Reading) -> bool {
    a.process.footprint.abs_diff(b.process.footprint) < SETTLE_EPSILON
        && a.children_footprint().abs_diff(b.children_footprint()) < SETTLE_EPSILON
}

/// Состояние серии от запроса в `since_ns`: устоялась, когда память процесса и потомков не
/// выходила за `SETTLE_EPSILON` от последнего показания `SETTLE_WINDOW_NS`.
pub fn settle(readings: &[Reading], since_ns: u64) -> Settle {
    let Some(last) = readings.last() else {
        return Settle::Pending;
    };
    let mut start = readings.len() - 1;
    while start > 0 && close(&readings[start - 1], last) {
        start -= 1;
    }
    if last.t_ns.saturating_sub(readings[start].t_ns) >= SETTLE_WINDOW_NS {
        Settle::Settled
    } else if last.t_ns.saturating_sub(since_ns) >= SETTLE_CAP_NS {
        Settle::Capped
    } else {
        Settle::Pending
    }
}

/// Итог серии до устоявшейся памяти.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Settled {
    pub since_ns: u64,
    pub readings: usize,
    /// `false` — серия упёрлась в `SETTLE_CAP_NS`, не устоявшись.
    pub settled: bool,
    /// От запроса до показания, в котором память изменилась последний раз, мс.
    pub last_change_ms: Option<f64>,
    pub first: Reading,
    pub last: Reading,
    /// Ход серии: мс от запроса, МиБ процесса, МиБ потомков.
    pub trace: Vec<[f64; 3]>,
}

impl Settled {
    pub fn from_series(readings: &[Reading], since_ns: u64, settled: bool) -> Option<Settled> {
        let first = readings.first()?.clone();
        let last = readings.last()?.clone();
        let last_change_ms = readings
            .windows(2)
            .rev()
            .find(|pair| !close(&pair[0], &pair[1]))
            .map(|pair| clock::interval_ms(since_ns, pair[1].t_ns));
        let trace = readings
            .iter()
            .map(|reading| {
                [
                    clock::interval_ms(since_ns, reading.t_ns),
                    mib(reading.process.footprint as f64),
                    mib(reading.children_footprint() as f64),
                ]
            })
            .collect();
        Some(Settled {
            since_ns,
            readings: readings.len(),
            settled,
            last_change_ms,
            first,
            last,
            trace,
        })
    }
}

enum Request {
    Settle,
    Window(Duration),
}

pub enum Reply {
    Settled(Settled),
    Window(Window),
    Failed(String),
}

/// Поток сэмплера. Одновременно идёт один запрос; закрытие сэмплера прерывает серию и окно.
pub struct Sampler {
    requests: Option<Sender<Request>>,
    replies: Receiver<Reply>,
    thread: Option<JoinHandle<()>>,
    pending: bool,
}

impl Sampler {
    pub fn start(notify: impl Fn() + Send + 'static) -> Result<Sampler, String> {
        let (requests, incoming) = mpsc::channel();
        let (outgoing, replies) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("probe-sampler".to_owned())
            .spawn(move || serve(&incoming, &outgoing, &notify))
            .map_err(|err| format!("поток сэмплера не запущен: {err}"))?;
        Ok(Sampler {
            requests: Some(requests),
            replies,
            thread: Some(thread),
            pending: false,
        })
    }

    fn request(&mut self, request: Request) -> Result<(), String> {
        if self.pending {
            return Err("сэмплер занят предыдущим запросом".to_owned());
        }
        self.requests
            .as_ref()
            .ok_or("сэмплер закрыт")?
            .send(request)
            .map_err(|_| "поток сэмплера завершён".to_owned())?;
        self.pending = true;
        Ok(())
    }

    /// Серия показаний с шагом `SETTLE_PACE` до устоявшейся памяти или предела.
    pub fn settle(&mut self) -> Result<(), String> {
        self.request(Request::Settle)
    }

    /// Окно замера: показание сейчас и через `duration`, без промежуточных пробуждений.
    pub fn window(&mut self, duration: Duration) -> Result<(), String> {
        self.request(Request::Window(duration))
    }

    /// Ответ на текущий запрос, если он готов. Не блокирует.
    pub fn take(&mut self) -> Option<Reply> {
        match self.replies.try_recv() {
            Ok(reply) => {
                self.pending = false;
                Some(reply)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.pending = false;
                Some(Reply::Failed("поток сэмплера завершён".to_owned()))
            }
        }
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        // Закрытый канал будит поток из ожидания серии или окна.
        self.requests = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(requests: &Receiver<Request>, replies: &Sender<Reply>, notify: &dyn Fn()) {
    let root = std::process::id() as i32;
    while let Ok(request) = requests.recv() {
        let reply = match request {
            Request::Settle => match series(root, requests) {
                Some(reply) => reply,
                None => return,
            },
            Request::Window(duration) => match window(root, duration, requests) {
                Some(reply) => reply,
                None => return,
            },
        };
        if replies.send(reply).is_err() {
            return;
        }
        notify();
    }
}

/// Ожидание на канале запросов: `Ok(())` — срок истёк, `Err(None)` — сэмплер закрыт,
/// `Err(Some)` — запрос пришёл раньше срока, причина для ответа.
fn pause(requests: &Receiver<Request>, duration: Duration) -> Result<(), Option<&'static str>> {
    match requests.recv_timeout(duration) {
        Err(RecvTimeoutError::Timeout) => Ok(()),
        Err(RecvTimeoutError::Disconnected) => Err(None),
        Ok(_) => Err(Some("новый запрос до ответа на текущий")),
    }
}

fn series(root: i32, requests: &Receiver<Request>) -> Option<Reply> {
    let since_ns = clock::uptime_ns();
    let mut readings = Vec::new();
    loop {
        match read(root) {
            Ok(reading) => readings.push(reading),
            Err(reason) => return Some(Reply::Failed(reason)),
        }
        let settled = match settle(&readings, since_ns) {
            Settle::Pending => None,
            Settle::Settled => Some(true),
            Settle::Capped => Some(false),
        };
        if let Some(settled) = settled {
            return Some(
                Settled::from_series(&readings, since_ns, settled).map_or_else(
                    || Reply::Failed("серия без показаний".to_owned()),
                    Reply::Settled,
                ),
            );
        }
        if let Err(stop) = pause(requests, SETTLE_PACE) {
            return stop.map(|reason| Reply::Failed(reason.to_owned()));
        }
    }
}

fn window(root: i32, duration: Duration, requests: &Receiver<Request>) -> Option<Reply> {
    let start = match read(root) {
        Ok(reading) => reading,
        Err(reason) => return Some(Reply::Failed(reason)),
    };
    if let Err(stop) = pause(requests, duration) {
        return stop.map(|reason| Reply::Failed(reason.to_owned()));
    }
    Some(match read(root) {
        Ok(end) => Reply::Window(Window { start, end }),
        Err(reason) => Reply::Failed(reason),
    })
}

/// Показание процесса `root` и всех его потомков.
pub fn read(root: i32) -> Result<Reading, String> {
    let t_ns = clock::uptime_ns();
    let process =
        usage(root, 0).map_err(|err| format!("proc_pid_rusage процесса {root}: {err}"))?;
    let rusage_cpu_ns = rusage_self_ns()?;
    let mut children = Vec::new();
    let mut unreadable = Vec::new();
    for (pid, ppid) in descendants(root) {
        match usage(pid, ppid) {
            Ok(usage) => children.push(usage),
            Err(error) => unreadable.push(Unreadable {
                pid,
                ppid,
                program: name(pid),
                error,
            }),
        }
    }
    Ok(Reading {
        t_ns,
        process,
        rusage_cpu_ns,
        children,
        unreadable,
    })
}

fn usage(pid: i32, ppid: i32) -> Result<Usage, String> {
    let mut info = MaybeUninit::<libc::rusage_info_v0>::uninit();
    // SAFETY: для `RUSAGE_INFO_V0` ядро пишет `rusage_info_v0` по переданному указателю.
    let code =
        unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, info.as_mut_ptr().cast()) };
    if code != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: при нулевом коде структура заполнена.
    let info = unsafe { info.assume_init() };
    Ok(Usage {
        pid,
        ppid,
        program: name(pid),
        cpu_ns: clock::mach_to_ns(info.ri_user_time.saturating_add(info.ri_system_time)),
        footprint: info.ri_phys_footprint,
        idle_wakeups: info.ri_pkg_idle_wkups,
        interrupt_wakeups: info.ri_interrupt_wkups,
    })
}

fn rusage_self_ns() -> Result<u64, String> {
    let mut usage = MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` пишет `rusage` по переданному указателю.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(format!("getrusage: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: при нулевом коде структура заполнена.
    let usage = unsafe { usage.assume_init() };
    Ok(timeval_ns(usage.ru_utime).saturating_add(timeval_ns(usage.ru_stime)))
}

fn timeval_ns(time: libc::timeval) -> u64 {
    (time.tv_sec.max(0) as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::from(time.tv_usec.max(0).unsigned_abs()) * 1000)
}

/// Имя исполняемого файла по `proc_pidpath`: в отличие от `proc_name` он читается и у процесса
/// другого владельца, например у `login` с setuid.
fn name(pid: i32) -> String {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: буфер длиной `PROC_PIDPATHINFO_MAXSIZE` жив на время вызова.
    let len = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    let Some(len) = usize::try_from(len).ok().filter(|len| *len > 0) else {
        return String::new();
    };
    let path = String::from_utf8_lossy(&buffer[..len.min(buffer.len())]).into_owned();
    match path.rsplit_once('/') {
        Some((_, file)) => file.to_owned(),
        None => path,
    }
}

/// Потомки `root` обходом в ширину: (PID, PID родителя), по возрастанию PID.
pub fn descendants(root: i32) -> Vec<(i32, i32)> {
    let mut found: Vec<(i32, i32)> = Vec::new();
    let mut queue = vec![root];
    while let Some(parent) = queue.pop() {
        for child in child_pids(parent) {
            if found.len() >= MAX_DESCENDANTS || child == root {
                continue;
            }
            if found.iter().any(|(pid, _)| *pid == child) {
                continue;
            }
            found.push((child, parent));
            queue.push(child);
        }
    }
    found.sort_unstable();
    found
}

fn child_pids(parent: i32) -> Vec<i32> {
    let mut buffer = vec![0i32; CHILD_CAPACITY];
    let bytes = c_int::try_from(buffer.len() * size_of::<i32>()).unwrap_or(c_int::MAX);
    // SAFETY: буфер длиной `bytes` байт жив на время вызова.
    let count = unsafe { libc::proc_listchildpids(parent, buffer.as_mut_ptr().cast(), bytes) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    buffer.truncate(count.min(CHILD_CAPACITY));
    buffer.retain(|pid| *pid > 0);
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    fn reading(t_ms: u64, process: u64, children: &[u64]) -> Reading {
        Reading {
            t_ns: t_ms * 1_000_000,
            process: Usage {
                footprint: process,
                ..Usage::default()
            },
            children: children
                .iter()
                .enumerate()
                .map(|(index, footprint)| Usage {
                    pid: index as i32 + 100,
                    footprint: *footprint,
                    ..Usage::default()
                })
                .collect(),
            ..Reading::default()
        }
    }

    #[test]
    fn units() {
        assert_eq!(mib(1_048_576.0), 1.0);
        assert_eq!(mb(1_000_000.0), 1.0);
        assert_eq!(mib(150.0 * MB), 143.051_147_460_937_5);
        assert_eq!(mb(1_048_576.0), 1.048_576);
        assert_eq!(mb(2.0 * MIB), 2.097_152);
    }

    #[test]
    fn cpu_percent_on_known_values() {
        // 50 мс CPU за 10 с — 0,5 %, без деления на ядра: 2 с CPU за 1 с — 200 %.
        assert_eq!(cpu_percent(0, 50_000_000, 0, 10_000_000_000), Some(0.5));
        assert_eq!(
            cpu_percent(1_000, 2_000_001_000, 5, 1_000_000_005),
            Some(200.0)
        );
        assert_eq!(cpu_percent(10, 10, 0, 1_000), Some(0.0));
        assert_eq!(cpu_percent(0, 1, 7, 7), None);
        assert_eq!(cpu_percent(0, 1, 8, 7), None);
        assert_eq!(cpu_percent(5, 4, 0, 1_000), None);
        assert_eq!(per_second(10, 40, 0, 2_000_000_000), Some(15.0));
        assert_eq!(per_second(10, 9, 0, 1), None);
    }

    #[test]
    fn window_formulas() {
        let mut start = reading(0, 0, &[]);
        start.process.cpu_ns = 1_000_000;
        start.rusage_cpu_ns = 2_000_000;
        start.process.idle_wakeups = 10;
        start.process.interrupt_wakeups = 5;
        start.children = vec![
            Usage {
                pid: 7,
                cpu_ns: 100,
                ..Usage::default()
            },
            Usage {
                pid: 8,
                cpu_ns: 0,
                ..Usage::default()
            },
        ];
        let mut end = reading(10_000, 0, &[]);
        end.process.cpu_ns = 101_000_000;
        end.rusage_cpu_ns = 102_000_000;
        end.process.idle_wakeups = 110;
        end.process.interrupt_wakeups = 105;
        // PID 8 вышел, PID 9 появился: в CPU потомков входят только общие.
        end.children = vec![
            Usage {
                pid: 7,
                cpu_ns: 20_000_100,
                ..Usage::default()
            },
            Usage {
                pid: 9,
                cpu_ns: 1_000_000_000,
                ..Usage::default()
            },
        ];
        let window = Window { start, end };
        assert_eq!(window.duration_ms(), 10_000.0);
        assert_eq!(window.cpu_percent(), Some(1.0));
        assert_eq!(window.rusage_percent(), Some(1.0));
        assert_eq!(window.children_cpu_percent(), Some(0.2));
        assert_eq!(window.wakeups_per_s(), Some(20.0));
        assert_eq!(window.units_ratio(1_000_000), Some(1.0));
        assert_eq!(window.units_ratio(200_000_000), None);
    }

    #[test]
    fn memory_increments_on_known_values() {
        let mib = 1_048_576u64;
        assert_eq!(first_increment(100 * mib, 130 * mib), 30 * mib as i64);
        assert_eq!(first_increment(130 * mib, 100 * mib), -30 * mib as i64);
        // M(1) = 130, M(8) = 165: (165 − 130) / 7 = 5 МиБ на поверхность.
        assert_eq!(
            marginal_increment(130 * mib, 165 * mib, 8),
            Some(5.0 * mib as f64)
        );
        assert_eq!(marginal_increment(130 * mib, 165 * mib, 1), None);
        assert_eq!(marginal_increment(130 * mib, 165 * mib, 0), None);
        assert_eq!(
            per_surface(200 * mib, 184 * mib, 8),
            Some(-2.0 * mib as f64)
        );
        assert_eq!(per_surface(1, 2, 0), None);
    }

    #[test]
    fn settle_waits_for_a_quiet_window() {
        let step = SETTLE_EPSILON;
        // Память растёт до 300 мс, затем стоит: устоялась через 1,5 с после последнего изменения.
        let mut series: Vec<Reading> = (0..=3)
            .map(|index| reading(index * 100, 100 * step + index * 2 * step, &[step]))
            .collect();
        assert_eq!(settle(&series, 0), Settle::Pending);
        for t in (400..=1_800).step_by(100) {
            series.push(reading(t, 106 * step, &[step]));
        }
        assert_eq!(settle(&series, 0), Settle::Settled);
        let settled = Settled::from_series(&series, 0, true).unwrap();
        assert_eq!(settled.readings, 19);
        assert_eq!(settled.last_change_ms, Some(300.0));
        assert_eq!(settled.trace[0], [0.0, 100.0, 1.0]);
        assert_eq!(settled.last.process.footprint, 106 * step);
        // Шум меньше порога серию не сбрасывает.
        let noisy: Vec<Reading> = (0..=15)
            .map(|index| reading(index * 100, 50 * step + (index % 2) * (step / 2), &[0]))
            .collect();
        assert_eq!(settle(&noisy, 0), Settle::Settled);
    }

    #[test]
    fn settle_tracks_children_and_caps() {
        let step = SETTLE_EPSILON;
        // Память потомков меняется каждые 200 мс: серия упирается в предел.
        let series: Vec<Reading> = (0..=60)
            .map(|index| reading(index * 100, 10 * step, &[(index / 2) * 2 * step]))
            .collect();
        assert_eq!(settle(&series[..30], 0), Settle::Pending);
        assert_eq!(settle(&series, 0), Settle::Capped);
        assert_eq!(settle(&[], 0), Settle::Pending);
    }

    #[test]
    fn reading_of_this_process_converts_ticks_like_getrusage() {
        let root = std::process::id() as i32;
        let before = read(root).unwrap();
        assert!(before.process.footprint > 0);
        assert_eq!(before.process.pid, root);
        // Нагрузка до 150 мс CPU процесса по getrusage; соседние тесты тоже идут в процессе.
        let mut value = 0u64;
        while rusage_self_ns().unwrap() - before.rusage_cpu_ns < 150_000_000 {
            for _ in 0..10_000 {
                value = black_box(
                    value
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1),
                );
            }
        }
        let after = read(root).unwrap();
        let window = Window {
            start: before,
            end: after,
        };
        let ratio = window.units_ratio(100_000_000).unwrap();
        assert!((0.8..1.25).contains(&ratio), "отношение {ratio}");
    }

    #[test]
    fn descendants_include_a_spawned_child() {
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let root = std::process::id() as i32;
        let found = descendants(root);
        let reading = read(root).unwrap();
        let _ = child.kill();
        let _ = child.wait();
        assert!(found.contains(&(pid, root)), "{found:?}");
        let usage = reading.child(pid).unwrap();
        assert_eq!(usage.ppid, root);
        assert_eq!(usage.program, "cat");
        assert!(usage.footprint > 0);
    }

    #[test]
    fn sampler_answers_requests_in_order_and_notifies() {
        let (notified, notes) = mpsc::channel();
        let mut sampler = Sampler::start(move || {
            let _ = notified.send(());
        })
        .unwrap();
        sampler.settle().unwrap();
        assert!(sampler.window(Duration::ZERO).is_err());
        notes.recv_timeout(Duration::from_secs(30)).unwrap();
        assert!(matches!(sampler.take(), Some(Reply::Settled(_))));
        sampler.window(Duration::from_millis(20)).unwrap();
        notes.recv_timeout(Duration::from_secs(10)).unwrap();
        match sampler.take() {
            Some(Reply::Window(window)) => assert!(window.duration_ms() >= 20.0),
            _ => panic!("нет окна"),
        }
    }

    #[test]
    fn closing_the_sampler_interrupts_a_window() {
        let mut sampler = Sampler::start(|| {}).unwrap();
        sampler.window(Duration::from_secs(600)).unwrap();
        let started = Instant::now();
        drop(sampler);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
