//! Сценарий `list`: `VirtualList` центральной плитки на 1 000, 10 000 и 100 000 строк. Для
//! каждого набора — первый экран после загрузки данных, щелчок по строке, прокрутка в конец и
//! обратно, обновление 10 строк и 100 перемещений выбора стрелкой после 10 прогревочных.
//! Проверки читают состояние AppKit: число и видимый диапазон строк таблицы, подписи
//! реализованных ячеек, выбор таблицы и слой строки, first responder, дерево доступности и
//! число созданных видов ячеек и строк.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{NSTableCellView, NSView, NSWindow};

use super::events::{self, Key};
use super::readback::{self, Rgba};
use super::snapshot::{self, Background, ViewRender};
use super::split::Stable;
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepFailure, Verdict, poll,
    run_steps,
};
use crate::fixtures::{self, LIST_SIZES, SCENE_ROWS};
use crate::list::VirtualList;
use crate::results::{Snapshot, SnapshotStatus};
use crate::scene::Scene;
use crate::tiles::{CENTER, Focus, Tile};

const STEP_TIMEOUT: Duration = Duration::from_secs(5);
const MOVE_TIMEOUT: Duration = Duration::from_secs(40);
const SCROLL_TIMEOUT: Duration = Duration::from_secs(10);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);
const WARMUP: usize = 10;
const MOVES: usize = 100;
/// Бюджет реакции на клавишу в `Navigation` из `docs/architecture.md`.
const BUDGET_MS: f64 = 16.0;
/// Строка, по которой щёлкает сценарий.
const CLICK_ROW: usize = 2;
const EDITS: usize = 10;

#[derive(Default)]
struct Moves {
    numbers: Vec<u64>,
    expected: usize,
    done: usize,
    post_to_state: Vec<f64>,
    post_to_dispatch: Vec<f64>,
    post_to_pass: Vec<f64>,
    dispatch_to_return: Vec<f64>,
    mismatches: Vec<String>,
    /// Перемещения, после которых выбранная строка ещё не видима: прокрутка анимирована.
    lagging: usize,
    error: Option<String>,
}

struct Run {
    stand: Rc<Stand>,
    scene: Rc<Scene>,
    tile: Retained<Tile>,
    settle: Rc<Settle>,
    report: RefCell<Report>,
    mark: Cell<Option<Duration>>,
    stable: RefCell<Stable>,
    click: RefCell<Option<(u64, u64)>>,
    moves: RefCell<Moves>,
    /// Наибольшее число видимых строк за прогон.
    max_visible: Cell<usize>,
    revision: Cell<u32>,
    /// Созданные списком виды на начало набора.
    created_at_fixture: Cell<(usize, usize)>,
}

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

/// Порог видов строк из раздела Technical Details плана.
fn reuse_limit(visible: usize) -> usize {
    2 * visible + 10
}

impl Run {
    fn mtm(&self) -> MainThreadMarker {
        self.stand.app.mtm()
    }

    fn window(&self) -> &NSWindow {
        &self.scene.window
    }

    fn list(&self) -> Result<&VirtualList, String> {
        self.tile
            .list()
            .ok_or_else(|| "у центральной плитки нет списка".to_owned())
    }

    /// Видимый диапазон строк таблицы: первая строка и число строк.
    fn visible(&self) -> (usize, usize) {
        self.list().map_or((0, 0), |list| {
            let table = list.table();
            let range = table.rowsInRect(table.visibleRect());
            (range.location, range.length)
        })
    }

    fn selected(&self) -> Option<usize> {
        self.list()
            .ok()
            .and_then(|list| usize::try_from(list.table().selectedRow()).ok())
    }

    fn state(&self) -> String {
        format!(
            "rows={} visible={:?} selected={:?} focus={:?} created={:?} {}",
            self.list().map_or(0, VirtualList::len),
            self.visible(),
            self.selected(),
            self.scene.tiles.focus(),
            self.list().map(VirtualList::created).ok(),
            events::focus_state(&self.stand.app)
        )
    }

    fn touch(&self) {
        self.mark.set(events::elapsed());
        *self.stable.borrow_mut() = Stable::new();
    }

    /// Миллисекунды от отметки до первого прохода отображения после неё.
    fn since_mark_to_pass(&self) -> Option<f64> {
        let mark = self.mark.get()?;
        events::display_pass_after(mark).map(|pass| events::to_ms(pass.saturating_sub(mark)))
    }

    fn settled(&self) -> bool {
        let Ok(list) = self.list() else {
            return false;
        };
        let frames = vec![
            readback::rect(self.window().frame()),
            readback::rect(list.view().documentVisibleRect()),
        ];
        self.since_mark_to_pass().is_some() && self.stable.borrow_mut().quiet(frames)
    }

    fn table_is_responder(&self) -> bool {
        self.list()
            .is_ok_and(|list| events::is_first_responder(self.window(), list.table()))
    }

    fn click_row(&self, row: usize) -> Result<(), String> {
        let table = self.list()?.table();
        let frame = table.rectOfRow(row as isize);
        let in_window = table.convertPoint_toView(events::center(frame), None);
        let point = self.window().convertPointToScreen(in_window);
        let label = format!("строка {row}");
        let gesture = events::mouse_down(self.mtm(), point, self.window(), &label)?;
        let up = gesture.up(&label)?;
        *self.click.borrow_mut() = Some((gesture.down, up));
        Ok(())
    }

    fn click_done(&self) -> bool {
        self.click
            .borrow()
            .and_then(|(down, _)| events::record(down))
            .and_then(|record| record.send_return)
            .is_some_and(|back| events::display_pass_after(back).is_some())
    }

    /// Подпись реализованной ячейки строки, без создания вида.
    fn cell_text(&self, row: usize) -> Option<String> {
        let table = self.list().ok()?.table();
        let view = table.viewAtColumn_row_makeIfNecessary(0, row as isize, false)?;
        let cell = view.downcast::<NSTableCellView>().ok()?;
        // SAFETY: `textField` у NSTableCellView.
        let field = unsafe { cell.textField() }?;
        Some(field.stringValue().to_string())
    }

    fn row_background(&self, row: usize) -> Option<Rgba> {
        let table = self.list().ok()?.table();
        let view = table.rowViewAtRow_makeIfNecessary(row as isize, false)?;
        view.layer()
            .and_then(|layer| readback::layer_state(&layer).background)
    }

    /// Число строк, видимый диапазон и подписи реализованных ячеек против модели.
    fn check_screen(&self, state: &str, first: Option<usize>, last: Option<usize>) {
        let Ok(list) = self.list() else {
            return;
        };
        let rows = list.table().numberOfRows();
        let (start, length) = self.visible();
        self.max_visible.set(self.max_visible.get().max(length));
        let end = (start + length).saturating_sub(1);
        let mut problems = Vec::new();
        if rows != list.len() as isize {
            problems.push(format!("numberOfRows {rows} при модели {}", list.len()));
        }
        if length == 0 {
            problems.push("видимых строк нет".to_owned());
        }
        if let Some(first) = first
            && start != first
        {
            problems.push(format!("первая видимая {start}, ожидалась {first}"));
        }
        if let Some(last) = last
            && end != last
        {
            problems.push(format!("последняя видимая {end}, ожидалась {last}"));
        }
        let mut realized = 0;
        for row in start..start + length {
            match (self.cell_text(row), list.title(row)) {
                (Some(text), Some(title)) if text == title => realized += 1,
                (text, title) => {
                    problems.push(format!("строка {row}: ячейка {text:?}, модель {title:?}"))
                }
            }
        }
        self.report.borrow_mut().finding(
            &format!("screen_{state}"),
            verdict(problems.is_empty()),
            if problems.is_empty() {
                format!(
                    "строк {rows}, видимы {start}…{end} ({length}), подписи {realized} реализованных ячеек совпадают с моделью"
                )
            } else {
                problems.join("; ")
            },
        );
    }

    /// Виды ячеек и строк, созданные списком за набор, и живые виды списка не больше порога
    /// по наибольшему числу видимых строк.
    fn check_reuse(&self, state: &str) {
        let Ok(list) = self.list() else {
            return;
        };
        let (cells, rows) = list.created();
        let (cells_at, rows_at) = self.created_at_fixture.get();
        let (cells_new, rows_new) = (cells - cells_at, rows - rows_at);
        let (cells_live, rows_live) = list.live();
        let visible = self.max_visible.get();
        let limit = reuse_limit(visible);
        let mut report = self.report.borrow_mut();
        report.push_json(&format!("created_{state}"), &(cells, rows));
        report.push_json(&format!("live_{state}"), &(cells_live, rows_live));
        report.finding(
            &format!("reuse_{state}"),
            verdict([cells_new, rows_new, cells_live, rows_live].iter().all(|n| *n <= limit)),
            format!(
                "за набор создано видов ячеек {cells_new} и строк {rows_new}, живых {cells_live} и {rows_live}, всего создано {cells} и {rows}; порог {limit} при видимых строках не больше {visible}"
            ),
        );
    }

    /// Выбор таблицы, фон слоя выбранной строки, если она реализована, first responder и
    /// фокус сцены.
    fn check_selection(&self, state: &str, want: usize) {
        let selected = self.selected();
        let (start, length) = self.visible();
        let on_screen = (start..start + length).contains(&want);
        let background = self.row_background(want);
        let sel = self.scene.tokens().sel;
        let responder = self.table_is_responder();
        let focus = self.scene.tiles.focus();
        let ok = selected == Some(want)
            && (!on_screen || background == Some(sel))
            && responder
            && focus == Focus::Tile(CENTER);
        self.report.borrow_mut().finding(
            &format!("selection_{state}"),
            verdict(ok),
            format!(
                "selectedRow {selected:?}, ожидалась {want}; видима {on_screen}; фон строки {:?}, ожидался {}; first responder — таблица: {responder} ({}); фокус {focus:?}",
                background.map(|color| color.hex()),
                sel.hex(),
                events::responder_name(self.window())
            ),
        );
    }

    /// Дерево доступности таблицы: роль, все строки, видимые строки и выбранная строка по
    /// индексу, даже если она прокручена за край. Чтение дерева не создаёт видов.
    fn check_accessibility(&self, state: &str, want: usize) {
        let Ok(list) = self.list() else {
            return;
        };
        let table: &AnyObject = list.table().as_ref();
        let created_before = list.created();
        let element = readback::accessibility(table);
        let started = std::time::Instant::now();
        let rows = readback::ax_array(table, sel!(accessibilityRows));
        let rows_ms = events::to_ms(started.elapsed());
        // Строка элемента: `accessibilityIndex`, иначе строка вида в таблице.
        let index_of = |row: &AnyObject| {
            readback::ax_index(row).or_else(|| {
                row.downcast_ref::<NSView>()
                    .map(|view| list.table().rowForView(view))
                    .filter(|row| *row >= 0)
            })
        };
        let mut classes = std::collections::BTreeSet::new();
        let mut indexes = |selector| {
            readback::ax_array(table, selector).map(|rows| {
                rows.iter()
                    .map(|row| {
                        classes.insert(readback::class_name(&row));
                        index_of(&row)
                    })
                    .collect::<Vec<_>>()
            })
        };
        let visible_ax = indexes(sel!(accessibilityVisibleRows));
        let selected_items: Vec<Option<isize>> =
            indexes(sel!(accessibilitySelectedRows)).unwrap_or_default();
        let created_after = list.created();
        let (start, length) = self.visible();
        let want_visible: Vec<Option<isize>> = (start..start + length)
            .map(|row| Some(row as isize))
            .collect();
        let row_count = rows.as_ref().map(|rows| rows.count());
        let mut problems = Vec::new();
        if element.role.as_deref() != Some("AXTable") {
            problems.push(format!("роль {:?}", element.role));
        }
        if row_count != Some(list.len()) {
            problems.push(format!(
                "accessibilityRows {row_count:?} при {} строках",
                list.len()
            ));
        }
        if visible_ax.as_ref() != Some(&want_visible) {
            problems.push(format!(
                "accessibilityVisibleRows {:?} при видимых {start}…{}",
                visible_ax.as_ref().map(|rows| (
                    rows.first().copied(),
                    rows.last().copied(),
                    rows.len()
                )),
                (start + length).saturating_sub(1)
            ));
        }
        if selected_items != [Some(want as isize)] {
            problems.push(format!(
                "accessibilitySelectedRows {selected_items:?}, ожидалась {want}"
            ));
        }
        if created_after != created_before {
            problems.push(format!(
                "чтение дерева создало виды: {created_before:?} → {created_after:?}"
            ));
        }
        let mut report = self.report.borrow_mut();
        report.push(&format!("ax_rows_ms_{state}"), format!("{rows_ms:.3}"));
        report.push_json(&format!("ax_row_classes_{state}"), &classes);
        report.finding(
            &format!("accessibility_{state}"),
            verdict(problems.is_empty()),
            if problems.is_empty() {
                format!(
                    "роль AXTable, строк {}, видимые {start}…{} совпадают с таблицей, выбрана {want} (видима {}), accessibilityRows за {rows_ms:.1} мс, новых видов нет",
                    list.len(),
                    (start + length).saturating_sub(1),
                    (start..start + length).contains(&want)
                )
            } else {
                problems.join("; ")
            },
        );
    }

    /// Обновлённые строки показывают новые подписи, остальные видимые — прежние.
    fn check_update(&self, state: &str, edited: &[usize]) {
        let Ok(list) = self.list() else {
            return;
        };
        let revision = self.revision.get();
        let mut problems = Vec::new();
        for row in edited {
            let want = fixtures::edited(&fixtures::list_row(*row), revision).title;
            let got = self.cell_text(*row);
            if got.as_deref() != Some(want.as_str()) || list.title(*row).as_deref() != Some(&want) {
                problems.push(format!("строка {row}: ячейка {got:?}, ожидалась {want}"));
            }
        }
        let (start, length) = self.visible();
        for row in (start..start + length).filter(|row| !edited.contains(row)) {
            let want = fixtures::list_row(row).title;
            if self.cell_text(row).as_deref() != Some(want.as_str()) {
                problems.push(format!("строка {row} изменилась без правки"));
            }
        }
        self.report.borrow_mut().finding(
            &format!("update_{state}"),
            verdict(problems.is_empty()),
            if problems.is_empty() {
                format!(
                    "{} строк показывают правку {revision}, остальные видимые без изменений",
                    edited.len()
                )
            } else {
                problems.join("; ")
            },
        );
    }

    fn drive_moves(&self) -> Poll<()> {
        let mut moves = self.moves.borrow_mut();
        if moves.error.is_some() {
            return Poll::Ready(());
        }
        if !moves.numbers.is_empty() {
            let records: Option<Vec<events::Record>> = moves
                .numbers
                .iter()
                .map(|number| events::record(*number))
                .collect();
            let Some(records) = records else {
                moves.error = Some("нет записи события перемещения".to_owned());
                return Poll::Ready(());
            };
            let returns: Option<Vec<Duration>> =
                records.iter().map(|record| record.send_return).collect();
            let Some(last) = returns.and_then(|returns| returns.into_iter().max()) else {
                return Poll::Pending(format!("перемещение {}: события не вернулись", moves.done));
            };
            if events::display_pass_after(last).is_none() {
                return Poll::Pending(format!("перемещение {}: нет прохода", moves.done));
            }
            let selected = self.selected();
            let (start, length) = self.visible();
            if !(start..start + length).contains(&moves.expected) {
                moves.lagging += 1;
            }
            if selected != Some(moves.expected) {
                let done = moves.done;
                let expected = moves.expected;
                moves.mismatches.push(format!(
                    "{done}: выбрана {selected:?}, ожидалась {expected}"
                ));
            }
            let key_down = &records[0];
            if moves.done >= WARMUP
                && let (Some(start), Some(back)) = (key_down.dispatch_start, key_down.send_return)
            {
                moves
                    .post_to_state
                    .push(events::to_ms(back.saturating_sub(key_down.posted)));
                moves
                    .post_to_dispatch
                    .push(events::to_ms(start.saturating_sub(key_down.posted)));
                moves
                    .dispatch_to_return
                    .push(events::to_ms(back.saturating_sub(start)));
                if let Some(pass) = events::display_pass_after(back) {
                    moves
                        .post_to_pass
                        .push(events::to_ms(pass.saturating_sub(key_down.posted)));
                }
            }
            moves.done += 1;
            moves.numbers.clear();
        }
        if moves.done >= WARMUP + MOVES {
            return Poll::Ready(());
        }
        moves.expected = self.selected().map_or(0, |row| row + 1);
        match events::press(Key::DOWN) {
            Ok(numbers) => moves.numbers = numbers,
            Err(reason) => {
                moves.error = Some(reason);
                return Poll::Ready(());
            }
        }
        Poll::Pending(format!("перемещение {} отправлено", moves.done))
    }

    fn conclude_moves(&self, size: usize) -> Result<(), String> {
        let moves = self.moves.replace(Moves::default());
        if let Some(error) = moves.error {
            return Err(format!("перемещения: {error}"));
        }
        let p95 = crate::stats::summarize(&moves.post_to_pass)
            .ok()
            .map(|summary| summary.p95);
        let mut report = self.report.borrow_mut();
        report.push(&format!("moves_lagging_{size}"), moves.lagging);
        report.finding(
            &format!("moves_{size}"),
            verdict(moves.mismatches.is_empty()),
            if moves.mismatches.is_empty() {
                format!(
                    "{} перемещений Down, из них {WARMUP} прогревочных: после каждого выбрана следующая строка",
                    moves.done
                )
            } else {
                moves.mismatches.join("; ")
            },
        );
        report.metric(
            &format!("list_{size}_move_post_to_state"),
            "ms",
            moves.post_to_state,
        );
        report.metric(
            &format!("list_{size}_move_post_to_dispatch"),
            "ms",
            moves.post_to_dispatch,
        );
        report.metric(
            &format!("list_{size}_move_post_to_display_pass"),
            "ms",
            moves.post_to_pass,
        );
        report.metric(
            &format!("list_{size}_move_dispatch_to_return"),
            "ms",
            moves.dispatch_to_return,
        );
        report.finding(
            &format!("move_budget_{size}"),
            verdict(p95.is_some_and(|p95| p95 <= BUDGET_MS)),
            format!(
                "p95 интервала postEvent → конец прохода отображения {} при бюджете {BUDGET_MS} мс; вывод об измеренном участке, не о времени до экрана",
                p95.map_or_else(|| "нет".to_owned(), |p95| format!("{p95:.3} мс"))
            ),
        );
        Ok(())
    }

    fn observe_pass(&self, key: &str) {
        let value = self
            .since_mark_to_pass()
            .map_or_else(|| "нет".to_owned(), |ms| format!("{ms:.3}"));
        self.report.borrow_mut().push(key, value);
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::List);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(scene) = stand.built_scene.clone() else {
        return stand.conclude(report, Err("сцена не собрана".to_owned()));
    };
    let tile = scene.tiles.tiles[CENTER].clone();
    if tile.list().is_none() {
        return stand.conclude(report, Err("у центральной плитки нет списка".to_owned()));
    }
    report.push("cell", scene.cell().name());
    report.push_json("list_sizes", &LIST_SIZES);
    report.push("scene_rows", SCENE_ROWS);
    report.push("row_height", scene.tokens().row);
    report.push(
        "interval_first_screen",
        "reloadData → конец первого прохода отображения после него; это не момент кадра на экране",
    );
    report.push(
        "interval_scroll",
        "scrollRowToVisible → конец первого прохода отображения после него",
    );
    report.push(
        "interval_post_to_state",
        "postEvent → наблюдаемое состояние AppKit: выбор меняется синхронно, поэтому конец интервала — возврат sendEvent",
    );
    report.push(
        "interval_post_to_display_pass",
        "postEvent → конец прохода отображения после возврата sendEvent; это не момент кадра на экране",
    );
    report.push(
        "interval_dispatch_to_return",
        "начало dispatch → возврат sendEvent",
    );
    report.push(
        "interval_post_to_dispatch",
        "postEvent → начало dispatch: ожидание в очереди событий",
    );
    let run = Rc::new(Run {
        settle: Settle::new(vec![scene.window.clone()]),
        stand: Rc::clone(&stand),
        scene,
        tile,
        report: RefCell::new(report),
        mark: Cell::new(None),
        stable: RefCell::new(Stable::new()),
        click: RefCell::new(None),
        moves: RefCell::new(Moves::default()),
        max_visible: Cell::new(0),
        revision: Cell::new(0),
        created_at_fixture: Cell::new((0, 0)),
    });
    run_steps(steps(&run), move |result| finish(run, result));
}

fn step(
    run: &Rc<Run>,
    name: &'static str,
    timeout: Duration,
    ready: impl Fn(&Run) -> bool + 'static,
    act: impl FnOnce(&Run) -> Result<(), String> + 'static,
) -> Step {
    let (for_ready, for_act) = (Rc::clone(run), Rc::clone(run));
    Step::new(
        name,
        timeout,
        move || poll(ready(&for_ready), || for_ready.state()),
        move || act(&for_act),
    )
}

fn steps(run: &Rc<Run>) -> Vec<Step> {
    let first = {
        let run = Rc::clone(run);
        move || {
            if let Poll::Pending(state) = run.settle.poll() {
                return Poll::Pending(state);
            }
            let app = &run.stand.app;
            let key = app.keyWindow();
            poll(
                app.isActive() && key.is_some_and(|key| events::same(&*key, run.window())),
                || run.state(),
            )
        }
    };
    let warp = {
        let run = Rc::clone(run);
        move || events::warp_cursor_outside(run.mtm(), &run.stand.app).map(|_| ())
    };
    let mut steps = vec![
        Step::new(
            "окно неподвижно, активность и key window",
            SETTLE_TIMEOUT,
            first,
            warp,
        ),
        step(
            run,
            "курсор вне окон",
            STEP_TIMEOUT,
            |run| events::cursor_outside(&run.stand.app),
            |run| {
                run.touch();
                Ok(())
            },
        ),
    ];
    for (index, size) in LIST_SIZES.into_iter().enumerate() {
        fixture_steps(run, &mut steps, index, size);
    }
    steps
}

/// Имена шагов статичны, поэтому у каждого набора свои.
const NAMES: [[&str; 8]; 3] = [
    [
        "1000: загрузка",
        "1000: первый экран",
        "1000: щелчок по строке обработан",
        "1000: конец списка",
        "1000: начало списка",
        "1000: 10 строк обновлены",
        "1000: 110 перемещений Down",
        "1000: прокрутка после перемещений установилась",
    ],
    [
        "10000: загрузка",
        "10000: первый экран",
        "10000: щелчок по строке обработан",
        "10000: конец списка",
        "10000: начало списка",
        "10000: 10 строк обновлены",
        "10000: 110 перемещений Down",
        "10000: прокрутка после перемещений установилась",
    ],
    [
        "100000: загрузка",
        "100000: первый экран",
        "100000: щелчок по строке обработан",
        "100000: конец списка",
        "100000: начало списка",
        "100000: 10 строк обновлены",
        "100000: 110 перемещений Down",
        "100000: прокрутка после перемещений установилась",
    ],
];

fn fixture_steps(run: &Rc<Run>, steps: &mut Vec<Step>, index: usize, size: usize) {
    let names = NAMES[index];
    let edited: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
    steps.push(step(
        run,
        names[0],
        STEP_TIMEOUT,
        Run::settled,
        move |run| {
            let rows = fixtures::list(size);
            let list = run.list()?;
            run.created_at_fixture.set(list.created());
            run.touch();
            list.set_rows(rows);
            Ok(())
        },
    ));
    steps.push(step(
        run,
        names[1],
        STEP_TIMEOUT,
        Run::settled,
        move |run| {
            let state = format!("{size}_first");
            run.observe_pass(&format!("first_screen_ms_{size}"));
            run.check_screen(&state, Some(0), None);
            run.check_reuse(&state);
            run.click_row(CLICK_ROW)
        },
    ));
    steps.push(step(
        run,
        names[2],
        STEP_TIMEOUT,
        |run| run.click_done() && run.table_is_responder(),
        move |run| {
            let state = format!("{size}_click");
            run.check_selection(&state, CLICK_ROW);
            run.check_accessibility(&state, CLICK_ROW);
            run.touch();
            run.list()?.table().scrollRowToVisible(size as isize - 1);
            Ok(())
        },
    ));
    steps.push(step(
        run,
        names[3],
        STEP_TIMEOUT,
        Run::settled,
        move |run| {
            let state = format!("{size}_end");
            run.observe_pass(&format!("scroll_end_ms_{size}"));
            run.check_screen(&state, None, Some(size - 1));
            run.check_reuse(&state);
            // Выбранная строка прокручена за край.
            run.check_accessibility(&state, CLICK_ROW);
            run.touch();
            run.list()?.table().scrollRowToVisible(0);
            Ok(())
        },
    ));
    let for_update = Rc::clone(&edited);
    steps.push(step(
        run,
        names[4],
        STEP_TIMEOUT,
        Run::settled,
        move |run| {
            let state = format!("{size}_start");
            run.observe_pass(&format!("scroll_start_ms_{size}"));
            run.check_screen(&state, Some(0), None);
            run.check_selection(&state, CLICK_ROW);
            run.check_reuse(&state);
            let (start, length) = run.visible();
            let rows: Vec<usize> = (start..start + length.min(EDITS)).collect();
            if rows.len() < EDITS {
                return Err(format!("видимых строк {length}, правке нужно {EDITS}"));
            }
            run.revision.set(run.revision.get() + 1);
            let revision = run.revision.get();
            run.touch();
            run.list()?
                .update_rows(&rows, |row| fixtures::edited(row, revision));
            *for_update.borrow_mut() = rows;
            Ok(())
        },
    ));
    steps.push(step(
        run,
        names[5],
        STEP_TIMEOUT,
        Run::settled,
        move |run| {
            let state = format!("{size}_update");
            run.observe_pass(&format!("update_ms_{size}"));
            run.check_update(&state, &edited.borrow());
            run.check_reuse(&state);
            *run.moves.borrow_mut() = Moves::default();
            Ok(())
        },
    ));
    let driver = {
        let run = Rc::clone(run);
        move || run.drive_moves()
    };
    let conclude = {
        let run = Rc::clone(run);
        move || {
            run.conclude_moves(size)?;
            run.touch();
            Ok(())
        }
    };
    steps.push(Step::new(names[6], MOVE_TIMEOUT, driver, conclude));
    steps.push(step(
        run,
        names[7],
        SCROLL_TIMEOUT,
        Run::settled,
        move |run| {
            let state = format!("{size}_moves");
            let want = CLICK_ROW + WARMUP + MOVES;
            run.check_selection(&state, want);
            let (start, length) = run.visible();
            run.report.borrow_mut().finding(
                &format!("moves_visible_{size}"),
                verdict((start..start + length).contains(&want)),
                format!(
                    "после установившейся прокрутки видимы {start}…{}, выбрана {want}",
                    (start + length).saturating_sub(1)
                ),
            );
            run.check_reuse(&state);
            Ok(())
        },
    ));
}

fn finish(run: Rc<Run>, result: Result<(), StepFailure>) {
    let stand = Rc::clone(&run.stand);
    let mut report = run.report.replace(Report::default());
    run.settle.add_to(&mut report);
    if let Err(failure) = result {
        return stand.conclude(report, Err(failure.describe()));
    }
    report.push("max_visible_rows", run.max_visible.get());
    capture(run, report);
}

/// Рендер плитки со списком и снимок окна для визуального прохода владельца.
fn capture(run: Rc<Run>, mut report: Report) {
    let stand = Rc::clone(&run.stand);
    let render = match ViewRender::of(&run.scene.root) {
        Ok(render) => render,
        Err(reason) => return stand.conclude(report, Err(reason)),
    };
    let jobs: Vec<Background<Snapshot>> = vec![
        render.save(&stand.out, "list-content"),
        snapshot::capture_window(run.window(), &stand.out, "list-window"),
    ];
    snapshot::wait_all(jobs, SNAPSHOT_TIMEOUT, move |result| {
        let snapshots = match result {
            Ok(snapshots) => snapshots,
            Err(reason) => return stand.conclude(report, Err(reason)),
        };
        let mut failed = Vec::new();
        for snapshot in snapshots {
            match snapshot {
                Ok(snapshot) => {
                    if snapshot.status == SnapshotStatus::Error {
                        failed.push(format!(
                            "{}: {}",
                            snapshot.name,
                            snapshot.reason.as_deref().unwrap_or("ошибка")
                        ));
                    }
                    report.snapshot(snapshot);
                }
                Err(reason) => failed.push(reason),
            }
        }
        if !failed.is_empty() {
            return stand.conclude(report, Err(format!("снимки не записаны: {failed:?}")));
        }
        drop(run);
        stand.conclude(report, Ok(()));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_names_match_fixtures() {
        for (names, size) in NAMES.iter().zip(LIST_SIZES) {
            assert!(
                names
                    .iter()
                    .all(|name| name.starts_with(&format!("{size}:")))
            );
        }
        assert_eq!(reuse_limit(20), 50);
    }
}
