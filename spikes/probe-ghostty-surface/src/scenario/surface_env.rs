//! Сценарий `surface-env`: одна живая поверхность с фикстурой и проверки H1 — версия
//! `ghostty_info`, `diagnostics_count = 0`, `TERM=xterm-ghostty`, `TERMINFO` в бандле с записью
//! `xterm-ghostty`, nonce команды и окружения у ребёнка, `foreground_pid` и `tty_name` совпадают
//! с квитанцией. Варианты `light` и `dark` выставляют схему цвета приложения до первой
//! поверхности.
//!
//! Конец сценария — два намерения закрыть, одно освобождение на следующем проходе, выход фикстуры,
//! затем `ghostty_app_free`.

use std::cell::RefCell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use objc2::MainThreadOnly;
use objc2_app_kit::NSAutoresizingMaskOptions;
use objc2_foundation::{NSBundle, NSRect};

use super::channel::Hub;
use super::{Poll, Report, Scenario, Stand, Step, StepFailure, poll, run_steps, window};
use crate::fixture::{self, Ready};
use crate::ghostty::config::{self, ColorScheme, Config};
use crate::ghostty::init::InitRecord;
use crate::ghostty::runtime::{self, Created, StartRecord, SurfaceReading};
use crate::ghostty::surface::{Phase, Spec, SurfaceId};
use crate::results::{Requirement, Verdict};
use crate::view::SurfaceView;
use crate::{clock, ghostty};

const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);
const SIZE_TIMEOUT: Duration = Duration::from_secs(3);
const READY_TIMEOUT: Duration = Duration::from_secs(12);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
const READY_STEP: &str = "готовность фикстуры в поверхности";
const VIEWPORT_LIMIT: usize = 600;
const EXPECTED_TERM: &str = "xterm-ghostty";

/// Одна поверхность с фикстурой; её же использует сценарий старта.
pub(super) struct State {
    pub(super) hub: Hub,
    pub(super) view: objc2::rc::Retained<SurfaceView>,
    pub(super) id: SurfaceId,
    exe: PathBuf,
    resources: PathBuf,
    pub(super) fixture_nonce: String,
    env_nonce: String,
    pub(super) init: InitRecord,
    pub(super) started: StartRecord,
    pub(super) created_at_ns: Option<u64>,
    /// Возврат из `ghostty_surface_new`.
    pub(super) created_end_ns: Option<u64>,
    pub(super) ready: Option<Ready>,
    pub(super) ready_t_ns: Option<u64>,
    reading: Option<SurfaceReading>,
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::SurfaceEnv);
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
    let variant = &stand.prelude.variant;
    let Some(scheme) = ColorScheme::from_variant(variant) else {
        let reason = format!("предусловие: вариант {variant}, ожидается light или dark");
        return stand.conclude(report, Err(reason));
    };
    let state = match prepare(&stand, &mut report, scheme) {
        Ok(state) => Rc::new(RefCell::new(state)),
        Err(reason) => return finish(&stand, report, Err(reason), None),
    };
    run_steps(steps(&stand, &state), move |result| {
        let verdict = match result {
            Ok(()) => {
                judge(&state.borrow(), &mut report);
                Ok(())
            }
            Err(failure) => classify(&state, &failure, &mut report),
        };
        let view = state.borrow().view.clone();
        finish(&stand, report, verdict, Some(view));
    });
}

/// Инициализация, конфигурация, приложение и вид до первой поверхности; вид заполняет вид стенда.
pub(super) fn prepare(
    stand: &Stand,
    report: &mut Report,
    scheme: ColorScheme,
) -> Result<State, String> {
    prepare_in(stand, report, scheme, |view| {
        view.setFrame(stand.view.bounds());
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        stand.view.addSubview(view);
        Ok(())
    })
}

/// То же, но вид кладёт в окно `place`; затем вид становится first responder окна.
pub(super) fn prepare_in(
    stand: &Stand,
    report: &mut Report,
    scheme: ColorScheme,
    place: impl FnOnce(&SurfaceView) -> Result<(), String>,
) -> Result<State, String> {
    let prelude = stand
        .prelude
        .ghostty
        .borrow_mut()
        .take()
        .ok_or("прелюдия ghostty не выполнена")?;
    let mut hub = prelude.hub;
    let init = prelude.init;
    report.push_json("ghostty_init", &init);
    report.push("ghostty_init_env_changes", init.env_changes().join(", "));
    report.finding(
        "h1-init-once",
        Requirement::Required,
        Verdict::Pass,
        format!(
            "ghostty_init вызван один раз до NSApplication и рабочих потоков, код {}, argv {:?}",
            init.code, init.argv
        ),
    );

    let info = ghostty::info();
    report.push("ghostty_version", &info.version);
    report.push("ghostty_build_mode", info.build_mode);
    if info.version != shellio_sys_ghostty::GHOSTTY_VERSION {
        return Err(format!(
            "неверная версия: ghostty_info {} при штампе набора {}",
            info.version,
            shellio_sys_ghostty::GHOSTTY_VERSION
        ));
    }
    report.finding(
        "h1-version",
        Requirement::Required,
        Verdict::Pass,
        format!("ghostty_info {} совпадает со штампом набора", info.version),
    );

    let path = stand.prelude.out.join(config::FILE);
    let text = fs::read_to_string(&path)
        .map_err(|err| format!("предусловие: конфигурация {}: {err}", path.display()))?;
    report.push("ghostty_config", &text);
    let config = Config::load(&path)?;
    let diagnostics = config.diagnostics();
    report.push_json("ghostty_config_diagnostics", &diagnostics);
    if !diagnostics.is_empty() {
        return Err(format!(
            "конфигурация: {} диагностик: {}",
            diagnostics.len(),
            diagnostics.join("; ")
        ));
    }
    report.finding(
        "h1-config-diagnostics",
        Requirement::Required,
        Verdict::Pass,
        "ghostty_config_diagnostics_count = 0",
    );

    hub.start()?;
    let started = runtime::start(config, scheme, stand.app.isActive())?;
    report.push_json("ghostty_runtime_start", &started);

    let resources = NSBundle::mainBundle()
        .resourcePath()
        .map(|path| PathBuf::from(path.to_string()))
        .ok_or("у бандла нет Resources")?;
    let exe = std::env::current_exe().map_err(|err| format!("путь исполняемого файла: {err}"))?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    let fixture_nonce = format!("{}-s1", stand.prelude.nonce);
    let env_nonce = crate::launch::nonce()?;
    let channel = hub.path().display().to_string();
    let program = exe.display().to_string();
    let spec = Spec {
        command: fixture::shell_command(
            &program,
            &[
                "--fixture",
                "--channel",
                &channel,
                "--nonce",
                &fixture_nonce,
            ],
        ),
        working_directory: stand.prelude.out.display().to_string(),
        env: vec![(fixture::ENV_NONCE.to_owned(), env_nonce.clone())],
        wait_after_command: true,
    };
    report.push("surface_command", &spec.command);
    report.push("fixture_nonce", &fixture_nonce);
    report.push("env_nonce", &env_nonce);

    let view = SurfaceView::new(stand.view.mtm(), NSRect::ZERO);
    place(&view)?;
    stand.window.makeFirstResponder(Some(&view));
    let id = runtime::add_surface(view.clone(), spec)?;
    Ok(State {
        hub,
        view,
        id,
        exe,
        resources,
        fixture_nonce,
        env_nonce,
        init,
        started,
        created_at_ns: None,
        created_end_ns: None,
        ready: None,
        ready_t_ns: None,
        reading: None,
    })
}

fn steps(stand: &Rc<Stand>, state: &Rc<RefCell<State>>) -> Vec<Step> {
    let focus = Rc::clone(stand);
    let focus_state = Rc::clone(state);
    let sized = Rc::clone(state);
    let creating = Rc::clone(state);
    let waiting = Rc::clone(state);
    let reading = Rc::clone(state);
    let closing = Rc::clone(state);
    let gone = Rc::clone(state);
    vec![
        Step::new(
            "активность, key window и фокус вида поверхности",
            FOCUS_TIMEOUT,
            move || focused(&focus, &focus_state.borrow().view),
            || Ok(()),
        ),
        Step::new(
            "ненулевой размер вида в пикселях",
            SIZE_TIMEOUT,
            move || {
                let pixels = sized.borrow().view.pixel_size();
                poll(pixels.0 > 0 && pixels.1 > 0, || {
                    format!("размер вида в пикселях {pixels:?}")
                })
            },
            move || create(&creating),
        ),
        Step::new(
            READY_STEP,
            READY_TIMEOUT,
            move || ready(&mut waiting.borrow_mut()),
            move || {
                let mut state = reading.borrow_mut();
                state.reading = runtime::read(state.id);
                match state.reading {
                    Some(_) => Ok(()),
                    None => Err("поверхность не живая после готовности фикстуры".to_owned()),
                }
            },
        ),
        Step::new(
            "два намерения закрыть",
            CLOSE_TIMEOUT,
            || Poll::Ready(()),
            move || {
                let id = closing.borrow().id;
                let first = runtime::request_close(id)?;
                let second = runtime::request_close(id)
                    .map(|key| key.to_string())
                    .unwrap_or_else(|reason| format!("отклонено: {reason}"));
                super::events::note(format!("намерения закрыть: {first}, {second}"));
                Ok(())
            },
        ),
        Step::new(
            "освобождение поверхности и выход фикстуры",
            CLOSE_TIMEOUT,
            move || freed(&mut gone.borrow_mut()),
            || Ok(()),
        ),
    ]
}

fn focused(stand: &Stand, view: &SurfaceView) -> Poll<()> {
    let key_is_main = stand
        .app
        .keyWindow()
        .as_deref()
        .is_some_and(|key| std::ptr::eq(key, &*stand.window));
    let responder = stand.window.firstResponder();
    let view_is_responder = responder
        .as_deref()
        .is_some_and(|responder| std::ptr::eq(responder, view.as_ref()));
    poll(
        stand.app.isActive() && key_is_main && view_is_responder,
        || super::events::focus_state(&stand.app),
    )
}

pub(super) fn create(state: &Rc<RefCell<State>>) -> Result<(), String> {
    let id = state.borrow().id;
    let started = clock::uptime_ns();
    match runtime::create(id)? {
        Created::Created { key, new_ms } => {
            let finished = clock::uptime_ns();
            super::events::note(format!("поверхность {key} создана за {new_ms:.1} мс"));
            let mut state = state.borrow_mut();
            state.created_at_ns = Some(started);
            state.created_end_ns = Some(finished);
            Ok(())
        }
        Created::Waiting(decision) => Err(format!("реестр не разрешил создание: {decision:?}")),
    }
}

pub(super) fn ready(state: &mut State) -> Poll<()> {
    state.hub.poll();
    let nonce = state.fixture_nonce.clone();
    let found = state.hub.by_nonce(&nonce).map(|conn| {
        let t_ns = conn
            .receipts
            .iter()
            .find_map(|receipt| match receipt.event {
                fixture::Event::Ready(_) => Some(receipt.t_ns),
                _ => None,
            });
        (conn.ready.clone(), t_ns)
    });
    if let Some((Some(ready), t_ns)) = found {
        state.ready = Some(ready);
        state.ready_t_ns = t_ns;
        return Poll::Ready(());
    }
    Poll::Pending(format!(
        "поверхность {}; {}",
        runtime::phase(state.id).map_or("нет", Phase::name),
        state.hub.describe()
    ))
}

pub(super) fn freed(state: &mut State) -> Poll<()> {
    state.hub.poll();
    let phase = runtime::phase(state.id);
    let nonce = state.fixture_nonce.clone();
    let pid = state.ready.as_ref().map(|ready| ready.pid);
    let (closed, exit) = state
        .hub
        .by_nonce(&nonce)
        .map(|conn| (conn.closed, conn.exit.clone()))
        .unwrap_or((false, None));
    let alive = pid.is_some_and(process_alive);
    poll(phase == Some(Phase::Freed) && closed && !alive, || {
        format!(
            "поверхность {}, сокет фикстуры закрыт {closed}, выход {exit:?}, процесс {pid:?} жив {alive}",
            phase.map_or("нет", Phase::name)
        )
    })
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: сигнал 0 только проверяет существование процесса.
    let status = unsafe { libc::kill(pid, 0) };
    status == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Выводы H1 по квитанции готовности и чтению поверхности.
fn judge(state: &State, report: &mut Report) {
    let Some(ready) = &state.ready else {
        return;
    };
    report.push_json("fixture_ready", ready);
    if let (Some(created), Some(ready_at)) = (state.created_at_ns, state.ready_t_ns) {
        report.metric(
            "surface_new_to_fixture_ready",
            "ms",
            vec![clock::interval_ms(created, ready_at)],
        );
    }
    let check = |report: &mut Report, name: &str, ok: bool, reason: String| {
        let verdict = if ok { Verdict::Pass } else { Verdict::Fail };
        report.finding(name, Requirement::Required, verdict, reason);
    };
    check(
        report,
        "h1-term",
        ready.term.as_deref() == Some(EXPECTED_TERM),
        format!("TERM у ребёнка {:?}, ожидалось {EXPECTED_TERM}", ready.term),
    );
    let expected_terminfo = state.resources.join("terminfo");
    let terminfo_ok = ready
        .terminfo
        .as_deref()
        .is_some_and(|dir| same_path(Path::new(dir), &expected_terminfo))
        && ready.terminfo_entry.is_some();
    check(
        report,
        "h1-terminfo",
        terminfo_ok,
        format!(
            "TERMINFO {:?}, запись {:?}, ожидался {} с записью xterm-ghostty",
            ready.terminfo,
            ready.terminfo_entry,
            expected_terminfo.display()
        ),
    );
    check(
        report,
        "h1-command-nonce",
        ready.nonce == state.fixture_nonce,
        format!(
            "фикстура из command поверхности ответила nonce {}, argv0 {}",
            ready.nonce, ready.argv0
        ),
    );
    check(
        report,
        "h1-env-nonce",
        ready.env_nonce.as_deref() == Some(state.env_nonce.as_str()),
        format!(
            "{} у ребёнка {:?}, ожидалось {}",
            fixture::ENV_NONCE,
            ready.env_nonce,
            state.env_nonce
        ),
    );
    let Some(reading) = &state.reading else {
        return;
    };
    report.push_json("surface_reading", reading);
    let foreground = ready
        .foreground_pgid
        .and_then(|pgid| u64::try_from(pgid).ok());
    check(
        report,
        "h1-foreground-pid",
        foreground == Some(reading.foreground_pid),
        format!(
            "ghostty_surface_foreground_pid {} (tcgetpgrp мастера), у фикстуры tcgetpgrp {:?}, pid {}, pgid {}, sid {}",
            reading.foreground_pid, ready.foreground_pgid, ready.pid, ready.pgid, ready.sid
        ),
    );
    check(
        report,
        "h1-tty-name",
        reading.tty_name.is_some() && reading.tty_name == ready.tty,
        format!(
            "ghostty_surface_tty_name {:?}, у фикстуры ttyname {:?}",
            reading.tty_name, ready.tty
        ),
    );
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Тайм-аут готовности, когда в PTY поверхности работает не фикстура, — отрицательный вывод H1:
/// команда поверхности не дошла до ребёнка. Иначе — ошибка исполнения.
fn classify(
    state: &Rc<RefCell<State>>,
    failure: &StepFailure,
    report: &mut Report,
) -> Result<(), String> {
    let reason = failure.describe();
    if failure.step != READY_STEP {
        return Err(reason);
    }
    let state = state.borrow();
    let text = runtime::viewport_text(state.id).map(|text| truncate(&text, VIEWPORT_LIMIT));
    report.push_json("surface_viewport_on_timeout", &text);
    let reading = runtime::read(state.id);
    let program = reading
        .as_ref()
        .and_then(|reading| i32::try_from(reading.foreground_pid).ok())
        .and_then(program_of);
    report.push_json("surface_reading_on_timeout", &reading);
    report.push_json("foreground_program_on_timeout", &program);
    match program {
        Some(program) if !same_path(Path::new(&program), &state.exe) => {
            let because = format!(
                "фикстура не ответила: на переднем плане PTY {program}, а не {}",
                state.exe.display()
            );
            for name in ["h1-command-nonce", "h1-env-nonce"] {
                report.finding(name, Requirement::Required, Verdict::Fail, because.clone());
            }
            Ok(())
        }
        _ => Err(format!(
            "{reason}; видимый текст поверхности: {}",
            text.as_deref().unwrap_or("<не прочитан>")
        )),
    }
}

fn program_of(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: буфер указанной длины живёт на время вызова.
    let len = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let len = usize::try_from(len).ok().filter(|len| *len > 0)?;
    buf.truncate(len);
    String::from_utf8(buf).ok()
}

fn truncate(text: &str, limit: usize) -> String {
    match text.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

/// Завершение runtime, если он запущен, и итог. Освобождение поверхности останавливает фикстуру.
/// Без `view` вид поверхности ищется среди дочерних видов вида стенда.
pub(super) fn finish(
    stand: &Stand,
    mut report: Report,
    verdict: Result<(), String>,
    view: Option<objc2::rc::Retained<SurfaceView>>,
) {
    let mut verdict = verdict;
    match runtime::shutdown() {
        Ok(record) => {
            let stats = &record.stats;
            let single = stats.surfaces_created == 1
                && stats.surfaces_freed == 1
                && stats.releases.len() == 1;
            if verdict.is_ok() && stats.close_intents >= 2 {
                report.finding(
                    "surface-single-free",
                    Requirement::Required,
                    if single { Verdict::Pass } else { Verdict::Fail },
                    format!(
                        "намерений закрыть {}, создано {}, освобождено {}, вызовов free {}",
                        stats.close_intents,
                        stats.surfaces_created,
                        stats.surfaces_freed,
                        stats.releases.len()
                    ),
                );
            }
            let free_ms: Vec<f64> = stats.releases.iter().map(|r| r.free_ms).collect();
            report.metric("surface_free", "ms", free_ms);
            report.metric("app_free", "ms", vec![record.app_free_ms]);
            report.push_json("ghostty_runtime", stats);
        }
        Err(reason) => report.push("ghostty_runtime", reason),
    }
    let view = view.or_else(|| {
        stand
            .view
            .subviews()
            .iter()
            .find_map(|view| view.downcast::<SurfaceView>().ok())
    });
    if let Some(view) = view {
        report.push_json("surface_view_feeds", &view.feeds());
        if view.is_attached() && verdict.is_ok() {
            verdict = Err("вид держит ручку поверхности после освобождения".to_owned());
        }
    }
    stand.conclude(report, verdict);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_counts_characters() {
        assert_eq!(truncate("абвгд", 3), "абв…");
        assert_eq!(truncate("abc", 3), "abc");
    }

    #[test]
    fn this_process_is_alive_and_its_program_is_known() {
        let pid = std::process::id() as i32;
        assert!(process_alive(pid));
        let exe = fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        let program = program_of(pid).unwrap();
        assert!(same_path(Path::new(&program), &exe));
    }
}
