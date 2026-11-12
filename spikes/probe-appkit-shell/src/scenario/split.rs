//! Сценарий `split`, по процессу на вариант сплита. Материал во всех окнах как разница с пустым
//! окном той же конфигурации; окно с key-статусом и без него; фокус плиток и навигатора;
//! щелчок по полю поиска; свёртка и разворот левой и правой зон; ресайз больше минимума и
//! обратно; смена темы; полноэкранный режим по уведомлениям окна; геометрия заголовка, плиток и
//! зазоров; кадры кнопок окна в полосе заголовка; пиксельная проба вуали.
//!
//! Каждое ожидание — состояние, которое наступает при любом ответе на гипотезу: свёрнутость
//! элемента, уведомление окна, неподвижные кадры после прохода отображения. Неверная геометрия
//! или видимый материал — отрицательный вывод, а не ошибка исполнения.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send, sel};
use objc2_app_kit::{
    NSView, NSWindow, NSWindowButton, NSWindowDidEnterFullScreenNotification,
    NSWindowDidExitFullScreenNotification, NSWindowStyleMask, NSWindowWillMoveNotification,
};
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
};
use serde::Serialize;

use super::counters::{Live, counter};
use super::events;
use super::material::{self, Found, Outcome, WindowScan};
use super::readback::{self, Rgba};
use super::snapshot::{self, Background, ViewRender};
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepFailure, Verdict, poll,
    run_steps,
};
use crate::chrome::{self, BUTTON_CENTER_FROM_TOP, SceneRoot};
use crate::results::{Rect, Size, Snapshot, SnapshotStatus};
use crate::scene::Scene;
use crate::split::{LEFT_THICKNESS, RIGHT_THICKNESS};
use crate::tiles::{BOTTOM, BOTTOM_HEIGHT, CENTER, Focus, RIGHT};
use crate::tokens::{self, InputMode, Tokens};

counter!(WINDOW_LOG, "ProbeWindowLog");

const STEP_TIMEOUT: Duration = Duration::from_secs(3);
const LAYOUT_TIMEOUT: Duration = Duration::from_secs(5);
const FULLSCREEN_TIMEOUT: Duration = Duration::from_secs(12);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);
/// Кадры считаются установившимися, если не менялись столько времени. Свёртку левой зоны в
/// варианте `plain` AppKit применяет через 418–424 мс.
const QUIET: Duration = Duration::from_millis(500);
const GEOMETRY_TOLERANCE: f64 = 0.5;
const PIXEL_TOLERANCE: u8 = 1;
/// Допуск центра кнопки окна от середины полосы; это наблюдение, вердикт — полоса.
const CENTER_TOLERANCE: f64 = 1.0;
const GROW: NSSize = NSSize::new(200.0, 100.0);
/// Возврат `sendEvent` для нажатия по полю поиска; перетаскивание окна держало бы его дольше.
const PROMPT_RETURN_MS: f64 = 100.0;

pub struct LogIvars {
    _live: Live,
    will_move: Cell<usize>,
    entered: Cell<usize>,
    exited: Cell<usize>,
}

define_class!(
    /// Счётчик уведомлений основного окна.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeWindowLog"]
    #[ivars = LogIvars]
    struct WindowLog;

    unsafe impl NSObjectProtocol for WindowLog {}

    impl WindowLog {
        #[unsafe(method(windowNote:))]
        fn window_note(&self, notification: &NSNotification) {
            let name = notification.name();
            let ivars = self.ivars();
            // SAFETY: имена уведомлений — неизменяемые константы AppKit.
            let (will_move, entered, exited) = unsafe {
                (
                    NSWindowWillMoveNotification,
                    NSWindowDidEnterFullScreenNotification,
                    NSWindowDidExitFullScreenNotification,
                )
            };
            let counter = if &*name == will_move {
                &ivars.will_move
            } else if &*name == entered {
                &ivars.entered
            } else if &*name == exited {
                &ivars.exited
            } else {
                return;
            };
            counter.set(counter.get() + 1);
            events::note(format!("уведомление окна {name}"));
        }
    }
);

impl WindowLog {
    fn new(mtm: MainThreadMarker) -> Retained<WindowLog> {
        let this = WindowLog::alloc(mtm).set_ivars(LogIvars {
            _live: WINDOW_LOG.live(),
            will_move: Cell::new(0),
            entered: Cell::new(0),
            exited: Cell::new(0),
        });
        // SAFETY: `init` у NSObject без аргументов.
        unsafe { msg_send![super(this), init] }
    }

    fn observe(&self, window: &NSWindow) {
        let center = NSNotificationCenter::defaultCenter();
        let window: &AnyObject = window;
        // SAFETY: имена уведомлений — неизменяемые константы AppKit.
        let names = unsafe {
            [
                NSWindowWillMoveNotification,
                NSWindowDidEnterFullScreenNotification,
                NSWindowDidExitFullScreenNotification,
            ]
        };
        for name in names {
            // SAFETY: `windowNote:` реализован этим классом; подписка снимается в `stop`.
            unsafe {
                center.addObserver_selector_name_object(
                    self,
                    sel!(windowNote:),
                    Some(name),
                    Some(window),
                )
            };
        }
    }

    fn stop(&self) {
        // SAFETY: снимаются все подписки этого наблюдателя.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(self) };
    }
}

/// Ожидаемые кадры зон в координатах окна (начало внизу слева).
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Zones {
    pub chrome: Rect,
    pub navigator: Option<Rect>,
    pub center: Rect,
    pub bottom: Rect,
    pub right: Option<Rect>,
}

/// Раскладка по разделу Technical Details: заголовок через всё окно, плоская левая зона из
/// навигатора и зазора, центр и нижняя плитка, правая зона, зазор `--gap` между плитками и у
/// краёв окна.
pub fn expected(
    width: f64,
    height: f64,
    left_open: bool,
    right_open: bool,
    tokens: &Tokens,
) -> Zones {
    let (bar, gap) = (tokens.titlebar, tokens.gap);
    let body = height - bar;
    let x0 = if left_open { LEFT_THICKNESS + gap } else { gap };
    let x1 = if right_open {
        width - RIGHT_THICKNESS - gap
    } else {
        width - gap
    };
    let rect = |x: f64, y: f64, width: f64, height: f64| Rect {
        x,
        y,
        width,
        height,
    };
    Zones {
        chrome: rect(0.0, height - bar, width, bar),
        navigator: left_open.then(|| rect(0.0, 0.0, LEFT_THICKNESS, body)),
        bottom: rect(x0, gap, x1 - x0, BOTTOM_HEIGHT),
        center: rect(
            x0,
            gap + BOTTOM_HEIGHT + gap,
            x1 - x0,
            body - 3.0 * gap - BOTTOM_HEIGHT,
        ),
        right: right_open.then(|| {
            rect(
                width - RIGHT_THICKNESS,
                gap,
                RIGHT_THICKNESS - gap,
                body - 2.0 * gap,
            )
        }),
    }
}

/// Прочитанные у AppKit кадры зон.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Actual {
    pub content: Size,
    pub chrome: Rect,
    pub navigator: Rect,
    pub navigator_hidden: bool,
    pub center: Rect,
    pub bottom: Rect,
    pub right: Rect,
    pub right_hidden: bool,
    pub left_collapsed: bool,
    pub right_collapsed: bool,
}

fn near(a: Rect, b: Rect) -> bool {
    [a.x - b.x, a.y - b.y, a.width - b.width, a.height - b.height]
        .iter()
        .all(|delta| delta.abs() <= GEOMETRY_TOLERANCE)
}

fn describe(rect: Rect) -> String {
    format!("{},{} {}x{}", rect.x, rect.y, rect.width, rect.height)
}

/// Расхождения прочитанной геометрии с ожидаемой.
pub fn compare(expected: &Zones, actual: &Actual) -> Vec<String> {
    let mut mismatches = Vec::new();
    let check = |mismatches: &mut Vec<String>, name: &str, want: Rect, got: Rect| {
        if !near(want, got) {
            mismatches.push(format!(
                "{name}: ожидался {}, прочитан {}",
                describe(want),
                describe(got)
            ));
        }
    };
    check(&mut mismatches, "заголовок", expected.chrome, actual.chrome);
    check(&mut mismatches, "центр", expected.center, actual.center);
    check(&mut mismatches, "низ", expected.bottom, actual.bottom);
    match expected.navigator {
        Some(want) if !actual.navigator_hidden => {
            check(&mut mismatches, "навигатор", want, actual.navigator)
        }
        Some(_) => mismatches.push("навигатор скрыт, а зона открыта".to_owned()),
        None if actual.navigator_hidden || actual.navigator.width <= GEOMETRY_TOLERANCE => {}
        None => mismatches.push(format!(
            "навигатор виден при свёрнутой зоне: {}",
            describe(actual.navigator)
        )),
    }
    match expected.right {
        Some(want) if !actual.right_hidden => {
            check(&mut mismatches, "правая плитка", want, actual.right)
        }
        Some(_) => mismatches.push("правая плитка скрыта, а зона открыта".to_owned()),
        None if actual.right_hidden || actual.right.width <= GEOMETRY_TOLERANCE => {}
        None => mismatches.push(format!(
            "правая плитка видна при свёрнутой зоне: {}",
            describe(actual.right)
        )),
    }
    mismatches
}

/// Кадры, которые должны перестать меняться, прежде чем их читать.
pub(super) struct Stable {
    last: Vec<Rect>,
    since: Instant,
    started: Instant,
    /// Изменения кадров после первого чтения и время последнего от начала ожидания.
    pub(super) changes: usize,
    pub(super) last_change: Option<Duration>,
}

impl Stable {
    pub(super) fn new() -> Stable {
        let now = Instant::now();
        Stable {
            last: Vec::new(),
            since: now,
            started: now,
            changes: 0,
            last_change: None,
        }
    }

    pub(super) fn quiet(&mut self, frames: Vec<Rect>) -> bool {
        if frames != self.last {
            if !self.last.is_empty() {
                self.changes += 1;
                self.last_change = Some(self.started.elapsed());
            }
            self.last = frames;
            self.since = Instant::now();
            return false;
        }
        self.since.elapsed() >= QUIET
    }
}

#[derive(Clone, Debug, Serialize)]
struct ButtonState {
    state: String,
    content_height: f64,
    buttons: Vec<(String, Option<Rect>)>,
    in_band: bool,
    centered: bool,
}

struct Run {
    stand: Rc<Stand>,
    scene: Rc<Scene>,
    settle: Rc<Settle>,
    log: Retained<WindowLog>,
    baseline: RefCell<Option<Retained<NSWindow>>>,
    stable: RefCell<Stable>,
    /// Время последнего изменения сцены: ждём проход отображения после него.
    mark: Cell<Option<Duration>>,
    click: RefCell<Option<(u64, u64)>>,
    initial_frame: Cell<NSRect>,
    grow_target: Cell<Option<NSRect>>,
    dark_before: Cell<bool>,
    search_before: Cell<(usize, usize, NSRect)>,
    report: RefCell<Report>,
    scans: RefCell<Vec<(String, Vec<WindowScan>)>>,
    baseline_scans: RefCell<Vec<WindowScan>>,
    buttons: RefCell<Vec<ButtonState>>,
    pixels: RefCell<Vec<(String, bool)>>,
    geometry: RefCell<Vec<(String, bool)>>,
}

impl Run {
    fn mtm(&self) -> MainThreadMarker {
        self.stand.app.mtm()
    }

    fn window(&self) -> &NSWindow {
        &self.scene.window
    }

    fn state(&self) -> String {
        format!(
            "frame={} left_collapsed={} right_collapsed={} fullscreen={} focus={:?} {}",
            events::describe_rect(self.window().frame()),
            self.scene.split.left.isCollapsed(),
            self.scene.split.right.isCollapsed(),
            self.fullscreen(),
            self.scene.tiles.focus(),
            events::focus_state(&self.stand.app)
        )
    }

    fn fullscreen(&self) -> bool {
        self.window()
            .styleMask()
            .contains(NSWindowStyleMask::FullScreen)
    }

    fn touch(&self) {
        self.mark.set(events::elapsed());
        *self.stable.borrow_mut() = Stable::new();
    }

    fn passed(&self) -> bool {
        self.mark
            .get()
            .is_some_and(|mark| events::display_pass_after(mark).is_some())
    }

    fn watched_frames(&self) -> Vec<Rect> {
        let tiles = &self.scene.tiles;
        let mut frames = vec![
            readback::rect(self.window().frame()),
            readback::frame_in_window(&tiles.navigator),
            readback::frame_in_window(&tiles.center_host),
            readback::frame_in_window(&tiles.right_host),
        ];
        frames.extend(
            tiles
                .tiles
                .iter()
                .map(|tile| readback::frame_in_window(tile)),
        );
        frames
    }

    /// Проход отображения после изменения и неподвижные кадры.
    fn settled(&self) -> bool {
        let frames = self.watched_frames();
        self.passed() && self.stable.borrow_mut().quiet(frames)
    }

    fn content_size(&self) -> NSSize {
        self.window()
            .contentView()
            .map_or(NSSize::new(0.0, 0.0), |content| content.bounds().size)
    }

    fn actual(&self) -> Actual {
        let tiles = &self.scene.tiles;
        let size = self.content_size();
        Actual {
            content: readback::size(size),
            chrome: readback::frame_in_window(&self.scene.chrome),
            navigator: readback::frame_in_window(&tiles.navigator),
            navigator_hidden: tiles.navigator.isHiddenOrHasHiddenAncestor(),
            center: readback::frame_in_window(&tiles.tiles[CENTER]),
            bottom: readback::frame_in_window(&tiles.tiles[BOTTOM]),
            right: readback::frame_in_window(&tiles.tiles[RIGHT]),
            right_hidden: tiles.tiles[RIGHT].isHiddenOrHasHiddenAncestor(),
            left_collapsed: self.scene.split.left.isCollapsed(),
            right_collapsed: self.scene.split.right.isCollapsed(),
        }
    }

    fn check_geometry(&self, state: &str) {
        let actual = self.actual();
        let zones = expected(
            actual.content.width,
            actual.content.height,
            !actual.left_collapsed,
            !actual.right_collapsed,
            &self.scene.tokens,
        );
        let mismatches = compare(&zones, &actual);
        let tiles = &self.scene.tiles;
        let stable = self.stable.borrow();
        self.report.borrow_mut().push(
            &format!("settle_{state}"),
            format!(
                "изменений кадров после действия {}, последнее через {}",
                stable.changes,
                stable.last_change.map_or_else(
                    || "—".to_owned(),
                    |at| format!("{:.1} мс", at.as_secs_f64() * 1000.0)
                )
            ),
        );
        drop(stable);
        self.report.borrow_mut().push_json(
            &format!("hosts_{state}"),
            &[
                readback::frame_in_window(&tiles.center_host),
                readback::frame_in_window(&tiles.right_host),
            ],
        );
        let ok = mismatches.is_empty();
        let mut report = self.report.borrow_mut();
        report.push_json(&format!("geometry_{state}"), &actual);
        report.finding(
            &format!("geometry_{state}"),
            verdict(ok),
            if ok {
                format!(
                    "содержимое {}x{}: заголовок, зоны и зазоры совпали с ожидаемыми в пределах {GEOMETRY_TOLERANCE} точки",
                    actual.content.width, actual.content.height
                )
            } else {
                format!(
                    "содержимое {}x{}: {}",
                    actual.content.width,
                    actual.content.height,
                    mismatches.join("; ")
                )
            },
        );
        self.geometry.borrow_mut().push((state.to_owned(), ok));
    }

    fn check_buttons(&self, state: &str) {
        let height = self.content_size().height;
        let window = self.window();
        let buttons: Vec<(String, Option<Rect>)> = [
            ("close", NSWindowButton::CloseButton),
            ("miniaturize", NSWindowButton::MiniaturizeButton),
            ("zoom", NSWindowButton::ZoomButton),
        ]
        .into_iter()
        .map(|(name, kind)| {
            let frame = window.standardWindowButton(kind).and_then(|button| {
                let own = button
                    .window()
                    .is_some_and(|own| std::ptr::eq(&*own, window));
                own.then(|| readback::rect(button.convertRect_toView(button.bounds(), None)))
            });
            (name.to_owned(), frame)
        })
        .collect();
        let band_bottom = height - self.scene.tokens.titlebar;
        let in_band = buttons.iter().all(|(_, frame)| {
            frame.is_some_and(|frame| {
                frame.y >= band_bottom - GEOMETRY_TOLERANCE
                    && frame.y + frame.height <= height + GEOMETRY_TOLERANCE
            })
        });
        let centered = buttons.iter().all(|(_, frame)| {
            frame.is_some_and(|frame| {
                (frame.y + frame.height / 2.0 - (height - BUTTON_CENTER_FROM_TOP)).abs()
                    <= CENTER_TOLERANCE
            })
        });
        self.buttons.borrow_mut().push(ButtonState {
            state: state.to_owned(),
            content_height: height,
            buttons,
            in_band,
            centered,
        });
    }

    fn scan(&self, state: &str) {
        let scans = material::scan_app(&self.stand.app);
        self.scans.borrow_mut().push((state.to_owned(), scans));
    }

    fn scan_baseline(&self) {
        if let Some(baseline) = self.baseline.borrow().as_ref() {
            self.baseline_scans
                .borrow_mut()
                .push(material::scan_window(baseline));
        }
    }

    /// Точка в центре вида на экране.
    fn screen_center(&self, view: &NSView) -> NSPoint {
        let in_window = view.convertPoint_toView(events::center(view.bounds()), None);
        self.window().convertPointToScreen(in_window)
    }

    /// Под центром вида на экране видно основное окно.
    fn on_top(&self, view: &NSView) -> bool {
        NSWindow::windowNumberAtPoint_belowWindowWithWindowNumber(
            self.screen_center(view),
            0,
            self.mtm(),
        ) == self.window().windowNumber()
    }

    fn click(&self, view: &NSView, label: &str) -> Result<(), String> {
        let point = self.screen_center(view);
        let gesture = events::mouse_down(self.mtm(), point, self.window(), label)?;
        let up = gesture.up(label)?;
        *self.click.borrow_mut() = Some((gesture.down, up));
        Ok(())
    }

    /// Нажатие вернулось из `sendEvent`, и после этого прошёл проход отображения: очередь к
    /// этому моменту пуста, отпускание тоже обработано, даже если его забрал вложенный цикл.
    fn click_done(&self) -> bool {
        self.click
            .borrow()
            .and_then(|(down, _)| events::record(down))
            .and_then(|record| record.send_return)
            .is_some_and(|back| events::display_pass_after(back).is_some())
    }

    /// First responder — сам вид или его потомок, например таблица списка плитки.
    fn is_responder(&self, view: &NSView) -> bool {
        events::is_first_responder(self.window(), view)
            || self
                .window()
                .firstResponder()
                .and_then(|responder| responder.downcast::<NSView>().ok())
                .is_some_and(|responder| responder.isDescendantOf(view))
    }

    /// Цвет слоя вуали плитки.
    fn veil_color(&self, index: usize) -> Option<Rgba> {
        self.scene.tiles.tiles[index]
            .veil()
            .layer()
            .and_then(|layer| readback::layer_state(&layer).background)
    }
}

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

fn tint_rgba(tint: crate::tokens::Tint) -> Rgba {
    let (r, g, b) = tint.color.unit();
    Rgba::from_unit(r, g, b, tint.alpha)
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Split);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(scene) = stand.built_scene.clone() else {
        return stand.conclude(report, Err("сцена не собрана".to_owned()));
    };
    report.push("cell", scene.cell.name());
    report.push_json("tokens", &scene.tokens.entries());
    report.push_json(
        "tokens_input_mode",
        &tokens::tokens(scene.cell, InputMode::Input).entries(),
    );
    report.push_json("split_items", &scene.split.describe());
    let log = WindowLog::new(stand.app.mtm());
    log.observe(&scene.window);
    let run = Rc::new(Run {
        settle: Settle::new(vec![scene.window.clone()]),
        initial_frame: Cell::new(scene.window.frame()),
        stand: Rc::clone(&stand),
        scene,
        log,
        baseline: RefCell::new(None),
        stable: RefCell::new(Stable::new()),
        mark: Cell::new(None),
        click: RefCell::new(None),
        grow_target: Cell::new(None),
        dark_before: Cell::new(false),
        search_before: Cell::new((0, 0, NSRect::ZERO)),
        report: RefCell::new(report),
        scans: RefCell::new(Vec::new()),
        baseline_scans: RefCell::new(Vec::new()),
        buttons: RefCell::new(Vec::new()),
        pixels: RefCell::new(Vec::new()),
        geometry: RefCell::new(Vec::new()),
    });
    run_steps(steps(&run), move |result| finish(run, result));
}

/// Шаг с предикатом и действием над общим состоянием прогона.
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
                app.isActive()
                    && key.is_some_and(|key| events::same(&*key, run.window()))
                    && run.is_responder(&run.scene.tiles.tiles[CENTER]),
                || run.state(),
            )
        }
    };
    let warp = {
        let run = Rc::clone(run);
        move || {
            run.initial_frame.set(run.window().frame());
            events::warp_cursor_outside(run.mtm(), &run.stand.app).map(|_| ())
        }
    };
    vec![
        Step::new(
            "окно неподвижно, активность, key window и фокус на центральной плитке",
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
                initial_checks(run)?;
                run.check_geometry("initial");
                run.check_buttons("initial");
                run.scan("key");
                probe_pixels(run, "center-focused")?;
                let mtm = run.mtm();
                let tokens = &run.scene.tokens;
                let baseline = chrome::window(mtm, &run.stand.display, tokens);
                let root =
                    SceneRoot::new(mtm, NSRect::new(NSPoint::ZERO, crate::WINDOW_SIZE), tokens);
                baseline.setContentView(Some(&root));
                baseline.setTitle(objc2_foundation::ns_string!(
                    "probe-appkit-shell: пустое окно"
                ));
                baseline.orderFront(None);
                *run.baseline.borrow_mut() = Some(baseline);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "пустое окно показано, основное окно key",
            STEP_TIMEOUT,
            |run| {
                run.passed()
                    && run.window().isKeyWindow()
                    && run
                        .baseline
                        .borrow()
                        .as_ref()
                        .is_some_and(|b| b.isVisible())
            },
            |run| {
                run.scan_baseline();
                if let Some(baseline) = run.baseline.borrow().as_ref() {
                    baseline.makeKeyAndOrderFront(None);
                }
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "пустое окно key, основное без key-статуса",
            STEP_TIMEOUT,
            |run| {
                run.passed()
                    && !run.window().isKeyWindow()
                    && run
                        .baseline
                        .borrow()
                        .as_ref()
                        .is_some_and(|b| b.isKeyWindow())
            },
            |run| {
                run.scan_baseline();
                run.scan("non-key");
                run.check_geometry("non-key");
                run.check_buttons("non-key");
                if let Some(baseline) = run.baseline.borrow().as_ref() {
                    baseline.close();
                }
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "пустое окно закрыто, основное снова key",
            STEP_TIMEOUT,
            |run| {
                run.passed()
                    && run.window().isKeyWindow()
                    && run
                        .baseline
                        .borrow()
                        .as_ref()
                        .is_some_and(|b| !b.isVisible())
                    // WindowServer убирает закрытое окно позже, чем AppKit снимает isVisible.
                    && run.on_top(&run.scene.tiles.navigator)
            },
            |run| {
                run.report.borrow_mut().finding(
                    "key_status",
                    Verdict::Pass,
                    "основное окно было key, потеряло key-статус при key пустом окне и получило его обратно от AppKit после закрытия пустого окна; стенд фокус не исправлял",
                );
                run.click(&run.scene.tiles.navigator, "навигатор")
            },
        ),
        step(
            run,
            "щелчок по навигатору обработан",
            STEP_TIMEOUT,
            |run| run.click_done(),
            |run| {
                check_navigator_focus(run);
                probe_pixels(run, "navigator-focused")?;
                let body = run.scene.tiles.tiles[RIGHT].body().retain();
                run.click(&body, "правая плитка")
            },
        ),
        step(
            run,
            "щелчок по правой плитке сквозь вуаль обработан",
            STEP_TIMEOUT,
            |run| run.click_done(),
            |run| {
                check_tile_focus(run, RIGHT, "right");
                probe_pixels(run, "right-focused")?;
                let body = run.scene.tiles.tiles[CENTER].body().retain();
                run.click(&body, "центральная плитка")
            },
        ),
        step(
            run,
            "щелчок по центральной плитке обработан",
            STEP_TIMEOUT,
            |run| run.click_done(),
            |run| {
                check_tile_focus(run, CENTER, "center");
                let search = run
                    .scene
                    .chrome
                    .parts()
                    .ok_or("у заголовка нет частей")?
                    .search
                    .retain();
                run.search_before.set((
                    search.presses(),
                    run.log.ivars().will_move.get(),
                    run.window().frame(),
                ));
                run.click(&search, "поле поиска")
            },
        ),
        step(
            run,
            "щелчок по полю поиска обработан",
            STEP_TIMEOUT,
            |run| run.click_done(),
            |run| {
                check_search(run);
                press(run, |parts| &parts.left_zone)
            },
        ),
        step(
            run,
            "левая зона свёрнута, кадры неподвижны",
            LAYOUT_TIMEOUT,
            |run| run.scene.split.left.isCollapsed() && run.settled(),
            |run| {
                run.check_geometry("left-collapsed");
                run.scan("left-collapsed");
                press(run, |parts| &parts.left_zone)
            },
        ),
        step(
            run,
            "левая зона развёрнута, кадры неподвижны",
            LAYOUT_TIMEOUT,
            |run| !run.scene.split.left.isCollapsed() && run.settled(),
            |run| {
                run.check_geometry("left-restored");
                press(run, |parts| &parts.right_zone)
            },
        ),
        step(
            run,
            "правая зона свёрнута, кадры неподвижны",
            LAYOUT_TIMEOUT,
            |run| run.scene.split.right.isCollapsed() && run.settled(),
            |run| {
                run.check_geometry("right-collapsed");
                run.scan("right-collapsed");
                press(run, |parts| &parts.right_zone)
            },
        ),
        step(
            run,
            "правая зона развёрнута, кадры неподвижны",
            LAYOUT_TIMEOUT,
            |run| !run.scene.split.right.isCollapsed() && run.settled(),
            |run| {
                run.check_geometry("right-restored");
                grow(run)
            },
        ),
        step(
            run,
            "окно увеличено, кадры неподвижны",
            LAYOUT_TIMEOUT,
            |run| run.settled(),
            |run| {
                check_resize(run, "large", run.grow_target.get());
                run.check_geometry("large");
                run.check_buttons("large");
                run.window().setFrame_display(run.initial_frame.get(), true);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "окно возвращено к минимуму, кадры неподвижны",
            LAYOUT_TIMEOUT,
            |run| run.settled(),
            |run| {
                check_resize(run, "min", Some(run.initial_frame.get()));
                run.check_geometry("min");
                run.check_buttons("min");
                run.dark_before
                    .set(chrome::is_dark(&run.stand.app.effectiveAppearance()));
                press(run, |parts| &parts.theme)
            },
        ),
        step(
            run,
            "тема сменилась",
            STEP_TIMEOUT,
            |run| {
                chrome::is_dark(&run.stand.app.effectiveAppearance()) != run.dark_before.get()
                    && run.settled()
            },
            |run| {
                run.check_buttons("theme-toggled");
                press(run, |parts| &parts.theme)
            },
        ),
        step(
            run,
            "тема вернулась",
            STEP_TIMEOUT,
            |run| {
                chrome::is_dark(&run.stand.app.effectiveAppearance()) == run.dark_before.get()
                    && run.settled()
            },
            |run| {
                run.check_buttons("theme-restored");
                run.report.borrow_mut().finding(
                    "theme_toggle",
                    Verdict::Pass,
                    format!(
                        "переключатель темы сменил NSApp.appearance на {} и вернул {}",
                        if run.dark_before.get() {
                            "светлую"
                        } else {
                            "тёмную"
                        },
                        if run.dark_before.get() {
                            "тёмную"
                        } else {
                            "светлую"
                        }
                    ),
                );
                run.window().toggleFullScreen(None);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "окно вошло в полноэкранный режим",
            FULLSCREEN_TIMEOUT,
            |run| run.log.ivars().entered.get() > 0 && run.fullscreen() && run.settled(),
            |run| {
                run.check_geometry("fullscreen");
                run.check_buttons("fullscreen");
                run.scan("fullscreen");
                run.window().toggleFullScreen(None);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "окно вышло из полноэкранного режима",
            FULLSCREEN_TIMEOUT,
            |run| run.log.ivars().exited.get() > 0 && !run.fullscreen() && run.settled(),
            |run| {
                check_resize(run, "after-fullscreen", Some(run.initial_frame.get()));
                run.check_geometry("after-fullscreen");
                run.check_buttons("after-fullscreen");
                run.report.borrow_mut().finding(
                    "fullscreen",
                    Verdict::Pass,
                    "вход и выход из полноэкранного режима подтверждены уведомлениями окна и styleMask",
                );
                Ok(())
            },
        ),
    ]
}

/// Нажатие кнопки заголовка тем же путём, что щелчок: действие кнопки.
fn press(
    run: &Run,
    which: impl Fn(&chrome::ChromeParts) -> &Retained<objc2_app_kit::NSButton>,
) -> Result<(), String> {
    let parts = run.scene.chrome.parts().ok_or("у заголовка нет частей")?;
    // SAFETY: отправитель действия может быть пустым.
    unsafe { which(parts).performClick(None) };
    run.touch();
    Ok(())
}

/// Окно больше минимума в пределах видимой области его экрана, верхний левый угол на месте.
fn grow(run: &Run) -> Result<(), String> {
    let window = run.window();
    let visible = window.screen().ok_or("окно не на экране")?.visibleFrame();
    let frame = window.frame();
    let width = (frame.size.width + GROW.width).min(visible.size.width);
    let height = (frame.size.height + GROW.height).min(visible.size.height);
    let x = frame
        .origin
        .x
        .min(visible.origin.x + visible.size.width - width)
        .max(visible.origin.x);
    let y = (frame.origin.y + frame.size.height - height).max(visible.origin.y);
    let target = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
    run.grow_target.set(Some(target));
    window.setFrame_display(target, true);
    run.touch();
    Ok(())
}

fn check_resize(run: &Run, state: &str, target: Option<NSRect>) {
    let frame = run.window().frame();
    let ok = target.is_some_and(|target| near(readback::rect(target), readback::rect(frame)));
    run.report.borrow_mut().finding(
        &format!("frame_{state}"),
        verdict(ok),
        format!(
            "кадр окна {} (ожидался {})",
            events::describe_rect(frame),
            target.map_or_else(|| "none".to_owned(), events::describe_rect)
        ),
    );
}

fn initial_checks(run: &Run) -> Result<(), String> {
    let window = run.window();
    let tokens = &run.scene.tokens;
    let size = run.content_size();
    let background = readback::srgb(&window.backgroundColor());
    let opaque = window.isOpaque() && background.is_some_and(|color| color.a == 255);
    let exact = size.width == crate::WINDOW_SIZE.width && size.height == crate::WINDOW_SIZE.height;
    let mut report = run.report.borrow_mut();
    report.finding(
        "window_opaque",
        verdict(opaque && exact),
        format!(
            "isOpaque={}, фон окна {}, содержимое {}x{} (ожидалось {}x{}), styleMask {:#x}",
            window.isOpaque(),
            background.map_or_else(|| "none".to_owned(), Rgba::hex),
            size.width,
            size.height,
            crate::WINDOW_SIZE.width,
            crate::WINDOW_SIZE.height,
            window.styleMask().0
        ),
    );
    let mut problems = Vec::new();
    for tile in &run.scene.tiles.tiles {
        let layer = tile.layer().ok_or("у плитки нет слоя")?;
        let state = readback::layer_state(&layer);
        if state.border_width != 1.0
            || state.border != Some(tokens.line)
            || state.corner_radius != tokens.r_plate
            || !layer.masksToBounds()
            || state.background != Some(tokens.panel)
        {
            problems.push(format!("плитка {}: {state:?}", tile.index()));
        }
        let head = readback::frame_in_window(tile.head());
        if (head.height - tokens.tabbar).abs() > GEOMETRY_TOLERANCE {
            problems.push(format!(
                "полоса вкладок плитки {}: {}",
                tile.index(),
                describe(head)
            ));
        }
    }
    report.finding(
        "tile_style",
        verdict(problems.is_empty()),
        if problems.is_empty() {
            format!(
                "у трёх плиток фон {}, линия 1 точка {}, радиус {}, обрезка по радиусу, полоса вкладок {} точки",
                tokens.panel.hex(),
                tokens.line.hex(),
                tokens.r_plate,
                tokens.tabbar
            )
        } else {
            problems.join("; ")
        },
    );
    report.push_json("windows_initial", &readback::hierarchy(&run.stand.app));
    drop(report);
    check_veil_passthrough(run)
}

/// Вуаль не принимает щелчок и не видна дереву доступности.
fn check_veil_passthrough(run: &Run) -> Result<(), String> {
    let window = run.window();
    let content = window.contentView().ok_or("у окна нет contentView")?;
    // SAFETY: `superview` у вида, который держит окно.
    let frame_view = unsafe { content.superview() }.ok_or("у contentView нет superview")?;
    let right = &run.scene.tiles.tiles[RIGHT];
    let point = right
        .body()
        .convertPoint_toView(events::center(right.body().bounds()), None);
    let hit = frame_view.hitTest(point);
    let hit_class = hit
        .as_deref()
        .map_or_else(|| "none".to_owned(), |view| readback::class_name(view));
    let tree = readback::accessibility_tree(&content, 12);
    let in_tree = tree
        .walk()
        .iter()
        .filter(|element| element.class == "ProbeVeil")
        .count();
    let veil = readback::accessibility(right.veil());
    let ok = hit_class != "ProbeVeil" && in_tree == 0 && veil.element != Some(true);
    let mut report = run.report.borrow_mut();
    report.push_json("content_accessibility", &tree);
    report.finding(
        "veil_passthrough",
        verdict(ok),
        format!(
            "hitTest в центре правой плитки вернул {hit_class}; вуалей в дереве доступности {in_tree}; у вуали isAccessibilityElement={:?}, роль {:?}; щелчок сквозь вуаль переводит фокус — шаг «правая плитка»",
            veil.element, veil.role
        ),
    );
    Ok(())
}

fn check_tile_focus(run: &Run, focused: usize, state: &str) {
    let tokens = &run.scene.tokens;
    let tone = tint_rgba(tokens.tone());
    let dim = tint_rgba(tokens.dim);
    let colors: Vec<Option<Rgba>> = (0..run.scene.tiles.tiles.len())
        .map(|index| run.veil_color(index))
        .collect();
    let responder = run.is_responder(&run.scene.tiles.tiles[focused]);
    let ok = responder
        && run.scene.tiles.focus() == Focus::Tile(focused)
        && colors.iter().enumerate().all(|(index, color)| {
            let want = if index == focused { tone } else { dim };
            color.is_some_and(|color| color.distance(want) <= 1)
        })
        && ring(run).is_none();
    run.report.borrow_mut().finding(
        &format!("tile_focus_{state}"),
        verdict(ok),
        format!(
            "first responder — плитка {focused}: {responder}; фокус {:?}; слой над плитками {:?}; тон {} у плитки {focused}, вуаль {} у остальных; рамка строки навигатора {:?}",
            run.scene.tiles.focus(),
            colors.iter().map(|c| c.map(Rgba::hex)).collect::<Vec<_>>(),
            tone.hex(),
            dim.hex(),
            ring(run)
        ),
    );
}

/// Рамка выбранной строки навигатора: толщина и цвет, если она есть.
fn ring(run: &Run) -> Option<(f64, Option<Rgba>)> {
    let layer = run.scene.tiles.navigator.selected_row()?.layer()?;
    let state = readback::layer_state(&layer);
    (state.border_width > 0.0).then_some((state.border_width, state.border))
}

fn check_navigator_focus(run: &Run) {
    let tokens = &run.scene.tokens;
    let dim = tint_rgba(tokens.dim);
    let colors: Vec<Option<Rgba>> = (0..run.scene.tiles.tiles.len())
        .map(|index| run.veil_color(index))
        .collect();
    let all_dim = colors
        .iter()
        .all(|color| color.is_some_and(|color| color.distance(dim) <= 1));
    let responder = run.is_responder(&run.scene.tiles.navigator);
    let ring = ring(run);
    let ring_ok = ring == Some((1.0, Some(tokens.focus)));
    let navigator_layer = run
        .scene
        .tiles
        .navigator
        .layer()
        .map(|layer| readback::layer_state(&layer));
    let flat = navigator_layer
        .as_ref()
        .is_some_and(|layer| layer.background == Some(tokens.chrome) && layer.border_width == 0.0);
    run.report.borrow_mut().finding(
        "navigator_focus",
        verdict(responder && run.scene.tiles.focus() == Focus::Navigator && all_dim && ring_ok && flat),
        format!(
            "first responder — навигатор: {responder}; фокус {:?}; слой над плитками {:?} (вуаль {}); рамка выбранной строки {ring:?} (ожидалась 1 точка {}); навигатор плоский на фоне окна: {flat}",
            run.scene.tiles.focus(),
            colors.iter().map(|c| c.map(Rgba::hex)).collect::<Vec<_>>(),
            dim.hex(),
            tokens.focus.hex()
        ),
    );
}

fn check_search(run: &Run) {
    let parts = run.scene.chrome.parts();
    let (presses_before, moves_before, frame_before) = run.search_before.get();
    let presses = parts.map_or(0, |parts| parts.search.presses());
    let moves = run.log.ivars().will_move.get() - moves_before;
    let frame = run.window().frame();
    let hit = run.window().contentView().and_then(|content| {
        // SAFETY: `superview` у вида, который держит окно.
        let frame_view = unsafe { content.superview() }?;
        let search = &parts?.search;
        let point = search.convertPoint_toView(events::center(search.bounds()), None);
        frame_view.hitTest(point)
    });
    let hit_class = hit
        .as_deref()
        .map_or_else(|| "none".to_owned(), |view| readback::class_name(view));
    let hit_moves = hit.as_deref().map(|view| view.mouseDownCanMoveWindow());
    let bar_moves = run.scene.chrome.mouseDownCanMoveWindow();
    let down = run
        .click
        .borrow()
        .and_then(|(down, _)| events::record(down));
    let held_ms = down.as_ref().and_then(|record| {
        Some(events::to_ms(record.send_return?) - events::to_ms(record.dispatch_start?))
    });
    let responder_kept = run.is_responder(&run.scene.tiles.tiles[CENTER]);
    let mut report = run.report.borrow_mut();
    if let Some(held) = held_ms {
        report.metric("search_mousedown_dispatch_to_return", "ms", vec![held]);
    }
    let ok = presses > presses_before
        && hit_class == "ProbeSearchField"
        && hit_moves == Some(false)
        && bar_moves
        && moves == 0
        && frame == frame_before
        && held_ms.is_some_and(|held| held < PROMPT_RETURN_MS)
        && responder_kept;
    report.finding(
        "search_click_no_drag",
        verdict(ok),
        format!(
            "нажатий до поля {presses_before} → {presses}; hitTest в центре поля {hit_class}, mouseDownCanMoveWindow {hit_moves:?}; у полосы заголовка {bar_moves}; NSWindowWillMove {moves}; кадр {} → {}; sendEvent нажатия {held_ms:?} мс (порог {PROMPT_RETURN_MS}); фокус остался на центральной плитке: {responder_kept}. Настоящее перетаскивание за заголовок проверяет владелец в задаче 14",
            events::describe_rect(frame_before),
            events::describe_rect(frame)
        ),
    );
}

/// Пиксельная проба рендера содержимого: плитки с фокусом и без, навигатор на фоне окна.
fn probe_pixels(run: &Run, state: &str) -> Result<(), String> {
    let root = &run.scene.root;
    let render = ViewRender::of(root)?;
    let tokens = &run.scene.tokens;
    let tiles = &run.scene.tiles;
    let focus = tiles.focus();
    let mut probes: Vec<(String, NSPoint, Rgba)> = tiles
        .tiles
        .iter()
        .enumerate()
        .map(|(index, tile)| {
            // В плитке со списком три строки ниже середины: щелчок «правая плитка» выбирает
            // строку в середине.
            let body = tile.body();
            let center = events::center(body.bounds());
            let shift = if tile.list().is_some() {
                3.0 * tokens.row
            } else {
                0.0
            };
            let below = NSPoint::new(center.x, center.y - shift);
            let point = body.convertPoint_toView(below, Some(root));
            let want = if focus == Focus::Tile(index) {
                tokens.focused_panel()
            } else {
                tokens.veiled_panel()
            };
            (format!("плитка {index}"), point, want)
        })
        .collect();
    // Справа внизу навигатора: подписи дерева короткие и туда не доходят, а полоса прокрутки
    // уже 40 точек.
    let navigator = &tiles.navigator;
    let bounds = navigator.bounds().size;
    let no_text = NSPoint::new(bounds.width - 40.0, bounds.height - 40.0);
    probes.push((
        "навигатор".to_owned(),
        navigator.convertPoint_toView(no_text, Some(root)),
        tokens.chrome,
    ));
    let mut ok = true;
    let mut lines = Vec::new();
    for (label, point, want) in probes {
        let got = render.pixel(point)?;
        ok &= got.distance(want) <= PIXEL_TOLERANCE;
        lines.push(format!("{label} {} (ожидался {})", got.hex(), want.hex()));
    }
    let mut report = run.report.borrow_mut();
    report.push(&format!("pixels_{state}"), lines.join(", "));
    if state == "center-focused" {
        report.push("render_color_space", render.color_space());
        report.push("render_format", render.format());
    }
    drop(report);
    run.pixels
        .borrow_mut()
        .push((format!("{state}: {}", lines.join(", ")), ok));
    Ok(())
}

fn finish(run: Rc<Run>, result: Result<(), StepFailure>) {
    run.log.stop();
    let stand = Rc::clone(&run.stand);
    let mut report = run.report.replace(Report::default());
    run.settle.add_to(&mut report);
    if let Err(failure) = result {
        return stand.conclude(report, Err(failure.describe()));
    }
    conclude_material(&run, &mut report);
    conclude_buttons(&run, &mut report);
    let pixels = run.pixels.borrow();
    report.finding(
        "veil_pixel",
        verdict(!pixels.is_empty() && pixels.iter().all(|(_, ok)| *ok)),
        format!(
            "рендер вида, допуск {PIXEL_TOLERANCE}: {}",
            pixels
                .iter()
                .map(|(line, _)| line.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        ),
    );
    drop(pixels);
    report.push("window_button_moves", run.scene.chrome.button_moves());
    capture(run, report);
}

fn conclude_material(run: &Run, report: &mut Report) {
    let main = run.window().windowNumber();
    let baseline_number = run.baseline.borrow().as_ref().map(|b| b.windowNumber());
    let baseline: Vec<Found> = run
        .baseline_scans
        .borrow()
        .iter()
        .flat_map(|scan| scan.found.iter().cloned())
        .collect();
    let scans = run.scans.borrow();
    let mut overall = Outcome::None;
    let mut states = Vec::new();
    let mut fullscreen = None;
    let mut others = Vec::new();
    for (state, windows) in scans.iter() {
        let own: Vec<Found> = windows
            .iter()
            .filter(|scan| scan.number == main)
            .flat_map(|scan| scan.found.iter().cloned())
            .collect();
        let diff = material::difference(&own, &baseline);
        let outcome = material::outcome(diff.iter().copied());
        let line = format!(
            "{state}: {} (находок {}, вне пустого окна {})",
            outcome.describe(),
            own.len(),
            diff.len()
        );
        if state == "fullscreen" {
            fullscreen = Some((outcome, line));
        } else {
            overall = overall.worst(outcome);
            states.push(line);
        }
        report.push_json(&format!("material_{state}"), windows);
        report.push_json(&format!("material_{state}_difference"), &diff);
        for scan in windows
            .iter()
            .filter(|scan| scan.number != main && Some(scan.number) != baseline_number)
        {
            if !scan.found.is_empty() {
                others.push(format!(
                    "{state}: окно #{} {} — {}",
                    scan.number,
                    scan.class,
                    material::outcome(scan.found.iter()).describe()
                ));
            }
        }
    }
    report.push_json("material_baseline", &*run.baseline_scans.borrow());
    report.push("material_outcome", overall.describe());
    report.finding(
        "material",
        verdict(overall != Outcome::VisibleMaterial),
        format!(
            "вариант {}: итог «{}» по разнице с пустым окном той же конфигурации; {}; находок пустого окна {}; системные окна: {:?}. Вывод предварительный, окончательный — после визуального прохода задачи 14",
            run.scene.split.variant.name(),
            overall.describe(),
            states.join("; "),
            baseline.len(),
            others
        ),
    );
    if let Some((outcome, line)) = fullscreen {
        report.finding(
            "material_fullscreen",
            verdict(outcome != Outcome::VisibleMaterial),
            format!(
                "{line}; разница с пустым окном вне полноэкранного режима, системные окна полноэкранного режима не входят в вывод: {others:?}"
            ),
        );
    }
}

fn conclude_buttons(run: &Run, report: &mut Report) {
    let states = run.buttons.borrow();
    report.push_json("window_buttons", &*states);
    let verdict_states: Vec<&ButtonState> = states
        .iter()
        .filter(|state| state.state != "fullscreen")
        .collect();
    let in_band = !verdict_states.is_empty() && verdict_states.iter().all(|state| state.in_band);
    let describe_states = |pick: fn(&ButtonState) -> bool| {
        verdict_states
            .iter()
            .map(|state| format!("{}={}", state.state, pick(state)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    report.finding(
        "window_buttons_in_band",
        verdict(in_band),
        format!(
            "кадры standardWindowButton в полосе {} точек: {}; в середине полосы (наблюдение, допуск {CENTER_TOLERANCE}): {}; в полноэкранном режиме кнопки в другом окне и в вывод не входят",
            run.scene.tokens.titlebar,
            describe_states(|state| state.in_band),
            describe_states(|state| state.centered)
        ),
    );
}

/// Рендер содержимого и снимок окна для визуального прохода владельца.
fn capture(run: Rc<Run>, mut report: Report) {
    let stand = Rc::clone(&run.stand);
    let variant = run.scene.split.variant.name();
    let render = match ViewRender::of(&run.scene.root) {
        Ok(render) => render,
        Err(reason) => return stand.conclude(report, Err(reason)),
    };
    let jobs: Vec<Background<Snapshot>> = vec![
        render.save(&stand.out, &format!("split-{variant}-content")),
        snapshot::capture_window(run.window(), &stand.out, &format!("split-{variant}-window")),
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
    use crate::args::Contrast;
    use crate::tokens::{Cell, Theme, tokens};

    fn dark() -> Tokens {
        tokens(
            Cell::new(Theme::Dark, Contrast::Normal),
            InputMode::Navigation,
        )
    }

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn expected_geometry_at_minimum() {
        let zones = expected(1180.0, 720.0, true, true, &dark());
        assert_eq!(zones.chrome, rect(0.0, 680.0, 1180.0, 40.0));
        assert_eq!(zones.navigator, Some(rect(0.0, 0.0, 230.0, 680.0)));
        // Центр между делителями: 236 … 1180 − 372.
        assert_eq!(zones.bottom, rect(236.0, 6.0, 572.0, 180.0));
        assert_eq!(zones.center, rect(236.0, 192.0, 572.0, 482.0));
        // Правая плитка за делителем, зазор 6 у правого края окна.
        assert_eq!(zones.right, Some(rect(814.0, 6.0, 360.0, 668.0)));
        // Зазор между центром и правой плиткой — 6 точек.
        let right = zones.right.map_or(0.0, |r| r.x);
        assert_eq!(right - (zones.center.x + zones.center.width), 6.0);
    }

    #[test]
    fn expected_geometry_with_collapsed_zones() {
        let left = expected(1180.0, 720.0, false, true, &dark());
        assert_eq!(left.navigator, None);
        assert_eq!(left.center.x, 6.0);
        assert_eq!(left.center.width, 1180.0 - 372.0 - 6.0);
        let right = expected(1180.0, 720.0, true, false, &dark());
        assert_eq!(right.right, None);
        assert_eq!(right.center.x + right.center.width, 1174.0);
    }

    fn actual_from(zones: &Zones) -> Actual {
        let none = rect(0.0, 0.0, 0.0, 0.0);
        Actual {
            content: Size {
                width: zones.chrome.width,
                height: zones.chrome.y + zones.chrome.height,
            },
            chrome: zones.chrome,
            navigator: zones.navigator.unwrap_or(none),
            navigator_hidden: zones.navigator.is_none(),
            center: zones.center,
            bottom: zones.bottom,
            right: zones.right.unwrap_or(none),
            right_hidden: zones.right.is_none(),
            left_collapsed: zones.navigator.is_none(),
            right_collapsed: zones.right.is_none(),
        }
    }

    #[test]
    fn compare_reports_mismatches() {
        let zones = expected(1180.0, 720.0, true, true, &dark());
        assert!(compare(&zones, &actual_from(&zones)).is_empty());
        let mut shifted = actual_from(&zones);
        shifted.center.x += 1.0;
        let mismatches = compare(&zones, &shifted);
        assert_eq!(mismatches.len(), 1, "{mismatches:?}");
        assert!(mismatches[0].starts_with("центр"), "{mismatches:?}");
        let mut within = actual_from(&zones);
        within.center.x += 0.4;
        assert!(compare(&zones, &within).is_empty());

        let collapsed = expected(1180.0, 720.0, false, true, &dark());
        let mut visible_nav = actual_from(&collapsed);
        visible_nav.navigator = rect(0.0, 0.0, 230.0, 680.0);
        visible_nav.navigator_hidden = false;
        assert_eq!(compare(&collapsed, &visible_nav).len(), 1);
        let mut zero_width = actual_from(&collapsed);
        zero_width.navigator_hidden = false;
        assert!(compare(&collapsed, &zero_width).is_empty());
        let mut hidden_open = actual_from(&zones);
        hidden_open.right_hidden = true;
        assert_eq!(compare(&zones, &hidden_open).len(), 1);
    }

    #[test]
    fn stable_needs_unchanged_frames() {
        let mut stable = Stable::new();
        let frames = vec![rect(0.0, 0.0, 1.0, 1.0)];
        assert!(!stable.quiet(frames.clone()), "первое чтение — изменение");
        stable.since = Instant::now() - QUIET;
        assert!(stable.quiet(frames.clone()));
        assert!(!stable.quiet(vec![rect(1.0, 0.0, 1.0, 1.0)]));
    }
}
