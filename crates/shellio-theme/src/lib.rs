//! Тема shellio без AppKit: ячейки оформления и их разбор из системных входов, цвета слотов,
//! слой фокуса ADR-0027, метрики в точках и параметры движения.

#![forbid(unsafe_code)]

mod cell;
mod color;
pub mod contrast;
pub mod icons;
mod tokens;

pub use cell::{
    Appearance, AppearanceChoice, Cell, Contrast, Motion, Overrides, ResolvedCell, Source,
};
pub use color::{Srgb8, Tint, UnitRgb};
pub use tokens::{
    Adr0027Column, AppTheme, Colors, CubicBezier, Focus, FontSizes, KeyMode, LineHeights, METRICS,
    MONO_FAMILIES, MONO_FONT_FILES, Metrics, MotionTokens, Origin, Radii, Shadow, Slot, Value,
};
