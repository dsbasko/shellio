//! Контракт вида плагина на главном потоке (ADR-0008, «Уточнено в P1.5 (планирование)»): фабрика
//! [`ViewFactory`] строит тело [`PluginView`] из безопасных компонентов `shellio-appkit`. Тело
//! получает неизменяемую проекцию данных панели, шлёт запросы хосту через [`PanelSink`] и привязывает
//! свою работу к токенам [`ViewWork`]. Хост держит тело в [`Body`]: закрытие отменяет работу вида и
//! зовёт `close_view` ровно один раз. От ядра и от `shellio-control` crate не зависит.

#![forbid(unsafe_code)]

mod body;
mod neutral;
mod sink;
mod work;

use serde_json::Value;
use shellio_appkit::{App, InspectNode, Piece};
use shellio_layout::Geometry;
use shellio_plugin_api::Presentation;
use shellio_theme::AppTheme;

pub use body::Body;
pub use neutral::{NeutralFactory, neutral_card};
pub use sink::{Done, PanelRequest, PanelSink};
pub use work::{ViewWork, WorkToken};

/// Фабрика вида плагина. Живёт на главном потоке в таблице фабрик хоста.
pub trait ViewFactory: 'static {
    /// Plugin id манифеста.
    fn plugin(&self) -> &'static str;
    /// Строит тело панели по её данным. Хост зовёт один раз на панель в статусе `ready`.
    fn build(&self, ctx: ViewCtx<'_>, state: &Value) -> Box<dyn PluginView>;
}

/// Тело панели плагина. Живёт на главном потоке.
pub trait PluginView {
    /// Корневой компонент тела: хост ставит его в контейнер панели.
    fn body(&self) -> Piece<'_>;
    /// Геометрия части зоны и представление, которое модель посчитала для неё.
    fn adapt(&mut self, geometry: &Geometry, presentation: &Presentation);
    /// Новая проекция данных панели.
    fn update(&mut self, state: &Value);
    fn set_theme(&mut self, theme: &AppTheme);
    fn set_active(&mut self, active: bool);
    fn set_focused(&mut self, focused: bool);
    /// Переводит ввод в нужный компонент тела.
    fn focus_preferred(&self);
    /// Узлы тела для `ui.inspect`: id начинаются с `prefix`, кадры уже в точках окна.
    fn inspect(&self, prefix: &str) -> Vec<InspectNode>;
    /// Снимает подписки вида. Работу по токенам [`ViewWork`] хост отменяет сам до этого вызова.
    fn close_view(&mut self);
}

/// Что фабрика получает от хоста при построении тела.
pub struct ViewCtx<'a> {
    /// Главный поток: его требуют конструкторы компонентов.
    pub app: &'a App,
    pub theme: &'a AppTheme,
    /// Отправитель запросов, привязанный хостом к панели тела.
    pub sink: PanelSink,
    /// Токены работы этого тела.
    pub work: ViewWork,
}
