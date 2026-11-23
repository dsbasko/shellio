//! Раскладка рабочей области без AppKit (ADR-0009, ADR-0031): зоны, части, операции, геометрия,
//! классы размера и контрактный тест геометрии.

#![forbid(unsafe_code)]

pub mod contract;
mod geometry;
mod model;
mod ops;

pub use geometry::{
    DropTarget, Edge, Frames, Geometry, LayoutMetrics, NavigatorFrame, PartFrame, Point, Rect,
    Size, SizeClass, Viewport, ZoneFrames, drop_target, geometry,
};
pub use model::{
    Axis, Hidden, Layout, LayoutError, Location, MinSize, Part, SHARE_TOTAL, Zone, ZoneMap,
    ZoneSizes, ZoneState,
};
pub use ops::{Direction, Hideable, OpError, Room};
