//! Шаги сценария `pairs` (строка отчёта «Пары `pairs`»). Верх clip view ставится на 0, 25%,
//! 50%, 75% документа и на предел прокрутки. В каждом положении у каждой видимой строки показа
//! адреса половин модели сверяются с сырым diff, нарисованные номера гуттера — с адресами.
//!
//! В ячейке `wrap` «Рядом» затем стенд пересобирается с малой фикстурой `pair31`: у первых трёх
//! пар 3 и 1 при фактических колонках половины нарисованная высота строки равна 3 высотам
//! визуальной строки. Итог этих проверок — также находка `pairs_height31`.

use probe_diff_view::model::state::Command;
use probe_diff_view::model::{Mode, Side};
use probe_diff_view::open::OpenFixture;

use super::contract::{self, Check};
use super::contract_run::{Ctx, Next, Restage, STEP_DEADLINE, Script, Wait};
use super::drawmark;
use super::pairs;
use crate::results::Report;

/// Доли документа у верха clip view, затем предел прокрутки.
const FRACTIONS: [f64; 4] = [0.0, 0.25, 0.5, 0.75];
/// Пар 3 и 1, которые проверяются.
const ROWS31: usize = 3;
/// Префикс проверок пар 3 и 1.
const HEIGHT31: &str = "pair31";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stimulus {
    /// Положение по доле документа или предел прокрутки (`None`).
    Position(Option<usize>),
    Restage31,
    Row31(usize),
}

impl Stimulus {
    fn name(self) -> String {
        match self {
            Stimulus::Position(Some(index)) => format!("top {:.0}%", FRACTIONS[index] * 100.0),
            Stimulus::Position(None) => "top limit".to_owned(),
            Stimulus::Restage31 => "restage pair31".to_owned(),
            Stimulus::Row31(index) => format!("{HEIGHT31} row {}", index + 1),
        }
    }
}

fn steps(height31: bool) -> Vec<Stimulus> {
    let mut steps: Vec<Stimulus> = (0..FRACTIONS.len())
        .map(|index| Stimulus::Position(Some(index)))
        .chain([Stimulus::Position(None)])
        .collect();
    if height31 {
        steps.push(Stimulus::Restage31);
        steps.extend((0..ROWS31).map(Stimulus::Row31));
    }
    steps
}

pub struct PairsScript {
    steps: Vec<Stimulus>,
    height31: bool,
    next: usize,
    current: Option<Stimulus>,
    target: f64,
    rows31: Vec<usize>,
}

impl PairsScript {
    /// Пары 3 и 1 — только в ячейке `wrap` «Рядом» (строка отчёта «Пары `pairs`»).
    pub fn new(fixture: OpenFixture, mode: Mode) -> Result<PairsScript, String> {
        let height31 = match fixture {
            OpenFixture::Wrap => mode == Mode::SideBySide,
            OpenFixture::Modified => false,
            other => return Err(format!("у pairs нет шагов фикстуры {}", other.name())),
        };
        Ok(PairsScript {
            steps: steps(height31),
            height31,
            next: 0,
            current: None,
            target: 0.0,
            rows31: Vec::new(),
        })
    }

    /// Верх clip view шага: доля высоты документа, не дальше предела прокрутки.
    fn target(ctx: &Ctx<'_>, index: Option<usize>) -> Result<f64, String> {
        let (total, limit) = ctx
            .stage
            .scene
            .scroll_limit()
            .ok_or_else(|| "нет предела прокрутки".to_owned())?;
        Ok(match index {
            Some(index) => (FRACTIONS[index] * total as f64).floor().min(limit as f64),
            None => limit as f64,
        })
    }

    /// Пары 3 и 1 фикстуры `pair31` при колонках половины установленной геометрии.
    fn rows31(ctx: &Ctx<'_>) -> Result<Vec<usize>, String> {
        let columns = ctx
            .stage
            .scene
            .geometry()
            .ok_or_else(|| "нет геометрии".to_owned())?
            .wrap(Mode::SideBySide)
            .columns();
        let rows = pairs::rows_3_and_1(ctx.raw, &ctx.fixture.old, &ctx.fixture.new, columns);
        if rows.len() < ROWS31 {
            return Err(format!(
                "у {} при {columns} колонках половины пар 3 и 1 {}, нужно {ROWS31}",
                ctx.fixture.name,
                rows.len()
            ));
        }
        Ok(rows.into_iter().take(ROWS31).collect())
    }

    fn stimulus(&mut self, ctx: &Ctx<'_>, stimulus: Stimulus) -> Result<Next, String> {
        let name = stimulus.name();
        match stimulus {
            Stimulus::Position(index) => {
                let target = Self::target(ctx, index)?;
                self.target = target;
                ctx.command(Command::ScrollTo(target as u64));
                Ok(Next::Wait(
                    Wait::new(name, Vec::new(), STEP_DEADLINE)
                        .until(move |ctx| Ok((ctx.stage.view.viewport_y() - target).abs() <= 1.0)),
                ))
            }
            Stimulus::Restage31 => Ok(Next::Restage(Restage {
                fixture: pairs::pair31(),
                quiet: false,
                cycle: false,
            })),
            Stimulus::Row31(index) => {
                if ctx.fixture.name != "pair31" {
                    return Err(format!("шаг {name} на фикстуре {}", ctx.fixture.name));
                }
                if ctx.shown_mode() != Mode::SideBySide {
                    return Err(format!("pair31 показана в {:?}", ctx.shown_mode()));
                }
                if self.rows31.is_empty() {
                    self.rows31 = Self::rows31(ctx)?;
                }
                let row = self.rows31[index] as u32;
                let top = ctx
                    .top_of(row)
                    .ok_or_else(|| format!("нет верха строки {row}"))?;
                ctx.command(Command::ScrollTo(top as u64));
                Ok(Next::Wait(
                    Wait::new(name, Vec::new(), STEP_DEADLINE)
                        .until(move |ctx| Ok(ctx.stage.scene.visible_rows().contains(&row))),
                ))
            }
        }
    }

    /// Адреса половин и гуттер видимых строк: по проверке на положение.
    fn checked_position(&self, ctx: &Ctx<'_>, name: &str, checks: &mut Vec<Check>) {
        let mode = ctx.shown_mode();
        let visible = ctx.stage.scene.visible_rows();
        let ledger = ctx.stage.ledger.borrow();
        let halves: &[Option<Side>] = match mode {
            Mode::Unified => &[None],
            Mode::SideBySide => &[Some(Side::Old), Some(Side::New)],
        };
        let (mut addrs, mut gutters) = (Vec::new(), Vec::new());
        for row in visible.clone() {
            let old = ctx.stage.scene.row_addr_at(row, Side::Old);
            let new = ctx.stage.scene.row_addr_at(row, Side::New);
            if let Err(reason) = pairs::model_matches(ctx.raw, mode, row as usize, old, new) {
                addrs.push(reason);
            }
            let Some(expected) = pairs::expected_gutter(ctx.raw, mode, row as usize) else {
                gutters.push(format!("строки {row} нет среди строк сырого diff"));
                continue;
            };
            let generation = ledger.required(row as usize);
            for &half in halves {
                let drawn = drawmark::gutter_of(row, half);
                let drawn = drawn.as_ref().map(|(at, text)| (*at, text.as_str()));
                if let Err(reason) = pairs::gutter_matches(
                    row,
                    half,
                    pairs::half_gutter(expected, half),
                    drawn,
                    generation,
                ) {
                    gutters.push(reason);
                }
            }
        }
        let rows = visible.end.saturating_sub(visible.start);
        let mut push = |kind: &str, errors: Vec<String>| {
            let detail = if errors.is_empty() {
                format!(
                    "верх {:.1}, видимые {}..{}, строк {rows}, расхождений 0",
                    self.target, visible.start, visible.end
                )
            } else {
                format!(
                    "верх {:.1}, видимые {}..{}, строк {rows}, расхождений {}: {}",
                    self.target,
                    visible.start,
                    visible.end,
                    errors.len(),
                    errors
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            };
            checks.push(Check::new(
                format!("{name} {kind}"),
                errors.is_empty(),
                detail,
            ));
        };
        push(
            match mode {
                Mode::Unified => "rows",
                Mode::SideBySide => "pairs",
            },
            addrs,
        );
        push("gutter", gutters);
    }

    fn checked_row31(
        &self,
        ctx: &Ctx<'_>,
        name: &str,
        index: usize,
        checks: &mut Vec<Check>,
    ) -> Result<(), String> {
        let row = self.rows31[index] as u32;
        let generation = ctx.stage.ledger.borrow().required(row as usize);
        let (drawn, _) = drawmark::drawn_row(row);
        let drawn = drawn.ok_or_else(|| format!("строка {row} не отрисована"))?;
        if drawn.generation < generation {
            return Err(format!(
                "строка {row} отрисована поколением {}, требуется {generation}",
                drawn.generation
            ));
        }
        let model = ctx.text_height(row);
        let result = pairs::height31_matches(row, drawn.height, ctx.line_height());
        let detail = match &result {
            Ok(()) => format!(
                "строка {row}: нарисована высота {:.1}, по индексу {model:?}",
                drawn.height
            ),
            Err(reason) => format!("{reason}, по индексу {model:?}"),
        };
        checks.push(Check::new(name, result.is_ok(), detail));
        Ok(())
    }
}

impl Script for PairsScript {
    fn next(&mut self, ctx: &Ctx<'_>) -> Result<Next, String> {
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
        match stimulus {
            Stimulus::Position(_) => {
                self.checked_position(ctx, name, checks);
                Ok(())
            }
            Stimulus::Row31(index) => self.checked_row31(ctx, name, index, checks),
            Stimulus::Restage31 => Err(format!("шаг {name} без ожидания")),
        }
    }

    fn finish(&mut self, report: &mut Report, checks: &[Check]) {
        if !self.height31 {
            return;
        }
        let height31: Vec<Check> = checks
            .iter()
            .filter(|check| check.step.starts_with(HEIGHT31))
            .cloned()
            .collect();
        let (verdict, reason) = contract::judge("pairs_height31", &height31);
        report.finding("pairs_height31", verdict, reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_follow_the_report_row() {
        let plain: Vec<String> = steps(false).iter().map(|step| step.name()).collect();
        assert_eq!(
            plain,
            ["top 0%", "top 25%", "top 50%", "top 75%", "top limit"]
        );
        let side = steps(true);
        assert_eq!(side.len(), plain.len() + 1 + ROWS31);
        assert!(side[plain.len() + 1].name().starts_with(HEIGHT31));
        assert!(
            PairsScript::new(OpenFixture::Wrap, Mode::SideBySide)
                .unwrap()
                .height31
        );
        assert!(
            !PairsScript::new(OpenFixture::Wrap, Mode::Unified)
                .unwrap()
                .height31
        );
        assert!(
            !PairsScript::new(OpenFixture::Modified, Mode::SideBySide)
                .unwrap()
                .height31
        );
        assert!(PairsScript::new(OpenFixture::Threads, Mode::Unified).is_err());
    }
}
