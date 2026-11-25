//! Перевод записанных ответов CDP (`conformance.facts()`) в элементы паспорта.

use serde_json::Value;
use shellio_conformance::cdp::{elements, kind_of, parse_color, parse_px};
use shellio_conformance::fields::ElementKind;

fn facts() -> Value {
    let text = include_str!("fixtures/facts-chrome-titlebar-dark-normal.json");
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn titlebar_facts_become_passport_elements() {
    let list = elements(&facts()).unwrap();
    assert_eq!(list.len(), 20);
    let kbd = list.iter().find(|e| e.ck == "chrome.search.kbd").unwrap();
    assert_eq!(kbd.kind, ElementKind::Badge);
    assert!(kbd.padding.is_some() && kbd.radius.is_some());
    let label = list.iter().find(|e| e.ck == "chrome.search.label").unwrap();
    assert_eq!(label.text.as_deref(), Some("Сессии, файлы, команды"));
    assert_eq!(label.truncated, Some(false));
    let icon = list.iter().find(|e| e.ck == "chrome.search.icon").unwrap();
    assert_eq!(icon.icon.as_ref().map(|i| i.name.as_str()), Some("search"));
}

#[test]
fn duplicate_key_is_rejected() {
    let mut f = facts();
    let list = f["elements"].as_array_mut().unwrap();
    let copy = list[1].clone();
    list.push(copy);
    assert!(elements(&f).unwrap_err().contains("повторяется"));
}

#[test]
fn css_values_parse() {
    assert_eq!(parse_px("12.5px"), Some(12.5));
    assert_eq!(parse_px("normal"), None);
    let c = parse_color("rgba(1, 2, 3, 0.5)").unwrap();
    assert_eq!((c.r, c.g, c.b, c.a), (1, 2, 3, 0.5));
    assert_eq!(parse_color("rgb(255, 0, 10)").unwrap().a, 1.0);
    assert_eq!(kind_of("div", "chrome::before"), ElementKind::Pseudo);
    assert_eq!(
        kind_of("button", "zone.center.part.1.tabs.tab.2"),
        ElementKind::Tab
    );
}
