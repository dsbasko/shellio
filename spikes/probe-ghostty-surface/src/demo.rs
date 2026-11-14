//! Живые поверхности демонстрации: shell пользователя во вкладках центральной и нижней плиток.
//! Флаг `--defer-create` откладывает создание центральной, чтобы владелец успел усыпить дисплей
//! (`pmset displaysleepnow`): сбой на спящем дисплее повторяется по пробуждению экранов или пути
//! раскладки. Каждая попытка и итог печатаются в stdout.
//!
//! `ghostty_init` вызывает `main` до `NSApplication`; здесь — конфигурация, runtime, поверхности,
//! маршрутизатор режимов (`Ctrl-\`, в `Navigation` — `h/j/k/l`, `Ctrl-o`, `i`), палитра (`Cmd-K`)
//! и модель видимости с вариантом обходов из `--visibility` для ручного прохода задачи 19.
//!
//! Доступность: вид поверхности получает уведомления об изменении по таймеру обновления
//! доступности, который запускает первый запрос доступности при включённом VoiceOver
//! ([`crate::a11y::enable_demo`]).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::ptr::NonNull;

use block2::RcBlock;
use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSWindow};
use objc2_foundation::{NSRect, NSRunLoop, NSRunLoopCommonModes, NSTimer};

use std::rc::Rc;

use crate::ghostty::config::{ColorScheme, Config};
use crate::ghostty::runtime::{self, Created};
use crate::ghostty::surface::{Phase, Spec, SurfaceId};
use crate::scene::{self, Scene, Zone};
use crate::view::SurfaceView;
use crate::visibility::{self, Workarounds};

/// Шаг ожидания ненулевого размера вида и шаг отчёта о повторах.
const POLL_S: f64 = 0.1;
const REPORT_S: f64 = 1.0;

/// Что задал запуск демонстрации.
pub struct DemoSurface {
    pub config: PathBuf,
    pub defer_create_s: Option<u32>,
    /// Вариант обходов видимости; `None` — `base`.
    pub visibility: Option<String>,
}

thread_local! {
    static SURFACES: RefCell<Vec<SurfaceId>> = const { RefCell::new(Vec::new()) };
    static TIMERS: RefCell<Vec<Retained<NSTimer>>> = const { RefCell::new(Vec::new()) };
}

fn say(line: &str) {
    println!("демонстрация: {line}");
}

/// Запускает runtime, регистрирует поверхности в центральной и нижней плитках и ставит
/// маршрутизатор режимов, палитру и модель видимости.
pub fn start(
    scene: &Rc<Scene>,
    window: &NSWindow,
    app: &NSApplication,
    demo: &DemoSurface,
) -> Result<(), String> {
    let variant = demo.visibility.as_deref().unwrap_or("base");
    let workarounds = Workarounds::from_variant(variant)
        .ok_or(format!("неизвестный вариант видимости {variant}"))?;
    let config = Config::load(&demo.config)?;
    let diagnostics = config.diagnostics();
    if !diagnostics.is_empty() {
        return Err(format!(
            "конфигурация {}: {}",
            demo.config.display(),
            diagnostics.join("; ")
        ));
    }
    let scheme = if scene::is_dark(&app.effectiveAppearance()) {
        ColorScheme::Dark
    } else {
        ColorScheme::Light
    };
    // Модель видимости действует на виды, которые получат поверхность после включения.
    visibility::enable(workarounds);
    runtime::start(config, scheme, app.isActive())?;
    let center = add_shell(scene, window, Zone::Center, "Терминал shell, центр")?;
    let bottom = add_shell(scene, window, Zone::Bottom, "Терминал shell, низ")?;
    crate::router::install(Rc::clone(scene), window);
    crate::palette::install(window, Some(Rc::clone(scene)));
    scene.focus(window, center.view.as_ref());
    say(&format!(
        "видимость: вариант {variant}, обходы {:?}; маршрутизатор: Ctrl-\\ — Navigation, \
         в Navigation h/j/k/l — фокус, Ctrl-o — возврат к источнику, i или Enter — Input, \
         второй Ctrl-\\ подряд уходит в терминал; палитра — Cmd-K",
        workarounds.names()
    ));
    let delay = demo.defer_create_s.map_or(0.0, f64::from);
    if let Some(seconds) = demo.defer_create_s {
        say(&format!(
            "центральная поверхность создаётся через {seconds} с; усыпите дисплей (pmset displaysleepnow) и разбудите его после этого срока"
        ));
    }
    let id = center.id;
    schedule(delay, false, move |_| create_when_sized(id));
    schedule(REPORT_S, true, move |timer| report(id, timer));
    let id = bottom.id;
    schedule(0.0, false, move |_| create_when_sized(id));
    Ok(())
}

struct Shell {
    id: SurfaceId,
    view: Retained<SurfaceView>,
}

/// Вид поверхности с shell пользователя во вкладке первой части зоны.
fn add_shell(scene: &Scene, window: &NSWindow, zone: Zone, label: &str) -> Result<Shell, String> {
    let view = SurfaceView::new(window.mtm(), NSRect::ZERO);
    scene.add_tab(zone, 0, "shell", &view)?;
    view.set_a11y_label(label);
    crate::a11y::enable_demo(&view);
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".to_owned());
    let spec = Spec {
        command: String::new(),
        working_directory: home,
        env: Vec::new(),
        wait_after_command: false,
    };
    let id = runtime::add_surface(view.clone(), spec)?;
    SURFACES.with(|cell| cell.borrow_mut().push(id));
    Ok(Shell { id, view })
}

/// Первая попытка, когда у вида появился размер; при нулевом размере — ещё шаг.
fn create_when_sized(id: SurfaceId) {
    match runtime::create(id) {
        Ok(Created::Waiting(decision)) if runtime::phase(id) == Some(Phase::WaitingSize) => {
            say(&format!("создание ждёт: {decision:?}"));
            schedule(POLL_S, false, move |_| create_when_sized(id));
        }
        Ok(created) => say(&format!("первая попытка: {created:?}")),
        Err(reason) => say(&format!("ошибка создания: {reason}")),
    }
}

/// Печатает новые попытки создания и состояние повтора, пока поверхность не создана или повтор
/// не прекращён.
fn report(id: SurfaceId, timer: &NSTimer) {
    thread_local! {
        static SEEN: Cell<usize> = const { Cell::new(0) };
    }
    let Some(stats) = runtime::stats() else {
        timer.invalidate();
        return;
    };
    let seen = SEEN.with(Cell::get);
    for record in stats.creations.iter().skip(seen) {
        say(&format!(
            "попытка {} ({:?}): создана {}, сбой {:?}, дальше {:?}, дисплей {:?} спит {:?}, {:.1} мс",
            record.key,
            record.trigger,
            record.created,
            record.failure,
            record.after,
            record.display_id,
            record.display_asleep,
            record.new_ms
        ));
    }
    SEEN.with(|cell| cell.set(stats.creations.len()));
    let retry = runtime::retry_state(id);
    let done = match &retry {
        Some(retry) => retry.created || retry.gave_up.is_some(),
        None => runtime::phase(id).is_some_and(|phase| phase != Phase::WaitingSize),
    };
    if done {
        if let Some(retry) = retry {
            say(&format!(
                "итог: попыток {}, поводы {:?}, отказ {:?}, создано поверхностей {}",
                retry.attempts, retry.triggers, retry.gave_up, stats.surfaces_created
            ));
        }
        timer.invalidate();
    }
}

/// Завершение runtime при выходе из демонстрации: поверхность освобождается, shell завершается.
pub fn shutdown() {
    crate::a11y::shutdown_demo();
    let _ = crate::router::uninstall();
    let _ = crate::palette::uninstall();
    TIMERS.with(|timers| {
        for timer in timers.borrow_mut().drain(..) {
            timer.invalidate();
        }
    });
    if !SURFACES
        .with(|cell| std::mem::take(&mut *cell.borrow_mut()))
        .is_empty()
    {
        match runtime::shutdown() {
            Ok(record) => say(&format!(
                "runtime завершён: создано {}, освобождено {}, сбоев создания {}",
                record.stats.surfaces_created,
                record.stats.surfaces_freed,
                record.stats.surfaces_failed
            )),
            Err(reason) => say(&format!("завершение runtime: {reason}")),
        }
    }
}

fn schedule(delay_s: f64, repeats: bool, action: impl Fn(&NSTimer) + 'static) {
    let block = RcBlock::new(move |timer: NonNull<NSTimer>| {
        // SAFETY: таймер жив, пока вызывается его блок.
        action(unsafe { timer.as_ref() });
    });
    // SAFETY: таймер добавляется в цикл главного потока и снимается там же, поэтому блок
    // вызывается и освобождается только на главном потоке.
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_repeats_block(delay_s.max(0.001), repeats, &block)
    };
    // SAFETY: `NSRunLoopCommonModes` — неизменяемая константа Foundation.
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
    TIMERS.with(|timers| {
        let mut timers = timers.borrow_mut();
        timers.retain(|timer| timer.isValid());
        timers.push(timer);
    });
}
