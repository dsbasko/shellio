//! Сценарии пробы и процесс сценария: предусловие окружения, сценарий, `result.json` в `--out`.
//! Сценарии добавляют задачи плана; задача 2 — сценарий стенда `smoke`, задача 3 — `host`.

pub mod host;
pub mod smoke;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use crate::args::Target;
use crate::launch;
use crate::results::{
    self, Build, Environment, Execution, Hardware, Os, ProcessInfo, Report, ResultFile, Status,
};
use crate::{EXIT_EXECUTION, EXIT_USAGE, say, sys};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// Самопроверка стенда: разрешения терминала исполнителя и окружение процесса.
    Smoke,
    /// Самопроверка стенда: минимальный хост, attach-клиент и нагрузка.
    Host,
}

impl Scenario {
    pub const ALL: [Scenario; 2] = [Scenario::Smoke, Scenario::Host];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Smoke => "smoke",
            Scenario::Host => "host",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL
            .into_iter()
            .find(|scenario| scenario.name() == name)
    }

    /// Срок процесса сценария; внешний сторож `--launch` добавляет к нему запас.
    pub fn deadline(self) -> Duration {
        match self {
            Scenario::Smoke => smoke::DEADLINE,
            Scenario::Host => host::DEADLINE,
        }
    }
}

impl fmt::Display for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Процесс сценария: предусловие окружения, сценарий, `result.json` в `--out` и итог в stdout.
/// Ненулевой код — только при ошибке исполнения.
pub fn process_main(
    target: &Target,
    nonce: Option<String>,
    entry_env: &[(String, String)],
) -> ExitCode {
    if !target.out.is_dir() {
        say(&format!(
            "{}: --out {}: каталога нет",
            crate::PROBE_NAME,
            target.out.display()
        ));
        return ExitCode::from(EXIT_USAGE);
    }
    let mut check = launch::check_env(entry_env, nonce.as_deref());
    let environment = collect_environment(entry_env, nonce, check.extra.clone());
    if let Some(state_dir) = &environment.process.state_dir
        && !Path::new(state_dir).is_dir()
    {
        check
            .violations
            .push(format!("{}={state_dir}: каталога нет", launch::STATE_DIR));
    }
    let (execution, report) = if check.violations.is_empty() {
        match run(target, &environment) {
            Ok(report) => (Execution::Ok, report),
            Err(reason) => (Execution::error(reason), Report::default()),
        }
    } else {
        (
            Execution::error(format!(
                "предусловие: окружение процесса: {}",
                check.violations.join("; ")
            )),
            Report::default(),
        )
    };
    let result = ResultFile::new(
        target.scenario.name(),
        &target.variant,
        environment,
        execution,
        report,
    );
    if let Err(reason) = results::write_result(&target.out, &result) {
        say(&format!("{}: {reason}", crate::PROBE_NAME));
        return ExitCode::from(EXIT_EXECUTION);
    }
    println!("{}", describe(&result));
    if result.execution.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_EXECUTION)
    }
}

fn run(target: &Target, environment: &Environment) -> Result<Report, String> {
    match target.scenario {
        Scenario::Smoke => smoke::run(&target.out, environment),
        Scenario::Host => host::run(&target.out, environment),
    }
}

/// Вспомогательный бинарь пробы рядом с раннером: `run.sh` собирает все бинари пакета в один
/// каталог.
pub fn sibling_binary(name: &str) -> Result<PathBuf, String> {
    let runner = std::env::current_exe().map_err(|err| format!("путь раннера: {err}"))?;
    let binary = runner
        .parent()
        .ok_or_else(|| format!("у {} нет каталога", runner.display()))?
        .join(name);
    if binary.is_file() {
        Ok(binary)
    } else {
        Err(format!(
            "предусловие: нет {} (run.sh собирает все бинари пакета)",
            binary.display()
        ))
    }
}

/// Строка итога процесса: исполнение и счётчики частей клеток.
pub fn describe(result: &ResultFile) -> String {
    let execution = match &result.execution {
        Execution::Ok => "ok".to_owned(),
        Execution::Error { reason, .. } => format!("error ({reason})"),
    };
    let counts: Vec<String> = Status::ALL
        .into_iter()
        .filter_map(|status| {
            let count = result
                .cells
                .iter()
                .filter(|cell| cell.status == status)
                .count();
            (count > 0).then(|| format!("{} {count}", status.name()))
        })
        .collect();
    format!(
        "{} {}: execution={execution}; клетки: {}",
        result.scenario,
        result.variant,
        if counts.is_empty() {
            "нет".to_owned()
        } else {
            counts.join(", ")
        }
    )
}

/// Окружение процесса сценария. Сведения о сборке и версии приходят от `run.sh` через запуск.
pub fn collect_environment(
    entry_env: &[(String, String)],
    nonce: Option<String>,
    env_extra: Vec<String>,
) -> Environment {
    let lookup = |name: &str| {
        entry_env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .filter(|value| !value.trim().is_empty())
    };
    Environment {
        os: Os {
            product_version: sys::sysctl_string("kern.osproductversion"),
            build: sys::sysctl_string("kern.osversion"),
        },
        hardware: Hardware {
            model: sys::sysctl_string("hw.model"),
            cpu: sys::sysctl_string("machdep.cpu.brand_string"),
            memory_bytes: sys::sysctl_u64("hw.memsize"),
        },
        build: Build {
            commit: lookup(launch::BUILD_COMMIT),
            rustc: lookup(launch::BUILD_RUSTC),
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_owned(),
            probe_version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        versions: launch::VERSIONS
            .iter()
            .filter_map(|(name, key)| lookup(name).map(|value| ((*key).to_owned(), value)))
            .collect::<BTreeMap<_, _>>(),
        process: ProcessInfo {
            pid: std::process::id(),
            nonce,
            state_dir: lookup(launch::STATE_DIR),
            env: entry_env.iter().cloned().collect(),
            env_extra,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::{Cell, Requirement};

    #[test]
    fn scenarios_round_trip_names() {
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::from_name(scenario.name()), Some(scenario));
            assert!(!scenario.deadline().is_zero());
        }
        assert_eq!(Scenario::from_name("swap"), None);
    }

    #[test]
    fn environment_takes_launch_facts() {
        let entry = vec![
            (launch::STATE_DIR.to_owned(), "/tmp/s".to_owned()),
            (launch::BUILD_COMMIT.to_owned(), "5c3d886".to_owned()),
            (launch::BUILD_RUSTC.to_owned(), " ".to_owned()),
            (
                "SHELLIO_PROBE_VERSION_HOMEBREW".to_owned(),
                "Homebrew 7.0.14".to_owned(),
            ),
        ];
        let environment = collect_environment(&entry, Some("n1".to_owned()), Vec::new());
        assert_eq!(environment.build.commit.as_deref(), Some("5c3d886"));
        assert_eq!(environment.build.rustc, None);
        assert_eq!(
            environment.versions.get("homebrew").map(String::as_str),
            Some("Homebrew 7.0.14")
        );
        assert_eq!(environment.process.state_dir.as_deref(), Some("/tmp/s"));
        assert_eq!(environment.process.nonce.as_deref(), Some("n1"));
        assert!(environment.os.product_version.is_some());
    }

    #[test]
    fn describe_counts_statuses() {
        let environment = collect_environment(&[], None, Vec::new());
        let mut result = ResultFile::new(
            "smoke",
            "base",
            environment,
            Execution::Ok,
            Report::default(),
        );
        assert_eq!(describe(&result), "smoke base: execution=ok; клетки: нет");
        result.cells.push(Cell {
            id: results::SMOKE_CELL.to_owned(),
            part: None,
            requirement: Requirement::Required,
            status: Status::Pass,
            stimulus: "s".to_owned(),
            oracle: "o".to_owned(),
            completion: "c".to_owned(),
            reason: "r".to_owned(),
            artifacts: vec!["a".to_owned()],
        });
        result.execution = Execution::error("нет разрешения");
        assert_eq!(
            describe(&result),
            "smoke base: execution=error (нет разрешения); клетки: pass 1"
        );
    }
}
