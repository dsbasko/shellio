//! Снимок и формулы сводки «Измерено: процессы» (ADR-0011, план P0.8, «Solution Overview»).
//!
//! Снимок — один список `sysctl(KERN_PROC_ALL)` и чтение выбранного по нему подмножества между
//! отметками начала и конца сбора по монотонным часам; граница `T` — середина сбора. Член
//! множества адресуется ключом списка `(pid, p_start)`: ключ есть и у процесса, который не
//! читается, а повторный PID получает новый ключ. Дельта CPU берётся только у ключа, прочитанного
//! на обеих границах одной жизнью PID: [`Identity`] (`ri_proc_start_abstime`) совпала.
//!
//! Память — сумма `ri_phys_footprint` прочитанных членов текущей границы. CPU% —
//! `100 × Σ Δ(ri_user_time + ri_system_time) / ΔT`, тики Mach переводятся одним вызовом
//! [`Timebase::ticks_to_ns`], 100% — одно логическое ядро, значения выше 100% не срезаются. Член,
//! не вошедший в сумму, несёт причину, итог — покрытие памяти и CPU по отдельности и признак
//! неполноты.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::proc::units::Timebase;
use crate::proc::{self, Identity, Listed, ProcRead, Source, StartTime, Unread};

/// Ключ записи списка: PID и настенное время старта `p_start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Key {
    pub pid: i32,
    pub start: StartTime,
}

impl Key {
    pub fn of(listed: &Listed) -> Key {
        Key {
            pid: listed.pid,
            start: listed.start,
        }
    }
}

/// Отметки сбора снимка по монотонным часам, нс.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Boundary {
    /// До перечисления.
    pub collect_start_ns: u64,
    /// После чтения последнего выбранного процесса.
    pub collect_end_ns: u64,
}

impl Boundary {
    /// Граница `T`: середина сбора.
    pub fn t_ns(self) -> u64 {
        self.collect_start_ns + self.collect_end_ns.saturating_sub(self.collect_start_ns) / 2
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub boundary: Boundary,
    /// Весь список прохода.
    pub listed: BTreeMap<Key, Listed>,
    /// Чтения выбранных ключей.
    pub reads: BTreeMap<Key, ProcRead>,
}

impl Snapshot {
    /// Список, выбор по нему и чтение только выбранного. Отказ списка — ошибка всего снимка.
    pub fn take(
        source: &impl Source,
        mut now: impl FnMut() -> u64,
        select: impl FnOnce(&[Listed]) -> BTreeSet<Key>,
    ) -> Result<Snapshot, String> {
        let collect_start_ns = now();
        let listing = source.list()?;
        let chosen = select(&listing);
        let reads = listing
            .iter()
            .filter(|listed| chosen.contains(&Key::of(listed)))
            .map(|listed| (Key::of(listed), proc::read_listed(source, listed)))
            .collect();
        let collect_end_ns = now();
        Ok(Snapshot {
            boundary: Boundary {
                collect_start_ns,
                collect_end_ns,
            },
            listed: listing
                .into_iter()
                .map(|listed| (Key::of(&listed), listed))
                .collect(),
            reads,
        })
    }
}

/// Почему значение члена не вошло в сумму.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Gap {
    /// Чтение на текущей границе отказало или не связано с записью списка.
    Unread {
        unread: Unread,
    },
    /// В текущем списке есть, но не выбран для чтения.
    NotRead,
    /// Предыдущей границы нет.
    NoPrevious,
    /// В предыдущем списке ключа нет: базы для дельты нет. `reused_pid` — на предыдущей
    /// границе этот PID держал другой процесс.
    Fresh {
        reused_pid: bool,
    },
    BaseUnread {
        unread: Unread,
    },
    BaseNotRead,
    /// Был в предыдущем списке и вышел до текущего: CPU от предыдущей границы до выхода потерян.
    Exited,
    /// Ключа нет ни в одном из двух списков.
    NotListed,
    /// Ключ тот же, а `ri_proc_start_abstime` другой.
    IdentityChanged {
        base: u64,
        current: u64,
    },
    CounterDecreased {
        base_mach: u64,
        current_mach: u64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Value<T> {
    Measured(T),
    Missing(Gap),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CpuDelta {
    pub mach: u64,
    pub ns: u64,
    /// % одного логического ядра за интервал.
    pub pct: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub key: Key,
    /// `None` — на текущей границе не прочитан.
    pub identity: Option<Identity>,
    /// `None` — в текущем списке процесса нет, в память он не входит.
    pub memory: Option<Value<u64>>,
    pub cpu: Value<CpuDelta>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub counted: usize,
    pub expected: usize,
}

impl Coverage {
    pub fn complete(self) -> bool {
        self.counted == self.expected
    }

    /// Доля учтённых; пустое множество покрыто целиком.
    pub fn share(self) -> f64 {
        if self.expected == 0 {
            1.0
        } else {
            self.counted as f64 / self.expected as f64
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub bytes: u64,
    /// Ожидаются члены, которые есть в текущем списке.
    pub coverage: Coverage,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cpu {
    /// `ΔT`; `None` — предыдущей границы нет.
    pub interval_ns: Option<u64>,
    pub delta_mach: u64,
    pub delta_ns: u64,
    pub pct: Option<f64>,
    /// Ожидаются все члены.
    pub coverage: Coverage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub boundary: Boundary,
    pub base: Option<Boundary>,
    pub memory: Memory,
    pub cpu: Cpu,
    /// Покрытие памяти или CPU неполное.
    pub partial: bool,
    pub entries: Vec<Entry>,
}

/// Дельта CPU члена в тиках Mach и `ΔT` её интервала.
fn cpu_delta(
    base: Option<(&Snapshot, u64)>,
    current: &Snapshot,
    key: &Key,
) -> Result<(u64, u64), Gap> {
    if !current.listed.contains_key(key) {
        let listed_before = base.is_some_and(|(base, _)| base.listed.contains_key(key));
        return Err(if listed_before {
            Gap::Exited
        } else {
            Gap::NotListed
        });
    }
    let now = current
        .reads
        .get(key)
        .ok_or(Gap::NotRead)?
        .reading
        .map_err(|unread| Gap::Unread { unread })?;
    let (base, interval_ns) = base.ok_or(Gap::NoPrevious)?;
    if !base.listed.contains_key(key) {
        return Err(Gap::Fresh {
            reused_pid: base.listed.keys().any(|other| other.pid == key.pid),
        });
    }
    let then = base
        .reads
        .get(key)
        .ok_or(Gap::BaseNotRead)?
        .reading
        .map_err(|unread| Gap::BaseUnread { unread })?;
    if then.identity != now.identity {
        return Err(Gap::IdentityChanged {
            base: then.identity.start_abstime,
            current: now.identity.start_abstime,
        });
    }
    let (before, after) = (then.usage.cpu_mach(), now.usage.cpu_mach());
    let delta = after.checked_sub(before).ok_or(Gap::CounterDecreased {
        base_mach: before,
        current_mach: after,
    })?;
    Ok((delta, interval_ns))
}

fn percent(ns: u64, interval_ns: u64) -> f64 {
    100.0 * ns as f64 / interval_ns as f64
}

/// Сводка множества `members` на границе `current` с дельтой CPU от `base`.
pub fn summarize(
    base: Option<&Snapshot>,
    current: &Snapshot,
    members: &BTreeSet<Key>,
    timebase: Timebase,
) -> Result<Summary, String> {
    let to_ns = |ticks: u64| {
        timebase.ticks_to_ns(ticks).ok_or_else(|| {
            format!(
                "mach_timebase_info не прочитан: {}/{}",
                timebase.numer, timebase.denom
            )
        })
    };
    to_ns(0)?;
    let base = match base {
        None => None,
        Some(base) => {
            let (from, to) = (base.boundary.t_ns(), current.boundary.t_ns());
            if to <= from {
                return Err(format!(
                    "граница {to} нс не позже предыдущей границы {from} нс"
                ));
            }
            Some((base, to - from))
        }
    };

    let mut memory = Memory::default();
    let mut cpu_coverage = Coverage {
        counted: 0,
        expected: members.len(),
    };
    let mut delta_mach = 0u64;
    let mut entries = Vec::with_capacity(members.len());
    for key in members {
        let read = current.reads.get(key).map(|read| read.reading);
        let entry_memory = current.listed.contains_key(key).then_some(match read {
            None => Value::Missing(Gap::NotRead),
            Some(Err(unread)) => Value::Missing(Gap::Unread { unread }),
            Some(Ok(reading)) => Value::Measured(reading.usage.phys_footprint),
        });
        if let Some(value) = &entry_memory {
            memory.coverage.expected += 1;
            if let Value::Measured(bytes) = value {
                memory.coverage.counted += 1;
                memory.bytes += bytes;
            }
        }
        let cpu = match cpu_delta(base, current, key) {
            Err(gap) => Value::Missing(gap),
            Ok((mach, interval_ns)) => {
                cpu_coverage.counted += 1;
                delta_mach += mach;
                let ns = to_ns(mach)?;
                Value::Measured(CpuDelta {
                    mach,
                    ns,
                    pct: percent(ns, interval_ns),
                })
            }
        };
        entries.push(Entry {
            key: *key,
            identity: read.and_then(Result::ok).map(|reading| reading.identity),
            memory: entry_memory,
            cpu,
        });
    }

    let delta_ns = to_ns(delta_mach)?;
    Ok(Summary {
        boundary: current.boundary,
        base: base.map(|(base, _)| base.boundary),
        memory,
        cpu: Cpu {
            interval_ns: base.map(|(_, interval_ns)| interval_ns),
            delta_mach,
            delta_ns,
            pct: base.map(|(_, interval_ns)| percent(delta_ns, interval_ns)),
            coverage: cpu_coverage,
        },
        partial: !memory.coverage.complete() || !cpu_coverage.complete(),
        entries,
    })
}

#[cfg(test)]
mod tests;
