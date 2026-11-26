//! Действия интерфейса, которые не меняют состояния ядра (ADR-0007, «Уточнено в P1.3»).
//!
//! Чистый тип без обработчика: обработчик один, на главном потоке владельца. Меню, CLI, а с P1.6
//! клавиша и палитра вызывают его. Актор ядра эти действия не видит, поэтому они не входят в
//! [`crate::Command`].

use serde::{Deserialize, Serialize};

/// Выбор оформления: светлота или следование системе. В JSON — значение [`Self::as_str`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppearanceChoice {
    Light,
    Dark,
    System,
}

impl AppearanceChoice {
    pub const ALL: [Self; 3] = [Self::Light, Self::Dark, Self::System];

    /// Значение аргумента `ui.appearance.set` и `shellioctl ui appearance`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }

    /// Выбор по значению аргумента.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|choice| choice.as_str() == value)
    }

    /// Следующий выбор кнопки оформления: система → светлое → тёмное → система.
    pub const fn next(self) -> Self {
        match self {
            Self::System => Self::Light,
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
        }
    }
}

/// Цель действия `layout.toggle.*`: что показать или скрыть в открытой рабочей области.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToggleTarget {
    Navigator,
    Bottom,
    /// Боковая зона третьей кнопки заголовка: плитка левой зоны или правая зона, цель выбирается
    /// в момент вызова.
    Side,
    Plate,
    Right,
}

impl ToggleTarget {
    pub const ALL: [Self; 5] = [
        Self::Navigator,
        Self::Bottom,
        Self::Side,
        Self::Plate,
        Self::Right,
    ];

    /// Операнд `shellioctl layout toggle` и последний сегмент имени действия.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Navigator => "navigator",
            Self::Bottom => "bottom",
            Self::Side => "side",
            Self::Plate => "plate",
            Self::Right => "right",
        }
    }

    /// Цель по операнду.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|target| target.as_str() == value)
    }
}

/// Движение списка панели с фокусом: действие `list.*` (ADR-0010, «Клавиши по умолчанию»).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListAction {
    Down,
    Up,
    Top,
    Bottom,
    PageDown,
    PageUp,
    Collapse,
    Expand,
    Activate,
}

impl ListAction {
    pub const ALL: [Self; 9] = [
        Self::Down,
        Self::Up,
        Self::Top,
        Self::Bottom,
        Self::PageDown,
        Self::PageUp,
        Self::Collapse,
        Self::Expand,
        Self::Activate,
    ];

    /// Операнд `shellioctl ui list`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
            Self::Top => "top",
            Self::Bottom => "bottom",
            Self::PageDown => "page-down",
            Self::PageUp => "page-up",
            Self::Collapse => "collapse",
            Self::Expand => "expand",
            Self::Activate => "activate",
        }
    }

    /// Движение по операнду.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == value)
    }

    /// Имя действия в keymap и на проводе.
    pub const fn keymap_name(self) -> &'static str {
        match self {
            Self::Down => "list.down",
            Self::Up => "list.up",
            Self::Top => "list.top",
            Self::Bottom => "list.bottom",
            Self::PageDown => "list.page_down",
            Self::PageUp => "list.page_up",
            Self::Collapse => "list.collapse",
            Self::Expand => "list.expand",
            Self::Activate => "list.activate",
        }
    }
}

/// Действие режима панели с фокусом: `mode.*` (ADR-0010, «Уточнено в P1.6 (планирование)»).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModeAction {
    Navigation,
    Input,
    Toggle,
}

impl ModeAction {
    pub const ALL: [Self; 3] = [Self::Navigation, Self::Input, Self::Toggle];

    /// Операнд `shellioctl ui mode` и последний сегмент имени действия.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Navigation => "navigation",
            Self::Input => "input",
            Self::Toggle => "toggle",
        }
    }

    /// Действие по операнду.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == value)
    }

    /// Имя действия в keymap и на проводе.
    pub const fn keymap_name(self) -> &'static str {
        match self {
            Self::Navigation => "mode.navigation",
            Self::Input => "mode.input",
            Self::Toggle => "mode.toggle",
        }
    }
}

/// Шаг истории переходов: `history.*` (ADR-0010, «История переходов»).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HistoryStep {
    Back,
    Forward,
}

impl HistoryStep {
    pub const ALL: [Self; 2] = [Self::Back, Self::Forward];

    /// Операнд `shellioctl ui history` и последний сегмент имени действия.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Back => "back",
            Self::Forward => "forward",
        }
    }

    /// Шаг по операнду.
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|step| step.as_str() == value)
    }

    /// Имя действия в keymap и на проводе.
    pub const fn keymap_name(self) -> &'static str {
        match self {
            Self::Back => "history.back",
            Self::Forward => "history.forward",
        }
    }
}

/// Действие интерфейса.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UiCommand {
    /// Спрятать окно владельца: процесс, сокет и ядро продолжают жить.
    WindowClose,
    /// Завершить владельца путём `applicationShouldTerminate`.
    AppQuit,
    /// Выбрать оформление до выхода процесса.
    AppearanceSet(AppearanceChoice),
    /// Следующее оформление по кругу: система → светлое → тёмное.
    AppearanceCycle,
    /// Показать или скрыть цель в открытой рабочей области: видимость читается в момент вызова,
    /// затем идёт команда ядра `layout.visibility`.
    LayoutToggle(ToggleTarget),
    /// Движение списка панели с фокусом: намерение её виду.
    List(ListAction),
    /// Открыть строку фильтра панели с фокусом. Текст фильтра действие не несёт: его задаёт
    /// запрос `panel.filter` с полем `text`.
    PanelFilter,
    /// Режим панели с фокусом. `Input` есть только у панели с получателем ввода.
    Mode(ModeAction),
    /// Шаг по истории переходов активного воркспейса.
    History(HistoryStep),
}

impl UiCommand {
    /// Все действия: по одному на каждое значение выбора оформления, каждую цель скрытия, каждое
    /// движение списка, каждое действие режима и каждый шаг истории.
    pub const ALL: [Self; 26] = [
        Self::WindowClose,
        Self::AppQuit,
        Self::AppearanceSet(AppearanceChoice::Light),
        Self::AppearanceSet(AppearanceChoice::Dark),
        Self::AppearanceSet(AppearanceChoice::System),
        Self::AppearanceCycle,
        Self::LayoutToggle(ToggleTarget::Navigator),
        Self::LayoutToggle(ToggleTarget::Bottom),
        Self::LayoutToggle(ToggleTarget::Side),
        Self::LayoutToggle(ToggleTarget::Plate),
        Self::LayoutToggle(ToggleTarget::Right),
        Self::List(ListAction::Down),
        Self::List(ListAction::Up),
        Self::List(ListAction::Top),
        Self::List(ListAction::Bottom),
        Self::List(ListAction::PageDown),
        Self::List(ListAction::PageUp),
        Self::List(ListAction::Collapse),
        Self::List(ListAction::Expand),
        Self::List(ListAction::Activate),
        Self::PanelFilter,
        Self::Mode(ModeAction::Navigation),
        Self::Mode(ModeAction::Input),
        Self::Mode(ModeAction::Toggle),
        Self::History(HistoryStep::Back),
        Self::History(HistoryStep::Forward),
    ];

    /// Имя команды в управляющем API.
    pub const fn name(self) -> &'static str {
        match self {
            Self::WindowClose => "window.close",
            Self::AppQuit => "app.quit",
            Self::AppearanceSet(_) => "ui.appearance.set",
            Self::AppearanceCycle
            | Self::LayoutToggle(_)
            | Self::List(_)
            | Self::PanelFilter
            | Self::Mode(_)
            | Self::History(_) => self.keymap_name(),
        }
    }

    /// Имя действия в keymap: у выбора оформления своё имя на каждое значение.
    pub const fn keymap_name(self) -> &'static str {
        match self {
            Self::WindowClose => "window.close",
            Self::AppQuit => "app.quit",
            Self::AppearanceSet(AppearanceChoice::Light) => "ui.appearance.light",
            Self::AppearanceSet(AppearanceChoice::Dark) => "ui.appearance.dark",
            Self::AppearanceSet(AppearanceChoice::System) => "ui.appearance.system",
            Self::AppearanceCycle => "ui.appearance.cycle",
            Self::LayoutToggle(ToggleTarget::Navigator) => "layout.toggle.navigator",
            Self::LayoutToggle(ToggleTarget::Bottom) => "layout.toggle.bottom",
            Self::LayoutToggle(ToggleTarget::Side) => "layout.toggle.side",
            Self::LayoutToggle(ToggleTarget::Plate) => "layout.toggle.plate",
            Self::LayoutToggle(ToggleTarget::Right) => "layout.toggle.right",
            Self::List(action) => action.keymap_name(),
            Self::PanelFilter => "panel.filter",
            Self::Mode(action) => action.keymap_name(),
            Self::History(step) => step.keymap_name(),
        }
    }

    /// Действие по имени в keymap.
    pub fn from_keymap_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|command| command.keymap_name() == name)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn command_and_keymap_names_are_stable() {
        let expected = [
            ("window.close", "window.close"),
            ("app.quit", "app.quit"),
            ("ui.appearance.set", "ui.appearance.light"),
            ("ui.appearance.set", "ui.appearance.dark"),
            ("ui.appearance.set", "ui.appearance.system"),
            ("ui.appearance.cycle", "ui.appearance.cycle"),
            ("layout.toggle.navigator", "layout.toggle.navigator"),
            ("layout.toggle.bottom", "layout.toggle.bottom"),
            ("layout.toggle.side", "layout.toggle.side"),
            ("layout.toggle.plate", "layout.toggle.plate"),
            ("layout.toggle.right", "layout.toggle.right"),
            ("list.down", "list.down"),
            ("list.up", "list.up"),
            ("list.top", "list.top"),
            ("list.bottom", "list.bottom"),
            ("list.page_down", "list.page_down"),
            ("list.page_up", "list.page_up"),
            ("list.collapse", "list.collapse"),
            ("list.expand", "list.expand"),
            ("list.activate", "list.activate"),
            ("panel.filter", "panel.filter"),
            ("mode.navigation", "mode.navigation"),
            ("mode.input", "mode.input"),
            ("mode.toggle", "mode.toggle"),
            ("history.back", "history.back"),
            ("history.forward", "history.forward"),
        ];
        assert_eq!(UiCommand::ALL.len(), expected.len());
        for (command, (name, keymap)) in UiCommand::ALL.into_iter().zip(expected) {
            assert_eq!(command.name(), name, "{command:?}");
            assert_eq!(command.keymap_name(), keymap, "{command:?}");
            assert_eq!(UiCommand::from_keymap_name(keymap), Some(command));
        }
        let unique: BTreeSet<_> = UiCommand::ALL.map(UiCommand::keymap_name).into();
        assert_eq!(unique.len(), UiCommand::ALL.len(), "имена keymap различны");
        assert_eq!(UiCommand::from_keymap_name("ui.appearance.set"), None);
    }

    #[test]
    fn list_actions_parse_their_operands() {
        let operands = [
            "down",
            "up",
            "top",
            "bottom",
            "page-down",
            "page-up",
            "collapse",
            "expand",
            "activate",
        ];
        for (action, operand) in ListAction::ALL.into_iter().zip(operands) {
            assert_eq!(action.as_str(), operand);
            assert_eq!(ListAction::parse(operand), Some(action));
            assert_eq!(
                UiCommand::List(action).name(),
                format!("list.{}", operand.replace('-', "_"))
            );
        }
        assert_eq!(ListAction::parse("page_down"), None);
    }

    #[test]
    fn mode_actions_parse_their_operands() {
        for action in ModeAction::ALL {
            assert_eq!(ModeAction::parse(action.as_str()), Some(action));
            assert_eq!(
                UiCommand::Mode(action).name(),
                format!("mode.{}", action.as_str())
            );
        }
        assert_eq!(ModeAction::parse("get"), None);
    }

    #[test]
    fn history_steps_parse_their_operands() {
        for step in HistoryStep::ALL {
            assert_eq!(HistoryStep::parse(step.as_str()), Some(step));
            assert_eq!(
                UiCommand::History(step).name(),
                format!("history.{}", step.as_str())
            );
        }
        assert_eq!(HistoryStep::parse("get"), None);
    }

    #[test]
    fn toggle_targets_parse_their_operands() {
        for target in ToggleTarget::ALL {
            assert_eq!(ToggleTarget::parse(target.as_str()), Some(target));
            assert_eq!(
                UiCommand::LayoutToggle(target).name(),
                format!("layout.toggle.{}", target.as_str())
            );
        }
        assert_eq!(ToggleTarget::parse("left"), None);
    }

    #[test]
    fn appearance_values_parse_and_cycle() {
        for (choice, value) in AppearanceChoice::ALL
            .into_iter()
            .zip(["light", "dark", "system"])
        {
            assert_eq!(choice.as_str(), value);
            assert_eq!(AppearanceChoice::parse(value), Some(choice));
        }
        assert_eq!(AppearanceChoice::parse("Light"), None);
        for choice in AppearanceChoice::ALL {
            let json = serde_json::to_value(choice).unwrap();
            assert_eq!(json, serde_json::Value::from(choice.as_str()));
            assert_eq!(
                serde_json::from_value::<AppearanceChoice>(json).unwrap(),
                choice
            );
        }
        assert_eq!(AppearanceChoice::System.next(), AppearanceChoice::Light);
        assert_eq!(AppearanceChoice::Light.next(), AppearanceChoice::Dark);
        assert_eq!(AppearanceChoice::Dark.next(), AppearanceChoice::System);
    }
}
