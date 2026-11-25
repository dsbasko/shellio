//! Сторона прототипа калибровки допусков (ADR-0037): корпус ширины текста, страница замера для
//! Chrome и проверка шрифта, которым Chrome набрал случай. Материалы закреплены в
//! `conformance/calibration/` и здесь только читаются.

use base64::Engine as _;
use serde::Deserialize;

use crate::Error;
use crate::passport::{Cell, Contrast, FontFace, Rgba, Theme};

/// Семейство стиля корпуса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    Ui,
    Mono,
}

impl Family {
    /// Стек прототипа: переменная `prototype/css/tokens.css`.
    fn css(self) -> &'static str {
        match self {
            Family::Ui => "var(--ui)",
            Family::Mono => "var(--mono)",
        }
    }
}

/// Стиль корпуса: шрифт, начертание, кегль и разрядка.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Style {
    pub id: String,
    pub family: Family,
    pub weight: u32,
    pub size: f64,
    pub tracking_em: f64,
}

/// Строка корпуса и её половина (`fit` или `holdout`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Line {
    pub id: String,
    pub half: String,
    pub text: String,
}

/// Корпус `text.json`: случай — каждая строка в каждом стиле.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Corpus {
    pub styles: Vec<Style>,
    pub strings: Vec<Line>,
}

impl Corpus {
    pub fn parse(place: &str, text: &str) -> Result<Self, Error> {
        let corpus: Corpus =
            serde_json::from_str(text).map_err(|e| Error::new(place, e.to_string()))?;
        if corpus.styles.is_empty() || corpus.strings.is_empty() {
            return Err(Error::new(place, "корпус без стилей или строк"));
        }
        let mut ids: Vec<String> = corpus.cases().map(|(s, l)| case_id(s, l)).collect();
        ids.sort();
        if let Some(pair) = ids.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(Error::new(place, format!("случай {} повторяется", pair[0])));
        }
        Ok(corpus)
    }

    /// Случаи по порядку файла: стиль, затем строка.
    pub fn cases(&self) -> impl Iterator<Item = (&Style, &Line)> {
        self.styles
            .iter()
            .flat_map(|style| self.strings.iter().map(move |line| (style, line)))
    }
}

/// Имя случая: `<стиль>.<строка>`; id строк точек не содержат.
pub fn case_id(style: &Style, line: &Line) -> String {
    format!("{}.{}", style.id, line.id)
}

/// Ячейки `controls.json` в именах паспорта: `more` файла — `high` паспорта.
pub fn cells(place: &str, controls: &str) -> Result<Vec<Cell>, Error> {
    #[derive(Deserialize)]
    struct Controls {
        cells: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        theme: Theme,
        contrast: String,
    }
    let parsed: Controls =
        serde_json::from_str(controls).map_err(|e| Error::new(place, e.to_string()))?;
    parsed
        .cells
        .iter()
        .map(|entry| {
            let contrast = match entry.contrast.as_str() {
                "normal" => Contrast::Normal,
                "more" => Contrast::High,
                other => return Err(Error::new(place, format!("контраст {other}"))),
            };
            Ok(Cell::new(entry.theme, contrast))
        })
        .collect()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Начало столбца подписей страницы в точках: правее листа значков.
pub const TEXT_X: u32 = 128;

/// Страница замера: лист значков с начала страницы на целых точках и строки корпуса — каждая в
/// `span` без отступов, рамки и переноса. Цвета и шрифты — из `tokens.css` по адресу `tokens`;
/// второй тон листа — `--line-2`.
pub fn page(tokens: &str, sheet: &str, corpus: &Corpus) -> String {
    let mut out = String::from("<!doctype html>\n<html><head><meta charset=\"utf-8\">\n");
    out.push_str(&format!(
        "<link rel=\"stylesheet\" href=\"{}\">\n",
        escape(tokens)
    ));
    out.push_str(&format!(
        "<style>\nhtml, body {{ margin: 0; padding: 0; }}\n\
         body {{ background: var(--surface); color: var(--text-2); }}\n\
         #icons {{ position: absolute; left: 0; top: 0; line-height: 0; }}\n\
         #icons > svg {{ display: block; }}\n\
         #icons [data-tone=\"secondary\"] {{ stroke: var(--line-2); }}\n\
         #text {{ position: absolute; left: {TEXT_X}px; top: 0; }}\n\
         #text div {{ white-space: pre; }}\n\
         #text span {{ white-space: pre; margin: 0; padding: 0; border: 0; }}\n\
         </style>\n</head><body>\n<div id=\"icons\">"
    ));
    out.push_str(sheet.trim());
    out.push_str("</div>\n<div id=\"text\">\n");
    for (style, line) in corpus.cases() {
        let tracking = if style.tracking_em == 0.0 {
            String::new()
        } else {
            format!("; letter-spacing: {}em", style.tracking_em)
        };
        out.push_str(&format!(
            "<div><span data-case=\"{}\" style=\"font-family: {}; font-weight: {}; font-size: {}px{}\">{}</span></div>\n",
            escape(&case_id(style, line)),
            style.family.css(),
            style.weight,
            style.size,
            tracking,
            escape(&line.text),
        ));
    }
    out.push_str("</div>\n</body></html>\n");
    out
}

/// Имя PostScript вложенных начертаний JetBrains Mono начинается так.
const MONO_FACE: &str = "JetBrainsMono-";

/// Случай моноширинного стиля набран только вложенным JetBrains Mono (`text.json`,
/// `families.mono.single_font`): одноимённый системный шрифт и запасной шрифт — отказ.
pub fn mono_faces(case: &str, faces: &[FontFace]) -> Result<(), String> {
    if faces.is_empty() {
        return Err(format!("{case}: Chrome не назвал шрифт"));
    }
    match faces
        .iter()
        .find(|face| !face.custom || !face.family.starts_with(MONO_FACE))
    {
        Some(face) => Err(format!(
            "{case}: набран шрифтом {} (вложенный: {}), а не вложенным JetBrains Mono",
            face.family, face.custom
        )),
        None => Ok(()),
    }
}

/// Оба тона двухцветного значка отличаются от фона не меньше порога переднего плана
/// (`controls.json`, `two_tone.rule`).
pub fn two_tone(background: Rgba, primary: Rgba, secondary: Rgba, threshold: u8) -> bool {
    let differs = |tone: Rgba| {
        let delta = |a: u8, b: u8| a.abs_diff(b);
        delta(tone.r, background.r)
            .max(delta(tone.g, background.g))
            .max(delta(tone.b, background.b))
            >= threshold
    };
    differs(primary) && differs(secondary)
}

/// Байты PNG снимка `Page.captureScreenshot` из его base64.
pub fn shot(data: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("снимок не base64: {e}"))
}
