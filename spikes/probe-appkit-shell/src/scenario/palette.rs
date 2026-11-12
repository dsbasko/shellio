//! Сценарий `palette`: палитра сцены. Cmd-K открывает её дочерней панелью основного окна;
//! проверяются key и main окна, first responder с field editor, геометрия, поверхность, тень и
//! отсутствие материала и затемнения. Клавиши в палитре не доходят до плиток, Tab ходит по кругу
//! между полем и списком, Cmd-A из «Правки» выделяет текст поля, а копирование и вставка
//! проверяются разрешением цели действия: общий буфер владельца сценарий не трогает. Escape и
//! Enter закрывают палитру с возвратом фокуса прежнему виду, убранный прежний вид заменяет
//! запасной, щелчок в родителя закрывает палитру. Затем 100 циклов открытия, фильтра одним
//! символом и закрытия после 10 прогревочных, палитра в полноэкранном режиме родителя, снимки, и
//! закрытие палитры вместе со скрытием, закрытием и сворачиванием родителя.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{MainThreadMarker, MainThreadOnly, Message, sel};
use objc2_app_kit::{NSEventModifierFlags, NSScreen, NSText, NSView, NSWindow, NSWindowStyleMask};
use objc2_foundation::NSPoint;

use super::events::{self, Key};
use super::launch::{input_source, require_latin};
use super::material::{self, Outcome};
use super::readback::{self, Rgba};
use super::snapshot::{self, Background, ViewRender};
use super::split::{Stable, WindowLog};
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepFailure, Verdict, poll,
    run_steps,
};
use crate::fixtures::{MARK_COMMAND, PALETTE_COMMANDS};
use crate::menu::Via;
use crate::palette::{self, Close, Palette, Restore};
use crate::results::{Rect, Snapshot, SnapshotStatus};
use crate::scene::Scene;
use crate::tiles::{CENTER, Focus, RIGHT};
use crate::tokens::Tokens;

const STEP_TIMEOUT: Duration = Duration::from_secs(5);
const CYCLES_TIMEOUT: Duration = Duration::from_secs(90);
const FULLSCREEN_TIMEOUT: Duration = Duration::from_secs(12);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);
/// Сколько ждать, что палитра закроется после того, как родитель убран.
const HIDE_GRACE: Duration = Duration::from_millis(1500);
const WARMUP: usize = 10;
const CYCLES: usize = 100;
/// Бюджет реакции на клавишу в `Navigation` из `docs/architecture.md`.
const BUDGET_MS: f64 = 16.0;
const GEOMETRY_TOLERANCE: f64 = 0.5;
/// Строка правого списка, по которой щёлкает сценарий, и строка щелчка при открытой палитре.
const RIGHT_ROW: isize = 3;
const PARENT_CLICK_ROW: isize = 5;

const NONE: NSEventModifierFlags = NSEventModifierFlags::empty();
const SHIFT: NSEventModifierFlags = NSEventModifierFlags::Shift;
const KEY_L: Key = Key {
    label: "l",
    code: 37,
    characters: "l",
    ignoring: "l",
    flags: NONE,
};
const KEY_SHIFT_L: Key = Key {
    label: "Shift-L",
    code: 37,
    characters: "L",
    ignoring: "L",
    flags: SHIFT,
};
const KEY_1: Key = Key {
    label: "1",
    code: 18,
    characters: "1",
    ignoring: "1",
    flags: NONE,
};
const KEY_ESCAPE: Key = Key {
    label: "Escape",
    code: 53,
    characters: "\u{1b}",
    ignoring: "\u{1b}",
    flags: NONE,
};
const KEY_TAB: Key = Key {
    label: "Tab",
    code: 48,
    characters: "\t",
    ignoring: "\t",
    flags: NONE,
};
const KEY_BACKTAB: Key = Key {
    label: "Shift-Tab",
    code: 48,
    characters: "\u{19}",
    ignoring: "\u{19}",
    flags: SHIFT,
};
const KEY_CMD_A: Key = Key {
    label: "Cmd-A",
    code: 0,
    characters: "a",
    ignoring: "a",
    flags: NSEventModifierFlags::Command,
};

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= GEOMETRY_TOLERANCE
}

fn near_rect(a: Rect, b: Rect) -> bool {
    near(a.x, b.x) && near(a.y, b.y) && near(a.width, b.width) && near(a.height, b.height)
}

fn describe(rect: Rect) -> String {
    format!("{},{} {}x{}", rect.x, rect.y, rect.width, rect.height)
}

/// Состояние плиток под палитрой: оно не должно меняться от клавиш, набранных в палитре.
#[derive(Clone, Debug, PartialEq)]
struct Beneath {
    focus: Focus,
    responder: usize,
    active: Vec<Option<usize>>,
    keys: Vec<usize>,
    lists: Vec<Option<usize>>,
    tree: isize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Open,
    Filter,
    Close,
}

/// Циклы открытия, фильтра одним символом и закрытия.
struct Cycles {
    phase: Phase,
    numbers: Vec<u64>,
    done: usize,
    /// Ожидаемый first responder родителя после закрытия.
    responder: usize,
    filtered: usize,
    late: usize,
    samples: [Samples; 3],
    /// Этапы открытия по `Palette::open_phases`.
    open_phases: Vec<(&'static str, Vec<f64>)>,
    /// Проверок подряд, на которых состояние фазы ещё не наступило.
    waiting: usize,
    mismatches: Vec<String>,
    error: Option<String>,
}

#[derive(Default)]
struct Samples {
    post_to_dispatch: Vec<f64>,
    dispatch_to_return: Vec<f64>,
    post_to_pass: Vec<f64>,
}

impl Cycles {
    fn new(responder: usize, filtered: usize) -> Cycles {
        Cycles {
            phase: Phase::Open,
            numbers: Vec::new(),
            done: 0,
            responder,
            filtered,
            late: 0,
            samples: Default::default(),
            open_phases: Vec::new(),
            waiting: 0,
            mismatches: Vec::new(),
            error: None,
        }
    }
}

struct Run {
    stand: Rc<Stand>,
    scene: Rc<Scene>,
    settle: Rc<Settle>,
    report: RefCell<Report>,
    keys: RefCell<Vec<u64>>,
    click: RefCell<Option<(u64, u64)>>,
    mark: Cell<Option<Duration>>,
    stable: RefCell<Stable>,
    /// First responder родителя перед открытием палитры.
    previous: Cell<usize>,
    beneath: RefCell<Option<Beneath>>,
    /// Вид правого списка, убранный сценарием, пока палитра открыта.
    removed: RefCell<Option<Retained<NSView>>>,
    marks_before: Cell<usize>,
    cycles: RefCell<Option<Cycles>>,
    snapshots: RefCell<Option<snapshot::Outcomes<Snapshot>>>,
    hidden_at: Cell<Option<Instant>>,
    root_subviews: Cell<usize>,
    log: Retained<WindowLog>,
}

fn address<T: ?Sized>(object: &T) -> usize {
    (object as *const T).cast::<()>() as usize
}

impl Run {
    fn mtm(&self) -> MainThreadMarker {
        self.stand.app.mtm()
    }

    fn window(&self) -> &NSWindow {
        &self.scene.window
    }

    fn palette(&self) -> &Palette {
        &self.scene.palette
    }

    fn panel(&self) -> &NSWindow {
        self.palette().panel()
    }

    fn tokens(&self) -> &Tokens {
        &self.scene.tokens
    }

    fn touch(&self) {
        self.mark.set(events::elapsed());
        *self.stable.borrow_mut() = Stable::new();
    }

    fn settled(&self) -> bool {
        let passed = self
            .mark
            .get()
            .is_some_and(|mark| events::display_pass_after(mark).is_some());
        let frames = vec![
            readback::rect(self.window().frame()),
            readback::rect(self.panel().frame()),
        ];
        passed && self.stable.borrow_mut().quiet(frames)
    }

    fn fullscreen(&self) -> bool {
        self.window()
            .styleMask()
            .contains(NSWindowStyleMask::FullScreen)
    }

    fn press(&self, key: Key) -> Result<(), String> {
        *self.keys.borrow_mut() = events::press(key)?;
        Ok(())
    }

    /// Все события клавиши вернулись из `sendEvent`, и после этого прошёл проход отображения.
    fn keys_done(&self) -> bool {
        returned_and_passed(&self.keys.borrow())
    }

    fn click_at(&self, point: NSPoint, label: &str) -> Result<(), String> {
        let gesture = events::mouse_down(self.mtm(), point, self.window(), label)?;
        let up = gesture.up(label)?;
        *self.click.borrow_mut() = Some((gesture.down, up));
        Ok(())
    }

    fn click_done(&self) -> bool {
        self.click
            .borrow()
            .and_then(|(down, up)| Some((events::record(down)?, events::record(up)?)))
            .and_then(|(down, up)| down.send_return.max(up.send_return))
            .is_some_and(|back| events::display_pass_after(back).is_some())
    }

    fn panel_is_key(&self) -> bool {
        self.stand
            .app
            .keyWindow()
            .is_some_and(|key| events::same(&*key, self.panel()))
    }

    fn parent_is_key(&self) -> bool {
        self.stand
            .app
            .keyWindow()
            .is_some_and(|key| events::same(&*key, self.window()))
    }

    fn parent_is_main(&self) -> bool {
        self.stand
            .app
            .mainWindow()
            .is_some_and(|main| events::same(&*main, self.window()))
    }

    /// First responder панели — field editor поля палитры.
    fn field_editing(&self) -> bool {
        match (
            self.palette().field().currentEditor(),
            self.panel().firstResponder(),
        ) {
            (Some(editor), Some(responder)) => events::same(&*editor, &*responder),
            _ => false,
        }
    }

    fn table_is_responder(&self) -> bool {
        events::is_first_responder(self.panel(), self.palette().list().table())
    }

    fn panel_attached(&self) -> bool {
        let child = self
            .window()
            .childWindows()
            .is_some_and(|children| children.iter().any(|w| events::same(&*w, self.panel())));
        let parent = self
            .panel()
            .parentWindow()
            .is_some_and(|parent| events::same(&*parent, self.window()));
        child || parent
    }

    fn opened(&self) -> bool {
        self.palette().is_open()
            && self.panel().isVisible()
            && self.panel_is_key()
            && self.field_editing()
    }

    fn closed(&self) -> bool {
        !self.palette().is_open() && !self.panel().isVisible() && !self.panel_attached()
    }

    fn parent_responder(&self) -> usize {
        self.window()
            .firstResponder()
            .map_or(0, |responder| address(&*responder))
    }

    fn field_text(&self) -> String {
        self.palette().field().stringValue().to_string()
    }

    fn state(&self) -> String {
        let palette = self.palette();
        format!(
            "open={} visible={} attached={} key_panel={} key_parent={} main_parent={} panel_responder={} parent_responder={} field={:?} results={} selected={:?} closes={:?} fullscreen={} miniaturized={} parent_visible={} {}",
            palette.is_open(),
            self.panel().isVisible(),
            self.panel_attached(),
            self.panel_is_key(),
            self.parent_is_key(),
            self.parent_is_main(),
            events::responder_name(self.panel()),
            events::responder_name(self.window()),
            self.field_text(),
            palette.results(),
            palette.list().selected(),
            palette.closes().last(),
            self.fullscreen(),
            self.window().isMiniaturized(),
            self.window().isVisible(),
            events::focus_state(&self.stand.app)
        )
    }

    fn beneath(&self) -> Beneath {
        let tiles = &self.scene.tiles;
        Beneath {
            focus: tiles.focus(),
            responder: self.parent_responder(),
            active: tiles
                .tiles
                .iter()
                .map(|tile| tile.strip().active())
                .collect(),
            keys: tiles
                .tiles
                .iter()
                .map(|tile| tile.strip().keys_handled())
                .collect(),
            lists: tiles
                .tiles
                .iter()
                .map(|tile| tile.list().and_then(|list| list.selected()))
                .collect(),
            tree: tiles.navigator.tree().outline().selectedRow(),
        }
    }

    fn check_beneath(&self, name: &str, keys: &str) {
        let before = self.beneath.borrow().clone();
        let now = self.beneath();
        let same = before.as_ref() == Some(&now);
        self.report.borrow_mut().finding(
            name,
            verdict(same),
            if same {
                format!(
                    "{keys} в палитре не изменили плитки: фокус {:?}, активные вкладки {:?}, обработанные полосами клавиши {:?}, выбор списков {:?}, выбор дерева {}",
                    now.focus, now.active, now.keys, now.lists, now.tree
                )
            } else {
                format!("{keys}: до {before:?}, после {now:?}")
            },
        );
    }

    /// Палитра открыта: окна, first responder, связь с родителем и геометрия.
    fn check_open(&self, state: &str) {
        let responder = readback::first_responder(self.panel());
        let surface = self.surface_on_screen();
        let expected = readback::rect(palette::surface_frame(self.window().frame()));
        let panel = readback::rect(self.panel().frame());
        let margin = self.palette().margin();
        let expected_panel = Rect {
            x: expected.x - margin,
            y: expected.y - margin,
            width: expected.width + 2.0 * margin,
            height: expected.height + 2.0 * margin,
        };
        let focus_ok = self.panel_is_key()
            && self.parent_is_main()
            && self.field_editing()
            && responder.as_ref().is_some_and(|responder| {
                responder.field_editor && responder.logical_class == "NSTextField"
            });
        let attached = self.panel_attached();
        let geometry = surface.is_some_and(|surface| near_rect(surface, expected))
            && near_rect(panel, expected_panel);
        let mut report = self.report.borrow_mut();
        report.push_json(&format!("panel_responder_{state}"), &responder);
        report.finding(
            &format!("focus_{state}"),
            verdict(focus_ok),
            format!(
                "key — палитра: {}, main — основное окно: {}, first responder панели — field editor поля: {}; {}",
                self.panel_is_key(),
                self.parent_is_main(),
                self.field_editing(),
                events::focus_state(&self.stand.app)
            ),
        );
        report.finding(
            &format!("child_{state}"),
            verdict(attached),
            format!(
                "панель — дочернее окно основного: {attached}; parentWindow {:?}",
                self.panel().parentWindow().map(|w| w.windowNumber())
            ),
        );
        report.finding(
            &format!("geometry_{state}"),
            verdict(geometry),
            format!(
                "поверхность {} при ожидаемой {} (по центру окна {}, на {} ниже его верха), панель {} при ожидаемой {} с полем тени {margin}",
                surface.map_or_else(|| "нет".to_owned(), describe),
                describe(expected),
                describe(readback::rect(self.window().frame())),
                palette::TOP,
                describe(panel),
                describe(expected_panel)
            ),
        );
    }

    fn surface_on_screen(&self) -> Option<Rect> {
        readback::frame_on_screen(self.palette().surface())
    }

    /// Поверхность непрозрачна, без затемнения окна, тень из токенов, окно без материала.
    fn check_surface(&self) {
        let tokens = *self.tokens();
        let palette = self.palette();
        let surface = palette
            .surface()
            .layer()
            .map(|layer| readback::layer_state(&layer));
        let surface_ok = surface.as_ref().is_some_and(|layer| {
            layer.background == Some(tokens.surface)
                && near(layer.corner_radius, tokens.r_float)
                && near(layer.border_width, 1.0)
                && layer.border == Some(tokens.line_2)
        });
        let masks = palette
            .surface()
            .layer()
            .is_some_and(|layer| layer.masksToBounds());
        let window_background = self.panel().backgroundColor();
        let background = readback::srgb(&window_background);
        let window_ok = !self.panel().isOpaque()
            && !self.panel().hasShadow()
            && background.is_some_and(|color| color.a == 0);
        let surface_size = palette.surface().frame().size;
        let casters: Vec<String> = palette
            .shadows()
            .iter()
            .map(|layer| {
                let offset = layer.shadowOffset();
                let frame = layer.frame();
                format!(
                    "кадр {}x{} фон {:?} радиус {} тень {:?} opacity {:.3} radius {} offset {},{}",
                    frame.size.width,
                    frame.size.height,
                    layer
                        .backgroundColor()
                        .as_deref()
                        .and_then(readback::cg_srgb),
                    layer.cornerRadius(),
                    layer.shadowColor().as_deref().and_then(readback::cg_srgb),
                    layer.shadowOpacity(),
                    layer.shadowRadius(),
                    offset.width,
                    offset.height
                )
            })
            .collect();
        let shadows_ok = palette.shadows().len() == tokens.shadow.len()
            && palette
                .shadows()
                .iter()
                .zip(tokens.shadow.iter())
                .all(|(layer, shadow)| {
                    let offset = layer.shadowOffset();
                    let frame = layer.frame();
                    let color = layer.shadowColor().as_deref().and_then(readback::cg_srgb);
                    color.is_some_and(|color| color.distance(shadow.color.color) <= 1)
                        && (f64::from(layer.shadowOpacity()) - shadow.color.alpha).abs() < 1e-3
                        && near(layer.shadowRadius(), shadow.blur / 2.0)
                        && near(offset.width, 0.0)
                        && near(offset.height, -shadow.y)
                        && near(frame.size.width, surface_size.width)
                        && near(frame.size.height, surface_size.height)
                        && near(layer.cornerRadius(), tokens.r_float)
                        && !layer.masksToBounds()
                });
        let visible: Vec<String> = self
            .stand
            .app
            .windows()
            .iter()
            .filter(|window| window.isVisible())
            .map(|window| events::describe_window(&window))
            .collect();
        let root_subviews = self.scene.root.subviews().count();
        let no_dim = visible.len() == 2 && root_subviews == self.root_subviews.get();
        let mut report = self.report.borrow_mut();
        report.push_json("surface_layer", &surface);
        report.push(
            "panel_background",
            background.map_or_else(|| "нет".to_owned(), Rgba::hex),
        );
        report.push_json("shadow_casters", &casters);
        report.push_json("visible_windows_open", &visible);
        report.finding(
            "surface",
            verdict(surface_ok && masks),
            format!(
                "слой поверхности {:?}: фон --surface {}, радиус --r-float {}, линия 1 --line-2 {}, обрезка по радиусу {masks}",
                surface.as_ref().map(|layer| (layer.background.map(Rgba::hex), layer.corner_radius, layer.border_width, layer.border.map(Rgba::hex))),
                tokens.surface.hex(),
                tokens.r_float,
                tokens.line_2.hex()
            ),
        );
        report.finding(
            "panel_window",
            verdict(window_ok),
            format!(
                "окно панели непрозрачно: {}, системная тень: {}, фон окна задан явно: {}",
                self.panel().isOpaque(),
                self.panel().hasShadow(),
                background.map_or_else(|| "нет".to_owned(), Rgba::hex)
            ),
        );
        report.finding(
            "shadow",
            verdict(shadows_ok),
            format!(
                "слои тени под поверхностью по --shadow (радиус слоя — половина размытия CSS, смещение вниз): {casters:?}"
            ),
        );
        report.finding(
            "no_dim",
            verdict(no_dim),
            format!(
                "видимые окна при открытой палитре: {visible:?}; видов в корне сцены {root_subviews}, до открытия {}",
                self.root_subviews.get()
            ),
        );
    }

    fn check_material(&self, state: &str) {
        let scan = material::scan_window(self.panel());
        let outcome = material::outcome(scan.found.iter());
        let mut report = self.report.borrow_mut();
        report.push_json(&format!("material_{state}"), &scan);
        report.finding(
            &format!("material_{state}"),
            verdict(outcome != Outcome::VisibleMaterial),
            format!(
                "в окне палитры {} (узлов {}, находок {})",
                outcome.describe(),
                scan.nodes,
                scan.found.len()
            ),
        );
    }

    /// Фон слоя реализованного вида строки `row` списка палитры.
    fn row_background(&self, row: usize) -> Option<Rgba> {
        let table = self.palette().list().table();
        let view = table.rowViewAtRow_makeIfNecessary(row as isize, false)?;
        view.layer()
            .and_then(|layer| readback::layer_state(&layer).background)
    }

    /// Выбранная строка залита `--sel`, остальные видимые без заливки.
    fn check_selection_paint(&self, name: &str) {
        let table = self.palette().list().table();
        let selected = self.palette().list().selected();
        let visible = table.rowsInRect(table.visibleRect());
        let sel = self.tokens().sel;
        let rows: Vec<(usize, Option<String>)> = (visible.location
            ..visible.location + visible.length)
            .map(|row| (row, self.row_background(row).map(|color| color.hex())))
            .collect();
        let layers = selected.is_some()
            && rows.iter().all(|(row, background)| {
                if Some(*row) == selected {
                    background.as_deref() == Some(sel.hex().as_str())
                } else {
                    background.as_deref() != Some(sel.hex().as_str())
                }
            });
        // Пиксели рендера списка у правого края строки, где нет подписи.
        let probe = |row: usize| -> Result<Rgba, String> {
            let frame = table.rectOfRow(row as isize);
            let point = NSPoint::new(
                frame.origin.x + frame.size.width - 20.0,
                frame.origin.y + frame.size.height / 2.0,
            );
            ViewRender::of(table)?.pixel(point)
        };
        let surface = self.tokens().surface;
        let other = selected.map(|row| if row == 0 { 1 } else { row - 1 });
        let picked = selected.map(probe).transpose();
        let plain = other.map(probe).transpose();
        let pixels = matches!(
            (&picked, &plain),
            (Ok(Some(picked)), Ok(Some(plain)))
                if picked.distance(sel) <= 1 && plain.distance(surface) <= 1
        );
        self.report.borrow_mut().finding(
            name,
            verdict(layers && pixels),
            format!(
                "выбрана {selected:?}, фон слоёв видимых строк {rows:?}, у выбранной ожидался --sel {}; пиксель рендера выбранной строки {:?}, соседней {other:?} {:?}, ожидались {} и --surface {}",
                sel.hex(),
                picked.map(|color| color.map(Rgba::hex)),
                plain.map(|color| color.map(Rgba::hex)),
                sel.hex(),
                surface.hex()
            ),
        );
    }

    fn check_results(&self, name: &str, query: &str) {
        let palette = self.palette();
        let folded: Vec<String> = palette
            .model()
            .iter()
            .map(|command| palette::fold(&command.title))
            .collect();
        let expected = palette::filter(folded.iter().map(String::as_str), query).len();
        let rows = usize::try_from(palette.list().table().numberOfRows()).unwrap_or(usize::MAX);
        let empty_shown = !palette.empty_label().isHidden();
        let selected = palette.list().selected();
        let ok = self.field_text() == query
            && rows == expected
            && palette.results() == expected
            && empty_shown == (expected == 0)
            && (expected == 0 || selected == Some(0));
        self.report.borrow_mut().finding(
            name,
            verdict(ok),
            format!(
                "запрос {query:?} (в поле {:?}): строк таблицы {rows}, результатов {}, ожидалось {expected} из {}; выбрана {selected:?}; надпись «{}» видна: {empty_shown}",
                self.field_text(),
                palette.results(),
                palette.model().len(),
                palette::EMPTY
            ),
        );
    }

    /// Палитра закрыта, фокус у ожидаемого first responder родителя.
    fn check_closed(
        &self,
        name: &str,
        reason: Close,
        restore: Restore,
        responder: usize,
        key: bool,
    ) {
        let last = self.palette().closes().last().copied();
        let responder_ok = self.parent_responder() == responder;
        let key_ok = !key || self.parent_is_key();
        let ok = self.closed()
            && last == Some((reason, restore))
            && responder_ok
            && key_ok
            && self.parent_is_main();
        self.report.borrow_mut().finding(
            name,
            verdict(ok),
            format!(
                "закрыта: {}, причина и возврат фокуса {last:?} (ожидалось {:?}), first responder родителя {} ожидаемый: {responder_ok}, key — основное окно: {}, main — основное окно: {}",
                self.closed(),
                (reason, restore),
                events::responder_name(self.window()),
                self.parent_is_key(),
                self.parent_is_main()
            ),
        );
    }

    fn check_edit_menu(&self) {
        let app = &self.stand.app;
        let editor = self.palette().field().currentEditor();
        let resolve = |action: Sel| -> bool {
            // SAFETY: стандартные селекторы «Правки»; поиск цели по цепочке ответчиков.
            let target = unsafe { app.targetForAction_to_from(action, None, None) };
            match (&target, &editor) {
                (Some(target), Some(editor)) => {
                    let target: &AnyObject = target;
                    let editor: &NSText = editor;
                    events::same(target, editor)
                }
                _ => false,
            }
        };
        let actions = [sel!(copy:), sel!(paste:), sel!(cut:), sel!(selectAll:)];
        let resolved: Vec<(String, bool)> = actions
            .iter()
            .map(|action| {
                (
                    action.name().to_string_lossy().into_owned(),
                    resolve(*action),
                )
            })
            .collect();
        let menu = self.scene.edit_menu.clone();
        if let Some(menu) = &menu {
            menu.update();
        }
        let item = |title: &str| {
            menu.as_ref()
                .and_then(|menu| menu.itemWithTitle(&objc2_foundation::NSString::from_str(title)))
        };
        let copy_enabled = item("Скопировать").is_some_and(|item| item.isEnabled());
        let cut_enabled = item("Вырезать").is_some_and(|item| item.isEnabled());
        let paste_item =
            item("Вставить").is_some_and(|item| item.keyEquivalent().to_string() == "v");
        let selected = editor.as_ref().map(|editor| {
            let range = editor.selectedRange();
            (range.location, range.length)
        });
        let length = self.field_text().chars().count();
        let ok = resolved.iter().all(|(_, ok)| *ok)
            && copy_enabled
            && cut_enabled
            && paste_item
            && selected == Some((0, length));
        self.report.borrow_mut().finding(
            "edit_menu",
            verdict(ok),
            format!(
                "Cmd-A из «Правки» выделил {selected:?} при длине текста {length}; цели действий — field editor поля палитры: {resolved:?}; «Скопировать» и «Вырезать» включены: {copy_enabled}, {cut_enabled}; «Вставить» с Cmd-V: {paste_item}. Копирование и вставка через общий буфер — ручная проверка задачи 14"
            ),
        );
    }

    fn drive_cycles(&self) -> Poll<()> {
        let mut slot = self.cycles.borrow_mut();
        let Some(cycles) = slot.as_mut() else {
            return Poll::Ready(());
        };
        if cycles.error.is_some() {
            return Poll::Ready(());
        }
        if !cycles.numbers.is_empty() {
            if !returned_and_passed(&cycles.numbers) {
                return Poll::Pending(format!(
                    "цикл {} {:?}: события не обработаны",
                    cycles.done, cycles.phase
                ));
            }
            let reached = match cycles.phase {
                Phase::Open => self.opened() && self.parent_is_main(),
                Phase::Filter => {
                    self.field_text() == KEY_L.characters
                        && self.palette().results() == cycles.filtered
                }
                Phase::Close => {
                    self.closed()
                        && self.parent_is_key()
                        && self.parent_responder() == cycles.responder
                }
            };
            if !reached {
                if cycles.waiting == 0 {
                    cycles.late += 1;
                }
                cycles.waiting += 1;
                if cycles.waiting > 1_000 {
                    cycles.error = Some(format!(
                        "цикл {} {:?}: состояние не наступило; {}",
                        cycles.done,
                        cycles.phase,
                        self.state()
                    ));
                    return Poll::Ready(());
                }
                return Poll::Pending(format!(
                    "цикл {} {:?}: ждём состояние",
                    cycles.done, cycles.phase
                ));
            }
            cycles.waiting = 0;
            if cycles.done >= WARMUP && cycles.phase == Phase::Open {
                for (name, ms) in self.palette().open_phases() {
                    match cycles
                        .open_phases
                        .iter_mut()
                        .find(|(known, _)| *known == name)
                    {
                        Some((_, samples)) => samples.push(ms),
                        None => cycles.open_phases.push((name, vec![ms])),
                    }
                }
            }
            let key_index = match cycles.phase {
                Phase::Open => Key::CMD_K.modifiers().len(),
                Phase::Filter | Phase::Close => 0,
            };
            if cycles.done >= WARMUP
                && let Some(record) = cycles
                    .numbers
                    .get(key_index)
                    .and_then(|n| events::record(*n))
                && let (Some(start), Some(back)) = (record.dispatch_start, record.send_return)
            {
                let samples = &mut cycles.samples[cycles.phase as usize];
                samples
                    .post_to_dispatch
                    .push(events::to_ms(start.saturating_sub(record.posted)));
                samples
                    .dispatch_to_return
                    .push(events::to_ms(back.saturating_sub(start)));
                if let Some(pass) = events::display_pass_after(back) {
                    samples
                        .post_to_pass
                        .push(events::to_ms(pass.saturating_sub(record.posted)));
                }
            }
            cycles.numbers.clear();
            cycles.phase = match cycles.phase {
                Phase::Open => Phase::Filter,
                Phase::Filter => Phase::Close,
                Phase::Close => {
                    cycles.done += 1;
                    Phase::Open
                }
            };
        }
        if cycles.done >= WARMUP + CYCLES {
            return Poll::Ready(());
        }
        let key = match cycles.phase {
            Phase::Open => Key::CMD_K,
            Phase::Filter => KEY_L,
            Phase::Close => KEY_ESCAPE,
        };
        match events::press(key) {
            Ok(numbers) => cycles.numbers = numbers,
            Err(reason) => {
                cycles.error = Some(reason);
                return Poll::Ready(());
            }
        }
        Poll::Pending(format!("цикл {} {:?} отправлен", cycles.done, cycles.phase))
    }

    fn conclude_cycles(&self) -> Result<(), String> {
        let Some(cycles) = self.cycles.borrow_mut().take() else {
            return Err("циклы не начаты".to_owned());
        };
        if let Some(error) = cycles.error {
            return Err(format!("циклы: {error}"));
        }
        let mut report = self.report.borrow_mut();
        report.push("cycles_state_polls_after_pass", cycles.late);
        report.finding(
            "cycles",
            verdict(cycles.mismatches.is_empty()),
            format!(
                "{} циклов Cmd-K, «l» и Escape, из них {WARMUP} прогревочных: после открытия палитра key и field editor — first responder, после «l» результатов {}, после Escape фокус у прежнего вида; проверок состояния после прохода, когда оно ещё не наступило: {}",
                cycles.done, cycles.filtered, cycles.late
            ),
        );
        for (name, samples) in cycles.open_phases {
            report.metric(&format!("palette_open_phase_{name}"), "ms", samples);
        }
        for (phase, samples) in [Phase::Open, Phase::Filter, Phase::Close]
            .into_iter()
            .zip(cycles.samples)
        {
            let name = match phase {
                Phase::Open => "open",
                Phase::Filter => "filter",
                Phase::Close => "close",
            };
            let p95 = crate::stats::summarize(&samples.post_to_pass)
                .ok()
                .map(|summary| summary.p95);
            report.metric(
                &format!("palette_{name}_post_to_display_pass"),
                "ms",
                samples.post_to_pass,
            );
            report.metric(
                &format!("palette_{name}_post_to_dispatch"),
                "ms",
                samples.post_to_dispatch,
            );
            report.metric(
                &format!("palette_{name}_dispatch_to_return"),
                "ms",
                samples.dispatch_to_return,
            );
            report.finding(
                &format!("{name}_budget"),
                verdict(p95.is_some_and(|p95| p95 <= BUDGET_MS)),
                format!(
                    "p95 интервала postEvent → конец прохода отображения {} при бюджете {BUDGET_MS} мс; вывод об измеренном участке, не о времени до экрана",
                    p95.map_or_else(|| "нет".to_owned(), |p95| format!("{p95:.3} мс"))
                ),
            );
        }
        Ok(())
    }

    /// Центр строки правого списка на экране.
    fn right_row_point(&self, row: isize) -> Result<NSPoint, String> {
        let list = self.scene.tiles.tiles[RIGHT]
            .list()
            .ok_or("у правой плитки нет списка")?;
        let table = list.table();
        let frame = table.rectOfRow(row);
        let in_window = table.convertPoint_toView(events::center(frame), None);
        Ok(self.window().convertPointToScreen(in_window))
    }

    /// Строка правого списка вне палитры и поля её тени: окно-адресат щелчка — родитель.
    fn parent_click_point(&self) -> Result<NSPoint, String> {
        let point = self.right_row_point(PARENT_CLICK_ROW)?;
        let panel = self.panel().frame();
        if events::contains(panel, point) {
            return Err(format!(
                "строка {PARENT_CLICK_ROW} правого списка {} под панелью палитры {}",
                events::describe_point(point),
                events::describe_rect(panel)
            ));
        }
        Ok(point)
    }

    fn start_snapshots(self: &Rc<Self>) -> Result<(), String> {
        let render = ViewRender::of(self.palette().surface())?;
        let primary = NSScreen::screens(self.mtm())
            .firstObject()
            .map(|screen| screen.frame().size.height)
            .ok_or("нет экранов для снимка композиции")?;
        let out = &self.stand.out;
        let jobs: Vec<Background<Snapshot>> = vec![
            render.save(out, "palette-surface"),
            snapshot::capture_window(self.panel(), out, "palette-window"),
            snapshot::capture_composition(self.window(), primary, out, "palette-composition"),
        ];
        let run = Rc::clone(self);
        snapshot::wait_all(jobs, SNAPSHOT_TIMEOUT, move |result| {
            *run.snapshots.borrow_mut() = Some(result);
        });
        Ok(())
    }

    fn take_snapshots(&self) -> Result<(), String> {
        let result = self
            .snapshots
            .borrow_mut()
            .take()
            .ok_or("снимки не готовы")?;
        let snapshots = result?;
        let mut failed = Vec::new();
        let mut report = self.report.borrow_mut();
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
        if failed.is_empty() {
            Ok(())
        } else {
            Err(format!("снимки не записаны: {failed:?}"))
        }
    }
}

/// Все события вернулись из `sendEvent`, и после последнего возврата прошёл проход отображения.
fn returned_and_passed(numbers: &[u64]) -> bool {
    let returns: Option<Vec<Duration>> = numbers
        .iter()
        .map(|number| events::record(*number).and_then(|record| record.send_return))
        .collect();
    !numbers.is_empty()
        && returns
            .and_then(|returns| returns.into_iter().max())
            .is_some_and(|last| events::display_pass_after(last).is_some())
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Palette);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(scene) = stand.built_scene.clone() else {
        return stand.conclude(report, Err("сцена не собрана".to_owned()));
    };
    report.push("cell", scene.cell.name());
    report.push("palette_commands", scene.palette.model().len());
    report.push("palette_commands_fixture", PALETTE_COMMANDS);
    report.push("palette_width", palette::WIDTH);
    report.push("palette_top", palette::TOP);
    report.push("palette_shadow_margin", scene.palette.margin());
    report.push("palette_result_row", palette::RESULT_ROW);
    report.push(
        "palette_filter",
        "подстрока подписи без учёта регистра, порядок набора сохраняется; пустой запрос — все команды",
    );
    report.push(
        "interval_post_to_display_pass",
        "postEvent keyDown → конец прохода отображения после возврата sendEvent; это не момент кадра на экране",
    );
    report.push(
        "interval_dispatch_to_return",
        "начало dispatch → возврат sendEvent для keyDown",
    );
    report.push(
        "interval_post_to_dispatch",
        "postEvent → начало dispatch: ожидание в очереди событий",
    );
    report.push(
        "edit_menu_scope",
        "Cmd-A идёт через пункт «Правки»; для copy:, cut: и paste: проверяется цель действия и включённость пунктов, общий буфер обмена не используется",
    );
    report.push(
        "app_hide_scope",
        "скрытие приложения (Cmd-H) отдаёт активность другому приложению и проверяется в задаче 14; сценарий убирает окно-родителя orderOut",
    );
    report.push(
        "interval_open_phases",
        "этапы Palette::open по монотонным часам: фильтр с перезагрузкой списка, кадр панели, addChildWindow, makeKeyAndOrderFront, makeFirstResponder поля",
    );
    let root_subviews = scene.root.subviews().count();
    let log = WindowLog::new(stand.app.mtm());
    log.observe(&scene.window);
    let run = Rc::new(Run {
        settle: Settle::new(vec![scene.window.clone()]),
        stand: Rc::clone(&stand),
        scene,
        report: RefCell::new(report),
        keys: RefCell::new(Vec::new()),
        click: RefCell::new(None),
        mark: Cell::new(None),
        stable: RefCell::new(Stable::new()),
        previous: Cell::new(0),
        beneath: RefCell::new(None),
        removed: RefCell::new(None),
        marks_before: Cell::new(0),
        cycles: RefCell::new(None),
        snapshots: RefCell::new(None),
        hidden_at: Cell::new(None),
        root_subviews: Cell::new(root_subviews),
        log,
    });
    run_steps(steps(&run), move |result| finish(run, result));
}

fn step(
    run: &Rc<Run>,
    name: &'static str,
    timeout: Duration,
    ready: impl Fn(&Run) -> bool + 'static,
    act: impl FnOnce(&Rc<Run>) -> Result<(), String> + 'static,
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
            poll(run.stand.app.isActive() && run.parent_is_key(), || {
                run.state()
            })
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
                let source = input_source(run.window());
                if let Some(source) = &source {
                    run.report.borrow_mut().push("input_source", source);
                }
                require_latin(source.as_deref())?;
                run.previous.set(run.parent_responder());
                run.press(Key::CMD_K)
            },
        ),
        step(
            run,
            "Cmd-K открыл палитру",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.opened(),
            |run| {
                run.check_open("cmd-k");
                run.check_surface();
                run.check_material("open");
                run.check_results("results_initial", "");
                run.check_selection_paint("selection_paint_open");
                *run.beneath.borrow_mut() = Some(run.beneath());
                run.press(Key::DOWN)
            },
        ),
        step(
            run,
            "Down обработана в палитре",
            STEP_TIMEOUT,
            Run::keys_done,
            |run| {
                let selected = run.palette().list().selected();
                run.report.borrow_mut().finding(
                    "arrow_selects",
                    verdict(selected == Some(1) && run.field_editing()),
                    format!(
                        "Down из поля выбрал строку {selected:?} списка палитры, ожидалась 1; field editor остался first responder: {}",
                        run.field_editing()
                    ),
                );
                run.check_selection_paint("selection_paint_down");
                run.check_beneath("keys_isolated_down", "Down");
                run.press(KEY_1)
            },
        ),
        step(
            run,
            "«1» набрана в поле",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.field_text() == "1",
            |run| {
                run.check_results("filter_digit", "1");
                run.press(KEY_SHIFT_L)
            },
        ),
        step(
            run,
            "Shift-L набрана в поле",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.field_text() == "1L",
            |run| {
                run.check_results("filter_empty", "1L");
                run.check_beneath("keys_isolated", "Down, «1» и Shift-L");
                run.press(KEY_TAB)
            },
        ),
        step(
            run,
            "Tab перевёл фокус в список",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.table_is_responder(),
            |run| run.press(KEY_TAB),
        ),
        step(
            run,
            "второй Tab вернул фокус полю",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.field_editing(),
            |run| run.press(KEY_BACKTAB),
        ),
        step(
            run,
            "Shift-Tab перевёл фокус в список",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.table_is_responder(),
            |run| run.press(KEY_TAB),
        ),
        step(
            run,
            "Tab снова вернул фокус полю",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.field_editing(),
            |run| {
                let ok = run.panel_is_key() && run.palette().is_open();
                run.report.borrow_mut().finding(
                    "tab_cycle",
                    verdict(ok),
                    format!(
                        "Tab, Tab, Shift-Tab и Tab переводили фокус поле → список → поле → список → поле, key window всё время палитра: {}",
                        run.panel_is_key()
                    ),
                );
                run.check_beneath("keys_isolated_tab", "Tab и Shift-Tab");
                run.press(KEY_CMD_A)
            },
        ),
        step(
            run,
            "Cmd-A обработан",
            STEP_TIMEOUT,
            |run| run.keys_done(),
            |run| {
                run.check_edit_menu();
                run.press(KEY_ESCAPE)
            },
        ),
        step(
            run,
            "Escape закрыл палитру",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.closed(),
            |run| {
                run.check_closed(
                    "escape",
                    Close::Escape,
                    Restore::Previous,
                    run.previous.get(),
                    true,
                );
                run.press(Key::CMD_K)
            },
        ),
        step(
            run,
            "палитра открыта для Enter",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.opened(),
            |run| {
                run.check_results("results_reopen", "");
                run.marks_before.set(run.scene.commands.count(MARK_COMMAND));
                run.press(Key::RETURN)
            },
        ),
        step(
            run,
            "Enter выполнил команду",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.closed(),
            |run| {
                run.check_closed(
                    "enter",
                    Close::Perform,
                    Restore::Previous,
                    run.previous.get(),
                    true,
                );
                let log = run.scene.commands.log();
                let count = run.scene.commands.count(MARK_COMMAND);
                let ok = count == run.marks_before.get() + 1
                    && log
                        .last()
                        .is_some_and(|last| last.id == MARK_COMMAND && last.via == Via::Palette);
                run.report.borrow_mut().finding(
                    "enter_performs",
                    verdict(ok),
                    format!(
                        "Enter выполнил выбранную команду: вызовов команды {MARK_COMMAND} было {}, стало {count}; последний вызов {:?}",
                        run.marks_before.get(),
                        log.last()
                    ),
                );
                let point = run.right_row_point(RIGHT_ROW)?;
                run.click_at(point, "строка правого списка")
            },
        ),
        step(
            run,
            "щелчок по правому списку обработан",
            STEP_TIMEOUT,
            |run| {
                run.click_done()
                    && run.scene.tiles.tiles[RIGHT]
                        .list()
                        .is_some_and(|list| events::is_first_responder(run.window(), list.table()))
            },
            |run| {
                run.previous.set(run.parent_responder());
                run.press(Key::CMD_K)
            },
        ),
        step(
            run,
            "палитра открыта над правым списком",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.opened(),
            |run| {
                let list = run.scene.tiles.tiles[RIGHT]
                    .list()
                    .ok_or("у правой плитки нет списка")?;
                let view: Retained<NSView> = Retained::into_super(list.view().retain());
                view.removeFromSuperview();
                *run.removed.borrow_mut() = Some(view);
                run.press(KEY_ESCAPE)
            },
        ),
        step(
            run,
            "Escape после удаления прежнего вида",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.closed(),
            |run| {
                let fallback = address(&*run.scene.tiles.tiles[CENTER]);
                run.check_closed("fallback", Close::Escape, Restore::Fallback, fallback, true);
                let focus = run.scene.tiles.focus();
                run.report
                    .borrow_mut()
                    .push("fallback_scene_focus", format!("{focus:?}"));
                let view = run
                    .removed
                    .borrow_mut()
                    .take()
                    .ok_or("вид списка не убран")?;
                run.scene.tiles.tiles[RIGHT].body().addSubview(&view);
                run.previous.set(run.parent_responder());
                run.press(Key::CMD_K)
            },
        ),
        step(
            run,
            "палитра открыта перед щелчком в родителя",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.opened(),
            |run| {
                run.click_at(
                    run.parent_click_point()?,
                    "строка правого списка при палитре",
                )
            },
        ),
        step(
            run,
            "щелчок в родителя обработан",
            STEP_TIMEOUT,
            |run| run.click_done() && run.closed(),
            |run| {
                let last = run.palette().closes().last().copied();
                let to_click = run.scene.tiles.tiles[RIGHT].list().is_some_and(|list| {
                    events::is_first_responder(run.window(), list.table())
                        && list.selected() == usize::try_from(PARENT_CLICK_ROW).ok()
                });
                let ok = run.closed()
                    && last.is_some_and(|(reason, _)| reason == Close::ResignKey)
                    && run.parent_is_key()
                    && to_click;
                run.report.borrow_mut().finding(
                    "parent_click",
                    verdict(ok),
                    format!(
                        "щелчок по строке правого списка вне палитры закрыл её: {}, причина и возврат фокуса {last:?}, key — основное окно: {}; first responder после щелчка {}, это список, по которому щёлкнули, со строкой {PARENT_CLICK_ROW}: {to_click}",
                        run.closed(),
                        run.parent_is_key(),
                        events::responder_name(run.window())
                    ),
                );
                let palette = run.palette();
                let folded: Vec<String> = palette
                    .model()
                    .iter()
                    .map(|command| palette::fold(&command.title))
                    .collect();
                let filtered =
                    palette::filter(folded.iter().map(String::as_str), KEY_L.characters).len();
                *run.cycles.borrow_mut() = Some(Cycles::new(run.parent_responder(), filtered));
                Ok(())
            },
        ),
    ];
    let cycles_driver = {
        let run = Rc::clone(run);
        move || run.drive_cycles()
    };
    let cycles_done = {
        let run = Rc::clone(run);
        move || {
            run.conclude_cycles()?;
            run.window().toggleFullScreen(None);
            run.touch();
            Ok(())
        }
    };
    steps.push(Step::new(
        "110 циклов открытия, фильтра и закрытия",
        CYCLES_TIMEOUT,
        cycles_driver,
        cycles_done,
    ));
    steps.extend([
        step(
            run,
            "родитель в полноэкранном режиме",
            FULLSCREEN_TIMEOUT,
            |run| run.log.fullscreen_notes().0 > 0 && run.fullscreen() && run.settled(),
            |run| {
                run.previous.set(run.parent_responder());
                run.press(Key::CMD_K)
            },
        ),
        step(
            run,
            "палитра открыта в полноэкранном режиме",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.opened(),
            |run| {
                run.check_open("fullscreen");
                let screens = (run.panel().screen(), run.window().screen());
                let same_screen = match &screens {
                    (Some(panel), Some(parent)) => {
                        panel.CGDirectDisplayID() == parent.CGDirectDisplayID()
                    }
                    _ => false,
                };
                let ok = run.panel().isOnActiveSpace() && same_screen;
                run.report.borrow_mut().finding(
                    "fullscreen_space",
                    verdict(ok),
                    format!(
                        "палитра на активном пространстве полноэкранного родителя: {}, на его экране: {same_screen}; collectionBehavior панели {:?}",
                        run.panel().isOnActiveSpace(),
                        run.panel().collectionBehavior()
                    ),
                );
                run.check_material("fullscreen");
                run.press(KEY_ESCAPE)
            },
        ),
        step(
            run,
            "Escape в полноэкранном режиме",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.closed(),
            |run| {
                run.check_closed(
                    "fullscreen_close",
                    Close::Escape,
                    Restore::Previous,
                    run.previous.get(),
                    true,
                );
                run.window().toggleFullScreen(None);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "родитель вышел из полноэкранного режима",
            FULLSCREEN_TIMEOUT,
            |run| run.log.fullscreen_notes().1 > 0 && !run.fullscreen() && run.settled(),
            |run| run.press(Key::CMD_K),
        ),
        step(
            run,
            "палитра открыта для снимков",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.opened(),
            |run| run.start_snapshots(),
        ),
        step(
            run,
            "снимки записаны",
            SNAPSHOT_TIMEOUT,
            |run| run.snapshots.borrow().is_some(),
            |run| {
                run.take_snapshots()?;
                run.hidden_at.set(Some(Instant::now()));
                run.window().orderOut(None);
                Ok(())
            },
        ),
        step(
            run,
            "родитель убран",
            STEP_TIMEOUT,
            |run| {
                !run.window().isVisible()
                    && (run.closed()
                        || run
                            .hidden_at
                            .get()
                            .is_some_and(|at| at.elapsed() >= HIDE_GRACE))
            },
            |run| {
                let last = run.palette().closes().last().copied();
                let closed = run.closed();
                run.report.borrow_mut().push(
                    "parent_hidden_state",
                    format!("палитра закрыта: {closed}, причина {last:?}; {}", run.state()),
                );
                run.window().makeKeyAndOrderFront(None);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "родитель снова показан",
            STEP_TIMEOUT,
            |run| run.window().isVisible() && run.parent_is_key() && run.settled(),
            |run| {
                let last = run.palette().closes().last().copied();
                let ok = run.closed()
                    && last.is_some_and(|(reason, _)| {
                        matches!(reason, Close::ParentHidden | Close::ResignKey)
                    });
                run.report.borrow_mut().finding(
                    "parent_hidden",
                    verdict(ok),
                    format!(
                        "после orderOut родителя палитра закрыта, причина {last:?}, и не вернулась вместе с ним: видима {}, дочерняя {}",
                        run.panel().isVisible(),
                        run.panel_attached()
                    ),
                );
                run.press(Key::CMD_K)
            },
        ),
        step(
            run,
            "палитра открыта перед закрытием родителя",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.opened(),
            |run| {
                run.window().close();
                Ok(())
            },
        ),
        step(
            run,
            "родитель закрыт",
            STEP_TIMEOUT,
            |run| !run.window().isVisible(),
            |run| {
                let last = run.palette().closes().last().copied();
                let visible: Vec<String> = run
                    .stand
                    .app
                    .windows()
                    .iter()
                    .filter(|window| window.isVisible())
                    .map(|window| events::describe_window(&window))
                    .collect();
                let ok = run.closed()
                    && last.is_some_and(|(reason, _)| reason == Close::ParentClosed)
                    && visible.is_empty();
                run.report.borrow_mut().finding(
                    "parent_close",
                    verdict(ok),
                    format!(
                        "закрытие родителя закрыло палитру ({last:?}), она не осталась самостоятельным окном: видимые окна {visible:?}"
                    ),
                );
                run.window().makeKeyAndOrderFront(None);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "родитель открыт снова",
            STEP_TIMEOUT,
            |run| run.window().isVisible() && run.parent_is_key() && run.settled(),
            |run| {
                run.report.borrow_mut().push(
                    "after_reopen",
                    format!("палитра видима: {}, дочерняя: {}", run.panel().isVisible(), run.panel_attached()),
                );
                run.press(Key::CMD_K)
            },
        ),
        step(
            run,
            "палитра открыта перед сворачиванием",
            STEP_TIMEOUT,
            |run| run.keys_done() && run.opened(),
            |run| {
                run.window().miniaturize(None);
                Ok(())
            },
        ),
        step(
            run,
            "родитель свёрнут",
            STEP_TIMEOUT,
            |run| run.window().isMiniaturized(),
            |run| {
                let last = run.palette().closes().last().copied();
                run.report.borrow_mut().push(
                    "miniaturized_state",
                    format!("палитра закрыта: {}, причина {last:?}", run.closed()),
                );
                run.window().deminiaturize(None);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "родитель развёрнут",
            STEP_TIMEOUT,
            |run| !run.window().isMiniaturized() && run.window().isVisible() && run.settled(),
            |run| {
                let last = run.palette().closes().last().copied();
                let ok = run.closed()
                    && last.is_some_and(|(reason, _)| reason == Close::ParentMiniaturized);
                run.report.borrow_mut().finding(
                    "parent_miniaturize",
                    verdict(ok),
                    format!(
                        "сворачивание родителя закрыло палитру ({last:?}); после разворота она видима: {}, дочерняя: {}; key — основное окно: {}",
                        run.panel().isVisible(),
                        run.panel_attached(),
                        run.parent_is_key()
                    ),
                );
                Ok(())
            },
        ),
    ]);
    steps
}

fn finish(run: Rc<Run>, result: Result<(), StepFailure>) {
    let stand = Rc::clone(&run.stand);
    run.log.stop();
    let mut report = run.report.replace(Report::default());
    run.settle.add_to(&mut report);
    report.push("palette_opened", run.palette().opened());
    report.push("palette_closes", format!("{:?}", run.palette().closes()));
    report.push("commands_log", format!("{:?}", run.scene.commands.log()));
    if let Some(view) = run.removed.borrow_mut().take() {
        run.scene.tiles.tiles[RIGHT].body().addSubview(&view);
    }
    let result = result.map_err(|failure| failure.describe());
    drop(run);
    stand.conclude(report, result);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_queries_have_expected_results() {
        let commands = crate::fixtures::commands(PALETTE_COMMANDS);
        let folded: Vec<String> = commands.iter().map(|c| palette::fold(&c.title)).collect();
        let count = |query: &str| palette::filter(folded.iter().map(String::as_str), query).len();
        // «l» сужает набор, но не опустошает его; «1L» ничего не находит, и видна надпись.
        assert!(count(KEY_L.characters) > 0 && count(KEY_L.characters) < PALETTE_COMMANDS);
        assert!(count("1") > 0);
        assert_eq!(count("1L"), 0);
        assert_eq!(KEY_SHIFT_L.characters, "L");
    }
}
