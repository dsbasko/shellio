//! Шаги сценария `accept` — приёмочного прохода исполнителя (задача 18, строка отчёта
//! «Приёмочный проход `accept`»): настоящий ход по фикстуре `real` в окне 1180 × 720 центра.
//! Открытие файла 0, `]f` до последнего файла, `[f` до файла действий, `]c`, `v j j`, `c`, текст,
//! `Cmd-Enter`, в «Рядом» щелчки по старой и новой половине, `]f` или `[f` к файлу проверки треда
//! и обратно, ресайз через границу `Narrow` и обратно.
//!
//! После каждого шага путь активного файла сверяется с `manifest.tsv`, адреса видимых строк — с
//! сырым разбором участка diff этого файла, затем снимок окна (он же «до» следующего шага) и
//! снимок внешнего клиента доступности против модели ([`probe_diff_view::access::compare`]) с
//! текстом каждой строки против строки blob по номеру из подписи.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use objc2_app_kit::NSEventModifierFlags;
use serde::Serialize;

use probe_diff_view::access::{self, AxSnapshot, Change, Comparison, Expectation};
use probe_diff_view::fixtures::Fixture;
use probe_diff_view::highlight::Source;
use probe_diff_view::model::layout::{NARROW_WIDTH, SizeClass};
use probe_diff_view::model::threads::ThreadAnchor;
use probe_diff_view::model::{LineAddr, Mode, Side};
use probe_diff_view::real::{self, RealFile, RealSource};

use super::ax_client::{Client, Reply};
use super::contract::{Check, Raw};
use super::contract_run::{Ctx, Next, RELAYOUT_DEADLINE, STEP_DEADLINE, Script, Wait};
use super::drawmark;
use super::events;
use super::selection::addr_at;
use super::snapshot::{self, Snapshot, SnapshotStatus};
use super::threads::{check_block, last_row};
use crate::results::{Report, Verdict};

/// Срок готовности клиента доступности.
const CLIENT_DEADLINE: Duration = Duration::from_secs(10);
/// Срок снимка окна и снимка клиента доступности.
const OBSERVE_DEADLINE: Duration = Duration::from_secs(20);
/// Ширина содержимого окна по ту сторону границы `Narrow`, точки.
const NARROW: f64 = 440.0;
/// Допуск ширины содержимого после ресайза, точки.
const WIDTH_TOLERANCE: f64 = 0.5;
/// Текст черновика.
const TEXT: &str = "jk";
/// Причина `not_run` того, что автоматика не наблюдает.
const UNOBSERVED: &str = "автоматика не наблюдает";

const _: () = assert!(NARROW < NARROW_WIDTH);

/// Причина `not_run` процесса без окна: `run.sh` не нашёл закреплённых коммитов.
pub fn not_run_reason(fixtures: &Path) -> Option<String> {
    match real::load(&real::dir(fixtures)) {
        Ok(Some(RealSource::NotRun(reason))) => Some(reason),
        Ok(Some(RealSource::Prepared(_)) | None) | Err(_) => None,
    }
}

/// `not_run` фикстуры в итог процесса.
pub fn not_run_report(reason: &str) -> Report {
    let mut report = Report::default();
    report.finding("accept_contract", Verdict::NotRun, reason);
    report
}

/// Фикстура `real` с активным файлом 0 и сырой разбор его участка diff.
pub fn load(fixtures: &Path) -> Result<(Fixture, Raw), String> {
    let fixture = match real::load(&real::dir(fixtures))? {
        Some(RealSource::Prepared(fixture)) => fixture,
        Some(RealSource::NotRun(reason)) => return Err(format!("фикстура real: {reason}")),
        None => return Err("фикстура real не подготовлена run.sh".to_owned()),
    };
    let raws = file_raws(&fixture.diff, &fixture.files)?;
    let view = fixture
        .fixture(0)
        .ok_or_else(|| "у фикстуры real нет файлов".to_owned())?;
    let raw = raws.into_iter().next().unwrap_or_default();
    Ok((view, raw))
}

/// Участки diff по файлам: от строки `diff --git ` до следующей. Число участков и пути сверяются
/// с манифестом.
pub fn sections<'a>(diff: &'a [u8], files: &[RealFile]) -> Result<Vec<&'a [u8]>, String> {
    const HEADER: &[u8] = b"diff --git ";
    let mut starts = Vec::new();
    let mut at = 0;
    for line in diff.split(|byte| *byte == b'\n') {
        if line.starts_with(HEADER) {
            starts.push(at);
        }
        at += line.len() + 1;
    }
    if starts.len() != files.len() {
        return Err(format!(
            "участков diff {}, файлов в манифесте {}",
            starts.len(),
            files.len()
        ));
    }
    let mut out = Vec::with_capacity(starts.len());
    for (index, &start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(diff.len());
        let section = &diff[start..end.min(diff.len())];
        let header = section
            .split(|byte| *byte == b'\n')
            .next()
            .unwrap_or_default();
        let wanted = format!(" b/{}", files[index].path);
        if !header.ends_with(wanted.as_bytes()) {
            return Err(format!(
                "участок {index}: «{}», а в манифесте {}",
                String::from_utf8_lossy(header),
                files[index].path
            ));
        }
        out.push(section);
    }
    Ok(out)
}

/// Сырой разбор участка каждого файла.
pub fn file_raws(diff: &[u8], files: &[RealFile]) -> Result<Vec<Raw>, String> {
    sections(diff, files)?
        .into_iter()
        .enumerate()
        .map(|(index, section)| {
            Raw::parse(section).map_err(|reason| format!("участок {index} не разобран: {reason}"))
        })
        .collect()
}

/// Файл действий: изменённый (обе стороны есть) с наибольшим числом строк unified, при равенстве
/// первый в порядке diff. Файл проверки треда — следующий, у последнего — предыдущий.
pub fn pick_files(files: &[RealFile], raws: &[Raw]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for (index, (file, raw)) in files.iter().zip(raws).enumerate() {
        if file.old.is_empty() || file.new.is_empty() {
            continue;
        }
        let rows = raw.lines.len();
        if best.is_none_or(|(_, most)| rows > most) {
            best = Some((index, rows));
        }
    }
    let (action, _) = best?;
    let neighbour = if action + 1 < files.len() {
        action + 1
    } else {
        action.checked_sub(1)?
    };
    Some((action, neighbour))
}

/// Строка `number` (с 1) blob без перевода строки.
fn blob_line(blob: &[u8], number: u32) -> Option<String> {
    let index = usize::try_from(number.checked_sub(1)?).ok()?;
    let line = blob.split(|byte| *byte == b'\n').nth(index)?;
    Some(String::from_utf8_lossy(line).into_owned())
}

/// Текст строки показа по подписи и blob файла: у контекстной — строка новой стороны, у пары
/// «Рядом» — старая и новая через перевод строки, как в [`access::describe_row`].
pub fn blob_text(
    change: Change,
    old: Option<u32>,
    new: Option<u32>,
    file: &RealFile,
) -> Option<String> {
    let old_line = || blob_line(&file.old, old?);
    let new_line = || blob_line(&file.new, new?);
    match change {
        Change::Context => new_line().or_else(old_line),
        Change::Added => new_line(),
        Change::Removed => old_line(),
        Change::Changed => Some(format!("{}\n{}", old_line()?, new_line()?)),
    }
}

/// Расхождения текста прочитанных строк с blob файла; число сверенных строк.
pub fn text_mismatches(snapshot: &AxSnapshot, file: &RealFile) -> (usize, Vec<String>) {
    let mut compared = 0;
    let mut out = Vec::new();
    for row in &snapshot.rows {
        let element = &row.element;
        let Some(label) = element.description.as_deref() else {
            out.push(format!("строка {:?}: нет подписи", element.index));
            continue;
        };
        if label == access::BLOCK_LABEL {
            continue;
        }
        let Some((change, old, new)) = access::parse_label(label) else {
            out.push(format!(
                "строка {:?}: подпись «{label}» не разобрана",
                element.index
            ));
            continue;
        };
        compared += 1;
        let want = blob_text(change, old, new, file);
        if want.is_none() || element.value != want {
            out.push(format!(
                "строка {:?} «{label}»: текст {:?}, в blob {want:?}",
                element.index, element.value
            ));
        }
    }
    (compared, out)
}

/// Шаг хода.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    /// Клиент доступности готов, проверки открытого файла 0.
    Open,
    /// `]f` или `[f` к файлу.
    File(usize),
    NextChange,
    Select,
    Draft,
    Text,
    Save,
    Click(Side),
    /// К файлу проверки треда.
    Away(usize),
    /// Обратно к файлу действий.
    Back(usize),
    Narrow,
    Restore,
}

impl Act {
    fn name(self) -> String {
        match self {
            Act::Open => "open".to_owned(),
            Act::File(file) => format!("file-{file:02}"),
            Act::NextChange => "next-change".to_owned(),
            Act::Select => "select".to_owned(),
            Act::Draft => "draft".to_owned(),
            Act::Text => "text".to_owned(),
            Act::Save => "save".to_owned(),
            Act::Click(Side::Old) => "click-old".to_owned(),
            Act::Click(Side::New) => "click-new".to_owned(),
            Act::Away(file) => format!("thread-away-{file:02}"),
            Act::Back(file) => format!("thread-back-{file:02}"),
            Act::Narrow => "narrow".to_owned(),
            Act::Restore => "restore".to_owned(),
        }
    }
}

/// Шаги по порядку: открытие, `]f` до последнего, `[f` до файла действий, действия, щелчки в
/// «Рядом», файл проверки треда и обратно, ресайз.
fn acts(files: usize, action: usize, neighbour: usize, mode: Mode) -> Vec<Act> {
    let mut acts = vec![Act::Open];
    acts.extend((1..files).map(Act::File));
    let last = files.saturating_sub(1);
    acts.extend((action..last).rev().map(Act::File));
    acts.extend([
        Act::NextChange,
        Act::Select,
        Act::Draft,
        Act::Text,
        Act::Save,
    ]);
    if mode == Mode::SideBySide {
        acts.extend([Act::Click(Side::Old), Act::Click(Side::New)]);
    }
    acts.extend([
        Act::Away(neighbour),
        Act::Back(action),
        Act::Narrow,
        Act::Restore,
    ]);
    acts
}

type Slot<T> = Rc<RefCell<Option<Result<T, String>>>>;

/// Идущее наблюдение после шага: снимок окна и снимок клиента.
struct Observe {
    act: Act,
    label: String,
    expectation: Expectation,
    draft_focused: bool,
    window: Slot<Snapshot>,
    ax: Slot<AxSnapshot>,
}

/// Итог наблюдения шага.
#[derive(Clone, Debug, Serialize)]
struct Record {
    step: String,
    file: usize,
    path: String,
    mode: String,
    rows: usize,
    comparison: Comparison,
    focus_skipped: bool,
    texts_compared: usize,
    text_mismatches: Vec<String>,
    window: Option<String>,
}

#[derive(Serialize)]
struct StepFile<'a> {
    snapshot: &'a AxSnapshot,
    expected: &'a Expectation,
}

pub struct AcceptScript {
    requested: Mode,
    out: PathBuf,
    client: Rc<RefCell<Client>>,
    raws: Vec<Raw>,
    acts: Vec<Act>,
    next: usize,
    current: Option<Act>,
    observe: Option<Observe>,
    /// Шаг проверен, его наблюдение ещё не начато.
    pending: bool,
    file: usize,
    action: usize,
    neighbour: usize,
    /// Строка показа начала изменения `]c`.
    change_row: Option<u32>,
    selection: Option<probe_diff_view::model::state::Selection>,
    anchor: Option<ThreadAnchor>,
    click: Option<LineAddr>,
    width: Option<f64>,
    records: Vec<Record>,
    snapshots: Vec<Snapshot>,
}

impl AcceptScript {
    pub fn new(requested: Mode, out: &Path) -> Result<AcceptScript, String> {
        let client = Client::spawn(std::process::id(), out)?;
        Ok(AcceptScript {
            requested,
            out: out.to_path_buf(),
            client: Rc::new(RefCell::new(client)),
            raws: Vec::new(),
            acts: Vec::new(),
            next: 0,
            current: None,
            observe: None,
            pending: false,
            file: 0,
            action: 0,
            neighbour: 0,
            change_row: None,
            selection: None,
            anchor: None,
            click: None,
            width: None,
            records: Vec::new(),
            snapshots: Vec::new(),
        })
    }

    /// Разбор участков и шаги по фикстуре: один раз, до первого шага.
    fn prepare(&mut self, ctx: &Ctx<'_>) -> Result<(), String> {
        if !self.acts.is_empty() {
            return Ok(());
        }
        let files = &ctx.fixture.files;
        self.raws = file_raws(&ctx.fixture.diff, files)?;
        let (action, neighbour) = pick_files(files, &self.raws)
            .ok_or_else(|| "нет изменённого файла для действий".to_owned())?;
        self.action = action;
        self.neighbour = neighbour;
        self.acts = acts(files.len(), action, neighbour, self.requested);
        Ok(())
    }

    fn raw(&self, file: usize) -> Result<&Raw, String> {
        self.raws
            .get(file)
            .ok_or_else(|| format!("нет сырого разбора файла {file}"))
    }

    fn file_of<'a>(ctx: &'a Ctx<'_>, file: usize) -> Result<&'a RealFile, String> {
        ctx.fixture
            .files
            .get(file)
            .ok_or_else(|| format!("в манифесте нет файла {file}"))
    }

    fn press(ctx: &Ctx<'_>, names: &[&str]) -> Result<Vec<u64>, String> {
        let mut posted = Vec::new();
        for name in names {
            posted.extend(ctx.press(name)?);
        }
        Ok(posted)
    }

    /// Переход к файлу `target`: условие — путь файла манифеста и снимок полного разбора после
    /// перехода.
    fn switch(&mut self, ctx: &Ctx<'_>, target: usize, name: String) -> Result<Wait, String> {
        let key = if target > self.file { "]f" } else { "[f" };
        let path = Self::file_of(ctx, target)?.path.clone();
        self.file = target;
        Ok(
            Wait::new(name, Self::press(ctx, &[key])?, RELAYOUT_DEADLINE).until(move |ctx| {
                let timeline = ctx.stage.scene.timeline();
                let full = timeline.open_ns.is_some_and(|open| {
                    timeline
                        .snapshots
                        .iter()
                        .any(|applied| applied.source == Source::Full && applied.at_ns >= open)
                });
                Ok(full && ctx.stage.scene.file_path().as_deref() == Some(path.as_str()))
            }),
        )
    }

    fn resize(ctx: &Ctx<'_>, width: f64, name: String) -> Wait {
        ctx.set_content_width(width);
        Wait::new(name, Vec::new(), RELAYOUT_DEADLINE)
            .until(move |ctx| Ok((ctx.content_width() - width).abs() <= WIDTH_TOLERANCE))
    }

    /// Строка «Рядом» с обеими половинами среди видимых: сначала изменённая пара, затем
    /// контекстная.
    fn pair_row(&self, ctx: &Ctx<'_>) -> Result<u32, String> {
        let raw = self.raw(self.file)?;
        let visible = ctx.stage.scene.visible_rows();
        let inner = visible.start + 1..visible.end.saturating_sub(1);
        let both = |row: u32| {
            raw.pairs
                .get(row as usize)
                .and_then(|pair| Some((pair.old?, pair.new?)))
        };
        inner
            .clone()
            .find(|&row| both(row).is_some_and(|(old, new)| old != new))
            .or_else(|| inner.clone().find(|&row| both(row).is_some()))
            .ok_or_else(|| format!("нет строки с обеими половинами среди видимых {visible:?}"))
    }

    fn stimulus(&mut self, ctx: &Ctx<'_>, act: Act) -> Result<Wait, String> {
        let name = act.name();
        match act {
            Act::Open => {
                let client = Rc::clone(&self.client);
                Ok(
                    Wait::new(name, Vec::new(), CLIENT_DEADLINE).until(move |_| {
                        match client.borrow().poll() {
                            Some(Reply::Ready(_)) => Ok(true),
                            Some(Reply::Failed(reason)) => {
                                Err(format!("клиент доступности: {reason}"))
                            }
                            Some(Reply::Snapshot(_)) => Err("снимок клиента до запроса".to_owned()),
                            None => Ok(false),
                        }
                    }),
                )
            }
            Act::File(target) | Act::Away(target) | Act::Back(target) => {
                self.switch(ctx, target, name)
            }
            Act::NextChange => {
                let raw = self.raw(self.file)?;
                let mode = ctx.shown_mode();
                let before = ctx
                    .stage
                    .scene
                    .cursor()
                    .and_then(|cursor| raw.row_of(mode, cursor))
                    .ok_or_else(|| "курсор до `]c` не на строке сырого diff".to_owned())?;
                let start = raw
                    .change_from(mode, before, true)
                    .ok_or_else(|| format!("нет изменения после строки {before}"))?;
                self.change_row = Some(start as u32);
                Ok(Wait::new(name, Self::press(ctx, &["]c"])?, STEP_DEADLINE))
            }
            Act::Select => Ok(Wait::new(
                name,
                Self::press(ctx, &["v", "j", "j"])?,
                STEP_DEADLINE,
            )),
            Act::Draft => Ok(
                Wait::new(name, Self::press(ctx, &["c"])?, STEP_DEADLINE).until(|ctx| {
                    Ok(ctx.stage.scene.draft().is_some()
                        && ctx.stage.view.draft_shown().is_some()
                        && ctx.stage.view.draft_focused())
                }),
            ),
            Act::Text => Ok(
                Wait::new(name, Self::press(ctx, &["j", "k"])?, STEP_DEADLINE).until(|ctx| {
                    Ok(ctx
                        .stage
                        .scene
                        .draft()
                        .is_some_and(|draft| draft.text() == TEXT))
                }),
            ),
            Act::Save => Ok(
                Wait::new(name, Self::press(ctx, &["Cmd-Enter"])?, STEP_DEADLINE)
                    .until(|ctx| Ok(ctx.stage.scene.draft().is_none())),
            ),
            Act::Click(side) => {
                let row = self.pair_row(ctx)?;
                let raw = self.raw(self.file)?;
                self.click = addr_at(raw, Mode::SideBySide, row as usize, side);
                let point = ctx.point(row, side)?;
                let gesture = events::mouse_down(
                    ctx.mtm,
                    point,
                    ctx.window,
                    NSEventModifierFlags::empty(),
                    1,
                    &name,
                )?;
                let posted = vec![gesture.down, gesture.up(&name)?];
                Ok(Wait::new(name, posted, STEP_DEADLINE))
            }
            Act::Narrow => {
                self.width = Some(ctx.content_width());
                Ok(Self::resize(ctx, NARROW, name))
            }
            Act::Restore => {
                let width = self
                    .width
                    .ok_or_else(|| "нет исходной ширины окна".to_owned())?;
                Ok(Self::resize(ctx, width, name))
            }
        }
    }

    /// Общие проверки шага: путь и номер активного файла, число строк и адреса видимых строк
    /// против сырого разбора участка этого файла.
    fn common(&self, ctx: &Ctx<'_>, name: &str, checks: &mut Vec<Check>) -> Result<(), String> {
        let file = Self::file_of(ctx, self.file)?;
        let raw = self.raw(self.file)?;
        let mode = ctx.shown_mode();
        let path = ctx.stage.scene.file_path();
        let index = ctx.stage.scene.model().borrow().file;
        checks.push(Check::new(
            format!("{name} path"),
            path.as_deref() == Some(file.path.as_str()) && index == self.file,
            format!(
                "активный файл {index} {path:?}, в манифесте {} {}",
                self.file, file.path
            ),
        ));
        let rows = {
            let model = ctx.stage.scene.model().borrow();
            model.shown().map(|shown| shown.heights.len())
        };
        checks.push(Check::new(
            format!("{name} rows"),
            rows == u32::try_from(raw.rows(mode)).ok(),
            format!("строк показа {rows:?}, в сыром разборе {}", raw.rows(mode)),
        ));
        let visible = ctx.stage.scene.visible_rows();
        let mut wrong = Vec::new();
        for row in visible.clone() {
            let mut model: Vec<LineAddr> = Side::BOTH
                .into_iter()
                .filter_map(|side| ctx.stage.scene.row_addr_at(row, side))
                .collect();
            model.sort();
            model.dedup();
            let mut want = raw.row_addrs(mode, row as usize);
            want.sort();
            want.dedup();
            if model != want {
                wrong.push(format!("{row}: модель {model:?}, сырой {want:?}"));
            }
        }
        checks.push(Check::new(
            format!("{name} addrs"),
            wrong.is_empty() && !visible.is_empty(),
            format!(
                "видимые {visible:?}, расхождения {}: {}",
                wrong.len(),
                wrong.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
            ),
        ));
        Ok(())
    }

    /// Проверки шага сверх общих.
    fn act_checks(
        &mut self,
        ctx: &Ctx<'_>,
        act: Act,
        name: &str,
        checks: &mut Vec<Check>,
    ) -> Result<(), String> {
        let mode = ctx.shown_mode();
        match act {
            Act::Open | Act::File(_) => {}
            Act::NextChange => {
                let start = self.change_row.ok_or("нет строки изменения")?;
                let addrs = self.raw(self.file)?.row_addrs(mode, start as usize);
                let cursor = ctx.stage.scene.cursor();
                checks.push(Check::new(
                    name,
                    cursor.is_some_and(|cursor| addrs.contains(&cursor)),
                    format!("курсор {cursor:?}, начало изменения {start} с адресами {addrs:?}"),
                ));
            }
            Act::Select => {
                let start = self.change_row.ok_or("нет строки изменения")?;
                let raw = self.raw(self.file)?;
                let addrs = raw.row_addrs(mode, start as usize + 2);
                let cursor = ctx.stage.scene.cursor();
                let selection = ctx.stage.scene.selection();
                let ends = selection.is_some_and(|selection| {
                    [selection.start(), selection.end()]
                        .into_iter()
                        .all(|line| {
                            raw.row_of(
                                mode,
                                LineAddr {
                                    side: selection.side,
                                    line,
                                },
                            )
                            .is_some()
                        })
                });
                checks.push(Check::new(
                    name,
                    cursor.is_some_and(|cursor| addrs.contains(&cursor)) && ends,
                    format!(
                        "курсор {cursor:?}, строка {} с адресами {addrs:?}, выделение {selection:?}",
                        start + 2
                    ),
                ));
                self.selection = selection;
            }
            Act::Draft => {
                let path = Self::file_of(ctx, self.file)?.path.clone();
                let selection = self.selection.ok_or("нет выделения до `c`")?;
                let expected =
                    ThreadAnchor::range(path, selection.side, selection.start(), selection.end());
                let draft = ctx.stage.scene.draft().map(|draft| draft.anchor.clone());
                self.anchor = Some(expected.clone());
                checks.push(Check::new(
                    name,
                    draft.as_ref() == Some(&expected) && ctx.stage.view.draft_focused(),
                    format!(
                        "якорь черновика {draft:?}, ожидался {expected:?}, фокус поля {}",
                        ctx.stage.view.draft_focused()
                    ),
                ));
            }
            Act::Text => {
                let text = ctx.stage.scene.draft().map(|draft| draft.text().to_owned());
                checks.push(Check::new(
                    name,
                    text.as_deref() == Some(TEXT),
                    format!("текст черновика {text:?}"),
                ));
            }
            Act::Save => {
                let expected = self.anchor.clone().ok_or("нет якоря черновика")?;
                self.check_thread(ctx, name, &expected, checks);
                let row = last_row(self.raw(self.file)?, mode, &expected)
                    .ok_or_else(|| format!("нет последней строки якоря {expected:?}"))?;
                let (text, block) = drawmark::drawn_row(row as u32);
                checks.push(check_block(&format!("{name} block"), text, block));
            }
            Act::Click(side) => {
                let cursor = ctx.stage.scene.cursor();
                checks.push(Check::new(
                    name,
                    self.click.is_some() && cursor == self.click,
                    format!(
                        "курсор {cursor:?}, половина {side:?} по сырому diff {:?}",
                        self.click
                    ),
                ));
            }
            Act::Away(_) => {
                let expected = self.anchor.clone().ok_or("нет якоря треда")?;
                self.check_thread(ctx, name, &expected, checks);
                let rows = self.raw(self.file)?.rows(mode) as u32;
                let blocks: Vec<u32> = (0..rows)
                    .filter(|&row| ctx.stage.scene.block_height(row) != Some(0))
                    .collect();
                checks.push(Check::new(
                    format!("{name} no block"),
                    blocks.is_empty(),
                    format!(
                        "тред файла {} ({}) не виден в файле {}: строки с блоками {blocks:?}",
                        self.action, expected.path, self.file
                    ),
                ));
            }
            Act::Back(_) => {
                let expected = self.anchor.clone().ok_or("нет якоря треда")?;
                self.check_thread(ctx, name, &expected, checks);
                let row = last_row(self.raw(self.file)?, mode, &expected)
                    .ok_or_else(|| format!("нет последней строки якоря {expected:?}"))?;
                let height = ctx.stage.scene.block_height(row as u32);
                checks.push(Check::new(
                    format!("{name} block"),
                    height.is_some_and(|height| height > 0),
                    format!("блок под строкой {row} якоря {expected:?}: высота {height:?}"),
                ));
            }
            Act::Narrow | Act::Restore => {
                let size = ctx.stage.view.scroll_view().contentView().bounds().size;
                let class = SizeClass::of(size.width, size.height);
                let geometry = ctx.stage.scene.geometry().map(|geometry| geometry.size);
                let want = match class {
                    SizeClass::Narrow | SizeClass::ShortBar => Mode::Unified,
                    SizeClass::Regular | SizeClass::Wide => self.requested,
                };
                let crossed = match act {
                    Act::Narrow => class == SizeClass::Narrow,
                    _ => class != SizeClass::Narrow,
                };
                checks.push(Check::new(
                    name,
                    crossed && geometry == Some(class) && mode == want,
                    format!(
                        "видимая область {:.0} × {:.0}: класс {class:?}, у раскладки {geometry:?}, \
                         показан {mode:?}, ожидался {want:?}",
                        size.width, size.height
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Тред с текстом на якоре в модели после перехода.
    fn check_thread(
        &self,
        ctx: &Ctx<'_>,
        name: &str,
        expected: &ThreadAnchor,
        checks: &mut Vec<Check>,
    ) {
        let threads = ctx.stage.scene.threads();
        let thread = threads.iter().find(|thread| {
            thread.anchor == *expected
                && thread
                    .comments
                    .last()
                    .is_some_and(|comment| comment.body == TEXT)
        });
        checks.push(Check::new(
            format!("{name} thread"),
            thread.is_some(),
            format!(
                "тред на {expected:?} с текстом {TEXT:?}: {:?}, тредов {}",
                thread.map(|thread| thread.id),
                threads.len()
            ),
        ));
    }

    /// Начало наблюдения: снимок окна в фоне, ожидание модели и запрос клиенту в один момент.
    fn observe(&mut self, ctx: &Ctx<'_>, act: Act) -> Result<Wait, String> {
        let label = format!("{:02}-{}", self.records.len(), act.name());
        let expectation = super::ax::expectation(ctx.mtm, ctx.window, ctx.stage)?;
        let draft_focused = ctx.stage.view.draft_focused();
        let background = Rc::new(snapshot::capture_window(
            ctx.window,
            &self.out,
            &format!("accept-{label}"),
        ));
        self.client.borrow_mut().request(&label)?;
        let window: Slot<Snapshot> = Rc::new(RefCell::new(None));
        let ax: Slot<AxSnapshot> = Rc::new(RefCell::new(None));
        let wait = {
            let (window, ax, client) =
                (Rc::clone(&window), Rc::clone(&ax), Rc::clone(&self.client));
            Wait::new(format!("{label} observe"), Vec::new(), OBSERVE_DEADLINE).until(move |_| {
                if window.borrow().is_none()
                    && let Some(result) = background.try_take()
                {
                    *window.borrow_mut() = Some(result);
                }
                if ax.borrow().is_none() {
                    match client.borrow().poll() {
                        Some(Reply::Snapshot(snapshot)) => *ax.borrow_mut() = Some(Ok(*snapshot)),
                        Some(Reply::Failed(reason)) => {
                            return Err(format!("клиент доступности: {reason}"));
                        }
                        Some(Reply::Ready(_)) => {
                            return Err("повторная готовность клиента".to_owned());
                        }
                        None => {}
                    }
                }
                Ok(window.borrow().is_some() && ax.borrow().is_some())
            })
        };
        self.observe = Some(Observe {
            act,
            label,
            expectation,
            draft_focused,
            window,
            ax,
        });
        Ok(wait)
    }

    /// Итог наблюдения: снимок окна записан, снимок клиента сверен с моделью и blob.
    fn observed(&mut self, ctx: &Ctx<'_>, checks: &mut Vec<Check>) -> Result<(), String> {
        let observe = self.observe.take().ok_or("нет идущего наблюдения")?;
        let window = observe
            .window
            .borrow_mut()
            .take()
            .ok_or("снимок окна не готов")??;
        if window.status == SnapshotStatus::Error {
            return Err(format!(
                "снимок окна {} не записан: {:?}",
                window.name, window.reason
            ));
        }
        let snapshot = observe
            .ax
            .borrow_mut()
            .take()
            .ok_or("снимок клиента не готов")??;
        if snapshot.label != observe.label {
            return Err(format!(
                "снимок «{}» вместо «{}»",
                snapshot.label, observe.label
            ));
        }
        if let Some(error) = &snapshot.error {
            return Err(format!("снимок {}: {error}", observe.label));
        }
        let path = self.out.join(format!("ax-accept-{}.json", observe.label));
        let json = serde_json::to_vec(&StepFile {
            snapshot: &snapshot,
            expected: &observe.expectation,
        })
        .map_err(|err| format!("снимок {}: JSON: {err}", observe.label))?;
        std::fs::write(&path, json).map_err(|err| format!("{}: {err}", path.display()))?;
        let mut comparison = access::compare(&snapshot, &observe.expectation);
        if observe.draft_focused {
            comparison.focus.clear();
        }
        checks.push(Check::new(
            format!("{} ax", observe.label),
            comparison.is_clean(),
            format!(
                "строк {}, расхождения: {}",
                snapshot.rows.len(),
                comparison
                    .all()
                    .into_iter()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        ));
        let file = Self::file_of(ctx, self.file)?;
        let (compared, mismatches) = text_mismatches(&snapshot, file);
        checks.push(Check::new(
            format!("{} ax text", observe.label),
            compared > 0 && mismatches.is_empty(),
            format!(
                "сверено строк {compared}, расхождений {}: {}",
                mismatches.len(),
                mismatches
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        ));
        self.records.push(Record {
            step: observe.act.name(),
            file: self.file,
            path: file.path.clone(),
            mode: format!("{:?}", ctx.shown_mode()),
            rows: snapshot.rows.len(),
            comparison,
            focus_skipped: observe.draft_focused,
            texts_compared: compared,
            text_mismatches: mismatches,
            window: window.file.clone(),
        });
        self.snapshots.push(window);
        Ok(())
    }
}

impl Script for AcceptScript {
    fn next(&mut self, ctx: &Ctx<'_>) -> Result<Next, String> {
        self.prepare(ctx)?;
        if self.pending {
            self.pending = false;
            let act = self.current.ok_or("наблюдение без шага")?;
            return self.observe(ctx, act).map(Next::Wait);
        }
        let Some(&act) = self.acts.get(self.next) else {
            return Ok(Next::Done);
        };
        self.next += 1;
        self.current = Some(act);
        self.stimulus(ctx, act).map(Next::Wait)
    }

    fn checked(
        &mut self,
        ctx: &Ctx<'_>,
        name: &str,
        checks: &mut Vec<Check>,
    ) -> Result<(), String> {
        if self
            .observe
            .as_ref()
            .is_some_and(|observe| name == format!("{} observe", observe.label))
        {
            return self.observed(ctx, checks);
        }
        let act = self
            .current
            .filter(|act| act.name() == name)
            .ok_or_else(|| format!("проверка шага {name} без стимула"))?;
        self.common(ctx, name, checks)?;
        self.act_checks(ctx, act, name, checks)?;
        self.pending = true;
        Ok(())
    }

    fn finish(&mut self, report: &mut Report, _checks: &[Check]) {
        report.push("accept_action_file", self.action);
        report.push("accept_thread_file", self.neighbour);
        report.push(
            "accept_steps",
            serde_json::to_string(&self.records).unwrap_or_default(),
        );
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
        let (verdict, reason) = if self.snapshots.len() != self.acts.len() {
            (
                Verdict::Fail,
                format!("снимков {} из {}", self.snapshots.len(), self.acts.len()),
            )
        } else if not_run.is_empty() {
            (
                Verdict::Pass,
                format!(
                    "снимок окна записан после каждого из {} шагов",
                    self.acts.len()
                ),
            )
        } else {
            (Verdict::NotRun, not_run.join("; "))
        };
        report.finding("accept_snapshots", verdict, reason);
        report.push(
            "accept_snapshot_files",
            serde_json::to_string(&self.snapshots).unwrap_or_default(),
        );
        if self.requested == Mode::Unified {
            report.finding(
                "accept_half_click",
                Verdict::NotRun,
                "не применимо: у unified нет половин строки",
            );
        }
        for name in ["accept_voiceover", "accept_smoothness", "accept_hid"] {
            report.finding(name, Verdict::NotRun, UNOBSERVED);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn file(path: &str, old: &str, new: &str) -> RealFile {
        RealFile {
            path: path.to_owned(),
            old: Arc::from(old.as_bytes()),
            new: Arc::from(new.as_bytes()),
        }
    }

    const DIFF: &str = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n x\n-y\n+z\n\
diff --git a/b.rs b/b.rs\nnew file mode 100644\n--- /dev/null\n+++ b/b.rs\n@@ -0,0 +1,2 @@\n+p\n+q\n\
diff --git a/c.rs b/c.rs\n--- a/c.rs\n+++ b/c.rs\n@@ -1 +1,2 @@\n k\n+l\n";

    fn files() -> Vec<RealFile> {
        vec![
            file("a.rs", "x\ny\n", "x\nz\n"),
            file("b.rs", "", "p\nq\n"),
            file("c.rs", "k\n", "k\nl\n"),
        ]
    }

    #[test]
    fn sections_follow_the_manifest() {
        let files = files();
        let raws = file_raws(DIFF.as_bytes(), &files).unwrap();
        assert_eq!(raws.len(), 3);
        assert_eq!(raws[0].lines.len(), 3);
        assert_eq!(raws[1].lines.len(), 2);
        assert_eq!(raws[2].lines.len(), 2);
        assert_eq!(raws[2].lines[1].new, Some(2));
        let mut wrong = files.clone();
        wrong[1].path = "other.rs".to_owned();
        assert!(sections(DIFF.as_bytes(), &wrong).is_err());
        assert!(sections(DIFF.as_bytes(), &files[..2]).is_err());
    }

    #[test]
    fn action_file_is_the_largest_modified_one() {
        let files = files();
        let raws = file_raws(DIFF.as_bytes(), &files).unwrap();
        assert_eq!(pick_files(&files, &raws), Some((0, 1)));
        let last = vec![files[1].clone(), files[0].clone()];
        let raws = vec![raws[1].clone(), raws[0].clone()];
        assert_eq!(pick_files(&last, &raws), Some((1, 0)));
        assert_eq!(pick_files(&files[1..2], &raws[..1]), None);
    }

    #[test]
    fn equal_modified_files_pick_the_first() {
        let files = vec![file("a.rs", "x\n", "y\n"), file("b.rs", "x\n", "y\n")];
        let raw = Raw::parse(b"@@ -1 +1 @@\n-x\n+y\n").unwrap();
        assert_eq!(pick_files(&files, &[raw.clone(), raw]), Some((0, 1)));
    }

    #[test]
    fn blob_text_follows_the_row_description() {
        let file = file("a.rs", "x\ny\n", "x\nz\n");
        assert_eq!(
            blob_text(Change::Context, Some(1), Some(1), &file).as_deref(),
            Some("x")
        );
        assert_eq!(
            blob_text(Change::Removed, Some(2), None, &file).as_deref(),
            Some("y")
        );
        assert_eq!(
            blob_text(Change::Added, None, Some(2), &file).as_deref(),
            Some("z")
        );
        assert_eq!(
            blob_text(Change::Changed, Some(2), Some(2), &file).as_deref(),
            Some("y\nz")
        );
        assert_eq!(blob_text(Change::Added, None, Some(9), &file), None);
        assert_eq!(blob_text(Change::Added, None, Some(0), &file), None);
    }

    #[test]
    fn acts_walk_all_files_and_return_to_the_action_file() {
        let acts = acts(32, 29, 30, Mode::SideBySide);
        assert_eq!(acts[0], Act::Open);
        assert_eq!(acts[31], Act::File(31));
        assert_eq!(acts[32], Act::File(30));
        assert_eq!(acts[33], Act::File(29));
        assert!(acts.contains(&Act::Click(Side::Old)));
        assert_eq!(
            &acts[acts.len() - 4..],
            &[Act::Away(30), Act::Back(29), Act::Narrow, Act::Restore]
        );
        let unified = super::acts(32, 31, 30, Mode::Unified);
        assert_eq!(unified[32], Act::NextChange);
        assert!(!unified.contains(&Act::Click(Side::New)));
        let names: std::collections::BTreeSet<String> =
            unified.iter().map(|act| act.name()).collect();
        assert_eq!(names.len(), unified.len());
        for name in ["]f", "[f", "]c", "v", "j", "c", "k", "Cmd-Enter"] {
            assert!(events::sequence(name).is_some(), "{name}");
        }
    }
}
