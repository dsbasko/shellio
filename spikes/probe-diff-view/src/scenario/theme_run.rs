//! Шаги сценария `theme` (задача 15) по строкам отчёта «Ячейки `theme`» и «Пиксельная проба
//! `theme`»: ячейка темы — пересборка стенда после переопределения оформления приложения и
//! Increase Contrast, затем курсор и выделение строки R, рендер вида прокрутки и снимок окна.
//! Пиксели фонов сверяются с токенами ячейки.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSEventModifierFlags,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use probe_diff_view::model::{LineKind, Mode, Side};
use probe_diff_view::tokens::{Appearance, Theme, ThemeKey};

use super::contract::{Check, Kind};
use super::contract_run::{Ctx, Next, Restage, STEP_DEADLINE, Script, Wait};
use super::drawmark;
use super::events;
use super::render::ViewRender;
use super::snapshot::{self, Snapshot, SnapshotStatus};
use super::theme::{self, Band, CELLS, Role, TOLERANCE};
use crate::results::{Report, Verdict};

/// Срок снимка окна (строка «Сроки `zones` и `theme`»).
const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    /// Переопределить тему ячейки и пересобрать стенд.
    Apply,
    Click,
    Visual,
    Probe,
}

type Slot = Rc<RefCell<Option<Result<Snapshot, String>>>>;

pub struct ThemeScript {
    out: PathBuf,
    cell: usize,
    next: Step,
    row: Option<u32>,
    /// Проверки пиксельной пробы текущей ячейки до конца ожидания снимка.
    probes: Vec<Check>,
    snapshot: Slot,
    snapshots: Vec<Snapshot>,
}

/// Оформление приложения и переопределение Increase Contrast ячейки; `None` — снять оба.
fn apply(mtm: MainThreadMarker, key: Option<ThemeKey>) {
    let app = NSApplication::sharedApplication(mtm);
    let appearance = key.and_then(|key| {
        // SAFETY: имена оформлений — неизменяемые константы AppKit.
        let name = unsafe {
            match key.appearance {
                Appearance::Light => NSAppearanceNameAqua,
                Appearance::Dark => NSAppearanceNameDarkAqua,
            }
        };
        NSAppearance::appearanceNamed(name)
    });
    app.setAppearance(appearance.as_deref());
    crate::text::override_contrast(key.map(|key| key.increase_contrast));
}

/// Точка пробы: подпись, ожидаемый токен и точка в координатах документа.
struct Target {
    label: String,
    token: &'static str,
    want: probe_diff_view::tokens::Rgb,
    point: (f64, f64),
}

impl ThemeScript {
    pub fn new(out: &Path) -> ThemeScript {
        ThemeScript {
            out: out.to_path_buf(),
            cell: 0,
            next: Step::Apply,
            row: None,
            probes: Vec::new(),
            snapshot: Rc::new(RefCell::new(None)),
            snapshots: Vec::new(),
        }
    }

    fn key(&self) -> ThemeKey {
        CELLS[self.cell]
    }

    /// Строка R: контекстная у середины видимых, не начало ханка.
    fn middle_context(ctx: &Ctx<'_>) -> Result<u32, String> {
        let visible = ctx.stage.scene.visible_rows();
        let mode = ctx.shown_mode();
        let middle = visible.start + (visible.end - visible.start) / 2;
        (middle..visible.end.saturating_sub(1))
            .find(|&row| {
                let lines = ctx.raw.row_lines(mode, row as usize);
                !lines.is_empty()
                    && lines
                        .iter()
                        .all(|&index| ctx.raw.lines[index].kind == Kind::Context)
            })
            .ok_or_else(|| format!("нет контекстной строки среди видимых {visible:?}"))
    }

    /// Точки пробы по модели: строка R и первые подходящие контекстная, добавленная и удалённая
    /// строки среди видимых, кроме крайних и начал ханков.
    fn targets(&self, ctx: &Ctx<'_>, theme: &Theme) -> Result<Vec<Target>, String> {
        let marked = self.row.ok_or_else(|| "нет строки R".to_owned())?;
        let visible = ctx.stage.scene.visible_rows();
        let model = ctx.stage.scene.model().borrow();
        let shown = model.shown().ok_or_else(|| "diff не показан".to_owned())?;
        let mode = model.mode;
        let char_width = model.font.char_width;
        let line_height = model.font.line_height;
        let gutter_cells = shown.geometry.gutter_cells(mode);
        let width = shown.geometry.width();
        let half_of = |kind: LineKind| match (mode, kind) {
            (Mode::Unified, _) => None,
            (Mode::SideBySide, LineKind::Removed) => Some(Side::Old),
            (Mode::SideBySide, LineKind::Context | LineKind::Added) => Some(Side::New),
        };
        let wanted: [(Role, LineKind, &str); 4] = [
            (Role::Marked, LineKind::Context, "R"),
            (Role::Plain, LineKind::Context, "context"),
            (Role::Plain, LineKind::Added, "added"),
            (Role::Plain, LineKind::Removed, "removed"),
        ];
        let mut targets = Vec::new();
        for (role, kind, label) in wanted {
            let half = half_of(kind);
            let rows: Vec<u32> = match role {
                Role::Marked => vec![marked],
                Role::Plain => (visible.start + 1..visible.end.saturating_sub(1))
                    .filter(|&row| row != marked && !shown.body.starts_hunk(mode, row))
                    .collect(),
            };
            let found = rows.into_iter().find_map(|row| {
                let halves = shown.body.row(mode, row)?;
                let index = match half {
                    None => halves.first()?,
                    Some(side) => halves.half(side)?,
                };
                let line = shown.body.line(index)?;
                if line.kind() != kind {
                    return None;
                }
                let (x, half_width) = match half {
                    None => (0.0, width),
                    Some(Side::Old) => (0.0, shown.geometry.half_width()),
                    Some(Side::New) => {
                        let x = shown.geometry.half_origin(Side::New);
                        (x, width - x)
                    }
                };
                let height = shown.heights.text_height(row)?;
                let band = Band {
                    x,
                    width: half_width,
                    top: shown.heights.top(row)? as f64,
                    height: f64::from(height),
                };
                let cells = theme::cells_upper(shown.diff.line_text(line));
                let lines = height / line_height.max(1);
                let text = theme::text_point(band, gutter_cells, char_width, cells, lines)?;
                let (_, gutter) = drawmark::gutter_of(row, half)?;
                let gutter = theme::gutter_point(band, char_width, &gutter)?;
                Some((row, text, gutter))
            });
            let Some((row, text, gutter)) = found else {
                return Err(format!(
                    "нет видимой строки «{label}» ({kind:?}, {half:?}) с точками без глифа"
                ));
            };
            let [(text_token, text_want), (gutter_token, gutter_want)] =
                theme::wants(theme, kind, role);
            targets.push(Target {
                label: format!("{label} row {row} text"),
                token: text_token,
                want: text_want,
                point: text,
            });
            targets.push(Target {
                label: format!("{label} row {row} gutter"),
                token: gutter_token,
                want: gutter_want,
                point: gutter,
            });
        }
        Ok(targets)
    }

    /// Пиксельная проба ячейки: рендер вида прокрутки, затем точки по модели.
    fn probe(&self, ctx: &Ctx<'_>) -> Result<Vec<Check>, String> {
        let key = self.key();
        let name = key.name();
        let mut checks = Vec::new();
        let current = crate::text::current_theme(ctx.mtm);
        checks.push(Check::new(
            format!("{name} theme"),
            current == key,
            format!("тема вида {}, ячейка {name}", current.name()),
        ));
        let scroll = ctx.stage.view.scroll_view();
        let render = ViewRender::of(scroll)?;
        let theme = Theme::of(key);
        let top = ctx.stage.view.viewport_y();
        let bottom = top + ctx.viewport_height();
        for target in self.targets(ctx, theme)? {
            let (x, y) = target.point;
            if !(top..bottom).contains(&y) {
                return Err(format!(
                    "точка {} вне видимой области [{top:.1}; {bottom:.1})",
                    target.label
                ));
            }
            let window = ctx
                .stage
                .view
                .to_window(NSRect::new(NSPoint::new(x, y), NSSize::new(0.0, 0.0)))
                .origin;
            let local = scroll.convertPoint_fromView(window, None);
            let pixel = render.pixel(local)?;
            let distance = pixel.distance(target.want);
            checks.push(Check::new(
                format!("{name} {}", target.label),
                distance <= TOLERANCE,
                format!(
                    "{} {} (сырые {} в «{}»), токен {} {}, разница {distance}, допуск {TOLERANCE}",
                    target.token,
                    pixel.hex(),
                    pixel.raw_hex(),
                    render.color_space(),
                    target.token,
                    format_rgb(target.want)
                ),
            ));
        }
        Ok(checks)
    }
}

fn format_rgb(rgb: probe_diff_view::tokens::Rgb) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb.r, rgb.g, rgb.b)
}

impl Script for ThemeScript {
    fn next(&mut self, ctx: &Ctx<'_>) -> Result<Next, String> {
        let name = self.key().name();
        match self.next {
            Step::Apply => {
                if self.cell >= CELLS.len() {
                    apply(ctx.mtm, None);
                    return Ok(Next::Done);
                }
                apply(ctx.mtm, Some(self.key()));
                self.next = Step::Click;
                Ok(Next::Restage(Restage {
                    fixture: ctx.fixture.clone(),
                    quiet: true,
                    cycle: false,
                }))
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
            Step::Visual => Ok(Next::Wait(Wait::new(
                format!("v {name}"),
                ctx.press("v")?,
                STEP_DEADLINE,
            ))),
            Step::Probe => {
                self.probes = self.probe(ctx)?;
                let background = Rc::new(snapshot::capture_window(
                    ctx.window,
                    &self.out,
                    &format!("theme-{name}"),
                ));
                *self.snapshot.borrow_mut() = None;
                let slot = Rc::clone(&self.snapshot);
                Ok(Next::Wait(
                    Wait::new(format!("snapshot {name}"), Vec::new(), SNAPSHOT_DEADLINE).until(
                        move |_| {
                            if slot.borrow().is_none()
                                && let Some(result) = background.try_take()
                            {
                                *slot.borrow_mut() = Some(result);
                            }
                            Ok(slot.borrow().is_some())
                        },
                    ),
                ))
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
            Step::Apply => return Err(format!("{name}: проверка без шага")),
            Step::Click => {
                let row = self.row.ok_or_else(|| "нет строки R".to_owned())?;
                let addrs = ctx.raw.row_addrs(ctx.shown_mode(), row as usize);
                let cursor = ctx.stage.scene.cursor();
                if !cursor.is_some_and(|cursor| addrs.contains(&cursor)) {
                    return Err(format!(
                        "щелчок не поставил курсор на строку {row}: {cursor:?}, адреса {addrs:?}"
                    ));
                }
                self.next = Step::Visual;
            }
            Step::Visual => {
                let selection = ctx.stage.scene.selection();
                let cursor = ctx.stage.scene.cursor();
                if !selection
                    .zip(cursor)
                    .is_some_and(|(selection, cursor)| selection.contains(cursor))
                {
                    return Err(format!(
                        "`v` не выделил строку курсора: выделение {selection:?}, курсор {cursor:?}"
                    ));
                }
                self.next = Step::Probe;
            }
            Step::Probe => {
                checks.append(&mut self.probes);
                let snapshot = self
                    .snapshot
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| "снимок окна не готов".to_owned())??;
                if snapshot.status == SnapshotStatus::Error {
                    return Err(format!(
                        "снимок окна {} не записан: {:?}",
                        snapshot.name, snapshot.reason
                    ));
                }
                self.snapshots.push(snapshot);
                self.cell += 1;
                self.next = Step::Apply;
            }
        }
        Ok(())
    }

    fn finish(&mut self, report: &mut Report, _checks: &[Check]) {
        let not_run: Vec<String> = self
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.status == SnapshotStatus::NotRun)
            .map(|snapshot| {
                format!(
                    "{}: {}",
                    snapshot.name,
                    snapshot.reason.clone().unwrap_or_default()
                )
            })
            .collect();
        let (verdict, reason) = if self.snapshots.len() != CELLS.len() {
            (
                Verdict::Fail,
                format!("снимков {} из {}", self.snapshots.len(), CELLS.len()),
            )
        } else if not_run.is_empty() {
            (
                Verdict::Pass,
                format!("снимок окна записан в каждой из {} ячеек", CELLS.len()),
            )
        } else {
            (Verdict::NotRun, not_run.join("; "))
        };
        report.finding("theme_snapshots", verdict, reason);
        report.push(
            "theme_snapshot_files",
            serde_json::to_string(&self.snapshots).unwrap_or_default(),
        );
        report.finding(
            "theme_system_contrast",
            Verdict::NotRun,
            "системный Increase Contrast процессом не переключается: ячейки контраста — \
             переопределение пробы",
        );
    }
}
