//! Чистая часть `TabStrip`: вкладки и их статусы, кнопки строки вкладок и признаки представления
//! по зоне (ADR-0024, ADR-0031, ADR-0032), разница состояний в список операций, клавиатурный выбор
//! и пункты меню «…».

use std::collections::HashSet;
use std::fmt;

use shellio_theme::KeyMode;

use crate::components::menu::{MenuEntry, MenuItem};

/// Статус вкладки: точка рядом со значком типа, значок им не заменяется (ADR-0024).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabStatus {
    /// Точки нет.
    None,
    /// Идёт работа: ровная синяя точка без анимации.
    Running,
    /// Ждёт пользователя.
    Waiting,
    Error,
    /// Работа закончилась успешно.
    Done,
    /// Процесс жив и ничего не делает: кольцо.
    Idle,
    /// Процесс завершён: черта.
    Exited,
}

impl TabStatus {
    pub const ALL: [TabStatus; 7] = [
        TabStatus::None,
        TabStatus::Running,
        TabStatus::Waiting,
        TabStatus::Error,
        TabStatus::Done,
        TabStatus::Idle,
        TabStatus::Exited,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            TabStatus::None => "none",
            TabStatus::Running => "running",
            TabStatus::Waiting => "waiting",
            TabStatus::Error => "error",
            TabStatus::Done => "done",
            TabStatus::Idle => "idle",
            TabStatus::Exited => "exited",
        }
    }

    /// Видна ли точка статуса и занимает ли она место во вкладке.
    pub const fn has_dot(self) -> bool {
        match self {
            TabStatus::None => false,
            TabStatus::Running
            | TabStatus::Waiting
            | TabStatus::Error
            | TabStatus::Done
            | TabStatus::Idle
            | TabStatus::Exited => true,
        }
    }
}

/// Вкладка: панель в строке вкладок.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    /// Устойчивый id: разница и выбор идут по нему.
    pub id: String,
    pub title: String,
    /// Тип панели, например `terminal`.
    pub kind: String,
    /// Имя системного значка типа.
    pub icon: String,
    pub status: TabStatus,
}

impl Tab {
    pub fn new(id: &str, title: &str, kind: &str, icon: &str, status: TabStatus) -> Tab {
        Tab {
            id: id.to_owned(),
            title: title.to_owned(),
            kind: kind.to_owned(),
            icon: icon.to_owned(),
            status,
        }
    }
}

/// Сторона зоны, в которой стоит полоса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZoneSide {
    /// Левая или правая зона.
    Side,
    Center,
    Bottom,
}

impl ZoneSide {
    pub const ALL: [ZoneSide; 3] = [ZoneSide::Side, ZoneSide::Center, ZoneSide::Bottom];

    pub const fn name(self) -> &'static str {
        match self {
            ZoneSide::Side => "side",
            ZoneSide::Center => "center",
            ZoneSide::Bottom => "bottom",
        }
    }
}

/// Ось деления зоны (ADR-0031): части рядом или одна над другой.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SplitAxis {
    /// Части рядом: центр и нижняя зона.
    Columns,
    /// Части одна над другой: левая и правая зона.
    Rows,
}

impl SplitAxis {
    pub const fn name(self) -> &'static str {
        match self {
            SplitAxis::Columns => "columns",
            SplitAxis::Rows => "rows",
        }
    }
}

/// Признаки представления полосы: где она стоит и что с панелью.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Presentation {
    pub zone: ZoneSide,
    /// Часть с навигатором: он не делится (ADR-0032).
    pub navigator: bool,
    /// Панель развёрнута (`zoom`).
    pub zoomed: bool,
}

impl Presentation {
    pub const fn new(zone: ZoneSide) -> Presentation {
        Presentation {
            zone,
            navigator: false,
            zoomed: false,
        }
    }

    /// Ось деления зоны: одна у зоны, выбирать нечего (ADR-0031).
    pub const fn axis(self) -> SplitAxis {
        match self.zone {
            ZoneSide::Side => SplitAxis::Rows,
            ZoneSide::Center | ZoneSide::Bottom => SplitAxis::Columns,
        }
    }

    /// В боковых зонах значка типа нет (ADR-0024).
    pub const fn shows_icons(self) -> bool {
        match self.zone {
            ZoneSide::Side => false,
            ZoneSide::Center | ZoneSide::Bottom => true,
        }
    }

    /// Место под кнопку закрытия во вкладке. В боковой зоне кнопка ложится поверх конца вкладки
    /// только при наведении и места не занимает (прототип, `.panel[data-zone]`).
    pub const fn reserves_close(self) -> bool {
        match self.zone {
            ZoneSide::Side => false,
            ZoneSide::Center | ZoneSide::Bottom => true,
        }
    }

    /// Кнопки справа по порядку (ADR-0032): в развёрнутой панели «Вернуть раскладку» вместо
    /// «Развернуть» и без деления, у навигатора деления нет, меню «…» есть всегда.
    pub fn buttons(self) -> Vec<StripButton> {
        if self.zoomed {
            vec![StripButton::Restore, StripButton::Menu]
        } else if self.navigator {
            vec![StripButton::Zoom, StripButton::Menu]
        } else {
            vec![
                StripButton::Split(self.axis()),
                StripButton::Zoom,
                StripButton::Menu,
            ]
        }
    }
}

/// Кнопка строки вкладок.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StripButton {
    /// «Разделить» по оси зоны.
    Split(SplitAxis),
    /// «Развернуть панель».
    Zoom,
    /// «Вернуть раскладку» в развёрнутой панели.
    Restore,
    /// Меню «…».
    Menu,
}

/// Подсказка кнопки без назначенной клавиши (ADR-0032).
pub const NO_KEY: &str = "Клавиша не назначена";

impl StripButton {
    /// Часть id узла `ui.inspect` после id полосы.
    pub const fn id(self) -> &'static str {
        match self {
            StripButton::Split(_) => "split",
            StripButton::Zoom => "zoom",
            StripButton::Restore => "restore",
            StripButton::Menu => "menu",
        }
    }

    /// Название действия: AX-имя и начало подсказки.
    pub const fn label(self) -> &'static str {
        match self {
            StripButton::Split(SplitAxis::Columns) => "Разделить: части рядом",
            StripButton::Split(SplitAxis::Rows) => "Разделить: одна над другой",
            StripButton::Zoom => "Развернуть панель",
            StripButton::Restore => "Вернуть раскладку",
            StripButton::Menu => "Действия панели",
        }
    }

    /// Системный значок: глиф деления показывает ось зоны.
    pub const fn symbol(self) -> &'static str {
        match self {
            StripButton::Split(SplitAxis::Columns) => "rectangle.split.2x1",
            StripButton::Split(SplitAxis::Rows) => "rectangle.split.1x2",
            StripButton::Zoom => "arrow.up.left.and.arrow.down.right",
            StripButton::Restore => "arrow.down.right.and.arrow.up.left",
            StripButton::Menu => "ellipsis",
        }
    }

    /// Клавиша кнопки из состояния полосы.
    pub fn key(self, keys: &ButtonKeys) -> Option<&str> {
        match self {
            StripButton::Split(_) => keys.split.as_deref(),
            StripButton::Zoom | StripButton::Restore => keys.zoom.as_deref(),
            StripButton::Menu => keys.menu.as_deref(),
        }
    }
}

/// Подсказка `toolTip`: действие и клавиша через два пробела, без клавиши на её месте
/// «Клавиша не назначена» (ADR-0032).
pub fn hint(button: StripButton, key: Option<&str>) -> String {
    format!("{}  {}", button.label(), key.unwrap_or(NO_KEY))
}

/// `accessibilityHelp` кнопки: клавиша, без неё — «Клавиша не назначена».
pub fn help(key: Option<&str>) -> String {
    key.unwrap_or(NO_KEY).to_owned()
}

/// Клавиши кнопок из действующего keymap; до P1.6 их нет.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ButtonKeys {
    pub split: Option<String>,
    pub zoom: Option<String>,
    pub menu: Option<String>,
}

/// Состояние полосы вкладок.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabStripState {
    /// Имя полосы для доступности.
    pub label: String,
    pub tabs: Vec<Tab>,
    /// Id выбранной вкладки; `None` — выбора нет.
    pub selected: Option<String>,
    /// Нагрузка активной панели, например память и CPU; `None` — её нет.
    pub payload: Option<String>,
    pub presentation: Presentation,
    /// Фокус у части: тон и линия под активной вкладкой.
    pub focused: bool,
    /// Режим клавиатуры: цвет линии под вкладкой.
    pub mode: KeyMode,
    pub keys: ButtonKeys,
    /// Действия панели в меню «…» под заголовком с нагрузкой.
    pub actions: Vec<MenuEntry>,
}

impl TabStripState {
    pub fn new(label: &str, presentation: Presentation) -> TabStripState {
        TabStripState {
            label: label.to_owned(),
            tabs: Vec::new(),
            selected: None,
            payload: None,
            presentation,
            focused: false,
            mode: KeyMode::Navigation,
            keys: ButtonKeys::default(),
            actions: Vec::new(),
        }
    }

    /// Номер выбранной вкладки.
    pub fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_deref()?;
        self.tabs.iter().position(|tab| tab.id == selected)
    }
}

/// Ошибка состояния полосы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabError {
    /// Id вкладки встречается больше одного раза: выбор и разница по нему неоднозначны.
    DuplicateId(String),
    /// Выбрана вкладка, которой нет в полосе.
    UnknownSelection(String),
}

impl fmt::Display for TabError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TabError::DuplicateId(id) => write!(f, "id вкладки повторяется: {id}"),
            TabError::UnknownSelection(id) => write!(f, "выбрана вкладка, которой нет: {id}"),
        }
    }
}

impl std::error::Error for TabError {}

/// Id вкладок уникальны, выбранная вкладка есть в полосе.
pub fn check(state: &TabStripState) -> Result<(), TabError> {
    let mut seen = HashSet::new();
    for tab in &state.tabs {
        if !seen.insert(tab.id.as_str()) {
            return Err(TabError::DuplicateId(tab.id.clone()));
        }
    }
    match &state.selected {
        Some(id) if !seen.contains(id.as_str()) => Err(TabError::UnknownSelection(id.clone())),
        _ => Ok(()),
    }
}

/// Изменение вкладки, которая осталась в полосе.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabChange {
    Title(String),
    Kind { kind: String, icon: String },
    Status(TabStatus),
}

/// Операция над вкладками. Номера — в порядке вкладок после предыдущих операций.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabOp {
    Remove { index: usize },
    Insert { index: usize, tab: Tab },
    Move { from: usize, to: usize },
    Update { index: usize, change: TabChange },
}

/// Операции, которые переводят вкладки из `before` в `after`. Вкладка узнаётся по id: удалённые
/// уходят, новые вставляются, переставленные переносятся той же вкладкой, у оставшихся меняется
/// только изменённое. Id в каждом наборе уникальны ([`check`]).
pub fn diff_tabs(before: &[Tab], after: &[Tab]) -> Vec<TabOp> {
    let mut ops = Vec::new();
    let kept: HashSet<&str> = after.iter().map(|tab| tab.id.as_str()).collect();
    let mut current: Vec<&Tab> = before.iter().collect();
    for index in (0..current.len()).rev() {
        if !kept.contains(current[index].id.as_str()) {
            ops.push(TabOp::Remove { index });
            current.remove(index);
        }
    }
    for (index, next) in after.iter().enumerate() {
        let found = current[index..]
            .iter()
            .position(|tab| tab.id == next.id)
            .map(|offset| index + offset);
        match found {
            Some(from) => {
                if from != index {
                    ops.push(TabOp::Move { from, to: index });
                    let tab = current.remove(from);
                    current.insert(index, tab);
                }
                let tab = current[index];
                if tab.title != next.title {
                    ops.push(TabOp::Update {
                        index,
                        change: TabChange::Title(next.title.clone()),
                    });
                }
                if tab.kind != next.kind || tab.icon != next.icon {
                    ops.push(TabOp::Update {
                        index,
                        change: TabChange::Kind {
                            kind: next.kind.clone(),
                            icon: next.icon.clone(),
                        },
                    });
                }
                if tab.status != next.status {
                    ops.push(TabOp::Update {
                        index,
                        change: TabChange::Status(next.status),
                    });
                }
            }
            None => {
                ops.push(TabOp::Insert {
                    index,
                    tab: next.clone(),
                });
                current.insert(index, next);
            }
        }
    }
    ops
}

/// Операция над полосой: вкладки разницей, остальное — новым значением при смене.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StripOp {
    Label(String),
    Tabs(TabOp),
    Select(Option<String>),
    Payload(Option<String>),
    Presentation(Presentation),
    Focus(bool),
    Mode(KeyMode),
    Keys(ButtonKeys),
    Actions(Vec<MenuEntry>),
}

/// Операции, которые переводят полосу из `before` в `after`.
pub fn diff(before: &TabStripState, after: &TabStripState) -> Vec<StripOp> {
    let mut ops = Vec::new();
    if before.label != after.label {
        ops.push(StripOp::Label(after.label.clone()));
    }
    ops.extend(
        diff_tabs(&before.tabs, &after.tabs)
            .into_iter()
            .map(StripOp::Tabs),
    );
    if before.selected != after.selected {
        ops.push(StripOp::Select(after.selected.clone()));
    }
    if before.payload != after.payload {
        ops.push(StripOp::Payload(after.payload.clone()));
    }
    if before.presentation != after.presentation {
        ops.push(StripOp::Presentation(after.presentation));
    }
    if before.focused != after.focused {
        ops.push(StripOp::Focus(after.focused));
    }
    if before.mode != after.mode {
        ops.push(StripOp::Mode(after.mode));
    }
    if before.keys != after.keys {
        ops.push(StripOp::Keys(after.keys.clone()));
    }
    if before.actions != after.actions {
        ops.push(StripOp::Actions(after.actions.clone()));
    }
    ops
}

/// Намерение полосы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabIntent {
    /// Выбрать вкладку: щелчок, `AXPress`, стрелки при фокусе в полосе, `Space` и `Enter`.
    Select { id: String },
    /// Закрыть вкладку: кнопка закрытия.
    Close { id: String },
    /// «Разделить» по оси зоны.
    Split(SplitAxis),
    /// «Развернуть панель».
    Zoom,
    /// «Вернуть раскладку».
    Restore,
    /// Выбран пункт меню «…».
    Menu { id: String },
}

/// Клавиша полосы, которую она обрабатывает сама при фокусе в ней.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripKey {
    Left,
    Right,
    /// `Space` и `Enter`: подтвердить выбранную вкладку.
    Confirm,
}

/// Намерение клавиши: стрелки выбирают соседнюю вкладку без перехода через край, подтверждение
/// повторяет выбор текущей. `None` — клавиша ничего не делает.
pub fn key_intent(state: &TabStripState, key: StripKey) -> Option<TabIntent> {
    let index = state.selected_index();
    let target = match (key, index) {
        (StripKey::Confirm, Some(index)) => Some(index),
        (StripKey::Left, Some(index)) => index.checked_sub(1),
        (StripKey::Right, Some(index)) => Some(index + 1).filter(|next| *next < state.tabs.len()),
        (StripKey::Left | StripKey::Right, None) => (!state.tabs.is_empty()).then_some(0),
        (StripKey::Confirm, None) => None,
    }?;
    state
        .tabs
        .get(target)
        .map(|tab| TabIntent::Select { id: tab.id.clone() })
}

/// Id заголовка меню «…» с нагрузкой и разделителя под ним.
pub const PAYLOAD_ITEM: &str = "tabs.payload";
pub const PAYLOAD_SEPARATOR: &str = "tabs.payload.separator";

/// Пункты меню «…»: нагрузка, которая не поместилась в строку, целиком в заголовке меню, под ней
/// действия панели (ADR-0032). Заголовок — недоступный пункт: выбрать его нельзя.
pub fn menu_entries(state: &TabStripState, payload_hidden: bool) -> Vec<MenuEntry> {
    let mut entries = Vec::new();
    if let (true, Some(payload)) = (payload_hidden, &state.payload) {
        entries.push(MenuEntry::Item(MenuItem {
            enabled: false,
            ..MenuItem::new(PAYLOAD_ITEM, payload)
        }));
        if !state.actions.is_empty() {
            entries.push(MenuEntry::Separator {
                id: PAYLOAD_SEPARATOR.to_owned(),
            });
        }
    }
    entries.extend(state.actions.iter().cloned());
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(id: &str) -> Tab {
        Tab::new(
            id,
            &format!("Вкладка {id}"),
            "terminal",
            "terminal",
            TabStatus::None,
        )
    }

    fn ids(tabs: &[Tab]) -> Vec<&str> {
        tabs.iter().map(|tab| tab.id.as_str()).collect()
    }

    /// Оракул: операции, применённые к `before` по порядку.
    fn apply(before: &[Tab], ops: &[TabOp]) -> Vec<Tab> {
        let mut tabs = before.to_vec();
        for op in ops {
            match op {
                TabOp::Remove { index } => {
                    tabs.remove(*index);
                }
                TabOp::Insert { index, tab } => tabs.insert(*index, tab.clone()),
                TabOp::Move { from, to } => {
                    let tab = tabs.remove(*from);
                    tabs.insert(*to, tab);
                }
                TabOp::Update { index, change } => match change {
                    TabChange::Title(title) => tabs[*index].title.clone_from(title),
                    TabChange::Kind { kind, icon } => {
                        tabs[*index].kind.clone_from(kind);
                        tabs[*index].icon.clone_from(icon);
                    }
                    TabChange::Status(status) => tabs[*index].status = *status,
                },
            }
        }
        tabs
    }

    fn assert_diff(before: &[Tab], after: &[Tab]) -> Vec<TabOp> {
        let ops = diff_tabs(before, after);
        assert_eq!(apply(before, &ops), after, "операции {ops:?}");
        ops
    }

    #[test]
    fn same_tabs_give_no_operations() {
        let tabs = vec![tab("a"), tab("b")];
        assert!(diff_tabs(&tabs, &tabs).is_empty());
        assert!(diff_tabs(&[], &[]).is_empty());
    }

    #[test]
    fn insert_in_the_middle_keeps_the_others() {
        let ops = assert_diff(&[tab("a"), tab("c")], &[tab("a"), tab("b"), tab("c")]);
        assert_eq!(
            ops,
            [TabOp::Insert {
                index: 1,
                tab: tab("b"),
            }]
        );
    }

    #[test]
    fn removal_takes_only_the_removed_tab() {
        let ops = assert_diff(&[tab("a"), tab("b"), tab("c")], &[tab("a"), tab("c")]);
        assert_eq!(ops, [TabOp::Remove { index: 1 }]);
        let ops = assert_diff(&[tab("a"), tab("b")], &[]);
        assert_eq!(ops.len(), 2, "только удаления: {ops:?}");
    }

    #[test]
    fn reorder_moves_the_same_tab_without_recreating_it() {
        let ops = assert_diff(
            &[tab("a"), tab("b"), tab("c"), tab("d")],
            &[tab("d"), tab("a"), tab("b"), tab("c")],
        );
        assert_eq!(ops, [TabOp::Move { from: 3, to: 0 }]);
        assert!(
            ops.iter()
                .all(|op| !matches!(op, TabOp::Insert { .. } | TabOp::Remove { .. })),
            "перестановка не пересоздаёт вкладки: {ops:?}"
        );
    }

    #[test]
    fn status_and_title_changes_are_updates_in_place() {
        let mut after = vec![tab("a"), tab("b")];
        after[1].status = TabStatus::Running;
        let ops = assert_diff(&[tab("a"), tab("b")], &after);
        assert_eq!(
            ops,
            [TabOp::Update {
                index: 1,
                change: TabChange::Status(TabStatus::Running),
            }]
        );
        after[0].title = "Сборка".into();
        after[0].icon = "hammer".into();
        let ops = assert_diff(&[tab("a"), tab("b")], &after);
        assert_eq!(ops.len(), 3, "{ops:?}");
    }

    #[test]
    fn random_pairs_are_reproduced_by_the_operations() {
        // SplitMix64: воспроизводимые пары наборов вкладок.
        let mut seed = 0x5EED_0011_u64;
        let mut next = move || {
            seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        let mut pick = |n: u64| usize::try_from(next() % n).unwrap_or(0);
        for _ in 0..500 {
            let make = |pick: &mut dyn FnMut(u64) -> usize| {
                let mut pool: Vec<Tab> = (0..12).map(|i| tab(&i.to_string())).collect();
                let len = pick(10);
                let mut tabs = Vec::new();
                for _ in 0..len {
                    let at = pick(pool.len() as u64);
                    let mut chosen = pool.remove(at);
                    chosen.status = TabStatus::ALL[pick(TabStatus::ALL.len() as u64)];
                    if pick(4) == 0 {
                        chosen.title = format!("Заголовок {}", pick(3));
                    }
                    tabs.push(chosen);
                }
                tabs
            };
            let before = make(&mut pick);
            let after = make(&mut pick);
            assert_diff(&before, &after);
        }
    }

    #[test]
    fn state_diff_names_only_what_changed() {
        let mut before = TabStripState::new("Вкладки", Presentation::new(ZoneSide::Center));
        before.tabs = vec![tab("a"), tab("b")];
        before.selected = Some("a".into());
        assert!(diff(&before, &before).is_empty());
        let mut after = before.clone();
        after.selected = Some("b".into());
        after.focused = true;
        after.payload = Some("1,2 ГБ".into());
        assert_eq!(
            diff(&before, &after),
            [
                StripOp::Select(Some("b".into())),
                StripOp::Payload(Some("1,2 ГБ".into())),
                StripOp::Focus(true),
            ]
        );
        let mut moved = before.clone();
        moved.presentation = Presentation::new(ZoneSide::Side);
        moved.tabs.swap(0, 1);
        assert_eq!(
            diff(&before, &moved),
            [
                StripOp::Tabs(TabOp::Move { from: 1, to: 0 }),
                StripOp::Presentation(Presentation::new(ZoneSide::Side)),
            ]
        );
    }

    #[test]
    fn check_rejects_duplicate_ids_and_unknown_selection() {
        let mut state = TabStripState::new("Вкладки", Presentation::new(ZoneSide::Center));
        state.tabs = vec![tab("a"), tab("b")];
        state.selected = Some("b".into());
        assert_eq!(check(&state), Ok(()));
        state.selected = Some("z".into());
        assert_eq!(check(&state), Err(TabError::UnknownSelection("z".into())));
        state.selected = None;
        state.tabs.push(tab("a"));
        assert_eq!(check(&state), Err(TabError::DuplicateId("a".into())));
    }

    #[test]
    fn presentation_follows_the_zone() {
        let side = Presentation::new(ZoneSide::Side);
        let center = Presentation::new(ZoneSide::Center);
        let bottom = Presentation::new(ZoneSide::Bottom);
        assert!(
            !side.shows_icons() && !side.reserves_close(),
            "в боковой зоне значка нет"
        );
        assert!(center.shows_icons() && bottom.shows_icons());
        assert_eq!(side.axis(), SplitAxis::Rows);
        assert_eq!(center.axis(), SplitAxis::Columns);
        assert_eq!(bottom.axis(), SplitAxis::Columns);
        assert_eq!(
            side.buttons(),
            [
                StripButton::Split(SplitAxis::Rows),
                StripButton::Zoom,
                StripButton::Menu
            ]
        );
        assert_eq!(
            center.buttons(),
            [
                StripButton::Split(SplitAxis::Columns),
                StripButton::Zoom,
                StripButton::Menu
            ]
        );
        assert_ne!(
            StripButton::Split(SplitAxis::Rows).symbol(),
            StripButton::Split(SplitAxis::Columns).symbol(),
            "глиф показывает ось"
        );
        let navigator = Presentation {
            navigator: true,
            ..side
        };
        assert_eq!(
            navigator.buttons(),
            [StripButton::Zoom, StripButton::Menu],
            "навигатор не делится"
        );
        for zone in ZoneSide::ALL {
            let zoomed = Presentation {
                zoomed: true,
                ..Presentation::new(zone)
            };
            assert_eq!(zoomed.buttons(), [StripButton::Restore, StripButton::Menu]);
        }
    }

    #[test]
    fn hints_name_the_action_and_the_key() {
        let split = StripButton::Split(SplitAxis::Columns);
        assert_eq!(
            hint(split, Some("Ctrl-w v")),
            "Разделить: части рядом  Ctrl-w v"
        );
        assert_eq!(
            hint(StripButton::Zoom, None),
            "Развернуть панель  Клавиша не назначена"
        );
        assert_eq!(help(None), NO_KEY);
        assert_eq!(help(Some("Ctrl-w z")), "Ctrl-w z");
        let keys = ButtonKeys {
            zoom: Some("Ctrl-w z".into()),
            ..ButtonKeys::default()
        };
        assert_eq!(StripButton::Restore.key(&keys), Some("Ctrl-w z"));
        assert_eq!(split.key(&keys), None);
    }

    #[test]
    fn arrows_select_neighbours_without_wrapping() {
        let mut state = TabStripState::new("Вкладки", Presentation::new(ZoneSide::Center));
        state.tabs = vec![tab("a"), tab("b"), tab("c")];
        let select = |id: &str| Some(TabIntent::Select { id: id.into() });
        assert_eq!(key_intent(&state, StripKey::Right), select("a"));
        assert_eq!(key_intent(&state, StripKey::Confirm), None);
        state.selected = Some("b".into());
        assert_eq!(key_intent(&state, StripKey::Left), select("a"));
        assert_eq!(key_intent(&state, StripKey::Right), select("c"));
        assert_eq!(key_intent(&state, StripKey::Confirm), select("b"));
        state.selected = Some("c".into());
        assert_eq!(key_intent(&state, StripKey::Right), None, "за край не идёт");
        state.selected = Some("a".into());
        assert_eq!(key_intent(&state, StripKey::Left), None);
        state.tabs.clear();
        state.selected = None;
        assert_eq!(key_intent(&state, StripKey::Right), None);
    }

    #[test]
    fn hidden_payload_heads_the_menu_whole() {
        let mut state = TabStripState::new("Вкладки", Presentation::new(ZoneSide::Center));
        state.actions = vec![MenuEntry::Item(MenuItem::new("close", "Закрыть"))];
        state.payload = Some("1,24 ГБ · 3,5 % CPU".into());
        let entries = menu_entries(&state, true);
        let MenuEntry::Item(head) = &entries[0] else {
            panic!("первый пункт — разделитель: {entries:?}");
        };
        assert_eq!(head.title, "1,24 ГБ · 3,5 % CPU", "нагрузка целиком");
        assert!(!head.enabled, "заголовок не выбирается");
        assert_eq!(entries[1].id(), PAYLOAD_SEPARATOR);
        assert_eq!(entries[2].id(), "close");
        assert_eq!(
            menu_entries(&state, false),
            state.actions,
            "видимая нагрузка в меню не дублируется"
        );
        state.payload = None;
        assert_eq!(menu_entries(&state, true), state.actions);
        assert_eq!(ids(&[tab("x")]), ["x"]);
    }
}
