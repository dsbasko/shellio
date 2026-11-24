//! Ожидания снимков сценария `scripts/e2e/plugins.sh` (пункт `demo-snapshot`): файлы обеих тем
//! разбираются после подстановки `PanelId`, называют участки плитки и выбранной строки
//! демо-списка, а цвет каждого участка выводится из `tokens.css` и таблицы ADR-0027.

use std::path::Path;

use shellio_e2e::TOLERANCE;
use shellio_e2e::css::{Appearance, Cell, Contrast, Layer, Palette};
use shellio_e2e::expect::{self, Expectations};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const PANEL: &str = "019a8f2e-0000-7000-8000-00000000e303";
const PLACEHOLDER: &str = "@DEMO@";

const LIGHT: Cell = Cell::new(Appearance::Light, Contrast::Normal);
const DARK: Cell = Cell::new(Appearance::Dark, Contrast::Normal);

fn text(name: &str) -> Result<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/e2e/plugins-expect")
        .join(name);
    Ok(std::fs::read_to_string(path)?)
}

/// Файл после той же подстановки, что делает сценарий: `@DEMO@` — `PanelId` демо-списка.
fn load(name: &str) -> Result<Expectations> {
    Ok(expect::parse_expectations(
        &text(name)?.replace(PLACEHOLDER, PANEL),
    )?)
}

#[test]
fn both_themes_name_the_chrome_the_tile_and_the_selected_row() -> Result<()> {
    for (name, cell) in [("light.json", LIGHT), ("dark.json", DARK)] {
        let raw = text(name)?;
        assert_eq!(raw.matches(PLACEHOLDER).count(), 2, "{name}");
        assert_eq!(raw.matches('@').count(), 4, "{name}: иных подстановок нет");
        let expectations = load(name)?;
        assert_eq!(expectations.cell, cell, "{name}");
        let regions: Vec<(&str, &str, &str, Layer)> = expectations
            .regions
            .iter()
            .map(|r| {
                (
                    r.id.as_str(),
                    r.component.as_str(),
                    r.slot.as_str(),
                    r.layer,
                )
            })
            .collect();
        let list = format!("layout.panel.{PANEL}.list");
        let row = format!("layout.panel.{PANEL}.list.row.c");
        assert_eq!(
            regions,
            [
                ("chrome.background", "chrome", "chrome", Layer::None),
                ("tile.fill", list.as_str(), "panel", Layer::ToneNavigation),
                ("row.selected", row.as_str(), "sel", Layer::ToneNavigation),
            ],
            "{name}"
        );
    }
    Ok(())
}

/// Оба файла несут одни и те же участки: темы различаются только ячейкой.
#[test]
fn themes_differ_only_by_the_cell() -> Result<()> {
    let light = text("light.json")?;
    let dark = text("dark.json")?;
    assert_eq!(
        light.replace("\"appearance\": \"light\"", "\"appearance\": \"dark\""),
        dark
    );
    Ok(())
}

/// Цвет каждого участка выводится из токенов, а выбранная строка отличается от заливки плитки
/// больше допуска оракула: снимок без выбора ожиданиям не соответствует.
#[test]
fn every_region_resolves_from_tokens_and_the_selection_is_distinguishable() -> Result<()> {
    let palette = Palette::embedded()?;
    for name in ["light.json", "dark.json"] {
        let expectations = load(name)?;
        let cell = expectations.cell;
        let mut colors = Vec::new();
        for region in &expectations.regions {
            colors.push(palette.expected(cell, &region.slot, region.layer)?);
        }
        let (tile, row) = (colors[1], colors[2]);
        assert!(
            row.max_delta(tile) > TOLERANCE,
            "{name}: выбранная строка {row:?} неотличима от плитки {tile:?}"
        );
        let plain = palette.expected(cell, "sel", Layer::None)?;
        assert_ne!(
            row, plain,
            "{name}: тон фокуса лежит и над выбранной строкой"
        );
    }
    Ok(())
}
