use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use super::*;
use crate::attrib::{OwnerRecord, Panel};
use crate::proc::fake::{FakeSource, listed};
use crate::proc::{BsdInfo, Listed, ReadError, Usage};

const APPLE: Timebase = Timebase {
    numer: 125,
    denom: 3,
};
/// Секунда CPU в тиках Mach при `timebase` 125/3.
const SECOND_MACH: u128 = 24_000_000;
const S: u64 = 1_000_000_000;

fn config() -> Config {
    Config {
        timebase: APPLE,
        steps: STEPS,
    }
}

fn secs(values: &[f64]) -> Vec<u64> {
    values.iter().map(|s| (s * S as f64) as u64).collect()
}

fn visibility(to: Visibility) -> Control {
    Control {
        visibility: Some(to),
        ..Control::default()
    }
}

fn stop() -> Control {
    Control {
        stop: true,
        ..Control::default()
    }
}

/// Подставные часы и управление по сценарию. Ожидание не спит: часы сразу переходят к сроку
/// или к ближайшему управлению. `lag` — поздние пробуждения: срок → насколько позже.
struct FakeTimer {
    clock: Rc<Cell<u64>>,
    script: VecDeque<(u64, Control)>,
    lag: BTreeMap<u64, u64>,
}

impl FakeTimer {
    fn new(clock: &Rc<Cell<u64>>, script: Vec<(f64, Control)>) -> FakeTimer {
        assert!(
            script.last().is_some_and(|(_, control)| control.stop),
            "сценарий должен кончаться остановкой"
        );
        FakeTimer {
            clock: Rc::clone(clock),
            script: script
                .into_iter()
                .map(|(at, control)| ((at * S as f64) as u64, control))
                .collect(),
            lag: BTreeMap::new(),
        }
    }
}

impl Timer for FakeTimer {
    fn now_ns(&self) -> u64 {
        self.clock.get()
    }

    fn wait_until(&mut self, deadline_ns: u64) -> Wake {
        let now = self.clock.get();
        let wake_ns = deadline_ns.max(now) + self.lag.get(&deadline_ns).copied().unwrap_or(0);
        if self.script.front().is_some_and(|(at, _)| *at <= wake_ns) {
            let (at, control) = self.script.pop_front().unwrap();
            self.clock.set(now.max(at));
            return Wake::Control(control);
        }
        self.clock.set(wake_ns);
        Wake::Deadline
    }
}

type Edit = Box<dyn FnOnce(&mut FakeSource)>;

/// Подставной источник поверх [`FakeSource`]: считает вызовы, наращивает CPU каждого процесса
/// по подставным часам (`load_pct` одного ядра), тратит `list_cost_ns` подставного времени на
/// список и правит мир перед заданным по счёту вызовом `list`.
struct World {
    inner: RefCell<FakeSource>,
    clock: Rc<Cell<u64>>,
    load_pct: u128,
    list_cost_ns: u64,
    lists: Cell<usize>,
    usages: Cell<usize>,
    sessions: Cell<usize>,
    edits: RefCell<BTreeMap<usize, Edit>>,
}

impl World {
    fn new(inner: FakeSource, clock: &Rc<Cell<u64>>) -> World {
        World {
            inner: RefCell::new(inner),
            clock: Rc::clone(clock),
            load_pct: 50,
            list_cost_ns: 0,
            lists: Cell::new(0),
            usages: Cell::new(0),
            sessions: Cell::new(0),
            edits: RefCell::new(BTreeMap::new()),
        }
    }

    fn before_list(&self, call: usize, edit: impl FnOnce(&mut FakeSource) + 'static) {
        self.edits.borrow_mut().insert(call, Box::new(edit));
    }
}

impl Source for World {
    fn list(&self) -> Result<Vec<Listed>, String> {
        let call = self.lists.get() + 1;
        self.lists.set(call);
        if let Some(edit) = self.edits.borrow_mut().remove(&call) {
            edit(&mut self.inner.borrow_mut());
        }
        self.clock.set(self.clock.get() + self.list_cost_ns);
        self.inner.borrow().list()
    }

    fn usage(&self, pid: i32) -> Result<Usage, ReadError> {
        self.usages.set(self.usages.get() + 1);
        let mut usage = self.inner.borrow().usage(pid)?;
        let ticks =
            u128::from(self.clock.get()) * SECOND_MACH * self.load_pct / (100 * 1_000_000_000);
        usage.user_mach += u64::try_from(ticks).unwrap();
        Ok(usage)
    }

    fn session(&self, pid: i32) -> Result<i32, ReadError> {
        self.sessions.set(self.sessions.get() + 1);
        self.inner.borrow().session(pid)
    }

    fn bsd_info(&self, pid: i32) -> Result<BsdInfo, ReadError> {
        self.inner.borrow().bsd_info(pid)
    }
}

fn add(source: &mut FakeSource, pid: i32, ppid: i32) -> Key {
    let entry = listed(pid, ppid, 100);
    let key = Key::of(&entry);
    source.add(entry, 5_000 + pid as u64, 0);
    key
}

/// `panels` панелей: у каждой процесс поверхности и его ребёнок; один посторонний процесс.
fn panels(count: usize) -> (FakeSource, Claims) {
    let mut source = FakeSource::default();
    add(&mut source, 50, 1);
    let mut claims = Claims::default();
    for i in 0..count {
        let surface = 1000 + 10 * i as i32;
        let key = add(&mut source, surface, 1);
        add(&mut source, surface + 1, surface);
        claims.panels.push(Panel {
            id: format!("p{i}"),
            surface: vec![key],
            record: None,
        });
    }
    (source, claims)
}

fn run_world(
    world: &World,
    clock: &Rc<Cell<u64>>,
    claims: Claims,
    visibility: Visibility,
    script: Vec<(f64, Control)>,
) -> (SamplerLog, Vec<Published>) {
    let mut timer = FakeTimer::new(clock, script);
    run_timer(world, &mut timer, claims, visibility)
}

fn run_timer(
    world: &World,
    timer: &mut FakeTimer,
    claims: Claims,
    visibility: Visibility,
) -> (SamplerLog, Vec<Published>) {
    let mut published = Vec::new();
    let log = run(world, timer, config(), claims, visibility, |p| {
        published.push(p)
    })
    .unwrap();
    (log, published)
}

fn dues(log: &SamplerLog) -> Vec<u64> {
    log.passes.iter().map(|pass| pass.due_ns).collect()
}

fn counts(pass: &PassRecord) -> &PassCounts {
    pass.outcome.as_ref().unwrap()
}

fn owner<'a>(published: &'a Published, id: &str) -> &'a Numbers {
    let owner = Owner::Panel(id.to_owned());
    &published
        .owners
        .iter()
        .find(|(o, _)| *o == owner)
        .unwrap()
        .1
}

#[test]
fn wakeups_and_passes_do_not_depend_on_panel_count() {
    let mut seen = Vec::new();
    for count in [1, 4, 16] {
        let clock = Rc::new(Cell::new(0));
        let (source, claims) = panels(count);
        let world = World::new(source, &clock);
        let (log, published) = run_world(
            &world,
            &clock,
            claims,
            Visibility::Visible,
            vec![(61.0, stop())],
        );
        let expected: Vec<u64> = (0..=30).map(|k| k * 2 * S).collect();
        assert_eq!(dues(&log), expected, "{count} панелей");
        assert_eq!(
            world.lists.get(),
            31,
            "{count} панелей: один список на проход"
        );
        assert_eq!(
            world.usages.get(),
            31 * 2 * count,
            "читается только множество"
        );
        for pass in &log.passes {
            assert_eq!(counts(pass).set_size, 2 * count);
            assert_eq!(counts(pass).panels, count);
            assert_eq!(pass.duration_ns, 0);
        }
        assert_eq!(published.len(), 31);
        assert_eq!(published[1].owners.len(), count + 1, "панели и «Git»");
        seen.push((log.wakeups, log.passes.len(), log.skipped_deadlines));
    }
    let expected = Wakeups {
        deadline: 31,
        control: 1,
        early: 0,
    };
    assert_eq!(seen, vec![(expected, 31, 0); 3]);
}

#[test]
fn background_step_is_ten_seconds_for_any_panel_count() {
    for count in [1, 16] {
        let clock = Rc::new(Cell::new(0));
        let (source, claims) = panels(count);
        let world = World::new(source, &clock);
        let (log, _) = run_world(
            &world,
            &clock,
            claims,
            Visibility::Background,
            vec![(61.0, stop())],
        );
        assert_eq!(dues(&log), secs(&[0.0, 10.0, 20.0, 30.0, 40.0, 50.0, 60.0]));
        assert_eq!(log.wakeups.total(), 8);
    }
}

#[test]
fn step_switch_keeps_one_anchor() {
    let clock = Rc::new(Cell::new(0));
    let (source, claims) = panels(2);
    let world = World::new(source, &clock);
    let (log, _) = run_world(
        &world,
        &clock,
        claims,
        Visibility::Visible,
        vec![
            (5.0, visibility(Visibility::Background)),
            (25.0, visibility(Visibility::Visible)),
            (31.0, stop()),
        ],
    );
    assert_eq!(
        dues(&log),
        secs(&[0.0, 2.0, 4.0, 14.0, 24.0, 26.0, 28.0, 30.0])
    );
    let switches: Vec<(u64, Visibility, u64)> = log
        .switches
        .iter()
        .map(|s| (s.at_ns, s.to, s.deadline_ns))
        .collect();
    assert_eq!(
        switches,
        vec![
            (5 * S, Visibility::Background, 14 * S),
            (25 * S, Visibility::Visible, 26 * S),
        ]
    );
    let steps: Vec<Visibility> = log.passes.iter().map(|p| p.visibility).collect();
    use Visibility::{Background as B, Visible as V};
    assert_eq!(steps, vec![V, V, V, B, B, V, V, V]);
}

#[test]
fn becoming_visible_after_a_stale_pass_is_due_at_once() {
    let clock = Rc::new(Cell::new(0));
    let (source, claims) = panels(1);
    let world = World::new(source, &clock);
    let (log, _) = run_world(
        &world,
        &clock,
        claims,
        Visibility::Background,
        vec![(15.0, visibility(Visibility::Visible)), (20.0, stop())],
    );
    assert_eq!(dues(&log), secs(&[0.0, 10.0, 15.0, 17.0, 19.0]));
    assert_eq!(log.switches[0].deadline_ns, 15 * S);
}

#[test]
fn toggle_storm_neither_postpones_nor_adds_passes() {
    use Visibility::{Background as B, Visible as V};
    for (last, expected) in [(V, secs(&[0.0, 2.0, 4.0, 6.0])), (B, secs(&[0.0, 2.0]))] {
        let clock = Rc::new(Cell::new(0));
        let (source, claims) = panels(1);
        let world = World::new(source, &clock);
        let mut script = Vec::new();
        for (i, at) in [2.5, 2.6, 2.7, 2.8, 2.9, 3.0, 3.1].into_iter().enumerate() {
            script.push((at, visibility(if i % 2 == 0 { B } else { V })));
        }
        script.push((3.2, visibility(last)));
        script.push((7.0, stop()));
        let (log, _) = run_world(&world, &clock, claims, V, script);
        assert_eq!(dues(&log), expected, "череда кончается {last:?}");
        assert_eq!(log.wakeups.control, 9);
        let last_switch = log.switches.last().unwrap();
        let next = if last == V { 4 * S } else { 12 * S };
        assert_eq!(last_switch.deadline_ns, next);
    }
}

#[test]
fn unchanged_visibility_is_not_a_switch() {
    let clock = Rc::new(Cell::new(0));
    let (source, claims) = panels(1);
    let world = World::new(source, &clock);
    let (log, _) = run_world(
        &world,
        &clock,
        claims,
        Visibility::Visible,
        vec![(3.0, visibility(Visibility::Visible)), (5.0, stop())],
    );
    assert!(log.switches.is_empty());
    assert_eq!(dues(&log), secs(&[0.0, 2.0, 4.0]));
}

#[test]
fn overrun_skips_missed_deadlines_without_catch_up() {
    let clock = Rc::new(Cell::new(0));
    let (source, claims) = panels(1);
    let mut world = World::new(source, &clock);
    world.list_cost_ns = 3 * S;
    let (log, _) = run_world(
        &world,
        &clock,
        claims,
        Visibility::Visible,
        vec![(9.0, stop())],
    );
    assert_eq!(dues(&log), secs(&[0.0, 4.0, 8.0]));
    for pass in &log.passes {
        assert_eq!(pass.duration_ns, 3 * S);
        assert_eq!(pass.late_ns, 0);
        assert_eq!(pass.skipped, 1);
    }
    assert_eq!(log.skipped_deadlines, 3);
}

#[test]
fn toggle_after_skipped_deadlines_keeps_the_grid() {
    let clock = Rc::new(Cell::new(0));
    let (source, claims) = panels(1);
    let mut world = World::new(source, &clock);
    world.list_cost_ns = 3 * S;
    let (log, _) = run_world(
        &world,
        &clock,
        claims,
        Visibility::Visible,
        vec![
            (3.5, visibility(Visibility::Background)),
            (3.6, visibility(Visibility::Visible)),
            (9.0, stop()),
        ],
    );
    assert_eq!(dues(&log), secs(&[0.0, 4.0, 8.0]));
    let deadlines: Vec<u64> = log.switches.iter().map(|s| s.deadline_ns).collect();
    assert_eq!(deadlines, vec![12 * S, 4 * S]);

    let clock = Rc::new(Cell::new(0));
    let (source, claims) = panels(1);
    let world = World::new(source, &clock);
    let mut timer = FakeTimer::new(
        &clock,
        vec![
            (9.5, visibility(Visibility::Background)),
            (9.6, visibility(Visibility::Visible)),
            (11.0, stop()),
        ],
    );
    timer.lag.insert(4 * S, 5 * S);
    let (log, _) = run_timer(&world, &mut timer, claims, Visibility::Visible);
    assert_eq!(dues(&log), secs(&[0.0, 2.0, 4.0, 10.0]));
    let deadlines: Vec<u64> = log.switches.iter().map(|s| s.deadline_ns).collect();
    assert_eq!(deadlines, vec![18 * S, 10 * S]);
}

#[test]
fn late_wake_runs_one_pass_and_returns_to_the_grid() {
    let clock = Rc::new(Cell::new(0));
    let (source, claims) = panels(1);
    let world = World::new(source, &clock);
    let mut timer = FakeTimer::new(&clock, vec![(13.0, stop())]);
    timer.lag.insert(4 * S, 5 * S);
    let (log, _) = run_timer(&world, &mut timer, claims, Visibility::Visible);
    assert_eq!(dues(&log), secs(&[0.0, 2.0, 4.0, 10.0, 12.0]));
    let late = &log.passes[2];
    assert_eq!(
        (late.start_ns, late.late_ns, late.skipped),
        (9 * S, 5 * S, 2)
    );
    assert_eq!(log.skipped_deadlines, 2);
}

#[test]
fn record_counts_set_size_and_partial_reads() {
    let clock = Rc::new(Cell::new(0));
    let (mut source, claims) = panels(1);
    source.usage.insert(1001, Err(ReadError::Denied));
    let world = World::new(source, &clock);
    let (log, published) = run_world(
        &world,
        &clock,
        claims,
        Visibility::Visible,
        vec![(3.0, stop())],
    );
    let first = counts(&log.passes[0]);
    assert_eq!(first.listed, 3);
    assert_eq!(first.set_size, 2);
    assert_eq!(first.partial_reads, 1);
    assert_eq!(first.cpu_interval_ns, None);
    assert_eq!(first.total.cpu_pct, None);
    assert!(first.total.partial);
    let second = counts(&log.passes[1]);
    assert_eq!(second.cpu_interval_ns, Some(2 * S));
    assert_eq!(second.partial_reads, 1);
    let panel = owner(&published[1], "p0");
    assert_eq!(panel.members, 2);
    assert_eq!(panel.memory_bytes, 1 << 20, "учтена только поверхность");
    assert!(panel.partial, "отказ чтения не скрыт");
    let pct = panel.cpu_pct.unwrap();
    assert!((pct - 50.0).abs() < 1e-6, "{pct}");
}

#[test]
fn failed_list_is_recorded_and_the_base_stays_at_the_last_good_pass() {
    let clock = Rc::new(Cell::new(0));
    let (source, claims) = panels(1);
    let world = World::new(source, &clock);
    world.before_list(2, |s| s.list_error = Some("sysctl: ENOMEM".to_owned()));
    world.before_list(3, |s| s.list_error = None);
    let (log, published) = run_world(
        &world,
        &clock,
        claims,
        Visibility::Visible,
        vec![(5.0, stop())],
    );
    assert_eq!(log.passes.len(), 3);
    assert_eq!(log.passes[1].outcome, Err("sysctl: ENOMEM".to_owned()));
    let indices: Vec<u64> = published.iter().map(|p| p.pass).collect();
    assert_eq!(indices, vec![1, 3], "неудачный проход не публикуется");
    let third = counts(&log.passes[2]);
    assert_eq!(third.cpu_interval_ns, Some(4 * S));
    let pct = third.total.cpu_pct.unwrap();
    assert!((pct - 100.0).abs() < 1e-6, "два процесса по 50%: {pct}");
    let digest = log.digest();
    assert_eq!((digest.passes, digest.failed_passes), (3, 1));
}

#[test]
fn exited_member_keeps_the_owner_partial_instead_of_vanishing() {
    let clock = Rc::new(Cell::new(0));
    let (source, claims) = panels(1);
    let world = World::new(source, &clock);
    world.before_list(2, |s| {
        s.listed.retain(|l| l.pid != 1001);
        s.usage.remove(&1001);
        s.info.remove(&1001);
    });
    let (log, published) = run_world(
        &world,
        &clock,
        claims,
        Visibility::Visible,
        vec![(3.0, stop())],
    );
    let panel = owner(&published[1], "p0");
    assert_eq!(panel.members, 2, "вышедший член остаётся в сводке");
    assert!(panel.partial);
    let second = counts(&log.passes[1]);
    assert_eq!(second.set_size, 1);
    assert_eq!(second.partial_reads, 0);
    assert!(second.total.partial);
}

#[test]
fn pass_runs_attribution_with_the_session_sweep() {
    let clock = Rc::new(Cell::new(0));
    let mut source = FakeSource::default();
    add(&mut source, 50, 1);
    let host = add(&mut source, 2000, 1);
    let program = add(&mut source, 2001, 2000);
    add(&mut source, 2002, 1);
    source.session.insert(2001, Ok(2001));
    source.session.insert(2002, Ok(2001));
    let claims = Claims {
        panels: vec![Panel {
            id: "p0".to_owned(),
            surface: Vec::new(),
            record: Some(OwnerRecord {
                host,
                programs: vec![program],
                group: program,
                session: program,
            }),
        }],
        git: Vec::new(),
    };
    let world = World::new(source, &clock);
    let (log, published) = run_world(
        &world,
        &clock,
        claims,
        Visibility::Visible,
        vec![(1.0, stop())],
    );
    let first = counts(&log.passes[0]);
    assert_eq!(first.getsid_calls, 4, "getsid на каждый PID списка");
    assert_eq!(
        first.set_size, 3,
        "хост, программа и член сессии вне дерева"
    );
    assert_eq!(owner(&published[0], "p0").members, 3);
}

#[test]
fn claims_change_takes_effect_on_the_next_pass_without_moving_the_deadline() {
    let clock = Rc::new(Cell::new(0));
    let (source, two) = panels(2);
    let one = Claims {
        panels: two.panels[..1].to_vec(),
        git: Vec::new(),
    };
    let world = World::new(source, &clock);
    let change = Control {
        claims: Some(two),
        ..Control::default()
    };
    let (log, published) = run_world(
        &world,
        &clock,
        one,
        Visibility::Visible,
        vec![(3.0, change), (5.0, stop())],
    );
    assert_eq!(dues(&log), secs(&[0.0, 2.0, 4.0]));
    assert_eq!(log.claims_changes, 1);
    let sizes: Vec<usize> = log.passes.iter().map(|p| counts(p).set_size).collect();
    assert_eq!(sizes, vec![2, 2, 4]);
    assert_eq!(published[2].owners.len(), 3);
}

#[test]
fn digest_counts_wakeups_per_minute_and_durations() {
    let clock = Rc::new(Cell::new(0));
    let (source, claims) = panels(1);
    let mut world = World::new(source, &clock);
    world.list_cost_ns = S / 100;
    let (log, _) = run_world(
        &world,
        &clock,
        claims,
        Visibility::Visible,
        vec![(60.0, stop())],
    );
    let digest = log.digest();
    assert_eq!(digest.passes, 30);
    assert_eq!(digest.wakeups.total(), 31);
    let per_minute = digest.wakeups_per_minute.unwrap();
    assert!((per_minute - 31.0).abs() < 1e-9, "{per_minute}");
    assert_eq!(digest.duration_median_ns, Some(S / 100));
    assert_eq!(digest.duration_max_ns, Some(S / 100));
    assert_eq!(
        (digest.set_size_min, digest.set_size_max),
        (Some(2), Some(2))
    );
    assert_eq!(digest.partial_reads_total, 0);
    assert_eq!(digest.partial_passes, 1, "первый проход без базы CPU");
}

#[test]
fn zero_step_is_rejected() {
    let steps = Steps {
        visible_ns: 0,
        background_ns: S,
    };
    assert!(Schedule::new(0, Visibility::Visible, steps).is_err());
    let (source, claims) = panels(1);
    let config = Config {
        timebase: APPLE,
        steps,
    };
    assert!(Sampler::spawn(source, config, claims, Visibility::Visible).is_err());
}

/// Настоящий поток и настоящие часы: только порядок событий, без утверждений о стенном времени.
#[test]
fn thread_follows_visibility_and_claims_and_stops() {
    let (source, claims) = panels(2);
    let one = Claims {
        panels: claims.panels[..1].to_vec(),
        git: Vec::new(),
    };
    let config = Config {
        timebase: APPLE,
        steps: Steps {
            visible_ns: 1_000_000,
            background_ns: 3600 * S,
        },
    };
    let timeout = Duration::from_secs(30);
    let sampler = Sampler::spawn(source, config, one.clone(), Visibility::Visible).unwrap();
    sampler.wait_published(timeout, |p| p.pass >= 3).unwrap();
    sampler.set_visibility(Visibility::Visible);
    sampler.set_claims(one);
    sampler.set_claims(claims);
    let wide = sampler
        .wait_published(timeout, |p| p.owners.len() == 3)
        .unwrap();
    assert_eq!(wide.total.members, 4);
    sampler.set_visibility(Visibility::Background);
    let log = sampler.stop().unwrap();
    assert_eq!(log.claims_changes, 1, "те же заявки поток не будят");
    let switches: Vec<Visibility> = log.switches.iter().map(|s| s.to).collect();
    assert_eq!(switches, vec![Visibility::Background]);
    assert!(log.passes.len() >= 4);
    for (i, pass) in log.passes.iter().enumerate() {
        assert_eq!(pass.index, i as u64 + 1);
        assert!(pass.outcome.is_ok());
    }
    assert_eq!(log.wakeups.deadline, log.passes.len() as u64);
    assert!((1..=3).contains(&log.wakeups.control), "{:?}", log.wakeups);
}

#[test]
fn dropping_the_handle_stops_the_thread() {
    let (source, claims) = panels(1);
    let config = Config {
        timebase: APPLE,
        steps: STEPS,
    };
    let sampler = Sampler::spawn(source, config, claims, Visibility::Visible).unwrap();
    let first = sampler
        .wait_published(Duration::from_secs(30), |p| p.pass == 1)
        .unwrap();
    assert_eq!(first.total.members, 2);
    drop(sampler);
}
