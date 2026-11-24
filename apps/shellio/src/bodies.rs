//! Тела плагинов в контейнерах панелей без AppKit (ADR-0008, «Жизненный цикл вида»): какие тела
//! живы, что каждое уже получило и что ему передать по новой сцене. Адаптер окна даёт сторону
//! AppKit портом [`Shell`]: построить тело фабрикой в контейнере, припарковать и освободить
//! контейнер.
//!
//! - [`Bodies::sync`] — сцена и зеркало → вызовы тел: построение, проекция данных, представление,
//!   активность, фокус, закрытие. Повторная доставка той же сцены вызовов не даёт.
//! - [`Requests`] — исходы запросов тел: исход получает только живое тело своего поколения и только
//!   за последний запрос.
//! - [`command_of`] и [`refusal`] — запрос тела в команду ядра и отказ ядра в отказ для тела.
//!
//! Контейнер панели создаётся, когда её рабочая область впервые показана, и живёт, пока панель есть
//! хотя бы в одной раскладке. Уход окна к другой рабочей области контейнер паркует, тело остаётся.
//! Тело живёт, пока панель в `ready`: уход из `ready` закрывает его, возврат строит новое.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use serde_json::Value;
use shellio_core::{
    Command, CoreError, PanelData, PanelId, PanelStatus, PluginId, State, WorkbenchId,
};
use shellio_layout::{Geometry, Layout};
use shellio_plugin_api::{Presentation, Refusal, Registry};
use shellio_plugin_view::{Body, Done, PanelRequest, PluginView};

use crate::scene::Bench;

/// Место поздних запросов в счётчиках `ui.stats`: запрос прислало тело, которого уже нет.
pub const LATE_REQUEST: &str = "PluginHost.request";
/// Место поздних исходов в счётчиках `ui.stats`: исход пришёл телу, которого уже нет.
pub const LATE_DONE: &str = "PluginHost.done";

/// Запрос тела в пути: панель, поколение тела и номер запроса.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Ticket {
    pub panel: PanelId,
    pub generation: u64,
    pub seq: u64,
}

/// Что делать с исходом запроса.
#[derive(Debug, PartialEq, Eq)]
pub enum Settled<D> {
    /// Тело живо, запрос у него последний: исход его.
    Deliver(D),
    /// Тело живо, но после этого запроса оно прислало новый: исход отброшен.
    Superseded,
    /// Тела этого поколения уже нет: поздний обратный вызов.
    Stale,
}

struct Pending<D> {
    generation: u64,
    /// Последний запрос тела: номер и его исход.
    last: Option<(u64, D)>,
}

/// Исходы запросов живых тел. `D` — обратный вызов исхода.
pub struct Requests<D> {
    bodies: BTreeMap<PanelId, Pending<D>>,
    seq: u64,
}

impl<D> Default for Requests<D> {
    fn default() -> Self {
        Self {
            bodies: BTreeMap::new(),
            seq: 0,
        }
    }
}

impl<D> Requests<D> {
    /// Тело панели `panel` поколения `generation` построено.
    pub fn open(&mut self, panel: PanelId, generation: u64) {
        self.bodies.insert(
            panel,
            Pending {
                generation,
                last: None,
            },
        );
    }

    /// Тело панели закрыто: его запросы в пути исхода не получат. Возвращает неотданный исход.
    pub fn close(&mut self, panel: PanelId) -> Option<D> {
        self.bodies
            .remove(&panel)
            .and_then(|pending| pending.last)
            .map(|(_, done)| done)
    }

    /// Запрос тела поколения `generation`. `Err` возвращает исход: такого тела уже нет. Прежний
    /// запрос тела исхода не получит.
    pub fn begin(&mut self, panel: PanelId, generation: u64, done: D) -> Result<Ticket, D> {
        match self.bodies.get_mut(&panel) {
            Some(pending) if pending.generation == generation => {
                self.seq += 1;
                pending.last = Some((self.seq, done));
                Ok(Ticket {
                    panel,
                    generation,
                    seq: self.seq,
                })
            }
            Some(_) | None => Err(done),
        }
    }

    /// Исход запроса `ticket` пришёл.
    pub fn settle(&mut self, ticket: Ticket) -> Settled<D> {
        let Some(pending) = self
            .bodies
            .get_mut(&ticket.panel)
            .filter(|pending| pending.generation == ticket.generation)
        else {
            return Settled::Stale;
        };
        match pending.last.take() {
            Some((seq, done)) if seq == ticket.seq => Settled::Deliver(done),
            newer => {
                pending.last = newer;
                Settled::Superseded
            }
        }
    }
}

/// Запросы тел окна: общие для отправителей тел и адаптера.
pub type SharedRequests = Rc<RefCell<Requests<Done>>>;

/// Запрос тела панели `panel` поколения `generation`. `None` — тела уже нет: запрос не уходит,
/// исход отброшен, поздний вызов посчитан.
pub fn begin_request<D>(
    requests: &RefCell<Requests<D>>,
    panel: PanelId,
    generation: u64,
    done: D,
) -> Option<Ticket> {
    let begun = requests.borrow_mut().begin(panel, generation, done);
    begun
        .map_err(|_done| shellio_appkit::late_callback(LATE_REQUEST))
        .ok()
}

/// Исход запроса `ticket`: обратный вызов, если его ещё ждут. Исход закрытого тела — поздний
/// вызов: он посчитан и отброшен.
pub fn settle_request<D>(requests: &RefCell<Requests<D>>, ticket: Ticket) -> Option<D> {
    let settled = requests.borrow_mut().settle(ticket);
    match settled {
        Settled::Deliver(done) => Some(done),
        Settled::Superseded => None,
        Settled::Stale => {
            shellio_appkit::late_callback(LATE_DONE);
            None
        }
    }
}

/// Почему запрос тела не выполнен.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Отказ ядра, включая ранний `busy`.
    Core(CoreError),
    /// Ядро не ответило.
    NoReply,
    /// Ядро ещё не подключено к окну.
    NoCore,
}

/// Отказ для тела: причина — `details.reason` отказа ядра, без неё — код.
pub fn refusal(failure: &Failure) -> Refusal {
    match failure {
        Failure::Core(error) => Refusal::new(
            error.detail_str("field").unwrap_or_default(),
            error
                .detail_str("reason")
                .unwrap_or_else(|| error.code().as_str()),
            error.message(),
        ),
        Failure::NoReply => Refusal::new("", "no_reply", "Ядро не ответило"),
        Failure::NoCore => Refusal::new("", "no_core", "Ядро ещё не запущено"),
    }
}

/// Команда ядра на запрос тела панели `panel` из рабочей области `workbench`: действие своей панели
/// или открытие панели плагина в её рабочей области.
pub fn command_of(
    panel: PanelId,
    workbench: Option<WorkbenchId>,
    request: PanelRequest,
) -> Command {
    match request {
        PanelRequest::Command { name, args } => Command::PanelCommand {
            panel,
            name,
            args: Some(args),
        },
        PanelRequest::Open { plugin, args } => Command::PanelOpen {
            plugin,
            workbench,
            zone: None,
            args,
        },
    }
}

/// Заказ тела: фабрика плагина `plugin` строит его по данным `data` в контейнере панели `panel`.
#[derive(Clone, Copy, Debug)]
pub struct Build<'a> {
    pub panel: PanelId,
    pub plugin: &'a PluginId,
    /// Поколение тела: с ним хост привязывает отправитель запросов.
    pub generation: u64,
    pub data: &'a Value,
}

/// Сторона AppKit: контейнеры панелей и фабрики тел.
pub trait Shell {
    /// Строит тело и ставит его в контейнер панели. `None` — фабрики плагина нет.
    fn build(&mut self, order: Build<'_>) -> Option<Body>;
    /// Контейнер отдаёт ввод окну, если держит его.
    fn resign(&mut self, panel: PanelId);
    /// Контейнер уходит скрытым в корень окна: его рабочая область больше не показана.
    fn park(&mut self, panel: PanelId);
    /// Контейнер освобождается: панели больше нет ни в одной раскладке.
    fn release(&mut self, panel: PanelId);
}

/// Что получает [`Bodies::sync`].
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    pub state: &'a State,
    pub registry: &'a Registry,
    /// Открытая рабочая область сцены.
    pub bench: Option<&'a Bench>,
    /// Окно видимо.
    pub visible: bool,
}

struct Live {
    body: Body,
    /// Данные панели, которые тело уже получило.
    data: String,
    geometry: Option<Geometry>,
    presentation: Option<Presentation>,
    /// Данные изменились после последнего расчёта представления.
    stale: bool,
    active: bool,
    focused: bool,
    /// Панель в фокусе, и её тело показано: ввод её.
    input: bool,
}

#[derive(Default)]
struct Hosted {
    parked: bool,
    built: u64,
    live: Option<Live>,
}

impl Hosted {
    fn close(&mut self, panel: PanelId, requests: &RefCell<Requests<Done>>) {
        if let Some(live) = self.live.take() {
            // Запрос, который тело шлёт из `close_view`, исхода уже не получит.
            let dropped = requests.borrow_mut().close(panel);
            drop(dropped);
            live.body.close();
        }
    }
}

/// Что панель должна получить по сцене.
#[derive(Clone, Copy, Default)]
struct Want {
    active: bool,
    focused: bool,
    input: bool,
}

/// Тела панелей окна.
#[derive(Default)]
pub struct Bodies {
    panels: BTreeMap<PanelId, Hosted>,
    generation: u64,
    requests: SharedRequests,
}

fn raw(state: &State, panel: PanelId) -> &str {
    match state.panel_data.get(&panel) {
        Some(PanelData::Text { raw, .. }) => raw,
        Some(PanelData::Unreadable {}) | None => "null",
    }
}

/// Панели, которые стоят хотя бы в одной раскладке.
fn placed(state: &State) -> BTreeSet<PanelId> {
    state
        .layouts
        .values()
        .flat_map(Layout::panels)
        .filter(|panel| state.panels.contains_key(panel))
        .collect()
}

impl Bodies {
    /// Запросы тел: их делят отправители, которые хост даёт телам.
    pub fn requests(&self) -> SharedRequests {
        Rc::clone(&self.requests)
    }

    /// Приводит контейнеры и тела к сцене `frame`. Порядок вызовов тела: построение, новые данные,
    /// представление, затем переходы активности и фокуса — сначала уходящие у всех панелей, потом
    /// приходящие, последним `focus_preferred` панели, получившей ввод.
    pub fn sync(&mut self, frame: &Frame<'_>, shell: &mut dyn Shell) {
        let state = frame.state;
        let placed = placed(state);
        let gone: Vec<PanelId> = self
            .panels
            .keys()
            .filter(|panel| !placed.contains(panel))
            .copied()
            .collect();
        for panel in gone {
            if let Some(mut hosted) = self.panels.remove(&panel) {
                hosted.close(panel, &self.requests);
                shell.release(panel);
            }
        }

        let views = frame.bench.map(|bench| &bench.views);
        let focus = frame.bench.and_then(|bench| bench.focus);
        for &panel in views.into_iter().flat_map(BTreeMap::keys) {
            self.panels.entry(panel).or_default();
        }

        let mut wants: BTreeMap<PanelId, Want> = BTreeMap::new();
        for (&panel, hosted) in &mut self.panels {
            let slot = views.and_then(|views| views.get(&panel));
            match slot {
                Some(_) => hosted.parked = false,
                None if !hosted.parked => {
                    hosted.parked = true;
                    shell.park(panel);
                }
                None => {}
            }
            if state.status_of(panel) != PanelStatus::Ready {
                hosted.close(panel, &self.requests);
                continue;
            }
            let Some(plugin) = state.panels.get(&panel).map(|record| &record.plugin) else {
                continue;
            };
            let data = raw(state, panel);
            if hosted.live.is_none() && slot.is_some() {
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                self.generation += 1;
                let generation = self.generation;
                // Тело может прислать запрос уже из фабрики.
                self.requests.borrow_mut().open(panel, generation);
                let built = shell.build(Build {
                    panel,
                    plugin,
                    generation,
                    data: &value,
                });
                match built {
                    Some(body) => {
                        hosted.built += 1;
                        hosted.live = Some(Live {
                            body,
                            data: data.to_owned(),
                            geometry: None,
                            presentation: None,
                            stale: true,
                            active: false,
                            focused: false,
                            input: false,
                        });
                    }
                    None => drop(self.requests.borrow_mut().close(panel)),
                }
            }
            let Some(live) = hosted.live.as_mut() else {
                continue;
            };
            if live.data != data
                && let Ok(value) = serde_json::from_str::<Value>(data)
            {
                data.clone_into(&mut live.data);
                live.stale = true;
                live.body.view_mut().update(&value);
            }
            let shown = slot.is_some_and(|slot| slot.shown());
            if shown
                && let Some(geometry) = slot.and_then(|slot| slot.geometry)
                && (live.stale || live.geometry != Some(geometry))
                && let Some(model) = frame.registry.get(plugin).map(|plugin| plugin.model())
                && let Ok(presentation) = model.adapt(&geometry, data)
            {
                let changed = live.geometry != Some(geometry)
                    || live.presentation.as_ref() != Some(&presentation);
                live.geometry = Some(geometry);
                live.stale = false;
                if changed {
                    live.body.view_mut().adapt(&geometry, &presentation);
                }
                live.presentation = Some(presentation);
            }
            let focused = slot.is_some() && focus == Some(panel);
            let want = Want {
                active: frame.visible && shown,
                focused,
                input: focused && shown,
            };
            if live.active && !want.active {
                live.active = false;
                live.body.view_mut().set_active(false);
            }
            if live.focused && !want.focused {
                live.focused = false;
                live.body.view_mut().set_focused(false);
                shell.resign(panel);
            }
            if !want.input {
                live.input = false;
            }
            wants.insert(panel, want);
        }

        let mut input = None;
        for (panel, want) in wants {
            let Some(live) = self
                .panels
                .get_mut(&panel)
                .and_then(|hosted| hosted.live.as_mut())
            else {
                continue;
            };
            if want.active && !live.active {
                live.active = true;
                live.body.view_mut().set_active(true);
            }
            if want.focused && !live.focused {
                live.focused = true;
                live.body.view_mut().set_focused(true);
            }
            if want.input && !live.input {
                live.input = true;
                input = Some(panel);
            }
        }
        if let Some(live) = input
            .and_then(|panel| self.panels.get(&panel))
            .and_then(|hosted| hosted.live.as_ref())
        {
            live.body.view().focus_preferred();
        }
    }

    /// Вид тела панели, если оно построено.
    pub fn view(&self, panel: PanelId) -> Option<&dyn PluginView> {
        self.live(panel).map(|live| live.body.view())
    }

    /// Виды всех построенных тел, включая припаркованные.
    pub fn views_mut(&mut self) -> impl Iterator<Item = &mut dyn PluginView> {
        self.panels
            .values_mut()
            .filter_map(|hosted| hosted.live.as_mut())
            .map(|live| live.body.view_mut())
    }

    /// Вариант представления, который тело панели получило последним.
    pub fn variant(&self, panel: PanelId) -> Option<&'static str> {
        self.live(panel)
            .and_then(|live| live.presentation.as_ref())
            .map(|presentation| presentation.variant)
    }

    /// Тело панели активно.
    pub fn active(&self, panel: PanelId) -> bool {
        self.live(panel).is_some_and(|live| live.active)
    }

    /// Контейнер панели припаркован: её рабочая область не показана.
    pub fn parked(&self, panel: PanelId) -> bool {
        self.panels.get(&panel).is_some_and(|hosted| hosted.parked)
    }

    /// Сколько тел построено в контейнере каждой панели: по всем живым и припаркованным
    /// контейнерам.
    pub fn built(&self) -> Vec<(PanelId, u64)> {
        self.panels
            .iter()
            .map(|(&panel, hosted)| (panel, hosted.built))
            .collect()
    }

    fn live(&self, panel: PanelId) -> Option<&Live> {
        self.panels
            .get(&panel)
            .and_then(|hosted| hosted.live.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shellio_appkit::{InspectNode, Piece, late_callbacks};
    use shellio_core::{Panel, ProjectId, Route, Title, UnixMillis, Workbench, layout_metrics};
    use shellio_layout::{Hideable, Viewport, Zone};
    use shellio_plugin_view::ViewWork;
    use shellio_theme::AppTheme;

    use super::*;
    use crate::scene::tests::{Fixture, SMALL, add_panel, fixture, id, layout_mut, registry};
    use crate::scene::{self, PartKey, Scene};

    type Log = Rc<RefCell<Vec<String>>>;

    /// Тело без AppKit: записывает вызовы хоста.
    struct FakeView {
        panel: PanelId,
        log: Log,
    }

    impl FakeView {
        fn say(&self, what: String) {
            self.log
                .borrow_mut()
                .push(format!("{} {what}", tag(self.panel)));
        }
    }

    impl PluginView for FakeView {
        fn body(&self) -> Piece<'_> {
            unimplemented!("тесты без NSApplication тело не размещают")
        }
        fn adapt(&mut self, geometry: &Geometry, presentation: &Presentation) {
            self.say(format!(
                "adapt {:?} {:?} {}",
                geometry.zone, geometry.size, presentation.variant
            ));
        }
        fn update(&mut self, state: &Value) {
            self.say(format!("update {state}"));
        }
        fn set_theme(&mut self, _theme: &AppTheme) {}
        fn set_active(&mut self, active: bool) {
            self.say(format!("active {active}"));
        }
        fn set_focused(&mut self, focused: bool) {
            self.say(format!("focused {focused}"));
        }
        fn focus_preferred(&self) {
            self.say("focus_preferred".to_owned());
        }
        fn inspect(&self, _prefix: &str) -> Vec<InspectNode> {
            Vec::new()
        }
        fn close_view(&mut self) {
            self.say("close".to_owned());
        }
    }

    /// Последний байт идентификатора панели: записи журнала читаемы.
    fn tag(panel: PanelId) -> String {
        let text = panel.to_string();
        format!("p{}", &text[text.len() - 2..])
    }

    #[derive(Default)]
    struct FakeShell {
        log: Log,
        /// Плагины без фабрики.
        missing: Vec<&'static str>,
        generations: Vec<(PanelId, u64)>,
    }

    impl Shell for FakeShell {
        fn build(&mut self, order: Build<'_>) -> Option<Body> {
            if self.missing.contains(&order.plugin.as_str()) {
                return None;
            }
            self.generations.push((order.panel, order.generation));
            self.log
                .borrow_mut()
                .push(format!("{} build {}", tag(order.panel), order.data));
            let view = FakeView {
                panel: order.panel,
                log: Rc::clone(&self.log),
            };
            Some(Body::new(Box::new(view), ViewWork::new()))
        }
        fn resign(&mut self, panel: PanelId) {
            self.log.borrow_mut().push(format!("{} resign", tag(panel)));
        }
        fn park(&mut self, panel: PanelId) {
            self.log.borrow_mut().push(format!("{} park", tag(panel)));
        }
        fn release(&mut self, panel: PanelId) {
            self.log
                .borrow_mut()
                .push(format!("{} release", tag(panel)));
        }
    }

    struct Host {
        fixture: Fixture,
        registry: Registry,
        bodies: Bodies,
        shell: FakeShell,
        visible: bool,
        viewport: Viewport,
    }

    const LIST: u8 = 0x20;

    impl Host {
        fn new() -> Host {
            Host {
                fixture: fixture(),
                registry: registry(),
                bodies: Bodies::default(),
                shell: FakeShell::default(),
                visible: true,
                viewport: SMALL,
            }
        }

        /// Панель «Демо: список» в правой зоне с данными `data` и фокусом на ней.
        fn open_list(&mut self, data: Value) -> PanelId {
            let panel = self.add(LIST, "demo-list", Some(data));
            let layout = layout_mut(&mut self.fixture);
            *layout = layout.open(panel, Some(Zone::Right)).unwrap();
            panel
        }

        fn add(&mut self, n: u8, plugin: &str, data: Option<Value>) -> PanelId {
            let panel = add_panel(&mut self.fixture, n);
            let record = self.fixture.state.panels.get_mut(&panel).unwrap();
            record.plugin = PluginId::new(plugin).unwrap();
            if let Some(data) = data {
                self.set_data(panel, &data);
            }
            panel
        }

        fn set_data(&mut self, panel: PanelId, data: &Value) {
            self.fixture.state.panel_data.insert(
                panel,
                PanelData::Text {
                    version: 1,
                    raw: data.to_string(),
                },
            );
        }

        fn layout(&mut self, change: impl FnOnce(&Layout<PanelId>) -> Layout<PanelId>) {
            let layout = layout_mut(&mut self.fixture);
            *layout = change(layout);
        }

        fn scene(&self) -> Scene {
            Scene::of(
                &self.fixture.state,
                &self.registry,
                self.viewport,
                layout_metrics(),
            )
        }

        /// Доставка сцены телам; ответ — вызовы этой доставки.
        fn sync(&mut self) -> Vec<String> {
            let scene = self.scene();
            self.bodies.sync(
                &Frame {
                    state: &self.fixture.state,
                    registry: &self.registry,
                    bench: scene.bench.as_ref(),
                    visible: self.visible,
                },
                &mut self.shell,
            );
            std::mem::take(&mut *self.shell.log.borrow_mut())
        }

        fn of(&mut self, panel: PanelId) -> Vec<String> {
            let prefix = format!("{} ", tag(panel));
            self.sync()
                .into_iter()
                .filter_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
                .collect()
        }

        /// Вторая рабочая область того же проекта с навигатором и терминалом.
        fn second_workbench(&mut self) -> WorkbenchId {
            let workbench: WorkbenchId = id(4);
            let project: ProjectId = id(2);
            let state = &mut self.fixture.state;
            state.workbenches.insert(
                workbench,
                Workbench {
                    id: workbench,
                    project,
                    title: Title::new("second").unwrap(),
                    position: 1,
                },
            );
            let (navigator, terminal): (PanelId, PanelId) = (id(0x30), id(0x31));
            for (panel, plugin, title) in [
                (navigator, "navigator", "Навигатор"),
                (terminal, "terminal", "Терминал"),
            ] {
                state.panels.insert(
                    panel,
                    Panel {
                        id: panel,
                        workbench,
                        plugin: PluginId::new(plugin).unwrap(),
                        title: Title::new(title).unwrap(),
                        created_at: UnixMillis(0),
                    },
                );
            }
            state
                .layouts
                .insert(workbench, Layout::default_for(navigator, terminal));
            workbench
        }

        fn show(&mut self, workbench: WorkbenchId) {
            self.fixture.state.window.route = Route::Workbench(workbench);
        }
    }

    fn list(key: &str, details: bool) -> Value {
        json!({"key": key, "history": [], "details": details})
    }

    #[test]
    fn body_is_built_once_and_gets_presentation_activity_and_focus_in_order() {
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        assert_eq!(
            host.of(panel),
            [
                r#"build {"details":false,"history":[],"key":"a"}"#,
                "adapt Right Narrow drill",
                "active true",
                "focused true",
                "focus_preferred",
            ]
        );
        assert_eq!(host.bodies.variant(panel), Some("drill"));
        assert!(host.bodies.active(panel));
        assert_eq!(
            host.bodies.built(),
            [
                (host.fixture.navigator, 1),
                (host.fixture.terminal, 1),
                (panel, 1)
            ]
        );
    }

    #[test]
    fn repeated_delivery_of_the_same_snapshot_makes_no_calls() {
        let mut host = Host::new();
        host.open_list(list("a", false));
        assert!(!host.sync().is_empty());
        for _ in 0..3 {
            assert_eq!(host.sync(), Vec::<String>::new());
        }
    }

    #[test]
    fn two_shown_parts_are_both_active_and_focus_moves_between_them() {
        let mut host = Host::new();
        let (terminal, list_panel) = (host.fixture.terminal, host.open_list(list("a", false)));
        let first = host.sync();
        for panel in [terminal, list_panel] {
            assert!(first.contains(&format!("{} active true", tag(panel))));
            assert!(host.bodies.active(panel));
        }
        assert!(!first.contains(&format!("{} focused true", tag(terminal))));

        host.layout(|layout| layout.focus_panel(terminal).unwrap());
        assert_eq!(
            host.sync(),
            [
                format!("{} focused false", tag(list_panel)),
                format!("{} resign", tag(list_panel)),
                format!("{} focused true", tag(terminal)),
                format!("{} focus_preferred", tag(terminal)),
            ],
            "сначала уходит фокус прежней панели, активность не меняется"
        );

        host.layout(|layout| layout.focus_panel(list_panel).unwrap());
        assert_eq!(
            host.sync(),
            [
                format!("{} focused false", tag(terminal)),
                format!("{} resign", tag(terminal)),
                format!("{} focused true", tag(list_panel)),
                format!("{} focus_preferred", tag(list_panel)),
            ]
        );
        assert_eq!(host.sync(), Vec::<String>::new());
    }

    /// Вызовы активности тела панели за доставку.
    fn activity(lines: &[String]) -> Vec<&str> {
        lines
            .iter()
            .map(String::as_str)
            .filter(|line| line.starts_with("active"))
            .collect()
    }

    #[test]
    fn every_way_out_of_sight_gives_one_inactive_and_one_active_call() {
        // Скрытие зоны.
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        host.sync();
        host.layout(|layout| layout.set_visible(Hideable::Right, false).unwrap());
        assert_eq!(activity(&host.of(panel)), ["active false"], "зона скрыта");
        assert_eq!(host.of(panel), Vec::<String>::new());
        host.layout(|layout| layout.set_visible(Hideable::Right, true).unwrap());
        assert_eq!(activity(&host.of(panel)), ["active true"], "зона показана");

        // Чужой zoom.
        let terminal = host.fixture.terminal;
        host.layout(|layout| layout.zoom(terminal).unwrap());
        assert_eq!(activity(&host.of(panel)), ["active false"], "чужой zoom");
        assert_eq!(host.of(panel), Vec::<String>::new());
        host.layout(Layout::unzoom);
        assert_eq!(activity(&host.of(panel)), ["active true"], "zoom снят");

        // Смена вкладки: вторая панель в той же части.
        let other = host.add(0x21, "terminal", None);
        host.layout(|layout| layout.open(other, Some(Zone::Right)).unwrap());
        assert_eq!(activity(&host.of(panel)), ["active false"], "вкладка ушла");
        host.layout(|layout| layout.select(panel).unwrap());
        assert_eq!(
            activity(&host.of(panel)),
            ["active true"],
            "вкладка выбрана"
        );

        // Скрытие и показ окна.
        host.visible = false;
        assert_eq!(host.of(panel), ["active false"], "окно скрыто");
        assert_eq!(host.of(panel), Vec::<String>::new());
        host.visible = true;
        assert_eq!(host.of(panel), ["active true"], "окно показано");
    }

    #[test]
    fn min_size_stub_deactivates_the_body_once_and_keeps_it() {
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        host.sync();
        let part = PartKey {
            zone: Zone::Right,
            index: 0,
        };
        // Окно ниже `min_size` демо-списка по высоте: 160 точек тела.
        let wide = host.viewport;
        host.viewport = Viewport {
            height: 190,
            ..wide
        };
        let scene = host.scene();
        let bench = scene.bench.as_ref().unwrap();
        assert!(bench.views[&panel].stub, "тело меньше min_size");
        assert_eq!(bench.views[&panel].host, scene::Host::Part(part));
        assert_eq!(
            host.of(panel),
            ["active false"],
            "под заглушкой тело неактивно"
        );
        assert_eq!(host.of(panel), Vec::<String>::new());

        host.viewport = wide;
        assert_eq!(
            host.of(panel),
            ["active true", "focus_preferred"],
            "та же геометрия: тело не строится и представление не повторяется"
        );
        assert_eq!(host.bodies.built().last(), Some(&(panel, 1)));
    }

    #[test]
    fn update_comes_only_with_new_data_and_changes_the_presentation_in_place() {
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        host.sync();

        // Другая панель меняется, ревизия растёт: списку ничего не приходит.
        let terminal = host.fixture.terminal;
        host.set_data(terminal, &Value::Null);
        assert_eq!(host.of(panel), Vec::<String>::new());

        // Новый выбор: проекция есть, представление прежнее.
        host.set_data(panel, &list("c", false));
        assert_eq!(
            host.of(panel),
            [r#"update {"details":false,"history":[],"key":"c"}"#]
        );

        // Детали раскрыты при той же геометрии: тот же вариант, другие регионы.
        host.set_data(panel, &list("c", true));
        assert_eq!(
            host.of(panel),
            [
                r#"update {"details":true,"history":[],"key":"c"}"#,
                "adapt Right Narrow drill",
            ]
        );
        assert_eq!(host.of(panel), Vec::<String>::new());
    }

    #[test]
    fn data_changed_out_of_sight_reach_the_presentation_when_shown_again() {
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        host.sync();
        host.layout(|layout| layout.set_visible(Hideable::Right, false).unwrap());
        host.sync();
        host.set_data(panel, &list("a", true));
        assert_eq!(
            host.of(panel),
            [r#"update {"details":true,"history":[],"key":"a"}"#]
        );
        host.layout(|layout| layout.set_visible(Hideable::Right, true).unwrap());
        assert_eq!(
            host.of(panel),
            ["adapt Right Narrow drill", "active true"],
            "фокус скрытие зоны увело к другой панели, показ его не возвращает"
        );
    }

    #[test]
    fn leaving_ready_closes_the_body_once_and_ready_again_builds_a_new_one() {
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        host.sync();
        let first = host.shell.generations.last().copied().unwrap();

        host.fixture
            .state
            .panel_status
            .insert(panel, PanelStatus::StateNewer);
        assert_eq!(host.of(panel), ["close"], "закрытие без прочих вызовов");
        assert_eq!(
            host.of(panel),
            Vec::<String>::new(),
            "заглушка статуса: тела нет"
        );
        assert_eq!(host.bodies.variant(panel), None);
        assert!(host.bodies.view(panel).is_none());

        host.fixture
            .state
            .panel_status
            .insert(panel, PanelStatus::Ready);
        let again = host.of(panel);
        assert_eq!(
            again.first().map(String::as_str),
            Some(r#"build {"details":false,"history":[],"key":"a"}"#)
        );
        assert_eq!(again.last().map(String::as_str), Some("focus_preferred"));
        let second = host.shell.generations.last().copied().unwrap();
        assert_eq!(first.0, second.0);
        assert!(second.1 > first.1, "у нового тела новое поколение");
        assert_eq!(host.bodies.built().last(), Some(&(panel, 2)));
    }

    #[test]
    fn panel_that_is_not_ready_gets_no_body() {
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        host.fixture
            .state
            .panel_status
            .insert(panel, PanelStatus::StateCorrupt);
        assert_eq!(host.of(panel), Vec::<String>::new());
        assert_eq!(host.bodies.built().last(), Some(&(panel, 0)));
    }

    #[test]
    fn window_leaving_the_workbench_parks_containers_and_keeps_bodies() {
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        let second = host.second_workbench();
        host.sync();
        assert_eq!(
            host.bodies.built().len(),
            3,
            "тела строятся только для показанной рабочей области"
        );

        host.show(second);
        assert_eq!(
            host.of(panel),
            ["park", "active false", "focused false", "resign"]
        );
        assert!(host.bodies.parked(panel));
        assert_eq!(host.of(panel), Vec::<String>::new(), "парковка один раз");
        assert_eq!(host.bodies.built().len(), 5);

        // Данные припаркованной панели меняются в фоне: тело их получает.
        host.set_data(panel, &list("d", false));
        assert_eq!(
            host.of(panel),
            [r#"update {"details":false,"history":[],"key":"d"}"#]
        );

        let first = host.fixture.workbench;
        host.show(first);
        assert_eq!(
            host.of(panel),
            ["active true", "focused true", "focus_preferred"],
            "то же тело встаёт на место"
        );
        assert!(!host.bodies.parked(panel));
        assert_eq!(host.bodies.built()[2], (panel, 1));
    }

    #[test]
    fn panel_closed_while_parked_is_released_once() {
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        let second = host.second_workbench();
        host.sync();
        host.show(second);
        host.sync();

        let first = host.fixture.workbench;
        let layout = host.fixture.state.layouts.get_mut(&first).unwrap();
        *layout = layout.close(panel).unwrap();
        host.fixture.state.panels.remove(&panel);
        assert_eq!(host.of(panel), ["close", "release"]);
        assert_eq!(host.of(panel), Vec::<String>::new());
        assert!(host.bodies.built().iter().all(|(known, _)| *known != panel));

        host.show(first);
        assert_eq!(
            host.of(panel),
            Vec::<String>::new(),
            "контейнера больше нет"
        );
    }

    #[test]
    fn panel_moved_to_the_shown_workbench_keeps_its_body() {
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        let second = host.second_workbench();
        host.sync();

        // Переезд в фоновую рабочую область: контейнер паркуется, тело живо.
        let first = host.fixture.workbench;
        let layouts = &mut host.fixture.state.layouts;
        let left = layouts[&first].close(panel).unwrap();
        let arrived = layouts[&second].open(panel, Some(Zone::Center)).unwrap();
        layouts.insert(first, left);
        layouts.insert(second, arrived);
        host.fixture.state.panels.get_mut(&panel).unwrap().workbench = second;
        assert_eq!(
            host.of(panel),
            ["park", "active false", "focused false", "resign"]
        );

        host.show(second);
        assert_eq!(
            host.of(panel),
            [
                "adapt Center Regular split",
                "active true",
                "focused true",
                "focus_preferred"
            ],
            "тело прежнее: только новое представление"
        );
        assert_eq!(
            host.bodies
                .built()
                .into_iter()
                .find(|(known, _)| *known == panel),
            Some((panel, 1))
        );
    }

    #[test]
    fn plugin_without_a_factory_is_asked_again_and_counts_nothing() {
        let mut host = Host::new();
        host.shell.missing = vec!["demo-list"];
        let panel = host.open_list(list("a", false));
        assert_eq!(host.of(panel), Vec::<String>::new());
        assert_eq!(host.bodies.built().last(), Some(&(panel, 0)));
    }

    fn late(site: &str) -> u64 {
        late_callbacks()
            .iter()
            .find(|late| late.site == site)
            .map_or(0, |late| late.count)
    }

    #[test]
    fn outcome_reaches_only_the_last_request_of_a_living_body() {
        let requests = RefCell::new(Requests::<&'static str>::default());
        let panel: PanelId = id(0x40);
        requests.borrow_mut().open(panel, 1);
        let first = begin_request(&requests, panel, 1, "first").unwrap();
        let second = begin_request(&requests, panel, 1, "second").unwrap();
        assert_ne!(first, second);
        assert_eq!(
            requests.borrow_mut().settle(first),
            Settled::Superseded,
            "исход более старого запроса отброшен"
        );
        assert_eq!(settle_request(&requests, second), Some("second"));
        assert_eq!(
            settle_request(&requests, second),
            None,
            "исход отдаётся один раз"
        );
    }

    #[test]
    fn stale_callbacks_are_ignored_and_counted() {
        let requests = RefCell::new(Requests::<&'static str>::default());
        let panel: PanelId = id(0x41);
        let (requests_before, done_before) = (late(LATE_REQUEST), late(LATE_DONE));

        // Запрос тела, которого ещё нет.
        assert_eq!(begin_request(&requests, panel, 1, "early"), None);
        assert!(late(LATE_REQUEST) > requests_before);

        // Исход пришёл после закрытия тела.
        requests.borrow_mut().open(panel, 1);
        let ticket = begin_request(&requests, panel, 1, "closed").unwrap();
        assert_eq!(requests.borrow_mut().close(panel), Some("closed"));
        assert_eq!(requests.borrow_mut().settle(ticket), Settled::Stale);
        assert_eq!(settle_request(&requests, ticket), None);
        assert!(late(LATE_DONE) > done_before);

        // Новое тело той же панели: исход и запрос прежнего поколения ему не достаются.
        requests.borrow_mut().open(panel, 2);
        assert_eq!(requests.borrow_mut().settle(ticket), Settled::Stale);
        assert_eq!(begin_request(&requests, panel, 1, "old sink"), None);
        let fresh = begin_request(&requests, panel, 2, "fresh").unwrap();
        assert_eq!(settle_request(&requests, fresh), Some("fresh"));
    }

    #[test]
    fn closing_a_body_drops_its_pending_outcome() {
        let mut host = Host::new();
        let panel = host.open_list(list("a", false));
        host.sync();
        let (owner, generation) = host.shell.generations.last().copied().unwrap();
        assert_eq!(owner, panel);
        let requests = host.bodies.requests();
        let called = Rc::new(RefCell::new(Vec::new()));
        let done: Done = {
            let called = Rc::clone(&called);
            Box::new(move |outcome| called.borrow_mut().push(outcome.is_ok()))
        };
        let ticket = begin_request(&requests, panel, generation, done).unwrap();

        host.fixture
            .state
            .panel_status
            .insert(panel, PanelStatus::MigrateFailed);
        host.sync();
        assert!(settle_request(&requests, ticket).is_none());
        assert!(
            called.borrow().is_empty(),
            "закрытое тело исхода не получает"
        );
    }

    #[test]
    fn requests_of_a_body_become_commands_of_its_panel() {
        let panel: PanelId = id(0x42);
        let workbench: WorkbenchId = id(3);
        assert_eq!(
            command_of(
                panel,
                Some(workbench),
                PanelRequest::Command {
                    name: "demo-list.select".into(),
                    args: json!({"key": "c"}),
                }
            ),
            Command::PanelCommand {
                panel,
                name: "demo-list.select".into(),
                args: Some(json!({"key": "c"})),
            }
        );
        assert_eq!(
            command_of(
                panel,
                Some(workbench),
                PanelRequest::Open {
                    plugin: "demo-native".into(),
                    args: None,
                }
            ),
            Command::PanelOpen {
                plugin: "demo-native".into(),
                workbench: Some(workbench),
                zone: None,
                args: None,
            }
        );
    }

    #[test]
    fn refusals_of_the_core_keep_their_reason_for_the_body() {
        let refused = refusal(&Failure::Core(CoreError::invalid_argument(
            "value",
            "value_too_long",
            "значение длиннее 256 символов",
        )));
        assert_eq!(
            (
                refused.field.as_str(),
                refused.reason.as_str(),
                refused.message.as_str()
            ),
            ("value", "value_too_long", "значение длиннее 256 символов")
        );
        let busy = refusal(&Failure::Core(CoreError::busy()));
        assert_eq!((busy.field.as_str(), busy.reason.as_str()), ("", "busy"));
        assert_eq!(refusal(&Failure::NoReply).reason, "no_reply");
        assert_eq!(refusal(&Failure::NoCore).reason, "no_core");
    }
}
