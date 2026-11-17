//! Сценарий `apps-surface` (задача 19): части F4 в поверхности на кандидате процесса. Модель
//! частей — `matrix::apps`, клавиши и источники ввода — `matrix::apps_surface`. Пока водитель не
//! построен, каждая часть поверхности — `not_run` с причиной `pending`.

use crate::args::Target;
use crate::matrix::apps::{self, Outcome, Place};
use crate::results::{Environment, NotRunCause, Report};

pub fn run(target: &Target, _environment: &Environment) -> Result<Report, String> {
    target
        .backend
        .ok_or("предусловие: сценарий apps-surface идёт на кандидате (--backend)")?;
    let cells = apps::parts_of(Place::Surface)
        .map(|spec| {
            apps::cell(
                spec.name,
                &Outcome::not_run(NotRunCause::Pending, "водитель части ещё не построен"),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Report {
        cells,
        ..Report::default()
    })
}
