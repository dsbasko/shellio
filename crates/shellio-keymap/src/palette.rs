//! Выдача палитры P1.6 (ADR-0010, «Уточнено в P1.6 (планирование): наложения»): группы «Команды»
//! и «Панели», категории `Tab`, сужение запросом, предел строк и выбранная строка.
//!
//! Порядок строк задаёт модель: закреплённые команды, действия панели с фокусом, остальные
//! действия с названием; открытые панели рабочей области, затем плагины реестра.

use std::cmp::Reverse;
use std::collections::HashSet;

use serde::Serialize;
use serde_json::{Value, json};

use crate::fuzzy::score;
use crate::keymap::{Context, Keymap};
use crate::sequence::Sequence;

/// Предел строк выдачи (прототип `overlays.js:221`).
pub const PALETTE_LIMIT: usize = 60;

const SPLIT: &str = "layout.split";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    Commands,
    Panels,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Self::Commands => "Команды",
            Self::Panels => "Панели",
        }
    }
}

/// Категория выдачи: `Tab` перебирает все, команды, панели.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
pub enum Category {
    #[default]
    #[serde(rename = "all")]
    All,
    #[serde(rename = "cmd")]
    Commands,
    #[serde(rename = "panel")]
    Panels,
}

const CATEGORIES: [Category; 3] = [Category::All, Category::Commands, Category::Panels];

impl Category {
    /// Имя категории на проводе и в CLI: `all`, `cmd`, `panel`.
    pub fn parse(name: &str) -> Option<Self> {
        CATEGORIES
            .into_iter()
            .find(|category| category.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Commands => "cmd",
            Self::Panels => "panel",
        }
    }

    /// Подпись плашки категории.
    pub fn title(self) -> &'static str {
        match self {
            Self::All => "Всё",
            Self::Commands => Group::Commands.title(),
            Self::Panels => Group::Panels.title(),
        }
    }

    pub fn next(self) -> Self {
        self.step(1)
    }

    pub fn prev(self) -> Self {
        self.step(CATEGORIES.len() - 1)
    }

    fn step(self, by: usize) -> Self {
        let at = CATEGORIES
            .iter()
            .position(|category| *category == self)
            .unwrap_or(0);
        CATEGORIES[(at + by) % CATEGORIES.len()]
    }

    fn admits(self, group: Group) -> bool {
        match self {
            Self::All => true,
            Self::Commands => group == Group::Commands,
            Self::Panels => group == Group::Panels,
        }
    }
}

/// Ось деления зоны с фокусом: палитра показывает только строку `layout.split` этой оси.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    Row,
    Stack,
}

impl Axis {
    pub fn name(self) -> &'static str {
        match self {
            Self::Row => "row",
            Self::Stack => "stack",
        }
    }
}

/// Аргументы строки.
#[derive(Clone, Debug, PartialEq)]
pub enum Args {
    None,
    Fixed(Value),
    /// Аргументы даёт вид панели с фокусом (`resolve_args`): без них строки нет.
    View(Option<Value>),
}

/// Кандидат в строку палитры.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Устойчивый id строки: его называет чтение `ui.overlay`.
    pub id: String,
    /// Значок набора: строка показывает его, id строки его не задаёт.
    pub icon: String,
    pub title: String,
    pub detail: String,
    pub action: String,
    pub args: Args,
    /// Действие, чья привязка подписывает строку, если оно другое (прототип `S.keyFor(id, …)`):
    /// строка темы выполняет `ui.appearance.dark`, а клавиша у `ui.appearance.cycle`.
    pub key_action: Option<String>,
    /// Привычная клавиша: подпись берёт её, пока она назначена, иначе первую из назначенных.
    pub usual: Option<String>,
}

/// Кандидаты палитры, собранные приложением в момент открытия.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PaletteSource {
    /// Закреплённые команды прототипа, которые есть в продукте.
    pub pinned: Vec<Candidate>,
    /// Действия панели с фокусом.
    pub panel: Vec<Candidate>,
    /// Остальные действия.
    pub commands: Vec<Candidate>,
    /// Открытые панели рабочей области.
    pub open: Vec<Candidate>,
    /// Плагины реестра: «Открыть: <название>».
    pub plugins: Vec<Candidate>,
    /// Ось деления зоны с фокусом; без неё строк деления нет.
    pub split_axis: Option<Axis>,
    /// Контексты панели-источника сверху вниз: из них берётся клавиша строки.
    pub contexts: Vec<Context>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PaletteRow {
    pub id: String,
    pub group: Group,
    pub icon: String,
    pub title: String,
    pub detail: String,
    /// Первая последовательность действия в контекстах источника.
    pub keys: Option<String>,
    pub action: String,
    pub args: Option<Value>,
}

/// Группа выдачи: номера строк из [`Palette::rows`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewGroup {
    pub group: Group,
    pub rows: Vec<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaletteView {
    pub groups: Vec<ViewGroup>,
    /// Совпавших строк до предела.
    pub matched: usize,
}

impl PaletteView {
    /// Номера показанных строк по порядку выдачи.
    pub fn flat(&self) -> impl Iterator<Item = usize> + '_ {
        self.groups
            .iter()
            .flat_map(|group| group.rows.iter().copied())
    }

    pub fn len(&self) -> usize {
        self.groups.iter().map(|group| group.rows.len()).sum()
    }

    /// «Ничего не найдено».
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// Открытая палитра: строки, запрос, категория и выбор.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    rows: Vec<PaletteRow>,
    query: String,
    category: Category,
    selected: usize,
    view: PaletteView,
}

fn shown(candidate: &Candidate, axis: Option<Axis>) -> Option<Option<Value>> {
    if candidate.title.is_empty() {
        return None;
    }
    let args = match &candidate.args {
        Args::None => None,
        Args::Fixed(args) => Some(args.clone()),
        Args::View(args) => Some(args.clone()?),
    };
    if candidate.action == SPLIT {
        let axis = axis?;
        if args.as_ref() != Some(&json!({ "axis": axis.name() })) {
            return None;
        }
    }
    Some(args)
}

impl Palette {
    pub fn new(source: &PaletteSource, keymap: &Keymap, query: &str, category: Category) -> Self {
        let lists = [
            (Group::Commands, &source.pinned),
            (Group::Commands, &source.panel),
            (Group::Commands, &source.commands),
            (Group::Panels, &source.open),
            (Group::Panels, &source.plugins),
        ];
        let mut ids = HashSet::new();
        let mut rows = Vec::new();
        for (group, list) in lists {
            for candidate in list {
                let Some(args) = shown(candidate, source.split_axis) else {
                    continue;
                };
                if !ids.insert(candidate.id.as_str()) {
                    continue;
                }
                // Привязка действия, аргументы которого даёт вид, записана без `args`.
                let bound = match candidate.args {
                    Args::View(_) => None,
                    _ => args.as_ref(),
                };
                let key_action = candidate.key_action.as_deref().unwrap_or(&candidate.action);
                let all = keymap.sequences_for(&source.contexts, key_action, bound);
                let usual = candidate
                    .usual
                    .as_deref()
                    .and_then(|text| Sequence::parse(text, keymap.leader()).ok());
                let keys = usual
                    .as_ref()
                    .filter(|usual| all.contains(usual))
                    .or(all.first().copied())
                    .map(ToString::to_string);
                rows.push(PaletteRow {
                    id: candidate.id.clone(),
                    group,
                    icon: candidate.icon.clone(),
                    title: candidate.title.clone(),
                    detail: candidate.detail.clone(),
                    keys,
                    action: candidate.action.clone(),
                    args,
                });
            }
        }
        let mut palette = Self {
            rows,
            query: query.to_owned(),
            category,
            selected: 0,
            view: PaletteView::default(),
        };
        palette.refresh();
        palette
    }

    /// Все строки палитры до запроса и категории.
    pub fn rows(&self) -> &[PaletteRow] {
        &self.rows
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn category(&self) -> Category {
        self.category
    }

    pub fn view(&self) -> &PaletteView {
        &self.view
    }

    /// Номер выбранной строки в выдаче; при пустой выдаче выбора нет.
    pub fn selected(&self) -> Option<usize> {
        (!self.view.is_empty()).then_some(self.selected)
    }

    pub fn selected_row(&self) -> Option<&PaletteRow> {
        let index = self.view.flat().nth(self.selected()?)?;
        self.rows.get(index)
    }

    pub fn set_query(&mut self, query: &str) {
        if self.query != query {
            query.clone_into(&mut self.query);
            self.selected = 0;
            self.refresh();
        }
    }

    pub fn set_category(&mut self, category: Category) {
        self.category = category;
        self.selected = 0;
        self.refresh();
    }

    pub fn next_category(&mut self) {
        self.set_category(self.category.next());
    }

    pub fn prev_category(&mut self) {
        self.set_category(self.category.prev());
    }

    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(self.view.len().saturating_sub(1));
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Выбор строки выдачи по номеру: наведение курсора.
    pub fn select(&mut self, position: usize) {
        if position < self.view.len() {
            self.selected = position;
        }
    }

    fn refresh(&mut self) {
        let query = self.query.trim();
        let scope = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| self.category.admits(row.group));
        // Без запроса оценка — обратный порядок строк: стабильная сортировка его сохраняет.
        let scored: Vec<(usize, i64)> = if query.is_empty() {
            scope.map(|(index, _)| (index, 0)).collect()
        } else {
            scope
                .map(|(index, row)| {
                    let text = format!("{} {}", row.title, row.detail);
                    (index, i64::from(score(&text, query)))
                })
                .filter(|(_, score)| *score > 0)
                .collect()
        };
        let mut groups: Vec<(ViewGroup, i64)> = [Group::Commands, Group::Panels]
            .into_iter()
            .filter_map(|group| {
                let mut rows: Vec<(usize, i64)> = scored
                    .iter()
                    .copied()
                    .filter(|(index, _)| self.rows[*index].group == group)
                    .collect();
                rows.sort_by_key(|(_, score)| Reverse(*score));
                let best = rows.first()?.1;
                let rows = rows.into_iter().map(|(index, _)| index).collect();
                Some((ViewGroup { group, rows }, best))
            })
            .collect();
        if !query.is_empty() {
            groups.sort_by_key(|(_, best)| Reverse(*best));
        }
        let mut left = PALETTE_LIMIT;
        let groups = groups
            .into_iter()
            .filter_map(|(mut group, _)| {
                group.rows.truncate(left);
                left -= group.rows.len();
                (!group.rows.is_empty()).then_some(group)
            })
            .collect();
        self.view = PaletteView {
            groups,
            matched: scored.len(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chord::Chord;
    use crate::keymap::{Binding, Source};

    fn leader() -> Chord {
        Chord::parse("Space").unwrap()
    }

    fn keymap() -> Keymap {
        let bind = |context: Context, sequence: &str, action: &str, args: Option<Value>| Binding {
            context,
            sequence: Sequence::parse(sequence, leader()).unwrap(),
            action: action.to_owned(),
            args,
            source: Source::Default,
        };
        Keymap::build(
            1,
            leader(),
            vec![
                bind(Context::Global, "Cmd-Alt-t", "ui.appearance.cycle", None),
                bind(Context::Global, "Cmd-j", "layout.toggle.bottom", None),
                bind(Context::Navigation, "Ctrl-\\", "mode.toggle", None),
                bind(
                    Context::Navigation,
                    "Ctrl-w v",
                    SPLIT,
                    Some(json!({"axis": "row"})),
                ),
                bind(
                    Context::Navigation,
                    "Ctrl-w s",
                    SPLIT,
                    Some(json!({"axis": "stack"})),
                ),
                bind(Context::Navigation, "Ctrl-w z", "layout.zoom", None),
                bind(
                    Context::Panel("demo-list".to_owned()),
                    "o",
                    "demo-list.details",
                    None,
                ),
                bind(
                    Context::Panel("demo-native".to_owned()),
                    "s",
                    "demo-native.set",
                    None,
                ),
            ],
            Vec::new(),
        )
        .unwrap()
    }

    fn cand(id: &str, title: &str, detail: &str, action: &str) -> Candidate {
        Candidate {
            id: id.to_owned(),
            icon: "cmd".to_owned(),
            title: title.to_owned(),
            detail: detail.to_owned(),
            action: action.to_owned(),
            args: Args::None,
            key_action: None,
            usual: None,
        }
    }

    fn split(axis: &str, title: &str) -> Candidate {
        Candidate {
            args: Args::Fixed(json!({ "axis": axis })),
            ..cand(&format!("split.{axis}"), title, "панели", SPLIT)
        }
    }

    fn source() -> PaletteSource {
        PaletteSource {
            pinned: vec![
                cand(
                    "mode.toggle",
                    "Режим «Ввод»",
                    "клавиши идут в терминал",
                    "mode.toggle",
                ),
                cand(
                    "ui.appearance.cycle",
                    "Тёмная тема",
                    "оформление",
                    "ui.appearance.cycle",
                ),
                cand(
                    "layout.toggle.bottom",
                    "Скрыть нижнюю зону",
                    "Терминал",
                    "layout.toggle.bottom",
                ),
            ],
            panel: vec![cand(
                "demo-list.details",
                "Подробности строки",
                "в панели «Демо: список»",
                "demo-list.details",
            )],
            commands: vec![
                // Закреплённая команда в остальных не повторяется.
                cand("mode.toggle", "Переключить режим", "", "mode.toggle"),
                split("row", "Разделить зону: части рядом"),
                split("stack", "Разделить зону: части одна над другой"),
                cand("layout.zoom", "Развернуть панель", "панели", "layout.zoom"),
                // Действие без названия в палитру не попадает.
                cand("history.back", "", "", "history.back"),
            ],
            open: vec![
                cand("panel:1", "Демо: список", "панель · центр", "panel.select"),
                cand("panel:2", "Терминал", "панель · снизу", "panel.select"),
            ],
            plugins: vec![
                cand(
                    "open:terminal",
                    "Открыть: Терминал",
                    "новая панель",
                    "panel.open.terminal",
                ),
                cand(
                    "open:demo-native",
                    "Открыть: Демо: поле",
                    "панель",
                    "panel.open.demo-native",
                ),
            ],
            split_axis: Some(Axis::Row),
            contexts: Keymap::contexts(crate::keymap::Mode::Navigation, Some("demo-list")),
        }
    }

    fn ids(palette: &Palette) -> Vec<(Group, String)> {
        palette
            .view()
            .flat()
            .map(|index| {
                let row = &palette.rows()[index];
                (row.group, row.id.clone())
            })
            .collect()
    }

    fn names(palette: &Palette) -> Vec<String> {
        ids(palette).into_iter().map(|(_, id)| id).collect()
    }

    #[test]
    fn empty_query_shows_groups_in_source_order_with_the_first_row_selected() {
        let palette = Palette::new(&source(), &keymap(), "", Category::All);
        assert_eq!(
            names(&palette),
            [
                "mode.toggle",
                "ui.appearance.cycle",
                "layout.toggle.bottom",
                "demo-list.details",
                "split.row",
                "layout.zoom",
                "panel:1",
                "panel:2",
                "open:terminal",
                "open:demo-native",
            ]
        );
        let groups: Vec<Group> = palette.view().groups.iter().map(|g| g.group).collect();
        assert_eq!(groups, [Group::Commands, Group::Panels]);
        assert_eq!(palette.view().matched, 10);
        assert_eq!(palette.selected(), Some(0));
        assert_eq!(palette.selected_row().unwrap().id, "mode.toggle");
        assert_eq!(Group::Commands.title(), "Команды");
        assert_eq!(Group::Panels.title(), "Панели");
    }

    #[test]
    fn rows_take_their_keys_from_the_source_contexts() {
        let palette = Palette::new(&source(), &keymap(), "", Category::All);
        let key = |id: &str| {
            palette
                .rows()
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .keys
                .clone()
        };
        assert_eq!(key("ui.appearance.cycle").as_deref(), Some("Cmd-Alt-t"));
        assert_eq!(key("mode.toggle").as_deref(), Some("Ctrl-\\"));
        assert_eq!(key("split.row").as_deref(), Some("Ctrl-w v"));
        assert_eq!(key("demo-list.details").as_deref(), Some("o"));
        assert_eq!(key("panel:1"), None);
        // В `Input` контекстов `navigation` и панели нет: клавиши `Ctrl-w v` строка не получает.
        let input = PaletteSource {
            contexts: Keymap::contexts(crate::keymap::Mode::Input, Some("demo-list")),
            ..source()
        };
        let palette = Palette::new(&input, &keymap(), "", Category::All);
        let row = palette.rows().iter().find(|row| row.id == "split.row");
        assert_eq!(row.unwrap().keys, None);
    }

    #[test]
    fn rows_label_with_the_key_action_and_prefer_the_usual_key() {
        let bind = |context: Context, sequence: &str, action: &str| Binding {
            context,
            sequence: Sequence::parse(sequence, leader()).unwrap(),
            action: action.to_owned(),
            args: None,
            source: Source::Default,
        };
        let keymap = |bindings| Keymap::build(1, leader(), bindings, Vec::new()).unwrap();
        let theme = Candidate {
            key_action: Some("ui.appearance.cycle".to_owned()),
            usual: Some("Cmd-Alt-t".to_owned()),
            ..cand(
                "ui.appearance.cycle",
                "Тёмная тема",
                "оформление",
                "ui.appearance.dark",
            )
        };
        let zone = Candidate {
            usual: Some("Cmd-0".to_owned()),
            ..cand(
                "layout.toggle.navigator",
                "Скрыть навигацию",
                "зоны",
                "layout.toggle.navigator",
            )
        };
        let source = PaletteSource {
            pinned: vec![theme, zone],
            contexts: Keymap::contexts(crate::keymap::Mode::Navigation, None),
            ..PaletteSource::default()
        };
        let keys = |keymap: &Keymap| -> Vec<Option<String>> {
            Palette::new(&source, keymap, "", Category::All)
                .rows()
                .iter()
                .map(|row| row.keys.clone())
                .collect()
        };
        // Привычная клавиша назначена: подпись — она, хотя короче её нет.
        let defaults = keymap(vec![
            bind(Context::Navigation, "Space z h", "layout.toggle.navigator"),
            bind(Context::Global, "Cmd-0", "layout.toggle.navigator"),
            bind(Context::Global, "Cmd-Alt-t", "ui.appearance.cycle"),
            bind(Context::Global, "Cmd-Alt-d", "ui.appearance.dark"),
        ]);
        assert_eq!(
            keys(&defaults),
            [Some("Cmd-Alt-t".to_owned()), Some("Cmd-0".to_owned())]
        );
        // Привычная снята: первая из назначенных.
        let remapped = keymap(vec![
            bind(Context::Navigation, "Space z h", "layout.toggle.navigator"),
            bind(Context::Global, "Cmd-Alt-y", "ui.appearance.cycle"),
        ]);
        assert_eq!(
            keys(&remapped),
            [Some("Cmd-Alt-y".to_owned()), Some("Space z h".to_owned())]
        );
    }

    #[test]
    fn categories_cycle_and_narrow_to_their_group() {
        assert_eq!(Category::All.next(), Category::Commands);
        assert_eq!(Category::Commands.next(), Category::Panels);
        assert_eq!(Category::Panels.next(), Category::All);
        assert_eq!(Category::All.prev(), Category::Panels);
        assert_eq!(Category::parse("cmd"), Some(Category::Commands));
        assert_eq!(Category::parse("panel"), Some(Category::Panels));
        assert_eq!(Category::parse("all"), Some(Category::All));
        assert_eq!(Category::parse("sess"), None);
        assert_eq!(Category::Commands.title(), "Команды");
        assert_eq!(
            serde_json::to_value(Category::Panels).unwrap(),
            json!("panel")
        );

        let mut palette = Palette::new(&source(), &keymap(), "", Category::All);
        palette.select_next();
        palette.next_category();
        assert_eq!(palette.category(), Category::Commands);
        assert_eq!(palette.selected(), Some(0));
        assert!(
            ids(&palette)
                .iter()
                .all(|(group, _)| *group == Group::Commands)
        );
        palette.next_category();
        assert_eq!(
            names(&palette),
            ["panel:1", "panel:2", "open:terminal", "open:demo-native"]
        );
        palette.prev_category();
        palette.prev_category();
        assert_eq!(palette.category(), Category::All);
        let opened = Palette::new(&source(), &keymap(), "", Category::Panels);
        assert_eq!(opened.view().groups.len(), 1);
    }

    #[test]
    fn query_narrows_scores_and_orders_groups_by_their_best_row() {
        let mut palette = Palette::new(&source(), &keymap(), "", Category::All);
        palette.select_next();
        palette.set_query("терминал");
        assert_eq!(palette.selected(), Some(0));
        // «Терминал» — начало названия панели: панели идут первыми, внутри группы строки по
        // убыванию оценки (подстрока ближе к началу — выше).
        assert_eq!(
            ids(&palette),
            [
                (Group::Panels, "panel:2".to_owned()),
                (Group::Panels, "open:terminal".to_owned()),
                (Group::Commands, "layout.toggle.bottom".to_owned()),
                (Group::Commands, "mode.toggle".to_owned()),
            ]
        );
        // Пробелы вокруг запроса не учитываются, повтор того же запроса выбор не сбрасывает.
        palette.select_next();
        palette.set_query("терминал");
        assert_eq!(palette.selected(), Some(1));
        palette.set_query("  тёмная ");
        assert_eq!(names(&palette), ["ui.appearance.cycle"]);
        palette.set_query("Открыть");
        assert_eq!(names(&palette), ["open:terminal", "open:demo-native"]);
        palette.set_category(Category::Commands);
        assert!(palette.view().is_empty());
    }

    #[test]
    fn empty_result_has_no_rows_and_no_selection() {
        let mut palette = Palette::new(&source(), &keymap(), "нет такого", Category::All);
        assert!(palette.view().is_empty());
        assert_eq!(palette.view().len(), 0);
        assert_eq!(palette.view().matched, 0);
        assert_eq!(palette.selected(), None);
        assert_eq!(palette.selected_row(), None);
        palette.select_next();
        palette.select_prev();
        palette.select(0);
        assert_eq!(palette.selected(), None);
        let empty = Palette::new(&PaletteSource::default(), &keymap(), "", Category::All);
        assert!(empty.view().is_empty());
    }

    #[test]
    fn selection_moves_within_the_view() {
        let mut palette = Palette::new(&source(), &keymap(), "", Category::Panels);
        palette.select_prev();
        assert_eq!(palette.selected(), Some(0));
        for _ in 0..10 {
            palette.select_next();
        }
        assert_eq!(palette.selected(), Some(3));
        assert_eq!(palette.selected_row().unwrap().id, "open:demo-native");
        palette.select_prev();
        assert_eq!(palette.selected_row().unwrap().id, "open:terminal");
        palette.select(1);
        assert_eq!(palette.selected_row().unwrap().id, "panel:2");
        palette.select(9);
        assert_eq!(palette.selected(), Some(1));
    }

    #[test]
    fn the_limit_spans_groups_in_view_order() {
        let many = |prefix: &str, count: usize| -> Vec<Candidate> {
            (0..count)
                .map(|n| cand(&format!("{prefix}{n}"), &format!("{prefix} {n}"), "", "x"))
                .collect()
        };
        let source = PaletteSource {
            commands: many("команда", 50),
            plugins: many("панель", 30),
            ..PaletteSource::default()
        };
        let palette = Palette::new(&source, &keymap(), "", Category::All);
        assert_eq!(palette.view().len(), PALETTE_LIMIT);
        assert_eq!(palette.view().matched, 80);
        let sizes: Vec<usize> = palette.view().groups.iter().map(|g| g.rows.len()).collect();
        assert_eq!(sizes, [50, 10]);
        let filled = PaletteSource {
            commands: many("команда", 70),
            plugins: many("панель", 5),
            ..PaletteSource::default()
        };
        let palette = Palette::new(&filled, &keymap(), "", Category::All);
        // Группа, которой не досталось строк, не показывается.
        assert_eq!(palette.view().groups.len(), 1);
        assert_eq!(palette.view().len(), PALETTE_LIMIT);
        let panels = Palette::new(&filled, &keymap(), "", Category::Panels);
        assert_eq!(panels.view().len(), 5);
    }

    #[test]
    fn only_the_split_of_the_focused_zone_axis_is_shown() {
        let split_ids = |axis| {
            let source = PaletteSource {
                split_axis: axis,
                ..source()
            };
            Palette::new(&source, &keymap(), "Разделить", Category::All)
                .rows()
                .iter()
                .filter(|row| row.action == SPLIT)
                .map(|row| row.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(split_ids(Some(Axis::Row)), ["split.row"]);
        assert_eq!(split_ids(Some(Axis::Stack)), ["split.stack"]);
        assert_eq!(split_ids(None), Vec::<String>::new());
        // Деление без оси в аргументах не показывается ни при какой оси.
        let bare = PaletteSource {
            commands: vec![cand("split", "Разделить", "", SPLIT)],
            ..source()
        };
        let palette = Palette::new(&bare, &keymap(), "", Category::All);
        assert!(palette.rows().iter().all(|row| row.action != SPLIT));
    }

    #[test]
    fn rows_needing_view_args_show_only_with_them() {
        let apply = |args| Candidate {
            args: Args::View(args),
            ..cand(
                "demo-native.set",
                "Применить поле",
                "в панели «Демо: поле»",
                "demo-native.set",
            )
        };
        let without = PaletteSource {
            panel: vec![apply(None)],
            ..source()
        };
        let palette = Palette::new(&without, &keymap(), "", Category::All);
        assert!(palette.rows().iter().all(|row| row.id != "demo-native.set"));

        let with = PaletteSource {
            panel: vec![apply(Some(json!({"text": "черновик"})))],
            contexts: Keymap::contexts(crate::keymap::Mode::Navigation, Some("demo-native")),
            ..source()
        };
        let palette = Palette::new(&with, &keymap(), "Применить", Category::All);
        let row = palette.selected_row().unwrap();
        assert_eq!(row.id, "demo-native.set");
        assert_eq!(row.args, Some(json!({"text": "черновик"})));
        // Клавиша — привязка действия без `args`, аргументы строки — от вида.
        assert_eq!(row.keys.as_deref(), Some("s"));
        assert_eq!(palette.rows()[0].args, None);
    }

    #[test]
    fn rows_serialize_for_the_wire() {
        let palette = Palette::new(&source(), &keymap(), "Разделить", Category::All);
        assert_eq!(
            serde_json::to_value(palette.selected_row().unwrap()).unwrap(),
            json!({
                "id": "split.row",
                "group": "commands",
                "icon": "cmd",
                "title": "Разделить зону: части рядом",
                "detail": "панели",
                "keys": "Ctrl-w v",
                "action": "layout.split",
                "args": {"axis": "row"},
            })
        );
    }
}
