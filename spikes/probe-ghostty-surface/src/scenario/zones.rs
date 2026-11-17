//! Сценарий `zones`: одна живая фикстура в сцене четырёх зон и проверки H5. Поверхность проходит
//! по четырём зонам и обратно, в том числе в части разделённых зон, под потоком вывода фикстуры;
//! окно и делители меняют размер; масштаб меняется переносом окна на другой экран, если он есть.
//! После каждого изменения сверяются фактическое состояние: тот же `NSView` в теле плитки, та же
//! ручка и поколение поверхности, тот же PID, TTY и nonce ребёнка, `TIOCGWINSZ` из квитанции
//! `winch` совпадает с `ghostty_surface_size`, счётчики создания и освобождения, first responder.
//!
//! Обслуживание главной очереди во время ресайза окна проверяется перетаскиванием угла окна
//! синтетическими событиями мыши: таймер в общих режимах цикла отправляет их по одному и читает
//! режим цикла, `inLiveResize`, проходы runtime и квитанции фикстуры.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{NSEventType, NSScreen};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use serde::Serialize;

use super::counters;
use super::surface_env::{self, State};
use super::{Poll, Report, Scenario, Stand, Step, poll, run_steps, window};
use crate::clock;
use crate::fixture::{Command, Event, Winsize};
use crate::ghostty::config::ColorScheme;
use crate::ghostty::runtime::{self, Created};
use crate::ghostty::surface::{Decision, GridSize, Phase};
use crate::results::{Requirement, Verdict};
use crate::scenario::events;
use crate::scene::{Divider, Scene, Zone};
use crate::view::SurfaceView;

const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);
const SIZE_TIMEOUT: Duration = Duration::from_secs(3);
const READY_TIMEOUT: Duration = Duration::from_secs(12);
const STREAM_TIMEOUT: Duration = Duration::from_secs(5);
/// Шаг ожидания размера; внутренний предел [`SETTLE_LIMIT`] короче, и по нему шаг записывает
/// расхождение, а не тайм-аут.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(6);
const SETTLE_LIMIT: Duration = Duration::from_secs(3);
const DRAG_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
const TITLE: &str = "терминал";
/// Поток вывода: 40 с по строке в миллисекунду, OSC заголовка каждые 25 строк будит `wakeup_cb`,
/// без него обычный вывод `tick` не вызывает.
const STREAM: Command = Command::Stream {
    lines: 40_000,
    interval_us: 1_000,
    columns: 48,
    osc_every: 25,
    report_every: 100,
};
const GROW: NSSize = NSSize::new(200.0, 120.0);
const DIVIDER_SHIFT: f64 = 80.0;
const DRAG_STEPS: u32 = 30;
const DRAG_STEP: f64 = 4.0;
const DRAG_INTERVAL: Duration = Duration::from_millis(16);
/// Срок перетаскивания до принудительного отпускания кнопки.
const DRAG_LIMIT: Duration = Duration::from_secs(4);
/// Отступ точки нажатия от правого нижнего угла окна внутрь.
const CORNER_INSET: f64 = 2.0;
const SURFACE_VIEW_CLASS: &str = "ProbeSurfaceView";
/// Окно считается неподвижным, если рамка не менялась столько времени.
const QUIET: Duration = Duration::from_millis(500);

/// Идентичность поверхности и процесса, прочитанная из фактического состояния.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct Identity {
    view: usize,
    /// Вид активной вкладки плитки, в которой вид находится по цепочке `superview`.
    shown_view: Option<usize>,
    zone: Option<Zone>,
    part: Option<usize>,
    handle: usize,
    key: String,
    generation: u32,
    pid: i32,
    tty: Option<String>,
    nonce: String,
    conn: u64,
    foreground_pid: u64,
    tty_name: Option<String>,
    ready_connections: usize,
    surface_views_created: usize,
    surface_views_live: usize,
}

impl Identity {
    /// Отличия от `base`; пусто — идентичность сохранена.
    fn differences(&self, base: &Identity) -> Vec<String> {
        let mut out = Vec::new();
        let mut check = |name: &str, same: bool, now: String, was: String| {
            if !same {
                out.push(format!("{name}: {was} → {now}"));
            }
        };
        check(
            "NSView",
            self.view == base.view,
            format!("{:#x}", self.view),
            format!("{:#x}", base.view),
        );
        check(
            "вид в плитке",
            self.shown_view == Some(self.view),
            format!("{:?}", self.shown_view),
            format!("{:#x}", self.view),
        );
        check(
            "ручка",
            self.handle == base.handle,
            format!("{:#x}", self.handle),
            format!("{:#x}", base.handle),
        );
        check(
            "поколение",
            self.generation == base.generation,
            self.key.clone(),
            base.key.clone(),
        );
        check(
            "PID",
            self.pid == base.pid,
            self.pid.to_string(),
            base.pid.to_string(),
        );
        check(
            "TTY",
            self.tty == base.tty && self.tty_name == base.tty_name,
            format!("{:?}/{:?}", self.tty, self.tty_name),
            format!("{:?}/{:?}", base.tty, base.tty_name),
        );
        check(
            "nonce",
            self.nonce == base.nonce && self.conn == base.conn,
            format!("{}#{}", self.nonce, self.conn),
            format!("{}#{}", base.nonce, base.conn),
        );
        check(
            "группа переднего плана",
            self.foreground_pid == base.foreground_pid,
            self.foreground_pid.to_string(),
            base.foreground_pid.to_string(),
        );
        check(
            "фикстур с готовностью",
            self.ready_connections == 1,
            self.ready_connections.to_string(),
            "1".to_owned(),
        );
        check(
            "видов поверхности создано",
            self.surface_views_created == 1 && self.surface_views_live == 1,
            format!(
                "{} (живых {})",
                self.surface_views_created, self.surface_views_live
            ),
            "1 (живых 1)".to_owned(),
        );
        out
    }
}

/// Изменение геометрии, размер которого ожидается.
struct Pending {
    label: String,
    started: Instant,
    t_begin_ns: u64,
    winches_before: usize,
    grid_before: Option<(u16, u16)>,
    pixels_before: (u32, u32),
    scale_before: f64,
    /// Ожидается смена масштаба подложки, а не только размера.
    expect_scale: bool,
}

#[derive(Clone, Debug, Serialize)]
struct Checkpoint {
    label: String,
    t_begin_ns: u64,
    t_end_ns: u64,
    window_frame: [f64; 4],
    zone: Option<Zone>,
    part: Option<usize>,
    view_points: (f64, f64),
    view_pixels: (u32, u32),
    scale: f64,
    surface: Option<GridSize>,
    winsize: Option<Winsize>,
    winches: usize,
    winsize_matches: bool,
    /// Размер вида или масштаб изменились, как ожидало действие.
    geometry_changed: bool,
    settled_ms: f64,
    settle_limit_reached: bool,
    first_responder: bool,
    last_focus_feed: Option<bool>,
    identity_differences: Vec<String>,
    surfaces_created: usize,
    surfaces_freed: usize,
    passes: u64,
    stream_lines: u64,
    stream_done: bool,
    zero_size_skipped: u64,
    detached_skipped: u64,
}

#[derive(Clone, Debug, Serialize)]
struct ZeroCheck {
    pixels: (u32, u32),
    zero_skipped_before: u64,
    zero_skipped_after: u64,
    create: String,
    phase: Option<&'static str>,
    created: usize,
    freed: usize,
}

#[derive(Clone, Debug, Serialize)]
struct MoveRecord {
    to: Zone,
    part: usize,
    responder_before: bool,
    /// Вид остался first responder после смены родителя без вмешательства сцены.
    kept_by_appkit: bool,
    restored: bool,
    located: Option<(Zone, usize)>,
    zero: Option<ZeroCheck>,
    focus_feeds_during: Vec<bool>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct Drag {
    corner: (f64, f64),
    frame_before: [f64; 4],
    frame_after: [f64; 4],
    posted_drags: u32,
    up_posted: bool,
    up_forced: bool,
    samples: u32,
    samples_tracking: u32,
    samples_live_resize: u32,
    passes_tracking_before: u64,
    passes_tracking_after: u64,
    receipts_in_tracking: usize,
    winches_in_tracking: usize,
    #[serde(skip)]
    last_post: Option<Instant>,
    #[serde(skip)]
    started: Option<Instant>,
    #[serde(skip)]
    last_receipts: usize,
    #[serde(skip)]
    last_winches: usize,
}

#[derive(Clone, Debug, Serialize)]
struct ScaleRecord {
    screens: Vec<(u32, f64)>,
    from_scale: f64,
    to_scale: Option<f64>,
}

struct Zones {
    /// Масштаб менялся режимом встроенного дисплея, а не переносом на второй экран.
    scale_substitute: bool,
    env: State,
    scene: Scene,
    baseline: Option<Identity>,
    final_identity: Option<Identity>,
    checkpoints: Vec<Checkpoint>,
    pending: Option<Pending>,
    moves: Vec<MoveRecord>,
    creation_wait: Option<String>,
    stream: Option<u64>,
    stream_stop: Option<u64>,
    drag: Drag,
    scale: Option<ScaleRecord>,
    home_frame: Option<NSRect>,
    /// Рамка окна после последнего действия сценария: другой её менять не должен.
    expected_frame: Option<NSRect>,
    /// Последняя замеченная рамка окна и с какого момента она не менялась.
    quiet: Option<(NSRect, Instant)>,
    /// Отметки квитанций хода потока и его конца.
    progress_t: Vec<u64>,
    done_t: Option<u64>,
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Zones);
    report.push("variant", &stand.prelude.variant);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let (bundle, problems) = window::verify_bundle();
    report.bundle = Some(bundle);
    if !problems.is_empty() {
        return stand.conclude(
            report,
            Err(format!("не бандл пробы: {}", problems.join("; "))),
        );
    }
    let mtm = stand.view.mtm();
    let scene = Scene::new(mtm, false);
    scene.install(&stand.view, &stand.window);
    let env = surface_env::prepare_in(&stand, &mut report, ColorScheme::Light, |view| {
        scene.add_tab(Zone::Center, 0, TITLE, view)
    });
    let env = match env {
        Ok(env) => env,
        Err(reason) => return surface_env::finish(&stand, report, Err(reason), None),
    };
    scene.sync_focus(&stand.window);
    report.push_json("scene_initial", &scene.describe());
    let other = other_screen(&stand);
    let zones = Rc::new(RefCell::new(Zones {
        env,
        scene,
        baseline: None,
        final_identity: None,
        checkpoints: Vec::new(),
        pending: None,
        moves: Vec::new(),
        creation_wait: None,
        stream: None,
        stream_stop: None,
        drag: Drag::default(),
        scale: None,
        scale_substitute: false,
        home_frame: None,
        expected_frame: None,
        quiet: None,
        progress_t: Vec::new(),
        done_t: None,
    }));
    let steps = steps(&stand, &zones, other.as_ref());
    run_steps(steps, move |result| {
        let verdict = match result {
            Ok(()) => {
                judge(&zones.borrow(), &mut report);
                Ok(())
            }
            Err(failure) => Err(failure.describe()),
        };
        let zones = zones.borrow();
        report.push_json("scene_final", &zones.scene.describe());
        report.push_json("checkpoints", &zones.checkpoints);
        report.push_json("moves", &zones.moves);
        report.push_json("drag", &zones.drag);
        report.push_json("scale", &zones.scale);
        report.push_json("identity_baseline", &zones.baseline);
        report.push_json("identity_final", &zones.final_identity);
        let view = zones.env.view.clone();
        drop(zones);
        surface_env::finish(&stand, report, verdict, Some(view));
    });
}

/// Контрольная точка смены масштаба: перенос на другой экран или режим 1x встроенного дисплея.
const SCALE_LABEL: &str = "смена масштаба подложки окна";

/// Замена второго экрана: встроенный дисплей окна с масштабом 2 и режим 1x с теми же пикселями.
fn one_x_substitute(stand: &Stand) -> Option<(u32, crate::display::Mode)> {
    let screen = stand.window.screen()?;
    let id = screen.CGDirectDisplayID();
    if !objc2_core_graphics::CGDisplayIsBuiltin(id) {
        return None;
    }
    let home = crate::display::current(id)?;
    let available = crate::display::has(id, |mode| one_x_of(mode, &home));
    (home.scale() == 2.0 && available).then_some((id, home))
}

/// Режим 1x с пикселями и частотой режима `home`.
fn one_x_of(mode: &crate::display::Mode, home: &crate::display::Mode) -> bool {
    mode.scale() == 1.0
        && mode.pixel_width == home.pixel_width
        && mode.pixel_height == home.pixel_height
        && (mode.refresh_hz - home.refresh_hz).abs() < 0.5
}

/// Экран с другим масштабом подложки, чем у экрана окна.
fn other_screen(stand: &Stand) -> Option<Retained<NSScreen>> {
    let current = stand.window.screen()?;
    let scale = current.backingScaleFactor();
    NSScreen::screens(stand.view.mtm())
        .iter()
        .find(|screen| (screen.backingScaleFactor() - scale).abs() > f64::EPSILON)
}

type Shared = Rc<RefCell<Zones>>;

/// Действие с ожиданием размера после него: два шага.
fn change(
    steps: &mut Vec<Step>,
    stand: &Rc<Stand>,
    zones: &Shared,
    name: &'static str,
    expect_scale: bool,
    act: impl FnOnce(&Stand, &mut Zones) -> Result<(), String> + 'static,
) {
    let (acting, acting_stand) = (Rc::clone(zones), Rc::clone(stand));
    steps.push(Step::new(
        name,
        SETTLE_TIMEOUT,
        || Poll::Ready(()),
        move || {
            let mut zones = acting.borrow_mut();
            guard_frame(&acting_stand, &zones)?;
            begin(&mut zones, name, expect_scale);
            act(&acting_stand, &mut zones)?;
            zones.expected_frame = Some(acting_stand.window.frame());
            Ok(())
        },
    ));
    let (settling, recording, recording_stand) =
        (Rc::clone(zones), Rc::clone(zones), Rc::clone(stand));
    steps.push(Step::new(
        "размер ребёнка после изменения",
        SETTLE_TIMEOUT,
        move || settle(&mut settling.borrow_mut()),
        move || checkpoint(&recording_stand, &mut recording.borrow_mut()),
    ));
}

/// Предикат: рамка окна не менялась [`QUIET`].
fn window_quiet(stand: &Stand, zones: &mut Zones) -> Poll<()> {
    let frame = stand.window.frame();
    let since = match zones.quiet {
        Some((last, since)) if last == frame => since,
        _ => {
            zones.quiet = Some((frame, Instant::now()));
            Instant::now()
        }
    };
    poll(since.elapsed() >= QUIET, || {
        format!(
            "рамка окна {} неподвижна {} мс",
            describe(frame),
            since.elapsed().as_millis()
        )
    })
}

/// Окно неподвижно и сохранило размер содержимого: внешний менеджер окон его не трогал.
fn window_untouched(stand: &Stand, zones: &mut Zones) -> Result<(), String> {
    let frame = stand.window.frame();
    let content = stand.window.contentRectForFrameRect(frame).size;
    if content != crate::WINDOW_SIZE {
        return Err(format!(
            "предусловие: окно переставлено извне: рамка {}, содержимое {:.0}×{:.0} вместо {:.0}×{:.0}; оконный менеджер (например, AeroSpace) выключается на время прогона",
            describe(frame),
            content.width,
            content.height,
            crate::WINDOW_SIZE.width,
            crate::WINDOW_SIZE.height
        ));
    }
    zones.expected_frame = Some(frame);
    Ok(())
}

/// Рамка окна та, что оставило последнее действие сценария; иначе окно двигает кто-то другой, и
/// стенд потерял контроль над геометрией.
fn guard_frame(stand: &Stand, zones: &Zones) -> Result<(), String> {
    let frame = stand.window.frame();
    match zones.expected_frame {
        Some(expected) if expected != frame => Err(format!(
            "окно сдвинуто извне: после действия сценария рамка {}, сейчас {}; оконный менеджер (например, AeroSpace) выключается на время прогона",
            describe(expected),
            describe(frame)
        )),
        _ => Ok(()),
    }
}

fn describe(frame: NSRect) -> String {
    format!(
        "{:.0},{:.0} {:.0}×{:.0}",
        frame.origin.x, frame.origin.y, frame.size.width, frame.size.height
    )
}

fn steps(stand: &Rc<Stand>, zones: &Shared, other: Option<&Retained<NSScreen>>) -> Vec<Step> {
    let mut steps = Vec::new();
    let (focus, focus_zones) = (Rc::clone(stand), Rc::clone(zones));
    steps.push(Step::new(
        "активность, key window и фокус вида поверхности",
        FOCUS_TIMEOUT,
        move || focused(&focus, &focus_zones.borrow().env.view),
        || Ok(()),
    ));
    let (quiet, quiet_stand, untouched, untouched_stand) = (
        Rc::clone(zones),
        Rc::clone(stand),
        Rc::clone(zones),
        Rc::clone(stand),
    );
    steps.push(Step::new(
        "окно неподвижно",
        FOCUS_TIMEOUT,
        move || window_quiet(&quiet_stand, &mut quiet.borrow_mut()),
        move || window_untouched(&untouched_stand, &mut untouched.borrow_mut()),
    ));
    let waiting = Rc::clone(zones);
    steps.push(Step::new(
        "нулевой размер вида: создание ждёт",
        SIZE_TIMEOUT,
        || Poll::Ready(()),
        move || creation_waits(&mut waiting.borrow_mut()),
    ));
    let (sized, creating) = (Rc::clone(zones), Rc::clone(zones));
    steps.push(Step::new(
        "ненулевой размер вида в пикселях",
        SIZE_TIMEOUT,
        move || {
            let pixels = sized.borrow().env.view.pixel_size();
            poll(pixels.0 > 0 && pixels.1 > 0, || {
                format!("размер вида в пикселях {pixels:?}")
            })
        },
        move || {
            let id = creating.borrow().env.id;
            match runtime::create(id)? {
                Created::Live { key, new_ms } => {
                    events::note(format!("поверхность {key} создана за {new_ms:.1} мс"));
                    Ok(())
                }
                Created::Failed { key, failure, .. } => Err(format!(
                    "ghostty_surface_new вернул NULL для {key}: {failure:?}"
                )),
                Created::Waiting(decision) => {
                    Err(format!("реестр не разрешил создание: {decision:?}"))
                }
            }
        },
    ));
    let (ready, baseline, baseline_stand) = (Rc::clone(zones), Rc::clone(zones), Rc::clone(stand));
    steps.push(Step::new(
        "готовность фикстуры в поверхности",
        READY_TIMEOUT,
        move || surface_env::ready(&mut ready.borrow_mut().env),
        move || {
            let mut zones = baseline.borrow_mut();
            zones.baseline = Some(identity(&mut zones)?);
            zones.home_frame = Some(baseline_stand.window.frame());
            Ok(())
        },
    ));
    change(
        &mut steps,
        stand,
        zones,
        "размер ребёнка после создания",
        false,
        |_, _| Ok(()),
    );
    let (starting, streaming, acked) = (Rc::clone(zones), Rc::clone(zones), Rc::clone(zones));
    steps.push(Step::new(
        "поток вывода фикстуры",
        STREAM_TIMEOUT,
        || Poll::Ready(()),
        move || {
            let mut zones = starting.borrow_mut();
            let nonce = zones.env.fixture_nonce.clone();
            zones.stream = Some(zones.env.hub.send(&nonce, STREAM)?);
            Ok(())
        },
    ));
    steps.push(Step::new(
        "первая квитанция хода потока",
        STREAM_TIMEOUT,
        move || {
            let mut zones = streaming.borrow_mut();
            let progress = stream_state(&mut zones);
            poll(progress.0 > 0, || {
                format!("ход потока {progress:?}; {}", zones.env.hub.describe())
            })
        },
        move || {
            let mut zones = acked.borrow_mut();
            let (nonce, id) = (zones.env.fixture_nonce.clone(), zones.stream);
            let ack = id.and_then(|id| zones.env.hub.by_nonce(&nonce)?.ack(id));
            match ack {
                Some((_, None)) => Ok(()),
                other => Err(format!("команда потока: ack {other:?}")),
            }
        },
    ));

    // Проход по четырём зонам и обратно, с частями разделённых зон.
    change(
        &mut steps,
        stand,
        zones,
        "перенос в левую зону через нулевой размер",
        false,
        |stand, zones| move_to(stand, zones, Zone::Left, 0, true),
    );
    change(
        &mut steps,
        stand,
        zones,
        "перенос в правую зону",
        false,
        |stand, zones| move_to(stand, zones, Zone::Right, 0, false),
    );
    change(
        &mut steps,
        stand,
        zones,
        "деление правой зоны и перенос во вторую часть",
        false,
        |stand, zones| {
            let part = zones.scene.split(Zone::Right);
            move_to(stand, zones, Zone::Right, part, false)
        },
    );
    change(
        &mut steps,
        stand,
        zones,
        "перенос в нижнюю зону",
        false,
        |stand, zones| {
            move_to(stand, zones, Zone::Bottom, 0, false)?;
            zones.scene.unsplit(Zone::Right, 1)
        },
    );
    change(
        &mut steps,
        stand,
        zones,
        "деление нижней зоны и перенос во вторую часть",
        false,
        |stand, zones| {
            let part = zones.scene.split(Zone::Bottom);
            move_to(stand, zones, Zone::Bottom, part, false)
        },
    );
    change(
        &mut steps,
        stand,
        zones,
        "возврат в центральную зону",
        false,
        |stand, zones| {
            move_to(stand, zones, Zone::Center, 0, false)?;
            zones.scene.unsplit(Zone::Bottom, 1)
        },
    );

    // Делители и окно под выводом, поверхность в центре.
    for (name, divider, shift) in [
        (
            "делитель левой и центральной зон",
            Divider::LeftCenter,
            DIVIDER_SHIFT,
        ),
        (
            "делитель центральной и нижней зон",
            Divider::CenterBottom,
            -DIVIDER_SHIFT,
        ),
        (
            "возврат делителя центральной и нижней зон",
            Divider::CenterBottom,
            DIVIDER_SHIFT,
        ),
        (
            "возврат делителя левой и центральной зон",
            Divider::LeftCenter,
            -DIVIDER_SHIFT,
        ),
    ] {
        change(&mut steps, stand, zones, name, false, move |_, zones| {
            let position = zones.scene.divider(divider);
            zones.scene.set_divider(divider, position + shift);
            events::note(format!(
                "делитель {} {position:.1} → {:.1}",
                divider.name(),
                position + shift
            ));
            Ok(())
        });
    }
    change(
        &mut steps,
        stand,
        zones,
        "рост окна",
        false,
        |stand, _| resize_window(stand, true),
    );
    change(
        &mut steps,
        stand,
        zones,
        "возврат окна к исходному размеру",
        false,
        |stand, zones| {
            let home = zones.home_frame.ok_or("нет исходной рамки окна")?;
            stand.window.setFrame_display(home, true);
            Ok(())
        },
    );

    // Смена масштаба переносом окна на экран с другим масштабом.
    match other {
        Some(screen) => {
            let target = screen.clone();
            change(
                &mut steps,
                stand,
                zones,
                SCALE_LABEL,
                true,
                move |stand, zones| {
                    zones.scale = Some(scale_record(stand, Some(target.backingScaleFactor())));
                    let frame = centered(stand.window.frame().size, target.visibleFrame());
                    stand.window.setFrame_display(frame, true);
                    Ok(())
                },
            );
            change(
                &mut steps,
                stand,
                zones,
                "возврат окна на исходный экран",
                true,
                |stand, zones| {
                    let home = zones.home_frame.ok_or("нет исходной рамки окна")?;
                    stand.window.setFrame_display(home, true);
                    Ok(())
                },
            );
        }
        None => match one_x_substitute(stand) {
            Some((id, home)) => {
                // Второго экрана нет: тот же встроенный дисплей в режиме 1x с теми же пикселями
                // на время процесса (`display.rs`), затем возврат прежнего режима.
                change(
                    &mut steps,
                    stand,
                    zones,
                    SCALE_LABEL,
                    true,
                    move |stand, zones| {
                        zones.scale = Some(scale_record(stand, Some(1.0)));
                        zones.scale_substitute = true;
                        crate::display::set(id, |mode| one_x_of(mode, &home)).map(|_| ())
                    },
                );
                change(
                    &mut steps,
                    stand,
                    zones,
                    "возврат прежнего режима дисплея",
                    true,
                    move |_, _| crate::display::set(id, |mode| *mode == home).map(|_| ()),
                );
            }
            None => {
                let recording = Rc::clone(zones);
                let recording_stand = Rc::clone(stand);
                steps.push(Step::new(
                    "другого экрана нет",
                    SIZE_TIMEOUT,
                    || Poll::Ready(()),
                    move || {
                        recording.borrow_mut().scale = Some(scale_record(&recording_stand, None));
                        Ok(())
                    },
                ));
            }
        },
    }

    // Живой ресайз окна перетаскиванием угла: режим отслеживания.
    let (pressing, pressing_stand) = (Rc::clone(zones), Rc::clone(stand));
    steps.push(Step::new(
        "нажатие в правом нижнем углу окна",
        SETTLE_TIMEOUT,
        || Poll::Ready(()),
        move || {
            let mut zones = pressing.borrow_mut();
            guard_frame(&pressing_stand, &zones)?;
            begin(&mut zones, "живой ресайз окна", false);
            press_corner(&pressing_stand, &mut zones)
        },
    ));
    let (dragging, dragging_stand) = (Rc::clone(zones), Rc::clone(stand));
    steps.push(Step::new(
        "перетаскивание угла окна",
        DRAG_TIMEOUT,
        move || drive_drag(&dragging_stand, &mut dragging.borrow_mut()),
        || Ok(()),
    ));
    let (settling, recording, recording_stand) =
        (Rc::clone(zones), Rc::clone(zones), Rc::clone(stand));
    steps.push(Step::new(
        "размер ребёнка после живого ресайза",
        SETTLE_TIMEOUT,
        move || settle(&mut settling.borrow_mut()),
        move || checkpoint(&recording_stand, &mut recording.borrow_mut()),
    ));
    change(
        &mut steps,
        stand,
        zones,
        "возврат окна после живого ресайза",
        false,
        |stand, zones| {
            let home = zones.home_frame.ok_or("нет исходной рамки окна")?;
            stand.window.setFrame_display(home, true);
            Ok(())
        },
    );

    // Остановка потока, итоговая идентичность и закрытие.
    let (stopping, stopped) = (Rc::clone(zones), Rc::clone(zones));
    steps.push(Step::new(
        "остановка потока",
        STREAM_TIMEOUT,
        || Poll::Ready(()),
        move || {
            let mut zones = stopping.borrow_mut();
            let nonce = zones.env.fixture_nonce.clone();
            zones.stream_stop = Some(zones.env.hub.send(&nonce, Command::StopStream)?);
            Ok(())
        },
    ));
    let final_zones = Rc::clone(zones);
    steps.push(Step::new(
        "конец потока",
        STREAM_TIMEOUT,
        move || {
            let mut zones = stopped.borrow_mut();
            let state = stream_state(&mut zones);
            poll(state.1, || format!("ход потока {state:?}"))
        },
        move || {
            let mut zones = final_zones.borrow_mut();
            zones.final_identity = Some(identity(&mut zones)?);
            collect_stream(&mut zones);
            Ok(())
        },
    ));
    let (closing, gone) = (Rc::clone(zones), Rc::clone(zones));
    steps.push(Step::new(
        "два намерения закрыть",
        CLOSE_TIMEOUT,
        || Poll::Ready(()),
        move || {
            let id = closing.borrow().env.id;
            let first = runtime::request_close(id)?;
            let second = runtime::request_close(id)
                .map(|key| key.to_string())
                .unwrap_or_else(|reason| format!("отклонено: {reason}"));
            events::note(format!("намерения закрыть: {first}, {second}"));
            Ok(())
        },
    ));
    steps.push(Step::new(
        "освобождение поверхности и выход фикстуры",
        CLOSE_TIMEOUT,
        move || surface_env::freed(&mut gone.borrow_mut().env),
        || Ok(()),
    ));
    steps
}

fn focused(stand: &Stand, view: &SurfaceView) -> Poll<()> {
    let key_is_main = stand
        .app
        .keyWindow()
        .as_deref()
        .is_some_and(|key| std::ptr::eq(key, &*stand.window));
    poll(
        stand.app.isActive() && key_is_main && is_responder(stand, view),
        || events::focus_state(&stand.app),
    )
}

fn is_responder(stand: &Stand, view: &SurfaceView) -> bool {
    stand
        .window
        .firstResponder()
        .as_deref()
        .is_some_and(|responder| std::ptr::addr_eq(responder, view))
}

/// Вид без размера: реестр ждёт ненулевого размера и поверхность не создаёт. Затем плитка
/// возвращает виду размер тела.
fn creation_waits(zones: &mut Zones) -> Result<(), String> {
    let view = zones.env.view.clone();
    view.setFrameSize(NSSize::ZERO);
    let result = runtime::create(zones.env.id)?;
    zones.creation_wait = Some(format!(
        "{result:?} при размере в пикселях {:?}",
        view.pixel_size()
    ));
    let tile = zones
        .scene
        .tile(Zone::Center, 0)
        .ok_or("нет плитки центральной зоны")?;
    view.setFrame(tile.content().bounds());
    match result {
        Created::Waiting(Decision::Wait) => Ok(()),
        other => Err(format!(
            "при нулевом размере реестр ответил {other:?}, а не ожидание"
        )),
    }
}

fn identity(zones: &mut Zones) -> Result<Identity, String> {
    let view = zones.env.view.clone();
    let located = zones.scene.locate(&view);
    let shown_view = located
        .and_then(|(zone, part)| zones.scene.tile(zone, part))
        .and_then(|tile| tile.active_view())
        .map(|shown| Retained::as_ptr(&shown) as usize);
    let (key, handle) = runtime::current(zones.env.id).ok_or("поверхность не живая")?;
    let reading = runtime::read(zones.env.id).ok_or("поверхность не читается")?;
    zones.env.hub.poll();
    let ready_connections = zones.env.hub.ready_count();
    let nonce = zones.env.fixture_nonce.clone();
    let conn = zones
        .env
        .hub
        .by_nonce(&nonce)
        .ok_or("нет соединения фикстуры")?;
    let ready = conn.ready.clone().ok_or("нет готовности фикстуры")?;
    let views = counters::snapshot()
        .into_iter()
        .find(|count| count.name == SURFACE_VIEW_CLASS);
    Ok(Identity {
        view: Retained::as_ptr(&view) as usize,
        shown_view,
        zone: located.map(|(zone, _)| zone),
        part: located.map(|(_, part)| part),
        handle: handle.addr(),
        key: key.to_string(),
        generation: key.generation,
        pid: ready.pid,
        tty: ready.tty,
        nonce: ready.nonce,
        conn: conn.id,
        foreground_pid: reading.foreground_pid,
        tty_name: reading.tty_name,
        ready_connections,
        surface_views_created: views.map_or(0, |count| count.created),
        surface_views_live: views.map_or(0, |count| count.live()),
    })
}

/// Число квитанций `winch` и размер окна PTY у ребёнка: последняя квитанция `winch`, иначе
/// размер из готовности.
fn child_size(zones: &mut Zones) -> (usize, Option<Winsize>, usize) {
    let nonce = zones.env.fixture_nonce.clone();
    let Some(conn) = zones.env.hub.by_nonce(&nonce) else {
        return (0, None, 0);
    };
    let mut winches = 0;
    let mut last = conn.ready.as_ref().and_then(|ready| ready.size);
    for receipt in conn.winches() {
        winches += 1;
        if let Event::Winch { size } = &receipt.event {
            last = *size;
        }
    }
    (winches, last, conn.receipts.len())
}

/// Строки потока по последней квитанции хода и признак конца потока.
fn stream_state(zones: &mut Zones) -> (u64, bool) {
    zones.env.hub.poll();
    let nonce = zones.env.fixture_nonce.clone();
    let Some(id) = zones.stream else {
        return (0, false);
    };
    let Some(conn) = zones.env.hub.by_nonce(&nonce) else {
        return (0, false);
    };
    let mut lines = 0;
    let mut done = false;
    for receipt in &conn.receipts {
        match receipt.event {
            Event::StreamProgress {
                id: of, lines: n, ..
            } if of == id => lines = n,
            Event::StreamDone {
                id: of, lines: n, ..
            } if of == id => {
                lines = n;
                done = true;
            }
            _ => {}
        }
    }
    (lines, done)
}

/// Отметки квитанций хода потока и его конца для вывода «под выводом».
fn collect_stream(zones: &mut Zones) {
    let nonce = zones.env.fixture_nonce.clone();
    let Some(id) = zones.stream else {
        return;
    };
    let Some(conn) = zones.env.hub.by_nonce(&nonce) else {
        return;
    };
    let mut progress = Vec::new();
    let mut done = None;
    for receipt in &conn.receipts {
        match receipt.event {
            Event::StreamProgress { id: of, .. } if of == id => progress.push(receipt.t_ns),
            Event::StreamDone { id: of, .. } if of == id => done = Some(receipt.t_ns),
            _ => {}
        }
    }
    zones.progress_t = progress;
    zones.done_t = done;
}

fn grid(zones: &Zones) -> Option<(u16, u16)> {
    runtime::read(zones.env.id).map(|reading| (reading.size.rows, reading.size.columns))
}

fn begin(zones: &mut Zones, label: &str, expect_scale: bool) {
    zones.env.hub.poll();
    let (winches_before, _, _) = child_size(zones);
    zones.pending = Some(Pending {
        label: label.to_owned(),
        started: Instant::now(),
        t_begin_ns: clock::uptime_ns(),
        winches_before,
        grid_before: grid(zones),
        pixels_before: zones.env.view.pixel_size(),
        scale_before: zones.env.view.backing_scale(),
        expect_scale,
    });
}

/// Готово, когда геометрия вида изменилась, как ожидало действие, сетка поверхности совпала с
/// `TIOCGWINSZ` ребёнка, и при смене сетки после действия пришла новая квитанция `winch`. По
/// пределу [`SETTLE_LIMIT`] шаг тоже готов: расхождение записывает контрольная точка.
fn settle(zones: &mut Zones) -> Poll<()> {
    zones.env.hub.poll();
    let Some(pending) = &zones.pending else {
        return Poll::Ready(());
    };
    let (started, winches_before, grid_before) =
        (pending.started, pending.winches_before, pending.grid_before);
    let changed = geometry_changed(zones);
    let now = grid(zones);
    let (winches, child, _) = child_size(zones);
    let matches = now.is_some() && child.map(|size| (size.rows, size.cols)) == now;
    let fresh = now == grid_before || winches > winches_before;
    if (changed && matches && fresh) || started.elapsed() >= SETTLE_LIMIT {
        return Poll::Ready(());
    }
    Poll::Pending(format!(
        "геометрия изменилась {changed}, сетка {now:?}, у ребёнка {child:?}, winch {winches} (до {winches_before})"
    ))
}

fn geometry_changed(zones: &Zones) -> bool {
    let Some(pending) = &zones.pending else {
        return true;
    };
    if pending.label == "размер ребёнка после создания" {
        return true;
    }
    let view = &zones.env.view;
    if pending.expect_scale {
        (view.backing_scale() - pending.scale_before).abs() > f64::EPSILON
    } else {
        view.pixel_size() != pending.pixels_before
    }
}

fn checkpoint(stand: &Stand, zones: &mut Zones) -> Result<(), String> {
    zones.env.hub.poll();
    let t_end_ns = clock::uptime_ns();
    let changed = geometry_changed(zones);
    let Some(pending) = zones.pending.take() else {
        return Err("контрольная точка без действия".to_owned());
    };
    let reading = runtime::read(zones.env.id);
    let surface = reading.as_ref().map(|reading| reading.size);
    let (winches, winsize, _) = child_size(zones);
    let winsize_matches = match (surface, winsize) {
        (Some(surface), Some(child)) => surface.rows == child.rows && surface.columns == child.cols,
        _ => false,
    };
    let differences = match (identity(zones), &zones.baseline) {
        (Ok(now), Some(base)) => now.differences(base),
        (Ok(_), None) => Vec::new(),
        (Err(reason), _) => vec![reason],
    };
    let located = zones.scene.locate(&zones.env.view);
    let (stream_lines, stream_done) = stream_state(zones);
    let view = zones.env.view.clone();
    let feeds = view.feeds();
    let stats = runtime::stats().unwrap_or_default();
    let elapsed = pending.started.elapsed();
    let point = Checkpoint {
        label: pending.label,
        t_begin_ns: pending.t_begin_ns,
        t_end_ns,
        window_frame: rect4(stand.window.frame()),
        zone: located.map(|(zone, _)| zone),
        part: located.map(|(_, part)| part),
        view_points: (view.bounds().size.width, view.bounds().size.height),
        view_pixels: view.pixel_size(),
        scale: view.backing_scale(),
        surface,
        winsize,
        winches,
        winsize_matches,
        geometry_changed: changed,
        settled_ms: elapsed.as_secs_f64() * 1e3,
        settle_limit_reached: elapsed >= SETTLE_LIMIT,
        first_responder: is_responder(stand, &view),
        last_focus_feed: feeds.focus.last().copied(),
        identity_differences: differences,
        surfaces_created: stats.surfaces_created,
        surfaces_freed: stats.surfaces_freed,
        passes: stats.passes,
        stream_lines,
        stream_done,
        zero_size_skipped: feeds.zero_size_skipped,
        detached_skipped: feeds.detached_skipped,
    };
    events::note(format!(
        "контрольная точка «{}»: {:?}/{:?}, сетка {:?}, у ребёнка {:?}, совпадает {}",
        point.label,
        point.zone,
        point.part,
        point.surface.map(|size| (size.rows, size.columns)),
        point.winsize.map(|size| (size.rows, size.cols)),
        point.winsize_matches
    ));
    zones.checkpoints.push(point);
    guard_frame(stand, zones)
}

/// Перенос вида в часть зоны сменой родителя. С `zero` вид между родителями получает нулевой
/// размер, и создание при нём проверяется вызовом реестра.
fn move_to(
    stand: &Stand,
    zones: &mut Zones,
    zone: Zone,
    part: usize,
    zero: bool,
) -> Result<(), String> {
    let view = zones.env.view.clone();
    let responder_before = is_responder(stand, &view);
    let feeds_before = view.feeds().focus.len();
    let title = zones.scene.take_tab(&view)?;
    let zero = if zero {
        let zero_skipped_before = view.feeds().zero_size_skipped;
        view.setFrameSize(NSSize::ZERO);
        let create = runtime::create(zones.env.id)?;
        let stats = runtime::stats().unwrap_or_default();
        Some(ZeroCheck {
            pixels: view.pixel_size(),
            zero_skipped_before,
            zero_skipped_after: view.feeds().zero_size_skipped,
            create: format!("{create:?}"),
            phase: runtime::phase(zones.env.id).map(Phase::name),
            created: stats.surfaces_created,
            freed: stats.surfaces_freed,
        })
    } else {
        None
    };
    zones.scene.add_tab(zone, part, &title, &view)?;
    let kept_by_appkit = is_responder(stand, &view);
    let restored = !kept_by_appkit;
    if restored && !zones.scene.focus(&stand.window, &view) {
        return Err("окно не приняло вид поверхности first responder после переноса".to_owned());
    }
    zones.scene.sync_focus(&stand.window);
    let focus_feeds_during = view.feeds().focus[feeds_before..].to_vec();
    zones.moves.push(MoveRecord {
        to: zone,
        part,
        responder_before,
        kept_by_appkit,
        restored,
        located: zones.scene.locate(&view),
        zero,
        focus_feeds_during,
    });
    Ok(())
}

/// Окно растёт на [`GROW`] вниз и вправо от левого верхнего угла в пределах видимой области.
fn resize_window(stand: &Stand, grow: bool) -> Result<(), String> {
    let screen = stand.window.screen().ok_or("окно не на экране")?;
    let visible = screen.visibleFrame();
    let frame = stand.window.frame();
    let (dw, dh) = if grow {
        (GROW.width, GROW.height)
    } else {
        (0.0, 0.0)
    };
    let width = (frame.size.width + dw).min(visible.size.width);
    let height = (frame.size.height + dh).min(visible.size.height);
    let top = frame.origin.y + frame.size.height;
    let x = frame
        .origin
        .x
        .min(visible.origin.x + visible.size.width - width)
        .max(visible.origin.x);
    let y = (top - height).max(visible.origin.y);
    let target = NSRect::new(NSPoint::new(x, y), NSSize::new(width, height));
    events::note(format!(
        "окно {:.0}×{:.0} → {width:.0}×{height:.0}",
        frame.size.width, frame.size.height
    ));
    stand.window.setFrame_display(target, true);
    Ok(())
}

fn centered(size: NSSize, area: NSRect) -> NSRect {
    NSRect::new(
        NSPoint::new(
            (area.origin.x + (area.size.width - size.width) / 2.0).floor(),
            (area.origin.y + (area.size.height - size.height) / 2.0).floor(),
        ),
        size,
    )
}

fn scale_record(stand: &Stand, to_scale: Option<f64>) -> ScaleRecord {
    ScaleRecord {
        screens: NSScreen::screens(stand.view.mtm())
            .iter()
            .map(|screen| (screen.CGDirectDisplayID(), screen.backingScaleFactor()))
            .collect(),
        from_scale: stand.window.backingScaleFactor(),
        to_scale,
    }
}

fn rect4(frame: NSRect) -> [f64; 4] {
    [
        frame.origin.x,
        frame.origin.y,
        frame.size.width,
        frame.size.height,
    ]
}

/// Нажатие левой кнопки в правом нижнем углу окна; дальше перетаскивание ведёт [`drive_drag`].
fn press_corner(stand: &Stand, zones: &mut Zones) -> Result<(), String> {
    let frame = stand.window.frame();
    let corner = NSPoint::new(
        frame.origin.x + frame.size.width - CORNER_INSET,
        frame.origin.y + CORNER_INSET,
    );
    let (winches, _, receipts) = child_size(zones);
    let stats = runtime::stats().unwrap_or_default();
    zones.drag = Drag {
        corner: (corner.x, corner.y),
        frame_before: rect4(frame),
        passes_tracking_before: stats.passes_tracking,
        started: Some(Instant::now()),
        last_post: Some(Instant::now()),
        last_receipts: receipts,
        last_winches: winches,
        ..Drag::default()
    };
    events::post_mouse(
        NSEventType::LeftMouseDown,
        &stand.window,
        corner,
        "mouseDown угол окна",
    )?;
    Ok(())
}

/// Вызывается таймером стенда в общих режимах цикла, в том числе внутри цикла отслеживания
/// AppKit: по одному событию перетаскивания на интервал, затем отпускание, и выборка режима
/// цикла, `inLiveResize`, проходов runtime и квитанций фикстуры.
fn drive_drag(stand: &Stand, zones: &mut Zones) -> Poll<()> {
    zones.env.hub.poll();
    let (winches, _, receipts) = child_size(zones);
    let tracking = runtime::in_tracking_mode();
    let live = stand.window.inLiveResize();
    let drag = &mut zones.drag;
    drag.samples += 1;
    if tracking {
        drag.samples_tracking += 1;
        drag.receipts_in_tracking += receipts.saturating_sub(drag.last_receipts);
        drag.winches_in_tracking += winches.saturating_sub(drag.last_winches);
    }
    drag.last_receipts = receipts;
    drag.last_winches = winches;
    if live {
        drag.samples_live_resize += 1;
    }
    if drag.up_posted {
        if !live && !tracking {
            let frame = stand.window.frame();
            drag.frame_after = rect4(frame);
            drag.passes_tracking_after = runtime::stats().map_or(0, |stats| stats.passes_tracking);
            zones.expected_frame = Some(frame);
            return Poll::Ready(());
        }
        return Poll::Pending(format!(
            "кнопка отпущена, inLiveResize {live}, режим отслеживания {tracking}"
        ));
    }
    let due = drag
        .last_post
        .is_none_or(|last| last.elapsed() >= DRAG_INTERVAL);
    let overdue = drag
        .started
        .is_some_and(|started| started.elapsed() >= DRAG_LIMIT);
    if !due && !overdue {
        return Poll::Pending(format!("перетаскивание {}", drag.posted_drags));
    }
    let shift = f64::from(drag.posted_drags + 1) * DRAG_STEP;
    let point = NSPoint::new(drag.corner.0 + shift, drag.corner.1 - shift * 0.6);
    let posted = if drag.posted_drags < DRAG_STEPS && !overdue {
        drag.posted_drags += 1;
        events::post_mouse(
            NSEventType::LeftMouseDragged,
            &stand.window,
            point,
            "mouseDragged угол окна",
        )
    } else {
        drag.up_posted = true;
        drag.up_forced = overdue;
        events::post_mouse(
            NSEventType::LeftMouseUp,
            &stand.window,
            point,
            "mouseUp угол окна",
        )
    };
    drag.last_post = Some(Instant::now());
    match posted {
        Ok(_) => Poll::Pending(format!("перетаскивание {}", drag.posted_drags)),
        Err(reason) => {
            drag.up_posted = true;
            Poll::Pending(format!("событие не отправлено: {reason}"))
        }
    }
}

/// Выводы H5 по контрольным точкам, переносам и перетаскиванию.
fn judge(zones: &Zones, report: &mut Report) {
    let verdict = |ok: bool| if ok { Verdict::Pass } else { Verdict::Fail };
    let points = &zones.checkpoints;
    let changed: Vec<&Checkpoint> = points.iter().skip(1).collect();

    let identity_broken: Vec<String> = points
        .iter()
        .filter(|point| !point.identity_differences.is_empty())
        .map(|point| {
            format!(
                "«{}»: {}",
                point.label,
                point.identity_differences.join(", ")
            )
        })
        .collect();
    let final_differences = match (&zones.final_identity, &zones.baseline) {
        (Some(now), Some(base)) => now.differences(base),
        _ => vec!["нет итоговой или исходной идентичности".to_owned()],
    };
    let base = zones.baseline.as_ref();
    report.finding(
        "h5-identity",
        Requirement::Required,
        verdict(identity_broken.is_empty() && final_differences.is_empty()),
        format!(
            "{} контрольных точек; NSView {}, ручка {}, {}, PID {}, TTY {:?}, nonce {}; расхождения: {}",
            points.len(),
            base.map_or("?".to_owned(), |base| format!("{:#x}", base.view)),
            base.map_or("?".to_owned(), |base| format!("{:#x}", base.handle)),
            base.map_or("?", |base| base.key.as_str()),
            base.map_or(0, |base| base.pid),
            base.and_then(|base| base.tty.clone()),
            base.map_or("?", |base| base.nonce.as_str()),
            if identity_broken.is_empty() && final_differences.is_empty() {
                "нет".to_owned()
            } else {
                [identity_broken, final_differences].concat().join("; ")
            }
        ),
    );

    let mismatched: Vec<String> = points
        .iter()
        .filter(|point| !point.winsize_matches)
        .map(|point| {
            format!(
                "«{}»: сетка {:?}, TIOCGWINSZ {:?}",
                point.label,
                point.surface.map(|size| (size.rows, size.columns)),
                point.winsize.map(|size| (size.rows, size.cols))
            )
        })
        .collect();
    report.finding(
        "h5-winsize",
        Requirement::Required,
        verdict(!points.is_empty() && mismatched.is_empty()),
        format!(
            "TIOCGWINSZ ребёнка совпадает с ghostty_surface_size в {} из {} контрольных точек; квитанций winch {}; расхождения: {}",
            points.len() - mismatched.len(),
            points.len(),
            points.last().map_or(0, |point| point.winches),
            if mismatched.is_empty() {
                "нет".to_owned()
            } else {
                mismatched.join("; ")
            }
        ),
    );

    let unchanged: Vec<&str> = changed
        .iter()
        .filter(|point| !point.geometry_changed)
        .map(|point| point.label.as_str())
        .collect();
    report.finding(
        "h5-geometry-applied",
        Requirement::Required,
        verdict(unchanged.is_empty()),
        format!(
            "размер вида менялся после {} из {} изменений; без изменения: {:?}",
            changed.len() - unchanged.len(),
            changed.len(),
            unchanged
        ),
    );

    let counts_ok = points
        .iter()
        .all(|point| point.surfaces_created == 1 && point.surfaces_freed == 0);
    let zero = zones.moves.iter().find_map(|record| record.zero.as_ref());
    let zero_ok = zero.is_some_and(|zero| {
        zero.pixels == (0, 0)
            && zero.zero_skipped_after > zero.zero_skipped_before
            && zero.create == format!("{:?}", Created::Waiting(Decision::Exists(Phase::Live)))
            && zero.created == 1
            && zero.freed == 0
    });
    let wait_ok = zones
        .creation_wait
        .as_deref()
        .is_some_and(|text| text.starts_with(&format!("{:?}", Created::Waiting(Decision::Wait))));
    report.finding(
        "h5-create-free-counts",
        Requirement::Required,
        verdict(counts_ok && zero_ok && wait_ok),
        format!(
            "до закрытия во всех точках создано 1 и освобождено 0: {counts_ok}; до создания при нулевом размере: {:?}; при нулевых промежуточных границах: {:?}",
            zones.creation_wait, zero
        ),
    );

    let unfocused: Vec<&str> = points
        .iter()
        .filter(|point| !point.first_responder || point.last_focus_feed != Some(true))
        .map(|point| point.label.as_str())
        .collect();
    let restored = zones.moves.iter().filter(|record| record.restored).count();
    let flaps: Vec<Vec<bool>> = zones
        .moves
        .iter()
        .map(|record| record.focus_feeds_during.clone())
        .filter(|feeds| !feeds.is_empty())
        .collect();
    report.finding(
        "h5-focus-kept",
        Requirement::Required,
        verdict(unfocused.is_empty()),
        format!(
            "вид поверхности first responder и последняя подача фокуса true во всех точках, кроме {unfocused:?}; сцена восстанавливала фокус после {restored} из {} переносов",
            zones.moves.len()
        ),
    );
    report.push_json("focus_feeds_during_moves", &flaps);

    let flowing: Vec<&&Checkpoint> = changed
        .iter()
        .filter(|point| point.label != "размер ребёнка после создания")
        .collect();
    let stalled: Vec<String> = flowing
        .iter()
        .filter(|point| {
            let before = zones.progress_t.iter().any(|t| *t <= point.t_begin_ns);
            // Квитанция конца потока пишется после последней пачки строк: поток шёл и после
            // изменения, если она позже его конца.
            let after = zones.progress_t.iter().any(|t| *t >= point.t_end_ns)
                || zones.done_t.is_some_and(|done| done >= point.t_end_ns);
            let running = zones.done_t.is_none_or(|done| done > point.t_end_ns);
            !(before && after && running)
        })
        .map(|point| point.label.clone())
        .collect();
    let inside: Vec<usize> = flowing
        .iter()
        .map(|point| {
            zones
                .progress_t
                .iter()
                .filter(|t| (point.t_begin_ns..=point.t_end_ns).contains(*t))
                .count()
        })
        .collect();
    report.finding(
        "h5-under-output",
        Requirement::Required,
        verdict(!flowing.is_empty() && stalled.is_empty()),
        format!(
            "поток фикстуры шёл до и после {} из {} изменений геометрии (квитанция хода до начала, квитанция хода или конца потока после конца изменения); квитанций хода внутри изменений {inside:?}; без потока: {stalled:?}",
            flowing.len() - stalled.len(),
            flowing.len()
        ),
    );

    match &zones.scale {
        Some(ScaleRecord {
            to_scale: Some(to), ..
        }) => {
            let scaled = points
                .iter()
                .find(|point| point.label == SCALE_LABEL);
            let ok = scaled.is_some_and(|point| {
                point.geometry_changed
                    && point.winsize_matches
                    && (point.scale - to).abs() < f64::EPSILON
            });
            report.finding(
                "h5-scale-change",
                Requirement::Required,
                verdict(ok),
                format!(
                    "смена масштаба окна на {to} ({}): {scaled:?}",
                    if zones.scale_substitute {
                        "замена второго экрана: режим 1x встроенного дисплея на время процесса"
                    } else {
                        "перенос окна на экран с другим масштабом"
                    }
                ),
            );
        }
        Some(record) => report.finding(
            "h5-scale-change",
            Requirement::Required,
            Verdict::PendingManual,
            format!(
                "экран с другим масштабом не подключён (экраны {:?}, масштаб окна {}); смена масштаба проверяется вручную в задаче 19 переносом окна между дисплеями",
                record.screens, record.from_scale
            ),
        ),
        None => report.finding(
            "h5-scale-change",
            Requirement::Required,
            Verdict::Fail,
            "шаг масштаба не выполнен",
        ),
    }

    let drag = &zones.drag;
    let passes_tracking = drag
        .passes_tracking_after
        .saturating_sub(drag.passes_tracking_before);
    let resized = drag.frame_after[2] > drag.frame_before[2];
    let served = drag.samples_live_resize > 0
        && drag.samples_tracking > 0
        && passes_tracking > 0
        && drag.receipts_in_tracking > 0;
    report.finding(
        "h5-main-queue-in-tracking",
        Requirement::Required,
        verdict(served && resized),
        format!(
            "живой ресайз перетаскиванием угла: выборок {} (в режиме отслеживания {}, inLiveResize {}), проходов runtime в режиме отслеживания {passes_tracking}, квитанций фикстуры {} (winch {}), окно {:.0}×{:.0} → {:.0}×{:.0}, отпускание по сроку {}",
            drag.samples,
            drag.samples_tracking,
            drag.samples_live_resize,
            drag.receipts_in_tracking,
            drag.winches_in_tracking,
            drag.frame_before[2],
            drag.frame_before[3],
            drag.frame_after[2],
            drag.frame_after[3],
            drag.up_forced
        ),
    );
    report.metric(
        "settle",
        "ms",
        points.iter().map(|point| point.settled_ms).collect(),
    );
}
