//! Сценарий `idle`: CPU в простое и память. Память — `ri_phys_footprint` в состояниях пустое
//! окно → сцена → список на 100 000 строк → его закрытие, базовое значение и прирост раздельно.
//! CPU — `100 × Δ(CLOCK_PROCESS_CPUTIME_ID) / Δ(монотонного времени)` за 10 секунд видимой
//! неподвижной сцены с остановленным индикатором. В окне замера таймеров-предикатов нет, кроме
//! одного разового на его конец, и `displayLink` не создаётся. Все замеры снимает
//! `sampler`.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::MainThreadOnly;
use objc2_foundation::{NSRunLoop, NSRunLoopCommonModes, NSTimer};

use super::events;
use super::readback;
use super::sampler::{self, Sample};
use super::split::Stable;
use super::startup::require_builtin;
use super::{Report, Scenario, Stand, Step, StepFailure, Verdict, poll, run_steps};
use crate::fixtures::{self, SCENE_ROWS};
use crate::scene::Scene;
use crate::tiles::{BOTTOM, CENTER};

const STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// Сколько каждое состояние памяти держится после того, как установилось.
const HOLD: Duration = Duration::from_secs(1);
const CPU_WINDOW: Duration = Duration::from_secs(10);
const LIST_ROWS: usize = 100_000;
/// Бюджет CPU GUI в простое из `docs/architecture.md`, проценты одного ядра.
const CPU_BUDGET: f64 = 1.0;
/// Бюджет памяти GUI без терминалов: 150 × 10⁶ байт.
const MEMORY_BUDGET: u64 = 150_000_000;

const EMPTY: &str = "empty_window";
const SCENE: &str = "scene";
const LIST: &str = "list_100000";
const CLOSED: &str = "list_closed";

#[derive(Default)]
struct Memory {
    current: Option<&'static str>,
    states: Vec<(&'static str, Vec<u64>)>,
    error: Option<String>,
}

impl Memory {
    fn begin(&mut self, state: &'static str) {
        self.current = Some(state);
        self.states.push((state, Vec::new()));
        self.sample();
    }

    fn end(&mut self) {
        self.sample();
        self.current = None;
    }

    fn sample(&mut self) {
        if self.current.is_none() {
            return;
        }
        match sampler::footprint() {
            Ok(bytes) => {
                if let Some((_, samples)) = self.states.last_mut() {
                    samples.push(bytes);
                }
            }
            Err(reason) => self.error = Some(reason),
        }
    }

    fn samples(&self, state: &str) -> &[u64] {
        self.states
            .iter()
            .find(|(name, _)| *name == state)
            .map_or(&[], |(_, samples)| samples.as_slice())
    }
}

/// Медиана и максимум, байты.
fn median_max(samples: &[u64]) -> Option<(u64, u64)> {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let max = *sorted.last()?;
    let rank = sorted.len().div_ceil(2);
    Some((sorted[rank - 1], max))
}

struct Run {
    stand: Rc<Stand>,
    scene: RefCell<Option<Rc<Scene>>>,
    report: RefCell<Report>,
    mark: Cell<Option<Duration>>,
    stable: RefCell<Stable>,
    memory: RefCell<Memory>,
    hold: Cell<Option<Instant>>,
}

impl Run {
    fn scene(&self) -> Result<Rc<Scene>, String> {
        self.scene
            .borrow()
            .clone()
            .ok_or_else(|| "сцена ещё не собрана".to_owned())
    }

    fn touch(&self) {
        self.mark.set(events::elapsed());
        *self.stable.borrow_mut() = Stable::new();
    }

    /// Проход отображения после отметки и неподвижные кадры окна и списка.
    fn settled(&self) -> bool {
        self.memory.borrow_mut().sample();
        let Some(mark) = self.mark.get() else {
            return false;
        };
        if events::display_pass_after(mark).is_none() {
            return false;
        }
        let mut frames = Vec::new();
        match self.scene.borrow().as_ref() {
            Some(scene) => {
                frames.push(readback::rect(scene.window.frame()));
                if let Some(list) = scene.tiles.tile(CENTER).list() {
                    frames.push(readback::rect(list.view().documentVisibleRect()));
                }
            }
            None => frames.push(readback::rect(self.stand.window.frame())),
        }
        self.stable.borrow_mut().quiet(frames)
    }

    fn held(&self) -> bool {
        self.memory.borrow_mut().sample();
        self.hold.get().is_some_and(|since| since.elapsed() >= HOLD)
    }

    fn state(&self) -> String {
        format!(
            "memory={:?} {}",
            self.memory.borrow().current,
            events::focus_state(&self.stand.app)
        )
    }

    fn key_is(&self, window: &objc2_app_kit::NSWindow) -> bool {
        self.memory.borrow_mut().sample();
        self.stand.app.isActive()
            && self
                .stand
                .app
                .keyWindow()
                .is_some_and(|key| events::same(&*key, window))
    }

    fn list_rows(&self, rows: usize) -> Result<(), String> {
        let scene = self.scene()?;
        let tile = scene.tiles.tile(CENTER);
        let list = tile.list().ok_or("у центральной плитки нет списка")?;
        self.touch();
        list.set_rows(fixtures::list(rows));
        Ok(())
    }

    fn scroll_to(&self, row: usize) -> Result<(), String> {
        let scene = self.scene()?;
        let tile = scene.tiles.tile(CENTER);
        let list = tile.list().ok_or("у центральной плитки нет списка")?;
        self.touch();
        list.table().scrollRowToVisible(row as isize);
        Ok(())
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Idle);
    if let Err(reason) = stand
        .check_session(&mut report)
        .and_then(|()| require_builtin(&stand))
    {
        return stand.conclude(report, Err(reason));
    }
    report.push(
        "empty_window",
        "окно стенда без сцены: NSWindow с заголовком 1180×720",
    );
    report.push(
        "memory_source",
        "ri_phys_footprint из proc_pid_rusage на каждой проверке предиката, около 200 раз в секунду",
    );
    report.push(
        "cpu_formula",
        "100 × Δ(CLOCK_PROCESS_CPUTIME_ID) / Δ(CLOCK_UPTIME_RAW), без деления на число ядер",
    );
    report.push(
        "list_close",
        format!("закрытие списка: центральная плитка возвращается к {SCENE_ROWS} строкам сцены"),
    );
    let run = Rc::new(Run {
        stand: Rc::clone(&stand),
        scene: RefCell::new(None),
        report: RefCell::new(report),
        mark: Cell::new(None),
        stable: RefCell::new(Stable::new()),
        memory: RefCell::new(Memory::default()),
        hold: Cell::new(None),
    });
    let next = Rc::clone(&run);
    run_steps(before_window(&run), move |result| match result {
        Ok(()) => cpu_window(next),
        Err(failure) => finish(next, Err(failure)),
    });
}

fn step(
    run: &Rc<Run>,
    name: &'static str,
    ready: impl Fn(&Run) -> bool + 'static,
    act: impl FnOnce(&Run) -> Result<(), String> + 'static,
) -> Step {
    let (for_ready, for_act) = (Rc::clone(run), Rc::clone(run));
    Step::new(
        name,
        STEP_TIMEOUT,
        move || poll(ready(&for_ready), || for_ready.state()),
        move || act(&for_act),
    )
}

fn hold(run: &Run) -> Result<(), String> {
    run.hold.set(Some(Instant::now()));
    Ok(())
}

fn before_window(run: &Rc<Run>) -> Vec<Step> {
    vec![
        step(
            run,
            "пустое окно: активность и key window",
            |run| run.key_is(&run.stand.window),
            |run| events::warp_cursor_outside(run.stand.app.mtm(), &run.stand.app).map(|_| ()),
        ),
        step(
            run,
            "курсор вне окон",
            |run| events::cursor_outside(&run.stand.app),
            |run| {
                run.touch();
                run.memory.borrow_mut().begin(EMPTY);
                Ok(())
            },
        ),
        step(run, "пустое окно установилось", Run::settled, hold),
        step(
            run,
            "память пустого окна",
            Run::held,
            |run| {
                run.memory.borrow_mut().end();
                let stand = &run.stand;
                let scene = Scene::build(stand.app.mtm(), &stand.display, stand.scene);
                scene.window.makeKeyAndOrderFront(None);
                stand.window.orderOut(None);
                *run.scene.borrow_mut() = Some(scene);
                run.touch();
                run.memory.borrow_mut().begin(SCENE);
                Ok(())
            },
        ),
        step(
            run,
            "сцена: key window",
            |run| {
                run.scene
                    .borrow()
                    .as_ref()
                    .is_some_and(|scene| run.key_is(&scene.window))
            },
            |run| {
                run.touch();
                Ok(())
            },
        ),
        step(run, "сцена установилась", Run::settled, hold),
        step(run, "память сцены", Run::held, |run| {
            run.memory.borrow_mut().end();
            let scene = run.scene()?;
            let bottom = scene.tiles.tile(BOTTOM);
            let indicator = bottom.indicator().ok_or("у нижней плитки нет индикатора")?;
            run.touch();
            indicator.set_running(false);
            Ok(())
        }),
        step(
            run,
            "индикатор остановлен, сцена неподвижна",
            |run| {
                let stopped = run.scene.borrow().as_ref().is_some_and(|scene| {
                    scene
                        .tiles
                        .tile(BOTTOM)
                        .indicator()
                        .is_some_and(|indicator| {
                            indicator.spinner().isHidden()
                                && readback::animation_keys(indicator.arc()).is_empty()
                        })
                });
                stopped && run.settled()
            },
            |_| Ok(()),
        ),
    ]
}

/// Окно замера CPU: замер сразу после последнего шага, затем один разовый таймер на конец окна.
/// Таймер предиката предыдущего шага уже снят.
fn cpu_window(run: Rc<Run>) {
    let start = match sampler::sample() {
        Ok(sample) => sample,
        Err(reason) => return finish_with(run, Err(reason)),
    };
    let passes = events::display_passes();
    events::note("окно замера CPU открыто");
    let pending = RefCell::new(Some(run));
    let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
        let Some(run) = pending.borrow_mut().take() else {
            return;
        };
        let end = sampler::sample();
        let passes = events::display_passes() - passes;
        events::note("окно замера CPU закрыто");
        match end {
            Ok(end) => {
                record_cpu(&run, start, end, passes);
                let next = Rc::clone(&run);
                run_steps(after_window(&run), move |result| finish(next, result));
            }
            Err(reason) => finish_with(run, Err(reason)),
        }
    });
    // SAFETY: таймер добавляется в цикл главного потока, блок вызывается там же.
    let timer = unsafe {
        NSTimer::timerWithTimeInterval_repeats_block(CPU_WINDOW.as_secs_f64(), false, &block)
    };
    // SAFETY: `NSRunLoopCommonModes` — неизменяемая константа Foundation.
    unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
}

fn record_cpu(run: &Run, start: Sample, end: Sample, passes: u64) {
    let cpu_ns = end.cpu_ns.saturating_sub(start.cpu_ns);
    let mono_ns = end.mono_ns.saturating_sub(start.mono_ns);
    let percent = sampler::cpu_percent(cpu_ns, mono_ns);
    let mut report = run.report.borrow_mut();
    report.push("cpu_window_ms", format!("{:.3}", mono_ns as f64 / 1e6));
    report.push("cpu_time_ms", format!("{:.3}", cpu_ns as f64 / 1e6));
    report.push("cpu_window_display_passes", passes);
    report.push(
        "cpu_window_idle_wakeups",
        end.idle_wakeups.saturating_sub(start.idle_wakeups),
    );
    report.push(
        "cpu_window_interrupt_wakeups",
        end.interrupt_wakeups
            .saturating_sub(start.interrupt_wakeups),
    );
    report.push_json("cpu_window_start", &start);
    report.push_json("cpu_window_end", &end);
    if let Some(percent) = percent {
        report.metric("idle_cpu", "%", vec![percent]);
    }
    report.finding(
        "cpu_budget",
        if percent.is_some_and(|percent| percent <= CPU_BUDGET) {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "CPU {} за {:.1} с видимой неподвижной сцены при бюджете {CPU_BUDGET}%: индикатор остановлен, displayLink нет, проходов отображения в окне {passes}",
            percent.map_or_else(|| "нет".to_owned(), |percent| format!("{percent:.3}%")),
            mono_ns as f64 / 1e9
        ),
    );
}

fn after_window(run: &Rc<Run>) -> Vec<Step> {
    vec![
        step(
            run,
            "100000: загрузка",
            |run| {
                run.scene
                    .borrow()
                    .as_ref()
                    .is_some_and(|scene| run.key_is(&scene.window))
            },
            |run| {
                run.memory.borrow_mut().begin(LIST);
                run.list_rows(LIST_ROWS)
            },
        ),
        step(run, "100000: первый экран", Run::settled, |run| {
            run.scroll_to(LIST_ROWS - 1)
        }),
        step(run, "100000: конец списка", Run::settled, |run| {
            run.scroll_to(0)
        }),
        step(run, "100000: начало списка", Run::settled, hold),
        step(run, "память списка", Run::held, |run| {
            run.memory.borrow_mut().end();
            run.list_rows(SCENE_ROWS)?;
            run.memory.borrow_mut().begin(CLOSED);
            Ok(())
        }),
        step(run, "список закрыт и установился", Run::settled, hold),
        step(
            run,
            "память после закрытия",
            Run::held,
            |run| {
                run.memory.borrow_mut().end();
                Ok(())
            },
        ),
    ]
}

fn finish(run: Rc<Run>, result: Result<(), StepFailure>) {
    finish_with(run, result.map_err(|failure| failure.describe()));
}

fn finish_with(run: Rc<Run>, result: Result<(), String>) {
    let stand = Rc::clone(&run.stand);
    let mut report = run.report.replace(Report::default());
    let memory = run.memory.replace(Memory::default());
    if let Some(scene) = run.scene.borrow().as_ref() {
        report.push_json("scene_window", &readback::window_info(&scene.window));
    }
    let result = result.and_then(|()| match memory.error {
        Some(reason) => Err(reason),
        None => record_memory(&mut report, &memory),
    });
    stand.conclude(report, result);
}

fn record_memory(report: &mut Report, memory: &Memory) -> Result<(), String> {
    let mut stats = Vec::new();
    for state in [EMPTY, SCENE, LIST, CLOSED] {
        let samples = memory.samples(state);
        let (median, max) =
            median_max(samples).ok_or_else(|| format!("память: нет замеров состояния {state}"))?;
        report.metric(
            &format!("footprint_{state}"),
            "MiB",
            samples.iter().map(|bytes| sampler::mib(*bytes)).collect(),
        );
        report.push(
            &format!("footprint_{state}_bytes"),
            format!("median {median} max {max} n {}", samples.len()),
        );
        stats.push((median, max));
    }
    let [(empty, _), (scene, scene_max), (_, list_max), (closed, _)] = stats[..] else {
        return Err("память: не все состояния".to_owned());
    };
    let delta = |a: u64, b: u64| (a as f64 - b as f64) / (1024.0 * 1024.0);
    report.push("footprint_base_mib", format!("{:.2}", sampler::mib(empty)));
    report.push(
        "footprint_scene_increment_mib",
        format!("{:.2}", delta(scene, empty)),
    );
    report.push(
        "footprint_scene_max_mib",
        format!("{:.2}", sampler::mib(scene_max)),
    );
    report.push(
        "footprint_list_increment_mib",
        format!("{:.2}", delta(list_max, empty)),
    );
    report.push(
        "footprint_closed_vs_scene_mib",
        format!("{:.2}", delta(closed, scene)),
    );
    report.finding(
        "memory_budget",
        if list_max <= MEMORY_BUDGET {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "максимум ri_phys_footprint сцены со списком на {LIST_ROWS} строк {:.2} МиБ при бюджете 150 × 10⁶ байт ({:.2} МиБ); база пустого окна {:.2} МиБ, прирост сцены {:.2} МиБ, прирост со списком {:.2} МиБ, после закрытия относительно сцены {:.2} МиБ",
            sampler::mib(list_max),
            sampler::mib(MEMORY_BUDGET),
            sampler::mib(empty),
            delta(scene, empty),
            delta(list_max, empty),
            delta(closed, scene)
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_max() {
        assert_eq!(median_max(&[]), None);
        assert_eq!(median_max(&[5]), Some((5, 5)));
        assert_eq!(median_max(&[4, 1, 3, 2]), Some((2, 4)));
        assert_eq!(median_max(&[9, 1, 5]), Some((5, 9)));
    }

    fn memory(list: Vec<u64>, closed: Option<Vec<u64>>) -> Memory {
        let mut states = vec![
            (EMPTY, vec![30_000_000]),
            (SCENE, vec![40_000_000]),
            (LIST, list),
        ];
        states.extend(closed.map(|samples| (CLOSED, samples)));
        Memory {
            states,
            ..Memory::default()
        }
    }

    fn memory_verdict(report: &Report) -> Option<Verdict> {
        report
            .findings
            .iter()
            .find(|finding| finding.name == "memory_budget")
            .map(|finding| finding.verdict)
    }

    #[test]
    fn missing_state_is_execution_error() {
        let mut report = Report::default();
        let result = record_memory(&mut report, &memory(vec![50_000_000], None));
        assert!(result.is_err_and(|reason| reason.contains(CLOSED)));
        assert_eq!(memory_verdict(&report), None);
    }

    #[test]
    fn budget_compares_list_maximum() {
        let cases = [
            (vec![MEMORY_BUDGET], Verdict::Pass),
            (vec![MEMORY_BUDGET + 1], Verdict::Fail),
            (
                vec![60_000_000, 60_000_000, MEMORY_BUDGET + 1],
                Verdict::Fail,
            ),
        ];
        for (list, want) in cases {
            let mut report = Report::default();
            record_memory(&mut report, &memory(list.clone(), Some(vec![41_048_576]))).unwrap();
            assert_eq!(memory_verdict(&report), Some(want), "{list:?}");
            let closed = report
                .observations
                .iter()
                .find(|observation| observation.key == "footprint_closed_vs_scene_mib")
                .map(|observation| observation.value.clone());
            assert_eq!(closed, Some("1.00".into()), "{list:?}");
        }
    }
}
