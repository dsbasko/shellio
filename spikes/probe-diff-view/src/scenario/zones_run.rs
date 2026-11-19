//! Шаги сценария `zones` (задача 15) по строкам отчёта «Хост части зоны `zones`», «Ячейки
//! `zones`», «Проверки ячейки `zones`» и «Клавиши `zones`»: хост ставит вид diff в часть зоны
//! каждой ячейки, проверки сверяют `adapt` видимой области с таблицей, показанный режим и
//! колонки гуттера — с журналом отрисованных строк, якорь — с исходным, клавиши — с сырым diff.

use objc2_app_kit::NSEventModifierFlags;

use probe_diff_view::model::layout::{DiffLayout, TextMetrics};
use probe_diff_view::model::state::{Anchor, Command};
use probe_diff_view::model::{LineAddr, Mode, Side};

use super::anchor::{self, as_check, check_kept};
use super::contract::{Check, Kind};
use super::contract_run::{Ctx, Next, RELAYOUT_DEADLINE, STEP_DEADLINE, Script, Wait};
use super::drawmark;
use super::events;
use super::zones::{self, CASES, Expect, Text};
use crate::results::Report;
use crate::scene::{DIVIDER, max_line};
use crate::zone_host::Host;

/// Смещение исходного якоря от верха строки, точки.
const ANCHOR_OFFSET: u64 = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    /// Поставить ячейку `index`.
    Place,
    /// Исходный якорь после первой ячейки.
    Anchor,
    Click,
    Down,
    Up,
    /// `j` под заглушкой.
    Blocked,
}

pub struct ZonesScript {
    requested: Mode,
    host: Option<Host>,
    index: usize,
    next: Step,
    initial: Option<Anchor>,
    /// Строка щелчка ячейки с клавишами.
    row: Option<u32>,
    /// Курсор и якорь до `j` под заглушкой.
    before: Option<(Option<LineAddr>, Option<Anchor>)>,
    placed: Vec<String>,
}

impl ZonesScript {
    pub fn new(requested: Mode) -> ZonesScript {
        ZonesScript {
            requested,
            host: None,
            index: 0,
            next: Step::Place,
            initial: None,
            row: None,
            before: None,
            placed: Vec::new(),
        }
    }

    fn host(&self) -> Result<&Host, String> {
        self.host
            .as_ref()
            .ok_or_else(|| "хост не установлен".to_owned())
    }

    fn label(&self) -> String {
        let Some(case) = CASES.get(self.index) else {
            return "конец".to_owned();
        };
        format!(
            "{} {:?} {}x{}",
            self.index + 1,
            case.zone,
            case.width,
            case.height
        )
    }

    fn text(ctx: &Ctx<'_>) -> Result<Text, String> {
        let model = ctx.stage.scene.model().borrow();
        let shown = model.shown().ok_or_else(|| "diff не показан".to_owned())?;
        Ok(Text {
            char_width: model.font.char_width,
            digits: TextMetrics::digits_of(max_line(shown.body)),
            divider: DIVIDER,
        })
    }

    fn expect(&self, ctx: &Ctx<'_>) -> Result<Expect, String> {
        Ok(zones::expect(&CASES[self.index], Self::text(ctx)?))
    }

    /// После ячейки `index`: клавиши ячейки или следующая ячейка.
    fn after_place(&mut self, expect: Expect) {
        self.next = match (CASES[self.index].keys, expect) {
            (true, Expect::Placeholder) => Step::Blocked,
            (true, Expect::Diff { .. }) => Step::Click,
            (false, _) => self.advance(),
        };
    }

    fn advance(&mut self) -> Step {
        self.index += 1;
        Step::Place
    }

    /// Контекстная строка показа у середины видимых: щелчок ячейки с клавишами.
    fn middle_context(ctx: &Ctx<'_>) -> Result<u32, String> {
        let visible = ctx.stage.scene.visible_rows();
        let mode = ctx.shown_mode();
        let middle = visible.start + (visible.end - visible.start) / 2;
        (middle..visible.end.saturating_sub(1))
            .chain((visible.start + 1..middle).rev())
            .find(|&row| {
                let lines = ctx.raw.row_lines(mode, row as usize);
                !lines.is_empty()
                    && lines
                        .iter()
                        .all(|&index| ctx.raw.lines[index].kind == Kind::Context)
            })
            .ok_or_else(|| format!("нет контекстной строки среди видимых {visible:?}"))
    }

    fn check_cursor(ctx: &Ctx<'_>, step: &str, row: u32) -> Check {
        let mode = ctx.shown_mode();
        let addrs = ctx.raw.row_addrs(mode, row as usize);
        let cursor = ctx.stage.scene.cursor();
        let ok = cursor.is_some_and(|cursor| addrs.contains(&cursor));
        Check::new(
            step,
            ok,
            format!("курсор {cursor:?}, адреса строки {row} по сырому diff {addrs:?}"),
        )
    }

    /// Проверки ячейки с видом diff: раскладка, режим и гуттер по журналу, якорь.
    fn check_diff(
        &self,
        ctx: &Ctx<'_>,
        step: &str,
        expect: Expect,
        checks: &mut Vec<Check>,
    ) -> Result<(), String> {
        let host = self.host()?;
        checks.push(Check::new(
            format!("{step} shown"),
            host.diff_shown() && !host.placeholder_shown(),
            format!(
                "вид diff {}, заглушка {}",
                host.diff_shown(),
                host.placeholder_shown()
            ),
        ));
        let Expect::Diff { columns, .. } = expect else {
            return Err("ожидалась заглушка".to_owned());
        };
        let want = zones::shown_mode(expect, self.requested).unwrap_or(Mode::Unified);
        let shown = ctx.shown_mode();
        checks.push(Check::new(
            format!("{step} model mode"),
            shown == want,
            format!("режим модели {shown:?}, ожидался {want:?}"),
        ));
        let digits = Self::text(ctx)?.digits;
        let visible = ctx.stage.scene.visible_rows();
        let ledger = ctx.stage.ledger.borrow();
        let mut errors = Vec::new();
        for row in visible.clone() {
            let required = ledger.required(row as usize);
            let current = |half: Option<Side>| {
                drawmark::gutter_of(row, half).filter(|(generation, _)| *generation >= required)
            };
            let unified = current(None);
            let halves: Vec<(Side, String)> = Side::BOTH
                .into_iter()
                .filter_map(|side| current(Some(side)).map(|(_, text)| (side, text)))
                .collect();
            match want {
                Mode::Unified => {
                    if !halves.is_empty() {
                        errors.push(format!("строка {row}: нарисованы половины «Рядом»"));
                    }
                    match unified {
                        Some((_, text)) => {
                            if let Err(reason) =
                                zones::gutter_matches(&text, Mode::Unified, columns, digits)
                            {
                                errors.push(format!("строка {row}: {reason}"));
                            }
                        }
                        None => errors.push(format!("строка {row}: гуттер unified не нарисован")),
                    }
                }
                Mode::SideBySide => {
                    if unified.is_some() {
                        errors.push(format!("строка {row}: нарисован гуттер unified"));
                    }
                    if halves.is_empty() {
                        errors.push(format!("строка {row}: половины «Рядом» не нарисованы"));
                    }
                    for (side, text) in &halves {
                        if let Err(reason) =
                            zones::gutter_matches(text, Mode::SideBySide, columns, digits)
                        {
                            errors.push(format!("строка {row} {side:?}: {reason}"));
                        }
                    }
                }
            }
        }
        drop(ledger);
        checks.push(Check::new(
            format!("{step} drawn"),
            errors.is_empty(),
            if errors.is_empty() {
                format!("строки {visible:?} нарисованы в режиме {want:?}, колонок {columns}")
            } else {
                errors.join("; ")
            },
        ));
        if let Some(initial) = self.initial {
            let (model, row) = ctx
                .anchor_row()
                .ok_or_else(|| "нет якоря модели".to_owned())?;
            checks.push(check_kept(step, ctx.raw, initial, model));
            let (drawn, _) = drawmark::drawn_row(row);
            let generation = ctx.stage.ledger.borrow().required(row as usize);
            let checked = anchor::check(
                None,
                model,
                row,
                drawn,
                generation,
                ctx.stage.view.viewport_y(),
            );
            checks.push(as_check(&format!("{step} anchor"), &checked));
        }
        Ok(())
    }

    fn check_placeholder(
        &self,
        ctx: &Ctx<'_>,
        step: &str,
        checks: &mut Vec<Check>,
    ) -> Result<(), String> {
        let host = self.host()?;
        checks.push(Check::new(
            format!("{step} shown"),
            host.placeholder_shown() && !host.diff_shown(),
            format!(
                "заглушка {}, вид diff {}",
                host.placeholder_shown(),
                host.diff_shown()
            ),
        ));
        let entries = drawmark::journal_entries();
        checks.push(Check::new(
            format!("{step} no rows"),
            entries == 0,
            format!("записей журнала строк после стимула: {entries}"),
        ));
        let document = events::is_first_responder(ctx.window, ctx.stage.view.responder());
        checks.push(Check::new(
            format!("{step} focus"),
            !document,
            format!(
                "first responder — документ: {document}, заглушка: {}",
                events::is_first_responder(ctx.window, host.placeholder())
            ),
        ));
        Ok(())
    }
}

impl Script for ZonesScript {
    fn next(&mut self, ctx: &Ctx<'_>) -> Result<Next, String> {
        if self.host.is_none() {
            let theme = crate::text::current_theme(ctx.mtm);
            self.host = Some(Host::install(
                ctx.mtm,
                ctx.window,
                ctx.stage.view.scroll_view(),
                theme,
            ));
        }
        let name = self.label();
        match self.next {
            Step::Place => {
                let Some(case) = CASES.get(self.index) else {
                    return Ok(Next::Done);
                };
                let expect = self.expect(ctx)?;
                if expect == Expect::Placeholder {
                    drawmark::record_rows(true);
                }
                let placed = self.host()?.place(
                    ctx.window,
                    &ctx.stage.scene,
                    ctx.stage.view.responder(),
                    case.zone,
                    case.width,
                    case.height,
                )?;
                let visible = self.host()?.visible();
                self.placed.push(format!(
                    "{name}: {placed:?}, видимая область {} × {}",
                    visible.width, visible.height
                ));
                Ok(Next::Wait(Wait::new(
                    format!("place {name}"),
                    Vec::new(),
                    RELAYOUT_DEADLINE,
                )))
            }
            Step::Anchor => {
                let mode = ctx.shown_mode();
                let row = anchor::start_row(ctx.raw, mode, |_| true)
                    .ok_or_else(|| "нет контекстной строки во второй половине".to_owned())?;
                let top = ctx
                    .top_of(row as u32)
                    .ok_or_else(|| format!("нет верха строки {row}"))?;
                ctx.command(Command::ScrollTo(top as u64 + ANCHOR_OFFSET));
                Ok(Next::Wait(Wait::new("anchor", Vec::new(), STEP_DEADLINE)))
            }
            Step::Click => {
                let row = Self::middle_context(ctx)?;
                self.row = Some(row);
                let point = ctx.point(row, Side::New)?;
                let label = format!("click {name}");
                let gesture = events::mouse_down(
                    ctx.mtm,
                    point,
                    ctx.window,
                    NSEventModifierFlags::empty(),
                    1,
                    &label,
                )?;
                let posted = vec![gesture.down, gesture.up(&label)?];
                Ok(Next::Wait(Wait::new(label, posted, STEP_DEADLINE)))
            }
            Step::Down => Ok(Next::Wait(Wait::new(
                format!("j {name}"),
                ctx.press("j")?,
                STEP_DEADLINE,
            ))),
            Step::Up => Ok(Next::Wait(Wait::new(
                format!("k {name}"),
                ctx.press("k")?,
                STEP_DEADLINE,
            ))),
            Step::Blocked => {
                self.before = Some((ctx.stage.scene.cursor(), ctx.stage.scene.anchor()));
                drawmark::record_rows(true);
                Ok(Next::Wait(Wait::new(
                    format!("j blocked {name}"),
                    ctx.press("j")?,
                    STEP_DEADLINE,
                )))
            }
        }
    }

    fn checked(
        &mut self,
        ctx: &Ctx<'_>,
        name: &str,
        checks: &mut Vec<Check>,
    ) -> Result<(), String> {
        match self.next {
            Step::Place => {
                let case = CASES[self.index];
                let expect = self.expect(ctx)?;
                // Под заглушкой рамка вида не меняется: геометрия — сама часть зоны. С видом diff
                // хост уже сверил видимую область с частью.
                let (width, height) = match expect {
                    Expect::Placeholder => (case.width, case.height),
                    Expect::Diff { .. } => {
                        let visible = self.host()?.visible();
                        (visible.width, visible.height)
                    }
                };
                let layout = ctx
                    .stage
                    .scene
                    .layout_at(case.zone, width, height)
                    .unwrap_or(DiffLayout::Placeholder);
                checks.push(match zones::layout_matches(expect, layout) {
                    Ok(()) => Check::new(format!("{name} adapt"), true, format!("{expect:?}")),
                    Err(reason) => Check::new(format!("{name} adapt"), false, reason),
                });
                match expect {
                    Expect::Placeholder => self.check_placeholder(ctx, name, checks)?,
                    Expect::Diff { .. } => self.check_diff(ctx, name, expect, checks)?,
                }
                if self.initial.is_none() {
                    self.next = Step::Anchor;
                } else {
                    self.after_place(expect);
                }
            }
            Step::Anchor => {
                let (model, _) = ctx
                    .anchor_row()
                    .ok_or_else(|| "нет якоря модели".to_owned())?;
                if model.offset != ANCHOR_OFFSET {
                    return Err(format!(
                        "исходный якорь {model:?}, смещение не {ANCHOR_OFFSET}"
                    ));
                }
                self.initial = Some(model);
                let expect = self.expect(ctx)?;
                self.after_place(expect);
            }
            Step::Click => {
                let row = self.row.ok_or_else(|| "нет строки щелчка".to_owned())?;
                let check = Self::check_cursor(ctx, name, row);
                if !check.ok {
                    return Err(format!("щелчок не поставил курсор: {}", check.detail));
                }
                checks.push(check);
                self.next = Step::Down;
            }
            Step::Down => {
                let row = self.row.ok_or_else(|| "нет строки щелчка".to_owned())?;
                checks.push(Self::check_cursor(ctx, name, row + 1));
                self.next = Step::Up;
            }
            Step::Up => {
                let row = self.row.ok_or_else(|| "нет строки щелчка".to_owned())?;
                checks.push(Self::check_cursor(ctx, name, row));
                self.next = self.advance();
            }
            Step::Blocked => {
                let (cursor, anchor) = self.before.take().unwrap_or((None, None));
                let now = (ctx.stage.scene.cursor(), ctx.stage.scene.anchor());
                checks.push(Check::new(
                    format!("{name} kept"),
                    now == (cursor, anchor),
                    format!("курсор и якорь {now:?}, до клавиши {:?}", (cursor, anchor)),
                ));
                self.check_placeholder(ctx, name, checks)?;
                self.next = self.advance();
            }
        }
        Ok(())
    }

    fn finish(&mut self, report: &mut Report, _checks: &[Check]) {
        report.push("zones_requested", format!("{:?}", self.requested));
        report.push("zones_placed", self.placed.join("; "));
    }
}
