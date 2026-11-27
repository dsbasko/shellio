//! `VirtualList` — список на view-based `NSTableView` (ADR-0002): строки с устойчивыми id, выбор,
//! собственные Up и Down с прокруткой через clip view (P0.2, находка 4), намерения `select`,
//! `activate` и `cancel` по `Escape`.
//!
//! Чистая часть — модель с разницей наборов ([`model`]); её тестирует `make check`. Вид применяет
//! операции к таблице: полный `reloadData` только при первом наполнении, замена набора —
//! диапазонами строк, поэтому виды строк переиспользуются.

pub(crate) mod model;
mod view;

pub(crate) use view::RowViews;
pub(crate) use view::press_row;
pub use view::{ListRowFacts, VirtualList, VirtualListFacts};
