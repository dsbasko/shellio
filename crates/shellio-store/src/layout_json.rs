//! JSON раскладки в `workbench_layouts.layout`: модель в конверте с версией формата (ADR-0018,
//! «Уточнено в P1.4 (планирование)»). Версия живёт только в базе и в провод не попадает.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use shellio_core::WorkbenchLayout;

/// Версия формата, которую пишет и читает эта сборка.
pub(crate) const LAYOUT_FORMAT: u32 = 1;

#[derive(Serialize)]
struct Outgoing<'a> {
    version: u32,
    layout: &'a WorkbenchLayout,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Incoming {
    version: u32,
    layout: Value,
}

pub(crate) fn encode(layout: &WorkbenchLayout) -> Result<String, serde_json::Error> {
    serde_json::to_string(&Outgoing {
        version: LAYOUT_FORMAT,
        layout,
    })
}

/// Раскладка из текста строки. `None` — JSON не разбирается или версия формата неизвестна.
pub(crate) fn decode(text: &str) -> Option<WorkbenchLayout> {
    let incoming: Incoming = serde_json::from_str(text).ok()?;
    if incoming.version != LAYOUT_FORMAT {
        return None;
    }
    serde_json::from_value(incoming.layout).ok()
}

#[cfg(test)]
mod tests {
    use shellio_core::test_support::SequentialIds;
    use shellio_core::{IdGen, PanelId};

    use super::*;

    fn layout() -> WorkbenchLayout {
        let mut ids = SequentialIds::new();
        let ids: &mut dyn IdGen = &mut ids;
        WorkbenchLayout::default_for(PanelId::generate(ids), PanelId::generate(ids))
    }

    #[test]
    fn layout_round_trips_in_versioned_envelope() {
        let layout = layout();
        let text = encode(&layout).unwrap();
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["version"], LAYOUT_FORMAT);
        assert_eq!(json["layout"], serde_json::to_value(&layout).unwrap());
        assert_eq!(decode(&text), Some(layout));
    }

    #[test]
    fn unreadable_text_and_unknown_version_decode_to_none() {
        let layout = serde_json::to_value(layout()).unwrap();
        let mut extra = layout.clone();
        extra["extra"] = Value::from(1);
        for text in [
            "не JSON".to_owned(),
            layout.to_string(),
            serde_json::json!({"version": 2, "layout": layout}).to_string(),
            serde_json::json!({"version": 1, "layout": layout, "extra": 1}).to_string(),
            serde_json::json!({"version": 1, "layout": extra}).to_string(),
            serde_json::json!({"version": 1, "layout": {"navigator": "x"}}).to_string(),
        ] {
            assert_eq!(decode(&text), None, "{text}");
        }
    }
}
