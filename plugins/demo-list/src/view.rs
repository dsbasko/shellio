//! Вид «Демо: список»: список строк и список деталей на одной подложке. Кадры и видимость списков
//! задают регионы представления модели, содержимое — данные панели. Клавиши списков становятся
//! командами панели через отправитель хоста, сам вид данные не меняет. Свой запрошенный выбор
//! списки показывают сразу, не дожидаясь данных панели. Намерения хоста — движения `list.*` и текст
//! фильтра — становятся теми же командами: список показывает только строки, прошедшие фильтр.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use serde::Deserialize;
use serde_json::{Value, json};
use shellio_appkit::{
    App, FocusLayer, Frame, InspectNode, ListIntent, ListState, Piece, Row, Surface, SurfaceFill,
    SurfaceRadius, SurfaceSpec, VirtualList,
};
use shellio_layout::Geometry;
use shellio_plugin_api::Presentation;
use shellio_plugin_view::{
    Handled, ListMove, PanelRequest, PanelSink, PluginView, ViewCtx, ViewFactory, ViewIntent,
};
use shellio_theme::AppTheme;

use crate::model::{
    self, BACK, DETAILS, DETAILS_REGION, DRILL, FILTER, LIST_REGION, MANIFEST, SELECT, State,
};

/// Шаг `list.page_down` и `list.page_up` в строках.
const PAGE: usize = 4;

/// Фабрика тела панели на главном потоке.
#[must_use]
pub fn factory() -> Box<dyn ViewFactory> {
    Box::new(Factory)
}

struct Factory;

impl ViewFactory for Factory {
    fn plugin(&self) -> &'static str {
        MANIFEST.id
    }

    fn build(&self, ctx: ViewCtx<'_>, state: &Value) -> Box<dyn PluginView> {
        Box::new(View::new(&ctx, state))
    }
}

/// Список тела.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pane {
    List,
    Details,
}

/// Что тело показывает: данные панели, вариант последнего представления и свой запрошенный выбор.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    state: State,
    variant: Option<&'static str>,
    /// Ключи своих `select`, данных которых тело ещё не видело, по порядку отправки. Последний —
    /// запрошенный выбор: списки показывают его сразу, и следующая стрелка считается от него.
    sent: Vec<String>,
    /// Хост принял последний запрос тела: ядро выполнило все свои `select`, в пути только их
    /// данные.
    settled: bool,
}

impl Shown {
    fn new(state: State) -> Shown {
        Shown {
            state,
            variant: None,
            sent: Vec::new(),
            settled: false,
        }
    }

    /// Строка, которую показывают списки.
    fn key(&self) -> &str {
        self.sent.last().map_or(&self.state.key, String::as_str)
    }

    /// Запрос хосту на намерение списка `pane`. Свой `select` становится запрошенным выбором,
    /// другой запрос снимает все: исхода прежних `select` тело уже не узнает.
    fn ask(&mut self, pane: Pane, intent: &ListIntent) -> Option<PanelRequest> {
        let request = request(pane, intent, self)?;
        match (pane, intent) {
            (Pane::List, ListIntent::Select { id }) => self.sent.push(id.clone()),
            (Pane::List | Pane::Details, _) => self.sent.clear(),
        }
        self.settled = false;
        Some(request)
    }

    /// Строка, на которую движение `step` переводит выбор среди строк, прошедших фильтр. Без
    /// выбора среди них шаг вниз и к началу берёт первую строку, шаг вверх и к концу — последнюю.
    /// `None` — показанных строк нет или шаг не движение выбора.
    fn target(&self, step: ListMove) -> Option<&'static str> {
        let keys: Vec<&'static str> = model::visible(&self.state.filter)
            .map(|item| item.key)
            .collect();
        let last = keys.len().checked_sub(1)?;
        let at = keys.iter().position(|key| *key == self.key());
        let index = match step {
            ListMove::Down => at.map_or(0, |at| (at + 1).min(last)),
            ListMove::Up => at.map_or(last, |at| at.saturating_sub(1)),
            ListMove::Top => 0,
            ListMove::Bottom => last,
            ListMove::PageDown => at.map_or(0, |at| (at + PAGE).min(last)),
            ListMove::PageUp => at.map_or(last, |at| at.saturating_sub(PAGE)),
            ListMove::Collapse | ListMove::Expand | ListMove::Activate => return None,
        };
        Some(keys[index])
    }

    /// Запрос хосту на намерение панели, пока ввод у списка `input`. Движения выбора и раскрытие
    /// принадлежат списку строк, `list.collapse` закрывает детали `drill` и выбор не трогает.
    /// `None` — намерение принято, но менять нечего: клавиша остаётся в панели.
    fn host(&mut self, input: Pane, intent: &ViewIntent) -> Option<PanelRequest> {
        let step = match intent {
            ViewIntent::List(step) => *step,
            ViewIntent::Filter { text } => {
                if *text == self.state.filter {
                    return None;
                }
                self.sent.clear();
                self.settled = false;
                return Some(command(FILTER, json!({"text": text})));
            }
        };
        match (input, step) {
            (Pane::Details, ListMove::Collapse) if self.variant == Some(DRILL) => {
                self.ask(Pane::Details, &ListIntent::Cancel)
            }
            (Pane::List | Pane::Details, ListMove::Collapse) => None,
            (Pane::List, ListMove::Expand | ListMove::Activate) => {
                let id = self.key().to_owned();
                self.ask(Pane::List, &ListIntent::Activate { id })
            }
            (Pane::List, _) => {
                let id = self.target(step)?.to_owned();
                self.ask(Pane::List, &ListIntent::Select { id })
            }
            // Строки деталей не выбираются и не открываются.
            (Pane::Details, _) => None,
        }
    }

    /// Новые данные панели. Данные приходят по порядку ревизий, поэтому совпавший ключ подтверждает
    /// самый ранний свой `select` с ним, а не запрошенный выбор: тот уходит только со своими
    /// данными. Чужие данные после принятого запроса снимают все свои `select`: `select`, который
    /// остался без данных, выбор не держит. `false` — данные прежние.
    fn update(&mut self, state: State) -> bool {
        if self.state == state {
            return false;
        }
        match self.sent.iter().position(|key| *key == state.key) {
            Some(own) => {
                self.sent.drain(..=own);
            }
            None if self.settled => self.sent.clear(),
            None => {}
        }
        self.state = state;
        true
    }

    /// Исход последнего запроса тела. Отказ возвращает выбор данных панели. Успех оставляет
    /// запрошенный выбор до его данных: исход приходит независимо от них и может обогнать данные
    /// прежних своих `select`.
    fn done(&mut self, accepted: bool) {
        if accepted {
            self.settled = true;
        } else {
            self.sent.clear();
        }
    }
}

/// Кадры списков по регионам представления. `None` — список не показан.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Frames {
    list: Option<Frame>,
    details: Option<Frame>,
}

impl Frames {
    fn of(presentation: &Presentation) -> Frames {
        let frame = |name| {
            presentation
                .regions
                .iter()
                .find(|region| region.name == name)
                .map(|region| {
                    let frame = region.frame;
                    Frame::new(frame.x, frame.y, frame.width, frame.height)
                })
        };
        Frames {
            list: frame(LIST_REGION),
            details: frame(DETAILS_REGION),
        }
    }

    fn of_pane(&self, pane: Pane) -> Option<Frame> {
        match pane {
            Pane::List => self.list,
            Pane::Details => self.details,
        }
    }

    /// Список, которому тело отдаёт ввод: детали — только когда показаны они одни.
    fn input(&self) -> Pane {
        match (self.list, self.details) {
            (None, Some(_)) => Pane::Details,
            (Some(_), _) | (None, None) => Pane::List,
        }
    }
}

fn command(name: &str, args: Value) -> PanelRequest {
    PanelRequest::Command {
        name: name.to_owned(),
        args,
    }
}

/// Запрос хосту на намерение списка `pane`. `Esc` — всегда уровень назад: из деталей `drill` к
/// списку, иначе к прежней строке истории. `Return` раскрывает детали только в `drill`.
fn request(pane: Pane, intent: &ListIntent, shown: &Shown) -> Option<PanelRequest> {
    let drill = shown.variant == Some(DRILL);
    let back = || (!shown.state.history.is_empty()).then(|| command(BACK, json!({})));
    match (pane, intent) {
        (Pane::List, ListIntent::Select { id }) => {
            (id != shown.key()).then(|| command(SELECT, json!({"key": id})))
        }
        (Pane::List, ListIntent::Activate { .. }) => {
            drill.then(|| command(DETAILS, json!({"open": true})))
        }
        (Pane::Details, ListIntent::Cancel) if drill => {
            Some(command(DETAILS, json!({"open": false})))
        }
        (Pane::List | Pane::Details, ListIntent::Cancel) => back(),
        // Строки деталей не выбираются и не открываются.
        (Pane::Details, ListIntent::Select { .. } | ListIntent::Activate { .. }) => None,
    }
}

/// Строки списка, прошедшие фильтр.
fn rows(filter: &str) -> Rc<[Row]> {
    model::visible(filter)
        .map(|item| Row::new(item.key, item.title))
        .collect()
}

/// Список строк `rows`. Выбранная строка, которую фильтр скрыл, выбора в списке не оставляет.
fn list_state(rows: &Rc<[Row]>, key: &str) -> ListState {
    let selected = rows.iter().any(|row| row.id == key).then_some(key);
    ListState::new(MANIFEST.title, Rc::clone(rows), selected)
}

fn details_state(key: &str) -> ListState {
    let (title, lines) = model::find(key).map_or(("", [""; 3]), |item| (item.title, item.details));
    let rows: Vec<Row> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| Row::new(&index.to_string(), line))
        .collect();
    ListState::new(&format!("Детали: {title}"), rows, None)
}

fn parse(state: &Value) -> Option<State> {
    State::deserialize(state).ok()
}

fn build_list(
    app: &App,
    theme: &AppTheme,
    state: ListState,
    on_intent: impl Fn(ListIntent) + 'static,
) -> VirtualList {
    match VirtualList::new(app, theme, state, on_intent) {
        Ok(list) => list,
        // Ключи строк различны, выбор взят из разобранных данных: `list_states_are_valid`.
        Err(error) => unreachable!("список «{}» не построен: {error}", MANIFEST.id),
    }
}

/// Часть тела, которую делят вид, обработчики намерений списков и исход запроса.
struct Shared {
    shown: RefCell<Shown>,
    /// Обработчики списков держат на общую часть слабую ссылку.
    list: VirtualList,
    details: VirtualList,
    /// Строки списка и фильтр, по которому они собраны: смена выбора набор не пересобирает.
    rows: RefCell<(String, Rc<[Row]>)>,
    sink: PanelSink,
}

impl Shared {
    /// Списки получают выбор тела. Отказа нет: строки фиксированы, выбор проверен разбором или взят
    /// из строк списка.
    fn show(&self) {
        let (list, details) = {
            let shown = self.shown.borrow();
            let mut rows = self.rows.borrow_mut();
            if rows.0 != shown.state.filter {
                *rows = (shown.state.filter.clone(), self::rows(&shown.state.filter));
            }
            (list_state(&rows.1, shown.key()), details_state(shown.key()))
        };
        let _ = self.list.set_state(list);
        let _ = self.details.set_state(details);
    }

    fn intent(this: &Rc<Shared>, pane: Pane, intent: &ListIntent) {
        let request = this.shown.borrow_mut().ask(pane, intent);
        Shared::send(this, request);
    }

    /// Намерение хоста, пока ввод у списка `input`.
    fn host(this: &Rc<Shared>, input: Pane, intent: &ViewIntent) {
        let request = this.shown.borrow_mut().host(input, intent);
        Shared::send(this, request);
    }

    fn send(this: &Rc<Shared>, request: Option<PanelRequest>) {
        let Some(request) = request else {
            return;
        };
        this.show();
        let shared = Rc::downgrade(this);
        // Общая часть свободна: хост вправе вернуть исход раньше, чем вернётся отправка.
        this.sink.send(
            request,
            Box::new(move |outcome| {
                if let Some(shared) = shared.upgrade() {
                    shared.shown.borrow_mut().done(outcome.is_ok());
                    shared.show();
                }
            }),
        );
    }
}

struct View {
    root: Surface,
    shared: Rc<Shared>,
    frames: Frames,
    focused: bool,
}

impl View {
    fn new(ctx: &ViewCtx<'_>, state: &Value) -> View {
        // Хост строит тело только для панели в `ready`, её данные разобраны моделью.
        let state = parse(state).unwrap_or_else(State::initial);
        let rows = rows(&state.filter);
        let root = Surface::new(
            ctx.app,
            ctx.theme,
            SurfaceSpec {
                fill: SurfaceFill::Panel,
                line: false,
                radius: SurfaceRadius::None,
                layer: FocusLayer::Plain,
            },
        );
        let shared = Rc::new_cyclic(|weak: &Weak<Shared>| {
            let sender = |pane| {
                let shared = weak.clone();
                move |intent: ListIntent| {
                    if let Some(shared) = shared.upgrade() {
                        Shared::intent(&shared, pane, &intent);
                    }
                }
            };
            Shared {
                list: build_list(
                    ctx.app,
                    ctx.theme,
                    list_state(&rows, &state.key),
                    sender(Pane::List),
                ),
                details: build_list(
                    ctx.app,
                    ctx.theme,
                    details_state(&state.key),
                    sender(Pane::Details),
                ),
                rows: RefCell::new((state.filter.clone(), rows)),
                shown: RefCell::new(Shown::new(state)),
                sink: ctx.sink.clone(),
            }
        });
        // До первого представления списки стоят в подложке скрытыми.
        for pane in [&shared.list, &shared.details] {
            root.place(pane, Frame::new(0.0, 0.0, 0.0, 0.0));
            pane.set_hidden(true);
        }
        View {
            root,
            shared,
            frames: Frames::default(),
            focused: false,
        }
    }

    fn pane(&self, pane: Pane) -> &VirtualList {
        match pane {
            Pane::List => &self.shared.list,
            Pane::Details => &self.shared.details,
        }
    }

    /// Списки встают в кадры регионов. Ввод переходит в список нового уровня до того, как прежний
    /// скрыт: скрытый список first responder не остаётся.
    fn show_frames(&mut self, frames: Frames) {
        let before = self.frames.input();
        let after = frames.input();
        for pane in [Pane::List, Pane::Details] {
            if let Some(frame) = frames.of_pane(pane) {
                if self.frames.of_pane(pane) != Some(frame) {
                    self.root.place(self.pane(pane), frame);
                }
                self.pane(pane).set_hidden(false);
            }
        }
        let orphaned = [Pane::List, Pane::Details]
            .into_iter()
            .any(|pane| frames.of_pane(pane).is_none() && self.pane(pane).has_focus());
        if orphaned || (before != after && self.focused) {
            self.pane(after).focus();
        }
        for pane in [Pane::List, Pane::Details] {
            if frames.of_pane(pane).is_none() {
                self.pane(pane).set_hidden(true);
            }
        }
        self.frames = frames;
    }
}

/// Ответ вида на намерение хоста. Плагин объявил `list_keys` и `filter`: вид принимает каждое
/// движение списка и текст фильтра в любой зоне, в том числе когда менять нечего.
fn handled(intent: &ViewIntent) -> Handled {
    match intent {
        ViewIntent::List(_) | ViewIntent::Filter { .. } => Handled::Yes,
    }
}

impl PluginView for View {
    fn body(&self) -> Piece<'_> {
        Piece::from(&self.root)
    }

    fn adapt(&mut self, _geometry: &Geometry, presentation: &Presentation) {
        self.shared.shown.borrow_mut().variant = Some(presentation.variant);
        self.show_frames(Frames::of(presentation));
    }

    fn update(&mut self, state: &Value) {
        let Some(state) = parse(state) else {
            return;
        };
        let changed = self.shared.shown.borrow_mut().update(state);
        if changed {
            self.shared.show();
        }
    }

    fn set_theme(&mut self, theme: &AppTheme) {
        self.root.set_theme(theme);
        let _ = self.shared.list.set_theme(theme);
        let _ = self.shared.details.set_theme(theme);
    }

    fn set_active(&mut self, _active: bool) {}

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn focus_preferred(&self) {
        self.pane(self.frames.input()).focus();
    }

    fn inspect(&self, prefix: &str) -> Vec<InspectNode> {
        let parent = prefix.strip_suffix('.').unwrap_or(prefix);
        let body = format!("{prefix}body");
        let mut nodes = self.root.inspect(&body, Some(parent));
        for (pane, name) in [(Pane::List, LIST_REGION), (Pane::Details, DETAILS_REGION)] {
            if self.frames.of_pane(pane).is_some() {
                nodes.extend(
                    self.pane(pane)
                        .inspect(&format!("{prefix}{name}"), Some(&body)),
                );
            }
        }
        nodes
    }

    fn intent(&mut self, intent: &ViewIntent) -> Handled {
        Shared::host(&self.shared, self.frames.input(), intent);
        handled(intent)
    }

    fn resolve_args(&self, action: &str) -> Option<Value> {
        resolve(action)
    }

    fn close_view(&mut self) {}
}

/// `args` действия, вызванного клавишей без своих `args`. Без ввода исполнимо только «назад»:
/// остальным действиям нужны значения, которых у вида нет.
fn resolve(action: &str) -> Option<Value> {
    (action == BACK).then(|| json!({}))
}

#[cfg(test)]
mod tests {
    use shellio_appkit::{ListKey, check_list, list_key_intent};
    use shellio_layout::Zone;
    use shellio_plugin_api::PanelModel;

    use super::*;
    use crate::model::{BAR, ITEMS, Model, SPLIT};

    fn state(key: &str, history: &[&str], details: bool) -> State {
        filtered(key, history, details, "")
    }

    fn filtered(key: &str, history: &[&str], details: bool, filter: &str) -> State {
        State {
            key: key.to_owned(),
            history: history.iter().map(|&key| key.to_owned()).collect(),
            details,
            filter: filter.to_owned(),
        }
    }

    fn shown(variant: &'static str, state: State) -> Shown {
        Shown {
            variant: Some(variant),
            ..Shown::new(state)
        }
    }

    /// Намерение стрелки вниз от строки, которую список показывает.
    fn down(rows: &Rc<[Row]>, shown: &Shown) -> ListIntent {
        list_key_intent(&list_state(rows, shown.key()), ListKey::Down).unwrap()
    }

    fn select(id: &str) -> ListIntent {
        ListIntent::Select { id: id.to_owned() }
    }

    fn activate(id: &str) -> ListIntent {
        ListIntent::Activate { id: id.to_owned() }
    }

    fn presentation(zone: Zone, width: f64, height: f64, state: &State) -> Presentation {
        Model.adapt(&Geometry::of(zone, width, height), state)
    }

    #[test]
    fn factory_names_the_plugin_of_the_manifest() {
        assert_eq!(factory().plugin(), "demo-list");
    }

    #[test]
    fn list_states_are_valid() {
        let rows = rows("");
        assert_eq!(rows.len(), 8);
        for item in &ITEMS {
            let state = state(item.key, &[], false);
            let list = list_state(&rows, &state.key);
            assert_eq!(check_list(&list), Ok(()));
            assert_eq!(list.selected.as_deref(), Some(item.key));
            assert_eq!(list.label, "Демо: список");
            assert!(Rc::ptr_eq(&list.rows, &rows), "набор строк общий");

            let details = details_state(&state.key);
            assert_eq!(check_list(&details), Ok(()));
            assert_eq!(details.selected, None);
            assert_eq!(details.label, format!("Детали: {}", item.title));
            let lines: Vec<&str> = details.rows.iter().map(|row| row.text.as_str()).collect();
            assert_eq!(lines, item.details);
        }
    }

    #[test]
    fn data_of_the_panel_parse_with_the_invariants_of_the_model() {
        let value = json!({"key": "e", "history": ["a"], "details": true, "filter": "эп"});
        assert_eq!(parse(&value), Some(filtered("e", &["a"], true, "эп")));
        assert_eq!(
            parse(&json!({"key": "z", "history": [], "details": false, "filter": ""})),
            None
        );
        assert_eq!(
            parse(&json!({"key": "a", "history": [], "details": false})),
            None,
            "данные версии 1"
        );
        assert_eq!(parse(&Value::Null), None);
    }

    #[test]
    fn arrows_and_clicks_select_another_row() {
        let at_c = shown(DRILL, state("c", &[], false));
        assert_eq!(
            request(Pane::List, &select("d"), &at_c),
            Some(command(SELECT, json!({"key": "d"})))
        );
        assert_eq!(
            request(Pane::List, &select("c"), &at_c),
            None,
            "та же строка"
        );
        for variant in [SPLIT, BAR] {
            let at_c = shown(variant, state("c", &[], true));
            assert_eq!(
                request(Pane::List, &select("a"), &at_c),
                Some(command(SELECT, json!({"key": "a"})))
            );
        }
    }

    #[test]
    fn return_opens_details_only_in_drill() {
        let closed = state("e", &[], false);
        assert_eq!(
            request(Pane::List, &activate("e"), &shown(DRILL, closed.clone())),
            Some(command(DETAILS, json!({"open": true})))
        );
        for variant in [SPLIT, BAR] {
            assert_eq!(
                request(Pane::List, &activate("e"), &shown(variant, closed.clone())),
                None
            );
        }
        let before_adapt = Shown::new(closed);
        assert_eq!(request(Pane::List, &activate("e"), &before_adapt), None);
    }

    #[test]
    fn arrows_ahead_of_the_data_move_from_the_requested_row() {
        let rows = rows("");
        let mut at = shown(SPLIT, state("a", &[], false));
        let first = down(&rows, &at);
        assert_eq!(
            at.ask(Pane::List, &first),
            Some(command(SELECT, json!({"key": "b"})))
        );
        assert_eq!(at.key(), "b", "список показывает запрошенную строку");
        // Вторая стрелка до данных первой идёт от `b`, а не повторяет её.
        let second = down(&rows, &at);
        assert_eq!(
            at.ask(Pane::List, &second),
            Some(command(SELECT, json!({"key": "c"})))
        );
        assert_eq!(at.key(), "c");

        // Данные первой стрелки выбор назад не сдвигают, исход успеха его не снимает.
        assert!(at.update(state("b", &[], false)));
        at.done(true);
        assert_eq!(at.key(), "c");
        assert!(at.update(state("c", &[], false)));
        assert!(at.sent.is_empty());
        assert_eq!(at.key(), "c");
    }

    /// Вниз, вниз, вверх от `a`: запросы `b`, `c`, `b`.
    fn down_down_up() -> Shown {
        let mut at = shown(SPLIT, state("a", &[], false));
        for key in ["b", "c", "b"] {
            assert!(at.ask(Pane::List, &select(key)).is_some());
        }
        at
    }

    #[test]
    fn data_of_an_earlier_select_do_not_confirm_a_later_one_of_the_same_row() {
        let rows = rows("");
        // Успех последнего запроса приходит до данных, между ними и после них.
        for accepted_after in 0..=3 {
            let mut at = down_down_up();
            for (seen, key) in ["b", "c", "b"].into_iter().enumerate() {
                if seen == accepted_after {
                    at.done(true);
                }
                assert!(at.update(state(key, &[], false)));
                assert_eq!(at.key(), "b", "исход после {accepted_after}, данные {seen}");
            }
            if accepted_after == 3 {
                at.done(true);
            }
            assert!(at.sent.is_empty(), "исход после {accepted_after}");
        }

        // Стрелка между данными `c` и последними данными `b` идёт от `b`.
        let mut at = down_down_up();
        at.update(state("b", &[], false));
        at.update(state("c", &[], false));
        let next = down(&rows, &at);
        assert_eq!(
            at.ask(Pane::List, &next),
            Some(command(SELECT, json!({"key": "c"})))
        );
    }

    #[test]
    fn select_without_data_does_not_hold_the_row() {
        // Данные пришли снимком: промежуточных ревизий тело не видело.
        for accepted_first in [true, false] {
            let mut at = down_down_up();
            if accepted_first {
                at.done(true);
            }
            at.update(state("b", &[], false));
            if !accepted_first {
                at.done(true);
            }
            assert_eq!(at.key(), "b");
            assert!(at.update(state("d", &[], false)));
            assert_eq!(at.key(), "d", "чужое изменение после принятого запроса");
            assert!(at.sent.is_empty());
        }
    }

    #[test]
    fn requested_row_is_compared_instead_of_the_stale_one() {
        let mut at = shown(DRILL, state("a", &[], false));
        assert!(at.ask(Pane::List, &select("b")).is_some());
        assert_eq!(at.ask(Pane::List, &select("b")), None, "та же строка");
        assert_eq!(
            at.ask(Pane::List, &select("a")),
            Some(command(SELECT, json!({"key": "a"}))),
            "возврат к строке данных, пока `b` в пути"
        );
        assert_eq!(at.key(), "a");
        // Данные `b` выбор не сдвигают, данные `a` его подтверждают.
        at.update(state("b", &[], false));
        assert_eq!(at.key(), "a");
        at.update(state("a", &[], false));
        assert!(at.sent.is_empty());
    }

    #[test]
    fn requested_row_leaves_with_its_request() {
        let pending = || {
            let mut at = shown(DRILL, state("a", &["h"], false));
            assert!(at.ask(Pane::List, &select("b")).is_some());
            at
        };

        let mut at = pending();
        at.done(false);
        assert_eq!(at.key(), "a", "отказ возвращает выбор данных панели");

        // Другой запрос тела: исхода `select` оно уже не узнает.
        let mut at = pending();
        assert_eq!(
            at.ask(Pane::List, &activate("b")),
            Some(command(DETAILS, json!({"open": true})))
        );
        assert!(at.sent.is_empty());
        let mut at = pending();
        assert_eq!(
            at.ask(Pane::List, &ListIntent::Cancel),
            Some(command(BACK, json!({})))
        );
        assert!(at.sent.is_empty());

        // Намерение без запроса выбор не трогает.
        let mut at = pending();
        assert_eq!(at.ask(Pane::Details, &select("1")), None);
        assert_eq!(at.key(), "b");

        // Возврат к строке данных: успех обгоняет данные `b`, и они выбор не сдвигают.
        let mut at = pending();
        assert!(at.ask(Pane::List, &select("a")).is_some());
        at.done(true);
        assert!(at.update(state("b", &["h"], false)));
        assert_eq!(at.key(), "a");
        assert!(at.update(state("a", &["h"], false)));
        assert!(at.sent.is_empty());

        // Чужое изменение запрошенный выбор не снимает, пока исхода запроса нет.
        let mut at = pending();
        at.update(state("d", &["h"], false));
        assert_eq!(at.key(), "b");
        at.update(state("b", &["h"], false));
        assert!(at.sent.is_empty());
    }

    #[test]
    fn escape_goes_one_level_back() {
        let open = state("e", &["a"], true);
        assert_eq!(
            request(
                Pane::Details,
                &ListIntent::Cancel,
                &shown(DRILL, open.clone())
            ),
            Some(command(DETAILS, json!({"open": false}))),
            "из деталей drill — к списку"
        );
        for variant in [DRILL, SPLIT, BAR] {
            assert_eq!(
                request(
                    Pane::List,
                    &ListIntent::Cancel,
                    &shown(variant, open.clone())
                ),
                Some(command(BACK, json!({}))),
                "{variant}: из списка — к прежней строке"
            );
            let empty = shown(variant, state("e", &[], false));
            assert_eq!(
                request(Pane::List, &ListIntent::Cancel, &empty),
                None,
                "{variant}: пустая история"
            );
        }
        for variant in [SPLIT, BAR] {
            assert_eq!(
                request(
                    Pane::Details,
                    &ListIntent::Cancel,
                    &shown(variant, open.clone())
                ),
                Some(command(BACK, json!({}))),
                "{variant}: детали рядом со списком — тот же уровень"
            );
        }
    }

    #[test]
    fn detail_rows_send_nothing() {
        for variant in [DRILL, SPLIT, BAR] {
            let open = shown(variant, state("e", &["a"], true));
            assert_eq!(request(Pane::Details, &select("1"), &open), None);
            assert_eq!(request(Pane::Details, &activate("1"), &open), None);
        }
    }

    #[test]
    fn requests_parse_as_commands_of_the_model() {
        let model = shellio_plugin_api::DynModel::new(Model);
        let data = serde_json::to_string(&state("c", &["a"], false)).unwrap();
        let requests = [
            request(
                Pane::List,
                &select("d"),
                &shown(DRILL, state("c", &["a"], false)),
            ),
            request(
                Pane::List,
                &activate("c"),
                &shown(DRILL, state("c", &["a"], false)),
            ),
            request(
                Pane::Details,
                &ListIntent::Cancel,
                &shown(DRILL, state("c", &["a"], true)),
            ),
            request(
                Pane::List,
                &ListIntent::Cancel,
                &shown(SPLIT, state("c", &["a"], false)),
            ),
        ];
        for request in requests {
            let Some(PanelRequest::Command { name, args }) = request else {
                panic!("ожидалась команда панели: {request:?}");
            };
            model.command(&data, &name, Some(args)).unwrap();
        }
    }

    fn list(step: ListMove) -> ViewIntent {
        ViewIntent::List(step)
    }

    fn filter(text: &str) -> ViewIntent {
        ViewIntent::Filter {
            text: text.to_owned(),
        }
    }

    #[test]
    fn filtered_list_shows_the_passing_rows_and_drops_a_hidden_selection() {
        let rows = rows("та");
        let ids: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["b", "d", "f", "g", "h"]);
        let shown = list_state(&rows, "d");
        assert_eq!(check_list(&shown), Ok(()));
        assert_eq!(shown.selected.as_deref(), Some("d"));
        let hidden = list_state(&rows, "a");
        assert_eq!(check_list(&hidden), Ok(()));
        assert_eq!(hidden.selected, None);
        let empty = list_state(&self::rows("нет"), "a");
        assert_eq!(check_list(&empty), Ok(()));
        assert!(empty.rows.is_empty());
    }

    #[test]
    fn list_moves_select_the_row_of_the_step() {
        let select_of = |key: &str, step| {
            let mut at = shown(SPLIT, state(key, &[], false));
            at.host(Pane::List, &list(step))
        };
        let to = |key: &str| Some(command(SELECT, json!({"key": key})));
        assert_eq!(select_of("c", ListMove::Down), to("d"));
        assert_eq!(select_of("c", ListMove::Up), to("b"));
        assert_eq!(select_of("c", ListMove::Top), to("a"));
        assert_eq!(select_of("c", ListMove::Bottom), to("h"));
        assert_eq!(select_of("c", ListMove::PageDown), to("g"));
        assert_eq!(select_of("f", ListMove::PageDown), to("h"));
        assert_eq!(select_of("f", ListMove::PageUp), to("b"));
        assert_eq!(select_of("c", ListMove::PageUp), to("a"));
        // За краем менять нечего: запроса нет, намерение всё равно принято видом.
        for (key, step) in [
            ("h", ListMove::Down),
            ("h", ListMove::Bottom),
            ("h", ListMove::PageDown),
            ("a", ListMove::Up),
            ("a", ListMove::Top),
            ("a", ListMove::PageUp),
        ] {
            assert_eq!(select_of(key, step), None, "{key} {step:?}");
        }
    }

    #[test]
    fn list_moves_ahead_of_the_data_go_from_the_requested_row() {
        let mut at = shown(SPLIT, state("a", &[], false));
        for key in ["b", "c"] {
            assert_eq!(
                at.host(Pane::List, &list(ListMove::Down)),
                Some(command(SELECT, json!({"key": key})))
            );
            assert_eq!(at.key(), key);
        }
    }

    #[test]
    fn list_moves_stay_inside_the_filtered_rows() {
        // Показаны b, d, f, g, h.
        let mut at = shown(SPLIT, filtered("d", &[], false, "та"));
        assert_eq!(
            at.host(Pane::List, &list(ListMove::Down)),
            Some(command(SELECT, json!({"key": "f"})))
        );
        let mut at = shown(SPLIT, filtered("d", &[], false, "та"));
        assert_eq!(
            at.host(Pane::List, &list(ListMove::Top)),
            Some(command(SELECT, json!({"key": "b"})))
        );
        // Выбранная строка скрыта фильтром: шаг вниз берёт первую показанную, вверх — последнюю.
        let mut at = shown(SPLIT, filtered("a", &[], false, "та"));
        assert_eq!(
            at.host(Pane::List, &list(ListMove::Down)),
            Some(command(SELECT, json!({"key": "b"})))
        );
        let mut at = shown(SPLIT, filtered("a", &[], false, "та"));
        assert_eq!(
            at.host(Pane::List, &list(ListMove::Up)),
            Some(command(SELECT, json!({"key": "h"})))
        );
        // Ни одной строки: двигаться некуда.
        let mut at = shown(SPLIT, filtered("a", &[], false, "нет"));
        for step in ListMove::ALL {
            assert_eq!(at.host(Pane::List, &list(step)), None, "{step:?}");
        }
    }

    #[test]
    fn expand_and_activate_open_details_and_collapse_closes_them_only_in_drill() {
        for step in [ListMove::Expand, ListMove::Activate] {
            let mut at = shown(DRILL, state("e", &["a"], false));
            assert_eq!(
                at.host(Pane::List, &list(step)),
                Some(command(DETAILS, json!({"open": true}))),
                "{step:?}"
            );
            for variant in [SPLIT, BAR] {
                let mut at = shown(variant, state("e", &["a"], false));
                assert_eq!(at.host(Pane::List, &list(step)), None, "{variant} {step:?}");
            }
        }
        let mut open = shown(DRILL, state("e", &["a"], true));
        assert_eq!(
            open.host(Pane::Details, &list(ListMove::Collapse)),
            Some(command(DETAILS, json!({"open": false})))
        );
        // `h` в списке историю «назад» не трогает: выбор прежний.
        for variant in [DRILL, SPLIT, BAR] {
            let mut at = shown(variant, state("e", &["a"], false));
            assert_eq!(
                at.host(Pane::List, &list(ListMove::Collapse)),
                None,
                "{variant}"
            );
        }
        // Пока открыты детали `drill`, выбор не двигается.
        for step in [ListMove::Down, ListMove::Top, ListMove::Activate] {
            assert_eq!(open.host(Pane::Details, &list(step)), None, "{step:?}");
        }
    }

    #[test]
    fn filter_text_becomes_the_filter_command_once() {
        let mut at = shown(SPLIT, state("a", &[], false));
        assert!(at.ask(Pane::List, &select("b")).is_some());
        assert_eq!(
            at.host(Pane::List, &filter("та")),
            Some(command(FILTER, json!({"text": "та"})))
        );
        assert!(at.sent.is_empty(), "другой запрос снимает свой выбор");
        assert!(at.update(filtered("a", &[], false, "та")));
        assert_eq!(at.host(Pane::List, &filter("та")), None, "тот же текст");
        assert_eq!(
            at.host(Pane::Details, &filter("")),
            Some(command(FILTER, json!({"text": ""})))
        );
    }

    #[test]
    fn view_accepts_every_declared_intent_in_every_zone() {
        let intents = ViewIntent::declared(MANIFEST.list_keys, MANIFEST.filter);
        assert_eq!(
            intents.len(),
            ListMove::ALL.len() + 2,
            "плагин объявил оба признака"
        );
        // Боковая, широкая и низкая панель в каждой зоне, список и раскрытые детали.
        let mut cases = Vec::new();
        for zone in Zone::ALL {
            for (width, height) in [(300.0, 400.0), (900.0, 500.0), (900.0, 200.0)] {
                for details in [false, true] {
                    cases.push((
                        format!("{zone:?} {width}×{height}, детали {details}"),
                        (zone, width, height, details),
                    ));
                }
            }
        }
        let model = shellio_plugin_api::DynModel::new(Model);
        let mut variants = std::collections::BTreeSet::new();
        let mut sent = 0;
        let refusals = shellio_plugin_view::check_intents(
            cases,
            &intents,
            |&(zone, width, height, details), intent| {
                let state = state("c", &["a"], details);
                let presentation = presentation(zone, width, height, &state);
                let data = serde_json::to_string(&state).unwrap();
                variants.insert(presentation.variant);
                let mut at = shown(presentation.variant, state);
                let input = Frames::of(&presentation).input();
                // Запрос вида на намерение — команда, которую модель принимает.
                if let Some(request) = at.host(input, intent) {
                    let PanelRequest::Command { name, args } = request else {
                        panic!("ожидалась команда панели");
                    };
                    model.command(&data, &name, Some(args)).unwrap();
                    sent += 1;
                }
                handled(intent)
            },
        );
        assert_eq!(refusals, []);
        assert_eq!(
            variants.into_iter().collect::<Vec<_>>(),
            [BAR, DRILL, SPLIT],
            "намерения проверены в каждом представлении"
        );
        assert!(sent > 0);
    }

    #[test]
    fn key_without_args_runs_only_back() {
        let model = shellio_plugin_api::DynModel::new(Model);
        let data = serde_json::to_string(&state("c", &["a"], false)).unwrap();
        for spec in MANIFEST.commands {
            match resolve(spec.name) {
                Some(args) => {
                    assert_eq!(spec.name, BACK);
                    model.command(&data, spec.name, Some(args)).unwrap();
                }
                None => assert_ne!(spec.name, BACK),
            }
        }
        assert_eq!(resolve("demo-list.unknown"), None);
    }

    #[test]
    fn requests_of_host_intents_parse_as_commands_of_the_model() {
        let model = shellio_plugin_api::DynModel::new(Model);
        let data = serde_json::to_string(&state("c", &["a"], false)).unwrap();
        let mut intents: Vec<ViewIntent> = ListMove::ALL.into_iter().map(list).collect();
        intents.push(filter("та"));
        let mut sent = 0;
        for variant in [DRILL, SPLIT, BAR] {
            for intent in &intents {
                let mut at = shown(variant, state("c", &["a"], false));
                if let Some(request) = at.host(Pane::List, intent) {
                    let PanelRequest::Command { name, args } = request else {
                        panic!("ожидалась команда панели");
                    };
                    model.command(&data, &name, Some(args)).unwrap();
                    sent += 1;
                }
            }
        }
        assert!(
            sent >= 3 * 7,
            "движения выбора и фильтр дают запросы: {sent}"
        );
        assert_eq!(ITEMS.len(), 8);
    }

    #[test]
    fn frames_follow_the_regions_and_input_follows_the_level() {
        assert_eq!(
            Frames::default().input(),
            Pane::List,
            "до первого представления"
        );

        let closed = state("c", &[], false);
        let frames = Frames::of(&presentation(Zone::Right, 300.0, 400.0, &closed));
        assert_eq!(
            frames,
            Frames {
                list: Some(Frame::new(0.0, 0.0, 300.0, 400.0)),
                details: None,
            }
        );
        assert_eq!(frames.input(), Pane::List);

        let open = state("c", &[], true);
        let frames = Frames::of(&presentation(Zone::Right, 300.0, 400.0, &open));
        assert_eq!(
            frames,
            Frames {
                list: None,
                details: Some(Frame::new(0.0, 0.0, 300.0, 400.0)),
            }
        );
        assert_eq!(frames.input(), Pane::Details);
        assert_eq!(frames.of_pane(Pane::List), None);

        // Рядом со списком детали ввод не забирают, даже раскрытые.
        let frames = Frames::of(&presentation(Zone::Center, 700.0, 400.0, &open));
        assert_eq!(
            frames,
            Frames {
                list: Some(Frame::new(0.0, 0.0, 280.0, 400.0)),
                details: Some(Frame::new(280.0, 0.0, 420.0, 400.0)),
            }
        );
        assert_eq!(frames.input(), Pane::List);
    }
}
