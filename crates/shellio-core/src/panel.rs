//! Панели рабочей области и раскладка, которую ядро строит без команд (ADR-0009, ADR-0018,
//! разделы «Уточнено в P1.4 (планирование)»).

use serde::{Deserialize, Serialize};
use shellio_layout::{
    Hidden, Layout, LayoutMetrics, Part, SHARE_TOTAL, ZoneMap, ZoneSizes, ZoneState,
};

pub use shellio_plugin_api::{PluginId, PluginIdError};

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

/// Панель колонки навигатора. Особое место навигатора в раскладке — знание хоста, а не контракта
/// плагина (ADR-0008, «Уточнено в P1.5 (планирование)»).
pub fn is_navigator(plugin: &PluginId) -> bool {
    plugin.as_str() == NAVIGATOR_PLUGIN
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

/// Данные панели, как их хранит база: исходный текст JSON с версией или строка, которую прочитать
/// нельзя (ADR-0018, «Уточнено в P1.5 (планирование)»). Панели без строки данных в
/// [`crate::model::State::panel_data`] нет.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PanelData {
    /// Исходный текст данных версии `version`. Текст `null` — данные `null`.
    Text { version: u32, raw: String },
    /// Строка есть, но версия вне `1..=u32::MAX`, тип столбца не `TEXT` или байты не UTF-8. Ядро
    /// её не перезаписывает.
    ///
    /// Вариант с пустыми полями, а не единичный: у единичного варианта внутренне помеченного
    /// перечня serde молча пропускает лишние поля.
    Unreadable {},
}

/// Статус панели: производное значение по строкам базы и реестру плагинов, в базу не пишется
/// (ADR-0008, «Статус панели»). Значения в JSON стабильны.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PanelStatus {
    /// Тело плагина строится, команды панели принимаются.
    Ready,
    /// Плагин панели не зарегистрирован.
    PluginMissing,
    /// Панель не каноническая для своего ключа экземпляра.
    Duplicate,
    /// Данные нечитаемы или не проходят разбор плагина.
    StateCorrupt,
    /// `init` отказал или дал данные больше предела.
    InitFailed,
    /// `migrate` отказал или дал данные больше предела.
    MigrateFailed,
    /// Данные записаны более новой версией плагина.
    StateNewer,
}

impl PanelStatus {
    /// Имя в JSON и в деталях отказа `panel_unavailable`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::PluginMissing => "plugin_missing",
            Self::Duplicate => "duplicate",
            Self::StateCorrupt => "state_corrupt",
            Self::InitFailed => "init_failed",
            Self::MigrateFailed => "migrate_failed",
            Self::StateNewer => "state_newer",
        }
    }
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
        .find(|panel| is_navigator(&panel.plugin))
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
    fn plugin_id_comes_from_the_plugin_contract() {
        let id: shellio_plugin_api::PluginId = PluginId::new(TERMINAL_PLUGIN).unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"terminal\"");
        assert_eq!(
            PluginId::new("Terminal"),
            Err(PluginIdError::InvalidChar { ch: 'T' })
        );
        assert!(serde_json::from_str::<PluginId>("\"Terminal\"").is_err());
    }

    #[test]
    fn only_the_navigator_plugin_is_the_navigator() {
        assert!(is_navigator(&PluginId::new(NAVIGATOR_PLUGIN).unwrap()));
        assert!(!is_navigator(&PluginId::new(TERMINAL_PLUGIN).unwrap()));
        assert!(!is_navigator(&PluginId::new("navigator-2").unwrap()));
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
