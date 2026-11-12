//! Детерминированные фикстуры сцены. Модуль не зависит от AppKit и в стоимость компонентов не
//! входит.

/// Наборы вкладок сценария `tabs`.
pub const TAB_COUNTS: [usize; 3] = [1, 8, 30];

/// Тип вкладки: от него зависит значок в центральной и нижней плитках.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabKind {
    Terminal,
    Agent,
    Editor,
    Changes,
    History,
}

/// Состояние, которое показывает точка статуса вкладки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabStatus {
    None,
    Run,
    Idle,
    Exit,
}

/// Нагрузка активной вкладки: замер процесса или оценка данных внутри приложения.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Meter {
    Measured(String),
    Estimate(String),
}

impl Meter {
    pub fn text(&self) -> &str {
        match self {
            Meter::Measured(text) | Meter::Estimate(text) => text,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabSpec {
    pub title: String,
    pub kind: TabKind,
    pub status: TabStatus,
    pub meter: Meter,
}

const BASE: [(&str, TabKind); 10] = [
    ("zsh", TabKind::Terminal),
    ("claude", TabKind::Agent),
    ("nvim", TabKind::Editor),
    ("cargo test", TabKind::Terminal),
    ("Изменения", TabKind::Changes),
    ("codex", TabKind::Agent),
    ("make check", TabKind::Terminal),
    ("История", TabKind::History),
    ("logs · api", TabKind::Terminal),
    ("ревью 42", TabKind::Changes),
];

const STATUSES: [TabStatus; 4] = [
    TabStatus::Run,
    TabStatus::Idle,
    TabStatus::None,
    TabStatus::Exit,
];

/// Набор из `count` вкладок. Подписи повторяются через 10 вкладок с номером круга.
pub fn tabs(count: usize) -> Vec<TabSpec> {
    (0..count)
        .map(|index| {
            let (name, kind) = BASE[index % BASE.len()];
            let round = index / BASE.len();
            let title = if round == 0 {
                name.to_owned()
            } else {
                format!("{name} {}", round + 1)
            };
            let meter = match kind {
                TabKind::Terminal | TabKind::Agent => {
                    Meter::Measured(format!("{} МБ · {}%", 120 + (index * 37) % 400, index % 7))
                }
                TabKind::Editor | TabKind::Changes | TabKind::History => {
                    Meter::Estimate(format!("≈ {} МБ", 8 + (index * 5) % 60))
                }
            };
            TabSpec {
                title,
                kind,
                status: STATUSES[index % STATUSES.len()],
                meter,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn tab_sets_are_deterministic() {
        for count in TAB_COUNTS {
            assert_eq!(tabs(count), tabs(count), "набор {count}");
        }
        // Меньший набор — начало большего.
        assert_eq!(tabs(8)[..], tabs(30)[..8]);
        assert_eq!(tabs(1)[0].title, "zsh");
        assert_eq!(tabs(30)[29].title, "ревью 42 3");
        assert_eq!(tabs(30)[29].meter, Meter::Estimate("≈ 33 МБ".to_owned()));
    }

    #[test]
    fn tab_sets_have_requested_sizes_and_unique_titles() {
        for count in TAB_COUNTS {
            let set = tabs(count);
            assert_eq!(set.len(), count);
            let titles: BTreeSet<&str> = set.iter().map(|tab| tab.title.as_str()).collect();
            assert_eq!(titles.len(), count, "подписи набора {count} повторяются");
            assert!(set.iter().all(|tab| !tab.meter.text().is_empty()));
        }
        let all = tabs(30);
        for status in STATUSES {
            assert!(all.iter().any(|tab| tab.status == status), "{status:?}");
        }
        assert!(
            all.iter()
                .any(|tab| matches!(tab.meter, Meter::Measured(_)))
        );
        assert!(
            all.iter()
                .any(|tab| matches!(tab.meter, Meter::Estimate(_)))
        );
    }

    #[test]
    fn empty_tab_set() {
        assert!(tabs(0).is_empty());
    }
}
