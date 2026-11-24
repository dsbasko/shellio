//! Нейтральное тело панели: карточка с названием, plugin id, зоной и классом размера. На ней стоят
//! плагины-заполнители, пока у панели нет своего содержимого.

use serde_json::Value;
use shellio_appkit::{InspectNode, NeutralCard, PanelCard, Piece};
use shellio_layout::{Geometry, SizeClass, Zone};
use shellio_plugin_api::Presentation;
use shellio_theme::AppTheme;

use crate::{PluginView, ViewCtx, ViewFactory};

/// Что показывает нейтральная карточка плагина `plugin` с названием `title` в геометрии части зоны;
/// `None` — геометрии ещё нет.
#[must_use]
pub fn neutral_card(title: &str, plugin: &str, geometry: Option<&Geometry>) -> PanelCard {
    let place = geometry.map_or("—", |geometry| match geometry.zone {
        Zone::Left => "Левая зона",
        Zone::Right => "Правая зона",
        Zone::Center => "Центр",
        Zone::Bottom => "Нижняя зона",
    });
    let size = geometry.map_or("—", |geometry| match geometry.size {
        SizeClass::Narrow => "Narrow",
        SizeClass::Regular => "Regular",
        SizeClass::Wide => "Wide",
        SizeClass::ShortBar => "ShortBar",
    });
    PanelCard {
        title: title.to_owned(),
        plugin: plugin.to_owned(),
        place: place.to_owned(),
        size: size.to_owned(),
    }
}

/// Фабрика нейтрального тела для плагина без собственного вида.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeutralFactory {
    plugin: &'static str,
    title: &'static str,
}

impl NeutralFactory {
    /// Фабрика для плагина `plugin` с названием манифеста `title`.
    #[must_use]
    pub const fn new(plugin: &'static str, title: &'static str) -> Self {
        Self { plugin, title }
    }
}

impl ViewFactory for NeutralFactory {
    fn plugin(&self) -> &'static str {
        self.plugin
    }

    fn build(&self, ctx: ViewCtx<'_>, _state: &Value) -> Box<dyn PluginView> {
        let card = neutral_card(self.title, self.plugin, None);
        Box::new(NeutralView {
            factory: *self,
            card: NeutralCard::new(ctx.app, ctx.theme, card),
        })
    }
}

struct NeutralView {
    factory: NeutralFactory,
    card: NeutralCard,
}

impl PluginView for NeutralView {
    fn body(&self) -> Piece<'_> {
        Piece::from(&self.card)
    }

    fn adapt(&mut self, geometry: &Geometry, _presentation: &Presentation) {
        self.card.set_card(neutral_card(
            self.factory.title,
            self.factory.plugin,
            Some(geometry),
        ));
    }

    fn update(&mut self, _state: &Value) {}

    fn set_theme(&mut self, theme: &AppTheme) {
        self.card.set_theme(theme);
    }

    fn set_active(&mut self, _active: bool) {}

    fn set_focused(&mut self, _focused: bool) {}

    fn focus_preferred(&self) {}

    fn inspect(&self, prefix: &str) -> Vec<InspectNode> {
        let parent = prefix.strip_suffix('.').unwrap_or(prefix);
        vec![self.card.inspect(&format!("{prefix}card"), Some(parent))]
    }

    fn close_view(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_names_the_zone_and_size_class_of_the_part() {
        let cases = [
            (Zone::Left, 240.0, 600.0, "Левая зона", "Narrow"),
            (Zone::Right, 480.0, 600.0, "Правая зона", "Regular"),
            (Zone::Center, 1200.0, 600.0, "Центр", "Wide"),
            (Zone::Bottom, 900.0, 140.0, "Нижняя зона", "ShortBar"),
        ];
        for (zone, width, height, place, size) in cases {
            let geometry = Geometry::of(zone, width, height);
            let card = neutral_card("Терминал", "terminal", Some(&geometry));
            assert_eq!(card.title, "Терминал");
            assert_eq!(card.meta(), format!("terminal · {place} · {size}"));
        }
    }

    #[test]
    fn card_without_geometry_leaves_the_place_open() {
        let card = neutral_card("Навигатор", "navigator", None);
        assert_eq!(card.meta(), "navigator · — · —");
    }

    #[test]
    fn factory_names_its_plugin() {
        assert_eq!(
            NeutralFactory::new("terminal", "Терминал").plugin(),
            "terminal"
        );
    }
}
