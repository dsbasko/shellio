//! Действия интерфейса, которые не меняют состояния ядра (ADR-0007, «Уточнено в P1.3»).
//!
//! Чистый тип без обработчика: обработчик один, на главном потоке владельца. Меню, CLI, а с P1.6
//! клавиша и палитра вызывают его. Актор ядра эти действия не видит, поэтому они не входят в
//! [`crate::Command`].

/// Выбор оформления: светлота или следование системе.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

/// Действие интерфейса.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UiCommand {
    /// Спрятать окно владельца: процесс, сокет и ядро продолжают жить.
    WindowClose,
    /// Завершить владельца путём `applicationShouldTerminate`.
    AppQuit,
    /// Выбрать оформление до выхода процесса.
    AppearanceSet(AppearanceChoice),
}

impl UiCommand {
    /// Все действия: по одному на каждое значение выбора оформления.
    pub const ALL: [Self; 5] = [
        Self::WindowClose,
        Self::AppQuit,
        Self::AppearanceSet(AppearanceChoice::Light),
        Self::AppearanceSet(AppearanceChoice::Dark),
        Self::AppearanceSet(AppearanceChoice::System),
    ];

    /// Имя команды в управляющем API.
    pub const fn name(self) -> &'static str {
        match self {
            Self::WindowClose => "window.close",
            Self::AppQuit => "app.quit",
            Self::AppearanceSet(_) => "ui.appearance.set",
        }
    }

    /// Имя действия в keymap: у выбора оформления своё имя на каждое значение. Привязку в
    /// `keymap.toml` делает P1.6.
    pub const fn keymap_name(self) -> &'static str {
        match self {
            Self::WindowClose => "window.close",
            Self::AppQuit => "app.quit",
            Self::AppearanceSet(AppearanceChoice::Light) => "ui.appearance.light",
            Self::AppearanceSet(AppearanceChoice::Dark) => "ui.appearance.dark",
            Self::AppearanceSet(AppearanceChoice::System) => "ui.appearance.system",
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
    fn appearance_values_parse_and_cycle() {
        for (choice, value) in AppearanceChoice::ALL
            .into_iter()
            .zip(["light", "dark", "system"])
        {
            assert_eq!(choice.as_str(), value);
            assert_eq!(AppearanceChoice::parse(value), Some(choice));
        }
        assert_eq!(AppearanceChoice::parse("Light"), None);
        assert_eq!(AppearanceChoice::System.next(), AppearanceChoice::Light);
        assert_eq!(AppearanceChoice::Light.next(), AppearanceChoice::Dark);
        assert_eq!(AppearanceChoice::Dark.next(), AppearanceChoice::System);
    }
}
