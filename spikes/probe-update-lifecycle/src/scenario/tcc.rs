//! Сценарий `tcc` — клетка-наблюдение E (план P0.5, раздел «TCC», задача 16).
//!
//! Варианты процесса — `plain` (хост запускается обычным `posix_spawn`) и `disclaim` (с
//! `responsibility_spawnattrs_setdisclaim`); `run.sh` запускает их по порядку отдельными
//! процессами сценария, то есть отдельными сессиями. Каждый процесс пишет все части обоих графов
//! (`allowed` — бандл пробы, `denied` — бандл `.denied` с хостом `<id>.denied.host` и нагрузкой
//! `<id>.denied.fixture`) с подрезультатами «чтение» и «атрибуция».
//!
//! Первое действие — проверка выдачи владельца (`tcc-grant.json` рядом с каталогом бандлов):
//! без действительной выдачи защищённых чтений нет вовсе, все части — `not_run` с причиной.
//! Иначе — живая базовая проверка: разрешённый бандл читает фикстуру, контроль получает `EPERM`.
//! После каждого открытия бандла сценарий сравнивает список окон со снимком до него: новое окно
//! вне тестового графа — кандидат окна запроса TCC. Граф части тогда снимается `SIGKILL`, кнопки
//! окна записываются, оставшееся окно закрывается синтетическим Escape, часть — `not_run`.
//! Матрица частей при действительной выдаче не написана и передана в P2.1 (отчёт P0.5, «Итог E
//! (второй запуск)»): при выдаче сценарий делает только базовую проверку.

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::Serialize;

use crate::generation::{self, Layout};
use crate::results::{Cell, Diagnostics, Environment, Report, Requirement, Status};
use crate::scenario::smoke::{WindowEntry, window_list};
use crate::scenario::{bundles, stand_inputs};
use crate::tcc::{self, Current, Op, OpReport, Role};
use crate::{spawn, window};

pub const DEADLINE: Duration = Duration::from_secs(300);
pub const CELL: &str = "E";
const COMPLETION: &str = "проверка выдачи, базовая проверка и части обоих графов записаны";
const GATE: &str = "tcc/gate.json";
const OPEN: &str = "/usr/bin/open";

/// Варианты запуска хоста по порядку.
pub const VARIANTS: [&str; 2] = ["plain", "disclaim"];

/// Часть клетки E в каждом графе.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Part {
    pub name: &'static str,
    pub stimulus: &'static str,
}

/// Части раздела «TCC» по порядку.
pub const PARTS: [Part; 6] = [
    Part {
        name: "gui-n",
        stimulus: "GUI N через open с --window --tcc-fixture читает фикстуру при старте",
    },
    Part {
        name: "host-n-live",
        stimulus: "нагрузка хоста N при живом GUI N: read <фикстура>",
    },
    Part {
        name: "host-n-after-gui-exit",
        stimulus: "та же нагрузка после выхода GUI N: read <фикстура>",
    },
    Part {
        name: "host-n-after-activation",
        stimulus: "та же нагрузка после активации N+1: read <фикстура>",
    },
    Part {
        name: "gui-n1",
        stimulus: "GUI N+1, запущенный помощником с аргументами режима N, читает фикстуру при старте",
    },
    Part {
        name: "host-n1",
        stimulus: "нагрузка нового хоста N+1: read <фикстура>",
    },
];

/// Подрезультат части.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sub {
    Read,
    Attribution,
}

impl Sub {
    pub const ALL: [Sub; 2] = [Sub::Read, Sub::Attribution];

    fn name(self) -> &'static str {
        match self {
            Sub::Read => "read",
            Sub::Attribution => "attribution",
        }
    }

    fn oracle(self, role: Role) -> &'static str {
        match (self, role) {
            (Sub::Read, Role::Allowed) => "чтение удалось, содержимое — текст фикстуры",
            (Sub::Read, Role::Denied) => {
                "EPERM; доступ отрицательного графа портит часть обоих графов (not_run)"
            }
            (Sub::Attribution, _) => {
                "ответственный PID (responsibility_get_pid_responsible_for_pid) и его идентичность записаны; успешное чтение атрибуцию не подтверждает"
            }
        }
    }
}

/// Имя части: `<вариант>-<граф>-<часть>-<подрезультат>`.
pub fn part_name(variant: &str, role: Role, part: &Part, sub: Sub) -> String {
    format!("{variant}-{}-{}-{}", role.name(), part.name, sub.name())
}

/// Все части процесса варианта `variant` со статусом `not_run` и причиной `reason`.
pub fn not_run_all(variant: &str, reason: &str, artifacts: &[&str]) -> Vec<Cell> {
    let mut cells = Vec::new();
    for role in Role::ALL {
        for part in &PARTS {
            for sub in Sub::ALL {
                cells.push(Cell {
                    id: CELL.to_owned(),
                    part: Some(part_name(variant, role, part, sub)),
                    requirement: Requirement::Observation,
                    status: Status::NotRun,
                    stimulus: format!(
                        "{}; хост — {}",
                        part.stimulus,
                        if variant == "disclaim" {
                            "posix_spawn с responsibility_spawnattrs_setdisclaim"
                        } else {
                            "обычный posix_spawn"
                        }
                    ),
                    oracle: sub.oracle(role).to_owned(),
                    completion: COMPLETION.to_owned(),
                    reason: reason.to_owned(),
                    artifacts: artifacts.iter().map(|path| (*path).to_owned()).collect(),
                });
            }
        }
    }
    cells
}

/// Сведения проверки выдачи и базовой проверки.
#[derive(Debug, Default, Serialize)]
struct Gate {
    variant: String,
    grant_file: String,
    grant_present: bool,
    grant_error: Option<String>,
    current: Vec<(String, String, String)>,
    valid: Option<String>,
    responsibility_available: bool,
    disclaim_available: bool,
    baseline: Vec<Baseline>,
    runs: Vec<generation::Run>,
}

#[derive(Debug, Serialize)]
struct Baseline {
    role: Role,
    open_code: Option<i32>,
    report: Option<OpReport>,
    new_windows: Vec<WindowEntry>,
    dialog: Option<DialogHandling>,
}

/// Что сделано с окном, появившимся при открытии бандла.
#[derive(Debug, Serialize)]
pub struct DialogHandling {
    pub killed: Vec<i64>,
    pub buttons: Vec<String>,
    pub texts: Vec<String>,
    pub escape: Option<Result<(), String>>,
    pub remaining: Vec<WindowEntry>,
}

/// Окно вне тестового графа появилось: граф `graph` снимается `SIGKILL`, кнопки окна
/// записываются, оставшееся окно закрывается синтетическим Escape.
pub fn handle_dialog(
    windows: &[WindowEntry],
    graph: &[i64],
    before: &[WindowEntry],
) -> DialogHandling {
    for pid in graph {
        // SAFETY: kill с номером процесса тестового графа.
        unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
    }
    let mut buttons = Vec::new();
    let mut texts = Vec::new();
    for window in windows {
        let dialog = window::ax_dialog(window.pid as i32);
        buttons.extend(dialog.buttons.iter().map(|button| format!("{button:?}")));
        texts.extend(dialog.texts);
    }
    let remaining = tcc::new_windows(before, &window_list(), graph);
    let escape = remaining
        .first()
        .map(|window| window::post_escape(window.pid as i32));
    DialogHandling {
        killed: graph.to_vec(),
        buttons,
        texts,
        escape,
        remaining: tcc::new_windows(before, &window_list(), graph),
    }
}

pub fn run(out: &Path, environment: &Environment) -> Result<Report, String> {
    let variant = environment_variant(out)?;
    let (_, _, layout) = stand_inputs(environment)?;
    let (bundles_dir, _) = bundles(environment)?;
    let grant_path = bundles_dir
        .parent()
        .ok_or("каталог бандлов без родителя")?
        .join(tcc::GRANT_FILE);
    let dir = out.join("tcc");
    fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut gate = Gate {
        variant: variant.clone(),
        grant_file: grant_path.display().to_string(),
        responsibility_available: tcc::responsible_pid(std::process::id() as i32).is_ok(),
        disclaim_available: tcc::disclaim_available(),
        ..Gate::default()
    };
    let current = tcc::current_signatures(&layout, &mut gate.runs)?;
    gate.current = current
        .iter()
        .map(|now| {
            (
                now.role.name().to_owned(),
                now.identifier.clone(),
                now.requirement.clone(),
            )
        })
        .collect();
    let checked = match tcc::read_grant(&grant_path) {
        Ok(grant) => {
            gate.grant_present = grant.is_some();
            tcc::check_grant(grant.as_ref(), &current)
        }
        Err(reason) => {
            gate.grant_present = true;
            gate.grant_error = Some(reason.clone());
            Err(format!("{}: {reason}", tcc::NO_GRANT))
        }
    };
    let reason = match checked {
        Err(reason) => reason,
        Ok(()) => match baseline(&layout, &current, &dir, &mut gate) {
            Err(reason) => reason,
            Ok(()) => {
                gate.valid = Some("выдача действительна, базовая проверка прошла".to_owned());
                "выдача действительна, базовая проверка прошла; матрица частей не написана, передана в P2.1 (отчёт P0.5, «Итог E (второй запуск)»)".to_owned()
            }
        },
    };
    results_gate(out, &gate)?;
    let notes = vec![format!(
        "E {variant}: {reason}; responsibility_get_pid_responsible_for_pid {}, responsibility_spawnattrs_setdisclaim {}",
        if gate.responsibility_available {
            "есть"
        } else {
            "нет"
        },
        if gate.disclaim_available {
            "есть"
        } else {
            "нет"
        }
    )];
    Ok(Report {
        cells: not_run_all(&variant, &reason, &[GATE]),
        diagnostics: Diagnostics {
            notes,
            partial: None,
        },
    })
}

fn results_gate(out: &Path, gate: &Gate) -> Result<(), String> {
    crate::results::write_json(&out.join("tcc"), "gate.json", gate).map(|_| ())
}

/// Вариант из имени каталога процесса `<сценарий>-<вариант>`.
fn environment_variant(out: &Path) -> Result<String, String> {
    let name = out
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("каталог процесса без имени")?;
    VARIANTS
        .into_iter()
        .find(|variant| name == format!("tcc-{variant}"))
        .map(str::to_owned)
        .ok_or_else(|| format!("каталог процесса {name}: ожидается tcc-plain или tcc-disclaim"))
}

/// Живая базовая проверка: разрешённый бандл читает, контроль получает `EPERM`.
fn baseline(
    layout: &Layout,
    current: &[Current],
    dir: &Path,
    gate: &mut Gate,
) -> Result<(), String> {
    let home = std::env::var_os("HOME").ok_or("нет HOME")?;
    let fixture = tcc::fixture_path(Path::new(&home));
    for now in current {
        let entry = layout.get(now.role.bundle())?;
        let report_path = dir.join(format!("baseline-{}.json", now.role.name()));
        let before = window_list();
        let opened = spawn::output(
            Path::new(OPEN),
            &tcc::open_args(&entry.app, Op::Read, &fixture, &report_path),
        )
        .map_err(|err| format!("open: {err}"))?;
        let report = tcc::read_report(&report_path).ok();
        let graph: Vec<i64> = report.iter().map(|report| i64::from(report.pid)).collect();
        let windows = tcc::new_windows(&before, &window_list(), &graph);
        let dialog = (!windows.is_empty()).then(|| handle_dialog(&windows, &graph, &before));
        let failed = match (&report, now.role) {
            _ if dialog.is_some() => {
                Some("появилось окно вне тестового графа: нужен ответ пользователя".to_owned())
            }
            (None, _) => Some("бандл не записал отчёт".to_owned()),
            (Some(report), Role::Allowed) if !report.read.read_fixture() => {
                Some(format!("разрешённый бандл: {}", report.read.describe()))
            }
            (Some(report), Role::Denied) if !report.read.denied() => Some(format!(
                "контроль: {}, ожидается EPERM",
                report.read.describe()
            )),
            _ => None,
        };
        gate.baseline.push(Baseline {
            role: now.role,
            open_code: opened.exit.code,
            report,
            new_windows: windows,
            dialog,
        });
        if let Some(failed) = failed {
            return Err(format!("{}: базовая проверка: {failed}", tcc::NO_GRANT));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_part_is_not_run_without_grant() {
        let cells = not_run_all("plain", "нет выдачи", &[GATE]);
        assert_eq!(cells.len(), Role::ALL.len() * PARTS.len() * Sub::ALL.len());
        assert!(cells.iter().all(|cell| cell.status == Status::NotRun
            && cell.requirement == Requirement::Observation
            && cell.reason == "нет выдачи"
            && cell.problems().is_empty()));
        let names: BTreeSet<_> = cells.iter().map(|cell| cell.part.clone()).collect();
        assert_eq!(names.len(), cells.len());
        assert!(names.contains(&Some("plain-denied-host-n1-attribution".to_owned())));
    }

    #[test]
    fn variants_name_their_parts() {
        let plain = not_run_all("plain", "r", &[]);
        let disclaim = not_run_all("disclaim", "r", &[]);
        assert!(plain.iter().zip(&disclaim).all(|(a, b)| a.part != b.part));
        assert!(disclaim[0].stimulus.contains("setdisclaim"));
        assert_eq!(
            environment_variant(Path::new("/r/tcc-disclaim")).as_deref(),
            Ok("disclaim")
        );
        assert!(environment_variant(Path::new("/r/tcc-base")).is_err());
    }
}
