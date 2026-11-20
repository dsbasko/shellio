//! Наблюдатель стоимости: режим `--observer --stand <pid> --out <каталог>` того же исполняемого
//! файла (план P0.8, «Solution Overview», «Стенд»; ожидания — отчёт, раздел «Ожидания», `cost`).
//!
//! Отдельный процесс, ребёнок стенда, поэтому собственная работа наблюдателя не входит в CPU
//! стенда, а в проверке фона вычитается вместе с группой стенда. На каждую отметку границы окна из
//! stdin (строка JSON с числом завершённых операций) он сначала читает `proc_pid_rusage` стенда
//! (`ri_user_time + ri_system_time` и `ri_phys_footprint`) и `CLOCK_UPTIME_RAW`, затем снимает
//! границу проверки фона по всей системе и отвечает строкой JSON в stdout: чтение стенда и
//! проверку фона окна между прошлой и этой отметками. Конец stdin — запись журнала
//! `observer.json` и выход.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::background::{self, BackgroundCheck, Bound, THRESHOLDS};
use crate::results::{self, nan_null};

pub const LOG_FILE: &str = "observer.json";

/// Отметка границы от стенда.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub index: u64,
    /// Завершённые операции нагрузки стенда к моменту отметки.
    pub ops: u64,
}

/// Чтение стенда на границе.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub index: u64,
    pub ops: u64,
    /// `CLOCK_UPTIME_RAW` в момент чтения стенда, нс.
    pub uptime_ns: u64,
    /// `ri_user_time + ri_system_time` стенда в тиках Mach.
    pub stand_cpu_mach: u64,
    #[serde(deserialize_with = "nan_null::deserialize")]
    pub stand_cpu_ns: f64,
    pub stand_footprint: u64,
    /// Проверка фона окна от прошлой отметки до этой; у первой отметки нет.
    pub background: Option<BackgroundCheck>,
}

/// Внешняя метка: внутренняя (`tag = "kind"`) буферизует содержимое, и через буфер ключи
/// `failed_by_errno` (`BTreeMap<i32, _>`, в JSON строки) не читаются.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Reading(Box<Reading>),
    Error { index: u64, reason: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Log {
    pub schema: u32,
    pub stand: i32,
    pub pid: u32,
    pub readings: Vec<Reading>,
    pub errors: Vec<String>,
}

fn parse(mut rest: Vec<String>) -> Result<(i32, PathBuf), String> {
    let mut value = |flag: &str| -> Result<String, String> {
        let index = rest
            .iter()
            .position(|a| a == flag)
            .ok_or_else(|| format!("нужен {flag}"))?;
        if index + 1 >= rest.len() {
            return Err(format!("{flag}: нужно значение"));
        }
        let value = rest.remove(index + 1);
        rest.remove(index);
        Ok(value)
    };
    let stand: i32 = value("--stand")?
        .parse()
        .map_err(|err| format!("--stand: {err}"))?;
    let out = PathBuf::from(value("--out")?);
    if let Some(extra) = rest.first() {
        return Err(format!("лишний аргумент «{extra}»"));
    }
    if stand <= 0 {
        return Err(format!("--stand {stand}: нужен PID"));
    }
    Ok((stand, out))
}

fn read(mark: &Mark, stand: i32, previous: &mut Option<Bound>) -> Result<Reading, String> {
    let units = background::current_units();
    let uptime_ns = background::uptime_ns();
    let usage =
        crate::proc::usage(stand).map_err(|err| format!("proc_pid_rusage(стенда): {err}"))?;
    let bound = background::sample_bound()?;
    let check = previous
        .as_ref()
        .map(|start| background::evaluate(start, &bound, stand, &[], units, THRESHOLDS));
    *previous = Some(bound);
    Ok(Reading {
        index: mark.index,
        ops: mark.ops,
        uptime_ns,
        stand_cpu_mach: usage.cpu_mach(),
        stand_cpu_ns: units.mach_to_ns(usage.cpu_mach()),
        stand_footprint: usage.phys_footprint,
        background: check,
    })
}

fn serve(stand: i32, out: &Path) -> Result<(), String> {
    let mut log = Log {
        schema: results::SCHEMA,
        stand,
        pid: std::process::id(),
        readings: Vec::new(),
        errors: Vec::new(),
    };
    let mut previous = None;
    let stdin = BufReader::new(std::io::stdin().lock());
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = line.map_err(|err| format!("stdin: {err}"))?;
        let reply = match serde_json::from_str::<Mark>(&line) {
            Err(err) => {
                let reason = format!("отметка «{line}»: {err}");
                log.errors.push(reason.clone());
                Reply::Error { index: 0, reason }
            }
            Ok(mark) => match read(&mark, stand, &mut previous) {
                Ok(reading) => {
                    log.readings.push(reading.clone());
                    Reply::Reading(Box::new(reading))
                }
                Err(reason) => {
                    log.errors.push(reason.clone());
                    Reply::Error {
                        index: mark.index,
                        reason,
                    }
                }
            },
        };
        let mut bytes =
            serde_json::to_vec(&reply).map_err(|err| format!("сериализация ответа: {err}"))?;
        bytes.push(b'\n');
        stdout
            .write_all(&bytes)
            .and_then(|()| stdout.flush())
            .map_err(|err| format!("stdout: {err}"))?;
    }
    results::write_json(out, LOG_FILE, &log)?;
    Ok(())
}

pub fn main(rest: Vec<String>) -> i32 {
    let (stand, out) = match parse(rest) {
        Ok(parsed) => parsed,
        Err(err) => {
            eprintln!("probe-resources --observer: {err}");
            return 2;
        }
    };
    match serve(stand, &out) {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("probe-resources --observer: {err}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn arguments_need_stand_and_out() {
        assert_eq!(
            parse(args(&["--stand", "42", "--out", "/tmp/x"])).unwrap(),
            (42, PathBuf::from("/tmp/x"))
        );
        assert!(parse(args(&["--out", "/tmp/x"])).is_err());
        assert!(parse(args(&["--stand", "0", "--out", "/tmp/x"])).is_err());
        assert!(parse(args(&["--stand", "1", "--out", "/tmp/x", "extra"])).is_err());
    }

    #[test]
    fn reading_of_own_process_has_cpu_and_footprint() {
        let mut previous = None;
        let pid = std::process::id() as i32;
        let first = read(&Mark { index: 0, ops: 0 }, pid, &mut previous).unwrap();
        assert!(first.background.is_none());
        assert!(first.stand_footprint > 0);
        let second = read(&Mark { index: 1, ops: 5 }, pid, &mut previous).unwrap();
        assert_eq!(second.ops, 5);
        assert!(second.uptime_ns > first.uptime_ns);
        assert!(second.stand_cpu_mach >= first.stand_cpu_mach);
        let check = second.background.unwrap();
        assert!(check.stand_pids.contains(&pid));
    }

    #[test]
    fn reading_with_failed_foreign_reads_round_trips() {
        let mut previous = None;
        let pid = std::process::id() as i32;
        read(&Mark { index: 0, ops: 0 }, pid, &mut previous).unwrap();
        let mut reading = read(&Mark { index: 1, ops: 0 }, pid, &mut previous).unwrap();
        let check = reading.background.as_mut().unwrap();
        check.coverage.failed_by_errno.insert(libc::EPERM, 223);
        let line = serde_json::to_string(&Reply::Reading(Box::new(reading))).unwrap();
        let Reply::Reading(parsed) = serde_json::from_str::<Reply>(&line).unwrap() else {
            panic!("ожидался Reading: {line}");
        };
        assert_eq!(parsed.index, 1);
        let coverage = &parsed.background.unwrap().coverage;
        assert_eq!(coverage.failed_by_errno.get(&libc::EPERM), Some(&223));
    }

    #[test]
    fn replies_round_trip() {
        let reply = Reply::Error {
            index: 3,
            reason: "x".to_owned(),
        };
        let line = serde_json::to_string(&reply).unwrap();
        assert_eq!(serde_json::from_str::<Reply>(&line).unwrap(), reply);
    }
}
