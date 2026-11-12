//! Сценарий `readback-check`: самопроверка стенда на известных ответах. Сцена стенда — слои
//! известного цвета, вуаль с известной непрозрачностью, вид с подсчётом `drawRect:`, слой с
//! анимацией и поле ввода с field editor. Проверяются иерархия видов и слоёв, цвет и ключи
//! анимаций слоя, first responder с разрешением field editor, атрибуты доступности, пиксели
//! рендера вида, порядок наблюдателя конца прохода отображения и снимки экрана.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSColor, NSResponder, NSScreen, NSTextField, NSView};
use objc2_foundation::{NSNumber, NSPoint, NSRect, NSSize, NSString, ns_string};
use objc2_quartz_core::{CABasicAnimation, CAMediaTiming};

use super::events::{self, Key};
use super::passes::{self, ObserverInfo};
use super::readback::{self, Rgba};
use super::snapshot::{self, Background, ViewRender};
use super::stand::DrawView;
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepFailure, Verdict, poll,
    run_steps,
};
use crate::results::{Rect, Snapshot, SnapshotStatus};

const STEP_TIMEOUT: Duration = Duration::from_secs(3);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);
/// Допуск пиксельной пробы: одна единица канала.
const PIXEL_TOLERANCE: u8 = 1;

const PANEL: Rgba = Rgba::opaque(0x0A, 0x09, 0x08);
const GRAY: Rgba = Rgba::opaque(128, 128, 128);
const BLACK: Rgba = Rgba::opaque(0, 0, 0);
const VEIL_ALPHA: f64 = 0.5;
const FILL: Rgba = Rgba::opaque(0x2F, 0x6F, 0xEB);
const PULSE_KEY: &str = "probe-pulse";
const FIELD_TEXT: &str = "abc";

const SWATCH: NSRect = rect(20.0, 20.0, 120.0, 80.0);
const VEILED: NSRect = rect(160.0, 20.0, 120.0, 80.0);
const DRAWN: NSRect = rect(300.0, 20.0, 120.0, 80.0);
const PULSE: NSRect = rect(440.0, 20.0, 40.0, 40.0);
const FIELD: NSRect = rect(20.0, 140.0, 240.0, 24.0);

const fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

fn ns_color(color: Rgba, alpha: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(
        f64::from(color.r) / 255.0,
        f64::from(color.g) / 255.0,
        f64::from(color.b) / 255.0,
        alpha,
    )
}

fn layer_view(
    mtm: objc2::MainThreadMarker,
    frame: NSRect,
    color: Rgba,
    alpha: f64,
) -> Retained<NSView> {
    let view = NSView::initWithFrame(NSView::alloc(mtm), frame);
    view.setWantsLayer(true);
    if let Some(layer) = view.layer() {
        layer.setBackgroundColor(Some(&ns_color(color, alpha).CGColor()));
    }
    view
}

struct Run {
    stand: Rc<Stand>,
    swatch: Retained<NSView>,
    veil: Retained<NSView>,
    draw: Retained<DrawView>,
    pulse: Retained<NSView>,
    field: Retained<NSTextField>,
    settle: Rc<Settle>,
    /// Время `setNeedsDisplay` и число отрисовок до него.
    change: Cell<Option<Duration>>,
    draws_before: Cell<usize>,
    keys: RefCell<Vec<u64>>,
}

impl Run {
    fn key_record(&self, index: usize) -> Option<events::Record> {
        self.keys
            .borrow()
            .get(index)
            .copied()
            .and_then(events::record)
    }

    fn logical_is_field(&self) -> bool {
        readback::logical_first_responder(&self.stand.window)
            .is_some_and(|logical| events::same(&*logical, &*self.field))
    }

    fn state(&self) -> String {
        format!(
            "draws={} passes={} {}",
            self.draw.draws(),
            events::display_passes(),
            events::focus_state(&self.stand.app)
        )
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::ReadbackCheck);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(content) = stand.window.contentView() else {
        return stand.conclude(report, Err("у окна нет contentView".to_owned()));
    };
    let mtm = stand.app.mtm();
    let swatch = layer_view(mtm, SWATCH, PANEL, 1.0);
    let veiled = layer_view(mtm, VEILED, GRAY, 1.0);
    let veil = layer_view(
        mtm,
        NSRect::new(NSPoint::ZERO, VEILED.size),
        BLACK,
        VEIL_ALPHA,
    );
    veiled.addSubview(&veil);
    let draw = DrawView::new(mtm, DRAWN, ns_color(FILL, 1.0));
    let pulse = layer_view(mtm, PULSE, GRAY, 1.0);
    let field = NSTextField::textFieldWithString(&NSString::from_str(FIELD_TEXT), mtm);
    field.setFrame(FIELD);
    for view in [&*swatch, &*veiled, &*draw, &*pulse, &*field] {
        content.addSubview(view);
    }
    if let Some(layer) = pulse.layer() {
        let animation = CABasicAnimation::animationWithKeyPath(Some(ns_string!("opacity")));
        let (from, to) = (NSNumber::new_f32(1.0), NSNumber::new_f32(0.98));
        let (from, to): (&AnyObject, &AnyObject) = (&from, &to);
        // SAFETY: значения анимации `opacity` — NSNumber.
        unsafe {
            animation.setFromValue(Some(from));
            animation.setToValue(Some(to));
        }
        animation.setDuration(1000.0);
        layer.addAnimation_forKey(&animation, Some(&NSString::from_str(PULSE_KEY)));
    }
    // Начальный first responder назначает код сцены, как в обычном приложении.
    let responder: &NSResponder = &field;
    if !stand.window.makeFirstResponder(Some(responder)) {
        return stand.conclude(report, Err("поле не стало first responder".to_owned()));
    }

    let run = Rc::new(Run {
        settle: Settle::new(vec![stand.window.clone()]),
        stand: Rc::clone(&stand),
        swatch,
        veil,
        draw,
        pulse,
        field,
        change: Cell::new(None),
        draws_before: Cell::new(0),
        keys: RefCell::new(Vec::new()),
    });
    run_steps(steps(&run), move |result| finish(run, report, result));
}

fn steps(run: &Rc<Run>) -> Vec<Step> {
    let focus = {
        let run = Rc::clone(run);
        move || {
            if let Poll::Pending(state) = run.settle.poll() {
                return Poll::Pending(state);
            }
            let app = &run.stand.app;
            let key = app.keyWindow();
            poll(
                app.isActive()
                    && key.is_some_and(|key| events::same(&*key, &*run.stand.window))
                    && run.logical_is_field(),
                || run.state(),
            )
        }
    };
    let warp = {
        let run = Rc::clone(run);
        move || events::warp_cursor_outside(run.stand.app.mtm(), &run.stand.app).map(|_| ())
    };
    let outside = {
        let run = Rc::clone(run);
        move || poll(events::cursor_outside(&run.stand.app), || run.state())
    };
    let invalidate = {
        let run = Rc::clone(run);
        move || {
            run.draws_before.set(run.draw.draws());
            run.change.set(events::elapsed());
            run.draw.setNeedsDisplay(true);
            Ok(())
        }
    };
    let drawn = {
        let run = Rc::clone(run);
        move || {
            let pass = run.change.get().and_then(events::display_pass_after);
            poll(pass.is_some(), || run.state())
        }
    };
    let press = {
        let run = Rc::clone(run);
        move || {
            *run.keys.borrow_mut() = events::press(Key::LEFT)?;
            Ok(())
        }
    };
    let pressed = {
        let run = Rc::clone(run);
        move || {
            let up = run.key_record(1).is_some_and(|r| r.send_return.is_some());
            let pass = run
                .key_record(0)
                .and_then(|r| r.send_return)
                .and_then(events::display_pass_after);
            poll(up && pass.is_some(), || run.state())
        }
    };
    vec![
        Step::new(
            "окно неподвижно, активность, key window и поле в фокусе",
            SETTLE_TIMEOUT,
            focus,
            warp,
        ),
        Step::new("курсор вне окон", STEP_TIMEOUT, outside, invalidate),
        Step::new(
            "проход отображения после setNeedsDisplay",
            STEP_TIMEOUT,
            drawn,
            press,
        ),
        Step::new(
            "Left обработана, проход отображения отмечен",
            STEP_TIMEOUT,
            pressed,
            || Ok(()),
        ),
    ]
}

fn finish(run: Rc<Run>, mut report: Report, result: Result<(), StepFailure>) {
    let stand = Rc::clone(&run.stand);
    run.settle.add_to(&mut report);
    if let Err(failure) = result {
        return stand.conclude(report, Err(failure.describe()));
    }
    check_display_pass(&run, &mut report);
    check_event_pass(&run, &mut report);
    check_hierarchy(&run, &mut report);
    check_layers(&run, &mut report);
    check_first_responder(&run, &mut report);
    check_accessibility(&run, &mut report);
    let render = match check_render(&run, &mut report) {
        Ok(render) => render,
        Err(reason) => return stand.conclude(report, Err(reason)),
    };
    capture(run, report, render);
}

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

fn check_display_pass(run: &Run, report: &mut Report) {
    let observers = passes::main_observers();
    report.push_json("main_runloop_observers", &observers);
    let later: Vec<&ObserverInfo> = observers
        .iter()
        .filter(|observer| observer.before_waiting() && observer.order > passes::ORDER)
        .collect();
    report.push_json("before_waiting_after_probe", &later);
    let change = run.change.get();
    let pass = change.and_then(events::display_pass_after);
    let last_draw = run.draw.last_draw();
    let (before, after) = (run.draws_before.get(), run.draw.draws());
    let ordered = matches!(
        (change, last_draw, pass),
        (Some(change), Some(draw), Some(pass)) if change <= draw && draw <= pass
    );
    let ms = |value: Option<Duration>| value.map_or(f64::NAN, events::to_ms);
    report.metric(
        "set_needs_display_to_display_pass",
        "ms",
        vec![ms(pass) - ms(change)],
    );
    report.finding(
        "display_pass_order",
        verdict(after > before && ordered),
        format!(
            "setNeedsDisplay в {:.3} мс, drawRect: в {:.3} мс (отрисовок {before} → {after}), конец прохода в {:.3} мс; наблюдатель с порядком {}, после него на BeforeWaiting: {:?}",
            ms(change),
            ms(last_draw),
            ms(pass),
            passes::ORDER,
            later.iter().map(|o| (o.order, o.callout.as_str())).collect::<Vec<_>>()
        ),
    );
}

fn check_event_pass(run: &Run, report: &mut Report) {
    let Some(down) = run.key_record(0) else {
        return report.finding("display_pass_after_event", Verdict::Fail, "нет keyDown");
    };
    let pass = down.send_return.and_then(events::display_pass_after);
    let ok = matches!((down.send_return, pass), (Some(back), Some(pass)) if back <= pass);
    let ms = |value: Option<Duration>| value.map_or(f64::NAN, events::to_ms);
    report.metric(
        "keydown_post_to_display_pass",
        "ms",
        vec![ms(pass) - events::to_ms(down.posted)],
    );
    report.metric(
        "keydown_dispatch_to_return",
        "ms",
        vec![ms(down.send_return) - ms(down.dispatch_start)],
    );
    report.finding(
        "display_pass_after_event",
        verdict(ok),
        format!(
            "keyDown Left: postEvent {:.3} мс, возврат sendEvent {:.3} мс, конец прохода отображения {:.3} мс",
            events::to_ms(down.posted),
            ms(down.send_return),
            ms(pass)
        ),
    );
}

fn check_hierarchy(run: &Run, report: &mut Report) {
    let windows = readback::hierarchy(&run.stand.app);
    let number = run.stand.window.windowNumber();
    let main = windows.iter().find(|window| window.number == number);
    let root = main.and_then(|window| window.root.as_ref());
    let expect = [
        ("NSView", SWATCH),
        ("NSView", VEILED),
        ("ProbeDrawView", DRAWN),
        ("NSView", PULSE),
        ("NSTextField", FIELD),
    ];
    let missing: Vec<String> = expect
        .iter()
        .filter(|(class, frame)| {
            root.and_then(|root| root.find(class, readback::rect(*frame)))
                .is_none()
        })
        .map(|(class, frame)| format!("{class} {}", events::describe_rect(*frame)))
        .collect();
    let root_class = root.map_or("none", |root| root.class.as_str());
    let veil_nested = root
        .and_then(|root| root.find("NSView", readback::rect(VEILED)))
        .is_some_and(|veiled| veiled.subviews.iter().any(|sub| sub.class == "NSView"));
    let ok = main.is_some() && root_class == "NSThemeFrame" && missing.is_empty() && veil_nested;
    report.finding(
        "readback_hierarchy",
        verdict(ok),
        format!(
            "окон {}, корень основного окна {root_class}, не найдено: {missing:?}, вуаль внутри своей плитки: {veil_nested}",
            windows.len()
        ),
    );
    report.push_json("windows", &windows);
}

fn check_layers(run: &Run, report: &mut Report) {
    let state = |view: &NSView| view.layer().map(|layer| readback::layer_state(&layer));
    let swatch = state(&run.swatch).and_then(|layer| layer.background);
    let veil = state(&run.veil).and_then(|layer| layer.background);
    let expected_veil = Rgba::from_unit(0.0, 0.0, 0.0, VEIL_ALPHA);
    let keys = state(&run.pulse).map(|layer| layer.animation_keys);
    if let Some(layer) = run.pulse.layer() {
        layer.removeAnimationForKey(&NSString::from_str(PULSE_KEY));
    }
    let keys_after = state(&run.pulse).map(|layer| layer.animation_keys);
    let ok = swatch == Some(PANEL)
        && veil == Some(expected_veil)
        && keys
            .as_ref()
            .is_some_and(|keys| keys.iter().any(|key| key == PULSE_KEY))
        && keys_after.as_ref().is_some_and(Vec::is_empty);
    report.finding(
        "readback_layer",
        verdict(ok),
        format!(
            "фон плитки {} (ожидался {}), фон вуали {} (ожидался {}), ключи анимаций {keys:?}, после снятия {keys_after:?}",
            swatch.map_or_else(|| "none".to_owned(), Rgba::hex),
            PANEL.hex(),
            veil.map_or_else(|| "none".to_owned(), Rgba::hex),
            expected_veil.hex()
        ),
    );
}

fn check_first_responder(run: &Run, report: &mut Report) {
    let responder = readback::first_responder(&run.stand.window);
    let ok = run.logical_is_field()
        && responder
            .as_ref()
            .is_some_and(|r| r.field_editor && r.logical_class == "NSTextField");
    report.push_json("focus", &readback::focus(&run.stand.app));
    report.finding(
        "readback_first_responder",
        verdict(ok),
        format!(
            "first responder {responder:?}, логическое поле — поле сцены: {}",
            run.logical_is_field()
        ),
    );
}

/// Глубина поддерева доступности поля: сам вид, его ячейка или field editor и их потомки.
const AX_DEPTH: usize = 3;
const AX_FRAME_SLACK: f64 = 2.0;

fn check_accessibility(run: &Run, report: &mut Report) {
    let tree = readback::accessibility_tree(&run.field, AX_DEPTH);
    let on_screen = readback::frame_on_screen(&run.field);
    let element = tree.find_role("AXTextField");
    let frame_matches = match (element.and_then(|e| e.frame), on_screen) {
        (Some(ax), Some(screen)) => encloses(ax, screen),
        _ => false,
    };
    let ok = element
        .is_some_and(|e| e.value.as_deref() == Some(FIELD_TEXT) && e.focused == Some(true))
        && frame_matches;
    let swatch = readback::accessibility(&run.swatch);
    report.push_json("field_accessibility", &tree);
    report.push_json("swatch_accessibility", &swatch);
    report.finding(
        "readback_accessibility",
        verdict(ok && swatch.element != Some(true)),
        format!(
            "элемент AXTextField в поддереве поля: {}, значение {:?}, фокус {:?}, кадр {:?} охватывает вид {on_screen:?}: {frame_matches}; вид поля: роль {:?}, элемент {:?}; простой вид элементом: {:?}",
            element.map_or_else(|| "нет".to_owned(), |e| e.class.clone()),
            element.and_then(|e| e.value.as_deref()),
            element.and_then(|e| e.focused),
            element.and_then(|e| e.frame),
            tree.element.role,
            tree.element.element,
            swatch.element
        ),
    );
}

/// Кадр элемента доступности охватывает кадр вида и отходит от каждого края не больше чем на
/// `AX_FRAME_SLACK`: у ячейки поля он шире вида на рамку.
fn encloses(ax: Rect, view: Rect) -> bool {
    let edges = [
        view.x - ax.x,
        view.y - ax.y,
        (ax.x + ax.width) - (view.x + view.width),
        (ax.y + ax.height) - (view.y + view.height),
    ];
    edges
        .iter()
        .all(|edge| (0.0..=AX_FRAME_SLACK).contains(edge))
}

fn center(frame: NSRect) -> NSPoint {
    events::center(frame)
}

fn check_render(run: &Run, report: &mut Report) -> Result<ViewRender, String> {
    let content = run
        .stand
        .window
        .contentView()
        .ok_or("у окна нет contentView")?;
    let render = ViewRender::of(&content)?;
    let probes = [
        ("слой", center(SWATCH), PANEL),
        ("вуаль", center(VEILED), GRAY.blend(BLACK, VEIL_ALPHA)),
        ("drawRect", center(DRAWN), FILL),
    ];
    let mut ok = true;
    let mut lines = Vec::new();
    for (label, point, expected) in probes {
        let got = render.pixel(point)?;
        ok &= got.distance(expected) <= PIXEL_TOLERANCE;
        lines.push(format!(
            "{label} {} (ожидался {})",
            got.hex(),
            expected.hex()
        ));
    }
    report.push("render_color_space", render.color_space());
    report.push("render_format", render.format());
    report.finding(
        "readback_render_pixel",
        verdict(ok),
        format!(
            "рендер вида {}x{} px: {}; допуск {PIXEL_TOLERANCE}",
            render.pixels().width,
            render.pixels().height,
            lines.join(", ")
        ),
    );
    Ok(render)
}

/// Рендер вида пишется всегда, снимки окна и композиции — только при разрешении.
fn capture(run: Rc<Run>, mut report: Report, render: ViewRender) {
    let stand = Rc::clone(&run.stand);
    let allowed = snapshot::screen_capture_allowed();
    let primary = NSScreen::screens(stand.app.mtm())
        .firstObject()
        .map(|screen| screen.frame().size.height);
    let Some(primary) = primary else {
        return stand.conclude(report, Err("нет экранов для снимка композиции".to_owned()));
    };
    let jobs: Vec<Background<Snapshot>> = vec![
        render.save(&stand.out, "readback-content"),
        snapshot::capture_window(&stand.window, &stand.out, "readback-window"),
        snapshot::capture_composition(&stand.window, primary, &stand.out, "readback-composition"),
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
        if allowed {
            report.finding(
                "screen_capture",
                Verdict::Pass,
                "снимки окна и композиции записаны, screencapture завершился успешно",
            );
        } else {
            report.finding(
                "screen_capture",
                Verdict::PendingManual,
                snapshot::NO_PERMISSION,
            );
        }
        stand.conclude(report, Ok(()));
    });
}
