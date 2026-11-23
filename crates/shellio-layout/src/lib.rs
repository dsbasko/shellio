//! Раскладка рабочей области без AppKit (ADR-0009, ADR-0031): зоны, части, геометрия и классы размера.

#![forbid(unsafe_code)]

mod geometry;
mod model;

pub use geometry::{
    DropTarget, Edge, Frames, Geometry, LayoutMetrics, NavigatorFrame, PartFrame, Point, Rect,
    SizeClass, Viewport, ZoneFrames, drop_target, geometry,
};
pub use model::{
    Axis, Hidden, Layout, LayoutError, Location, MinSize, Part, SHARE_TOTAL, Zone, ZoneMap,
    ZoneSizes, ZoneState,
};
