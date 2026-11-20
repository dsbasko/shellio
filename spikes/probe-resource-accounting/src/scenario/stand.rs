//! Общее для сценариев задачи 6: стенд фикстур, снимки по атрибуции и проверки состава.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::attrib::{Attribution, Claims, Note, Owner, OwnerRecord};
use crate::background;
use crate::fixture::Spec;
use crate::fixture::bench::Bench;
use crate::fixture::receipts::{Event, Receipt};
use crate::proc::units::Timebase;
use crate::proc::{self, ReadError, Source};
use crate::sampler;
use crate::snapshot::{self, Key, Snapshot, Summary};

pub const READY: Duration = Duration::from_secs(30);
pub const RECEIPT: Duration = Duration::from_secs(10);
pub const EXIT: Duration = Duration::from_secs(15);
/// Собственный срок фикстур: они выходят по закрытию канала раньше.
pub const LIFETIME: Duration = Duration::from_secs(600);

pub struct Stand {
    pub bench: Bench,
    pub timebase: Timebase,
}

impl Stand {
    pub fn start(out: &Path) -> Result<Stand, String> {
        let exe = std::env::current_exe().map_err(|err| format!("current_exe: {err}"))?;
        let socket_dir = std::env::var("SHELLIO_STATE_DIR")
            .ok()
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .ok_or("SHELLIO_STATE_DIR не задан: сокет канала негде создать")?;
        let timebase = background::current_units().timebase();
        timebase
            .ticks_to_ns(0)
            .ok_or_else(|| format!("mach_timebase_info: {}/{}", timebase.numer, timebase.denom))?;
        Ok(Stand {
            bench: Bench::start(exe, &socket_dir, out, LIFETIME)?,
            timebase,
        })
    }

    /// Запускает фикстуры и ждёт `ready` от них и от `extra` (их потомков).
    pub fn spawn_ready(
        &mut self,
        specs: &[Spec],
        extra: &[String],
    ) -> Result<BTreeMap<String, Receipt>, String> {
        let mut names = Vec::new();
        for spec in specs {
            self.bench.spawn(spec)?;
            names.push(spec.name.clone());
        }
        names.extend(extra.iter().cloned());
        self.bench.wait_ready(&names, READY)
    }

    pub fn spawn_one(&mut self, spec: &Spec) -> Result<(i32, Receipt), String> {
        let pid = self.bench.spawn(spec)?;
        let ready = self
            .bench
            .wait_ready(std::slice::from_ref(&spec.name), READY)?;
        let receipt = ready
            .get(&spec.name)
            .cloned()
            .ok_or_else(|| format!("нет ready «{}»", spec.name))?;
        Ok((pid, receipt))
    }

    /// Запись владения хоста.
    pub fn owner_record(&mut self, host: &str) -> Result<OwnerRecord, String> {
        self.bench.wait_event(host, READY, |event| match event {
            Event::Owner { record } => Some(record.clone()),
            _ => None,
        })
    }

    /// `child-exit` ребёнка `pid` у фикстуры-родителя `parent`.
    pub fn child_exit(&mut self, parent: &str, pid: i32) -> Result<(), String> {
        self.bench.wait_event(parent, EXIT, |event| match event {
            Event::ChildExit { pid: got, .. } if *got == pid => Some(()),
            _ => None,
        })
    }

    pub fn receipts(
        &mut self,
        names: &[String],
        label: &str,
    ) -> Result<BTreeMap<String, Receipt>, String> {
        let mut out = BTreeMap::new();
        for name in names {
            out.insert(name.clone(), self.bench.receipt(name, label, RECEIPT)?);
        }
        Ok(out)
    }

    pub fn finish(mut self) -> Result<Vec<String>, String> {
        self.bench.finish(EXIT)?;
        Ok(std::mem::take(&mut self.bench.faults))
    }
}

/// Ключ фикстуры из её квитанции.
pub fn key_of(receipts: &BTreeMap<String, Receipt>, name: &str) -> Result<Key, String> {
    let receipt = receipts
        .get(name)
        .ok_or_else(|| format!("нет квитанции «{name}»"))?;
    receipt
        .process
        .key
        .map_err(|err| format!("ключ «{name}» в квитанции: {err}"))
}

/// Снимок заданных ключей.
pub fn snap(source: &impl Source, keys: &BTreeSet<Key>) -> Result<Snapshot, String> {
    Snapshot::take(source, background::uptime_ns, |_| keys.clone())
}

/// Снимок множества атрибуции по тому же списку: тот же путь, что у прохода сэмплера.
pub fn snap_claims(
    source: &impl Source,
    claims: &Claims,
) -> Result<(Snapshot, Attribution), String> {
    sampler::attributed_snapshot(source, background::uptime_ns, claims)
        .map(|(snapshot, attribution, _)| (snapshot, attribution))
}

/// Сводка владельца: объединение его атрибуции на обеих границах.
pub fn owner_summary(
    base: Option<(&Snapshot, &Attribution)>,
    current: (&Snapshot, &Attribution),
    owner: &Owner,
    timebase: Timebase,
) -> Result<Summary, String> {
    let mut members = current.1.members(owner);
    if let Some((_, attribution)) = base {
        members.extend(attribution.members(owner));
    }
    snapshot::summarize(base.map(|(s, _)| s), current.0, &members, timebase)
}

/// Сколько раз ключ встречается во всех итогах.
pub fn occurrences(attribution: &Attribution, key: Key) -> usize {
    attribution
        .shares
        .iter()
        .map(|share| share.members.iter().filter(|m| m.key == key).count())
        .sum()
}

/// Заметки `session_unknown` с ошибкой, отличной от `ESRCH`: принадлежность не проверена.
pub fn session_doubts(attribution: &Attribution) -> Vec<String> {
    attribution
        .notes
        .iter()
        .filter_map(|note| match note {
            Note::SessionUnknown { key, error } if *error != ReadError::Exited => {
                Some(format!("getsid({}): {error}", key.pid))
            }
            _ => None,
        })
        .collect()
}

/// Расхождение итога владельца с ожидаемым множеством и двойной счёт.
pub fn composition_problems(
    attribution: &Attribution,
    owner: &Owner,
    expected: &BTreeMap<String, Key>,
) -> Vec<String> {
    let got = attribution.members(owner);
    let want: BTreeSet<Key> = expected.values().copied().collect();
    let mut problems = Vec::new();
    for (name, key) in expected {
        if !got.contains(key) {
            problems.push(format!("нет «{name}» (PID {})", key.pid));
        }
        let count = occurrences(attribution, *key);
        if count != 1 {
            problems.push(format!("«{name}» встречается в итогах {count} раз"));
        }
    }
    for key in got.difference(&want) {
        problems.push(format!("лишний PID {} в итоге", key.pid));
    }
    problems
}

/// Итоги вызовов для одного PID: наблюдение без вердикта.
pub fn calls(pid: i32) -> serde_json::Value {
    serde_json::json!({
        "pid": pid,
        "proc_pid_rusage": json(&proc::usage(pid)),
        "getsid": json(&proc::session(pid)),
        "proc_pidinfo": json(&proc::bsd_info(pid)),
    })
}

pub fn json(value: &impl serde::Serialize) -> serde_json::Value {
    serde_json::to_value(value)
        .unwrap_or_else(|err| serde_json::Value::String(format!("сериализация: {err}")))
}

/// Список процессов прямо сейчас, по ключу.
pub fn listing_now() -> Result<BTreeMap<Key, proc::Listed>, String> {
    Ok(proc::list_all()?
        .into_iter()
        .map(|listed| (Key::of(&listed), listed))
        .collect())
}
