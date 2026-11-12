//! Сценарий `tree`: `TreeList` навигатора на 10 000 узлов. Перезагрузка без изменений, щелчок по
//! корню, раскрытие и свёртка клавишами Right и Left и щелчком по треугольнику, раскрытие и
//! свёртка всех корней с замером, 100 перемещений выбора после 10 прогревочных при раскрытом
//! первом уровне, перезагрузка с обратным порядком корней и свёртка над выбранным ребёнком.
//! Проверки читают состояние AppKit: строки и уровни дерева против модели фикстуры, подписи
//! реализованных ячеек, раскрытость узлов, выбор с узлом строки, слой выбранной строки, first
//! responder, дерево доступности, счётчики узлов и число созданных видов.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, sel};
use objc2_app_kit::{NSTableCellView, NSView, NSWindow};
use objc2_foundation::NSPoint;

use super::events::{self, Key};
use super::readback::{self, Rgba};
use super::snapshot::{self, Background, ViewRender};
use super::split::Stable;
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepFailure, Verdict, poll,
    run_steps,
};
use crate::fixtures::{self, TREE_BRANCHING, TREE_SIZE, TreeSpec};
use crate::results::{Snapshot, SnapshotStatus};
use crate::scene::Scene;
use crate::tiles::{Focus, NAV_EXPANDED_ROOT, NAV_SELECTED};
use crate::tree::{self, Reload, TreeList, TreeNode};

const STEP_TIMEOUT: Duration = Duration::from_secs(5);
const ROUNDS_TIMEOUT: Duration = Duration::from_secs(30);
const MOVE_TIMEOUT: Duration = Duration::from_secs(40);
const SCROLL_TIMEOUT: Duration = Duration::from_secs(10);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);
const WARMUP: usize = 10;
const MOVES: usize = 100;
const ROUND_WARMUP: usize = 3;
const ROUNDS: usize = 20;
/// Бюджет реакции на клавишу в `Navigation` из `docs/architecture.md`.
const BUDGET_MS: f64 = 16.0;
/// Корень, который сценарий раскрывает клавишами и щелчком; в начальной сцене он свёрнут.
const TOGGLE_ROOT: usize = 1;

#[derive(Clone, Copy)]
struct Phase {
    expanding: bool,
    start: Duration,
    end: Duration,
}

/// Раунды раскрытия и свёртки всех корней.
#[derive(Default)]
struct Rounds {
    done: usize,
    pending: Option<Phase>,
    expand_call: Vec<f64>,
    expand_pass: Vec<f64>,
    collapse_call: Vec<f64>,
    collapse_pass: Vec<f64>,
    mismatches: Vec<String>,
    error: Option<String>,
}

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
    lagging: usize,
    error: Option<String>,
}

struct Run {
    stand: Rc<Stand>,
    scene: Rc<Scene>,
    settle: Rc<Settle>,
    report: RefCell<Report>,
    mark: Cell<Option<Duration>>,
    stable: RefCell<Stable>,
    click: RefCell<Option<(u64, u64)>>,
    keys: RefCell<Vec<u64>>,
    /// Модель фикстуры в исходном и обратном порядке корней.
    spec: Vec<TreeSpec>,
    reversed_spec: Vec<TreeSpec>,
    reversed: Cell<bool>,
    /// Узлы, которые сценарий ожидает раскрытыми.
    expanded: RefCell<BTreeSet<u64>>,
    rounds: RefCell<Rounds>,
    moves: RefCell<Moves>,
    /// Наибольшее число видимых строк за прогон.
    max_visible: Cell<usize>,
    /// Созданные деревом виды на начало отрезка между перезагрузками.
    created_at_segment: Cell<(usize, usize)>,
    nodes_created_at_start: Cell<usize>,
    /// Узел, выбранный перед перезагрузкой с обратным порядком корней, и его строка.
    reorder_from: Cell<Option<(u64, isize)>>,
    reload: RefCell<Option<Reload>>,
}

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

/// Порог видов строк из раздела Technical Details плана.
fn reuse_limit(visible: usize) -> usize {
    2 * visible + 10
}

/// Строк при раскрытых корнях без их потомков.
fn first_level_rows() -> usize {
    TREE_BRANCHING[0] * (1 + TREE_BRANCHING[1])
}

impl Run {
    fn mtm(&self) -> MainThreadMarker {
        self.stand.app.mtm()
    }

    fn window(&self) -> &NSWindow {
        &self.scene.window
    }

    fn tree(&self) -> &TreeList {
        self.scene.tiles.navigator.tree()
    }

    /// Корень с индексом `index` в исходном порядке фикстуры.
    fn root(&self, index: usize) -> Result<Retained<TreeNode>, String> {
        let id = self
            .spec
            .get(index)
            .map(|root| root.id)
            .ok_or_else(|| format!("в фикстуре нет корня {index}"))?;
        self.node_by_id(id)
    }

    fn node_by_id(&self, id: u64) -> Result<Retained<TreeNode>, String> {
        self.tree()
            .roots()
            .into_iter()
            .find(|root| root.id() == id)
            .ok_or_else(|| format!("в дереве нет корня {id}"))
    }

    fn expected_rows(&self) -> Vec<(u64, usize)> {
        let spec = if self.reversed.get() {
            &self.reversed_spec
        } else {
            &self.spec
        };
        let expanded = self.expanded.borrow();
        fixtures::visible_rows(spec, &|id| expanded.contains(&id))
    }

    fn actual_rows(&self) -> Vec<(u64, usize)> {
        let tree = self.tree();
        let outline = tree.outline();
        (0..outline.numberOfRows())
            .map(|row| {
                (
                    tree.node_at(row).map_or(0, |node| node.id()),
                    usize::try_from(outline.levelForRow(row)).unwrap_or(usize::MAX),
                )
            })
            .collect()
    }

    /// Видимый диапазон строк дерева: первая строка и число строк.
    fn visible(&self) -> (usize, usize) {
        let outline = self.tree().outline();
        let range = outline.rowsInRect(outline.visibleRect());
        (range.location, range.length)
    }

    fn selected_id(&self) -> Option<u64> {
        self.tree().selected_node().map(|node| node.id())
    }

    fn state(&self) -> String {
        format!(
            "rows={} visible={:?} selected={:?} ({:?}) focus={:?} created={:?} {}",
            self.tree().outline().numberOfRows(),
            self.visible(),
            self.tree().outline().selectedRow(),
            self.selected_id(),
            self.scene.tiles.focus(),
            self.tree().created(),
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

    /// Окно, видимая часть дерева и его кадр не меняются: анимация раскрытия закончилась.
    fn settled(&self) -> bool {
        let tree = self.tree();
        let frames = vec![
            readback::rect(self.window().frame()),
            readback::rect(tree.view().documentVisibleRect()),
            readback::rect(tree.outline().frame()),
        ];
        self.since_mark_to_pass().is_some() && self.stable.borrow_mut().quiet(frames)
    }

    fn outline_is_responder(&self) -> bool {
        events::is_first_responder(self.window(), self.tree().outline())
    }

    fn click_at(&self, in_outline: NSPoint, label: &str) -> Result<(), String> {
        let outline = self.tree().outline();
        let in_window = outline.convertPoint_toView(in_outline, None);
        let point = self.window().convertPointToScreen(in_window);
        let gesture = events::mouse_down(self.mtm(), point, self.window(), label)?;
        let up = gesture.up(label)?;
        *self.click.borrow_mut() = Some((gesture.down, up));
        Ok(())
    }

    fn click_row(&self, row: isize) -> Result<(), String> {
        let frame = self.tree().outline().rectOfRow(row);
        self.click_at(events::center(frame), &format!("строка {row}"))
    }

    /// Щелчок по треугольнику раскрытия строки; класс вида под точкой пишется наблюдением.
    fn click_disclosure(&self, row: isize, state: &str) -> Result<(), String> {
        let outline = self.tree().outline();
        let frame = outline.frameOfOutlineCellAtRow(row);
        if frame.size.width <= 0.0 {
            return Err(format!("у строки {row} нет треугольника раскрытия"));
        }
        let center = events::center(frame);
        let in_window = outline.convertPoint_toView(center, None);
        let hit = self.window().contentView().and_then(|content| {
            // SAFETY: `superview` у вида, который держит окно.
            let frame_view = unsafe { content.superview() }?;
            frame_view.hitTest(in_window)
        });
        self.report.borrow_mut().push(
            &format!("disclosure_hit_{state}"),
            format!(
                "{} в {} (кадр треугольника {})",
                hit.as_deref()
                    .map_or_else(|| "none".to_owned(), |view| readback::class_name(view)),
                events::describe_point(center),
                events::describe_rect(frame)
            ),
        );
        self.click_at(center, &format!("треугольник строки {row}"))
    }

    /// Нажатие вернулось из `sendEvent`, и после этого прошёл проход отображения.
    fn click_done(&self) -> bool {
        self.click
            .borrow()
            .and_then(|(down, _)| events::record(down))
            .and_then(|record| record.send_return)
            .is_some_and(|back| events::display_pass_after(back).is_some())
    }

    fn press(&self, key: Key) -> Result<(), String> {
        *self.keys.borrow_mut() = events::press(key)?;
        Ok(())
    }

    fn keys_done(&self) -> bool {
        let numbers = self.keys.borrow();
        let returns: Option<Vec<Duration>> = numbers
            .iter()
            .map(|number| events::record(*number).and_then(|record| record.send_return))
            .collect();
        returns
            .and_then(|returns| returns.into_iter().max())
            .is_some_and(|last| events::display_pass_after(last).is_some())
    }

    /// Интервал «postEvent → конец прохода отображения» последней клавиши.
    fn observe_key(&self, key: &str) {
        let value = self
            .keys
            .borrow()
            .first()
            .and_then(|number| events::record(*number))
            .and_then(|record| {
                let back = record.send_return?;
                let pass = events::display_pass_after(back)?;
                Some(format!(
                    "{:.3}",
                    events::to_ms(pass.saturating_sub(record.posted))
                ))
            })
            .unwrap_or_else(|| "нет".to_owned());
        self.report.borrow_mut().push(key, value);
    }

    fn observe_pass(&self, key: &str) {
        let value = self
            .since_mark_to_pass()
            .map_or_else(|| "нет".to_owned(), |ms| format!("{ms:.3}"));
        self.report.borrow_mut().push(key, value);
    }

    /// Подпись реализованной ячейки строки, без создания вида.
    fn cell_text(&self, row: usize) -> Option<String> {
        let outline = self.tree().outline();
        let view = outline.viewAtColumn_row_makeIfNecessary(0, row as isize, false)?;
        let cell = view.downcast::<NSTableCellView>().ok()?;
        // SAFETY: `textField` у NSTableCellView.
        let field = unsafe { cell.textField() }?;
        Some(field.stringValue().to_string())
    }

    /// Строки и уровни дерева против модели фикстуры и подписи реализованных ячеек.
    fn check_rows(&self, state: &str) {
        let expected = self.expected_rows();
        let actual = self.actual_rows();
        let (start, length) = self.visible();
        self.max_visible.set(self.max_visible.get().max(length));
        let mut problems = Vec::new();
        if actual.len() != expected.len() {
            problems.push(format!(
                "строк {}, по модели {}",
                actual.len(),
                expected.len()
            ));
        }
        if let Some(index) =
            (0..actual.len().min(expected.len())).find(|i| actual[*i] != expected[*i])
        {
            problems.push(format!(
                "строка {index}: узел и уровень {:?}, по модели {:?}",
                actual[index], expected[index]
            ));
        }
        if length == 0 {
            problems.push("видимых строк нет".to_owned());
        }
        let mut realized = 0;
        for row in start..start + length {
            let title = self
                .tree()
                .node_at(row as isize)
                .map(|node| node.title().to_owned());
            match (self.cell_text(row), title) {
                (Some(text), Some(title)) if text == title => realized += 1,
                (text, title) => {
                    problems.push(format!("строка {row}: ячейка {text:?}, узел {title:?}"))
                }
            }
        }
        self.report.borrow_mut().finding(
            &format!("rows_{state}"),
            verdict(problems.is_empty()),
            if problems.is_empty() {
                format!(
                    "строк {}, узлы и уровни совпадают с моделью, видимы {start}…{} ({length}), подписи {realized} реализованных ячеек совпадают с узлами",
                    actual.len(),
                    (start + length).saturating_sub(1)
                )
            } else {
                problems.join("; ")
            },
        );
    }

    /// Живых узлов столько, сколько в фикстуре, и новых объектов узлов не создано.
    fn check_nodes(&self, state: &str) {
        let count = tree::node_count();
        let at_start = self.nodes_created_at_start.get();
        let model = self.tree().len();
        let ok = count.live() == TREE_SIZE && count.created == at_start && model == TREE_SIZE;
        self.report.borrow_mut().finding(
            &format!("nodes_{state}"),
            verdict(ok),
            format!(
                "ProbeTreeNode: живых {}, создано {} (на начало сценария {at_start}), освобождено {}; узлов в модели {model}, в фикстуре {TREE_SIZE}",
                count.live(),
                count.created,
                count.released
            ),
        );
    }

    /// Рамка слоя выбранной строки: толщина и цвет.
    fn ring(&self) -> Option<(f64, Option<Rgba>)> {
        let layer = self.tree().selected_row_view()?.layer()?;
        let state = readback::layer_state(&layer);
        Some((state.border_width, state.border))
    }

    /// Выбранный узел, строка выбора, фон и рамка слоя строки; при `focused` также first
    /// responder и фокус сцены в навигаторе.
    fn check_selection(&self, state: &str, want: u64, focused: bool) {
        let tree = self.tree();
        let outline = tree.outline();
        let row = outline.selectedRow();
        let node = tree.selected_node();
        let node_row = node.as_ref().map(|node| tree.row_of(node));
        let (start, length) = self.visible();
        let on_screen =
            usize::try_from(row).is_ok_and(|row| (start..start + length).contains(&row));
        let background = tree
            .selected_row_view()
            .and_then(|view| view.layer())
            .and_then(|layer| readback::layer_state(&layer).background);
        let tokens = &self.scene.tokens();
        let ring = self.ring();
        let ring_ok = match (focused, ring) {
            (true, Some((width, color))) => width == 1.0 && color == Some(tokens.focus),
            (false, Some((width, _))) => width == 0.0,
            (_, None) => !on_screen,
        };
        let responder = self.outline_is_responder();
        let focus = self.scene.tiles.focus();
        let ok = node.as_ref().map(|node| node.id()) == Some(want)
            && node_row == Some(row)
            && (!on_screen || background == Some(tokens.sel))
            && ring_ok
            && (!focused || (responder && focus == Focus::Navigator));
        self.report.borrow_mut().finding(
            &format!("selection_{state}"),
            verdict(ok),
            format!(
                "выбран узел {:?} в строке {row} (rowForItem {node_row:?}), ожидался {want}; видима {on_screen}; фон строки {:?}, ожидался {}; рамка {ring:?}, ожидалась {}; first responder — дерево: {responder} ({}); фокус {focus:?}",
                node.as_ref().map(|node| node.id()),
                background.map(|color| color.hex()),
                tokens.sel.hex(),
                if focused {
                    format!("1 точка {}", tokens.focus.hex())
                } else {
                    "нет".to_owned()
                },
                events::responder_name(self.window())
            ),
        );
    }

    fn check_expansion(&self, state: &str, node: &TreeNode, want: bool) {
        let expanded = self.tree().is_expanded(node);
        self.report.borrow_mut().finding(
            &format!("expand_{state}"),
            verdict(expanded == want),
            format!(
                "isItemExpanded узла {} — {expanded}, ожидалось {want}; строк {}",
                node.id(),
                self.tree().outline().numberOfRows()
            ),
        );
    }

    /// Виды ячеек и строк, созданные деревом за отрезок, и живые виды дерева не больше порога
    /// по наибольшему числу видимых строк.
    fn check_reuse(&self, state: &str) {
        let tree = self.tree();
        let (cells, rows) = tree.created();
        let (cells_at, rows_at) = self.created_at_segment.get();
        let (cells_new, rows_new) = (cells - cells_at, rows - rows_at);
        let (cells_live, rows_live) = tree.live();
        let visible = self.max_visible.get();
        let limit = reuse_limit(visible);
        let mut report = self.report.borrow_mut();
        report.push_json(&format!("created_{state}"), &(cells, rows));
        report.push_json(&format!("live_{state}"), &(cells_live, rows_live));
        report.finding(
            &format!("reuse_{state}"),
            verdict([cells_new, rows_new, cells_live, rows_live].iter().all(|n| *n <= limit)),
            format!(
                "за отрезок создано видов ячеек {cells_new} и строк {rows_new}, живых {cells_live} и {rows_live}, всего создано {cells} и {rows}; порог {limit} при видимых строках не больше {visible}"
            ),
        );
    }

    /// Дерево доступности: роль, все строки, выбор и у видимых и выбранной строк раскрытость,
    /// уровень и выбранность против `NSOutlineView`. Чтение дерева не создаёт видов.
    fn check_accessibility(&self, state: &str) {
        let tree = self.tree();
        let outline = tree.outline();
        let object: &AnyObject = outline;
        let created_before = tree.created();
        let element = readback::accessibility(object);
        let started = std::time::Instant::now();
        let rows = readback::ax_array(object, sel!(accessibilityRows));
        let rows_ms = events::to_ms(started.elapsed());
        let selected_row = outline.selectedRow();
        let selected_items: Vec<Option<isize>> =
            readback::ax_array(object, sel!(accessibilitySelectedRows))
                .map(|items| items.iter().map(|item| readback::ax_index(&item)).collect())
                .unwrap_or_default();
        let (start, length) = self.visible();
        let mut sample: BTreeSet<isize> = (start..start + length).map(|row| row as isize).collect();
        if selected_row >= 0 {
            sample.insert(selected_row);
        }
        let mut problems = Vec::new();
        let mut via = BTreeSet::new();
        let mut classes = BTreeSet::new();
        let row_count = rows.as_ref().map(|rows| rows.count());
        if element.role.as_deref() != Some("AXOutline") {
            problems.push(format!("роль {:?}", element.role));
        }
        if row_count != usize::try_from(outline.numberOfRows()).ok() {
            problems.push(format!(
                "accessibilityRows {row_count:?} при {} строках",
                outline.numberOfRows()
            ));
        }
        if selected_items != [Some(selected_row)] {
            problems.push(format!(
                "accessibilitySelectedRows {selected_items:?}, выбрана {selected_row}"
            ));
        }
        let mut checked = 0;
        if let Some(rows) = &rows {
            for row in &sample {
                let Some(item) = usize::try_from(*row)
                    .ok()
                    .filter(|index| *index < rows.count())
                    .map(|index| rows.objectAtIndex(index))
                else {
                    problems.push(format!("нет элемента строки {row}"));
                    continue;
                };
                classes.insert(readback::class_name(&item));
                let index = readback::ax_index(&item);
                let disclosure = readback::ax_disclosure(&item);
                via.extend(disclosure.via.iter().copied());
                let expanded = tree.node_at(*row).map(|node| tree.is_expanded(&node));
                let level = outline.levelForRow(*row);
                let selected = *row == selected_row;
                if index != Some(*row)
                    || disclosure.disclosing != expanded
                    || disclosure.level != Some(level)
                    || disclosure.selected != Some(selected)
                {
                    problems.push(format!(
                        "строка {row}: индекс {index:?}, раскрыта {:?} (дерево {expanded:?}), уровень {:?} (дерево {level}), выбрана {:?} (дерево {selected})",
                        disclosure.disclosing, disclosure.level, disclosure.selected
                    ));
                } else {
                    checked += 1;
                }
            }
        }
        let created_after = tree.created();
        if created_after != created_before {
            problems.push(format!(
                "чтение дерева создало виды: {created_before:?} → {created_after:?}"
            ));
        }
        let mut report = self.report.borrow_mut();
        report.push(&format!("ax_rows_ms_{state}"), format!("{rows_ms:.3}"));
        report.push_json(&format!("ax_row_classes_{state}"), &classes);
        report.push_json(&format!("ax_disclosure_via_{state}"), &via);
        report.finding(
            &format!("accessibility_{state}"),
            verdict(problems.is_empty()),
            if problems.is_empty() {
                format!(
                    "роль AXOutline, строк {row_count:?}, выбрана {selected_row}; раскрытость, уровень и выбор {checked} строк совпадают с деревом; accessibilityRows за {rows_ms:.1} мс, новых видов нет"
                )
            } else {
                problems.join("; ")
            },
        );
    }

    fn check_reload(&self, state: &str, want_row_change: bool) {
        let Some(reload) = self.reload.borrow_mut().take() else {
            self.report.borrow_mut().finding(
                &format!("reload_{state}"),
                Verdict::Fail,
                "нет итога перезагрузки",
            );
            return;
        };
        let selected_now = self
            .selected_id()
            .map(|id| (self.tree().outline().selectedRow(), id));
        let kept_node = match (reload.selected_before, reload.selected_after) {
            (Some((_, before)), Some((_, after))) => before == after,
            _ => false,
        };
        let row_changed = match (reload.selected_before, reload.selected_after) {
            (Some((before, _)), Some((after, _))) => before != after,
            _ => false,
        };
        let all_expanded = self
            .expanded
            .borrow()
            .iter()
            .filter_map(|id| self.node_by_id(*id).ok())
            .all(|node| self.tree().is_expanded(&node));
        let ok = kept_node
            && row_changed == want_row_change
            && selected_now == reload.selected_after
            && all_expanded;
        let mut report = self.report.borrow_mut();
        report.push(
            &format!("reload_appkit_selection_{state}"),
            format!(
                "до reloadData выбор {:?}, сразу после — строка и узел {:?}",
                reload.selected_before, reload.appkit_selected
            ),
        );
        report.push(
            &format!("reload_appkit_expanded_{state}"),
            format!(
                "AppKit сам оставил раскрытыми {} из {} узлов",
                reload.appkit_kept_expanded, reload.expanded_before
            ),
        );
        report.finding(
            &format!("reload_{state}"),
            verdict(ok),
            format!(
                "выбор до {:?}, после возврата узлу {:?}, сейчас {selected_now:?}; узел тот же: {kept_node}; строка сменилась: {row_changed} (ожидалось {want_row_change}); ожидаемые раскрытые узлы раскрыты: {all_expanded}",
                reload.selected_before, reload.selected_after
            ),
        );
    }

    fn drive_rounds(&self) -> Poll<()> {
        let mut rounds = self.rounds.borrow_mut();
        if rounds.error.is_some() {
            return Poll::Ready(());
        }
        if let Some(phase) = rounds.pending {
            let Some(pass) = events::display_pass_after(phase.end) else {
                return Poll::Pending(format!("раунд {}: нет прохода", rounds.done));
            };
            rounds.pending = None;
            let rows = usize::try_from(self.tree().outline().numberOfRows()).unwrap_or(0);
            let want = if phase.expanding {
                first_level_rows()
            } else {
                TREE_BRANCHING[0]
            };
            if rows != want {
                let done = rounds.done;
                rounds
                    .mismatches
                    .push(format!("{done}: строк {rows}, ожидалось {want}"));
            }
            if rounds.done >= 2 * ROUND_WARMUP {
                let call = events::to_ms(phase.end.saturating_sub(phase.start));
                let to_pass = events::to_ms(pass.saturating_sub(phase.start));
                if phase.expanding {
                    rounds.expand_call.push(call);
                    rounds.expand_pass.push(to_pass);
                } else {
                    rounds.collapse_call.push(call);
                    rounds.collapse_pass.push(to_pass);
                }
            }
            rounds.done += 1;
        }
        if rounds.done >= 2 * (ROUND_WARMUP + ROUNDS) {
            return Poll::Ready(());
        }
        let expanding = rounds.done.is_multiple_of(2);
        let Some(start) = events::elapsed() else {
            rounds.error = Some("нет часов стенда".to_owned());
            return Poll::Ready(());
        };
        if expanding {
            self.tree().expand_roots();
        } else {
            self.tree().collapse_roots();
        }
        let end = events::elapsed().unwrap_or(start);
        rounds.pending = Some(Phase {
            expanding,
            start,
            end,
        });
        Poll::Pending(format!("раунд {} начат", rounds.done))
    }

    fn conclude_rounds(&self) -> Result<(), String> {
        let rounds = self.rounds.replace(Rounds::default());
        if let Some(error) = rounds.error {
            return Err(format!("раунды: {error}"));
        }
        let mut report = self.report.borrow_mut();
        report.finding(
            "rounds",
            verdict(rounds.mismatches.is_empty()),
            if rounds.mismatches.is_empty() {
                format!(
                    "{} раскрытий и свёрток всех {} корней, из них {} прогревочных: после раскрытия {} строк, после свёртки {}",
                    rounds.done,
                    TREE_BRANCHING[0],
                    2 * ROUND_WARMUP,
                    first_level_rows(),
                    TREE_BRANCHING[0]
                )
            } else {
                rounds.mismatches.join("; ")
            },
        );
        report.metric("tree_expand_roots_call", "ms", rounds.expand_call);
        report.metric(
            "tree_expand_roots_to_display_pass",
            "ms",
            rounds.expand_pass,
        );
        report.metric("tree_collapse_roots_call", "ms", rounds.collapse_call);
        report.metric(
            "tree_collapse_roots_to_display_pass",
            "ms",
            rounds.collapse_pass,
        );
        Ok(())
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
            let selected = usize::try_from(self.tree().outline().selectedRow()).ok();
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
        moves.expected =
            usize::try_from(self.tree().outline().selectedRow()).map_or(0, |row| row + 1);
        match events::press(Key::DOWN) {
            Ok(numbers) => moves.numbers = numbers,
            Err(reason) => {
                moves.error = Some(reason);
                return Poll::Ready(());
            }
        }
        Poll::Pending(format!("перемещение {} отправлено", moves.done))
    }

    fn conclude_moves(&self) -> Result<(), String> {
        let moves = self.moves.replace(Moves::default());
        if let Some(error) = moves.error {
            return Err(format!("перемещения: {error}"));
        }
        let p95 = crate::stats::summarize(&moves.post_to_pass)
            .ok()
            .map(|summary| summary.p95);
        let mut report = self.report.borrow_mut();
        report.push("moves_lagging", moves.lagging);
        report.finding(
            "moves",
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
        report.metric("tree_move_post_to_state", "ms", moves.post_to_state);
        report.metric("tree_move_post_to_dispatch", "ms", moves.post_to_dispatch);
        report.metric("tree_move_post_to_display_pass", "ms", moves.post_to_pass);
        report.metric(
            "tree_move_dispatch_to_return",
            "ms",
            moves.dispatch_to_return,
        );
        report.finding(
            "move_budget",
            verdict(p95.is_some_and(|p95| p95 <= BUDGET_MS)),
            format!(
                "p95 интервала postEvent → конец прохода отображения {} при бюджете {BUDGET_MS} мс; вывод об измеренном участке, не о времени до экрана",
                p95.map_or_else(|| "нет".to_owned(), |p95| format!("{p95:.3} мс"))
            ),
        );
        Ok(())
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Tree);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(scene) = stand.built_scene.clone() else {
        return stand.conclude(report, Err("сцена не собрана".to_owned()));
    };
    let spec = fixtures::tree(&TREE_BRANCHING);
    let mut reversed_spec = spec.clone();
    reversed_spec.reverse();
    report.push("cell", scene.cell().name());
    report.push_json("tree_branching", &TREE_BRANCHING);
    report.push("tree_size", fixtures::tree_size(&spec));
    report.push("tree_depth", fixtures::tree_depth(&spec));
    report.push("row_height", scene.tokens().row);
    report.push("indent", tree::INDENT);
    report.push(
        "strongly_references_items",
        scene
            .tiles
            .navigator
            .tree()
            .outline()
            .stronglyReferencesItems(),
    );
    report.push(
        "interval_reload",
        "reloadData с возвратом раскрытия и выбора → конец первого прохода отображения после него",
    );
    report.push(
        "interval_roots",
        "вызовы expandItem или collapseItem у всех корней: «call» — их длительность, «to_display_pass» — от начала вызовов до конца первого прохода отображения после них",
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
    report.push(
        "moves_handler",
        "Up и Down обрабатывает list::step_selection пробы, как у VirtualList; Left и Right — NSOutlineView",
    );
    let expanded = BTreeSet::from([spec[NAV_EXPANDED_ROOT].id]);
    let run = Rc::new(Run {
        settle: Settle::new(vec![scene.window.clone()]),
        stand: Rc::clone(&stand),
        scene,
        report: RefCell::new(report),
        mark: Cell::new(None),
        stable: RefCell::new(Stable::new()),
        click: RefCell::new(None),
        keys: RefCell::new(Vec::new()),
        spec,
        reversed_spec,
        reversed: Cell::new(false),
        expanded: RefCell::new(expanded),
        rounds: RefCell::new(Rounds::default()),
        moves: RefCell::new(Moves::default()),
        max_visible: Cell::new(0),
        created_at_segment: Cell::new((0, 0)),
        nodes_created_at_start: Cell::new(0),
        reorder_from: Cell::new(None),
        reload: RefCell::new(None),
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
                run.nodes_created_at_start.set(tree::node_count().created);
                run.created_at_segment.set(run.tree().created());
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "дерево загружено",
            STEP_TIMEOUT,
            Run::settled,
            |run| {
                run.check_rows("initial");
                run.check_nodes("initial");
                let want = run
                    .expected_rows()
                    .get(NAV_SELECTED as usize)
                    .map(|(id, _)| *id)
                    .ok_or("в модели нет начальной выбранной строки")?;
                run.check_selection("initial", want, false);
                run.check_reuse("initial");
                run.created_at_segment.set(run.tree().created());
                run.touch();
                *run.reload.borrow_mut() = Some(run.tree().reload());
                Ok(())
            },
        ),
        step(
            run,
            "перезагрузка без изменений",
            STEP_TIMEOUT,
            Run::settled,
            |run| {
                run.observe_pass("reload_ms");
                run.check_reload("plain", false);
                run.check_rows("reload");
                run.check_nodes("reload");
                run.check_accessibility("reload");
                let root = run.root(TOGGLE_ROOT)?;
                run.click_row(run.tree().row_of(&root))
            },
        ),
        step(
            run,
            "щелчок по корню обработан",
            STEP_TIMEOUT,
            |run| run.click_done() && run.outline_is_responder(),
            |run| {
                let root = run.root(TOGGLE_ROOT)?;
                run.check_selection("click", root.id(), true);
                run.touch();
                run.press(Key::RIGHT)
            },
        ),
        step(
            run,
            "Right обработана",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.settled(),
            |run| {
                let root = run.root(TOGGLE_ROOT)?;
                run.expanded.borrow_mut().insert(root.id());
                run.observe_key("key_right_post_to_display_pass_ms");
                run.check_expansion("key-right", &root, true);
                run.check_rows("key-right");
                run.check_selection("key-right", root.id(), true);
                run.touch();
                run.press(Key::LEFT)
            },
        ),
        step(
            run,
            "Left обработана",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.settled(),
            |run| {
                let root = run.root(TOGGLE_ROOT)?;
                run.expanded.borrow_mut().remove(&root.id());
                run.observe_key("key_left_post_to_display_pass_ms");
                run.check_expansion("key-left", &root, false);
                run.check_rows("key-left");
                run.touch();
                run.click_disclosure(run.tree().row_of(&root), "open")
            },
        ),
        step(
            run,
            "щелчок по треугольнику обработан",
            STEP_TIMEOUT,
            |run| run.click_done() && run.settled(),
            |run| {
                let root = run.root(TOGGLE_ROOT)?;
                run.expanded.borrow_mut().insert(root.id());
                run.check_expansion("click-open", &root, true);
                run.check_rows("click-open");
                run.report.borrow_mut().push(
                    "disclosure_click_responder",
                    events::responder_name(run.window()),
                );
                run.touch();
                run.click_disclosure(run.tree().row_of(&root), "close")
            },
        ),
        step(
            run,
            "второй щелчок по треугольнику обработан",
            STEP_TIMEOUT,
            |run| run.click_done() && run.settled(),
            |run| {
                let root = run.root(TOGGLE_ROOT)?;
                run.expanded.borrow_mut().remove(&root.id());
                run.check_expansion("click-close", &root, false);
                run.check_rows("click-close");
                *run.rounds.borrow_mut() = Rounds::default();
                Ok(())
            },
        ),
    ];
    let rounds_driver = {
        let run = Rc::clone(run);
        move || run.drive_rounds()
    };
    let rounds_done = {
        let run = Rc::clone(run);
        move || {
            run.conclude_rounds()?;
            run.expanded.borrow_mut().clear();
            run.touch();
            Ok(())
        }
    };
    steps.push(Step::new(
        "раскрытие и свёртка всех корней",
        ROUNDS_TIMEOUT,
        rounds_driver,
        rounds_done,
    ));
    steps.push(step(
        run,
        "корни свёрнуты",
        STEP_TIMEOUT,
        Run::settled,
        |run| {
            run.check_rows("rounds");
            let root = run.root(TOGGLE_ROOT)?;
            run.check_selection("rounds", root.id(), true);
            run.expanded
                .borrow_mut()
                .extend(run.spec.iter().map(|root| root.id));
            run.tree().expand_roots();
            run.tree().outline().scrollRowToVisible(0);
            run.touch();
            Ok(())
        },
    ));
    steps.push(step(
        run,
        "первый уровень раскрыт",
        STEP_TIMEOUT,
        Run::settled,
        |run| {
            run.check_rows("first-level");
            run.check_accessibility("first-level");
            run.check_reuse("first-level");
            run.click_row(0)
        },
    ));
    steps.push(step(
        run,
        "щелчок по первой строке обработан",
        STEP_TIMEOUT,
        |run| run.click_done() && run.outline_is_responder(),
        |run| {
            run.check_selection("moves-start", run.spec[0].id, true);
            *run.moves.borrow_mut() = Moves::default();
            Ok(())
        },
    ));
    let moves_driver = {
        let run = Rc::clone(run);
        move || run.drive_moves()
    };
    let moves_done = {
        let run = Rc::clone(run);
        move || {
            run.conclude_moves()?;
            run.touch();
            Ok(())
        }
    };
    steps.push(Step::new(
        "110 перемещений Down",
        MOVE_TIMEOUT,
        moves_driver,
        moves_done,
    ));
    steps.push(step(
        run,
        "прокрутка после перемещений установилась",
        SCROLL_TIMEOUT,
        Run::settled,
        |run| {
            let row = WARMUP + MOVES;
            let want = run
                .expected_rows()
                .get(row)
                .map(|(id, _)| *id)
                .ok_or_else(|| format!("в модели нет строки {row}"))?;
            run.check_selection("moves", want, true);
            let (start, length) = run.visible();
            run.report.borrow_mut().finding(
                "moves_visible",
                verdict((start..start + length).contains(&row)),
                format!(
                    "после установившейся прокрутки видимы {start}…{}, выбрана {row}",
                    (start + length).saturating_sub(1)
                ),
            );
            run.check_accessibility("moves");
            run.check_reuse("moves");
            run.reorder_from.set(Some((want, row as isize)));
            run.created_at_segment.set(run.tree().created());
            run.reversed.set(true);
            run.touch();
            *run.reload.borrow_mut() = Some(run.tree().reverse_roots());
            Ok(())
        },
    ));
    steps.push(step(
        run,
        "перезагрузка с обратным порядком корней",
        STEP_TIMEOUT,
        Run::settled,
        |run| {
            run.observe_pass("reorder_reload_ms");
            let (want, _) = run
                .reorder_from
                .get()
                .ok_or("нет выбора перед перезагрузкой")?;
            run.check_reload("reordered", true);
            run.check_rows("reordered");
            run.check_nodes("reordered");
            run.check_selection("reordered", want, true);
            run.check_accessibility("reordered");
            run.check_reuse("reordered");
            run.touch();
            run.press(Key::DOWN)
        },
    ));
    steps.push(step(
        run,
        "Down к первому ребёнку",
        STEP_TIMEOUT,
        |run| run.keys_done() && run.settled(),
        |run| {
            let (parent_id, _) = run
                .reorder_from
                .get()
                .ok_or("нет выбора перед перезагрузкой")?;
            let parent = run.node_by_id(parent_id)?;
            let child = parent
                .children()
                .first()
                .map(|child| child.id())
                .ok_or("у выбранного корня нет детей")?;
            run.check_selection("child", child, true);
            run.touch();
            run.tree().collapse(&parent, false);
            run.expanded.borrow_mut().remove(&parent_id);
            Ok(())
        },
    ));
    steps.push(step(
        run,
        "свёртка над выбранным ребёнком",
        STEP_TIMEOUT,
        Run::settled,
        |run| {
            let (parent_id, _) = run
                .reorder_from
                .get()
                .ok_or("нет выбора перед перезагрузкой")?;
            let parent = run.node_by_id(parent_id)?;
            let row = run.tree().outline().selectedRow();
            let selected = run.selected_id();
            run.report.borrow_mut().push(
                "collapse_over_selection",
                format!(
                    "после свёртки корня {parent_id} (строка {}) над выбранным ребёнком выбрана строка {row}, узел {selected:?}; выбран сам корень: {}",
                    run.tree().row_of(&parent),
                    selected == Some(parent_id)
                ),
            );
            run.check_rows("collapse-over-selection");
            Ok(())
        },
    ));
    steps
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

/// Рендер сцены с деревом и снимок окна для визуального прохода владельца.
fn capture(run: Rc<Run>, mut report: Report) {
    let stand = Rc::clone(&run.stand);
    let navigator: &NSView = &run.scene.tiles.navigator;
    let render = match ViewRender::of(navigator) {
        Ok(render) => render,
        Err(reason) => return stand.conclude(report, Err(reason)),
    };
    let jobs: Vec<Background<Snapshot>> = vec![
        render.save(&stand.out, "tree-navigator"),
        snapshot::capture_window(run.window(), &stand.out, "tree-window"),
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
    fn scenario_rows_fit_the_fixture() {
        // 110 перемещений идут при раскрытом первом уровне.
        assert!(first_level_rows() > WARMUP + MOVES);
        assert_eq!(first_level_rows(), 1_000);
        // Корень, который раскрывает сценарий, в начальной сцене свёрнут.
        assert_ne!(TOGGLE_ROOT, NAV_EXPANDED_ROOT);
        assert_eq!(reuse_limit(26), 62);
    }
}
