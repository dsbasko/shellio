//! Сценарий `teardown` (H8, H14): закрытие при насыщенных колбэках и повтор создания.
//!
//! Части сценария, каждая с собственной уликой — без неё шаг ждёт до тайм-аута:
//! - 100 циклов создать-закрыть после прогрева: одно освобождение на цикл, выход фикстуры,
//!   длительность `ghostty_surface_new` и `ghostty_surface_free` и разрыв главного цикла вокруг них
//!   (таймер стенда 2 мс), затем возврат живых видов и записей реестра к базовой линии;
//! - четыре поверхности с потоком OSC заголовка и рабочего каталога на каждой строке: закрытие A при
//!   выводе соседей, затем два закрытия B и C до следующего `tick` (оба освобождения в одном проходе)
//!   при выводе D; соседи продолжают вывод и принимают ввод;
//! - закрывающаяся D: освобождение задержано швом стенда, колбэки снятой поверхности (заголовки из её
//!   потока, вставка по привязке) отбрасываются у источника, `wakeup` продолжаются, сообщение после
//!   освобождения уходит как устаревшее;
//! - выход ребёнка и явное закрытие раздельно: непустая команда включает `wait-after-command`, поэтому
//!   после выхода поверхность жива; затем клавиша закрывает её через `close_surface_cb` вместе с
//!   намерением пробы — одно освобождение;
//! - закрытие против выдачи вставки и против отклонения подтверждения OSC 52: шов стенда задерживает
//!   выдачу или отклонение, закрытие отклоняет ожидающий запрос на живой поверхности, поздняя работа
//!   видит снятый ID; после освобождения ожидающих запросов 0;
//! - повтор создания: NULL имитирован стендом (сон дисплея задан), пробуждение экранов и путь
//!   раскладки в одном обороте дают одну попытку и одну поверхность; неповторяемый сбой не
//!   повторяется. Настоящий сон дисплея — ручная проверка задачи 19.
//!
//! Вариант `scribble` идёт с `MallocScribble=1` от `--launch`; сценарий проверяет, что переменная
//! дошла до процесса. Отсутствие падения не доказывает отсутствие обращения к освобождённой памяти.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

use block2::RcBlock;
use objc2::MainThreadOnly;
use objc2::rc::Retained;
use objc2_foundation::{NSRect, NSRunLoop, NSRunLoopCommonModes, NSTimer};
use serde::Serialize;

use super::channel::Hub;
use super::input::focused;
use super::surface_env::{self, State};
use super::visibility::{Shield, shield_step};
use super::{Poll, Report, Scenario, Stand, Step, counters, events, poll, run_steps, window};
use crate::clock;
use crate::fixture::{self, Command};
use crate::ghostty::clipboard::{self, Board};
use crate::ghostty::config::ColorScheme;
use crate::ghostty::runtime::{self, CreateRecord, Created, Event, Hold, Stats};
use crate::ghostty::surface::{
    AfterFailure, Failure, Phase, ReleaseRecord, SurfaceId, SurfaceKey, Trigger,
};
use crate::results::{Requirement, Verdict};
use crate::scene::{Scene, Zone};
use crate::view::SurfaceView;
use crate::visibility::{self, Workarounds};

const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);
const READY_TIMEOUT: Duration = Duration::from_secs(12);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
const EVIDENCE_TIMEOUT: Duration = Duration::from_secs(5);
const ACT_TIMEOUT: Duration = Duration::from_secs(5);
/// Прогрев перед зачётными циклами.
pub const WARMUP: usize = 3;
pub const CYCLES: usize = 100;
/// Шаг таймера стенда, по которому видно, сколько главный цикл не обслуживался.
const LOOP_TICK_S: f64 = 0.002;
/// Выход раньше `abnormal-command-exit-runtime` (250 мс) ghostty считает аварийным.
const CHILD_MIN_AGE: Duration = Duration::from_millis(400);
/// Вариант с `MallocScribble=1`.
pub const SCRIBBLE: &str = "scribble";
const SCRIBBLE_VAR: &str = "MallocScribble";
const VIEW_CLASS: &str = "ProbeSurfaceView";
const OSC52_READ: &str = "\x1b]52;c;?\x07";
/// Поток соседей: OSC 2 и OSC 7 перед каждой строкой, квитанция хода каждые 100 строк, 30 с.
const STREAM: Command = Command::Stream {
    lines: 120_000,
    interval_us: 250,
    columns: 60,
    osc_every: 1,
    report_every: 100,
};

type Shared = Rc<RefCell<Td>>;

/// Поверхность с фикстурой.
struct Fx {
    view: Retained<SurfaceView>,
    id: SurfaceId,
    nonce: String,
    pid: Option<i32>,
    ready_ns: Option<u64>,
    key: Option<SurfaceKey>,
}

/// Счётчики runtime в момент отметки.
#[derive(Clone, Debug, Default, Serialize)]
struct Mark {
    t_ns: u64,
    pushed: u64,
    dropped_at_source: u64,
    dropped_stale: usize,
    wakeups: u64,
    passes: u64,
    late_skipped: u64,
    denied_on_retire: u64,
    clipboard_held: u64,
    child_exited: u64,
    titles: u64,
    created: usize,
}

impl Mark {
    fn take() -> Mark {
        let stats = runtime::stats().unwrap_or_default();
        let (pushed, dropped_at_source) = runtime::message_counts().unwrap_or_default();
        Mark {
            t_ns: clock::uptime_ns(),
            pushed,
            dropped_at_source,
            dropped_stale: stats.dropped_stale,
            wakeups: stats.wakeups,
            passes: stats.passes,
            late_skipped: stats.clipboard_late_skipped,
            denied_on_retire: stats.clipboard_denied_on_retire,
            clipboard_held: stats.clipboard_held,
            child_exited: delivered(&stats, "child-exited"),
            titles: delivered(&stats, "title"),
            created: stats.surfaces_created,
        }
    }
}

fn delivered(stats: &Stats, kind: &str) -> u64 {
    stats.delivered.get(kind).copied().unwrap_or(0)
}

/// Итог одного цикла создать-закрыть.
#[derive(Clone, Debug, Serialize)]
struct Cycle {
    index: usize,
    warmup: bool,
    key: String,
    new_ms: f64,
    free_ms: f64,
    loop_gap_new_ms: Option<f64>,
    loop_gap_free_ms: Option<f64>,
    single_free: bool,
    fixture_gone: bool,
}

struct Td {
    env: State,
    scene: Scene,
    fx: BTreeMap<&'static str, Fx>,
    ticks: Rc<RefCell<Vec<u64>>>,
    tick_timer: Option<Retained<NSTimer>>,
    cycle: usize,
    cycles: Vec<Cycle>,
    pids: Vec<i32>,
    baseline: Option<(usize, usize)>,
    marks: BTreeMap<&'static str, Mark>,
    /// Улики по частям: имя → значение для итога.
    evidence: BTreeMap<&'static str, serde_json::Value>,
    findings: Vec<(String, Requirement, Verdict, String)>,
    shield: Option<Shield>,
    board: String,
}

impl Td {
    fn hub(&mut self) -> &mut Hub {
        &mut self.env.hub
    }

    fn fx(&self, name: &str) -> Result<&Fx, String> {
        self.fx.get(name).ok_or(format!("нет поверхности {name}"))
    }

    fn id(&self, name: &str) -> Result<SurfaceId, String> {
        self.fx(name).map(|fx| fx.id)
    }

    fn mark(&mut self, label: &'static str) {
        self.marks.insert(label, Mark::take());
    }

    fn since(&self, label: &str) -> Mark {
        self.marks.get(label).cloned().unwrap_or_default()
    }

    fn note(&mut self, name: &'static str, value: impl Serialize) {
        let value = serde_json::to_value(value)
            .unwrap_or_else(|err| serde_json::Value::String(format!("не сериализуется: {err}")));
        self.evidence.insert(name, value);
    }

    fn finding(&mut self, name: &str, requirement: Requirement, ok: bool, reason: String) {
        let verdict = if ok { Verdict::Pass } else { Verdict::Fail };
        self.findings
            .push((name.to_owned(), requirement, verdict, reason));
    }

    /// Регистрирует поверхность с фикстурой `suffix` во вкладке зоны.
    fn add(
        &mut self,
        stand: &Stand,
        name: &'static str,
        zone: Zone,
        suffix: &str,
    ) -> Result<(), String> {
        let view = SurfaceView::new(stand.view.mtm(), NSRect::ZERO);
        self.scene.add_tab(zone, 0, name, &view)?;
        let extra = self.env.add_fixture(stand, suffix, view)?;
        self.fx.insert(
            name,
            Fx {
                view: extra.view,
                id: extra.id,
                nonce: extra.fixture_nonce,
                pid: None,
                ready_ns: None,
                key: None,
            },
        );
        Ok(())
    }

    fn create(&mut self, name: &str) -> Result<(), String> {
        let id = self.id(name)?;
        match runtime::create(id)? {
            Created::Live { key, new_ms } => {
                events::note(format!(
                    "{name}: поверхность {key} создана за {new_ms:.1} мс"
                ));
                if let Some(fx) = self.fx.get_mut(name) {
                    fx.key = Some(key);
                }
                Ok(())
            }
            other => Err(format!("{name}: поверхность не создана: {other:?}")),
        }
    }

    fn ready(&mut self, name: &str) -> Poll<()> {
        self.hub().poll();
        let Some(nonce) = self.fx.get(name).map(|fx| fx.nonce.clone()) else {
            return Poll::Pending(format!("нет поверхности {name}"));
        };
        let found = self.hub().by_nonce(&nonce).and_then(|conn| {
            let ready = conn.ready.clone()?;
            let t_ns = conn
                .receipts
                .iter()
                .find_map(|receipt| match receipt.event {
                    fixture::Event::Ready(_) => Some(receipt.t_ns),
                    _ => None,
                });
            Some((ready.pid, t_ns))
        });
        match found {
            Some((pid, t_ns)) => {
                if !self.pids.contains(&pid) {
                    self.pids.push(pid);
                }
                if let Some(fx) = self.fx.get_mut(name) {
                    fx.pid = Some(pid);
                    fx.ready_ns = t_ns;
                }
                Poll::Ready(())
            }
            None => {
                let phase = self.fx.get(name).and_then(|fx| runtime::phase(fx.id));
                let hub = self.hub().describe();
                Poll::Pending(format!("{name}: поверхность {phase:?}; {hub}"))
            }
        }
    }

    /// Поверхность освобождена, сокет фикстуры закрыт и процесса нет.
    fn gone(&mut self, name: &str) -> (bool, String) {
        self.hub().poll();
        let Some((id, nonce, pid)) = self
            .fx
            .get(name)
            .map(|fx| (fx.id, fx.nonce.clone(), fx.pid))
        else {
            return (false, format!("нет поверхности {name}"));
        };
        let phase = runtime::phase(id);
        let (closed, exit) = self
            .hub()
            .by_nonce(&nonce)
            .map(|conn| (conn.closed, conn.exit.clone()))
            .unwrap_or((false, None));
        let alive = pid.is_some_and(surface_env::process_alive);
        (
            phase == Some(Phase::Freed) && closed && !alive,
            format!(
                "{name}: фаза {phase:?}, сокет закрыт {closed}, выход {exit:?}, процесс жив {alive}"
            ),
        )
    }

    /// Фикстура закрыта и процесса нет, поверхность может быть жива.
    fn child_gone(&mut self, name: &str) -> (bool, String) {
        self.hub().poll();
        let Some((nonce, pid)) = self.fx.get(name).map(|fx| (fx.nonce.clone(), fx.pid)) else {
            return (false, format!("нет поверхности {name}"));
        };
        let (closed, exit) = self
            .hub()
            .by_nonce(&nonce)
            .map(|conn| (conn.closed, conn.exit.clone()))
            .unwrap_or((false, None));
        let alive = pid.is_some_and(surface_env::process_alive);
        (
            closed && !alive,
            format!("{name}: сокет закрыт {closed}, выход {exit:?}, процесс жив {alive}"),
        )
    }

    fn send(&mut self, name: &str, command: Command) -> Result<u64, String> {
        let nonce = self.fx(name)?.nonce.clone();
        self.hub().send(&nonce, command)
    }

    /// Квитанции хода потока фикстуры позже `t_ns`.
    fn progress_after(&mut self, name: &str, t_ns: u64) -> usize {
        self.receipts(name, |receipt| {
            receipt.t_ns > t_ns && matches!(receipt.event, fixture::Event::StreamProgress { .. })
        })
    }

    fn progress_between(&mut self, name: &str, from: u64, to: u64) -> usize {
        self.receipts(name, |receipt| {
            (from..=to).contains(&receipt.t_ns)
                && matches!(receipt.event, fixture::Event::StreamProgress { .. })
        })
    }

    fn receipts(&mut self, name: &str, matches: impl Fn(&fixture::Receipt) -> bool) -> usize {
        self.hub().poll();
        let Some(nonce) = self.fx.get(name).map(|fx| fx.nonce.clone()) else {
            return 0;
        };
        self.hub().by_nonce(&nonce).map_or(0, |conn| {
            conn.receipts.iter().filter(|r| matches(r)).count()
        })
    }

    /// Прочитанные фикстурой байты содержат `text`.
    fn read_contains(&mut self, name: &str, text: &str) -> bool {
        self.hub().poll();
        let Some(nonce) = self.fx.get(name).map(|fx| fx.nonce.clone()) else {
            return false;
        };
        let wanted = fixture::hex(text.as_bytes());
        self.hub().by_nonce(&nonce).is_some_and(|conn| {
            let all: String = conn
                .receipts
                .iter()
                .filter_map(|receipt| match &receipt.event {
                    fixture::Event::Read { hex, .. } => Some(hex.as_str()),
                    _ => None,
                })
                .collect();
            all.contains(&wanted)
        })
    }

    fn phase(&self, name: &str) -> Option<Phase> {
        self.fx.get(name).and_then(|fx| runtime::phase(fx.id))
    }

    fn key_of(&self, name: &str) -> Result<SurfaceKey, String> {
        self.fx(name)?
            .key
            .ok_or(format!("{name}: поверхность не создавалась"))
    }

    fn release_of(&self, name: &str) -> Option<ReleaseRecord> {
        let key = self.key_of(name).ok()?.to_string();
        runtime::stats()?
            .releases
            .into_iter()
            .find(|record| record.key == key)
    }

    fn releases_of(&self, name: &str) -> usize {
        let Ok(key) = self.key_of(name).map(|key| key.to_string()) else {
            return 0;
        };
        runtime::stats().map_or(0, |stats| {
            stats.releases.iter().filter(|r| r.key == key).count()
        })
    }

    fn creations_of(&self, name: &str) -> Vec<CreateRecord> {
        let Ok(id) = self.id(name) else {
            return Vec::new();
        };
        let prefix = format!("s{}g", id.0);
        runtime::stats().map_or_else(Vec::new, |stats| {
            stats
                .creations
                .into_iter()
                .filter(|record| record.key.starts_with(&prefix))
                .collect()
        })
    }

    /// Снимает вид поверхности со сцены; запись реестра уже снята освобождением.
    fn drop_view(&mut self, name: &str) -> Result<(), String> {
        let fx = self
            .fx
            .remove(name)
            .ok_or(format!("нет поверхности {name}"))?;
        self.scene.take_tab(&fx.view)?;
        fx.view.removeFromSuperview();
        Ok(())
    }

    fn age_ok(&self, name: &str) -> bool {
        self.fx
            .get(name)
            .and_then(|fx| fx.ready_ns)
            .is_some_and(|ready| {
                clock::interval_ms(ready, clock::uptime_ns()) >= CHILD_MIN_AGE.as_secs_f64() * 1e3
            })
    }
}

/// Разрыв главного цикла вокруг интервала: от последней отметки таймера стенда до начала до первой
/// после конца. `ticks` — по возрастанию.
fn loop_gap_ms(ticks: &[u64], start: u64, end: u64) -> Option<f64> {
    let before = ticks.partition_point(|tick| *tick <= start);
    let after = ticks.partition_point(|tick| *tick < end);
    let first = *ticks.get(before.checked_sub(1)?)?;
    let last = *ticks.get(after)?;
    Some(clock::interval_ms(first, last))
}

fn live_views() -> usize {
    counters::snapshot()
        .into_iter()
        .find(|count| count.name == VIEW_CLASS)
        .map_or(0, |count| count.live())
}

/// Шаги сценария поверх общего состояния.
struct Plan {
    stand: Rc<Stand>,
    td: Shared,
    steps: Vec<Step>,
}

impl Plan {
    fn act(
        &mut self,
        name: &'static str,
        f: impl FnOnce(&Stand, &mut Td) -> Result<(), String> + 'static,
    ) {
        let (stand, td) = (Rc::clone(&self.stand), Rc::clone(&self.td));
        self.steps.push(Step::new(
            name,
            ACT_TIMEOUT,
            || Poll::Ready(()),
            move || f(&stand, &mut td.borrow_mut()),
        ));
    }

    fn wait(
        &mut self,
        name: &'static str,
        timeout: Duration,
        mut f: impl FnMut(&Stand, &mut Td) -> Poll<()> + 'static,
    ) {
        let (stand, td) = (Rc::clone(&self.stand), Rc::clone(&self.td));
        self.steps.push(Step::new(
            name,
            timeout,
            move || f(&stand, &mut td.borrow_mut()),
            || Ok(()),
        ));
    }

    fn wait_ready(&mut self, name: &'static str, step: &'static str) {
        self.wait(step, READY_TIMEOUT, move |_, td| td.ready(name));
    }

    fn wait_gone(&mut self, names: &'static [&'static str], step: &'static str) {
        self.wait(step, CLOSE_TIMEOUT, move |_, td| {
            let states: Vec<(bool, String)> = names.iter().map(|name| td.gone(name)).collect();
            poll(states.iter().all(|(ok, _)| *ok), || {
                states
                    .iter()
                    .map(|(_, state)| state.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            })
        });
    }
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Teardown);
    let variant = stand.prelude.variant.clone();
    report.push("variant", &variant);
    report.push(
        "teardown_note",
        "освобождение в части D задержано швом стенда, чтобы окно между намерением и освобождением было наблюдаемым; выдача и отклонение буфера обмена задержаны швом в гонках закрытия; NULL ghostty_surface_new в части повтора имитирован стендом; отсутствие падения не доказывает отсутствие обращения к освобождённой памяти",
    );
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let scribble = std::env::var(SCRIBBLE_VAR).ok();
    report.push_json("malloc_scribble", &scribble);
    let expected = (variant == SCRIBBLE).then(|| "1".to_owned());
    if scribble != expected {
        let reason = format!(
            "предусловие: {SCRIBBLE_VAR}={scribble:?} в варианте {variant}, ожидалось {expected:?}"
        );
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
    let board = format!("io.github.dsbasko.shellio.probe.{}", stand.prelude.nonce);
    clipboard::use_board(Board::Named(board.clone()));
    report.push("pasteboard", &board);
    visibility::enable(Workarounds::default());
    let mtm = stand.view.mtm();
    let scene = Scene::new(mtm, false);
    scene.install(&stand.view, &stand.window);
    let env = surface_env::prepare_in(&stand, &mut report, ColorScheme::Light, |view| {
        scene.add_tab(Zone::Center, 0, "A", view)
    });
    let env = match env {
        Ok(env) => env,
        Err(reason) => return surface_env::finish(&stand, report, Err(reason), None),
    };
    scene.sync_focus(&stand.window);
    let a = Fx {
        view: env.view.clone(),
        id: env.id,
        nonce: env.fixture_nonce.clone(),
        pid: None,
        ready_ns: None,
        key: None,
    };
    let td = Rc::new(RefCell::new(Td {
        env,
        scene,
        fx: BTreeMap::from([("A", a)]),
        ticks: Rc::new(RefCell::new(Vec::new())),
        tick_timer: None,
        cycle: 0,
        cycles: Vec::new(),
        pids: Vec::new(),
        baseline: None,
        marks: BTreeMap::new(),
        evidence: BTreeMap::new(),
        findings: Vec::new(),
        shield: None,
        board,
    }));
    td.borrow_mut().finding(
        "h8-malloc-scribble-env",
        Requirement::Required,
        true,
        format!("{SCRIBBLE_VAR} в процессе {scribble:?}, вариант {variant}"),
    );
    let mut plan = Plan {
        stand: Rc::clone(&stand),
        td: Rc::clone(&td),
        steps: Vec::new(),
    };
    setup(&mut plan);
    cycles(&mut plan);
    neighbors(&mut plan);
    closing(&mut plan);
    child_exit(&mut plan);
    clipboard_races(&mut plan);
    retry(&mut plan);
    final_checks(&mut plan);
    let steps = plan.steps;
    run_steps(steps, move |result| {
        let verdict = result.map_err(|failure| failure.describe());
        let views = cleanup(&td, &mut report);
        surface_env::finish_views(&stand, report, verdict, views);
    });
}

fn setup(plan: &mut Plan) {
    plan.wait(
        "активность, key window и фокус A",
        FOCUS_TIMEOUT,
        |stand, td| match td.fx.get("A") {
            Some(fx) => focused(stand, &fx.view),
            None => Poll::Pending("нет поверхности A".to_owned()),
        },
    );
    let shield_td = Rc::clone(&plan.td);
    let stand = Rc::clone(&plan.stand);
    plan.steps.push(shield_step(&stand, move |shield| {
        shield_td.borrow_mut().shield = Some(shield);
    }));
    plan.act(
        "таймер разрыва главного цикла",
        |_, td| {
            let ticks = Rc::clone(&td.ticks);
            let block = RcBlock::new(move |_timer: NonNull<NSTimer>| {
                ticks.borrow_mut().push(clock::uptime_ns());
            });
            // SAFETY: таймер добавляется в цикл главного потока и снимается там же, поэтому блок
            // вызывается и освобождается только на главном потоке.
            let timer =
                unsafe { NSTimer::timerWithTimeInterval_repeats_block(LOOP_TICK_S, true, &block) };
            // SAFETY: `NSRunLoopCommonModes` — неизменяемая константа Foundation.
            unsafe { NSRunLoop::currentRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes) };
            td.tick_timer = Some(timer);
            td.baseline = Some((live_views(), runtime::entry_count().unwrap_or(0)));
            Ok(())
        },
    );
}

/// Прогрев и 100 циклов создать-закрыть в центральной зоне.
fn cycles(plan: &mut Plan) {
    for _ in 0..WARMUP + CYCLES {
        plan.act(
            "цикл: создание поверхности",
            |stand, td| {
                let suffix = format!("c{}", td.cycle);
                td.add(stand, "cycle", Zone::Center, &suffix)?;
                td.create("cycle")
            },
        );
        plan.wait_ready("cycle", "цикл: готовность фикстуры");
        plan.act("цикл: намерение закрыть", |_, td| {
            runtime::request_close(td.id("cycle")?).map(|_| ())
        });
        plan.wait_gone(&["cycle"], "цикл: освобождение и выход фикстуры");
        plan.act("цикл: итог и снятие вида", |_, td| {
            let index = td.cycle;
            let key = td.key_of("cycle")?;
            let release = td
                .release_of("cycle")
                .ok_or(format!("нет записи освобождения {key}"))?;
            let creation = td
                .creations_of("cycle")
                .into_iter()
                .find(|record| record.created)
                .ok_or(format!("нет записи создания {key}"))?;
            let (loop_gap_new_ms, loop_gap_free_ms) = {
                let ticks = td.ticks.borrow();
                (
                    loop_gap_ms(&ticks, creation.started_ns, creation.finished_ns),
                    loop_gap_ms(&ticks, release.started_ns, release.finished_ns),
                )
            };
            let single_free = td.releases_of("cycle") == 1;
            let fixture_gone = td.gone("cycle").0;
            td.cycles.push(Cycle {
                index,
                warmup: index < WARMUP,
                key: key.to_string(),
                new_ms: creation.new_ms,
                free_ms: release.free_ms,
                loop_gap_new_ms,
                loop_gap_free_ms,
                single_free,
                fixture_gone,
            });
            td.cycle += 1;
            td.drop_view("cycle")
        });
    }
    plan.wait(
        "живые виды и записи реестра вернулись к базовой линии",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let now = (live_views(), runtime::entry_count().unwrap_or(0));
            poll(Some(now) == td.baseline, || {
                format!("сейчас {now:?}, базовая линия {:?}", td.baseline)
            })
        },
    );
    plan.act("итог циклов", |_, td| {
        let counted: Vec<&Cycle> = td.cycles.iter().filter(|cycle| !cycle.warmup).collect();
        let single = counted.iter().all(|cycle| cycle.single_free && cycle.fixture_gone);
        let reason = format!(
            "{} циклов после {WARMUP} прогревочных: одно освобождение и выход фикстуры в каждом — {single}; живые виды и записи реестра вернулись к {:?}",
            counted.len(),
            td.baseline
        );
        let ok = single && counted.len() == CYCLES;
        td.finding("h8-cycles", Requirement::Required, ok, reason);
        Ok(())
    });
}

/// Четыре поверхности с потоком OSC: закрытие A при выводе соседей, два закрытия до `tick`.
fn neighbors(plan: &mut Plan) {
    plan.act(
        "B, C и D в трёх зонах, создание A–D",
        |stand, td| {
            td.add(stand, "B", Zone::Bottom, "b")?;
            td.add(stand, "C", Zone::Right, "c")?;
            td.add(stand, "D", Zone::Left, "d")?;
            for name in ["A", "B", "C", "D"] {
                td.create(name)?;
            }
            Ok(())
        },
    );
    plan.wait(
        "готовность фикстур A–D",
        READY_TIMEOUT,
        |_, td| {
            for name in ["A", "B", "C", "D"] {
                if let Poll::Pending(state) = td.ready(name) {
                    return Poll::Pending(state);
                }
            }
            Poll::Ready(())
        },
    );
    plan.act("raw-режим и поток OSC у A–D", |_, td| {
        for name in ["A", "B", "C", "D"] {
            td.send(name, Command::Raw { on: true })?;
            td.send(name, STREAM)?;
        }
        td.mark("streams");
        Ok(())
    });
    plan.wait("ход потоков A–D", EVIDENCE_TIMEOUT, |_, td| {
        let since = td.since("streams").t_ns;
        let counts: Vec<usize> = ["A", "B", "C", "D"]
            .iter()
            .map(|name| td.progress_after(name, since))
            .collect();
        poll(counts.iter().all(|count| *count > 0), || {
            format!("квитанции хода A–D {counts:?}")
        })
    });
    plan.act(
        "закрытие A при выводе соседей",
        |_, td| {
            td.mark("close-a");
            runtime::request_close(td.id("A")?).map(|_| ())
        },
    );
    plan.wait_gone(&["A"], "освобождение A и выход фикстуры");
    plan.wait(
        "соседи выводят после освобождения A",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let Some(free) = td.release_of("A") else {
                return Poll::Pending("нет записи освобождения A".to_owned());
            };
            let counts: Vec<usize> = ["B", "C", "D"]
                .iter()
                .map(|name| td.progress_after(name, free.finished_ns))
                .collect();
            let titles = Mark::take().titles - td.since("close-a").titles;
            poll(counts.iter().all(|count| *count > 0) && titles > 0, || {
                format!(
                    "квитанции хода B–D после free A {counts:?}, заголовков с закрытия {titles}"
                )
            })
        },
    );
    let input_b = format!("in{}b", short(&plan.stand.prelude.nonce));
    let typed_b = input_b.clone();
    plan.act(
        "ввод в B после закрытия A",
        move |_, td| runtime::type_text(td.id("B")?, &typed_b).map(|_| ()),
    );
    plan.wait(
        "фикстура B прочитала ввод",
        EVIDENCE_TIMEOUT,
        move |_, td| {
            poll(td.read_contains("B", &input_b), || {
                format!("ввода {input_b} у B нет")
            })
        },
    );
    plan.act("итог закрытия A", |_, td| {
        let free = td.release_of("A");
        let after: Vec<usize> = ["B", "C", "D"]
            .iter()
            .map(|name| free.as_ref().map_or(0, |f| td.progress_after(name, f.finished_ns)))
            .collect();
        let single = td.releases_of("A") == 1;
        td.note("close_a", (&free, &after));
        td.finding(
            "h8-neighbor-after-close",
            Requirement::Required,
            single,
            format!(
                "A закрыта при выводе B, C и D с OSC на каждой строке: освобождений A {}, квитанций хода B–D после free {after:?}, ввод в B прочитан фикстурой; free {:?} мс",
                td.releases_of("A"),
                free.as_ref().map(|f| f.free_ms)
            ),
        );
        Ok(())
    });
    plan.act(
        "два закрытия B и C до следующего tick",
        |_, td| {
            td.mark("close-bc");
            runtime::request_close(td.id("B")?)?;
            runtime::request_close(td.id("C")?)?;
            Ok(())
        },
    );
    plan.wait_gone(&["B", "C"], "освобождение B и C и выход фикстур");
    plan.wait(
        "D выводит после освобождения B и C",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let last = ["B", "C"]
                .iter()
                .filter_map(|name| td.release_of(name).map(|r| r.finished_ns))
                .max()
                .unwrap_or(u64::MAX);
            let count = td.progress_after("D", last);
            poll(count > 0, || {
                format!("квитанций хода D после free B и C {count}")
            })
        },
    );
    let input_d = format!("in{}d", short(&plan.stand.prelude.nonce));
    let typed_d = input_d.clone();
    plan.act(
        "ввод в D после двух закрытий",
        move |_, td| runtime::type_text(td.id("D")?, &typed_d).map(|_| ()),
    );
    plan.wait(
        "фикстура D прочитала ввод",
        EVIDENCE_TIMEOUT,
        move |_, td| {
            poll(td.read_contains("D", &input_d), || {
                format!("ввода {input_d} у D нет")
            })
        },
    );
    plan.act("итог двух закрытий", |_, td| {
        let b = td.release_of("B");
        let c = td.release_of("C");
        let same_pass = matches!((&b, &c), (Some(b), Some(c)) if b.pass == c.pass && b.pass > 0);
        let window = match (&b, &c) {
            (Some(b), Some(c)) => Some((b.started_ns.min(c.started_ns), b.finished_ns.max(c.finished_ns))),
            _ => None,
        };
        let during = window.map_or(0, |(from, to)| td.progress_between("D", from, to));
        let single = td.releases_of("B") == 1 && td.releases_of("C") == 1;
        td.note("close_bc", (&b, &c, during));
        td.finding(
            "h8-two-closes-one-pass",
            Requirement::Required,
            same_pass && single,
            format!(
                "намерения B и C приняты одним проходом, освобождения на проходах {:?} и {:?} (один проход {same_pass}), до следующего tick; D выводила с OSC: квитанций хода D внутри окна двух free {during}, после них вывод и ввод D работают; сообщений за время free B и C {:?}",
                b.as_ref().map(|r| r.pass),
                c.as_ref().map(|r| r.pass),
                (b.as_ref().map(|r| r.messages_during_free), c.as_ref().map(|r| r.messages_during_free))
            ),
        );
        td.drop_view("B")?;
        td.drop_view("C")
    });
}

/// Закрывающаяся D: освобождение задержано, колбэки снятой поверхности отбрасываются у источника.
fn closing(plan: &mut Plan) {
    plan.act(
        "удержание освобождения и намерение закрыть D",
        |_, td| {
            runtime::hold_frees(true)?;
            td.mark("close-d");
            runtime::request_close(td.id("D")?).map(|_| ())
        },
    );
    plan.wait(
        "D закрывается, колбэки её потока отбрасываются у источника",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let phase = td.phase("D");
            let mark = td.since("close-d");
            let now = Mark::take();
            let dropped = now.dropped_at_source - mark.dropped_at_source;
            let wakeups = now.wakeups - mark.wakeups;
            poll(
                phase == Some(Phase::Closing) && dropped > 0 && wakeups > 0,
                || format!("фаза D {phase:?}, отброшено у источника {dropped}, wakeup {wakeups}"),
            )
        },
    );
    plan.act(
        "вставка по привязке у закрывающейся D",
        |_, td| {
            let before = Mark::take();
            let handled = runtime::binding_action(td.id("D")?, "paste_from_clipboard")?;
            let after = Mark::take();
            td.note(
                "closing_paste",
                serde_json::json!({
                    "handled": handled,
                    "dropped_at_source": after.dropped_at_source - before.dropped_at_source,
                    "pushed": after.pushed - before.pushed,
                }),
            );
            td.mark("closing-window");
            Ok(())
        },
    );
    plan.act(
        "снятие удержания освобождения",
        |_, _| runtime::hold_frees(false),
    );
    plan.wait_gone(&["D"], "освобождение D и выход фикстуры");
    plan.act(
        "сообщение для освобождённой D",
        |_, td| {
            td.mark("stale-d");
            let via = runtime::inject_event(
                td.key_of("D")?,
                Event::Title(Some("после free".to_owned())),
            )?;
            td.note("stale_injected_via", via);
            Ok(())
        },
    );
    plan.wait(
        "сообщение освобождённой D отброшено как устаревшее",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let stale = Mark::take().dropped_stale - td.since("stale-d").dropped_stale;
            poll(stale > 0, || format!("устаревших с отметки {stale}"))
        },
    );
    plan.act("итог закрывающейся D", |_, td| {
        let mark = td.since("close-d");
        let window = td.since("closing-window");
        let paste = td.evidence.get("closing_paste").cloned();
        let paste_dropped = paste
            .as_ref()
            .and_then(|value| value["dropped_at_source"].as_u64())
            .unwrap_or(0);
        let free = td.release_of("D");
        let stats = runtime::stats().unwrap_or_default();
        let ok = td.releases_of("D") == 1 && paste_dropped > 0;
        td.finding(
            "h8-closing-callbacks-dropped",
            Requirement::Required,
            ok,
            format!(
                "намерение закрыть D принято, освобождение задержано на {} проходов: за это время отброшено у источника {} сообщений колбэков снятой D (заголовки её потока), wakeup {}; вставка по привязке у закрывающейся D — отброшено {paste_dropped}; после освобождения сообщение для D отброшено как устаревшее; освобождений D {}, free {:?} мс",
                stats.frees_held_passes,
                window.dropped_at_source - mark.dropped_at_source,
                window.wakeups - mark.wakeups,
                td.releases_of("D"),
                free.as_ref().map(|f| f.free_ms)
            ),
        );
        td.note("close_d", &free);
        Ok(())
    });
}

/// Выход ребёнка и явное закрытие раздельно.
fn child_exit(plan: &mut Plan) {
    plan.act(
        "E в центре и F в нижней зоне",
        |stand, td| {
            td.drop_view("D")?;
            td.add(stand, "E", Zone::Center, "e")?;
            td.add(stand, "F", Zone::Bottom, "f")?;
            td.create("E")?;
            td.create("F")
        },
    );
    plan.wait(
        "готовность фикстур E и F",
        READY_TIMEOUT,
        |_, td| {
            for name in ["E", "F"] {
                if let Poll::Pending(state) = td.ready(name) {
                    return Poll::Pending(state);
                }
            }
            Poll::Ready(())
        },
    );
    plan.wait(
        "фикстура E старше порога аварийного выхода",
        EVIDENCE_TIMEOUT,
        |_, td| poll(td.age_ok("E"), || "фикстура E моложе 400 мс".to_owned()),
    );
    plan.act("выход ребёнка E", |_, td| {
        td.mark("exit-e");
        td.send("E", Command::Exit { code: 0 }).map(|_| ())
    });
    plan.wait(
        "ребёнок E вышел, поверхность E жива",
        CLOSE_TIMEOUT,
        |_, td| {
            let (gone, state) = td.child_gone("E");
            let exited = Mark::take().child_exited - td.since("exit-e").child_exited;
            let phase = td.phase("E");
            poll(gone && exited > 0 && phase == Some(Phase::Live), || {
                format!("{state}; child-exited {exited}; фаза {phase:?}")
            })
        },
    );
    plan.act("явное закрытие E", |_, td| {
        td.note(
            "e_live_after_child_exit",
            runtime::phase(td.id("E")?).map(Phase::name),
        );
        runtime::request_close(td.id("E")?).map(|_| ())
    });
    plan.wait("освобождение E", CLOSE_TIMEOUT, |_, td| {
        let phase = td.phase("E");
        poll(phase == Some(Phase::Freed), || format!("фаза E {phase:?}"))
    });
    plan.act("итог E", |_, td| {
        let single = td.releases_of("E") == 1;
        let live = td.evidence.get("e_live_after_child_exit").cloned();
        td.finding(
            "h8-child-exit-keeps-surface",
            Requirement::Required,
            single && live == Some(serde_json::json!("live")),
            format!(
                "после выхода ребёнка E (child-exited доставлен, процесса нет) поверхность {live:?}: непустая команда включает wait-after-command; явное закрытие — освобождений {}",
                td.releases_of("E")
            ),
        );
        Ok(())
    });
    plan.wait(
        "фикстура F старше порога аварийного выхода",
        EVIDENCE_TIMEOUT,
        |_, td| poll(td.age_ok("F"), || "фикстура F моложе 400 мс".to_owned()),
    );
    plan.act("выход ребёнка F", |_, td| {
        td.mark("exit-f");
        td.send("F", Command::Exit { code: 0 }).map(|_| ())
    });
    plan.wait(
        "ребёнок F вышел, поверхность F жива",
        CLOSE_TIMEOUT,
        |_, td| {
            let (gone, state) = td.child_gone("F");
            let exited = Mark::take().child_exited - td.since("exit-f").child_exited;
            let phase = td.phase("F");
            poll(gone && exited > 0 && phase == Some(Phase::Live), || {
                format!("{state}; child-exited {exited}; фаза {phase:?}")
            })
        },
    );
    plan.act(
        "клавиша в F и намерение пробы в одном обороте",
        |_, td| {
            let before = runtime::stats().unwrap_or_default().close_sources;
            let id = td.id("F")?;
            let typed = runtime::type_text(id, "x")?;
            let probe = runtime::request_close(id)
                .map(|key| key.to_string())
                .unwrap_or_else(|reason| format!("отклонено: {reason}"));
            td.note(
            "f_close",
            serde_json::json!({ "key_handled": typed, "probe": probe, "sources_before": before }),
        );
            Ok(())
        },
    );
    plan.wait("освобождение F", CLOSE_TIMEOUT, |_, td| {
        let phase = td.phase("F");
        poll(phase == Some(Phase::Freed), || format!("фаза F {phase:?}"))
    });
    plan.act("итог F", |_, td| {
        let sources = runtime::stats().unwrap_or_default().close_sources;
        let before: BTreeMap<String, u64> = td
            .evidence
            .get("f_close")
            .and_then(|value| serde_json::from_value(value["sources_before"].clone()).ok())
            .unwrap_or_default();
        let delta = |key: &str| {
            sources.get(key).copied().unwrap_or(0) - before.get(key).copied().unwrap_or(0)
        };
        let from_callback = delta("close-surface-cb:scheduled") + delta("close-surface-cb:already-closing");
        let from_probe = delta("probe:scheduled") + delta("probe:already-closing");
        let single = td.releases_of("F") == 1;
        td.note("f_close_sources", &sources);
        td.finding(
            "h8-child-exit-key-and-close",
            Requirement::Required,
            single && from_callback == 1 && from_probe == 1,
            format!(
                "после выхода ребёнка F клавиша закрыла поверхность через close_surface_cb ({from_callback}) в одном обороте с намерением пробы ({from_probe}): освобождений {}",
                td.releases_of("F")
            ),
        );
        td.drop_view("E")?;
        td.drop_view("F")
    });
}

/// Закрытие против выдачи вставки и против отклонения подтверждения OSC 52.
fn clipboard_races(plan: &mut Plan) {
    plan.act("G и H, текст на доске", |stand, td| {
        let text = format!("teardown{}", short(&stand.prelude.nonce));
        if !clipboard::write_text(&text) {
            return Err(format!("доска {} не приняла текст", td.board));
        }
        td.note("paste_text", &text);
        td.add(stand, "G", Zone::Right, "g")?;
        td.add(stand, "H", Zone::Bottom, "h")?;
        td.create("G")?;
        td.create("H")
    });
    plan.wait(
        "готовность фикстур G и H",
        READY_TIMEOUT,
        |_, td| {
            for name in ["G", "H"] {
                if let Poll::Pending(state) = td.ready(name) {
                    return Poll::Pending(state);
                }
            }
            Poll::Ready(())
        },
    );
    plan.act(
        "удержание выдачи и вставка в G",
        |_, td| {
            td.mark("race-g");
            runtime::hold_clipboard(Hold {
                serves: true,
                denies: false,
            })?;
            let handled = runtime::binding_action(td.id("G")?, "paste_from_clipboard")?;
            td.note("g_paste_handled", handled);
            Ok(())
        },
    );
    plan.wait(
        "чтение G занесено, выдача задержана",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let pending = td.id("G").ok().and_then(runtime::clipboard);
            let held = Mark::take().clipboard_held - td.since("race-g").clipboard_held;
            let ok = pending
                .as_ref()
                .is_some_and(|(pending, stats)| *pending == 1 && stats.reads_started == 1);
            poll(ok && held > 0, || {
                format!("реестр G {pending:?}, задержано {held}")
            })
        },
    );
    plan.act(
        "закрытие G при задержанной выдаче",
        |_, td| runtime::request_close(td.id("G")?).map(|_| ()),
    );
    plan.wait_gone(&["G"], "освобождение G и выход фикстуры");
    plan.act("снятие удержания выдачи", |_, _| {
        runtime::hold_clipboard(Hold::default())
    });
    plan.wait(
        "поздняя выдача G пропущена по снятому ID",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let late = Mark::take().late_skipped - td.since("race-g").late_skipped;
            poll(late > 0, || format!("пропущенных поздних {late}"))
        },
    );
    plan.act("итог гонки с выдачей", |_, td| {
        let mark = td.since("race-g");
        let now = Mark::take();
        let free = td.release_of("G");
        let text = td
            .evidence
            .get("paste_text")
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        let leaked = td.read_contains("G", &text);
        let stats = free.as_ref().and_then(|f| f.clipboard.clone());
        let denied_on_close = stats.as_ref().map_or(0, |s| s.denied_on_close);
        let pending_after = free.as_ref().map_or(usize::MAX, |f| f.clipboard_pending_after_free);
        let ok = denied_on_close == 1
            && pending_after == 0
            && now.late_skipped - mark.late_skipped == 1
            && !leaked
            && td.releases_of("G") == 1;
        td.note("race_g", &free);
        td.finding(
            "h8-close-vs-paste-serve",
            Requirement::Required,
            ok,
            format!(
                "чтение вставки G занесено (STARTED), выдача задержана; закрытие отклонило ожидающий запрос на живой поверхности ({denied_on_close}), после free ожидающих {pending_after}; поздняя выдача пропущена по снятому ID ({}), текст доски до ребёнка не дошёл ({})",
                now.late_skipped - mark.late_skipped,
                !leaked
            ),
        );
        Ok(())
    });
    plan.act(
        "удержание отклонения и запрос OSC 52 из H",
        |_, td| {
            td.mark("race-h");
            runtime::hold_clipboard(Hold {
                serves: false,
                denies: true,
            })?;
            td.send(
                "H",
                Command::Print {
                    text: OSC52_READ.to_owned(),
                },
            )
            .map(|_| ())
        },
    );
    plan.wait(
        "чтение H выдано и ушло в подтверждение, отклонение задержано",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let pending = td.id("H").ok().and_then(runtime::clipboard);
            let held = Mark::take().clipboard_held - td.since("race-h").clipboard_held;
            let ok = pending
                .as_ref()
                .is_some_and(|(pending, stats)| *pending == 1 && stats.completed_to_confirm == 1);
            poll(ok && held > 0, || {
                format!("реестр H {pending:?}, задержано {held}")
            })
        },
    );
    plan.act(
        "закрытие H при задержанном отклонении",
        |_, td| runtime::request_close(td.id("H")?).map(|_| ()),
    );
    plan.wait_gone(&["H"], "освобождение H и выход фикстуры");
    plan.act(
        "снятие удержания отклонения",
        |_, _| runtime::hold_clipboard(Hold::default()),
    );
    plan.wait(
        "позднее отклонение H пропущено по снятому ID",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let late = Mark::take().late_skipped - td.since("race-h").late_skipped;
            poll(late > 0, || format!("пропущенных поздних {late}"))
        },
    );
    plan.act("итог гонки с отклонением", |_, td| {
        let mark = td.since("race-h");
        let now = Mark::take();
        let free = td.release_of("H");
        let leaked = td.read_contains("H", "\x1b]52;");
        let stats = free.as_ref().and_then(|f| f.clipboard.clone());
        let denied_on_close = stats.as_ref().map_or(0, |s| s.denied_on_close);
        let pending_after = free.as_ref().map_or(usize::MAX, |f| f.clipboard_pending_after_free);
        let ok = denied_on_close == 1
            && pending_after == 0
            && now.late_skipped - mark.late_skipped == 1
            && !leaked
            && td.releases_of("H") == 1;
        td.note("race_h", &free);
        td.finding(
            "h8-close-vs-confirm-deny",
            Requirement::Required,
            ok,
            format!(
                "OSC 52 из H: чтение выдано и тем же токеном ушло в подтверждение (clipboard-read = ask), отклонение задержано; закрытие отклонило ожидающее подтверждение на живой поверхности ({denied_on_close}), после free ожидающих {pending_after}; позднее отклонение пропущено ({}), ответа OSC 52 у ребёнка нет ({})",
                now.late_skipped - mark.late_skipped,
                !leaked
            ),
        );
        td.drop_view("G")?;
        td.drop_view("H")
    });
}

/// Повтор создания с имитированным NULL: пробуждение и раскладка дают одну попытку.
fn retry(plan: &mut Plan) {
    plan.act(
        "R: имитированный сбой при спящем дисплее",
        |stand, td| {
            td.add(stand, "R", Zone::Center, "r")?;
            td.mark("retry");
            runtime::inject_null(true)?;
            match runtime::create(td.id("R")?)? {
                Created::Failed {
                    key,
                    failure: Failure::DisplayAsleep,
                    after: AfterFailure::WaitTrigger,
                } => {
                    events::note(format!("R: имитированный сбой {key}, ждёт повода"));
                    Ok(())
                }
                other => Err(format!("R: ожидался повторяемый сбой, а не {other:?}")),
            }
        },
    );
    plan.act(
        "пробуждение экранов и раскладка R в одном обороте",
        |_, td| {
            let view = td.fx("R")?.view.clone();
            runtime::post_screens_did_wake();
            view.setFrameSize(view.frame().size);
            td.note("r_retry_after_triggers", runtime::retry_state(td.id("R")?));
            Ok(())
        },
    );
    plan.wait_ready("R", "R создана повтором, фикстура готова");
    plan.act("итог повтора R", |_, td| {
        let id = td.id("R")?;
        let retry = runtime::retry_state(id);
        let creations = td.creations_of("R");
        let created = Mark::take().created - td.since("retry").created;
        if let Some((key, _)) = runtime::current(id)
            && let Some(fx) = td.fx.get_mut("R")
        {
            fx.key = Some(key);
        }
        let triggers = retry.as_ref().map(|r| r.triggers.clone()).unwrap_or_default();
        let ok = retry.as_ref().is_some_and(|r| r.attempts == 2 && r.created)
            && created == 1
            && creations.len() == 2
            && creations[0].injected == Some(true)
            && creations[1].trigger == Trigger::Wake
            && triggers == [(Trigger::Wake, "scheduled"), (Trigger::Layout, "coalesced")];
        td.note("r_creations", &creations);
        td.note("r_retry", &retry);
        td.finding(
            "h14-retry-plumbing",
            Requirement::Required,
            ok,
            format!(
                "NULL имитирован стендом при заданном сне дисплея (ghostty_surface_new не вызывался): сбой повторяемый; пробуждение экранов и путь раскладки в одном обороте — поводы {triggers:?}, попыток {:?}, создано поверхностей {created}; это проверка механизма, не сна дисплея",
                retry.as_ref().map(|r| r.attempts)
            ),
        );
        Ok(())
    });
    plan.act(
        "P: имитированный неповторяемый сбой",
        |stand, td| {
            td.add(stand, "P", Zone::Bottom, "p")?;
            runtime::inject_null(false)?;
            match runtime::create(td.id("P")?)? {
                Created::Failed {
                    failure: Failure::Permanent,
                    after: AfterFailure::GiveUp,
                    ..
                } => {}
                other => return Err(format!("P: ожидался неповторяемый сбой, а не {other:?}")),
            }
            td.mark("permanent");
            let view = td.fx("P")?.view.clone();
            runtime::post_screens_did_wake();
            view.setFrameSize(view.frame().size);
            runtime::request_pass();
            Ok(())
        },
    );
    plan.wait(
        "два прохода после поводов у P",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let passes = Mark::take().passes - td.since("permanent").passes;
            if passes < 2 {
                runtime::request_pass();
            }
            poll(passes >= 2, || format!("проходов {passes}"))
        },
    );
    plan.act("итог неповторяемого сбоя и закрытие R", |_, td| {
        let id = td.id("P")?;
        let retry = runtime::retry_state(id);
        let phase = runtime::phase(id);
        let ok = retry
            .as_ref()
            .is_some_and(|r| r.attempts == 1 && !r.created && r.gave_up.is_some())
            && phase == Some(Phase::WaitingSize);
        td.note("p_retry", &retry);
        td.finding(
            "h14-permanent-not-retried",
            Requirement::Required,
            ok,
            format!(
                "имитированный сбой при бодрствующем дисплее неповторяемый: после пробуждения экранов и раскладки попыток {:?}, отказ {:?}, фаза {phase:?}",
                retry.as_ref().map(|r| r.attempts),
                retry.as_ref().and_then(|r| r.gave_up)
            ),
        );
        runtime::request_close(td.id("R")?).map(|_| ())
    });
    plan.wait_gone(&["R"], "освобождение R и выход фикстуры");
}

fn final_checks(plan: &mut Plan) {
    plan.wait(
        "процессов фикстур не осталось",
        EVIDENCE_TIMEOUT,
        |_, td| {
            let alive: Vec<i32> = td
                .pids
                .iter()
                .copied()
                .filter(|pid| surface_env::process_alive(*pid))
                .collect();
            poll(alive.is_empty(), || format!("живы {alive:?}"))
        },
    );
    plan.act("ожидающие запросы буфера обмена после освобождений", |_, td| {
        let stats = runtime::stats().unwrap_or_default();
        let pending: Vec<(String, usize)> = stats
            .releases
            .iter()
            .filter(|r| r.clipboard_pending_after_free > 0)
            .map(|r| (r.key.clone(), r.clipboard_pending_after_free))
            .collect();
        let denied: u64 = stats
            .releases
            .iter()
            .filter_map(|r| r.clipboard.as_ref().map(|c| c.denied_on_close))
            .sum();
        td.finding(
            "h8-clipboard-pending-zero",
            Requirement::Required,
            pending.is_empty() && denied >= 2,
            format!(
                "освобождений {}, с ожидающими запросами после free {pending:?}; отклонено при закрытии всего {denied}",
                stats.releases.len()
            ),
        );
        td.findings.push((
            "h14-display-sleep".to_owned(),
            Requirement::Required,
            Verdict::PendingManual,
            "настоящий сон дисплея под caffeinate не наступает: создание во время сна и видимая поверхность после пробуждения проверяются в задаче 19 по make probe-ghostty-demo ARGS=\"--defer-create 20\"".to_owned(),
        ));
        td.pids.sort_unstable();
        Ok(())
    });
}

/// Итог в отчёт и виды для проверки, что ни один не держит ручку.
fn cleanup(td: &Shared, report: &mut Report) -> Vec<Retained<SurfaceView>> {
    let mut td = td.borrow_mut();
    if let Some(timer) = td.tick_timer.take() {
        timer.invalidate();
    }
    let _ = runtime::hold_frees(false);
    let _ = runtime::hold_clipboard(Hold::default());
    let counted: Vec<&Cycle> = td.cycles.iter().filter(|cycle| !cycle.warmup).collect();
    report.metric(
        "teardown_surface_new",
        "ms",
        counted.iter().map(|cycle| cycle.new_ms).collect(),
    );
    report.metric(
        "teardown_surface_free",
        "ms",
        counted.iter().map(|cycle| cycle.free_ms).collect(),
    );
    report.metric(
        "teardown_loop_gap_around_new",
        "ms",
        counted
            .iter()
            .filter_map(|cycle| cycle.loop_gap_new_ms)
            .collect(),
    );
    report.metric(
        "teardown_loop_gap_around_free",
        "ms",
        counted
            .iter()
            .filter_map(|cycle| cycle.loop_gap_free_ms)
            .collect(),
    );
    report.push_json("teardown_cycles", &td.cycles);
    report.push_json("teardown_evidence", &td.evidence);
    report.push_json("teardown_marks", &td.marks);
    report.push_json("teardown_fixture_pids", &td.pids);
    report.push_json("visibility_shield", &td.shield);
    report.push("occlusion_masked", visibility::occlusion_masked());
    report.push("loop_ticks", td.ticks.borrow().len());
    for (name, requirement, verdict, reason) in std::mem::take(&mut td.findings) {
        report.finding(&name, requirement, verdict, reason);
    }
    clipboard::release_named(&td.board);
    clipboard::use_board(Board::General);
    let mut views: Vec<Retained<SurfaceView>> = vec![td.env.view.clone()];
    views.extend(td.fx.values().map(|fx| fx.view.clone()));
    views
}

fn short(nonce: &str) -> String {
    nonce
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(6)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_gap_spans_the_ticks_around_the_interval() {
        let ms = 1_000_000;
        let ticks = [0, 2 * ms, 4 * ms, 30 * ms, 32 * ms];
        assert_eq!(loop_gap_ms(&ticks, 5 * ms, 25 * ms), Some(26.0));
        assert_eq!(loop_gap_ms(&ticks, 4 * ms, 30 * ms), Some(26.0));
        assert_eq!(loop_gap_ms(&ticks, 31 * ms, 31 * ms), Some(2.0));
        assert_eq!(
            loop_gap_ms(&ticks, 33 * ms, 34 * ms),
            None,
            "отметки после нет"
        );
        assert_eq!(loop_gap_ms(&[5 * ms], ms, 2 * ms), None, "отметки до нет");
    }

    #[test]
    fn short_nonce_keeps_six_alphanumerics() {
        assert_eq!(short("a1-b2-c3d4e5"), "a1b2c3");
    }

    #[test]
    fn stream_runs_longer_than_the_neighbor_part() {
        let Command::Stream {
            lines, interval_us, ..
        } = STREAM
        else {
            panic!("поток соседей — команда stream");
        };
        assert!(u64::from(lines) * interval_us >= 30_000_000);
    }
}
