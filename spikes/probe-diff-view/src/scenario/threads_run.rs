//! Шаги сценария `threads` (строка отчёта «Треды `threads`»): черновик на строке (щелчок по
//! контекстной R, `c`), на диапазоне (R2..R2+2, `v j j c`) и на диапазоне через границу ханка
//! (последняя строка ханка и первая следующего на новой стороне, `v j c`), у каждого текст `jk` и
//! `Cmd-Enter`. Затем 100 циклов `c` и `Escape` на строке без треда. Поле черновика одно.

use objc2_app_kit::NSEventModifierFlags;

use probe_diff_view::model::state::Command;
use probe_diff_view::model::threads::ThreadAnchor;
use probe_diff_view::model::{LineAddr, Side};

use super::contract::{Check, Kind};
use super::contract_run::{Ctx, Next, STEP_DEADLINE, Script, Wait};
use super::drawmark;
use super::events;
use super::selection::addr_at;
use super::threads::{
    DRAFT_CYCLES, check_block, check_field, crosses_hunk, hunk_boundary, last_row,
};

/// Текст черновика.
const TEXT: &str = "jk";
/// Видимых строк выше строки стимула.
const BELOW_TOP: u32 = 5;
/// Допуск поля над блоком черновика, точки.
const FIELD: f64 = 0.5;

/// Где открывается черновик.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Line,
    Range,
    Cross,
}

impl Target {
    fn name(self) -> &'static str {
        match self {
            Target::Line => "line",
            Target::Range => "range",
            Target::Cross => "cross",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stimulus {
    /// Подготовка: прокрутка к строке стимула и щелчок по ней.
    Place(Target),
    Open(Target),
    Type(Target),
    Save(Target),
    CycleClick,
    CycleOpen(usize),
    CycleCancel(usize),
}

impl Stimulus {
    fn name(self) -> String {
        match self {
            Stimulus::Place(target) => format!("{} place", target.name()),
            Stimulus::Open(target) => format!("{} c", target.name()),
            Stimulus::Type(target) => format!("{} text", target.name()),
            Stimulus::Save(target) => format!("{} Cmd-Enter", target.name()),
            Stimulus::CycleClick => "cycle place".to_owned(),
            Stimulus::CycleOpen(index) => format!("cycle {index} c"),
            Stimulus::CycleCancel(index) => format!("cycle {index} Escape"),
        }
    }
}

/// Шаги по порядку: три черновика, затем циклы.
fn steps() -> Vec<Stimulus> {
    let mut steps = Vec::new();
    for target in [Target::Line, Target::Range, Target::Cross] {
        steps.extend([
            Stimulus::Place(target),
            Stimulus::Open(target),
            Stimulus::Type(target),
            Stimulus::Save(target),
        ]);
    }
    steps.push(Stimulus::CycleClick);
    for index in 0..DRAFT_CYCLES {
        steps.extend([Stimulus::CycleOpen(index), Stimulus::CycleCancel(index)]);
    }
    steps
}

pub struct ThreadsScript {
    steps: Vec<Stimulus>,
    next: usize,
    current: Option<Stimulus>,
    /// Первая и последняя строка показа стимула черновика.
    rows: Option<(u32, u32)>,
    anchor: Option<ThreadAnchor>,
}

impl Default for ThreadsScript {
    fn default() -> Self {
        ThreadsScript {
            steps: steps(),
            next: 0,
            current: None,
            rows: None,
            anchor: None,
        }
    }
}

/// Строка показа без блоков под ней и без треда на ней.
fn free(ctx: &Ctx<'_>, row: u32) -> bool {
    ctx.stage.scene.block_height(row) == Some(0)
}

/// Контекстная строка показа сырого diff.
fn context(ctx: &Ctx<'_>, row: usize) -> bool {
    let lines = ctx.raw.row_lines(ctx.shown_mode(), row);
    !lines.is_empty()
        && lines
            .iter()
            .all(|&index| ctx.raw.lines[index].kind == Kind::Context)
}

impl ThreadsScript {
    fn rows(&self) -> Result<(u32, u32), String> {
        self.rows
            .ok_or_else(|| "строки стимула не выбраны".to_owned())
    }

    /// Первая и последняя строка стимула: R — свободная контекстная, R2..R2+2 — три свободные
    /// контекстные одного ханка, граница — последняя строка ханка и первая следующего.
    fn pick(&self, ctx: &Ctx<'_>, target: Target) -> Result<(u32, u32), String> {
        let mode = ctx.shown_mode();
        let visible = ctx.stage.scene.visible_rows();
        let from = visible.start + BELOW_TOP;
        let after = self.rows.map_or(from, |(_, last)| last.max(from) + 1);
        match target {
            Target::Line | Target::Range => {
                let span = if target == Target::Line { 1 } else { 3 };
                (after..visible.end.saturating_sub(span))
                    .find(|&row| {
                        (row..row + span)
                            .all(|other| context(ctx, other as usize) && free(ctx, other))
                            && (row..row + span).all(|other| {
                                ctx.raw
                                    .row_lines(mode, other as usize)
                                    .first()
                                    .map(|&i| ctx.raw.lines[i].hunk)
                                    == ctx
                                        .raw
                                        .row_lines(mode, row as usize)
                                        .first()
                                        .map(|&i| ctx.raw.lines[i].hunk)
                            })
                    })
                    .map(|row| (row, row + span - 1))
                    .ok_or_else(|| {
                        format!(
                            "нет свободных контекстных строк ({span}) в {after}..{}",
                            visible.end
                        )
                    })
            }
            Target::Cross => hunk_boundary(ctx.raw, mode, Side::New, 0)
                .map(|(last, first)| (last as u32, first as u32))
                .ok_or_else(|| "нет границы ханков на новой стороне".to_owned()),
        }
    }

    /// Якорь стимула по сырому diff: новая сторона от первой до последней строки.
    fn expected(&self, ctx: &Ctx<'_>) -> Result<ThreadAnchor, String> {
        let (first, last) = self.rows()?;
        let mode = ctx.shown_mode();
        let addr = |row: u32| -> Result<LineAddr, String> {
            addr_at(ctx.raw, mode, row as usize, Side::New)
                .ok_or_else(|| format!("у строки {row} нет новой стороны"))
        };
        let path = ctx.fixture.path.clone();
        let (from, to) = (addr(first)?, addr(last)?);
        Ok(if first == last {
            ThreadAnchor::at(path, from)
        } else {
            ThreadAnchor::range(path, Side::New, from.line, to.line)
        })
    }

    fn click(ctx: &Ctx<'_>, row: u32, label: &str) -> Result<Vec<u64>, String> {
        let point = ctx.point(row, Side::New)?;
        let gesture = events::mouse_down(
            ctx.mtm,
            point,
            ctx.window,
            NSEventModifierFlags::empty(),
            1,
            label,
        )?;
        Ok(vec![gesture.down, gesture.up(label)?])
    }

    fn press(ctx: &Ctx<'_>, names: &[&str]) -> Result<Vec<u64>, String> {
        let mut posted = Vec::new();
        for name in names {
            posted.extend(ctx.press(name)?);
        }
        Ok(posted)
    }

    fn stimulus(&mut self, ctx: &Ctx<'_>, stimulus: Stimulus) -> Result<Wait, String> {
        let name = stimulus.name();
        match stimulus {
            Stimulus::Place(target) => {
                if target == Target::Cross {
                    let (first, _) = self.pick(ctx, target)?;
                    let top = ctx
                        .top_of(first.saturating_sub(BELOW_TOP))
                        .ok_or_else(|| format!("нет верха строки {first}"))?;
                    ctx.command(Command::ScrollTo(top as u64));
                }
                let rows = self.pick(ctx, target)?;
                self.rows = Some(rows);
                self.anchor = Some(self.expected(ctx)?);
                let posted = Self::click(ctx, rows.0, &name)?;
                Ok(Wait::new(name, posted, STEP_DEADLINE))
            }
            Stimulus::Open(target) => {
                let keys: &[&str] = match target {
                    Target::Line => &["c"],
                    Target::Range => &["v", "j", "j", "c"],
                    Target::Cross => &["v", "j", "c"],
                };
                Ok(
                    Wait::new(name, Self::press(ctx, keys)?, STEP_DEADLINE).until(|ctx| {
                        Ok(ctx.stage.scene.draft().is_some()
                            && ctx.stage.view.draft_shown().is_some()
                            && ctx.stage.view.draft_focused())
                    }),
                )
            }
            Stimulus::Type(_) => Ok(
                Wait::new(name, Self::press(ctx, &["j", "k"])?, STEP_DEADLINE).until(|ctx| {
                    Ok(ctx
                        .stage
                        .scene
                        .draft()
                        .is_some_and(|draft| draft.text() == TEXT))
                }),
            ),
            Stimulus::Save(_) => {
                Ok(
                    Wait::new(name, Self::press(ctx, &["Cmd-Enter"])?, STEP_DEADLINE)
                        .until(|ctx| Ok(ctx.stage.scene.draft().is_none())),
                )
            }
            Stimulus::CycleClick => {
                let rows = self.pick(ctx, Target::Line)?;
                self.rows = Some(rows);
                self.anchor = Some(self.expected(ctx)?);
                let posted = Self::click(ctx, rows.0, &name)?;
                Ok(Wait::new(name, posted, STEP_DEADLINE))
            }
            Stimulus::CycleOpen(_) => Ok(Wait::new(name, Self::press(ctx, &["c"])?, STEP_DEADLINE)
                .until(|ctx| {
                    Ok(ctx.stage.scene.draft().is_some()
                        && ctx.stage.view.draft_shown().is_some()
                        && ctx.stage.view.draft_focused())
                })),
            Stimulus::CycleCancel(_) => {
                Ok(
                    Wait::new(name, Self::press(ctx, &["Escape"])?, STEP_DEADLINE).until(|ctx| {
                        Ok(ctx.stage.scene.draft().is_none()
                            && ctx.stage.view.draft_shown().is_none())
                    }),
                )
            }
        }
    }

    /// Черновик на якоре стимула, поле ровно над блоком черновика и с фокусом.
    fn check_open(&self, ctx: &Ctx<'_>, name: &str) -> Result<Check, String> {
        let expected = self
            .anchor
            .clone()
            .ok_or_else(|| "нет якоря стимула".to_owned())?;
        let draft = ctx.stage.scene.draft();
        let field = ctx.stage.view.draft_shown();
        let frame = ctx.stage.scene.draft_frame();
        let over = match (field, frame) {
            (Some(field), Some(frame)) => {
                (field.origin.x - frame.x).abs() <= FIELD
                    && (field.origin.y - frame.y).abs() <= FIELD
                    && (field.size.width - frame.width).abs() <= FIELD
                    && (field.size.height - frame.height).abs() <= FIELD
            }
            _ => false,
        };
        let focused = ctx.stage.view.draft_focused();
        let anchor = draft.as_ref().map(|draft| draft.anchor.clone());
        Ok(Check::new(
            name,
            anchor.as_ref() == Some(&expected) && over && focused,
            format!(
                "якорь черновика {anchor:?}, ожидался {expected:?}, поле {:?}, блок {:?}, фокус {focused}",
                field.map(crate::describe_rect),
                frame.map(|frame| (frame.x, frame.y, frame.width, frame.height))
            ),
        ))
    }

    /// Тред с текстом на якоре стимула, блок сразу под текстом последней строки якоря.
    fn check_saved(
        &self,
        ctx: &Ctx<'_>,
        target: Target,
        checks: &mut Vec<Check>,
    ) -> Result<(), String> {
        let name = format!("{} Cmd-Enter", target.name());
        let expected = self
            .anchor
            .clone()
            .ok_or_else(|| "нет якоря стимула".to_owned())?;
        let threads = ctx.stage.scene.threads();
        let thread = threads.iter().find(|thread| {
            thread.anchor == expected
                && thread
                    .comments
                    .last()
                    .is_some_and(|comment| comment.body == TEXT)
        });
        checks.push(Check::new(
            format!("{name} thread"),
            thread.is_some(),
            format!(
                "тред на {expected:?} с текстом {TEXT:?}: {:?}",
                thread.map(|thread| thread.id)
            ),
        ));
        if target == Target::Cross {
            checks.push(Check::new(
                format!("{name} crosses"),
                crosses_hunk(ctx.raw, &expected),
                format!("диапазон {expected:?} через границу ханка"),
            ));
        }
        let row = last_row(ctx.raw, ctx.shown_mode(), &expected)
            .ok_or_else(|| format!("нет последней строки якоря {expected:?}"))?;
        let (text, block) = drawmark::drawn_row(row as u32);
        checks.push(check_block(&format!("{name} block"), text, block));
        Ok(())
    }
}

impl Script for ThreadsScript {
    fn next(&mut self, ctx: &Ctx<'_>) -> Result<Next, String> {
        let Some(&stimulus) = self.steps.get(self.next) else {
            return Ok(Next::Done);
        };
        self.next += 1;
        self.current = Some(stimulus);
        self.stimulus(ctx, stimulus).map(Next::Wait)
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
            Stimulus::Place(_) | Stimulus::CycleClick => {
                let (first, _) = self.rows()?;
                let expected = addr_at(ctx.raw, ctx.shown_mode(), first as usize, Side::New);
                let cursor = ctx.stage.scene.cursor();
                if expected.is_none() || cursor != expected {
                    return Err(format!(
                        "щелчок подготовки не поставил курсор: {cursor:?}, ожидался {expected:?}"
                    ));
                }
            }
            Stimulus::Open(_) => checks.push(self.check_open(ctx, name)?),
            Stimulus::Type(_) => {
                let text = ctx.stage.scene.draft().map(|draft| draft.text().to_owned());
                checks.push(Check::new(
                    name,
                    text.as_deref() == Some(TEXT),
                    format!("текст черновика {text:?}"),
                ));
            }
            Stimulus::Save(target) => self.check_saved(ctx, target, checks)?,
            Stimulus::CycleOpen(index) => {
                if index == 0 || index + 1 == DRAFT_CYCLES {
                    checks.push(self.check_open(ctx, name)?);
                }
            }
            Stimulus::CycleCancel(index) => {
                if index + 1 == DRAFT_CYCLES {
                    let count = crate::draft_field::count();
                    checks.push(check_field("draft field", count.created, count.live()));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_cover_three_drafts_and_all_cycles() {
        let steps = steps();
        assert_eq!(steps.len(), 3 * 4 + 1 + 2 * DRAFT_CYCLES);
        assert_eq!(steps[1].name(), "line c");
        assert_eq!(
            steps.last().unwrap().name(),
            format!("cycle {} Escape", DRAFT_CYCLES - 1)
        );
        for name in ["c", "v", "j", "k", "Cmd-Enter", "Escape"] {
            assert!(events::sequence(name).is_some(), "{name}");
        }
    }
}
