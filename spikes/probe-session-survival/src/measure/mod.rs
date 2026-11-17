//! Сравнительные замеры (план P0.4, раздел «Замеры»): чистая часть сценария `measure`.
//!
//! Здесь условия замеров, сэмплер, расчёт CPU, пробуждений, памяти и задержки эха над рядами
//! показаний и подсчёт объёма кода. Сам сценарий на настоящих кандидатах — [`scenario`], ему нужен
//! стенд с `libghostty-vt`.
//!
//! Сэмплер — отдельный поток вне потоков стенда. Он снимает ряд показаний с шагом `pace` и между
//! ними ждёт на канале остановки, а не спит: стенд в это время ничего у кандидата не спрашивает, а
//! закрытие ряда будит поток сразу. Показание — CPU и физическая память процесса по
//! `proc_pid_rusage` или CPU потоков своего процесса по имени. Расходы хоста, клиента и
//! поверхности снимаются раздельными наблюдениями. В headless-прогоне поверхности нет: её место
//! занимает приёмник стенда, `libghostty-vt` в потоке маршрута `oracle-route`, поэтому
//! «поверхность» здесь — CPU этих потоков без рендера.

#[cfg(feature = "vt")]
pub mod scenario;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::matrix::Candidate;
use crate::observer::ProcessKey;
use crate::sys;

// --- условия, записанные до прогона ---

/// Ширина окна клиента и строки истории (раздел «Замеры»).
pub const COLS: u16 = 120;
pub const ROWS: u16 = 40;
/// Строк истории в каждой сессии замеров: предел истории обоих кандидатов.
pub const HISTORY_LINES: u64 = 10_000;
/// Строка потока во всю ширину окна: без мягких переносов.
pub const LINE_WIDTH: u16 = COLS;
/// Повторов каждого замера.
pub const REPEATS: usize = 5;
/// Окно CPU в простое и под потоком.
pub const WINDOW: Duration = Duration::from_secs(2);
/// Окно CPU скрытых сессий в простое.
pub const HIDDEN_WINDOW: Duration = Duration::from_secs(1);
/// Темп потока под нагрузкой: строка раз в 500 мкс, 2000 строк в секунду, около 240 КБ/с.
pub const FLOOD_INTERVAL_US: u64 = 500;
/// Строк потока под нагрузкой: с запасом длиннее всех окон, остаток отменяется.
pub const FLOOD_LINES: u64 = 60_000;
/// Квитанция хода потока раз в столько строк, около 10 в секунду: по ним считается вывод в окне.
pub const FLOOD_REPORT_EVERY: u64 = 200;
/// Скрытых сессий при замере эха.
pub const HIDDEN: [usize; 3] = [1, 8, 32];
/// Нажатий в одном повторе замера эха.
pub const KEYSTROKES: usize = 20;
/// Имя потока маршрута приёмника стенда: заменитель поверхности в headless-прогоне.
pub const SURFACE_THREAD: &str = "oracle-route";
/// Допуск сверки перевода тиков CPU с `getrusage`.
pub const UNITS_RANGE: std::ops::RangeInclusive<f64> = 0.9..=1.1;
/// Работа сверки перевода тиков: столько CPU по `getrusage`.
pub const UNITS_BURN_NS: u64 = 100_000_000;

pub const MIB: f64 = 1_048_576.0;

// --- наблюдения ---

/// Чей расход.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Part {
    Host,
    Client,
    /// Приёмник стенда вместо поверхности.
    Surface,
    /// Процесс стенда: контроль счётчика пробуждений, у него есть свои таймеры.
    Stand,
}

impl Part {
    pub fn name(self) -> &'static str {
        match self {
            Part::Host => "host",
            Part::Client => "client",
            Part::Surface => "surface",
            Part::Stand => "stand",
        }
    }
}

/// Что снимает сэмплер.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Watch {
    Process {
        part: Part,
        label: String,
        key: ProcessKey,
    },
    /// Потоки своего процесса с этим именем, суммарно.
    Threads { part: Part, name: String },
}

impl Watch {
    pub fn part(&self) -> Part {
        match self {
            Watch::Process { part, .. } | Watch::Threads { part, .. } => *part,
        }
    }
}

/// Показание одного наблюдения.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    /// CPU, нс.
    pub cpu_ns: u64,
    /// `ri_phys_footprint`, байты; у потоков нет.
    pub footprint: Option<u64>,
    /// Пробуждения пакета и прерываний; у потоков нет.
    pub wakeups: Option<u64>,
    /// Процессов или потоков в показании: сумма по разному набору потоков не сравнивается.
    pub units: usize,
}

/// Показания всех наблюдений в один момент `t_ns` (`CLOCK_UPTIME_RAW`). `None` — не прочитано.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    pub t_ns: u64,
    pub values: Vec<Option<Sample>>,
}

/// Перевод тиков `mach_absolute_time` в наносекунды.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timebase {
    pub numer: u32,
    pub denom: u32,
}

impl Timebase {
    pub fn system() -> Result<Timebase, String> {
        sys::timebase()
            .map(|(numer, denom)| Timebase { numer, denom })
            .ok_or_else(|| "mach_timebase_info не прочитан".to_owned())
    }

    pub fn ns(self, ticks: u64) -> u64 {
        let ns = u128::from(ticks) * u128::from(self.numer) / u128::from(self.denom);
        u64::try_from(ns).unwrap_or(u64::MAX)
    }
}

/// Показание наблюдения по системе.
pub fn read_system(watch: &Watch, timebase: Timebase) -> Option<Sample> {
    match watch {
        Watch::Process { key, .. } => sys::usage(key.pid).map(|usage| Sample {
            cpu_ns: timebase.ns(usage.cpu_ticks),
            footprint: Some(usage.footprint),
            wakeups: Some(usage.wakeups),
            units: 1,
        }),
        Watch::Threads { name, .. } => sys::threads_cpu_ns(name)
            .filter(|(_, count)| *count > 0)
            .map(|(cpu_ns, units)| Sample {
                cpu_ns,
                footprint: None,
                wakeups: None,
                units,
            }),
    }
}

// --- сэмплер ---

/// Ряд показаний в отдельном потоке. Закрытие ряда до конца прерывает его.
pub struct Series {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<Result<Vec<Reading>, String>>>,
}

impl Series {
    /// `count` показаний с шагом `pace`; первое — сразу. `read` снимает одно наблюдение.
    pub fn start<F>(
        watches: Vec<Watch>,
        count: usize,
        pace: Duration,
        read: F,
    ) -> Result<Series, String>
    where
        F: Fn(&Watch) -> Option<Sample> + Send + 'static,
    {
        if count < 2 {
            return Err(format!("ряд из {count} показаний не даёт окна"));
        }
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = thread::Builder::new()
            .name("measure-sampler".to_owned())
            .spawn(move || {
                let mut readings = Vec::with_capacity(count);
                for index in 0..count {
                    readings.push(Reading {
                        t_ns: sys::uptime_ns(),
                        values: watches.iter().map(&read).collect(),
                    });
                    if index + 1 == count {
                        break;
                    }
                    match stopped.recv_timeout(pace) {
                        Err(RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                            return Err(format!(
                                "ряд прерван после {} показаний из {count}",
                                readings.len()
                            ));
                        }
                    }
                }
                Ok(readings)
            })
            .map_err(|err| format!("поток сэмплера не запущен: {err}"))?;
        Ok(Series {
            stop: Some(stop),
            thread: Some(thread),
        })
    }

    /// Ждёт конца ряда.
    pub fn join(mut self) -> Result<Vec<Reading>, String> {
        let thread = self.thread.take().ok_or("ряд уже забран")?;
        let readings = thread
            .join()
            .map_err(|_| "поток сэмплера упал".to_owned())?;
        self.stop = None;
        readings
    }
}

impl Drop for Series {
    fn drop(&mut self) {
        // Закрытый канал будит поток из ожидания шага.
        self.stop = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// --- расчёты ---

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

/// Показания наблюдения `index` на концах окна `window` ряда.
fn ends(
    readings: &[Reading],
    window: usize,
    index: usize,
) -> Result<(u64, Sample, u64, Sample), String> {
    let (Some(start), Some(end)) = (readings.get(window), readings.get(window + 1)) else {
        return Err(format!("окна {window} в ряду из {} нет", readings.len()));
    };
    let value = |reading: &Reading| {
        reading
            .values
            .get(index)
            .copied()
            .flatten()
            .ok_or_else(|| format!("наблюдение {index} не прочитано в окне {window}"))
    };
    let (from, to) = (value(start)?, value(end)?);
    if from.units != to.units {
        return Err(format!(
            "наблюдение {index}: в окне {window} {} единиц в начале и {} в конце",
            from.units, to.units
        ));
    }
    Ok((start.t_ns, from, end.t_ns, to))
}

/// CPU группы наблюдений `indices` по каждому окну ряда, %: сумма группы, без деления на ядра.
pub fn window_cpu(readings: &[Reading], indices: &[usize]) -> Result<Vec<f64>, String> {
    if indices.is_empty() {
        return Err("группа наблюдений пуста".to_owned());
    }
    (0..readings.len().saturating_sub(1))
        .map(|window| {
            let mut total = 0.0;
            for index in indices {
                let (t_from, from, t_to, to) = ends(readings, window, *index)?;
                total += cpu_percent(from.cpu_ns, to.cpu_ns, t_from, t_to).ok_or_else(|| {
                    format!("наблюдение {index}: CPU убывает или окно {window} пустое")
                })?;
            }
            Ok(total)
        })
        .collect()
}

/// Пробуждения группы в секунду по каждому окну ряда.
pub fn window_wakeups(readings: &[Reading], indices: &[usize]) -> Result<Vec<f64>, String> {
    if indices.is_empty() {
        return Err("группа наблюдений пуста".to_owned());
    }
    (0..readings.len().saturating_sub(1))
        .map(|window| {
            let mut total = 0.0;
            for index in indices {
                let (t_from, from, t_to, to) = ends(readings, window, *index)?;
                let (Some(from), Some(to)) = (from.wakeups, to.wakeups) else {
                    return Err(format!("у наблюдения {index} нет пробуждений"));
                };
                total += per_second(from, to, t_from, t_to).ok_or_else(|| {
                    format!("наблюдение {index}: пробуждения убывают в окне {window}")
                })?;
            }
            Ok(total)
        })
        .collect()
}

/// Накопленный вывод нагрузки в момент `t_ns` по квитанциям хода `(t_ns, байты)`: линейно между
/// соседними квитанциями. `None` вне их диапазона — окно, не покрытое квитанциями, не считается.
pub fn bytes_at(points: &[(u64, u64)], t_ns: u64) -> Option<f64> {
    let after = points.iter().position(|(t, _)| *t >= t_ns)?;
    let (t1, b1) = points[after];
    if t1 == t_ns {
        return Some(b1 as f64);
    }
    let (t0, b0) = points[after.checked_sub(1)?];
    let share = (t_ns - t0) as f64 / (t1 - t0) as f64;
    Some(b0 as f64 + (b1 as f64 - b0 as f64) * share)
}

/// Вывод нагрузки в каждом окне ряда, байты. Квитанции идут по времени, объём не убывает.
pub fn window_bytes(readings: &[Reading], points: &[(u64, u64)]) -> Result<Vec<f64>, String> {
    if points
        .windows(2)
        .any(|pair| pair[1].0 <= pair[0].0 || pair[1].1 < pair[0].1)
    {
        return Err("квитанции хода потока не по порядку".to_owned());
    }
    let at = |t_ns: u64| {
        bytes_at(points, t_ns).ok_or_else(|| format!("отметка {t_ns} вне квитанций хода потока"))
    };
    readings
        .windows(2)
        .map(|pair| Ok(at(pair[1].t_ns)? - at(pair[0].t_ns)?))
        .collect()
}

/// CPU группы на МиБ вывода нагрузки по каждому окну, мс CPU на МиБ.
pub fn window_cpu_per_mib(
    readings: &[Reading],
    indices: &[usize],
    bytes: &[f64],
) -> Result<Vec<f64>, String> {
    if indices.is_empty() {
        return Err("группа наблюдений пуста".to_owned());
    }
    if bytes.len() != readings.len().saturating_sub(1) {
        return Err(format!(
            "объём вывода дан для {} окон, а в ряду их {}",
            bytes.len(),
            readings.len().saturating_sub(1)
        ));
    }
    bytes
        .iter()
        .enumerate()
        .map(|(window, written)| {
            if *written <= 0.0 {
                return Err(format!("в окне {window} нет вывода"));
            }
            let mut cpu_ns = 0u64;
            for index in indices {
                let (_, from, _, to) = ends(readings, window, *index)?;
                cpu_ns += to
                    .cpu_ns
                    .checked_sub(from.cpu_ns)
                    .ok_or_else(|| format!("наблюдение {index}: CPU убывает в окне {window}"))?;
            }
            Ok(cpu_ns as f64 / 1e6 / (written / MIB))
        })
        .collect()
}

/// Сверка перевода тиков: CPU своего процесса по `proc_pid_rusage` к `getrusage` за одну работу.
/// `None` — `getrusage` насчитал меньше `min_ns`: его шаг — микросекунда, малая работа сверку не
/// несёт.
pub fn units_ratio(proc_delta_ns: u64, rusage_delta_ns: u64, min_ns: u64) -> Option<f64> {
    (rusage_delta_ns >= min_ns).then(|| proc_delta_ns as f64 / rusage_delta_ns as f64)
}

pub fn mib(bytes: u64) -> f64 {
    bytes as f64 / MIB
}

/// Маргинальная память на сессию между двумя числами сессий: `(M(b) − M(a)) / (b − a)`, байты.
/// `None`, если сессий не прибавилось.
pub fn marginal(from: (usize, u64), to: (usize, u64)) -> Option<f64> {
    let added = to.0.checked_sub(from.0).filter(|added| *added > 0)?;
    Some((to.1 as f64 - from.1 as f64) / added as f64)
}

/// Одно нажатие замера эха: отметки `CLOCK_UPTIME_RAW` отправки в окно клиента, чтения нагрузкой
/// и прихода эха в приёмник.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Echo {
    pub sent_ns: u64,
    pub read_ns: u64,
    pub echoed_ns: u64,
}

/// Плечи эха, мс.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EchoLegs {
    /// От окна клиента до чтения нагрузкой.
    pub input_ms: f64,
    /// От чтения нагрузкой до приёмника.
    pub output_ms: f64,
    pub total_ms: f64,
}

impl Echo {
    /// `None`, если отметки не идут по порядку: эхо пришло раньше чтения или чтение раньше
    /// отправки — значит, это не эхо этого нажатия.
    pub fn legs(self) -> Option<EchoLegs> {
        let input = self.read_ns.checked_sub(self.sent_ns)?;
        let output = self.echoed_ns.checked_sub(self.read_ns)?;
        Some(EchoLegs {
            input_ms: input as f64 / 1e6,
            output_ms: output as f64 / 1e6,
            total_ms: (input + output) as f64 / 1e6,
        })
    }
}

/// Медиана непустого набора; при чётном числе — среднее двух средних.
pub fn median(values: &[f64]) -> Option<f64> {
    crate::stats::repeats(values)
        .ok()
        .map(|repeats| repeats.median)
}

// --- объём кода ---

/// Непустые строки файла Rust: код и тесты. Тестами считаются модуль или элемент под
/// `#[cfg(test)]` до закрывающей скобки с тем же отступом и файлы `tests.rs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lines {
    pub code: usize,
    pub tests: usize,
}

impl Lines {
    pub fn add(&mut self, other: Lines) {
        self.code += other.code;
        self.tests += other.tests;
    }
}

pub fn rust_lines(text: &str) -> Lines {
    let lines: Vec<&str> = text.lines().collect();
    let mut counted = Lines::default();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if line.trim() == "#[cfg(test)]" {
            let indent = &line[..line.len() - line.trim_start().len()];
            let item = lines.get(index + 1).map_or("", |next| next.trim_end());
            let end = if item.ends_with('{') {
                let close = format!("{indent}}}");
                lines[index + 2..]
                    .iter()
                    .position(|later| later.trim_end() == close)
                    .map_or(lines.len() - 1, |offset| index + 2 + offset)
            } else {
                (index + 1).min(lines.len() - 1)
            };
            counted.tests += lines[index..=end]
                .iter()
                .filter(|line| !line.trim().is_empty())
                .count();
            index = end + 1;
            continue;
        }
        if !line.trim().is_empty() {
            counted.code += 1;
        }
        index += 1;
    }
    counted
}

/// Добавленные и удалённые строки патча без заголовков файлов.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchLines {
    pub added: usize,
    pub removed: usize,
}

pub fn patch_lines(text: &str) -> PatchLines {
    let mut counted = PatchLines::default();
    for line in text.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(rest) = line.strip_prefix('+') {
            counted.added += usize::from(!rest.trim().is_empty());
        } else if let Some(rest) = line.strip_prefix('-') {
            counted.removed += usize::from(!rest.trim().is_empty());
        }
    }
    counted
}

pub fn nonempty_lines(text: &str) -> usize {
    text.lines().filter(|line| !line.trim().is_empty()).count()
}

/// Как считается файл.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Rust,
    /// Привязки, которые генерирует скрипт: вручную не правятся.
    Generated,
    /// Векторы и данные тестов.
    TestData,
    Patch,
    Script,
    /// Уведомление об авторстве: не код.
    Notice,
}

pub fn kind_of(path: &Path) -> Kind {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let in_vectors = path.components().any(|part| part.as_os_str() == "vectors");
    match path.extension().and_then(|ext| ext.to_str()) {
        _ if name == "NOTICE" => Kind::Notice,
        _ if in_vectors => Kind::TestData,
        Some("rs") if name == "bindings.rs" => Kind::Generated,
        Some("rs") => Kind::Rust,
        Some("patch") => Kind::Patch,
        _ => Kind::Script,
    }
}

/// Группа объёма кода кандидата: пути от корня crate, каталог — все файлы в нём.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: &'static str,
    pub paths: &'static [&'static str],
}

/// Что считается собственным кодом кандидата. Rust-хост: хост, протокол, кодировщик состояния
/// и обёртка `libghostty-vt`, бинари хоста и attach-клиента, сборка набора VT. zmx: патчи из
/// `zmx-patches` к zmx и его ghostty и сборка наборов; исходники zmx и ghostty — зависимость, а не свой код.
/// Адаптер стенда у обоих — форма контракта для матрицы, в продукт он не идёт.
pub fn groups(candidate: Candidate) -> &'static [Group] {
    match candidate {
        Candidate::Rust => &[
            Group {
                name: "хост",
                paths: &[
                    "src/host",
                    "src/proto",
                    "src/bridge",
                    "src/bin/host.rs",
                    "src/bin/attach.rs",
                ],
            },
            Group {
                name: "обёртка libghostty-vt",
                paths: &["src/vt"],
            },
            Group {
                name: "сборка набора",
                paths: &["scripts/vt.sh", "scripts/vt-bindings.sh"],
            },
            Group {
                name: "адаптер стенда",
                paths: &["src/backend/rust.rs"],
            },
        ],
        Candidate::Zmx => &[
            Group {
                name: "патчи",
                paths: &["zmx-patches"],
            },
            Group {
                name: "сборка набора",
                paths: &["scripts/zmx.sh", "scripts/zmx-pin"],
            },
            Group {
                name: "адаптер стенда",
                paths: &["src/backend/zmx.rs"],
            },
        ],
    }
}

/// Объём группы по видам файлов.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupLines {
    pub name: String,
    pub files: usize,
    pub rust: Lines,
    pub generated: usize,
    pub test_data: usize,
    pub patch: PatchLines,
    pub script: usize,
}

impl GroupLines {
    pub fn count(&mut self, path: &Path, text: &str) {
        let kind = kind_of(path);
        if kind == Kind::Notice {
            return;
        }
        self.files += 1;
        let tests_file = path.file_name().is_some_and(|name| name == "tests.rs");
        match kind {
            Kind::Rust if tests_file => self.rust.tests += nonempty_lines(text),
            Kind::Rust => self.rust.add(rust_lines(text)),
            Kind::Generated => self.generated += nonempty_lines(text),
            Kind::TestData => self.test_data += nonempty_lines(text),
            Kind::Patch => {
                let lines = patch_lines(text);
                self.patch.added += lines.added;
                self.patch.removed += lines.removed;
            }
            Kind::Script => self.script += nonempty_lines(text),
            Kind::Notice => {}
        }
    }
}

/// Файлы путей группы по порядку; отсутствующий путь — ошибка, а не ноль строк.
pub fn group_files(root: &Path, group: &Group) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for path in group.paths {
        let full = root.join(path);
        if full.is_dir() {
            collect(&full, &mut files)?;
        } else if full.is_file() {
            files.push(full);
        } else {
            return Err(format!("объём кода: {} нет", full.display()));
        }
    }
    files.sort();
    Ok(files)
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    for entry in entries {
        let path = entry
            .map_err(|err| format!("{}: {err}", dir.display()))?
            .path();
        if path.is_dir() {
            collect(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

/// Объём кода кандидата по группам; пути в итоге — от корня crate.
pub fn count_lines(root: &Path, candidate: Candidate) -> Result<Vec<GroupLines>, String> {
    groups(candidate)
        .iter()
        .map(|group| {
            let mut counted = GroupLines {
                name: group.name.to_owned(),
                ..GroupLines::default()
            };
            for file in group_files(root, group)? {
                let text = std::fs::read_to_string(&file)
                    .map_err(|err| format!("{}: {err}", file.display()))?;
                let relative = file.strip_prefix(root).unwrap_or(&file);
                counted.count(relative, &text);
            }
            Ok(counted)
        })
        .collect()
}

#[cfg(test)]
mod tests;
