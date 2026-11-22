//! Оракул на синтетических снимках галереи.
//!
//! Снимки рисуются по прямоугольникам, переведённым в точки сверху слева вручную, и цветам,
//! посчитанным вручную по `tokens.css` и таблице ADR-0027: ни перевод кадров, ни палитра оракула
//! в рисовании не участвуют. Фон окна вне частей — пурпурный, чтобы участок, попавший не туда,
//! расходился с ожиданием.

use std::path::Path;
use std::process::Command;

use serde_json::json;
use shellio_e2e::css::{Appearance, Cell, Contrast, Palette, Rgb};
use shellio_e2e::expect::{self, Expectations, Geometry, InspectView, WindowView};
use shellio_e2e::png::{self as snapshot, Image, SrgbTag};
use shellio_e2e::{Inputs, Report, TOLERANCE, Verdict, compare, read_marker};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const EXPECT_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/e2e/appkit-expect"
);

const WIDTH: f64 = 1180.0;
const HEIGHT: f64 = 720.0;

/// Прямоугольник в точках окна с началом сверху слева, переведённый вручную.
#[derive(Clone, Copy)]
struct Top {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

const fn top(x: f64, y: f64, w: f64, h: f64) -> Top {
    Top { x, y, w, h }
}

/// Узлы галереи как в дымовом прогоне задачи 8: id, родитель, кадр протокола (начало внизу
/// слева) и тот же кадр сверху слева.
const NODES: [(&str, Option<&str>, [f64; 4], Top); 10] = [
    (
        "chrome",
        None,
        [0.0, 680.0, 1180.0, 40.0],
        top(0.0, 0.0, 1180.0, 40.0),
    ),
    (
        "chrome.workspace",
        Some("chrome"),
        [81.0, 689.0, 86.0, 22.0],
        top(81.0, 9.0, 86.0, 22.0),
    ),
    (
        "chrome.search",
        Some("chrome"),
        [360.0, 687.0, 460.0, 26.0],
        top(360.0, 7.0, 460.0, 26.0),
    ),
    (
        "chrome.zones",
        Some("chrome"),
        [1040.0, 687.0, 94.0, 26.0],
        top(1040.0, 7.0, 94.0, 26.0),
    ),
    (
        "chrome.appearance",
        Some("chrome"),
        [1142.0, 688.0, 28.0, 24.0],
        top(1142.0, 8.0, 28.0, 24.0),
    ),
    (
        "gallery.marker",
        None,
        [12.0, 644.0, 24.0, 24.0],
        top(12.0, 52.0, 24.0, 24.0),
    ),
    (
        "gallery.surface.focus",
        None,
        [48.0, 12.0, 554.0, 656.0],
        top(48.0, 52.0, 554.0, 656.0),
    ),
    (
        "gallery.field",
        Some("gallery.surface.focus"),
        [60.0, 624.0, 280.0, 32.0],
        top(60.0, 64.0, 280.0, 32.0),
    ),
    (
        "gallery.menu",
        Some("gallery.surface.focus"),
        [352.0, 628.0, 24.0, 24.0],
        top(352.0, 68.0, 24.0, 24.0),
    ),
    (
        "gallery.surface.plain",
        None,
        [614.0, 12.0, 554.0, 656.0],
        top(614.0, 52.0, 554.0, 656.0),
    ),
];

fn node(id: &str) -> Top {
    NODES
        .iter()
        .find(|(name, ..)| *name == id)
        .map_or(top(0.0, 0.0, 0.0, 0.0), |(.., rect)| *rect)
}

const DARK: Cell = Cell::new(Appearance::Dark, Contrast::Normal);
const LIGHT: Cell = Cell::new(Appearance::Light, Contrast::Normal);
const DARK_IC: Cell = Cell::new(Appearance::Dark, Contrast::Increased);
const LIGHT_IC: Cell = Cell::new(Appearance::Light, Contrast::Increased);

/// Цвета частей галереи, посчитанные вручную: заливка `chrome`; `panel` под тоном навигации;
/// `panel` под вуалью; `surface` поля под тоном навигации подложки.
#[derive(Clone, Copy)]
struct Colors {
    chrome: Rgb,
    focus: Rgb,
    plain: Rgb,
    field: Rgb,
}

fn colors(cell: Cell) -> Colors {
    let rgb = Rgb::new;
    match (cell.appearance, cell.contrast) {
        (Appearance::Dark, Contrast::Normal) => Colors {
            chrome: rgb(30, 29, 27),
            focus: rgb(16, 16, 17),
            plain: rgb(28, 26, 24),
            field: rgb(30, 31, 31),
        },
        (Appearance::Light, Contrast::Normal) => Colors {
            chrome: rgb(239, 237, 232),
            focus: rgb(251, 252, 253),
            plain: rgb(244, 243, 242),
            field: rgb(246, 246, 244),
        },
        (Appearance::Dark, Contrast::Increased) => Colors {
            chrome: rgb(30, 29, 27),
            focus: rgb(17, 18, 19),
            plain: rgb(27, 26, 25),
            field: rgb(31, 32, 32),
        },
        (Appearance::Light, Contrast::Increased) => Colors {
            chrome: rgb(239, 237, 232),
            focus: rgb(248, 249, 250),
            plain: rgb(236, 236, 235),
            field: rgb(243, 243, 242),
        },
    }
}

const MAGENTA: Rgb = Rgb::new(255, 0, 255);
const BUTTON: Rgb = Rgb::new(128, 128, 128);

/// Холст RGBA с рисованием прямоугольников в точках сверху слева.
struct Canvas {
    scale: f64,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(scale: f64) -> Canvas {
        let width = (WIDTH * scale) as u32;
        let height = (HEIGHT * scale) as u32;
        let mut canvas = Canvas {
            scale,
            width,
            height,
            rgba: vec![255; width as usize * height as usize * 4],
        };
        canvas.fill(top(0.0, 0.0, WIDTH, HEIGHT), MAGENTA);
        canvas
    }

    fn fill(&mut self, rect: Top, color: Rgb) {
        let s = self.scale;
        let (x0, y0) = ((rect.x * s) as u32, (rect.y * s) as u32);
        let (x1, y1) = (
            ((rect.x + rect.w) * s) as u32,
            ((rect.y + rect.h) * s) as u32,
        );
        for y in y0..y1.min(self.height) {
            for x in x0..x1.min(self.width) {
                let i = (y as usize * self.width as usize + x as usize) * 4;
                self.rgba[i..i + 4].copy_from_slice(&[color.r, color.g, color.b, 255]);
            }
        }
    }

    fn marker(&mut self, generation: u64) {
        let origin = node("gallery.marker");
        for row in 0..8_u32 {
            for col in 0..8_u32 {
                let bit = (generation >> (63 - (row * 8 + col))) & 1 == 1;
                let color = if bit {
                    Rgb::new(255, 255, 255)
                } else {
                    Rgb::new(0, 0, 0)
                };
                let cell = top(
                    origin.x + f64::from(col) * 3.0,
                    origin.y + f64::from(row) * 3.0,
                    3.0,
                    3.0,
                );
                self.fill(cell, color);
            }
        }
    }

    /// Галерея в ячейке с маркером поколения.
    fn gallery(scale: f64, colors: Colors, generation: u64) -> Canvas {
        let mut canvas = Canvas::new(scale);
        canvas.fill(node("chrome"), colors.chrome);
        for button in [
            "chrome.workspace",
            "chrome.search",
            "chrome.zones",
            "chrome.appearance",
        ] {
            canvas.fill(node(button), BUTTON);
        }
        canvas.fill(node("gallery.surface.focus"), colors.focus);
        canvas.fill(node("gallery.surface.plain"), colors.plain);
        canvas.fill(node("gallery.field"), colors.field);
        canvas.fill(node("gallery.menu"), BUTTON);
        canvas.marker(generation);
        canvas
    }
}

/// Как PNG объявляет цвет.
enum Tag {
    Srgb,
    Cicp { primaries: u8, transfer: u8 },
    Icc(Vec<u8>),
    GammaChromaticities { gamma: u32 },
    Untagged,
}

fn encode(canvas: &Canvas, tag: Tag) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut info = png::Info::with_size(canvas.width, canvas.height);
    if let Tag::Icc(profile) = &tag {
        info.icc_profile = Some(profile.clone().into());
    }
    let mut encoder = png::Encoder::with_info(&mut bytes, info)?;
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::NoCompression);
    encoder.set_filter(png::Filter::NoFilter);
    if let Tag::Srgb = tag {
        encoder.set_source_srgb(png::SrgbRenderingIntent::Perceptual);
    }
    let mut writer = encoder.write_header()?;
    match tag {
        Tag::Cicp {
            primaries,
            transfer,
        } => writer.write_chunk(png::chunk::cICP, &[primaries, transfer, 0, 1])?,
        Tag::GammaChromaticities { gamma } => {
            writer.write_chunk(png::chunk::gAMA, &gamma.to_be_bytes())?;
            let chromaticities: Vec<u8> = [
                31_270_u32, 32_900, 64_000, 33_000, 30_000, 60_000, 15_000, 6_000,
            ]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
            writer.write_chunk(png::chunk::cHRM, &chromaticities)?;
        }
        Tag::Srgb | Tag::Icc(_) | Tag::Untagged => {}
    }
    writer.write_image_data(&canvas.rgba)?;
    writer.finish()?;
    Ok(bytes)
}

/// Профиль ICC с одним тегом `desc` версии 4.
fn icc_profile(name: &str) -> Vec<u8> {
    let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_be_bytes).collect();
    let mut desc = b"mluc".to_vec();
    desc.extend([0; 4]);
    desc.extend(1_u32.to_be_bytes());
    desc.extend(12_u32.to_be_bytes());
    desc.extend(*b"enUS");
    desc.extend((units.len() as u32).to_be_bytes());
    desc.extend(28_u32.to_be_bytes());
    desc.extend(units);
    let mut profile = vec![0; 128];
    profile.extend(1_u32.to_be_bytes());
    profile.extend(*b"desc");
    profile.extend(144_u32.to_be_bytes());
    profile.extend((desc.len() as u32).to_be_bytes());
    profile.extend(desc);
    let size = profile.len() as u32;
    profile[0..4].copy_from_slice(&size.to_be_bytes());
    profile
}

fn window_json(cell: Cell, scale: f64, generation: u64) -> String {
    json!({
        "id": 3,
        "ok": {"ui": {"window": {
            "pid": 123,
            "mode": "gallery",
            "window_number": 4242,
            "frame": {"x": 166.0, "y": 171.0, "width": WIDTH, "height": HEIGHT},
            "backing_scale_factor": scale,
            "visible": true,
            "key": true,
            "color_space": {"name": "sRGB IEC61966-2.1", "srgb": true},
            "title": "Основной",
            "appearance_choice": "system",
            "lightness": {"value": cell.appearance, "source": "gallery"},
            "contrast": {"value": cell.contrast, "source": "gallery"},
            "motion": {"value": "normal", "source": "system"},
            "screen": 0,
            "maximum_frames_per_second": 120,
            "screens": [],
            "mirror_revision": 1,
            "generation": generation
        }}}
    })
    .to_string()
}

fn inspect_json(generation: u64) -> String {
    let components: Vec<serde_json::Value> = NODES
        .iter()
        .enumerate()
        .map(|(i, (id, parent, [x, y, width, height], _))| {
            json!({
                "id": id,
                "class": "Synthetic",
                "object": 1000 + i,
                "parent": parent,
                "frame": {"x": x, "y": y, "width": width, "height": height},
                "visible": true,
                "first_responder": false,
                "enabled": null,
                "role": null,
                "label": null,
                "help": null,
                "selected": null,
                "expanded": null,
                "animation_keys": 0,
                "state": null
            })
        })
        .collect();
    json!({
        "id": 4,
        "ok": {"ui": {"inspect": {
            "window_number": 4242,
            "content": {"width": WIDTH, "height": HEIGHT},
            "generation": generation,
            "components": components,
            "materials": []
        }}}
    })
    .to_string()
}

fn expectations_file(cell: Cell) -> &'static str {
    match (cell.appearance, cell.contrast) {
        (Appearance::Light, Contrast::Normal) => "light.json",
        (Appearance::Dark, Contrast::Normal) => "dark.json",
        (Appearance::Light, Contrast::Increased) => "light-ic.json",
        (Appearance::Dark, Contrast::Increased) => "dark-ic.json",
    }
}

fn expectations_text(cell: Cell) -> Result<String> {
    Ok(std::fs::read_to_string(
        Path::new(EXPECT_DIR).join(expectations_file(cell)),
    )?)
}

fn load_expectations(cell: Cell) -> Result<Expectations> {
    Ok(expect::parse_expectations(&expectations_text(cell)?)?)
}

/// Сравнение снимка с входами по умолчанию: окно и дерево того же поколения.
struct Case {
    cell: Cell,
    scale: f64,
    window_generation: u64,
    inspect_generation: u64,
    expected_generation: u64,
    expectations: Expectations,
}

impl Case {
    fn new(cell: Cell, generation: u64) -> Result<Case> {
        Ok(Case {
            cell,
            scale: 2.0,
            window_generation: generation,
            inspect_generation: generation,
            expected_generation: generation,
            expectations: load_expectations(cell)?,
        })
    }

    fn window(&self) -> Result<WindowView> {
        Ok(expect::parse_window(&window_json(
            self.cell,
            self.scale,
            self.window_generation,
        ))?)
    }

    fn inspect(&self) -> Result<InspectView> {
        Ok(expect::parse_inspect(&inspect_json(
            self.inspect_generation,
        ))?)
    }

    fn run(&self, png: &[u8]) -> std::result::Result<Report, String> {
        let palette = Palette::embedded().map_err(|e| e.to_string())?;
        let window = self.window().map_err(|e| e.to_string())?;
        let inspect = self.inspect().map_err(|e| e.to_string())?;
        let image = snapshot::decode(png).map_err(|e| e.to_string())?;
        compare(&Inputs {
            palette: &palette,
            expectations: &self.expectations,
            window: &window,
            inspect: &inspect,
            image: &image,
            generation: self.expected_generation,
        })
        .map_err(|e| e.to_string())
    }
}

#[test]
fn matching_snapshot_passes_in_every_cell() -> Result<()> {
    for cell in Cell::ALL {
        let png = encode(&Canvas::gallery(2.0, colors(cell), 9), Tag::Srgb)?;
        let report = Case::new(cell, 9)?.run(&png)?;
        assert_eq!(report.verdict, Verdict::Pass, "{cell}: {:?}", report.reason);
        assert_eq!(report.regions.len(), 4, "{cell}");
        for region in &report.regions {
            assert_eq!(region.max_delta, 0, "{cell}: {}", region.id);
            assert_eq!(region.transparent, 0);
        }
        assert_eq!(report.generation.and_then(|g| g.png), Some(9));
    }
    Ok(())
}

#[test]
fn matching_snapshot_passes_at_scale_one() -> Result<()> {
    let png = encode(&Canvas::gallery(1.0, colors(LIGHT), 9), Tag::Srgb)?;
    let mut case = Case::new(LIGHT, 9)?;
    case.scale = 1.0;
    let report = case.run(&png)?;
    assert_eq!(report.verdict, Verdict::Pass, "{:?}", report.reason);
    let chrome = report
        .regions
        .iter()
        .find(|r| r.id == "chrome.background")
        .ok_or("нет участка chrome.background")?;
    assert_eq!(
        (
            chrome.pixels.x,
            chrome.pixels.y,
            chrome.pixels.width,
            chrome.pixels.height
        ),
        (860, 8, 140, 24)
    );
    Ok(())
}

/// Участки берут строки сверху: снимок, отражённый по высоте, расходится. Светлая ячейка: в
/// тёмной `chrome` и подложка под вуалью отличаются всего на 3 единицы.
#[test]
fn flipped_snapshot_fails() -> Result<()> {
    let mut canvas = Canvas::gallery(2.0, colors(LIGHT), 9);
    let stride = canvas.width as usize * 4;
    let rows: Vec<Vec<u8>> = canvas
        .rgba
        .chunks_exact(stride)
        .map(<[u8]>::to_vec)
        .collect();
    canvas.rgba = rows.into_iter().rev().flatten().collect();
    canvas.marker(9);
    let report = Case::new(LIGHT, 9)?.run(&encode(&canvas, Tag::Srgb)?)?;
    assert_eq!(report.verdict, Verdict::Mismatch);
    let failed: Vec<&str> = report
        .regions
        .iter()
        .filter(|region| !region.pass)
        .map(|region| region.id.as_str())
        .collect();
    // Подложка без фокуса тянется почти на всю высоту и под отражением остаётся собой.
    assert_eq!(
        failed,
        [
            "chrome.background",
            "surface.focus.background",
            "field.background"
        ]
    );
    Ok(())
}

#[test]
fn four_units_off_fails_and_three_passes() -> Result<()> {
    for (shift, verdict) in [(3_u8, Verdict::Pass), (4, Verdict::Mismatch)] {
        let mut colors = colors(DARK);
        colors.plain = Rgb::new(colors.plain.r, colors.plain.g + shift, colors.plain.b);
        let png = encode(&Canvas::gallery(2.0, colors, 9), Tag::Srgb)?;
        let report = Case::new(DARK, 9)?.run(&png)?;
        assert_eq!(
            report.verdict, verdict,
            "сдвиг {shift}: {:?}",
            report.reason
        );
        for region in &report.regions {
            let off = region.id == "surface.plain.background";
            assert_eq!(
                region.max_delta,
                if off { shift } else { 0 },
                "{}",
                region.id
            );
            assert_eq!(region.pass, !off || shift <= TOLERANCE, "{}", region.id);
        }
    }
    Ok(())
}

/// Отдельные выбросы не сдвигают медиану.
#[test]
fn median_ignores_a_minority_of_outliers() -> Result<()> {
    let mut canvas = Canvas::gallery(2.0, colors(DARK), 9);
    canvas.fill(
        top(48.0 + 24.0, 52.0 + 560.0, 20.0, 48.0),
        Rgb::new(250, 250, 250),
    );
    let report = Case::new(DARK, 9)?.run(&encode(&canvas, Tag::Srgb)?)?;
    assert_eq!(report.verdict, Verdict::Pass, "{:?}", report.reason);
    Ok(())
}

#[test]
fn transparent_pixels_in_a_region_fail() -> Result<()> {
    let mut canvas = Canvas::gallery(2.0, colors(DARK), 9);
    let i = ((64 * 2 + 20) as usize * canvas.width as usize + (60 + 200) * 2) * 4;
    canvas.rgba[i + 3] = 0;
    let report = Case::new(DARK, 9)?.run(&encode(&canvas, Tag::Srgb)?)?;
    assert_eq!(report.verdict, Verdict::Mismatch);
    let field = report
        .regions
        .iter()
        .find(|r| r.id == "field.background")
        .ok_or("нет участка field.background")?;
    assert_eq!(
        (field.transparent, field.max_delta, field.pass),
        (1, 0, false)
    );
    Ok(())
}

/// Самопроверка сценария: слот `chrome`, заменённый на `text`, обязан дать расхождение.
#[test]
fn deliberately_wrong_slot_is_detected() -> Result<()> {
    for cell in Cell::ALL {
        let mut case = Case::new(cell, 9)?;
        let text = expectations_text(cell)?.replace("\"slot\": \"chrome\"", "\"slot\": \"text\"");
        case.expectations = expect::parse_expectations(&text)?;
        let png = encode(&Canvas::gallery(2.0, colors(cell), 9), Tag::Srgb)?;
        let report = case.run(&png)?;
        assert_eq!(report.verdict, Verdict::Mismatch, "{cell}");
        let wrong: Vec<&str> = report
            .regions
            .iter()
            .filter(|r| !r.pass)
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(wrong, ["chrome.background"], "{cell}");
    }
    Ok(())
}

#[test]
fn region_outside_the_frame_is_an_error() -> Result<()> {
    let png = encode(&Canvas::gallery(2.0, colors(DARK), 9), Tag::Srgb)?;
    let outside = [
        // За нижним краем поля.
        (
            r#""x": 180, "y": 8, "width": 88, "height": 16"#,
            r#""x": 180, "y": 30, "width": 88, "height": 16"#,
        ),
        // За правым краем ChromeBar.
        (
            r#""x": 860, "y": 8, "width": 140, "height": 24"#,
            r#""x": 1100, "y": 8, "width": 140, "height": 24"#,
        ),
        // У края компонента ближе 4 точек.
        (
            r#""x": 860, "y": 8, "width": 140, "height": 24"#,
            r#""x": 860, "y": 2, "width": 140, "height": 24"#,
        ),
        // Рядом с полем поиска.
        (
            r#""x": 860, "y": 8, "width": 140, "height": 24"#,
            r#""x": 700, "y": 8, "width": 140, "height": 24"#,
        ),
    ];
    for (from, to) in outside {
        let mut case = Case::new(DARK, 9)?;
        let text = expectations_text(DARK)?.replace(from, to);
        assert_ne!(text, expectations_text(DARK)?, "замена {from:?} не найдена");
        case.expectations = expect::parse_expectations(&text)?;
        let error = case.run(&png).err().ok_or("участок принят")?;
        assert!(error.contains("участок"), "{error}");
    }
    let mut case = Case::new(DARK, 9)?;
    let text = expectations_text(DARK)?.replace("\"gallery.field\"", "\"gallery.missing\"");
    case.expectations = expect::parse_expectations(&text)?;
    assert!(case.run(&png).is_err(), "компонента нет в ui.inspect");
    Ok(())
}

#[test]
fn wrong_scale_is_an_error() -> Result<()> {
    let png = encode(&Canvas::gallery(1.0, colors(DARK), 9), Tag::Srgb)?;
    let error = Case::new(DARK, 9)?
        .run(&png)
        .err()
        .ok_or("масштаб принят")?;
    assert!(error.contains("размер PNG 1180×720"), "{error}");

    let png = encode(&Canvas::gallery(2.0, colors(DARK), 9), Tag::Srgb)?;
    let mut case = Case::new(DARK, 9)?;
    case.scale = 1.0;
    assert!(case.run(&png).is_err());
    case.scale = 0.0;
    assert!(case.run(&png).is_err());
    Ok(())
}

#[test]
fn non_srgb_png_is_an_error() -> Result<()> {
    let canvas = Canvas::gallery(1.0, colors(DARK), 9);
    let mut case = Case::new(DARK, 9)?;
    case.scale = 1.0;
    let rejected = [
        (Tag::Icc(icc_profile("Display P3")), "Display P3"),
        (
            Tag::Cicp {
                primaries: 12,
                transfer: 13,
            },
            "первичными 12",
        ),
        (Tag::GammaChromaticities { gamma: 45_000 }, "gAMA 45000"),
        (Tag::Untagged, "нет сведений о цвете"),
    ];
    for (tag, reason) in rejected {
        let error = case
            .run(&encode(&canvas, tag)?)
            .err()
            .ok_or("PNG не в sRGB принят")?;
        assert!(error.contains(reason), "{error}");
    }
    let accepted = [
        (Tag::Srgb, SrgbTag::SrgbChunk),
        (
            Tag::Cicp {
                primaries: 1,
                transfer: 13,
            },
            SrgbTag::Cicp,
        ),
        (
            Tag::GammaChromaticities { gamma: 45_455 },
            SrgbTag::GammaChromaticities,
        ),
    ];
    for (tag, srgb) in accepted {
        let png = encode(&canvas, tag)?;
        assert_eq!(snapshot::decode(&png)?.tag, srgb);
        let report = case.run(&png)?;
        assert_eq!(
            report.verdict,
            Verdict::Pass,
            "{srgb:?}: {:?}",
            report.reason
        );
    }
    Ok(())
}

#[test]
fn snapshot_of_generation_5_is_rejected_when_9_is_expected() -> Result<()> {
    let png = encode(&Canvas::gallery(2.0, colors(DARK), 5), Tag::Srgb)?;
    let mut case = Case::new(DARK, 9)?;
    let report = case.run(&png)?;
    assert_eq!(report.verdict, Verdict::Generation);
    assert!(
        report.regions.is_empty(),
        "участки сравнены на чужом поколении"
    );
    assert_eq!(report.generation.and_then(|g| g.png), Some(5));

    // Тот же снимок при ожидаемом 5 проходит: отказ дал только маркер.
    case.window_generation = 5;
    case.inspect_generation = 5;
    case.expected_generation = 5;
    assert_eq!(case.run(&png)?.verdict, Verdict::Pass);
    Ok(())
}

#[test]
fn inspect_of_another_generation_is_not_observed() -> Result<()> {
    let png = encode(&Canvas::gallery(2.0, colors(DARK), 9), Tag::Srgb)?;
    let mut case = Case::new(DARK, 9)?;
    case.inspect_generation = 8;
    let report = case.run(&png)?;
    assert_eq!(report.verdict, Verdict::Generation);
    assert!(report.regions.is_empty());
    Ok(())
}

#[test]
fn unreadable_marker_is_not_observed() -> Result<()> {
    let mut canvas = Canvas::gallery(2.0, colors(DARK), 9);
    canvas.fill(top(12.0 + 9.0, 52.0 + 6.0, 3.0, 3.0), Rgb::new(128, 30, 30));
    let report = Case::new(DARK, 9)?.run(&encode(&canvas, Tag::Srgb)?)?;
    assert_eq!(report.verdict, Verdict::Generation);
    assert!(
        report
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("ячейка маркера 2×3")),
        "{:?}",
        report.reason
    );
    Ok(())
}

#[test]
fn generations_beyond_32_bits_are_read_exactly() -> Result<()> {
    let case = Case::new(DARK, 0)?;
    let window = case.window()?;
    let inspect = case.inspect()?;
    for generation in [
        0,
        1,
        (1 << 32) + 7,
        (1 << 40) | 0xABCD,
        0x8000_0000_0000_0001,
        u64::MAX,
    ] {
        for scale in [1.0, 2.0] {
            let png = encode(&Canvas::gallery(scale, colors(DARK), generation), Tag::Srgb)?;
            let image: Image = snapshot::decode(&png)?;
            let mut window = window.clone();
            window.backing_scale_factor = scale;
            let geometry = Geometry::new(&window, &inspect, (image.width, image.height))?;
            let read = read_marker(&image, &inspect, &geometry)?;
            assert_eq!(read.ok(), Some(generation), "масштаб {scale}");
        }
    }
    Ok(())
}

#[test]
fn window_of_another_cell_is_an_error() -> Result<()> {
    let png = encode(&Canvas::gallery(2.0, colors(LIGHT), 9), Tag::Srgb)?;
    let mut case = Case::new(LIGHT, 9)?;
    case.expectations = load_expectations(DARK)?;
    let error = case.run(&png).err().ok_or("чужая ячейка принята")?;
    assert!(error.contains("ячейка окна «светлая»"), "{error}");
    Ok(())
}

/// Файлы ожиданий четырёх ячеек: одни и те же участки, своя ячейка, все годятся для раскладки
/// галереи.
#[test]
fn committed_expectations_cover_the_milestone_regions() -> Result<()> {
    let ids = [
        "chrome.background",
        "surface.focus.background",
        "surface.plain.background",
        "field.background",
    ];
    for cell in [LIGHT, DARK, LIGHT_IC, DARK_IC] {
        let expectations = load_expectations(cell)?;
        assert_eq!(expectations.cell, cell);
        let found: Vec<&str> = expectations.regions.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(found, ids, "{cell}");
    }
    Ok(())
}

fn write(dir: &Path, name: &str, contents: &[u8]) -> Result<String> {
    let path = dir.join(name);
    std::fs::write(&path, contents)?;
    Ok(path.to_string_lossy().into_owned())
}

fn oracle(args: &[&str]) -> Result<(i32, String, String)> {
    let output = Command::new(env!("CARGO_BIN_EXE_shellio-e2e-oracle"))
        .env_clear()
        .args(args)
        .output()?;
    Ok((
        output.status.code().ok_or("оракул убит сигналом")?,
        String::from_utf8(output.stdout)?,
        String::from_utf8(output.stderr)?,
    ))
}

/// Коды выхода бинаря: совпадение 0, ошибка исполнения 1, неверный вызов 2, расхождение 3,
/// поколение не наблюдалось 4.
#[test]
fn binary_reports_verdicts_with_exit_codes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let dir = dir.path();
    let window = write(dir, "window.json", window_json(DARK, 1.0, 9).as_bytes())?;
    let inspect = write(dir, "inspect.json", inspect_json(9).as_bytes())?;
    let expect = Path::new(EXPECT_DIR)
        .join("dark.json")
        .to_string_lossy()
        .into_owned();
    let good = write(
        dir,
        "good.png",
        &encode(&Canvas::gallery(1.0, colors(DARK), 9), Tag::Srgb)?,
    )?;
    let compare_args = |png: &str| {
        vec![
            "compare".to_owned(),
            "--expect".to_owned(),
            expect.clone(),
            "--inspect".to_owned(),
            inspect.clone(),
            "--window".to_owned(),
            window.clone(),
            "--png".to_owned(),
            png.to_owned(),
        ]
    };
    let run = |args: Vec<String>| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        oracle(&args)
    };

    let (code, stdout, _) = run(compare_args(&good))?;
    assert_eq!(code, 0);
    let report: serde_json::Value = serde_json::from_str(&stdout)?;
    assert_eq!(report["verdict"], "pass");
    assert_eq!(report["generation"]["png"], 9);
    assert_eq!(report["regions"][0]["expected"], "#1E1D1B");

    let mut off = colors(DARK);
    off.chrome = Rgb::new(40, 29, 27);
    let off = write(
        dir,
        "off.png",
        &encode(&Canvas::gallery(1.0, off, 9), Tag::Srgb)?,
    )?;
    let (code, stdout, _) = run(compare_args(&off))?;
    assert_eq!(code, 3);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stdout)?["verdict"],
        "mismatch"
    );

    let mut explicit = compare_args(&good);
    explicit.extend(["--generation".to_owned(), "10".to_owned()]);
    let (code, stdout, _) = run(explicit)?;
    assert_eq!(code, 4);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stdout)?["verdict"],
        "generation"
    );

    let p3 = write(
        dir,
        "p3.png",
        &encode(
            &Canvas::gallery(1.0, colors(DARK), 9),
            Tag::Icc(icc_profile("Display P3")),
        )?,
    )?;
    let (code, stdout, stderr) = run(compare_args(&p3))?;
    assert_eq!(code, 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stdout)?["verdict"],
        "error"
    );
    assert!(stderr.contains("Display P3"), "{stderr}");

    let marker = |png: &str| {
        oracle(&[
            "marker",
            "--inspect",
            inspect.as_str(),
            "--window",
            window.as_str(),
            "--png",
            png,
        ])
    };
    let (code, stdout, _) = marker(&good)?;
    assert_eq!((code, stdout.trim()), (0, "9"));

    let mut smeared = Canvas::gallery(1.0, colors(DARK), 9);
    smeared.fill(top(12.0, 52.0, 3.0, 3.0), Rgb::new(200, 50, 50));
    let smeared = write(dir, "smeared.png", &encode(&smeared, Tag::Srgb)?)?;
    assert_eq!(marker(&smeared)?.0, 4);

    assert_eq!(oracle(&["compare", "--png", good.as_str()])?.0, 2);
    assert_eq!(oracle(&["marker", "--expect", expect.as_str()])?.0, 2);
    assert_eq!(oracle(&["unknown"])?.0, 2);
    assert_eq!(oracle(&[])?.0, 2);
    assert_eq!(oracle(&["--help"])?.0, 0);
    Ok(())
}
