//! Вид «Демо: список»: список строк и список деталей на одной подложке. Кадры и видимость списков
//! задают регионы представления модели, содержимое — данные панели. Клавиши списков становятся
//! командами панели через отправитель хоста, сам вид данные не меняет.

use std::cell::RefCell;
use std::rc::Rc;

use serde::Deserialize;
use serde_json::{Value, json};
use shellio_appkit::{
    App, FocusLayer, Frame, InspectNode, ListIntent, ListState, Piece, Row, Surface, SurfaceFill,
    SurfaceRadius, SurfaceSpec, VirtualList,
};
use shellio_layout::Geometry;
use shellio_plugin_api::Presentation;
use shellio_plugin_view::{PanelRequest, PluginView, ViewCtx, ViewFactory};
use shellio_theme::AppTheme;

use crate::model::{
    self, BACK, DETAILS, DETAILS_REGION, DRILL, ITEMS, LIST_REGION, MANIFEST, SELECT, State,
};

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

/// Что тело показывает: данные панели и вариант последнего представления.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    state: State,
    variant: Option<&'static str>,
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
            (*id != shown.state.key).then(|| command(SELECT, json!({"key": id})))
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

fn rows() -> Rc<[Row]> {
    ITEMS
        .iter()
        .map(|item| Row::new(item.key, item.title))
        .collect()
}

fn list_state(rows: &Rc<[Row]>, state: &State) -> ListState {
    ListState::new(MANIFEST.title, Rc::clone(rows), Some(&state.key))
}

fn details_state(state: &State) -> ListState {
    let (title, lines) =
        model::find(&state.key).map_or(("", [""; 3]), |item| (item.title, item.details));
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

struct View {
    root: Surface,
    list: VirtualList,
    details: VirtualList,
    /// Строки списка общие для всех состояний: смена выбора не пересобирает набор.
    rows: Rc<[Row]>,
    /// Читают обработчики намерений списков.
    shown: Rc<RefCell<Shown>>,
    frames: Frames,
    focused: bool,
}

impl View {
    fn new(ctx: &ViewCtx<'_>, state: &Value) -> View {
        // Хост строит тело только для панели в `ready`, её данные разобраны моделью.
        let state = parse(state).unwrap_or_else(State::initial);
        let rows = rows();
        let shown = Rc::new(RefCell::new(Shown {
            state: state.clone(),
            variant: None,
        }));
        let sender = |pane| {
            let shown = Rc::clone(&shown);
            let sink = ctx.sink.clone();
            move |intent: ListIntent| {
                let request = request(pane, &intent, &shown.borrow());
                if let Some(request) = request {
                    sink.send(request, Box::new(|_| {}));
                }
            }
        };
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
        let list = build_list(
            ctx.app,
            ctx.theme,
            list_state(&rows, &state),
            sender(Pane::List),
        );
        let details = build_list(
            ctx.app,
            ctx.theme,
            details_state(&state),
            sender(Pane::Details),
        );
        // До первого представления списки стоят в подложке скрытыми.
        for pane in [&list, &details] {
            root.place(pane, Frame::new(0.0, 0.0, 0.0, 0.0));
            pane.set_hidden(true);
        }
        View {
            root,
            list,
            details,
            rows,
            shown,
            frames: Frames::default(),
            focused: false,
        }
    }

    fn pane(&self, pane: Pane) -> &VirtualList {
        match pane {
            Pane::List => &self.list,
            Pane::Details => &self.details,
        }
    }

    /// Списки получают данные панели. Отказа нет: строки фиксированы, выбор проверен разбором.
    fn show_state(&self) {
        let shown = self.shown.borrow();
        let _ = self.list.set_state(list_state(&self.rows, &shown.state));
        let _ = self.details.set_state(details_state(&shown.state));
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

impl PluginView for View {
    fn body(&self) -> Piece<'_> {
        Piece::from(&self.root)
    }

    fn adapt(&mut self, _geometry: &Geometry, presentation: &Presentation) {
        self.shown.borrow_mut().variant = Some(presentation.variant);
        self.show_frames(Frames::of(presentation));
    }

    fn update(&mut self, state: &Value) {
        let Some(state) = parse(state) else {
            return;
        };
        if self.shown.borrow().state == state {
            return;
        }
        self.shown.borrow_mut().state = state;
        self.show_state();
    }

    fn set_theme(&mut self, theme: &AppTheme) {
        self.root.set_theme(theme);
        let _ = self.list.set_theme(theme);
        let _ = self.details.set_theme(theme);
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

    fn close_view(&mut self) {}
}

#[cfg(test)]
mod tests {
    use shellio_appkit::check_list;
    use shellio_layout::Zone;
    use shellio_plugin_api::PanelModel;

    use super::*;
    use crate::model::{BAR, Model, SPLIT};

    fn state(key: &str, history: &[&str], details: bool) -> State {
        State {
            key: key.to_owned(),
            history: history.iter().map(|&key| key.to_owned()).collect(),
            details,
        }
    }

    fn shown(variant: &'static str, state: State) -> Shown {
        Shown {
            state,
            variant: Some(variant),
        }
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
        let rows = rows();
        assert_eq!(rows.len(), 8);
        for item in &ITEMS {
            let state = state(item.key, &[], false);
            let list = list_state(&rows, &state);
            assert_eq!(check_list(&list), Ok(()));
            assert_eq!(list.selected.as_deref(), Some(item.key));
            assert_eq!(list.label, "Демо: список");
            assert!(Rc::ptr_eq(&list.rows, &rows), "набор строк общий");

            let details = details_state(&state);
            assert_eq!(check_list(&details), Ok(()));
            assert_eq!(details.selected, None);
            assert_eq!(details.label, format!("Детали: {}", item.title));
            let lines: Vec<&str> = details.rows.iter().map(|row| row.text.as_str()).collect();
            assert_eq!(lines, item.details);
        }
    }

    #[test]
    fn data_of_the_panel_parse_with_the_invariants_of_the_model() {
        let value = json!({"key": "e", "history": ["a"], "details": true});
        assert_eq!(parse(&value), Some(state("e", &["a"], true)));
        assert_eq!(
            parse(&json!({"key": "z", "history": [], "details": false})),
            None
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
        let before_adapt = Shown {
            state: closed,
            variant: None,
        };
        assert_eq!(request(Pane::List, &activate("e"), &before_adapt), None);
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
