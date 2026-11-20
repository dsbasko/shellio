//! Общий сэмплер сводки «Измерено: процессы» (ADR-0011, план P0.8, «Solution Overview»).
//!
//! Один поток и один срок следующего прохода на всё приложение: число панелей меняет только
//! множество, которое читает проход, но не число пробуждений. Отдельный таймер на сессию —
//! антиобразец `detached/crates/app/src/terminal/session.rs:239–254`.
//!
//! Расписание держит один якорь — срок последнего прохода — и одну формулу: следующий срок =
//! якорь + шаг текущей видимости (2 с при видимом индикаторе, 10 с в фоне), но не раньше
//! текущего момента. Первый проход идёт сразу при запуске. Смена видимости пересчитывает срок от
//! того же якоря и не взводит его заново от текущего момента, поэтому череда переключений не
//! откладывает проход и не добавляет лишний. Проход, закончившийся позже следующего срока, и
//! позднее пробуждение не догоняются: пропущенные сроки считаются, следующий срок остаётся на
//! сетке якоря.
//!
//! Проход — один список, атрибуция по нему (в том числе `getsid` на каждый PID списка при записи
//! владения с сессией), чтение уникального множества и сводки владельцев по объединению их
//! атрибуции на обеих границах. Отказ списка — записанный неудачный проход, база дельты остаётся
//! у последнего удачного снимка.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::attrib::{self, Attribution, Claims, Owner};
use crate::background;
use crate::proc::Source;
use crate::proc::units::Timebase;
use crate::snapshot::{self, Key, Snapshot, Summary};

/// Видимость индикатора ресурсов.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    Visible,
    Background,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steps {
    pub visible_ns: u64,
    pub background_ns: u64,
}

impl Steps {
    pub fn of(self, visibility: Visibility) -> u64 {
        match visibility {
            Visibility::Visible => self.visible_ns,
            Visibility::Background => self.background_ns,
        }
    }
}

/// Шаг 2 с при видимом индикаторе (ADR-0011) и фоновый шаг 10 с по правилу «Фоновый шаг
/// сэмплера» плана.
pub const STEPS: Steps = Steps {
    visible_ns: 2_000_000_000,
    background_ns: 10_000_000_000,
};

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub timebase: Timebase,
    pub steps: Steps,
}

/// Срок следующего прохода.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    steps: Steps,
    visibility: Visibility,
    /// Срок последнего прохода.
    anchor: Option<u64>,
    deadline: u64,
}

impl Schedule {
    pub fn new(now_ns: u64, visibility: Visibility, steps: Steps) -> Result<Schedule, String> {
        if steps.visible_ns == 0 || steps.background_ns == 0 {
            return Err(format!("шаг сэмплера должен быть больше нуля: {steps:?}"));
        }
        Ok(Schedule {
            steps,
            visibility,
            anchor: None,
            deadline: now_ns,
        })
    }

    pub fn deadline(&self) -> u64 {
        self.deadline
    }

    pub fn visibility(&self) -> Visibility {
        self.visibility
    }

    /// Проход со сроком `due` закончился в `end_ns`. Возвращает число пропущенных сроков.
    pub fn passed(&mut self, due: u64, end_ns: u64) -> u64 {
        let step = self.steps.of(self.visibility);
        self.anchor = Some(due);
        let skipped = end_ns.saturating_sub(due) / step;
        self.deadline = due + (skipped + 1) * step;
        skipped
    }

    /// Смена видимости: срок от того же якоря с новым шагом, не раньше `now_ns`. До первого
    /// прохода срок не меняется: первый проход и так идёт сразу.
    pub fn set_visibility(&mut self, visibility: Visibility, now_ns: u64) {
        self.visibility = visibility;
        if let Some(anchor) = self.anchor {
            self.deadline = (anchor + self.steps.of(visibility)).max(now_ns);
        }
    }
}

/// Изменение управления, которое увидел поток.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Control {
    pub visibility: Option<Visibility>,
    pub claims: Option<Claims>,
    pub stop: bool,
}

/// Почему поток вернулся из ожидания.
#[derive(Clone, Debug, PartialEq)]
pub enum Wake {
    /// Срок наступил.
    Deadline,
    Control(Control),
    /// Проснулся раньше срока без изменений.
    Early,
}

/// Монотонные часы и прерываемое ожидание срока: настоящие ([`ControlTimer`]) или подставные.
pub trait Timer {
    fn now_ns(&self) -> u64;
    fn wait_until(&mut self, deadline_ns: u64) -> Wake;
}

/// Числа владельца или уникального множества после прохода.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Numbers {
    pub members: usize,
    pub memory_bytes: u64,
    /// `None` — базы нет.
    pub cpu_pct: Option<f64>,
    pub partial: bool,
}

impl Numbers {
    fn of(summary: &Summary) -> Numbers {
        Numbers {
            members: summary.entries.len(),
            memory_bytes: summary.memory.bytes,
            cpu_pct: summary.cpu.pct,
            partial: summary.partial,
        }
    }
}

/// Итог удачного прохода, который читал бы индикатор.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Published {
    pub pass: u64,
    pub at_ns: u64,
    pub total: Numbers,
    pub owners: Vec<(Owner, Numbers)>,
}

/// Счётчики удачного прохода.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PassCounts {
    /// Записей списка.
    pub listed: usize,
    /// Прочитано PID: уникальное множество прохода.
    pub set_size: usize,
    /// Члены множества, чтение которых отказало или не связано со списком.
    pub partial_reads: usize,
    pub getsid_calls: usize,
    pub attribution_notes: usize,
    pub panels: usize,
    /// `ΔT` сводки; `None` — базы нет.
    pub cpu_interval_ns: Option<u64>,
    pub total: Numbers,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PassRecord {
    /// С единицы, по порядку, включая неудачные.
    pub index: u64,
    pub visibility: Visibility,
    pub due_ns: u64,
    pub start_ns: u64,
    pub end_ns: u64,
    pub duration_ns: u64,
    pub late_ns: u64,
    /// Сроки, пропущенные после этого прохода.
    pub skipped: u64,
    pub outcome: Result<PassCounts, String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wakeups {
    pub deadline: u64,
    pub control: u64,
    pub early: u64,
}

impl Wakeups {
    pub fn total(self) -> u64 {
        self.deadline + self.control + self.early
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Switch {
    pub at_ns: u64,
    pub to: Visibility,
    pub deadline_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SamplerLog {
    pub steps: Steps,
    pub started_ns: u64,
    pub stopped_ns: u64,
    pub wakeups: Wakeups,
    pub skipped_deadlines: u64,
    pub switches: Vec<Switch>,
    pub claims_changes: u64,
    pub passes: Vec<PassRecord>,
}

/// Сжатая сводка журнала для `result.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Digest {
    pub passes: usize,
    pub failed_passes: usize,
    pub wakeups: Wakeups,
    /// Все пробуждения потока за минуту работы; `None` — работа нулевой длительности.
    pub wakeups_per_minute: Option<f64>,
    pub skipped_deadlines: u64,
    /// Медиана и максимум длительности удачных проходов.
    pub duration_median_ns: Option<u64>,
    pub duration_max_ns: Option<u64>,
    pub set_size_min: Option<usize>,
    pub set_size_max: Option<usize>,
    pub partial_reads_total: usize,
    pub partial_passes: usize,
}

impl SamplerLog {
    pub fn digest(&self) -> Digest {
        let counts: Vec<&PassCounts> = self
            .passes
            .iter()
            .filter_map(|pass| pass.outcome.as_ref().ok())
            .collect();
        let mut durations: Vec<u64> = self
            .passes
            .iter()
            .filter(|pass| pass.outcome.is_ok())
            .map(|pass| pass.duration_ns)
            .collect();
        durations.sort_unstable();
        let span = self.stopped_ns.saturating_sub(self.started_ns);
        Digest {
            passes: self.passes.len(),
            failed_passes: self.passes.len() - counts.len(),
            wakeups: self.wakeups,
            wakeups_per_minute: (span > 0)
                .then(|| self.wakeups.total() as f64 * 60e9 / span as f64),
            skipped_deadlines: self.skipped_deadlines,
            duration_median_ns: durations.get(durations.len() / 2).copied(),
            duration_max_ns: durations.last().copied(),
            set_size_min: counts.iter().map(|c| c.set_size).min(),
            set_size_max: counts.iter().map(|c| c.set_size).max(),
            partial_reads_total: counts.iter().map(|c| c.partial_reads).sum(),
            partial_passes: counts.iter().filter(|c| c.total.partial).count(),
        }
    }
}

/// Снимок множества атрибуции по тому же списку. Третье значение — число вызовов `getsid`.
pub fn attributed_snapshot(
    source: &impl Source,
    now: impl FnMut() -> u64,
    claims: &Claims,
) -> Result<(Snapshot, Attribution, usize), String> {
    let found = RefCell::new(None);
    let calls = Cell::new(0usize);
    let snapshot = Snapshot::take(source, now, |listing| {
        let attribution = attrib::attribute(listing, claims, |pid| {
            calls.set(calls.get() + 1);
            source.session(pid)
        });
        let all = attribution.all();
        *found.borrow_mut() = Some(attribution);
        all
    })?;
    let attribution = found.into_inner().ok_or("атрибуция не выполнена")?;
    Ok((snapshot, attribution, calls.get()))
}

type Boundary = (Snapshot, Attribution);

struct PassOutput {
    boundary: Boundary,
    counts: PassCounts,
    total: Numbers,
    owners: Vec<(Owner, Numbers)>,
}

/// Один проход: снимок по атрибуции и сводки каждого владельца и уникального множества.
fn pass(
    source: &impl Source,
    now: impl FnMut() -> u64,
    claims: &Claims,
    base: Option<&Boundary>,
    timebase: Timebase,
) -> Result<PassOutput, String> {
    let (snapshot, attribution, getsid_calls) = attributed_snapshot(source, now, claims)?;
    let mut all = BTreeSet::<Key>::new();
    let mut owners = Vec::with_capacity(attribution.shares.len());
    for share in &attribution.shares {
        let mut members = attribution.members(&share.owner);
        if let Some((_, before)) = base {
            members.extend(before.members(&share.owner));
        }
        let summary = snapshot::summarize(base.map(|(s, _)| s), &snapshot, &members, timebase)?;
        owners.push((share.owner.clone(), Numbers::of(&summary)));
        all.extend(members);
    }
    let summary = snapshot::summarize(base.map(|(s, _)| s), &snapshot, &all, timebase)?;
    let total = Numbers::of(&summary);
    let counts = PassCounts {
        listed: snapshot.listed.len(),
        set_size: snapshot.reads.len(),
        partial_reads: snapshot
            .reads
            .values()
            .filter(|read| read.reading.is_err())
            .count(),
        getsid_calls,
        attribution_notes: attribution.notes.len(),
        panels: claims.panels.len(),
        cpu_interval_ns: summary.cpu.interval_ns,
        total: total.clone(),
    };
    Ok(PassOutput {
        boundary: (snapshot, attribution),
        counts,
        total,
        owners,
    })
}

/// Цикл сэмплера до команды остановки. `publish` получает итог каждого удачного прохода.
pub fn run(
    source: &impl Source,
    timer: &mut impl Timer,
    config: Config,
    mut claims: Claims,
    visibility: Visibility,
    mut publish: impl FnMut(Published),
) -> Result<SamplerLog, String> {
    let started_ns = timer.now_ns();
    let mut schedule = Schedule::new(started_ns, visibility, config.steps)?;
    let mut log = SamplerLog {
        steps: config.steps,
        started_ns,
        stopped_ns: started_ns,
        wakeups: Wakeups::default(),
        skipped_deadlines: 0,
        switches: Vec::new(),
        claims_changes: 0,
        passes: Vec::new(),
    };
    let mut base: Option<Boundary> = None;
    loop {
        match timer.wait_until(schedule.deadline()) {
            Wake::Deadline => {
                log.wakeups.deadline += 1;
                let due = schedule.deadline();
                let start_ns = timer.now_ns();
                let index = log.passes.len() as u64 + 1;
                let outcome = pass(
                    source,
                    || timer.now_ns(),
                    &claims,
                    base.as_ref(),
                    config.timebase,
                )
                .map(|output| {
                    base = Some(output.boundary);
                    publish(Published {
                        pass: index,
                        at_ns: timer.now_ns(),
                        total: output.total,
                        owners: output.owners,
                    });
                    output.counts
                });
                let end_ns = timer.now_ns();
                let visibility = schedule.visibility();
                let skipped = schedule.passed(due, end_ns);
                log.skipped_deadlines += skipped;
                log.passes.push(PassRecord {
                    index,
                    visibility,
                    due_ns: due,
                    start_ns,
                    end_ns,
                    duration_ns: end_ns.saturating_sub(start_ns),
                    late_ns: start_ns.saturating_sub(due),
                    skipped,
                    outcome,
                });
            }
            Wake::Early => log.wakeups.early += 1,
            Wake::Control(control) => {
                log.wakeups.control += 1;
                let now_ns = timer.now_ns();
                if let Some(next) = control.visibility
                    && next != schedule.visibility()
                {
                    schedule.set_visibility(next, now_ns);
                    log.switches.push(Switch {
                        at_ns: now_ns,
                        to: next,
                        deadline_ns: schedule.deadline(),
                    });
                }
                if let Some(next) = control.claims {
                    claims = next;
                    log.claims_changes += 1;
                }
                if control.stop {
                    break;
                }
            }
        }
    }
    log.stopped_ns = timer.now_ns();
    Ok(log)
}

/// Запрошенное управление: поток сверяет его с увиденным.
struct Requested {
    visibility: Visibility,
    claims: Claims,
    claims_version: u64,
    stop: bool,
}

#[derive(Default)]
struct Board {
    latest: Option<Published>,
    finished: bool,
}

struct Shared {
    requested: Mutex<Requested>,
    requested_cv: Condvar,
    board: Mutex<Board>,
    board_cv: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Настоящий таймер: `CLOCK_UPTIME_RAW` и ожидание на условной переменной управления.
pub struct ControlTimer {
    shared: Arc<Shared>,
    visibility: Visibility,
    claims_version: u64,
}

impl ControlTimer {
    fn change(&mut self, requested: &Requested) -> Option<Control> {
        let visibility = (requested.visibility != self.visibility).then_some(requested.visibility);
        let claims =
            (requested.claims_version != self.claims_version).then(|| requested.claims.clone());
        if visibility.is_none() && claims.is_none() && !requested.stop {
            return None;
        }
        self.visibility = requested.visibility;
        self.claims_version = requested.claims_version;
        Some(Control {
            visibility,
            claims,
            stop: requested.stop,
        })
    }
}

impl Timer for ControlTimer {
    fn now_ns(&self) -> u64 {
        background::uptime_ns()
    }

    fn wait_until(&mut self, deadline_ns: u64) -> Wake {
        let shared = Arc::clone(&self.shared);
        let requested = lock(&shared.requested);
        if let Some(control) = self.change(&requested) {
            return Wake::Control(control);
        }
        let now_ns = self.now_ns();
        if now_ns >= deadline_ns {
            return Wake::Deadline;
        }
        let (requested, _) = shared
            .requested_cv
            .wait_timeout(requested, Duration::from_nanos(deadline_ns - now_ns))
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(control) = self.change(&requested) {
            Wake::Control(control)
        } else if self.now_ns() >= deadline_ns {
            Wake::Deadline
        } else {
            Wake::Early
        }
    }
}

/// Отмечает конец потока и при панике.
struct Finished(Arc<Shared>);

impl Drop for Finished {
    fn drop(&mut self) {
        lock(&self.0.board).finished = true;
        self.0.board_cv.notify_all();
    }
}

/// Поток сэмплера и управление им.
pub struct Sampler {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Result<SamplerLog, String>>>,
}

impl Sampler {
    pub fn spawn<S: Source + Send + 'static>(
        source: S,
        config: Config,
        claims: Claims,
        visibility: Visibility,
    ) -> Result<Sampler, String> {
        Schedule::new(0, visibility, config.steps)?;
        let shared = Arc::new(Shared {
            requested: Mutex::new(Requested {
                visibility,
                claims: claims.clone(),
                claims_version: 0,
                stop: false,
            }),
            requested_cv: Condvar::new(),
            board: Mutex::new(Board::default()),
            board_cv: Condvar::new(),
        });
        let mut timer = ControlTimer {
            shared: Arc::clone(&shared),
            visibility,
            claims_version: 0,
        };
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("sampler".to_owned())
            .spawn(move || {
                let finished = Finished(Arc::clone(&thread_shared));
                let log = run(
                    &source,
                    &mut timer,
                    config,
                    claims,
                    visibility,
                    |published| {
                        lock(&thread_shared.board).latest = Some(published);
                        thread_shared.board_cv.notify_all();
                    },
                );
                drop(finished);
                log
            })
            .map_err(|err| format!("поток сэмплера: {err}"))?;
        Ok(Sampler {
            shared,
            thread: Some(thread),
        })
    }

    /// То же значение поток не будит.
    pub fn set_visibility(&self, visibility: Visibility) {
        let mut requested = lock(&self.shared.requested);
        if requested.visibility != visibility {
            requested.visibility = visibility;
            self.shared.requested_cv.notify_one();
        }
    }

    /// Те же заявки поток не будят.
    pub fn set_claims(&self, claims: Claims) {
        let mut requested = lock(&self.shared.requested);
        if requested.claims != claims {
            requested.claims = claims;
            requested.claims_version += 1;
            self.shared.requested_cv.notify_one();
        }
    }

    /// Ждёт опубликованный итог, для которого `accept` истинно.
    pub fn wait_published(
        &self,
        timeout: Duration,
        mut accept: impl FnMut(&Published) -> bool,
    ) -> Result<Published, String> {
        let deadline = Instant::now() + timeout;
        let mut board = lock(&self.shared.board);
        loop {
            if let Some(latest) = board.latest.as_ref().filter(|p| accept(p)) {
                return Ok(latest.clone());
            }
            if board.finished {
                return Err("поток сэмплера завершился без нужного итога".to_owned());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("нужный итог сэмплера не пришёл за {timeout:?}"));
            }
            board = self
                .shared
                .board_cv
                .wait_timeout(board, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn request_stop(&self) {
        lock(&self.shared.requested).stop = true;
        self.shared.requested_cv.notify_one();
    }

    pub fn stop(mut self) -> Result<SamplerLog, String> {
        self.request_stop();
        let thread = self.thread.take().ok_or("поток сэмплера уже остановлен")?;
        thread
            .join()
            .map_err(|_| "поток сэмплера запаниковал".to_owned())?
    }
}

impl Drop for Sampler {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.request_stop();
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests;
