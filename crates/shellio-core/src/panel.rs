//! Панели рабочей области и раскладка, которую ядро строит без команд (ADR-0009, ADR-0018,
//! разделы «Уточнено в P1.4 (планирование)»).

use std::fmt;

use serde::{Deserialize, Serialize};
use shellio_layout::{
    Hidden, Layout, LayoutMetrics, MinSize, Part, SHARE_TOTAL, ZoneMap, ZoneSizes, ZoneState,
};

use crate::clock::UnixMillis;
use crate::ids::{PanelId, WorkbenchId};
use crate::title::Title;

/// Раскладка рабочей области в ядре.
pub type WorkbenchLayout = Layout<PanelId>;

/// Plugin id панели колонки навигатора.
pub const NAVIGATOR_PLUGIN: &str = "navigator";
/// Plugin id терминала.
pub const TERMINAL_PLUGIN: &str = "terminal";
/// Название панели навигатора раскладки по умолчанию.
pub const NAVIGATOR_TITLE: &str = "Навигатор";
/// Название панели терминала раскладки по умолчанию.
pub const TERMINAL_TITLE: &str = "Терминал";

/// Метрики геометрии раскладки из темы. Ядро считает по ним отказ деления, границы размера и кадры
/// `query.layout`, адаптер окна — кадры видов, поэтому чтение и кадры AX совпадают.
pub fn layout_metrics() -> LayoutMetrics {
    let metrics = shellio_theme::METRICS;
    LayoutMetrics {
        titlebar: whole_points(metrics.titlebar),
        tabbar: whole_points(metrics.tabbar),
        gap: whole_points(metrics.gap),
        line: whole_points(metrics.line),
        status_bar: whole_points(metrics.status_bar),
        drop_edge: whole_points(metrics.drop_edge),
    }
}

/// Метрика темы в целых точках: значения токенов целые, тест сверяет их обратным переводом.
fn whole_points(value: f64) -> u32 {
    value.round().clamp(0.0, f64::from(u32::MAX)) as u32
}

/// `min_size` панели плагина. До манифестов P1.5 у всех плагинов значение по умолчанию 200×120
/// (ADR-0009).
pub fn min_size(_plugin: &PluginId) -> MinSize {
    MinSize::DEFAULT
}

/// Строковый plugin id панели: от 1 до [`PluginId::MAX_CHARS`] символов из `[a-z0-9-]`
/// (ADR-0007). До P1.5 хост рисует для любого id вид-заглушку, P1.5 подставляет фабрики по
/// тому же id без миграции.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PluginId(String);

/// Причина отказа в plugin id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginIdError {
    Empty,
    /// Символ вне `[a-z0-9-]`.
    InvalidChar {
        ch: char,
    },
    /// Длиннее [`PluginId::MAX_CHARS`] символов.
    TooLong {
        chars: usize,
    },
}

impl fmt::Display for PluginIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("plugin id пустой"),
            Self::InvalidChar { ch } => write!(
                f,
                "plugin id содержит символ U+{:04X} вне [a-z0-9-]",
                u32::from(*ch)
            ),
            Self::TooLong { chars } => write!(
                f,
                "plugin id длиной {chars} символов длиннее {} символов",
                PluginId::MAX_CHARS
            ),
        }
    }
}

impl std::error::Error for PluginIdError {}

impl PluginId {
    /// Наибольшая длина plugin id в символах.
    pub const MAX_CHARS: usize = 64;

    /// Plugin id из строки как есть, без обрезки пробелов.
    pub fn new(raw: &str) -> Result<Self, PluginIdError> {
        if raw.is_empty() {
            return Err(PluginIdError::Empty);
        }
        if let Some(ch) = raw
            .chars()
            .find(|ch| !matches!(ch, 'a'..='z' | '0'..='9' | '-'))
        {
            return Err(PluginIdError::InvalidChar { ch });
        }
        if raw.len() > Self::MAX_CHARS {
            return Err(PluginIdError::TooLong { chars: raw.len() });
        }
        Ok(Self(raw.to_owned()))
    }

    /// Plugin id как строка.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Панель колонки навигатора.
    pub fn is_navigator(&self) -> bool {
        self.0 == NAVIGATOR_PLUGIN
    }
}

impl TryFrom<String> for PluginId {
    type Error = PluginIdError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::new(&raw)
    }
}

impl From<PluginId> for String {
    fn from(id: PluginId) -> Self {
        id.0
    }
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Панель рабочей области. Сессии у панели до P2.1 нет.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    pub id: PanelId,
    pub workbench: WorkbenchId,
    pub plugin: PluginId,
    pub title: Title,
    pub created_at: UnixMillis,
}

/// Раскладка, восстановленная из панелей рабочей области (ADR-0018, «Восстановление испорченной
/// раскладки»). Первая по `(created_at, id)` панель `navigator` встаёт в колонку навигатора,
/// остальные — в центр одной частью. Фокус — первая панель центра, иначе навигатор. Размеры и
/// скрытие по умолчанию. `None` — панелей нет.
pub(crate) fn recovered_layout<'a>(
    panels: impl IntoIterator<Item = &'a Panel>,
) -> Option<WorkbenchLayout> {
    let mut ordered: Vec<&Panel> = panels.into_iter().collect();
    ordered.sort_by_key(|panel| (panel.created_at, panel.id));
    let navigator = ordered
        .iter()
        .find(|panel| panel.plugin.is_navigator())
        .map(|panel| panel.id);
    let center: Vec<PanelId> = ordered
        .iter()
        .map(|panel| panel.id)
        .filter(|&id| Some(id) != navigator)
        .collect();
    let focus = center.first().copied().or(navigator)?;
    let mut zones: ZoneMap<ZoneState<PanelId>> = ZoneMap::default();
    if !center.is_empty() {
        zones.center.parts.push(Part {
            tabs: center,
            active: 0,
            share: SHARE_TOTAL,
        });
    }
    Some(Layout {
        navigator,
        zones,
        sizes: ZoneSizes::DEFAULT,
        hidden: Hidden::default(),
        only: None,
        zoomed: None,
        focus: vec![focus],
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::ids::{IdGen, SequentialIds};

    #[test]
    fn layout_metrics_come_from_theme_tokens() {
        let theme = shellio_theme::METRICS;
        let metrics = layout_metrics();
        for (points, token) in [
            (metrics.titlebar, theme.titlebar),
            (metrics.tabbar, theme.tabbar),
            (metrics.gap, theme.gap),
            (metrics.line, theme.line),
            (metrics.status_bar, theme.status_bar),
            (metrics.drop_edge, theme.drop_edge),
        ] {
            assert_eq!(f64::from(points), token);
        }
        assert_eq!(
            metrics,
            LayoutMetrics {
                titlebar: 40,
                tabbar: 34,
                gap: 6,
                line: 1,
                status_bar: 26,
                drop_edge: 24,
            }
        );
    }

    #[test]
    fn plugin_id_follows_wire_rule() {
        let longest = "a".repeat(PluginId::MAX_CHARS);
        for ok in [
            "navigator",
            "terminal",
            "a",
            "git-diff",
            "x9",
            longest.as_str(),
        ] {
            assert_eq!(
                PluginId::new(ok).map(|id| id.to_string()),
                Ok(ok.to_owned())
            );
        }
        assert_eq!(PluginId::new(""), Err(PluginIdError::Empty));
        assert_eq!(
            PluginId::new(&"a".repeat(65)),
            Err(PluginIdError::TooLong { chars: 65 })
        );
        for (raw, ch) in [
            ("Terminal", 'T'),
            ("git_diff", '_'),
            (" terminal", ' '),
            ("терминал", 'т'),
            ("a.b", '.'),
        ] {
            assert_eq!(
                PluginId::new(raw),
                Err(PluginIdError::InvalidChar { ch }),
                "{raw}"
            );
        }
        assert!(PluginId::new(NAVIGATOR_PLUGIN).unwrap().is_navigator());
        assert!(!PluginId::new(TERMINAL_PLUGIN).unwrap().is_navigator());
    }

    #[test]
    fn plugin_id_is_checked_when_parsed() {
        let id: PluginId = serde_json::from_str("\"terminal\"").unwrap();
        assert_eq!(id.as_str(), "terminal");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"terminal\"");
        assert!(serde_json::from_str::<PluginId>("\"Terminal\"").is_err());
        assert!(serde_json::from_str::<PluginId>("\"\"").is_err());
    }

    struct Panels {
        ids: SequentialIds,
        workbench: WorkbenchId,
        panels: Vec<Panel>,
    }

    impl Panels {
        fn new() -> Self {
            let mut ids = SequentialIds::new();
            let workbench = WorkbenchId::generate(&mut ids);
            Self {
                ids,
                workbench,
                panels: Vec::new(),
            }
        }

        fn add(&mut self, plugin: &str, created_at: i64) -> PanelId {
            let ids: &mut dyn IdGen = &mut self.ids;
            let id = PanelId::generate(ids);
            self.panels.push(Panel {
                id,
                workbench: self.workbench,
                plugin: PluginId::new(plugin).unwrap(),
                title: Title::new(plugin).unwrap(),
                created_at: UnixMillis(created_at),
            });
            id
        }

        fn recovered(&self) -> Option<WorkbenchLayout> {
            // Порядок входа не важен: восстановление сортирует само.
            let layout = recovered_layout(self.panels.iter().rev());
            if let Some(layout) = &layout {
                let live: BTreeSet<_> = self.panels.iter().map(|panel| panel.id).collect();
                assert_eq!(layout.validate(&live), Ok(()));
                assert_eq!(recovered_layout(&self.panels).as_ref(), Some(layout));
            }
            layout
        }
    }

    fn center(layout: &WorkbenchLayout) -> Vec<PanelId> {
        layout
            .zones
            .center
            .parts
            .iter()
            .flat_map(|part| part.tabs.iter().copied())
            .collect()
    }

    #[test]
    fn recovery_puts_first_navigator_in_column_and_rest_in_center() {
        let mut p = Panels::new();
        let terminal = p.add(TERMINAL_PLUGIN, 30);
        let late_navigator = p.add(NAVIGATOR_PLUGIN, 20);
        let navigator = p.add(NAVIGATOR_PLUGIN, 10);
        let notes = p.add("notes", 20);
        let layout = p.recovered().unwrap();
        assert_eq!(layout.navigator, Some(navigator));
        // `created_at` 20 у двух панелей: порядок решает `PanelId`.
        assert_eq!(center(&layout), [late_navigator, notes, terminal]);
        assert_eq!(layout.zones.center.parts[0].active, 0);
        assert_eq!(layout.zones.center.parts[0].share, SHARE_TOTAL);
        assert_eq!(layout.focus, [late_navigator]);
        assert_eq!(layout.sizes, ZoneSizes::DEFAULT);
        assert_eq!(layout.hidden, Hidden::default());
        for zone in [
            &layout.zones.left,
            &layout.zones.right,
            &layout.zones.bottom,
        ] {
            assert!(zone.is_empty());
        }
    }

    #[test]
    fn recovery_without_navigator_puts_everything_in_center() {
        let mut p = Panels::new();
        let second = p.add(TERMINAL_PLUGIN, 5);
        let first = p.add(TERMINAL_PLUGIN, 5);
        let layout = p.recovered().unwrap();
        assert_eq!(layout.navigator, None);
        let mut expected = [second, first];
        expected.sort();
        assert_eq!(center(&layout), expected);
        assert_eq!(layout.focus, [expected[0]]);
    }

    #[test]
    fn recovery_of_lone_navigator_focuses_it() {
        let mut p = Panels::new();
        let navigator = p.add(NAVIGATOR_PLUGIN, 1);
        let layout = p.recovered().unwrap();
        assert_eq!(layout.navigator, Some(navigator));
        assert!(layout.zones.center.is_empty());
        assert_eq!(layout.focus, [navigator]);
    }

    #[test]
    fn recovery_of_no_panels_is_none() {
        assert_eq!(Panels::new().recovered(), None);
    }
}
