//! Сценарии `startup` (прямой маршрут) и `startup-ls` (маршрут LaunchServices): от запуска процесса
//! до обработанной контрольной клавиши, по определению P0.2. Начало — `p_starttime` процесса,
//! конец — `gettimeofday` в момент, когда клавиша, отправленная после первого `display` key
//! window, вернулась из `sendEvent` и её приход прочитан. У варианта `surface` к этому проходу
//! поверхность уже создана: `ghostty_init`, `ghostty_app_new` и `ghostty_surface_new` входят в
//! старт. Готовность фикстуры и первое появление `contents` у слоя поверхности — отдельные
//! отметки, конец их не ждёт.
//!
//! Варианты: `surface` — окно с одной поверхностью и фикстурой; `bare` — базовая линия, то же окно
//! без поверхности и без libghostty; `badlang` (только `startup`) — диагностический запуск с
//! недопустимым непустым `LANG`. Проходы и возвраты `sendEvent` перехватываются сразу, без
//! ожидания таймера: таймер-предикат здесь только ограничивает срок.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;

use super::events::{self, Key};
use super::surface_env::{self, State};
use super::{Poll, Report, Scenario, Stand, Step, run_steps, window};
use crate::args::Route;
use crate::clock::{self, Entry, ProcessStart};
use crate::env;
use crate::ghostty::config::ColorScheme;
use crate::ghostty::init::InitRecord;
use crate::ghostty::runtime;
use crate::results::{ClockCheck, Requirement, StartupMarks, Verdict};
use crate::view::SurfaceView;

/// Вариант с поверхностью.
pub const SURFACE: &str = "surface";
/// Базовая линия: окно без поверхности и без libghostty.
pub const BARE: &str = "bare";

const KEY_TIMEOUT: Duration = Duration::from_secs(10);
const READY_TIMEOUT: Duration = Duration::from_secs(12);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
const CONTROL_KEY: Key = Key::RETURN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Surface,
    Bare,
    InvalidLang,
}

impl Kind {
    fn of(scenario: Scenario, variant: &str) -> Result<Kind, String> {
        match (scenario, variant) {
            (_, SURFACE) => Ok(Kind::Surface),
            (_, BARE) => Ok(Kind::Bare),
            (Scenario::Startup, env::INVALID_LANG_VARIANT) => Ok(Kind::InvalidLang),
            _ => Err(format!(
                "предусловие: вариант {variant} у {}, ожидается {SURFACE}, {BARE}{}",
                scenario.name(),
                if scenario == Scenario::Startup {
                    format!(" или {}", env::INVALID_LANG_VARIANT)
                } else {
                    String::new()
                }
            )),
        }
    }
}

/// Маршрут сценария: `startup` — прямой, `startup-ls` — через LaunchServices.
fn check_route(scenario: Scenario, route: Route) -> Result<(), String> {
    let expected = match scenario {
        Scenario::StartupLs => Route::LaunchServices,
        _ => Route::Direct,
    };
    if route == expected {
        Ok(())
    } else {
        Err(format!(
            "предусловие: {} идёт маршрутом {}, а процесс запущен маршрутом {}",
            scenario.name(),
            expected.name(),
            route.name()
        ))
    }
}

/// Предусловие замеров старта, как в P0.2: окно на встроенном дисплее.
fn require_builtin(stand: &Stand) -> Result<(), String> {
    if stand.display.builtin {
        Ok(())
    } else {
        Err(format!(
            "предусловие: встроенного дисплея нет, окно на «{}»; старт снимается на встроенном дисплее, нужен владелец",
            stand.display.screen.localizedName()
        ))
    }
}

#[derive(Default)]
struct Marks {
    first_display: Option<u64>,
    ready: Option<u64>,
    key_display: Option<u64>,
    posted: Option<u64>,
    returned: Option<u64>,
    numbers: Vec<u64>,
    /// Конец: стенные мкс и монотонные нс.
    end: Option<(i64, u64)>,
    /// Клавиши, дошедшие до вида стенда к возврату `keyDown` из `sendEvent`.
    effect: Option<Vec<(&'static str, u16)>>,
    first_contents: Option<u64>,
    error: Option<String>,
    views_need_display_at_ready: Option<bool>,
}

pub fn run(stand: Rc<Stand>, scenario: Scenario) {
    let mut report = stand.report(scenario);
    let variant = stand.prelude.variant.clone();
    report.push("variant", &variant);
    let kind = match Kind::of(scenario, &variant)
        .and_then(|kind| check_route(scenario, stand.route).map(|()| kind))
        .and_then(|kind| {
            stand
                .check_session(&mut report)
                .and_then(|()| require_builtin(&stand))
                .map(|()| kind)
        }) {
        Ok(kind) => kind,
        Err(reason) => return stand.conclude(report, Err(reason)),
    };
    report.push(
        "interval_startup",
        "p_starttime процесса → gettimeofday после возврата keyDown контрольной клавиши из sendEvent и чтения её прихода; клавиша отправлена в конце первого прохода отображения, на котором окно key, видимо, ничего не ждёт отображения и поверхность уже создана",
    );
    report.push(
        "control_key_route",
        "у варианта surface first responder — вид поверхности; клавишу он не обрабатывает, и она по цепочке ответчиков доходит до вида стенда",
    );
    if kind == Kind::Bare {
        report.push(
            "baseline",
            "то же окно без поверхности и без libghostty: нет ghostty_init, ghostty_app_new, ghostty_surface_new, сокета фикстур и фикстуры; first responder — вид стенда",
        );
    }
    let surface = match kind {
        Kind::Bare => None,
        Kind::Surface | Kind::InvalidLang => {
            match surface_env::prepare(&stand, &mut report, ColorScheme::Light) {
                Ok(state) => Some(Rc::new(RefCell::new(state))),
                Err(reason) => return surface_env::finish(&stand, report, Err(reason)),
            }
        }
    };
    if let Some(state) = &surface {
        let pixels = state.borrow().view.pixel_size();
        let created = if pixels.0 == 0 || pixels.1 == 0 {
            Err(format!(
                "предусловие: размер вида поверхности в пикселях {pixels:?} при открытии окна"
            ))
        } else {
            surface_env::create(state)
        };
        if let Err(reason) = created {
            return surface_env::finish(&stand, report, Err(reason));
        }
    }

    let marks = Rc::new(RefCell::new(Marks::default()));
    hook_key_display(&stand, &marks, surface.as_ref());
    hook_control_return(&stand, &marks);
    if let Some(state) = &surface {
        hook_first_contents(state.borrow().view.clone(), &marks);
    }
    let steps = steps(&marks, surface.as_ref());
    run_steps(steps, move |result| {
        let verdict = match result {
            Ok(()) => conclude(&stand, &mut report, kind, &marks.borrow(), surface.as_ref()),
            Err(failure) => Err(failure.describe()),
        };
        match surface {
            Some(_) => surface_env::finish(&stand, report, verdict),
            None => stand.conclude(report, verdict),
        }
    });
}

/// Отметки проходов и отправка контрольной клавиши в конце первого `display` key window.
fn hook_key_display(
    stand: &Rc<Stand>,
    marks: &Rc<RefCell<Marks>>,
    surface: Option<&Rc<RefCell<State>>>,
) {
    let stand = Rc::clone(stand);
    let marks = Rc::clone(marks);
    let surface = surface.map(Rc::clone);
    events::on_display_pass(move |at| {
        let window = &stand.window;
        let mut marks = marks.borrow_mut();
        if marks.first_display.is_none() && window.isVisible() {
            marks.first_display = Some(at);
        }
        let key_window = stand
            .app
            .keyWindow()
            .as_deref()
            .is_some_and(|key| std::ptr::eq(key, &**window));
        if marks.ready.is_none() && stand.app.isActive() && key_window {
            marks.ready = Some(at);
            marks.views_need_display_at_ready = Some(window.viewsNeedDisplay());
        }
        let created = surface.as_ref().is_none_or(|state| {
            state
                .try_borrow()
                .is_ok_and(|state| state.created_end_ns.is_some())
        });
        if marks.ready.is_none() || !created || !window.isVisible() || window.viewsNeedDisplay() {
            return true;
        }
        marks.key_display = Some(at);
        marks.posted = Some(clock::uptime_ns());
        match events::press(CONTROL_KEY) {
            Ok(numbers) => marks.numbers = numbers,
            Err(reason) => marks.error = Some(reason),
        }
        false
    });
}

/// Конец: возврат `keyDown` контрольной клавиши и её приход к виду стенда.
fn hook_control_return(stand: &Rc<Stand>, marks: &Rc<RefCell<Marks>>) {
    let stand = Rc::clone(stand);
    let marks = Rc::clone(marks);
    events::on_send_return(move |number, at| {
        let mut marks = marks.borrow_mut();
        if marks.numbers.first() != Some(&number) {
            return true;
        }
        let effect = stand
            .view
            .arrivals()
            .iter()
            .map(|arrival| (arrival.kind, arrival.key_code))
            .collect();
        let wall = clock::wall_us();
        let mono = clock::uptime_ns();
        marks.returned = Some(at);
        marks.effect = Some(effect);
        match wall {
            Ok(wall) => marks.end = Some((wall, mono)),
            Err(reason) => marks.error = Some(reason),
        }
        false
    });
}

/// Первый проход, к концу которого у слоя поверхности есть `contents`. libghostty ставит его
/// на главном потоке (`IOSurfaceLayer.zig`), но это ещё не показ кадра.
fn hook_first_contents(view: Retained<SurfaceView>, marks: &Rc<RefCell<Marks>>) {
    let marks = Rc::clone(marks);
    events::on_display_pass(move |at| {
        // SAFETY: чтение свойства слоя на главном потоке.
        let present = view
            .layer()
            .is_some_and(|layer| unsafe { layer.contents() }.is_some());
        if present {
            marks.borrow_mut().first_contents = Some(at);
        }
        !present
    });
}

fn steps(marks: &Rc<RefCell<Marks>>, surface: Option<&Rc<RefCell<State>>>) -> Vec<Step> {
    let handled = Rc::clone(marks);
    let mut steps = vec![Step::new(
        "контрольная клавиша обработана",
        KEY_TIMEOUT,
        move || {
            let marks = handled.borrow();
            let up_returned = marks
                .numbers
                .last()
                .and_then(|number| events::record(*number))
                .is_some_and(|record| record.send_return.is_some());
            if marks.error.is_some() || (marks.end.is_some() && up_returned) {
                Poll::Ready(())
            } else {
                Poll::Pending(format!(
                    "first_display={} ready={} key_display={} posted={} returned={} keyUp вернулся={up_returned}",
                    marks.first_display.is_some(),
                    marks.ready.is_some(),
                    marks.key_display.is_some(),
                    marks.posted.is_some(),
                    marks.returned.is_some()
                ))
            }
        },
        || Ok(()),
    )];
    let Some(state) = surface else {
        return steps;
    };
    let waiting = Rc::clone(state);
    let contents = Rc::clone(marks);
    let closing = Rc::clone(state);
    let gone = Rc::clone(state);
    steps.push(Step::new(
        "готовность фикстуры и первый contents слоя поверхности",
        READY_TIMEOUT,
        move || {
            let fixture = surface_env::ready(&mut waiting.borrow_mut());
            let present = contents.borrow().first_contents.is_some();
            match fixture {
                Poll::Ready(()) if present => Poll::Ready(()),
                Poll::Ready(()) => Poll::Pending("фикстура готова, contents слоя нет".to_owned()),
                Poll::Pending(state) => Poll::Pending(format!("{state}; contents слоя {present}")),
            }
        },
        || Ok(()),
    ));
    steps.push(Step::new(
        "намерение закрыть",
        CLOSE_TIMEOUT,
        || Poll::Ready(()),
        move || {
            let id = closing.borrow().id;
            runtime::request_close(id).map(|_| ())
        },
    ));
    steps.push(Step::new(
        "освобождение поверхности и выход фикстуры",
        CLOSE_TIMEOUT,
        move || surface_env::freed(&mut gone.borrow_mut()),
        || Ok(()),
    ));
    steps
}

/// Отметки старта из отметок стенда, входа в `main`, запуска процесса и поверхности.
fn startup_marks(
    marks: &Marks,
    entry: Entry,
    start: ProcessStart,
    surface: Option<&State>,
) -> Result<StartupMarks, String> {
    if let Some(error) = &marks.error {
        return Err(error.clone());
    }
    let (end_wall_us, end_mono_ns) = marks.end.ok_or("нет отметки конца")?;
    Ok(StartupMarks {
        process_start_wall_us: start.wall_us,
        process_start_bsd_wall_us: start.bsd_wall_us,
        process_start_mono_ns: start.mono_ns,
        main_wall_us: entry.wall_us,
        main_mono_ns: entry.mono_ns,
        ghostty_init_mono_ns: surface.map(|state| state.init.finished_ns),
        app_new_mono_ns: surface.map(|state| state.started.app_new_finished_ns),
        surface_new_mono_ns: surface.and_then(|state| state.created_end_ns),
        fixture_ready_mono_ns: surface.and_then(|state| state.ready_t_ns),
        first_contents_mono_ns: marks.first_contents,
        first_display_mono_ns: marks.first_display,
        ready_mono_ns: marks.ready,
        key_display_mono_ns: marks.key_display,
        control_posted_mono_ns: marks.posted,
        control_returned_mono_ns: marks.returned,
        end_wall_us,
        end_mono_ns,
    })
}

fn conclude(
    stand: &Stand,
    report: &mut Report,
    kind: Kind,
    marks: &Marks,
    surface: Option<&Rc<RefCell<State>>>,
) -> Result<(), String> {
    let entry = clock::main_entry().ok_or("нет отметки входа в main")?;
    let start = clock::process_start()?;
    let surface = surface.map(|state| state.borrow());
    let startup = startup_marks(marks, entry, start, surface.as_deref())?;
    report.startup = Some(startup);
    record_marks(report, &startup, marks);
    report.push(
        "process_start_sources_agree",
        start.bsd_wall_us == Some(start.wall_us),
    );
    let (bundle, problems) = window::verify_bundle();
    report.bundle = Some(bundle);
    if !problems.is_empty() {
        return Err(format!("не бандл пробы: {}", problems.join("; ")));
    }
    if let Some(state) = surface.as_deref() {
        report.push_json("fixture_ready", &state.ready);
        match kind {
            Kind::InvalidLang => invalid_lang_finding(report, &state.init, state),
            Kind::Surface if stand.route == Route::LaunchServices => {
                cocoa_path_finding(report, &state.init, state);
            }
            Kind::Surface | Kind::Bare => {}
        }
    }
    let effect = marks.effect.clone().unwrap_or_default();
    let expected = vec![("keyDown", CONTROL_KEY.code)];
    let effect_ok = effect == expected;
    report.finding(
        "startup-control-key",
        Requirement::Required,
        if effect_ok { Verdict::Pass } else { Verdict::Fail },
        format!(
            "к возврату keyDown из sendEvent до вида стенда дошло {effect:?}, ожидалось {expected:?}"
        ),
    );
    if !effect_ok {
        return Err(format!(
            "эффект контрольной клавиши не прочитан: дошло {effect:?}, ожидалось {expected:?}"
        ));
    }
    match startup.clock_check() {
        ClockCheck::Ok => Ok(()),
        ClockCheck::Distorted(reason) => Err(format!(
            "запуск искажён скачком системного времени, прогон повторяется: {reason}"
        )),
    }
}

fn record_marks(report: &mut Report, startup: &StartupMarks, marks: &Marks) {
    let since = |mark: Option<u64>| {
        startup
            .since_start_ms(mark)
            .map_or_else(|| "нет".to_owned(), |ms| format!("{ms:.3}"))
    };
    report.push("startup_wall_ms", format!("{:.3}", startup.wall_ms()));
    report.push("main_to_end_mono_ms", format!("{:.3}", startup.mono_ms()));
    report.push("pre_main_mono_ms", format!("{:.3}", startup.pre_main_ms()));
    for (name, mark) in [
        ("ghostty_init_since_start_ms", startup.ghostty_init_mono_ns),
        ("app_new_since_start_ms", startup.app_new_mono_ns),
        ("surface_new_since_start_ms", startup.surface_new_mono_ns),
        (
            "first_display_since_start_ms",
            startup.first_display_mono_ns,
        ),
        ("ready_since_start_ms", startup.ready_mono_ns),
        ("key_display_since_start_ms", startup.key_display_mono_ns),
        (
            "control_posted_since_start_ms",
            startup.control_posted_mono_ns,
        ),
        (
            "control_returned_since_start_ms",
            startup.control_returned_mono_ns,
        ),
        ("end_since_start_ms", Some(startup.end_mono_ns)),
        (
            "fixture_ready_since_start_ms",
            startup.fixture_ready_mono_ns,
        ),
        (
            "first_contents_since_start_ms",
            startup.first_contents_mono_ns,
        ),
    ] {
        report.push(name, since(mark));
    }
    report.push(
        "views_need_display_at_ready",
        format!("{:?}", marks.views_need_display_at_ready),
    );
    report.metric("startup_wall", "ms", vec![startup.wall_ms()]);
    report.metric("startup_main_to_end_mono", "ms", vec![startup.mono_ms()]);
    for (name, mark) in [
        ("startup_fixture_ready", startup.fixture_ready_mono_ns),
        ("startup_first_contents", startup.first_contents_mono_ns),
    ] {
        if let Some(ms) = startup.since_start_ms(mark) {
            report.metric(name, "ms", vec![ms]);
        }
    }
}

fn locale_line(init: &InitRecord) -> String {
    ["LC_ALL", "LC_CTYPE", "LC_NUMERIC", "LC_MESSAGES"]
        .iter()
        .map(|name| {
            format!(
                "{name}={}",
                init.after.locale.get(*name).map_or("<нет>", String::as_str)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn child_lang(state: &State) -> String {
    state
        .ready
        .as_ref()
        .and_then(|ready| ready.lang.clone())
        .unwrap_or_else(|| "<нет>".to_owned())
}

/// Путь Cocoa на маршруте LaunchServices: переменных языка на входе нет, `ghostty_init`
/// выставил `LANG` и UTF-8 `LC_CTYPE`. Значения записываются как есть, без пересчёта из
/// `NSLocale`.
fn cocoa_path_finding(report: &mut Report, init: &InitRecord, state: &State) {
    let lang_after = init.after.language_env.get(env::LANG);
    let ctype = init.after.locale.get("LC_CTYPE");
    let passed = init.before.language_env.is_empty()
        && init.code == 0
        && lang_after.is_some_and(|lang| !lang.is_empty())
        && ctype.is_some_and(|ctype| ctype.contains("UTF-8"));
    report.finding(
        "h1-launch-services-cocoa",
        Requirement::Required,
        if passed { Verdict::Pass } else { Verdict::Fail },
        format!(
            "переменные языка до ghostty_init {:?}; после: LANG={}, LANGUAGE={}; {}; изменения окружения: {}; потоков до {:?}, после {:?}; у ребёнка LANG={}",
            init.before.language_env,
            lang_after.map_or("<нет>", String::as_str),
            init.after
                .language_env
                .get("LANGUAGE")
                .map_or("<нет>", String::as_str),
            locale_line(init),
            init.env_changes().join(", "),
            init.before.threads,
            init.after.threads,
            child_lang(state)
        ),
    );
}

/// Диагностический запуск с недопустимым непустым `LANG`: наблюдение без вердикта.
fn invalid_lang_finding(report: &mut Report, init: &InitRecord, state: &State) {
    report.finding(
        "lang-invalid-diagnostic",
        Requirement::Research,
        Verdict::Observed,
        format!(
            "LANG={} до ghostty_init, код {}; изменения окружения: {}; категории locale до: {}, после: {}; у ребёнка LANG={}",
            init.before
                .language_env
                .get(env::LANG)
                .map_or("<нет>", String::as_str),
            init.code,
            init.env_changes().join(", "),
            init.before
                .locale
                .get("LC_ALL")
                .map_or("LC_ALL=<нет>".to_owned(), |all| format!("LC_ALL={all}")),
            locale_line(init),
            child_lang(state)
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::{STARTUP_SCENARIOS, STARTUP_VARIANTS};

    const WALL_US: i64 = 1_794_000_000_000_000;
    const MONO_NS: u64 = 5_000_000_000;

    fn start() -> ProcessStart {
        ProcessStart {
            wall_us: WALL_US,
            bsd_wall_us: Some(WALL_US),
            mono_ns: MONO_NS,
        }
    }

    fn entry() -> Entry {
        Entry {
            wall_us: WALL_US + 30_000,
            mono_ns: MONO_NS + 30_000_000,
        }
    }

    fn marks(jump_us: i64) -> Marks {
        Marks {
            end: Some((WALL_US + 320_000 + jump_us, MONO_NS + 320_000_000)),
            effect: Some(vec![("keyDown", CONTROL_KEY.code)]),
            first_display: Some(MONO_NS + 200_000_000),
            ..Marks::default()
        }
    }

    #[test]
    fn variants_follow_scenario() {
        assert_eq!(Kind::of(Scenario::Startup, SURFACE), Ok(Kind::Surface));
        assert_eq!(Kind::of(Scenario::StartupLs, BARE), Ok(Kind::Bare));
        assert_eq!(
            Kind::of(Scenario::Startup, env::INVALID_LANG_VARIANT),
            Ok(Kind::InvalidLang)
        );
        assert!(Kind::of(Scenario::StartupLs, env::INVALID_LANG_VARIANT).is_err());
        assert!(Kind::of(Scenario::Startup, "base").is_err());
        assert_eq!(STARTUP_VARIANTS, [SURFACE, BARE]);
        assert!(!Scenario::Startup.uses_ghostty(BARE));
        assert!(Scenario::StartupLs.uses_ghostty(SURFACE));
        assert!(Scenario::Startup.uses_ghostty(env::INVALID_LANG_VARIANT));
    }

    #[test]
    fn route_follows_scenario() {
        assert_eq!(check_route(Scenario::Startup, Route::Direct), Ok(()));
        assert_eq!(
            check_route(Scenario::StartupLs, Route::LaunchServices),
            Ok(())
        );
        assert!(check_route(Scenario::Startup, Route::LaunchServices).is_err());
        assert!(check_route(Scenario::StartupLs, Route::Direct).is_err());
    }

    #[test]
    fn marks_without_surface_leave_ghostty_marks_empty() {
        let startup = startup_marks(&marks(0), entry(), start(), None).unwrap();
        assert_eq!(startup.ghostty_init_mono_ns, None);
        assert_eq!(startup.surface_new_mono_ns, None);
        assert_eq!(startup.clock_check(), ClockCheck::Ok);
        assert!((startup.wall_ms() - 320.0).abs() < 1e-9);
        assert_eq!(
            startup.since_start_ms(startup.first_display_mono_ns),
            Some(200.0)
        );
    }

    #[test]
    fn stand_error_or_missing_end_gives_no_marks() {
        let failed = Marks {
            error: Some("сбой стенда".to_owned()),
            ..marks(0)
        };
        assert_eq!(
            startup_marks(&failed, entry(), start(), None),
            Err("сбой стенда".to_owned())
        );
        let no_end = Marks {
            end: None,
            ..marks(0)
        };
        assert!(startup_marks(&no_end, entry(), start(), None).is_err());
    }

    #[test]
    fn clock_jump_is_detected() {
        let startup = startup_marks(&marks(-100_000), entry(), start(), None).unwrap();
        assert!(matches!(startup.clock_check(), ClockCheck::Distorted(_)));
    }

    /// `run.sh` перечисляет запуски сценариев старта сам; числа должны совпадать с
    /// `results::STARTUP_SCENARIOS`.
    #[test]
    fn run_script_matches_startup_runs() {
        let script =
            |name: &str| -> usize { super::super::tests::script_value(name).parse().unwrap() };
        assert_eq!(
            STARTUP_SCENARIOS,
            [
                ("startup", script("STARTUP_RUNS")),
                ("startup-ls", script("STARTUP_LS_RUNS"))
            ]
        );
        assert_eq!(
            super::super::tests::script_value("BAD_LANG_VARIANT"),
            env::INVALID_LANG_VARIANT
        );
    }
}
