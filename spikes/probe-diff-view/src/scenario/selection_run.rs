//! Шаги сценария `selection` (строка отчёта «Выделение `selection`»). R — первая строка участка
//! из 4 контекстных строк одного ханка среди видимых строк ниже пятой, сторона новая. Щелчок по
//! R, `v j j`, `k`, `Escape`, протяжка R → R+3, щелчок R и `Shift`-щелчок R+3, в «Рядом» щелчок
//! по старой и новой половине изменённой строки. Затем при выделении R..R+3 — сужение и возврат,
//! раскрытие и свёртка ближайшего треда не выше R, прокрутка на 3 экрана вниз и обратно: после
//! каждого выделение прежнее.

use objc2_app_kit::NSEventModifierFlags;

use probe_diff_view::model::state::{Command, Selection, ThreadId};
use probe_diff_view::model::{Mode, Side};

use super::contract::{Check, Kind};
use super::contract_run::{Ctx, Next, RELAYOUT_DEADLINE, STEP_DEADLINE, Script, Wait};
use super::events;
use super::selection::{CONTEXT_RUN, addr_at, check, check_cursor, context_run, expected};

/// Сужение окна, точки.
const NARROW: f64 = 120.0;
/// Видимых строк выше R.
const BELOW_TOP: u32 = 5;
/// Прокрутка за пределы выделения, экранов.
const SCREENS: f64 = 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stimulus {
    Click,
    Visual,
    Up,
    Escape,
    Drag,
    ShiftClick,
    HalfOld,
    HalfNew,
    Reselect,
    Narrow,
    Restore,
    Expand,
    Collapse,
    ScrollAway,
    ScrollBack,
}

impl Stimulus {
    fn name(self) -> &'static str {
        match self {
            Stimulus::Click => "click",
            Stimulus::Visual => "v j j",
            Stimulus::Up => "k",
            Stimulus::Escape => "Escape",
            Stimulus::Drag => "drag",
            Stimulus::ShiftClick => "shift-click",
            Stimulus::HalfOld => "half old",
            Stimulus::HalfNew => "half new",
            Stimulus::Reselect => "reselect",
            Stimulus::Narrow => "narrow",
            Stimulus::Restore => "restore",
            Stimulus::Expand => "expand",
            Stimulus::Collapse => "collapse",
            Stimulus::ScrollAway => "scroll away",
            Stimulus::ScrollBack => "scroll back",
        }
    }
}

const UNIFIED_STEPS: [Stimulus; 12] = [
    Stimulus::Click,
    Stimulus::Visual,
    Stimulus::Up,
    Stimulus::Escape,
    Stimulus::Drag,
    Stimulus::ShiftClick,
    Stimulus::Narrow,
    Stimulus::Restore,
    Stimulus::Expand,
    Stimulus::Collapse,
    Stimulus::ScrollAway,
    Stimulus::ScrollBack,
];

const SIDE_STEPS: [Stimulus; 15] = [
    Stimulus::Click,
    Stimulus::Visual,
    Stimulus::Up,
    Stimulus::Escape,
    Stimulus::Drag,
    Stimulus::ShiftClick,
    Stimulus::HalfOld,
    Stimulus::HalfNew,
    Stimulus::Reselect,
    Stimulus::Narrow,
    Stimulus::Restore,
    Stimulus::Expand,
    Stimulus::Collapse,
    Stimulus::ScrollAway,
    Stimulus::ScrollBack,
];

pub struct SelectionScript {
    steps: &'static [Stimulus],
    next: usize,
    current: Option<Stimulus>,
    row: Option<u32>,
    half: Option<u32>,
    base_width: f64,
    away_from: f64,
    thread: Option<ThreadId>,
}

impl SelectionScript {
    pub fn new(mode: Mode) -> SelectionScript {
        SelectionScript {
            steps: match mode {
                Mode::Unified => &UNIFIED_STEPS,
                Mode::SideBySide => &SIDE_STEPS,
            },
            next: 0,
            current: None,
            row: None,
            half: None,
            base_width: 0.0,
            away_from: 0.0,
            thread: None,
        }
    }

    fn row(&self) -> Result<u32, String> {
        self.row.ok_or_else(|| "строка R не выбрана".to_owned())
    }

    /// R среди видимых строк: участок из 4 контекстных строк одного ханка ниже пятой.
    fn pick(ctx: &Ctx<'_>) -> Result<u32, String> {
        let visible = ctx.stage.scene.visible_rows();
        let from = (visible.start + BELOW_TOP) as usize;
        let row = context_run(ctx.raw, ctx.shown_mode(), from)
            .ok_or_else(|| format!("нет участка из {CONTEXT_RUN} контекстных строк от {from}"))?;
        if row + CONTEXT_RUN > visible.end as usize {
            return Err(format!(
                "участок {row}..{} не среди видимых {visible:?}",
                row + CONTEXT_RUN
            ));
        }
        Ok(row as u32)
    }

    /// Видимая изменённая строка с обеими половинами разных строк сырого diff.
    fn pick_half(ctx: &Ctx<'_>) -> Result<u32, String> {
        let visible = ctx.stage.scene.visible_rows();
        (visible.start..visible.end)
            .find(|&row| {
                ctx.raw.pairs.get(row as usize).is_some_and(|pair| {
                    matches!((pair.old, pair.new), (Some(old), Some(new))
                        if old != new && ctx.raw.lines[old].kind == Kind::Removed)
                })
            })
            .ok_or_else(|| format!("нет видимой изменённой пары в {visible:?}"))
    }

    fn click(
        ctx: &Ctx<'_>,
        row: u32,
        side: Side,
        shift: bool,
        label: &str,
    ) -> Result<Vec<u64>, String> {
        let point = ctx.point(row, side)?;
        let flags = if shift {
            NSEventModifierFlags::Shift
        } else {
            NSEventModifierFlags::empty()
        };
        let gesture = events::mouse_down(ctx.mtm, point, ctx.window, flags, 1, label)?;
        Ok(vec![gesture.down, gesture.up(label)?])
    }

    /// Выделение R..R+3 по сырому diff.
    fn range(&self, ctx: &Ctx<'_>) -> Result<Option<Selection>, String> {
        let row = self.row()? as usize;
        Ok(expected(
            ctx.raw,
            ctx.shown_mode(),
            Side::New,
            row,
            row + CONTEXT_RUN - 1,
        ))
    }

    fn stimulus(&mut self, ctx: &Ctx<'_>, stimulus: Stimulus) -> Result<Wait, String> {
        let name = stimulus.name();
        let step = |posted| Ok(Wait::new(name, posted, STEP_DEADLINE));
        match stimulus {
            Stimulus::Click => {
                let row = Self::pick(ctx)?;
                self.row = Some(row);
                self.base_width = ctx.content_width();
                step(Self::click(ctx, row, Side::New, false, name)?)
            }
            Stimulus::Visual => step(
                ctx.press("v")?
                    .into_iter()
                    .chain(ctx.press("j")?)
                    .chain(ctx.press("j")?)
                    .collect(),
            ),
            Stimulus::Up => step(ctx.press("k")?),
            Stimulus::Escape => step(ctx.press("Escape")?),
            Stimulus::Drag => {
                let row = self.row()?;
                let (start, end) = (
                    ctx.point(row, Side::New)?,
                    ctx.point(row + CONTEXT_RUN as u32 - 1, Side::New)?,
                );
                let gesture = events::mouse_down(
                    ctx.mtm,
                    start,
                    ctx.window,
                    NSEventModifierFlags::empty(),
                    1,
                    name,
                )?;
                let posted = vec![gesture.down, gesture.drag_to(end, name)?, gesture.up(name)?];
                step(posted)
            }
            Stimulus::ShiftClick | Stimulus::Reselect => {
                let row = self.row()?;
                let mut posted = Self::click(ctx, row, Side::New, false, name)?;
                posted.extend(Self::click(
                    ctx,
                    row + CONTEXT_RUN as u32 - 1,
                    Side::New,
                    true,
                    name,
                )?);
                step(posted)
            }
            Stimulus::HalfOld | Stimulus::HalfNew => {
                let row = match self.half {
                    Some(row) => row,
                    None => {
                        let row = Self::pick_half(ctx)?;
                        self.half = Some(row);
                        row
                    }
                };
                let side = if stimulus == Stimulus::HalfOld {
                    Side::Old
                } else {
                    Side::New
                };
                step(Self::click(ctx, row, side, false, name)?)
            }
            Stimulus::Narrow | Stimulus::Restore => {
                let before = ctx.relayouts();
                let width = if stimulus == Stimulus::Narrow {
                    self.base_width - NARROW
                } else {
                    self.base_width
                };
                ctx.set_content_width(width);
                Ok(Wait::new(name, Vec::new(), RELAYOUT_DEADLINE)
                    .until(move |ctx| Ok(ctx.relayouts() > before)))
            }
            Stimulus::Expand | Stimulus::Collapse => {
                let id = match (stimulus, self.thread) {
                    (Stimulus::Collapse, Some(id)) => id,
                    (Stimulus::Collapse, None) => return Err("нет раскрытого треда".to_owned()),
                    _ => {
                        let id = thread_from(ctx, self.row()?)?;
                        self.thread = Some(id);
                        id
                    }
                };
                ctx.stage.scene.toggle_thread(id);
                step(Vec::new())
            }
            Stimulus::ScrollAway => {
                self.away_from = ctx.stage.view.viewport_y();
                let height = ctx.viewport_height();
                ctx.command(Command::ScrollTo(
                    (self.away_from + SCREENS * height).max(0.0) as u64,
                ));
                step(Vec::new())
            }
            Stimulus::ScrollBack => {
                ctx.command(Command::ScrollTo(self.away_from.max(0.0) as u64));
                step(Vec::new())
            }
        }
    }
}

/// Ближайший свёрнутый тред, блок которого не выше строки `row`.
fn thread_from(ctx: &Ctx<'_>, row: u32) -> Result<ThreadId, String> {
    let model = ctx.stage.scene.model().borrow();
    let shown = model
        .shown()
        .ok_or_else(|| "нет показанного diff".to_owned())?;
    let state = model
        .state
        .as_ref()
        .ok_or_else(|| "нет состояния вида".to_owned())?;
    model
        .threads
        .threads()
        .iter()
        .filter(|thread| !state.is_expanded(thread.id))
        .filter_map(|thread| {
            let block = shown
                .body
                .row_of_addr(model.mode, thread.anchor.last())?
                .index;
            (block >= row).then_some((block, thread.id))
        })
        .min_by_key(|(block, _)| *block)
        .map(|(_, id)| id)
        .ok_or_else(|| format!("нет свёрнутого треда не выше строки {row}"))
}

impl Script for SelectionScript {
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
        let mode = ctx.shown_mode();
        let row = self.row()? as usize;
        let model = ctx.stage.scene.selection();
        let cursor = ctx.stage.scene.cursor();
        match stimulus {
            Stimulus::Click => {
                checks.push(check_cursor(
                    name,
                    addr_at(ctx.raw, mode, row, Side::New),
                    cursor,
                ));
                checks.push(check(name, None, model));
            }
            Stimulus::Visual => checks.push(check(
                name,
                expected(ctx.raw, mode, Side::New, row, row + 2),
                model,
            )),
            Stimulus::Up => checks.push(check(
                name,
                expected(ctx.raw, mode, Side::New, row, row + 1),
                model,
            )),
            Stimulus::Escape => checks.push(check(name, None, model)),
            Stimulus::Drag | Stimulus::ShiftClick | Stimulus::Reselect => {
                checks.push(check(name, self.range(ctx)?, model));
            }
            Stimulus::HalfOld | Stimulus::HalfNew => {
                let half = self.half.ok_or_else(|| "нет изменённой пары".to_owned())?;
                let side = if stimulus == Stimulus::HalfOld {
                    Side::Old
                } else {
                    Side::New
                };
                checks.push(check_cursor(
                    name,
                    addr_at(ctx.raw, mode, half as usize, side),
                    cursor,
                ));
            }
            Stimulus::Narrow | Stimulus::Restore => {
                let shown = ctx.shown_mode();
                if shown != ctx.mode {
                    return Err(format!("смена ширины сменила режим: {shown:?}"));
                }
                checks.push(check(name, self.range(ctx)?, model));
            }
            Stimulus::Expand | Stimulus::Collapse | Stimulus::ScrollBack => {
                checks.push(check(name, self.range(ctx)?, model));
            }
            Stimulus::ScrollAway => {
                let visible = ctx.stage.scene.visible_rows();
                let last = (row + CONTEXT_RUN - 1) as u32;
                if visible.start <= last {
                    return Err(format!(
                        "прокрутка без эффекта: строки выделения {row}..={last} не ушли за видимые \
                         {visible:?}"
                    ));
                }
                checks.push(check(name, self.range(ctx)?, model));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_steps_add_half_clicks_and_reselect() {
        assert_eq!(SIDE_STEPS.len(), UNIFIED_STEPS.len() + 3);
        for step in UNIFIED_STEPS {
            assert!(SIDE_STEPS.contains(&step));
        }
        for name in ["v", "j", "k", "Escape"] {
            assert!(events::sequence(name).is_some(), "{name}");
        }
    }
}
