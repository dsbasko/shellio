//! Шаги сценария `bounded` (строка отчёта «Обход `bounded`»): 30 шагов прокрутки по экрану от
//! начала, `G`, `gg`, 10 × `Ctrl-d`, 10 прыжков по 50 экранов от начала. После каждого шага
//! виды строк B сверяются с пределом по видимым строкам таблицы, в конце обхода — записи и
//! предел кэшей строк и блоков A, раскладки дальше 2 экранов у обоих вариантов. Затем 100
//! циклов снятия стенда и открытия той же фикстуры, срезы счётчиков после 10-го и 100-го.

use probe_diff_view::model::state::Command;

use super::bounded::{self, CYCLES, FIRST_CUT, RowViews};
use super::contract::Check;
use super::contract_run::{Ctx, Next, Restage, STEP_DEADLINE, Script, Wait};
use super::counters::{self, Count};
use super::stage::View;

/// Шагов прокрутки по экрану.
const SCREEN_STEPS: u32 = 30;
/// Нажатий `Ctrl-d`.
const HALF_PAGES: u32 = 10;
/// Прыжков и их шаг в экранах.
const JUMPS: u32 = 10;
const JUMP_SCREENS: f64 = 50.0;
/// Допуск положения верха clip view, точки.
const TOLERANCE: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stimulus {
    Screen(u32),
    Bottom,
    Top,
    HalfPage(u32),
    Jump(u32),
    Totals,
    Cycle(usize),
    Cycles,
}

impl Stimulus {
    fn name(self) -> String {
        match self {
            Stimulus::Screen(step) => format!("screen {step}"),
            Stimulus::Bottom => "G".to_owned(),
            Stimulus::Top => "gg".to_owned(),
            Stimulus::HalfPage(step) => format!("Ctrl-d {step}"),
            Stimulus::Jump(step) => format!("jump {step}"),
            Stimulus::Totals => "totals".to_owned(),
            Stimulus::Cycle(cycle) => format!("cycle {cycle}"),
            Stimulus::Cycles => "cycles".to_owned(),
        }
    }

    /// Шаг обхода: после него сверяются виды строк B.
    fn traversal(self) -> bool {
        matches!(
            self,
            Stimulus::Screen(_)
                | Stimulus::Bottom
                | Stimulus::Top
                | Stimulus::HalfPage(_)
                | Stimulus::Jump(_)
        )
    }
}

fn steps() -> Vec<Stimulus> {
    (1..=SCREEN_STEPS)
        .map(Stimulus::Screen)
        .chain([Stimulus::Bottom, Stimulus::Top])
        .chain((1..=HALF_PAGES).map(Stimulus::HalfPage))
        .chain((1..=JUMPS).map(Stimulus::Jump))
        .chain([Stimulus::Totals])
        .chain((1..=CYCLES).map(Stimulus::Cycle))
        .chain([Stimulus::Cycles])
        .collect()
}

#[derive(Default)]
pub struct BoundedScript {
    steps: Vec<Stimulus>,
    next: usize,
    current: Option<Stimulus>,
    /// Циклов, после которых открытие закончено.
    reopened: usize,
    first: Option<Vec<Count>>,
    last: Option<Vec<Count>>,
}

impl BoundedScript {
    pub fn new() -> BoundedScript {
        BoundedScript {
            steps: steps(),
            ..BoundedScript::default()
        }
    }

    fn limit(ctx: &Ctx<'_>) -> Result<f64, String> {
        ctx.stage
            .scene
            .scroll_limit()
            .map(|(_, limit)| limit as f64)
            .ok_or_else(|| "нет предела прокрутки".to_owned())
    }

    /// Верх clip view на `y`, не дальше предела: ожидание, пока вид его не достигнет.
    fn scroll(ctx: &Ctx<'_>, name: String, y: f64) -> Result<Wait, String> {
        let target = y.floor().min(Self::limit(ctx)?).max(0.0);
        ctx.command(Command::ScrollTo(target as u64));
        Ok(Wait::new(name, Vec::new(), STEP_DEADLINE)
            .until(move |ctx| Ok((ctx.stage.view.viewport_y() - target).abs() <= TOLERANCE)))
    }

    fn stimulus(&mut self, ctx: &Ctx<'_>, stimulus: Stimulus) -> Result<Next, String> {
        let name = stimulus.name();
        let screen = ctx.viewport_height();
        let wait = match stimulus {
            Stimulus::Screen(step) => Self::scroll(ctx, name, f64::from(step) * screen)?,
            Stimulus::Jump(step) => {
                Self::scroll(ctx, name, f64::from(step) * JUMP_SCREENS * screen)?
            }
            Stimulus::Bottom => {
                let limit = Self::limit(ctx)?;
                Wait::new(name, ctx.press("G")?, STEP_DEADLINE)
                    .until(move |ctx| Ok((ctx.stage.view.viewport_y() - limit).abs() <= TOLERANCE))
            }
            Stimulus::Top => Wait::new(name, ctx.press("gg")?, STEP_DEADLINE)
                .until(|ctx| Ok(ctx.stage.view.viewport_y() <= TOLERANCE)),
            Stimulus::HalfPage(_) => {
                let before = ctx.stage.view.viewport_y();
                Wait::new(name, ctx.press("Ctrl-d")?, STEP_DEADLINE)
                    .until(move |ctx| Ok(ctx.stage.view.viewport_y() > before + TOLERANCE))
            }
            Stimulus::Totals | Stimulus::Cycles => Wait::new(name, Vec::new(), STEP_DEADLINE),
            Stimulus::Cycle(_) => {
                return Ok(Next::Restage(Restage {
                    fixture: ctx.fixture.clone(),
                    quiet: true,
                    cycle: true,
                }));
            }
        };
        Ok(Next::Wait(wait))
    }

    /// Виды строк B против предела по видимым строкам таблицы.
    fn checked_row_views(ctx: &Ctx<'_>, name: &str, checks: &mut Vec<Check>) {
        if let View::B(view) = &ctx.stage.view {
            checks.push(bounded::check_row_views(
                name,
                RowViews {
                    views: view.available_row_views(),
                    visible: view.visible_table_rows().len(),
                },
            ));
        }
    }

    /// Итоги обхода: кэши A и раскладки дальше 2 экранов. Предел кэша 0 — сломанный стенд.
    fn checked_totals(ctx: &Ctx<'_>, checks: &mut Vec<Check>) -> Result<(), String> {
        match &ctx.stage.view {
            View::A(view) => {
                let (rows, blocks) = (view.cache_counters(), view.block_cache_counters());
                for (name, counters) in [("cache rows", rows), ("cache blocks", blocks)] {
                    if counters.max_capacity == 0 {
                        return Err(format!("{name}: предел кэша не установлен"));
                    }
                    checks.push(bounded::check_cache(
                        name,
                        counters.max_entries,
                        counters.max_capacity,
                    ));
                }
                checks.push(bounded::check_far(
                    "far layouts",
                    rows.far_builds + blocks.far_builds,
                ));
            }
            View::B(view) => {
                checks.push(bounded::check_far("far layouts", view.stats().far_builds));
            }
        }
        Ok(())
    }
}

impl Script for BoundedScript {
    fn next(&mut self, ctx: &Ctx<'_>) -> Result<Next, String> {
        if let Some(Stimulus::Cycle(cycle)) = self.current {
            self.reopened = cycle;
            if cycle == FIRST_CUT {
                self.first = Some(counters::snapshot());
            }
            if cycle == CYCLES {
                self.last = Some(counters::snapshot());
            }
        }
        let Some(&stimulus) = self.steps.get(self.next) else {
            return Ok(Next::Done);
        };
        self.next += 1;
        self.current = Some(stimulus);
        self.stimulus(ctx, stimulus)
    }

    fn checked(
        &mut self,
        ctx: &Ctx<'_>,
        name: &str,
        checks: &mut Vec<Check>,
    ) -> Result<(), String> {
        let stimulus = self
            .current
            .filter(|stimulus| stimulus.name() == name)
            .ok_or_else(|| format!("проверка шага {name} без стимула"))?;
        if stimulus.traversal() {
            Self::checked_row_views(ctx, name, checks);
            return Ok(());
        }
        match stimulus {
            Stimulus::Totals => Self::checked_totals(ctx, checks),
            Stimulus::Cycles => {
                let (Some(first), Some(last)) = (&self.first, &self.last) else {
                    return Err(format!(
                        "срезы счётчиков не сняты: открыто циклов {}",
                        self.reopened
                    ));
                };
                checks.push(bounded::check_cycles(first, last));
                Ok(())
            }
            _ => Err(format!("шаг {name} без проверки")),
        }
    }

    fn finish(&mut self, report: &mut crate::results::Report, _checks: &[Check]) {
        report.push("bounded_cycles", self.reopened);
        for (key, counts) in [
            ("bounded_counters_first", &self.first),
            ("bounded_counters_last", &self.last),
        ] {
            if let Some(counts) = counts {
                report.push(
                    key,
                    counts
                        .iter()
                        .map(|count| format!("{} {}/{}", count.name, count.created, count.released))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_follow_the_report_row() {
        let steps = steps();
        let traversal = steps.iter().filter(|step| step.traversal()).count();
        assert_eq!(traversal, (SCREEN_STEPS + 2 + HALF_PAGES + JUMPS) as usize);
        let cycles = steps
            .iter()
            .filter(|step| matches!(step, Stimulus::Cycle(_)))
            .count();
        assert_eq!(cycles, CYCLES);
        assert_eq!(steps.last(), Some(&Stimulus::Cycles));
        let totals = steps.iter().position(|step| *step == Stimulus::Totals);
        assert_eq!(totals, Some(traversal));
        for keys in ["G", "gg", "Ctrl-d"] {
            assert!(super::super::events::sequence(keys).is_some(), "{keys}");
        }
    }
}
