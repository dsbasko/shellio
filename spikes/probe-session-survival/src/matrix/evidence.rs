//! Доказательства клетки и сама клетка для сценариев матрицы и стенда: проверки части, сырые
//! наблюдения и сведение их в итог. Отказ кандидата или C API — проверка с `ok: false`, ошибка
//! стенда — `Err` вызывающего, а не проверка.

use std::fmt::Display;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::results::{Cell, DefectOrigin, NotRunCause, Requirement, Status};

#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// Проверки части клетки и сырые наблюдения. Отказ кандидата — проверка с `ok: false`; ошибка
/// стенда — `Err` вызывающего, а не проверка.
#[derive(Debug, Default, Serialize)]
pub struct Evidence {
    pub checks: Vec<Check>,
    /// Стенд не доказал условие клетки: часть получает `not_run`.
    pub not_proven: Option<String>,
    pub observed: Map<String, Value>,
}

impl Evidence {
    pub fn check(&mut self, name: &str, ok: bool, detail: impl Display) -> bool {
        self.checks.push(Check {
            name: name.to_owned(),
            ok,
            detail: detail.to_string(),
        });
        ok
    }

    /// Шаг кандидата: значение при успехе, проваленная проверка при отказе.
    pub fn require<T, E: Display>(&mut self, name: &str, result: Result<T, E>) -> Option<T> {
        match result {
            Ok(value) => {
                self.check(name, true, "ok");
                Some(value)
            }
            Err(err) => {
                self.check(name, false, err);
                None
            }
        }
    }

    pub fn note(&mut self, key: &str, value: impl Serialize) {
        self.observed.insert(
            key.to_owned(),
            serde_json::to_value(value).unwrap_or(Value::Null),
        );
    }

    pub fn status(&self) -> (Status, String) {
        let failed: Vec<String> = self
            .checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| format!("{}: {}", check.name, check.detail))
            .collect();
        if !failed.is_empty() {
            (Status::Fail, failed.join("; "))
        } else if let Some(reason) = &self.not_proven {
            (Status::NotRun, reason.clone())
        } else {
            let names: Vec<&str> = self
                .checks
                .iter()
                .map(|check| check.name.as_str())
                .collect();
            (Status::Pass, format!("выполнено: {}", names.join("; ")))
        }
    }
}

pub struct Spec<'a> {
    pub id: &'a str,
    pub part: Option<&'a str>,
    pub stimulus: String,
    pub oracle: &'a str,
    pub completion: &'a str,
    /// Где дефект при `fail`.
    pub origin: DefectOrigin,
}

pub fn cell(spec: Spec<'_>, evidence: &Evidence, artifacts: Vec<String>) -> Cell {
    let (status, reason) = evidence.status();
    Cell {
        id: spec.id.to_owned(),
        part: spec.part.map(str::to_owned),
        status,
        requirement: Requirement::Required,
        stimulus: spec.stimulus,
        oracle: spec.oracle.to_owned(),
        completion: spec.completion.to_owned(),
        not_run_cause: (status == Status::NotRun).then_some(NotRunCause::Stand),
        reason,
        defect_origin: (status == Status::Fail).then_some(spec.origin),
        artifacts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec<'static> {
        Spec {
            id: "capi-B3",
            part: Some("formatter"),
            stimulus: "стимул".to_owned(),
            oracle: "оракул",
            completion: "условие",
            origin: DefectOrigin::Vt,
        }
    }

    #[test]
    fn failed_check_wins_over_not_proven_and_keeps_origin() {
        let mut evidence = Evidence::default();
        evidence.check("первая", true, "ok");
        evidence.check("вторая", false, "расхождение");
        evidence.not_proven = Some("граница не доказана".to_owned());
        let cell = cell(spec(), &evidence, vec!["a.json".to_owned()]);
        assert_eq!(cell.status, Status::Fail);
        assert_eq!(cell.reason, "вторая: расхождение");
        assert_eq!(cell.defect_origin, Some(DefectOrigin::Vt));
        assert_eq!(cell.not_run_cause, None);
        assert!(cell.problems().is_empty(), "{:?}", cell.problems());
    }

    #[test]
    fn not_proven_gives_not_run_by_stand_and_pass_lists_checks() {
        let mut evidence = Evidence::default();
        evidence.check("первая", true, "ok");
        evidence.not_proven = Some("сравнение неполное".to_owned());
        let cell = cell(spec(), &evidence, Vec::new());
        assert_eq!(cell.status, Status::NotRun);
        assert_eq!(cell.not_run_cause, Some(NotRunCause::Stand));
        assert_eq!(cell.defect_origin, None);
        assert!(cell.problems().is_empty(), "{:?}", cell.problems());

        let mut evidence = Evidence::default();
        assert_eq!(evidence.require("шаг", Ok::<u8, String>(7)), Some(7));
        evidence.note("число", 7);
        let (status, reason) = evidence.status();
        assert_eq!(status, Status::Pass);
        assert_eq!(reason, "выполнено: шаг");
        assert_eq!(evidence.require("отказ", Err::<u8, _>("нет")), None);
        assert_eq!(evidence.status().0, Status::Fail);
    }
}
