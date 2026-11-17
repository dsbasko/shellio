//! Матрица рубежей A–G: кандидаты, каталог клеток, сценарии и процесс сценария. Сценарии рубежей
//! добавляют задачи плана: сценарии стенда `smoke` и `capi` (ранняя проверка C API), рубеж A —
//! `lifecycle`, headless-матрица задачи 9 — `restore` (B), `queries` (D), `io` (E), `geometry`
//! (F1, F3) и `versions` (G1, G2). Сценарий поверхности задачи 10 — `report-policy` (политика
//! отчётов, клетки стенда `surface-*`), он идёт в бинаре `probe-session-surface` с функцией
//! `surface`. Общий стенд zmx — `stand`, чистые сценарные утилиты — `util`.

#[cfg(feature = "vt")]
pub mod capi;
pub mod evidence;
#[cfg(feature = "vt")]
pub mod geometry;
#[cfg(feature = "vt")]
pub mod io;
#[cfg(feature = "vt")]
pub mod lifecycle;
#[cfg(feature = "vt")]
pub mod publish;
#[cfg(feature = "vt")]
pub mod queries;
pub mod report_policy;
#[cfg(feature = "vt")]
pub mod restore;
#[cfg(feature = "vt")]
pub mod stand;
pub mod util;
#[cfg(feature = "vt")]
pub mod versions;

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::args::Target;
use crate::launch;
use crate::results::{
    self, Build, Cell, Environment, Execution, Hardware, Os, ProcessInfo, Report, Requirement,
    ResultFile, Status,
};
use crate::{EXIT_EXECUTION, EXIT_USAGE, oracle, say, sys};

/// Кандидат хоста сессии (ADR-0005).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Candidate {
    /// Закреплённый zmx с патчами: эталон и запасной вариант.
    Zmx,
    /// Минимальный Rust-хост на `libghostty-vt`.
    Rust,
}

impl Candidate {
    pub const ALL: [Candidate; 2] = [Candidate::Zmx, Candidate::Rust];

    pub fn name(self) -> &'static str {
        match self {
            Candidate::Zmx => "zmx",
            Candidate::Rust => "rust",
        }
    }

    pub fn from_name(name: &str) -> Option<Candidate> {
        Candidate::ALL
            .into_iter()
            .find(|candidate| candidate.name() == name)
    }
}

impl fmt::Display for Candidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Клетки матрицы по разделу плана «Рубежи и клетки». Каждая прогоняется на обоих кандидатах.
pub const MATRIX_CELLS: [&str; 38] = [
    "A1", "A2", "A3", "A4", "A5", "A6", "A7", // жизненный цикл
    "B1", "B2", "B3", "B4", "B5", "B6", "B7", "B8", "B9", // восстановление
    "C1", "C2", "C3", "C4", // мост в поверхность
    "D1", "D2", "D3", "D4", "D5", "D6", // запросы и эффекты
    "E1", "E2", "E3", "E4", "E5", // ввод-вывод
    "F1", "F2", "F3", "F4", // ввод и геометрия
    "G1", "G2", "G3", // версии
];

/// Клетки стенда, вне матрицы и без кандидата: самопроверка, сюжеты ранней проверки C API
/// (`capi-<клетка матрицы>`) и политики отчётов в поверхности (`surface-<клетка матрицы>`,
/// `surface-control`). Они доказывают осуществимость, а не `pass` клетки кандидата.
pub const STAND_CELLS: [&str; 19] = [
    SMOKE_CELL,
    "capi-B1",
    "capi-B2",
    "capi-B3",
    "capi-B4",
    "capi-B5",
    "capi-B6",
    "capi-B7",
    "capi-B8",
    "capi-B9",
    "capi-D1",
    "capi-D2",
    "capi-D3",
    "capi-D4",
    "capi-budget",
    "surface-C1",
    "surface-C2",
    "surface-C3",
    "surface-control",
];

const SMOKE_CELL: &str = "smoke";

pub fn is_matrix_cell(id: &str) -> bool {
    MATRIX_CELLS.contains(&id)
}

pub fn is_known_cell(id: &str) -> bool {
    is_matrix_cell(id) || STAND_CELLS.contains(&id)
}

/// Порядок строк сводки: клетки матрицы по каталогу, затем клетки стенда, затем неизвестные.
pub fn cell_order(id: &str) -> usize {
    MATRIX_CELLS
        .iter()
        .chain(STAND_CELLS.iter())
        .position(|known| *known == id)
        .unwrap_or(MATRIX_CELLS.len() + STAND_CELLS.len())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    /// Самопроверка стенда: окружение процесса и `result.json` с одной клеткой.
    Smoke,
    /// Рубеж A: жизненный цикл сессии, клетки A1–A7.
    Lifecycle,
    /// Ранняя проверка C API `libghostty-vt` без хоста: сюжеты B1–B9 и D1–D4, бюджет continuation.
    Capi,
    /// Рубеж B: восстановление экрана, клетки B1–B9.
    Restore,
    /// Рубеж D: запросы терминала и эффекты, клетки D1–D6.
    Queries,
    /// Рубеж E: ввод-вывод, очереди и публикация сокета, клетки E1–E5.
    Io,
    /// Рубеж F без поверхности: источник размера и ввод из CLI, клетки F1 и F3.
    Geometry,
    /// Рубеж G без бандла: соседние поколения кандидата, клетки G1 и G2.
    Versions,
    /// Политика отчётов в минимальной поверхности (задача 10, часть B): клетки `surface-*`.
    ReportPolicy,
}

impl Scenario {
    pub const ALL: [Scenario; 9] = [
        Scenario::Smoke,
        Scenario::Lifecycle,
        Scenario::Capi,
        Scenario::Restore,
        Scenario::Queries,
        Scenario::Io,
        Scenario::Geometry,
        Scenario::Versions,
        Scenario::ReportPolicy,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Scenario::Smoke => "smoke",
            Scenario::Lifecycle => "lifecycle",
            Scenario::Capi => "capi",
            Scenario::Restore => "restore",
            Scenario::Queries => "queries",
            Scenario::Io => "io",
            Scenario::Geometry => "geometry",
            Scenario::Versions => "versions",
            Scenario::ReportPolicy => "report-policy",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL
            .into_iter()
            .find(|scenario| scenario.name() == name)
    }

    /// Кандидаты, на которых идёт сценарий; пусто у сценария стенда.
    pub fn candidates(self) -> &'static [Candidate] {
        match self {
            Scenario::Smoke | Scenario::Capi | Scenario::ReportPolicy => &[],
            Scenario::Lifecycle | Scenario::Io | Scenario::Restore | Scenario::Queries => {
                &Candidate::ALL
            }
            Scenario::Geometry | Scenario::Versions => &[Candidate::Zmx],
        }
    }

    /// Срок процесса сценария; внешний сторож `--launch` добавляет к нему запас.
    pub fn deadline(self) -> Duration {
        match self {
            Scenario::Smoke => Duration::from_secs(10),
            Scenario::Lifecycle | Scenario::Capi => Duration::from_secs(300),
            Scenario::Geometry | Scenario::Versions | Scenario::ReportPolicy => {
                Duration::from_secs(300)
            }
            Scenario::Restore | Scenario::Queries | Scenario::Io => Duration::from_secs(900),
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
        target.backend,
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
        Scenario::Smoke => smoke(&target.out, environment),
        Scenario::Lifecycle => lifecycle_run(target, environment),
        Scenario::Capi => capi_run(target, environment),
        Scenario::Restore => vt_only(target, environment, "restore"),
        Scenario::Queries => vt_only(target, environment, "queries"),
        Scenario::Io => vt_only(target, environment, "io"),
        Scenario::Geometry => vt_only(target, environment, "geometry"),
        Scenario::Versions => vt_only(target, environment, "versions"),
        Scenario::ReportPolicy => surface_run(target, environment),
    }
}

#[cfg(feature = "surface")]
fn surface_run(target: &Target, environment: &Environment) -> Result<Report, String> {
    crate::surface::scene::run(target, environment)
}

/// Сценарию нужна поверхность libghostty: без функции `surface` он не запускается.
#[cfg(not(feature = "surface"))]
fn surface_run(_target: &Target, _environment: &Environment) -> Result<Report, String> {
    Err("сценарий report-policy требует функцию surface".to_owned())
}

#[cfg(feature = "vt")]
fn vt_only(target: &Target, environment: &Environment, _name: &str) -> Result<Report, String> {
    match target.scenario {
        Scenario::Restore => restore::run(target, environment),
        Scenario::Queries => queries::run(target, environment),
        Scenario::Io => io::run(target, environment),
        Scenario::Geometry => geometry::run(target, environment),
        Scenario::Versions => versions::run(target, environment),
        other => Err(format!("сценарий {other} не из headless-матрицы")),
    }
}

/// Оракулам headless-матрицы нужен `libghostty-vt`: без функции `vt` сценарий не запускается.
#[cfg(not(feature = "vt"))]
fn vt_only(_target: &Target, _environment: &Environment, name: &str) -> Result<Report, String> {
    Err(format!("сценарий {name} требует функцию vt"))
}

#[cfg(feature = "vt")]
fn lifecycle_run(target: &Target, environment: &Environment) -> Result<Report, String> {
    lifecycle::run(target, environment)
}

#[cfg(feature = "vt")]
fn capi_run(target: &Target, environment: &Environment) -> Result<Report, String> {
    capi::run(target, environment)
}

/// Оракулам рубежа нужен `libghostty-vt`: без функции `vt` сценарий не запускается.
#[cfg(not(feature = "vt"))]
fn lifecycle_run(_target: &Target, _environment: &Environment) -> Result<Report, String> {
    Err("сценарий lifecycle требует функцию vt".to_owned())
}

#[cfg(not(feature = "vt"))]
fn capi_run(_target: &Target, _environment: &Environment) -> Result<Report, String> {
    Err("сценарий capi требует функцию vt".to_owned())
}

/// Строка итога процесса: исполнение и счётчики клеток.
pub fn describe(result: &ResultFile) -> String {
    let backend = result.backend.map_or("стенд", Candidate::name);
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
        "{} {backend} {}: execution={execution}; клетки: {}",
        result.scenario,
        result.variant,
        if counts.is_empty() {
            "нет".to_owned()
        } else {
            counts.join(", ")
        }
    )
}

/// Окружение процесса сценария. Версии и штампы наборов пробы приходят от `run.sh` через
/// запуск, версия профиля нормализаций — из `oracle::model`.
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
    let facts = |vars: &[(&str, &str)]| {
        vars.iter()
            .filter_map(|(name, key)| lookup(name).map(|value| ((*key).to_owned(), value)))
            .collect::<BTreeMap<_, _>>()
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
            features: enabled_features(),
        },
        versions: facts(&launch::SET_VERSIONS)
            .into_iter()
            .chain(facts(&launch::SURFACE_FACTS[..1]))
            .collect(),
        stamps: facts(&launch::SET_STAMPS)
            .into_iter()
            .chain(facts(&launch::SURFACE_FACTS[1..]))
            .collect(),
        normalization_profile: Some(oracle::model::PROFILE_VERSION),
        process: ProcessInfo {
            pid: std::process::id(),
            nonce,
            state_dir: lookup(launch::STATE_DIR),
            env: entry_env.iter().cloned().collect(),
            env_extra,
        },
    }
}

fn enabled_features() -> Vec<String> {
    [
        ("vt", cfg!(feature = "vt")),
        ("surface", cfg!(feature = "surface")),
        ("proto-v1", cfg!(feature = "proto-v1")),
    ]
    .into_iter()
    .filter(|(_, enabled)| *enabled)
    .map(|(name, _)| name.to_owned())
    .collect()
}

/// Сырой артефакт `smoke`: окружение процесса отдельным файлом.
pub const SMOKE_ARTIFACT: &str = "environment.json";

fn smoke(out: &Path, environment: &Environment) -> Result<Report, String> {
    results::write_json(out, SMOKE_ARTIFACT, environment)?;
    Ok(smoke_report(environment))
}

/// Итог `smoke`: одна клетка стенда. Неполное окружение — `fail`, а не ошибка исполнения:
/// процесс дошёл до конца, но сведений для отчёта не хватает. Версии и штампы наборов здесь же:
/// расхождение имён переменных `run.sh` и запуска иначе дало бы пустые сведения без отказа.
pub fn smoke_report(environment: &Environment) -> Report {
    let mut missing: Vec<String> = Vec::new();
    let os = &environment.os;
    let hardware = &environment.hardware;
    if os.product_version.is_none() {
        missing.push("версия macOS".to_owned());
    }
    if os.build.is_none() {
        missing.push("сборка macOS".to_owned());
    }
    if hardware.model.is_none() {
        missing.push("модель".to_owned());
    }
    if hardware.cpu.is_none() {
        missing.push("процессор".to_owned());
    }
    if hardware.memory_bytes.is_none() {
        missing.push("объём памяти".to_owned());
    }
    if environment.process.nonce.is_none() {
        missing.push("nonce".to_owned());
    }
    if environment.process.state_dir.is_none() {
        missing.push(launch::STATE_DIR.to_owned());
    }
    for (_, key) in launch::SET_VERSIONS {
        if !environment.versions.contains_key(key) {
            missing.push(format!("версия {key}"));
        }
    }
    for (_, key) in launch::SET_STAMPS {
        if !environment.stamps.contains_key(key) {
            missing.push(format!("штамп {key}"));
        }
    }
    if environment.normalization_profile != Some(oracle::model::PROFILE_VERSION) {
        missing.push(format!(
            "профиль нормализаций {}",
            oracle::model::PROFILE_VERSION
        ));
    }
    let (status, reason) = if missing.is_empty() {
        (
            Status::Pass,
            format!(
                "macOS {} ({}), {}, {}, временный {} {}",
                os.product_version.as_deref().unwrap_or_default(),
                os.build.as_deref().unwrap_or_default(),
                hardware.model.as_deref().unwrap_or_default(),
                hardware.cpu.as_deref().unwrap_or_default(),
                launch::STATE_DIR,
                environment.process.state_dir.as_deref().unwrap_or_default()
            ),
        )
    } else {
        (
            Status::Fail,
            format!("в окружении нет: {}", missing.join(", ")),
        )
    };
    let mut report = Report::default();
    report.cells.push(Cell {
        id: SMOKE_CELL.to_owned(),
        part: None,
        status,
        requirement: Requirement::Required,
        stimulus: "процесс сценария запущен через --launch".to_owned(),
        oracle: "окружение процесса: ОС, железо, сборка, версии и штампы наборов пробы, nonce и \
                 временный SHELLIO_STATE_DIR"
            .to_owned(),
        completion: format!("записаны {SMOKE_ARTIFACT} и {}", results::RESULT_FILE),
        not_run_cause: None,
        reason,
        defect_origin: None,
        artifacts: vec![SMOKE_ARTIFACT.to_owned()],
    });
    report
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn environment() -> Environment {
        Environment {
            os: Os {
                product_version: Some("27.0.1".to_owned()),
                build: Some("26A434".to_owned()),
            },
            hardware: Hardware {
                model: Some("Mac15,6".to_owned()),
                cpu: Some("Apple M3 Pro".to_owned()),
                memory_bytes: Some(18 << 30),
            },
            build: Build {
                commit: Some("a9cbb05".to_owned()),
                rustc: Some("rustc 1.99.1".to_owned()),
                profile: "release".to_owned(),
                probe_version: "0.1.0".to_owned(),
                features: vec!["vt".to_owned()],
            },
            versions: launch::SET_VERSIONS
                .iter()
                .map(|(_, key)| ((*key).to_owned(), format!("{key} 1")))
                .collect(),
            stamps: launch::SET_STAMPS
                .iter()
                .map(|(_, key)| ((*key).to_owned(), "ab12".to_owned()))
                .collect(),
            normalization_profile: Some(oracle::model::PROFILE_VERSION),
            process: ProcessInfo {
                pid: 4242,
                nonce: Some("0123abcd".to_owned()),
                state_dir: Some("/tmp/shellio-probe-state.0123abcd".to_owned()),
                env: BTreeMap::new(),
                env_extra: Vec::new(),
            },
        }
    }

    #[test]
    fn candidates_and_scenarios_round_trip_names() {
        for candidate in Candidate::ALL {
            assert_eq!(Candidate::from_name(candidate.name()), Some(candidate));
        }
        assert_eq!(Candidate::from_name("zmx-prev"), None);
        for scenario in Scenario::ALL {
            assert_eq!(Scenario::from_name(scenario.name()), Some(scenario));
        }
        assert!(Scenario::Smoke.candidates().is_empty());
        assert!(Scenario::Capi.candidates().is_empty());
        assert_eq!(Scenario::Lifecycle.candidates(), Candidate::ALL);
        assert_eq!(Scenario::Io.candidates(), Candidate::ALL);
        assert_eq!(Scenario::Restore.candidates(), Candidate::ALL);
        assert_eq!(Scenario::Queries.candidates(), Candidate::ALL);
        assert_eq!(Scenario::Geometry.candidates(), [Candidate::Zmx]);
    }

    #[test]
    fn catalog_covers_gates_without_duplicates() {
        let mut ids: Vec<&str> = MATRIX_CELLS.to_vec();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MATRIX_CELLS.len());
        for (gate, count) in [
            ('A', 7),
            ('B', 9),
            ('C', 4),
            ('D', 6),
            ('E', 5),
            ('F', 4),
            ('G', 3),
        ] {
            assert_eq!(
                MATRIX_CELLS
                    .iter()
                    .filter(|id| id.starts_with(gate))
                    .count(),
                count,
                "{gate}"
            );
        }
        assert!(is_known_cell("smoke") && !is_matrix_cell("smoke"));
        let mut stand: Vec<&str> = STAND_CELLS.to_vec();
        stand.sort_unstable();
        stand.dedup();
        assert_eq!(stand.len(), STAND_CELLS.len());
        for id in STAND_CELLS.iter().filter_map(|id| id.strip_prefix("capi-")) {
            assert!(is_matrix_cell(id) || id == "budget", "{id}");
        }
        assert!(is_known_cell("capi-B7") && !is_matrix_cell("capi-B7"));
        assert!(!is_known_cell("H1"));
        assert!(cell_order("A1") < cell_order("G3"));
        assert!(cell_order("G3") < cell_order("smoke"));
        assert!(cell_order("smoke") < cell_order("H1"));
    }

    #[test]
    fn smoke_passes_with_full_environment() {
        let report = smoke_report(&environment());
        assert_eq!(report.cells.len(), 1);
        let cell = &report.cells[0];
        assert_eq!(cell.status, Status::Pass);
        assert!(cell.problems().is_empty(), "{:?}", cell.problems());
        assert!(cell.reason.contains("26A434"));
        assert_eq!(cell.artifacts, [SMOKE_ARTIFACT]);
    }

    #[test]
    fn smoke_fails_on_missing_facts() {
        let mut environment = environment();
        environment.os.build = None;
        environment.hardware.memory_bytes = None;
        let cell = &smoke_report(&environment).cells[0];
        assert_eq!(cell.status, Status::Fail);
        assert_eq!(cell.reason, "в окружении нет: сборка macOS, объём памяти");
        assert!(cell.problems().is_empty());
    }

    #[test]
    fn smoke_fails_without_set_facts() {
        let mut environment = environment();
        environment.versions.remove("zmx-prev");
        environment.stamps.remove("zmx");
        let cell = &smoke_report(&environment).cells[0];
        assert_eq!(cell.status, Status::Fail);
        assert_eq!(cell.reason, "в окружении нет: версия zmx-prev, штамп zmx");
    }

    #[test]
    fn smoke_fails_without_current_normalization_profile() {
        let mut environment = environment();
        environment.normalization_profile = None;
        let cell = &smoke_report(&environment).cells[0];
        assert_eq!(cell.status, Status::Fail);
        assert_eq!(
            cell.reason,
            format!(
                "в окружении нет: профиль нормализаций {}",
                oracle::model::PROFILE_VERSION
            )
        );
        environment.normalization_profile = Some(oracle::model::PROFILE_VERSION + 1);
        assert_eq!(smoke_report(&environment).cells[0].status, Status::Fail);
    }

    #[test]
    fn describe_counts_cells_and_execution() {
        let ok = ResultFile::new(
            "smoke",
            None,
            "base",
            environment(),
            Execution::Ok,
            smoke_report(&environment()),
        );
        assert_eq!(
            describe(&ok),
            "smoke стенд base: execution=ok; клетки: pass 1"
        );
        let error = ResultFile::new(
            "smoke",
            None,
            "base",
            environment(),
            Execution::error("предусловие"),
            Report::default(),
        );
        assert_eq!(
            describe(&error),
            "smoke стенд base: execution=error (предусловие); клетки: нет"
        );
    }

    #[test]
    fn collected_environment_reads_system_and_entry() {
        let entry = vec![
            (launch::STATE_DIR.to_owned(), "/tmp/s".to_owned()),
            (launch::BUILD_COMMIT.to_owned(), "abc".to_owned()),
            (launch::BUILD_RUSTC.to_owned(), " ".to_owned()),
            (
                "SHELLIO_PROBE_VERSION_ZMX_PREV".to_owned(),
                "zmx 0.8.1".to_owned(),
            ),
            (
                "SHELLIO_PROBE_VERSION_ZIG_ZMX".to_owned(),
                "0.16.0".to_owned(),
            ),
            ("SHELLIO_PROBE_STAMP_ZMX".to_owned(), "ab12".to_owned()),
            ("SHELLIO_PROBE_STAMP_ZMX_PREV".to_owned(), String::new()),
        ];
        let environment = collect_environment(&entry, Some("n1".to_owned()), Vec::new());
        assert_eq!(
            environment.versions,
            BTreeMap::from([
                ("zig-zmx".to_owned(), "0.16.0".to_owned()),
                ("zmx-prev".to_owned(), "zmx 0.8.1".to_owned()),
            ])
        );
        assert_eq!(
            environment.stamps,
            BTreeMap::from([("zmx".to_owned(), "ab12".to_owned())])
        );
        assert!(environment.os.product_version.is_some());
        assert!(environment.hardware.memory_bytes.is_some());
        assert_eq!(environment.build.commit.as_deref(), Some("abc"));
        assert_eq!(environment.build.rustc, None);
        assert_eq!(environment.process.state_dir.as_deref(), Some("/tmp/s"));
        assert_eq!(environment.process.nonce.as_deref(), Some("n1"));
        assert_eq!(
            environment.normalization_profile,
            Some(oracle::model::PROFILE_VERSION)
        );
        assert_eq!(environment.process.env.len(), 7);
    }
}
