//! Палитра команд (ADR-0010, «Уточнено в P1.6 (планирование): наложения»): плавающая непрозрачная
//! панель с полем запроса, списком групп и подвалом. Чистая раскладка по числам паспорта —
//! [`layout`].

pub(crate) mod layout;
pub(crate) mod rows;
mod view;

pub use view::{CommandPalette, PaletteIntent, shadow_margin};
