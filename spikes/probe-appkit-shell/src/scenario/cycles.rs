//! Сценарий `cycles`: жизненный цикл объектов сцены. 100 циклов палитры (Cmd-K, «l», Escape) и
//! 100 переключений вкладок центральной полосы (Shift-L) со снимками счётчиков живых экземпляров
//! после 10-го и 100-го цикла, затем закрытие и повторное открытие центральной плитки со списком
//! и закрытие и повторное открытие окна сцены. Сцену собирает сам сценарий, как `idle`: иначе её
//! держал бы делегат до конца процесса.
//!
//! Снимок счётчиков берётся после очистки таблицы событий и возврата `sendEvent` служебного
//! `flagsChanged`, который опустошает пул autorelease итерации цикла, и после прохода
//! отображения. Освобождение прежнего экземпляра проверяется слабыми ссылками Objective-C на его
//! виды и окна и слабыми ссылками Rust на его владельцев. Поздний обратный вызов — вызов, не
//! нашедший владельца живым (`counters::owner`); после закрытия сценарий вызывает источники
//! таких вызовов: уведомление о параметрах отображения, смену оформления, клавиши.

use std::cell::{Cell, RefCell};
use std::rc::{self, Rc};
use std::time::{Duration, Instant};

use objc2::MainThreadOnly;
use objc2::rc::{Retained, Weak, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSApplication, NSEventModifierFlags, NSView, NSWindow, NSWorkspace,
    NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification,
};

use super::counters::{self, Count};
use super::events::{self, Key};
use super::launch::{input_source, require_latin};
use super::tabs::tab_background;
use super::{Poll, Report, Scenario, Stand, Step, StepFailure, Verdict, poll, run_steps};
use crate::chrome;
use crate::palette::{self, Palette};
use crate::results::LiveObjects;
use crate::scene::Scene;
use crate::theme::Choice;
use crate::tiles::{CENTER, SCENE_TABS, Tile};

const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const WARMUP_TIMEOUT: Duration = Duration::from_secs(20);
const CYCLES_TIMEOUT: Duration = Duration::from_secs(90);
const TABS_TIMEOUT: Duration = Duration::from_secs(30);
/// Срок освобождения прежнего экземпляра после закрытия; по его истечении — вывод `fail`.
const RELEASE_GRACE: Duration = Duration::from_secs(3);
/// Цикл, после которого берётся первый снимок; он же конец прогрева.
const WARMUP: usize = 10;
const CYCLES: usize = 100;
/// Проверок подряд, на которых состояние цикла не наступило, до ошибки исполнения.
const MAX_WAITING: usize = 1_000;

const KEY_L: Key = Key {
    label: "l",
    code: 37,
    characters: "l",
    ignoring: "l",
    flags: NSEventModifierFlags::empty(),
};
const KEY_SHIFT_L: Key = Key {
    label: "Shift-L",
    code: 37,
    characters: "L",
    ignoring: "L",
    flags: NSEventModifierFlags::Shift,
};
const KEY_ESCAPE: Key = Key {
    label: "Escape",
    code: 53,
    characters: "\u{1b}",
    ignoring: "\u{1b}",
    flags: NSEventModifierFlags::empty(),
};

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

/// Порог видов строк из раздела Technical Details плана.
fn reuse_limit(visible: usize) -> usize {
    2 * visible + 10
}

fn address<T: ?Sized>(object: &T) -> usize {
    (object as *const T).cast::<()>() as usize
}

/// Классы, чьё число живых экземпляров изменилось, для текста вывода.
fn describe_changes(changes: &[(&str, usize, usize)]) -> String {
    if changes.is_empty() {
        return "нет".to_owned();
    }
    changes
        .iter()
        .map(|(name, before, after)| format!("{name} {before} → {after}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn find<'a>(counts: &'a [Count], name: &str) -> Option<&'a Count> {
    counts.iter().find(|count| count.name == name)
}

/// Палитра держит ровно один экземпляр панели и контроллера: создано по одному, живо по одному.
fn single_palette(counts: &[Count]) -> bool {
    ["ProbePalettePanel", "ProbePaletteController"]
        .iter()
        .all(|name| find(counts, name).is_some_and(|count| count.created == 1 && count.live() == 1))
}

/// Слабые ссылки на объекты закрытого экземпляра: виды, окна и источник данных.
#[derive(Default)]
struct Previous {
    objects: Vec<(String, Weak<AnyObject>)>,
}

impl Previous {
    fn add(&mut self, label: impl Into<String>, object: &AnyObject) {
        self.objects.push((label.into(), Weak::new(object)));
    }

    /// Вид и все его потомки, с именами классов.
    fn add_tree(&mut self, view: &NSView) {
        let object: &AnyObject = view;
        self.add(object.class().name().to_string_lossy(), object);
        for child in view.subviews() {
            self.add_tree(&child);
        }
    }

    /// Число живых объектов и они же по классам: «класс ×число».
    fn alive(&self) -> (usize, Vec<String>) {
        let mut alive: Vec<(&str, usize)> = Vec::new();
        for (label, weak) in &self.objects {
            if weak.load().is_none() {
                continue;
            }
            match alive.iter_mut().find(|(known, _)| *known == label.as_str()) {
                Some((_, count)) => *count += 1,
                None => alive.push((label.as_str(), 1)),
            }
        }
        let total = alive.iter().map(|(_, count)| count).sum();
        let classes = alive
            .into_iter()
            .map(|(label, count)| format!("{label} ×{count}"))
            .collect();
        (total, classes)
    }
}

/// Слабые ссылки Rust на владельцев закрытой сцены.
struct Owners {
    scene: rc::Weak<Scene>,
    palette: rc::Weak<Palette>,
    tiles: rc::Weak<crate::tiles::Tiles>,
    theme: rc::Weak<crate::theme::SceneTheme>,
    commands: rc::Weak<crate::menu::Commands>,
}

impl Owners {
    fn alive(&self) -> Vec<&'static str> {
        [
            ("Scene", self.scene.strong_count()),
            ("Palette", self.palette.strong_count()),
            ("Tiles", self.tiles.strong_count()),
            ("SceneTheme", self.theme.strong_count()),
            ("Commands", self.commands.strong_count()),
        ]
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(name, _)| name)
        .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Open,
    Filter,
    Close,
}

/// Циклы палитры или переключения вкладок до `target`.
struct Driver {
    kind: Kind,
    phase: Phase,
    numbers: Vec<u64>,
    done: usize,
    target: usize,
    waiting: usize,
    /// Проверок после прохода, на которых состояние ещё не наступило.
    late_polls: usize,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Palette,
    Tabs,
}

impl Driver {
    fn new(kind: Kind) -> Driver {
        Driver {
            kind,
            phase: Phase::Open,
            numbers: Vec::new(),
            done: 0,
            target: WARMUP,
            waiting: 0,
            late_polls: 0,
            error: None,
        }
    }
}

struct Run {
    stand: Rc<Stand>,
    scene: RefCell<Option<Rc<Scene>>>,
    report: RefCell<Report>,
    mark: Cell<Option<Duration>>,
    keys: RefCell<Vec<u64>>,
    nudge: Cell<Option<u64>>,
    driver: RefCell<Option<Driver>>,
    snapshots: RefCell<Vec<(&'static str, Vec<Count>)>>,
    /// First responder окна сцены, к которому палитра возвращает фокус.
    responder: Cell<usize>,
    filtered: Cell<usize>,
    palette_visible: Cell<usize>,
    center_visible: Cell<usize>,
    /// Активная вкладка центральной полосы, которую ждёт текущее переключение.
    expected_tab: Cell<usize>,
    /// First responder окна — новая плитка после повторного открытия.
    responder_kept: Cell<bool>,
    menu_items: Cell<isize>,
    scene_menu_items: Cell<isize>,
    previous: RefCell<Previous>,
    owners: RefCell<Option<Owners>>,
    closed_at: Cell<Option<Instant>>,
    appearance: RefCell<Option<Retained<objc2_app_kit::NSAppearance>>>,
    notifications: Cell<usize>,
    released: RefCell<Vec<(&'static str, String)>>,
}

impl Run {
    fn scene(&self) -> Result<Rc<Scene>, String> {
        self.scene
            .borrow()
            .clone()
            .ok_or_else(|| "сцена не открыта".to_owned())
    }

    fn app(&self) -> &NSApplication {
        &self.stand.app
    }

    fn touch(&self) {
        self.mark.set(events::elapsed());
    }

    fn passed(&self) -> bool {
        self.mark
            .get()
            .is_some_and(|mark| events::display_pass_after(mark).is_some())
    }

    fn key_is(&self, window: &NSWindow) -> bool {
        self.app().isActive()
            && self
                .app()
                .keyWindow()
                .is_some_and(|key| events::same(&*key, window))
    }

    fn scene_is_key(&self) -> bool {
        self.scene
            .borrow()
            .as_ref()
            .is_some_and(|scene| self.key_is(&scene.window))
    }

    fn menu_count(&self) -> isize {
        self.app().mainMenu().map_or(-1, |bar| bar.numberOfItems())
    }

    fn state(&self) -> String {
        let scene = self.scene.borrow();
        let palette = scene.as_ref().map(|scene| {
            format!(
                "palette open={} results={} field={:?}",
                scene.palette.is_open(),
                scene.palette.results(),
                scene.palette.field().stringValue().to_string()
            )
        });
        let driver = self
            .driver
            .borrow()
            .as_ref()
            .map(|driver| format!("{:?} {} {:?}", driver.kind, driver.done, driver.phase));
        format!(
            "scene={} {} driver={driver:?} {}",
            scene.is_some(),
            palette.unwrap_or_default(),
            events::focus_state(self.app())
        )
    }

    /// Начало снимка: очистка таблицы событий и служебное событие для опустошения пула.
    fn begin_count(&self) -> Result<(), String> {
        events::clear();
        self.nudge.set(Some(events::nudge()?));
        Ok(())
    }

    fn counted(&self) -> bool {
        self.nudge
            .get()
            .and_then(events::record)
            .and_then(|record| record.send_return)
            .is_some_and(|back| events::display_pass_after(back).is_some())
    }

    fn snapshot(&self, name: &'static str) -> Vec<Count> {
        let counts = autoreleasepool(|_| counters::snapshot());
        let live: Vec<LiveObjects> = counts
            .iter()
            .map(|count| LiveObjects {
                class: count.name.to_owned(),
                created: count.created,
                released: count.released,
                live: count.live(),
            })
            .collect();
        self.report
            .borrow_mut()
            .push_json(&format!("live_{name}"), &live);
        self.snapshots.borrow_mut().push((name, counts.clone()));
        counts
    }

    fn counts(&self, name: &str) -> Vec<Count> {
        self.snapshots
            .borrow()
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, counts)| counts.clone())
            .unwrap_or_default()
    }

    fn parent_responder(&self, scene: &Scene) -> usize {
        scene
            .window
            .firstResponder()
            .map_or(0, |responder| address(&*responder))
    }

    fn palette_opened(&self, scene: &Scene) -> bool {
        let palette = &scene.palette;
        let editing = match (
            palette.field().currentEditor(),
            palette.panel().firstResponder(),
        ) {
            (Some(editor), Some(responder)) => events::same(&*editor, &*responder),
            _ => false,
        };
        palette.is_open() && palette.panel().isVisible() && self.key_is(palette.panel()) && editing
    }

    fn palette_closed(&self, scene: &Scene) -> bool {
        !scene.palette.is_open() && !scene.palette.panel().isVisible() && self.key_is(&scene.window)
    }

    fn visible_rows(table: &crate::list::ListTable) -> usize {
        table.rowsInRect(table.visibleRect()).length
    }

    /// Активная вкладка центральной полосы по фону её слоя.
    fn layer_active(&self, scene: &Scene) -> Option<usize> {
        let panel = scene.tokens().panel;
        scene
            .tiles
            .tile(CENTER)
            .strip()
            .tab_views()
            .iter()
            .position(|tab| tab_background(tab) == Some(panel))
    }

    fn drive(&self) -> Poll<()> {
        let scene = match self.scene() {
            Ok(scene) => scene,
            Err(reason) => {
                if let Some(driver) = self.driver.borrow_mut().as_mut() {
                    driver.error = Some(reason);
                }
                return Poll::Ready(());
            }
        };
        let mut slot = self.driver.borrow_mut();
        let Some(driver) = slot.as_mut() else {
            return Poll::Ready(());
        };
        if driver.error.is_some() {
            return Poll::Ready(());
        }
        if !driver.numbers.is_empty() {
            if !returned_and_passed(&driver.numbers) {
                return Poll::Pending(format!(
                    "{:?} {} {:?}: события не обработаны",
                    driver.kind, driver.done, driver.phase
                ));
            }
            let reached = match (driver.kind, driver.phase) {
                (Kind::Palette, Phase::Open) => {
                    let opened = self.palette_opened(&scene);
                    if opened {
                        self.note_palette_rows(&scene);
                    }
                    opened
                }
                (Kind::Palette, Phase::Filter) => {
                    let filtered = scene.palette.field().stringValue().to_string()
                        == KEY_L.characters
                        && scene.palette.results() == self.filtered.get();
                    if filtered {
                        self.note_palette_rows(&scene);
                    }
                    filtered
                }
                (Kind::Palette, Phase::Close) => {
                    self.palette_closed(&scene)
                        && self.parent_responder(&scene) == self.responder.get()
                }
                (Kind::Tabs, _) => {
                    let expected = self.expected_tab.get();
                    scene.tiles.tile(CENTER).strip().active() == Some(expected)
                        && self.layer_active(&scene) == Some(expected)
                }
            };
            if !reached {
                if driver.waiting == 0 {
                    driver.late_polls += 1;
                }
                driver.waiting += 1;
                if driver.waiting > MAX_WAITING {
                    driver.error = Some(format!(
                        "{:?} {} {:?}: состояние не наступило",
                        driver.kind, driver.done, driver.phase
                    ));
                    return Poll::Ready(());
                }
                return Poll::Pending(format!(
                    "{:?} {} {:?}: ждём состояние",
                    driver.kind, driver.done, driver.phase
                ));
            }
            driver.waiting = 0;
            driver.numbers.clear();
            match driver.kind {
                Kind::Palette => {
                    driver.phase = match driver.phase {
                        Phase::Open => Phase::Filter,
                        Phase::Filter => Phase::Close,
                        Phase::Close => {
                            driver.done += 1;
                            Phase::Open
                        }
                    };
                }
                Kind::Tabs => driver.done += 1,
            }
        }
        if driver.done >= driver.target {
            return Poll::Ready(());
        }
        let key = match (driver.kind, driver.phase) {
            (Kind::Palette, Phase::Open) => Key::CMD_K,
            (Kind::Palette, Phase::Filter) => KEY_L,
            (Kind::Palette, Phase::Close) => KEY_ESCAPE,
            (Kind::Tabs, _) => {
                let len = SCENE_TABS[CENTER];
                let active = scene.tiles.tile(CENTER).strip().active().unwrap_or(0);
                self.expected_tab.set((active + 1) % len);
                KEY_SHIFT_L
            }
        };
        match events::press(key) {
            Ok(numbers) => driver.numbers = numbers,
            Err(reason) => {
                driver.error = Some(reason);
                return Poll::Ready(());
            }
        }
        Poll::Pending(format!(
            "{:?} {} {:?} отправлен",
            driver.kind, driver.done, driver.phase
        ))
    }

    fn note_palette_rows(&self, scene: &Scene) {
        let visible = Run::visible_rows(scene.palette.list().table());
        self.palette_visible
            .set(self.palette_visible.get().max(visible));
    }

    /// Ошибка исполнения циклов или следующая цель.
    fn advance(&self, target: Option<usize>) -> Result<(), String> {
        let mut slot = self.driver.borrow_mut();
        let driver = slot.as_mut().ok_or("циклы не начаты")?;
        if let Some(error) = driver.error.take() {
            return Err(error);
        }
        match target {
            Some(target) => driver.target = target,
            None => {
                let mut report = self.report.borrow_mut();
                let name = match driver.kind {
                    Kind::Palette => "palette",
                    Kind::Tabs => "tabs",
                };
                report.push(&format!("{name}_cycles"), driver.done);
                report.push(&format!("{name}_state_polls_after_pass"), driver.late_polls);
            }
        }
        Ok(())
    }

    fn start(&self, kind: Kind) {
        *self.driver.borrow_mut() = Some(Driver::new(kind));
    }

    /// Живые объекты каждого класса после 10-го и 100-го цикла одинаковы.
    fn check_stable(&self, name: &str, tenth: &str, hundredth: &str, what: &str) {
        let changes = counters::live_changes(&self.counts(tenth), &self.counts(hundredth));
        self.report.borrow_mut().finding(
            name,
            verdict(changes.is_empty()),
            format!(
                "{what}: классы, у которых число живых объектов после {CYCLES}-го цикла отличается от числа после {WARMUP}-го: {}",
                describe_changes(&changes)
            ),
        );
    }

    fn check_palette_single(&self) {
        let tenth = self.counts("palette_10");
        let hundredth = self.counts("palette_100");
        let ok = single_palette(&tenth) && single_palette(&hundredth);
        let show = |counts: &[Count]| {
            ["ProbePalettePanel", "ProbePaletteController"]
                .iter()
                .map(|name| {
                    find(counts, name).map_or_else(
                        || format!("{name}: нет"),
                        |count| format!("{name}: создано {}, живо {}", count.created, count.live()),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        self.report.borrow_mut().finding(
            "palette_single_instance",
            verdict(ok),
            format!(
                "переиспользуемая палитра за {CYCLES} циклов: после {WARMUP}-го — {}; после {CYCLES}-го — {}",
                show(&tenth),
                show(&hundredth)
            ),
        );
    }

    /// Виды строк и ячеек, созданные списком, и живые виды списка не больше порога.
    fn check_reuse(&self, name: &str, list: &crate::list::VirtualList, visible: usize) {
        let (cells, rows) = list.created();
        let (cells_live, rows_live) = list.live();
        let limit = reuse_limit(visible);
        self.report.borrow_mut().finding(
            name,
            verdict([cells, rows, cells_live, rows_live].iter().all(|n| *n <= limit)),
            format!(
                "создано видов ячеек {cells} и строк {rows} за всю жизнь списка, включая каждую перезагрузку циклов (строже набора в list, где считается прирост за набор), живых {cells_live} и {rows_live}; порог {limit} при видимых строках не больше {visible}"
            ),
        );
    }

    /// Слабые ссылки на центральную плитку перед закрытием: все её виды и источник данных.
    fn remember_tile(&self, tile: &Tile) {
        let mut previous = Previous::default();
        previous.add_tree(tile);
        // SAFETY: источник данных держит список плитки, он жив, пока жива плитка.
        if let Some(source) = tile
            .list()
            .and_then(|list| unsafe { list.table().dataSource() })
        {
            let source: &AnyObject = source.as_ref();
            previous.add("ProbeListSource", source);
        }
        let menu: &AnyObject = tile.strip().menu();
        previous.add("NSMenu полосы", menu);
        *self.previous.borrow_mut() = previous;
    }

    /// Слабые ссылки на окно сцены, его виды, панель палитры с видами и владельцев Rust.
    fn remember_scene(&self, scene: &Rc<Scene>) {
        let mut previous = Previous::default();
        let window: &AnyObject = &scene.window;
        previous.add("NSWindow сцены", window);
        if let Some(content) = scene.window.contentView() {
            previous.add_tree(&content);
        }
        let panel: &AnyObject = scene.palette.panel();
        previous.add("ProbePalettePanel", panel);
        if let Some(content) = scene.palette.panel().contentView() {
            previous.add_tree(&content);
        }
        *self.previous.borrow_mut() = previous;
        *self.owners.borrow_mut() = Some(Owners {
            scene: Rc::downgrade(scene),
            palette: Rc::downgrade(&scene.palette),
            tiles: Rc::downgrade(&scene.tiles),
            theme: Rc::downgrade(&scene.theme),
            commands: Rc::downgrade(&scene.commands),
        });
    }

    fn owners_alive(&self) -> Vec<&'static str> {
        self.owners
            .borrow()
            .as_ref()
            .map(Owners::alive)
            .unwrap_or_default()
    }

    /// Прежний экземпляр освобождён или истёк срок ожидания.
    fn released_or_grace(&self) -> bool {
        let released = self.previous.borrow().alive().0 == 0 && self.owners_alive().is_empty();
        let grace = self
            .closed_at
            .get()
            .is_some_and(|at| at.elapsed() >= RELEASE_GRACE);
        (released || grace) && self.passed()
    }

    /// Итог ожидания освобождения: что осталось живым и сколько ждали.
    fn note_release(&self, name: &'static str) {
        let (count, alive) = self.previous.borrow().alive();
        let owners = self.owners_alive();
        let waited = self
            .closed_at
            .get()
            .map_or(0.0, |at| at.elapsed().as_secs_f64() * 1e3);
        let tracked = self.previous.borrow().objects.len();
        self.released.borrow_mut().push((
            name,
            format!(
                "слабых ссылок {tracked}, живых {count}{}; живых владельцев Rust: {owners:?}; ожидание {waited:.1} мс",
                if alive.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", alive.join(", "))
                }
            ),
        ));
    }

    fn post_display_options(&self) {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        // SAFETY: имя уведомления — неизменяемая константа AppKit.
        unsafe {
            center.postNotificationName_object(
                NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification,
                None,
            )
        };
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
    let mut report = stand.report(Scenario::Cycles);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    report.push(
        "count_procedure",
        "снимок счётчиков: очистка таблицы событий, flagsChanged без модификаторов для опустошения пула autorelease, возврат его sendEvent и проход отображения",
    );
    report.push(
        "late_callback",
        "поздний обратный вызов — вызов метода или замыкания, не нашедший владельца живым (counters::owner); после закрытия вызываются уведомление о параметрах отображения, смена оформления, Cmd-K, Escape и Shift-L",
    );
    report.push(
        "release_check",
        "освобождение прежнего экземпляра: слабые ссылки Objective-C на его виды, окна и источник данных, слабые ссылки Rust на владельцев сцены; ожидание до 3 с",
    );
    report.push(
        "counters_scope",
        "счётчики охватывают классы define_class!; блоки, наблюдатели и системные объекты не охвачены, отсутствие падения не доказывает отсутствия обращения к освобождённой памяти",
    );
    let run = Rc::new(Run {
        stand: Rc::clone(&stand),
        scene: RefCell::new(None),
        report: RefCell::new(report),
        mark: Cell::new(None),
        keys: RefCell::new(Vec::new()),
        nudge: Cell::new(None),
        driver: RefCell::new(None),
        snapshots: RefCell::new(Vec::new()),
        responder: Cell::new(0),
        filtered: Cell::new(0),
        palette_visible: Cell::new(0),
        center_visible: Cell::new(0),
        expected_tab: Cell::new(0),
        responder_kept: Cell::new(false),
        menu_items: Cell::new(0),
        scene_menu_items: Cell::new(0),
        previous: RefCell::new(Previous::default()),
        owners: RefCell::new(None),
        closed_at: Cell::new(None),
        appearance: RefCell::new(None),
        notifications: Cell::new(0),
        released: RefCell::new(Vec::new()),
    });
    let next = Rc::clone(&run);
    run_steps(steps(&run), move |result| finish(next, result));
}

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

/// Шаг, который крутит циклы до текущей цели, затем выполняет `act`.
fn cycles_step(
    run: &Rc<Run>,
    name: &'static str,
    timeout: Duration,
    act: impl FnOnce(&Run) -> Result<(), String> + 'static,
) -> Step {
    let (for_ready, for_act) = (Rc::clone(run), Rc::clone(run));
    Step::new(
        name,
        timeout,
        move || for_ready.drive(),
        move || act(&for_act),
    )
}

fn steps(run: &Rc<Run>) -> Vec<Step> {
    let mut steps = before_close(run);
    steps.extend(tile_reopen(run));
    steps.extend(window_reopen(run));
    steps
}

fn before_close(run: &Rc<Run>) -> Vec<Step> {
    vec![
        step(
            run,
            "окно стенда: активность и key window",
            STEP_TIMEOUT,
            |run| run.key_is(&run.stand.window),
            |run| {
                let source = input_source(&run.stand.window);
                if let Some(source) = &source {
                    run.report.borrow_mut().push("input_source", source);
                }
                require_latin(source.as_deref())?;
                events::warp_cursor_outside(run.app().mtm(), run.app()).map(|_| ())
            },
        ),
        step(
            run,
            "курсор вне окон",
            STEP_TIMEOUT,
            |run| events::cursor_outside(run.app()),
            Run::begin_count,
        ),
        step(
            run,
            "снимок: без сцены",
            STEP_TIMEOUT,
            Run::counted,
            |run| {
                run.snapshot("baseline");
                run.menu_items.set(run.menu_count());
                let stand = &run.stand;
                let scene = autoreleasepool(|_| {
                    let scene = Scene::build(stand.app.mtm(), &stand.display, stand.scene);
                    scene.window.makeKeyAndOrderFront(None);
                    stand.window.orderOut(None);
                    scene
                });
                *run.scene.borrow_mut() = Some(scene);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "сцена: key window и фокус центральной плитки",
            STEP_TIMEOUT,
            |run| {
                run.scene_is_key()
                    && run.passed()
                    && run.scene.borrow().as_ref().is_some_and(|scene| {
                        events::is_first_responder(&scene.window, &scene.tiles.tile(CENTER))
                    })
            },
            Run::begin_count,
        ),
        step(
            run,
            "снимок: открытая сцена",
            STEP_TIMEOUT,
            Run::counted,
            |run| {
                run.snapshot("scene_open");
                let scene = run.scene()?;
                run.scene_menu_items.set(run.menu_count());
                run.report
                    .borrow_mut()
                    .push("menu_items_scene", run.scene_menu_items.get());
                run.responder.set(run.parent_responder(&scene));
                let folded: Vec<String> = scene
                    .palette
                    .model()
                    .iter()
                    .map(|command| palette::fold(&command.title))
                    .collect();
                run.filtered.set(
                    palette::filter(folded.iter().map(String::as_str), KEY_L.characters).len(),
                );
                if let Some(list) = scene.tiles.tile(CENTER).list() {
                    run.center_visible.set(Run::visible_rows(list.table()));
                }
                run.start(Kind::Palette);
                Ok(())
            },
        ),
        cycles_step(
            run,
            "палитра: циклы 1–10",
            WARMUP_TIMEOUT,
            |run| {
                run.advance(Some(CYCLES))?;
                run.begin_count()
            },
        ),
        step(
            run,
            "снимок: палитра после 10",
            STEP_TIMEOUT,
            Run::counted,
            |run| {
                run.snapshot("palette_10");
                Ok(())
            },
        ),
        cycles_step(
            run,
            "палитра: циклы 11–100",
            CYCLES_TIMEOUT,
            |run| {
                run.advance(None)?;
                run.begin_count()
            },
        ),
        step(
            run,
            "снимок: палитра после 100",
            STEP_TIMEOUT,
            Run::counted,
            |run| {
                run.snapshot("palette_100");
                run.check_stable(
                    "palette_cycles_stable",
                    "palette_10",
                    "palette_100",
                    "циклы палитры Cmd-K, «l», Escape",
                );
                run.check_palette_single();
                let scene = run.scene()?;
                run.check_reuse(
                    "palette_row_reuse",
                    scene.palette.list(),
                    run.palette_visible.get(),
                );
                run.start(Kind::Tabs);
                Ok(())
            },
        ),
        cycles_step(
            run,
            "вкладки: переключения 1–10",
            WARMUP_TIMEOUT,
            |run| {
                run.advance(Some(CYCLES))?;
                run.begin_count()
            },
        ),
        step(
            run,
            "снимок: вкладки после 10",
            STEP_TIMEOUT,
            Run::counted,
            |run| {
                run.snapshot("tabs_10");
                Ok(())
            },
        ),
        cycles_step(
            run,
            "вкладки: переключения 11–100",
            TABS_TIMEOUT,
            |run| {
                run.advance(None)?;
                run.begin_count()
            },
        ),
    ]
}

fn tile_reopen(run: &Rc<Run>) -> Vec<Step> {
    vec![
        step(
            run,
            "снимок: вкладки после 100",
            STEP_TIMEOUT,
            Run::counted,
            |run| {
                run.snapshot("tabs_100");
                run.check_stable(
                    "tabs_cycles_stable",
                    "tabs_10",
                    "tabs_100",
                    "переключения вкладок центральной полосы Shift-L",
                );
                let scene = run.scene()?;
                let old = scene.tiles.tile(CENTER);
                if let Some(list) = old.list() {
                    run.check_reuse("center_row_reuse", list, run.center_visible.get());
                }
                run.remember_tile(&old);
                drop(old);
                autoreleasepool(|_| {
                    scene.reopen_tile(CENTER);
                });
                run.closed_at.set(Some(Instant::now()));
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "плитка: прежняя освобождена",
            RELEASE_GRACE + STEP_TIMEOUT,
            Run::released_or_grace,
            |run| {
                run.note_release("tile");
                run.begin_count()
            },
        ),
        step(
            run,
            "снимок: плитка открыта заново",
            STEP_TIMEOUT,
            Run::counted,
            |run| {
                run.snapshot("tile_reopened");
                let scene = run.scene()?;
                let tile = scene.tiles.tile(CENTER);
                let responder = events::is_first_responder(&scene.window, &tile);
                run.responder_kept.set(responder);
                run.report.borrow_mut().push(
                    "tile_reopened_responder",
                    events::responder_name(&scene.window),
                );
                let active = tile.strip().active().unwrap_or(0);
                run.expected_tab.set((active + 1) % SCENE_TABS[CENTER]);
                *run.keys.borrow_mut() = events::press(KEY_SHIFT_L)?;
                Ok(())
            },
        ),
        step(
            run,
            "плитка: Shift-L новой полосе",
            STEP_TIMEOUT,
            |run| returned_and_passed(&run.keys.borrow()),
            |run| {
                let scene = run.scene()?;
                let tile = scene.tiles.tile(CENTER);
                let strip = tile.strip();
                let (handled, active, layer) = (
                    strip.keys_handled(),
                    strip.active(),
                    run.layer_active(&scene),
                );
                let ((alive, _), tracked) = {
                    let previous = run.previous.borrow();
                    (previous.alive(), previous.objects.len())
                };
                let changes =
                    counters::live_changes(&run.counts("tabs_100"), &run.counts("tile_reopened"));
                let detail = run
                    .released
                    .borrow()
                    .iter()
                    .find(|(name, _)| *name == "tile")
                    .map(|(_, detail)| detail.clone())
                    .unwrap_or_default();
                let expected = Some(run.expected_tab.get());
                let responder = run.responder_kept.get();
                run.report.borrow_mut().finding(
                    "tile_reopen_released",
                    verdict(
                        alive == 0
                            && responder
                            && handled == 1
                            && active == expected
                            && layer == expected,
                    ),
                    format!(
                        "центральная плитка закрыта и открыта заново: {detail}; живых объектов прежней плитки {alive} из {tracked}; first responder окна — новая плитка: {responder}; Shift-L в новой плитке принят её полосой: клавиш {handled}, активная вкладка {active:?}, по слою {layer:?} при ожидаемой {expected:?}; классы, у которых число живых объектов после повторного открытия отличается от числа до закрытия: {}",
                        describe_changes(&changes)
                    ),
                );
                Ok(())
            },
        ),
    ]
}

fn window_reopen(run: &Rc<Run>) -> Vec<Step> {
    vec![
        step(
            run,
            "окно: закрытие",
            STEP_TIMEOUT,
            |run| run.scene_is_key() && run.passed(),
            |run| {
                let scene = run.scene.borrow_mut().take().ok_or("сцена не открыта")?;
                run.remember_scene(&scene);
                // Окно стенда возвращается, как в состоянии без сцены: после закрытия у
                // приложения остаётся окно, а снимок сравнивается с этим состоянием.
                run.stand.window.orderFront(None);
                autoreleasepool(|_| {
                    scene.window.close();
                    drop(scene);
                });
                run.closed_at.set(Some(Instant::now()));
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "окно: прежнее освобождено",
            RELEASE_GRACE + STEP_TIMEOUT,
            Run::released_or_grace,
            |run| {
                run.note_release("window");
                run.post_display_options();
                let app = run.app();
                *run.appearance.borrow_mut() = app.appearance();
                let flip = if chrome::is_dark(&app.effectiveAppearance()) {
                    Choice::Light
                } else {
                    Choice::Dark
                };
                run.report
                    .borrow_mut()
                    .push("closed_appearance_flip", flip.name());
                flip.apply(app);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "окно закрыто: оформление сменилось",
            STEP_TIMEOUT,
            Run::passed,
            |run| {
                let appearance = run.appearance.borrow_mut().take();
                run.app().setAppearance(appearance.as_deref());
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "окно закрыто: оформление возвращено",
            STEP_TIMEOUT,
            Run::passed,
            Run::begin_count,
        ),
        step(
            run,
            "снимок: окно закрыто",
            STEP_TIMEOUT,
            Run::counted,
            |run| {
                let closed = run.snapshot("window_closed");
                let baseline = run.counts("baseline");
                let changes = counters::live_changes(&baseline, &closed);
                let (alive, _) = run.previous.borrow().alive();
                let owners = run.owners_alive();
                let menu = run.menu_count();
                let detail = run
                    .released
                    .borrow()
                    .iter()
                    .find(|(name, _)| *name == "window")
                    .map(|(_, detail)| detail.clone())
                    .unwrap_or_default();
                run.report.borrow_mut().finding(
                "window_close_released",
                verdict(
                    changes.is_empty()
                        && alive == 0
                        && owners.is_empty()
                        && menu == run.menu_items.get(),
                ),
                format!(
                    "окно сцены закрыто и сцена освобождена: {detail}; классы, у которых число живых объектов отличается от состояния без сцены: {}; пунктов главного меню {menu} при {} без сцены",
                    describe_changes(&changes),
                    run.menu_items.get()
                ),
            );
                let stand = &run.stand;
                let scene = autoreleasepool(|_| {
                    let scene = Scene::build(stand.app.mtm(), &stand.display, stand.scene);
                    scene.window.makeKeyAndOrderFront(None);
                    stand.window.orderOut(None);
                    scene
                });
                *run.scene.borrow_mut() = Some(scene);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "окно открыто заново: key window",
            STEP_TIMEOUT,
            |run| {
                run.scene_is_key()
                    && run.passed()
                    && run.scene.borrow().as_ref().is_some_and(|scene| {
                        events::is_first_responder(&scene.window, &scene.tiles.tile(CENTER))
                    })
            },
            Run::begin_count,
        ),
        step(
            run,
            "снимок: окно открыто заново",
            STEP_TIMEOUT,
            Run::counted,
            |run| {
                let reopened = run.snapshot("window_reopened");
                let changes = counters::live_changes(&run.counts("scene_open"), &reopened);
                let menu = run.menu_count();
                let scene_menu = run.scene_menu_items.get();
                let panel =
                    find(&reopened, "ProbePalettePanel").map(|count| (count.created, count.live()));
                run.report.borrow_mut().finding(
                    "window_reopen_same",
                    verdict(
                        changes.is_empty()
                            && scene_menu == menu
                            && panel.is_some_and(|(_, live)| live == 1),
                    ),
                    format!(
                        "сцена в новом окне: классы, у которых число живых объектов отличается от первой открытой сцены: {}; пунктов главного меню {menu}, у первой сцены {scene_menu}; панель палитры создано и живо {panel:?}",
                        describe_changes(&changes)
                    ),
                );
                let scene = run.scene()?;
                run.responder.set(run.parent_responder(&scene));
                run.notifications.set(scene.theme.notifications());
                *run.keys.borrow_mut() = events::press(Key::CMD_K)?;
                Ok(())
            },
        ),
        step(
            run,
            "новое окно: Cmd-K открыл новую палитру",
            STEP_TIMEOUT,
            |run| {
                returned_and_passed(&run.keys.borrow())
                    && run
                        .scene
                        .borrow()
                        .as_ref()
                        .is_some_and(|scene| run.palette_opened(scene))
            },
            |run| {
                *run.keys.borrow_mut() = events::press(KEY_ESCAPE)?;
                Ok(())
            },
        ),
        step(
            run,
            "новое окно: Escape закрыл палитру",
            STEP_TIMEOUT,
            |run| {
                returned_and_passed(&run.keys.borrow())
                    && run.scene.borrow().as_ref().is_some_and(|scene| {
                        run.palette_closed(scene)
                            && run.parent_responder(scene) == run.responder.get()
                    })
            },
            |run| {
                run.post_display_options();
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "новое окно: уведомление получила новая тема",
            STEP_TIMEOUT,
            Run::passed,
            |run| {
                let scene = run.scene()?;
                let notifications = scene.theme.notifications() - run.notifications.get();
                let opened = scene.palette.opened();
                let late = counters::late_calls();
                let owners = run.owners_alive();
                run.report.borrow_mut().finding(
                    "late_callbacks",
                    verdict(late.is_empty() && owners.is_empty() && notifications == 1),
                    format!(
                        "поздних обратных вызовов {late:?}; после закрытия окна вызваны уведомление о параметрах отображения и смена оформления, в новом окне — Cmd-K, Escape и то же уведомление: новая палитра открыта {opened} раз, новая тема получила уведомлений {notifications}; живых владельцев прежней сцены: {owners:?}"
                    ),
                );
                Ok(())
            },
        ),
    ]
}

fn finish(run: Rc<Run>, result: Result<(), StepFailure>) {
    let stand = Rc::clone(&run.stand);
    let mut report = run.report.replace(Report::default());
    for (name, detail) in run.released.borrow().iter() {
        report.push(&format!("release_{name}"), detail);
    }
    if let Some(scene) = run.scene.borrow().as_ref() {
        report.push_json("scene_window", &super::readback::window_info(&scene.window));
    }
    let late = counters::late_calls();
    report.push_json("late_calls", &late);
    stand.conclude(report, result.map_err(|failure| failure.describe()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(name: &'static str, created: usize, released: usize) -> Count {
        Count {
            name,
            created,
            released,
        }
    }

    #[test]
    fn palette_single_instance_needs_one_created_and_live() {
        let one = [
            count("ProbePalettePanel", 1, 0),
            count("ProbePaletteController", 1, 0),
        ];
        assert!(single_palette(&one));
        let recreated = [
            count("ProbePalettePanel", 2, 1),
            count("ProbePaletteController", 1, 0),
        ];
        assert!(!single_palette(&recreated));
        assert!(!single_palette(&[count("ProbePalettePanel", 1, 0)]));
    }

    #[test]
    fn reuse_limit_matches_plan() {
        assert_eq!(reuse_limit(0), 10);
        assert_eq!(reuse_limit(15), 40);
    }

    #[test]
    fn changes_are_described_by_class() {
        assert_eq!(describe_changes(&[]), "нет");
        assert_eq!(
            describe_changes(&[("ProbeListRow", 30, 31), ("ProbeTab", 8, 16)]),
            "ProbeListRow 30 → 31, ProbeTab 8 → 16"
        );
    }
}
