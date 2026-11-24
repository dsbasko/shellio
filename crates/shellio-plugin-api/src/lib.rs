//! Чистый контракт плагина shellio без AppKit (ADR-0008): plugin id, манифест, кардинальность,
//! модель панели и её стёртая обёртка, представление, реестр и проверка представления по фикстурам
//! раскладки.

#![forbid(unsafe_code)]

pub mod contract;
#[cfg(test)]
mod fake;
mod id;
mod manifest;
mod model;
mod registry;

pub use contract::{SizeVariants, VariantTable};
pub use id::{PluginId, PluginIdError};
pub use manifest::{Cardinality, CommandSpec, KeyDefault, Manifest, PluginInfo, ZoneSet};
pub use model::{
    DynModel, PanelModel, Presentation, Reach, Refusal, Region, STATE_LIMIT, Step, Via,
};
pub use registry::{Entry, HOST_NAMESPACES, Plugin, Registry, RegistryError, TITLE_MAX_CHARS};
