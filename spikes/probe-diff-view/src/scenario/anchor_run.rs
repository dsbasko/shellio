//! Шаги сценария `anchor` (строка отчёта «Якорь `anchor`»). Исходное положение: строка показа R
//! — первая контекстная не раньше середины документа, у `wrap` — с текстом не меньше 2
//! визуальных строк при колонках unified начальной ширины, курсор на R, верх clip view на
//! `top(R) + ⌊высота строки / 2⌋`, у `wrap` — `+ высота визуальной строки`.
//!
//! `wrap`: сужение на 120 точек, возврат, смена режима без смены ширины, возврат. `threads`:
//! раскрытие ближайшего свёрнутого треда выше видимой области, свёртка, затем `G`, `gg`, `]c`,
//! `[c`, каждый с возвратом `Ctrl-o`.

use probe_diff_view::model::state::{Anchor, Command, ThreadId};
use probe_diff_view::model::wrap;
use probe_diff_view::model::{LineAddr, Mode, Side};
use probe_diff_view::open::OpenFixture;

use super::anchor::{self, as_check, check_kept, expected_addr, start_row};
use super::contract::Check;
use super::contract_run::{Ctx, Next, RELAYOUT_DEADLINE, STEP_DEADLINE, Script, Wait};
use super::drawmark;

/// Сужение окна, точки.
const NARROW: f64 = 120.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Jump {
    Bottom,
    Top,
    Next,
    Prev,
}

impl Jump {
    fn keys(self) -> &'static str {
        match self {
            Jump::Bottom => "G",
            Jump::Top => "gg",
            Jump::Next => "]c",
            Jump::Prev => "[c",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stimulus {
    Position,
    Narrow,
    Restore,
    Mode,
    ModeBack,
    Expand,
    Collapse,
    Jump(Jump),
    Back(Jump),
}

impl Stimulus {
    fn name(self) -> String {
        match self {
            Stimulus::Position => "position".to_owned(),
            Stimulus::Narrow => "narrow".to_owned(),
            Stimulus::Restore => "restore".to_owned(),
            Stimulus::Mode => "mode".to_owned(),
            Stimulus::ModeBack => "mode-back".to_owned(),
            Stimulus::Expand => "expand".to_owned(),
            Stimulus::Collapse => "collapse".to_owned(),
            Stimulus::Jump(jump) => jump.keys().to_owned(),
            Stimulus::Back(jump) => format!("Ctrl-o after {}", jump.keys()),
        }
    }
}

const WRAP_STEPS: [Stimulus; 5] = [
    Stimulus::Position,
    Stimulus::Narrow,
    Stimulus::Restore,
    Stimulus::Mode,
    Stimulus::ModeBack,
];

const THREAD_STEPS: [Stimulus; 11] = [
    Stimulus::Position,
    Stimulus::Expand,
    Stimulus::Collapse,
    Stimulus::Jump(Jump::Bottom),
    Stimulus::Back(Jump::Bottom),
    Stimulus::Jump(Jump::Top),
    Stimulus::Back(Jump::Top),
    Stimulus::Jump(Jump::Next),
    Stimulus::Back(Jump::Next),
    Stimulus::Jump(Jump::Prev),
    Stimulus::Back(Jump::Prev),
];

/// Исходное положение: строка R сырого diff и модели, верх clip view и якорь после установки.
#[derive(Clone, Copy, Debug)]
struct Initial {
    row: u32,
    top_y: f64,
    offset: u64,
    anchor: Option<Anchor>,
}

/// Состояние до стимула: верх строки якоря в документе, режим, перестройки.
#[derive(Clone, Copy, Debug)]
struct Before {
    top: f64,
    mode: Mode,
    relayouts: u64,
}

pub struct AnchorScript {
    steps: &'static [Stimulus],
    wrap: bool,
    next: usize,
    current: Option<Stimulus>,
    initial: Option<Initial>,
    before: Option<Before>,
    base_width: f64,
    thread: Option<ThreadId>,
}

impl AnchorScript {
    pub fn new(fixture: OpenFixture) -> Result<AnchorScript, String> {
        let (steps, wrap): (&'static [Stimulus], bool) = match fixture {
            OpenFixture::Wrap => (&WRAP_STEPS, true),
            OpenFixture::Threads => (&THREAD_STEPS, false),
            other => return Err(format!("у anchor нет шагов фикстуры {}", other.name())),
        };
        Ok(AnchorScript {
            steps,
            wrap,
            next: 0,
            current: None,
            initial: None,
            before: None,
            base_width: 0.0,
            thread: None,
        })
    }

    fn initial(&self) -> Result<Initial, String> {
        self.initial
            .ok_or_else(|| "исходное положение не установлено".to_owned())
    }

    fn initial_anchor(&self) -> Result<Anchor, String> {
        self.initial()?
            .anchor
            .ok_or_else(|| "исходный якорь не записан".to_owned())
    }

    /// Текст строки показа `row` даёт не меньше 2 визуальных строк при колонках unified
    /// установленной геометрии.
    fn wraps_twice(ctx: &Ctx<'_>, row: u32) -> bool {
        let model = ctx.stage.scene.model().borrow();
        let Some(shown) = model.shown() else {
            return false;
        };
        let Some(halves) = shown.body.row(model.mode, row) else {
            return false;
        };
        let Some(line) = halves
            .half(Side::New)
            .and_then(|index| shown.body.line(index))
        else {
            return false;
        };
        let columns = shown.geometry.wrap(Mode::Unified);
        wrap::visual_lines(shown.diff.line_text(line), columns) >= 2
    }

    fn position(&mut self, ctx: &Ctx<'_>) -> Result<Wait, String> {
        let mode = ctx.shown_mode();
        let raw_row = if self.wrap {
            start_row(ctx.raw, mode, |row| Self::wraps_twice(ctx, row as u32))
        } else {
            start_row(ctx.raw, mode, |_| true)
        }
        .ok_or_else(|| "нет исходной строки R".to_owned())?;
        let row = raw_row as u32;
        let model_addr = ctx.stage.scene.row_addr_at(row, Side::New);
        if !model_addr.is_some_and(|addr| ctx.raw.row_addrs(mode, raw_row).contains(&addr)) {
            return Err(format!(
                "строка {row}: адрес модели {model_addr:?} не из строки сырого diff {:?}",
                ctx.raw.row_addrs(mode, raw_row)
            ));
        }
        let top = ctx
            .top_of(row)
            .ok_or_else(|| format!("нет верха строки {row}"))?;
        let offset = if self.wrap {
            u64::from(ctx.line_height())
        } else {
            u64::from(ctx.text_height(row).unwrap_or(0) / 2)
        };
        let top_y = top + offset as f64;
        ctx.command(Command::ScrollTo(top_y as u64));
        ctx.command(Command::Click {
            row,
            side: Side::New,
        });
        ctx.command(Command::ScrollTo(top_y as u64));
        self.base_width = ctx.content_width();
        self.initial = Some(Initial {
            row,
            top_y,
            offset,
            anchor: None,
        });
        Ok(Wait::new("position", Vec::new(), STEP_DEADLINE))
    }

    /// Верх в документе строки исходного якоря в текущей раскладке.
    fn anchor_top(&self, ctx: &Ctx<'_>) -> Result<f64, String> {
        let initial = self.initial_anchor()?;
        let model = ctx.stage.scene.model().borrow();
        let shown = model
            .shown()
            .ok_or_else(|| "нет показанного diff".to_owned())?;
        let row = shown
            .body
            .row_of_addr(model.mode, initial.addr)
            .ok_or_else(|| format!("нет строки исходного якоря {:?}", initial.addr))?
            .index;
        shown
            .heights
            .top(row)
            .map(|top| top as f64)
            .ok_or_else(|| format!("нет верха строки {row}"))
    }

    fn remember(&mut self, ctx: &Ctx<'_>) -> Result<Before, String> {
        let before = Before {
            top: self.anchor_top(ctx)?,
            mode: ctx.shown_mode(),
            relayouts: ctx.relayouts(),
        };
        self.before = Some(before);
        Ok(before)
    }

    /// Ближайший к якорю свёрнутый тред, блок которого выше видимой области.
    fn thread_above(ctx: &Ctx<'_>, top_row: u32) -> Option<ThreadId> {
        let model = ctx.stage.scene.model().borrow();
        let shown = model.shown()?;
        let state = model.state.as_ref()?;
        model
            .threads
            .threads()
            .iter()
            .filter(|thread| !state.is_expanded(thread.id))
            .filter_map(|thread| {
                let row = shown
                    .body
                    .row_of_addr(model.mode, thread.anchor.last())?
                    .index;
                (row < top_row).then_some((row, thread.id))
            })
            .max_by_key(|(row, _)| *row)
            .map(|(_, id)| id)
    }

    fn stimulus(&mut self, ctx: &Ctx<'_>, stimulus: Stimulus) -> Result<Wait, String> {
        let name = stimulus.name();
        match stimulus {
            Stimulus::Position => self.position(ctx),
            Stimulus::Narrow | Stimulus::Restore => {
                let before = self.remember(ctx)?;
                let width = if stimulus == Stimulus::Narrow {
                    self.base_width - NARROW
                } else {
                    self.base_width
                };
                ctx.set_content_width(width);
                Ok(Wait::new(name, Vec::new(), RELAYOUT_DEADLINE)
                    .until(move |ctx| Ok(ctx.relayouts() > before.relayouts)))
            }
            Stimulus::Mode | Stimulus::ModeBack => {
                let before = self.remember(ctx)?;
                let target = if stimulus == Stimulus::Mode {
                    other(ctx.mode)
                } else {
                    ctx.mode
                };
                ctx.stage.scene.request_mode(target);
                Ok(
                    Wait::new(name, Vec::new(), RELAYOUT_DEADLINE).until(move |ctx| {
                        Ok(ctx.relayouts() > before.relayouts && ctx.shown_mode() == target)
                    }),
                )
            }
            Stimulus::Expand | Stimulus::Collapse => {
                self.remember(ctx)?;
                let id = match (stimulus, self.thread) {
                    (Stimulus::Collapse, Some(id)) => id,
                    (Stimulus::Collapse, None) => return Err("нет раскрытого треда".to_owned()),
                    _ => {
                        let (_, top_row) = ctx
                            .anchor_row()
                            .ok_or_else(|| "нет якоря модели".to_owned())?;
                        let id = Self::thread_above(ctx, top_row)
                            .ok_or_else(|| format!("нет свёрнутого треда выше строки {top_row}"))?;
                        self.thread = Some(id);
                        id
                    }
                };
                ctx.stage.scene.toggle_thread(id);
                Ok(Wait::new(name, Vec::new(), STEP_DEADLINE))
            }
            Stimulus::Jump(jump) => {
                self.remember(ctx)?;
                Ok(Wait::new(name, ctx.press(jump.keys())?, STEP_DEADLINE))
            }
            Stimulus::Back(_) => {
                self.remember(ctx)?;
                Ok(Wait::new(name, ctx.press("Ctrl-o")?, STEP_DEADLINE))
            }
        }
    }

    /// Сверка якоря модели с отрисованной строкой.
    fn drawn_check(
        ctx: &Ctx<'_>,
        name: &str,
        expected: Option<LineAddr>,
    ) -> Result<(Check, Anchor, u32), String> {
        let (model, row) = ctx
            .anchor_row()
            .ok_or_else(|| "нет якоря модели".to_owned())?;
        let (drawn, _) = drawmark::drawn_row(row);
        let generation = ctx.stage.ledger.borrow().required(row as usize);
        let checked = anchor::check(
            expected,
            model,
            row,
            drawn,
            generation,
            ctx.stage.view.viewport_y(),
        );
        Ok((as_check(name, &checked), model, row))
    }

    fn checked_position(&mut self, ctx: &Ctx<'_>, checks: &mut Vec<Check>) -> Result<(), String> {
        let mut initial = self.initial()?;
        let top = ctx.stage.view.viewport_y();
        let (model, row) = ctx
            .anchor_row()
            .ok_or_else(|| "нет якоря модели".to_owned())?;
        let cursor = ctx.stage.scene.cursor();
        let cursor_row = ctx.stage.scene.model().borrow().cursor_row();
        if (top - initial.top_y).abs() > anchor::TOLERANCE
            || row != initial.row
            || model.offset != initial.offset
            || cursor_row != Some(initial.row)
        {
            return Err(format!(
                "исходное положение не установлено: верх {top:.1} при ожидаемом {:.1}, якорь \
                 {model:?} в строке {row}, ожидалась строка {} со смещением {}, курсор {cursor:?} \
                 в строке {cursor_row:?}",
                initial.top_y, initial.row, initial.offset
            ));
        }
        initial.anchor = Some(model);
        self.initial = Some(initial);
        let (check, _, _) = Self::drawn_check(ctx, "position", Some(model.addr))?;
        checks.push(check);
        Ok(())
    }

    /// Шаг, который обязан сохранить якорь: ширина, режим, тред, возврат `Ctrl-o`.
    fn checked_kept(
        &self,
        ctx: &Ctx<'_>,
        stimulus: Stimulus,
        checks: &mut Vec<Check>,
    ) -> Result<(), String> {
        let name = stimulus.name();
        let initial = self.initial_anchor()?;
        let before = self
            .before
            .ok_or_else(|| "нет состояния до стимула".to_owned())?;
        let shown = ctx.shown_mode();
        match stimulus {
            Stimulus::Narrow | Stimulus::Restore if shown != before.mode => {
                return Err(format!(
                    "смена ширины сменила режим: {:?} → {shown:?}",
                    before.mode
                ));
            }
            Stimulus::Mode | Stimulus::ModeBack if shown == before.mode => {
                return Err(format!("режим не сменился: {shown:?}"));
            }
            _ => {}
        }
        if matches!(
            stimulus,
            Stimulus::Narrow
                | Stimulus::Restore
                | Stimulus::Mode
                | Stimulus::ModeBack
                | Stimulus::Expand
                | Stimulus::Collapse
        ) {
            let after = self.anchor_top(ctx)?;
            if !anchor::moved(before.top, after) {
                return Err(format!(
                    "стимул без эффекта: верх строки якоря {:.1} → {after:.1}",
                    before.top
                ));
            }
        }
        let (model, _) = ctx
            .anchor_row()
            .ok_or_else(|| "нет якоря модели".to_owned())?;
        let expected = if anchor::same_line(ctx.raw, initial.addr, model.addr) {
            model.addr
        } else {
            initial.addr
        };
        let (check, model, _) = Self::drawn_check(ctx, &name, Some(expected))?;
        checks.push(check);
        checks.push(check_kept(&name, ctx.raw, initial, model));
        Ok(())
    }

    /// Переход: якорь на ожидаемой строке со смещением 0, у `G` — курсор на последней строке.
    fn checked_jump(
        &self,
        ctx: &Ctx<'_>,
        jump: Jump,
        checks: &mut Vec<Check>,
    ) -> Result<(), String> {
        let name = Jump::keys(jump);
        let mode = ctx.shown_mode();
        let rows = ctx.raw.rows(mode);
        let initial = self.initial()?;
        let (model, row) = ctx
            .anchor_row()
            .ok_or_else(|| "нет якоря модели".to_owned())?;
        if jump == Jump::Bottom {
            let cursor = ctx.stage.scene.cursor();
            let expected = rows
                .checked_sub(1)
                .and_then(|last| expected_addr(ctx.raw, mode, last, cursor?));
            checks.push(Check::new(
                format!("{name} cursor"),
                expected.is_some() && expected == cursor,
                format!("курсор модели {cursor:?}, ожидался {expected:?}"),
            ));
            let (check, _, _) = Self::drawn_check(ctx, name, None)?;
            checks.push(check);
            return Ok(());
        }
        let target = match jump {
            Jump::Top => Some(0),
            Jump::Next => ctx.raw.change_from(mode, initial.row as usize, true),
            Jump::Prev => ctx.raw.change_from(mode, initial.row as usize, false),
            Jump::Bottom => None,
        }
        .ok_or_else(|| format!("{name}: у R={} нет ожидаемой строки", initial.row))?;
        let expected = expected_addr(ctx.raw, mode, target, model.addr);
        let (check, model, _) = Self::drawn_check(ctx, name, expected)?;
        checks.push(check);
        checks.push(Check::new(
            format!("{name} row"),
            row as usize == target && model.offset == 0,
            format!(
                "якорь модели {model:?} в строке {row}, ожидалась строка {target} со смещением 0"
            ),
        ));
        Ok(())
    }
}

fn other(mode: Mode) -> Mode {
    match mode {
        Mode::Unified => Mode::SideBySide,
        Mode::SideBySide => Mode::Unified,
    }
}

impl Script for AnchorScript {
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
            Stimulus::Position => self.checked_position(ctx, checks),
            Stimulus::Jump(jump) => self.checked_jump(ctx, jump, checks),
            Stimulus::Narrow
            | Stimulus::Restore
            | Stimulus::Mode
            | Stimulus::ModeBack
            | Stimulus::Expand
            | Stimulus::Collapse
            | Stimulus::Back(_) => self.checked_kept(ctx, stimulus, checks),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_follow_the_report_row() {
        let names: Vec<String> = THREAD_STEPS.iter().map(|step| step.name()).collect();
        assert_eq!(names[3], "G");
        assert_eq!(names[4], "Ctrl-o after G");
        for jump in [Jump::Bottom, Jump::Top, Jump::Next, Jump::Prev] {
            assert!(super::super::events::sequence(jump.keys()).is_some());
        }
        assert!(super::super::events::sequence("Ctrl-o").is_some());
        assert!(AnchorScript::new(OpenFixture::Modified).is_err());
        assert_eq!(other(Mode::Unified), Mode::SideBySide);
    }
}
