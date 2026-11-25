//! Материалы калибровки допусков сверки (ADR-0037): корпус текста, лист значков и контроли,
//! вложенные в бинарь, и их места на листе страницы «Калибровка». Чистая часть: вида не строит.
//! Лист значков читается как закреплённое подмножество SVG из `controls.json` (`icons.subset`).

use serde_json::Value;
use shellio_appkit::{
    CalibrationFigure, CalibrationIcon, CalibrationOp, CalibrationShape, CalibrationSpec,
    CalibrationText,
};

const TEXT: &str = include_str!("../../../conformance/calibration/text.json");
const ICONS: &str = include_str!("../../../conformance/calibration/icons.svg");
const CONTROLS: &str = include_str!("../../../conformance/calibration/controls.json");

/// Контроль ширины текста `padding-2pt` и его отступ в точках.
const PADDING_CONTROL: &str = "padding-2pt";
const PADDING_PT: f64 = 2.0;
/// Половина корпуса, строки которой идут в контроль.
const HOLDOUT: &str = "holdout";

/// Промежуток между листом значков и подписями и между подписями в ряду.
const GAP: f64 = 16.0;
/// Оценка ширины знака и шага ряда в долях кегля: только для мест на листе, ширину меряет набор.
const ADVANCE: f64 = 0.75;
const PITCH: f64 = 1.6;

/// Лист страницы «Калибровка» шириной `width`: ячейки значков на местах листа SVG от левого
/// верхнего угла, справа от них — подписи корпуса рядами, за ними подписи контроля. Места — на
/// целых точках. Подпись, не вошедшая в лист, остаётся в наборе: её ширину меряет набор, а не
/// снимок.
pub fn sheet(width: f64) -> Result<CalibrationSpec, String> {
    let (sheet_width, icons) = icons(ICONS)?;
    let mut texts = labels(TEXT, CONTROLS)?;
    let left = (sheet_width + GAP).ceil();
    let (mut x, mut y, mut row) = (left, 0.0_f64, 0.0_f64);
    for text in &mut texts {
        #[allow(clippy::cast_precision_loss)]
        let advance =
            (text.text.chars().count() as f64 * text.size * ADVANCE + text.pad + GAP).ceil();
        if x > left && x + advance > width {
            x = left;
            y += row;
            row = 0.0;
        }
        text.x = x;
        text.y = y;
        x += advance;
        row = row.max((text.size * PITCH).ceil());
    }
    Ok(CalibrationSpec { texts, icons })
}

fn labels(corpus: &str, controls: &str) -> Result<Vec<CalibrationText>, String> {
    let corpus: Value = serde_json::from_str(corpus).map_err(|e| format!("text.json: {e}"))?;
    let controls: Value =
        serde_json::from_str(controls).map_err(|e| format!("controls.json: {e}"))?;
    let styles = list(&corpus, "styles")?;
    let strings = list(&corpus, "strings")?;
    let padded: Vec<&str> = controls["text"]["controls"]
        .as_array()
        .and_then(|all| all.iter().find(|c| c["id"] == PADDING_CONTROL))
        .and_then(|control| control["styles"].as_array())
        .ok_or("controls.json: нет контроля padding-2pt")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut texts = Vec::with_capacity(styles.len() * strings.len());
    let mut control = Vec::new();
    for style in styles {
        let style_id = string(style, "id")?;
        let mono = match string(style, "family")? {
            "ui" => false,
            "mono" => true,
            other => return Err(format!("text.json: семейство {other}")),
        };
        let weight = style["weight"]
            .as_u64()
            .and_then(|weight| u16::try_from(weight).ok())
            .ok_or_else(|| format!("text.json: начертание стиля {style_id}"))?;
        let size = number(style, "size")?;
        let tracking_em = number(style, "tracking_em")?;
        for line in strings {
            let line_id = string(line, "id")?;
            let label = |id: String, pad: f64| -> Result<CalibrationText, String> {
                Ok(CalibrationText {
                    id,
                    mono,
                    weight,
                    size,
                    tracking_em,
                    text: string(line, "text")?.to_owned(),
                    pad,
                    x: 0.0,
                    y: 0.0,
                })
            };
            texts.push(label(format!("text.{style_id}.{line_id}"), 0.0)?);
            if padded.contains(&style_id) && string(line, "half")? == HOLDOUT {
                control.push(label(
                    format!("control.{PADDING_CONTROL}.{style_id}.{line_id}"),
                    PADDING_PT,
                )?);
            }
        }
    }
    texts.append(&mut control);
    Ok(texts)
}

fn list<'a>(value: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    value[key]
        .as_array()
        .ok_or_else(|| format!("text.json: нет списка {key}"))
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("text.json: нет строки {key}"))
}

fn number(value: &Value, key: &str) -> Result<f64, String> {
    value[key]
        .as_f64()
        .ok_or_else(|| format!("text.json: нет числа {key}"))
}

/// Ширина листа и его ячейки в порядке файла.
fn icons(svg: &str) -> Result<(f64, Vec<CalibrationIcon>), String> {
    let (root, mut rest) = svg
        .find('>')
        .map(|end| svg.split_at(end + 1))
        .ok_or("icons.svg: нет корня")?;
    let sheet_width = real(root, "width")?;
    let mut cells = Vec::new();
    while let Some(start) = rest.find("<svg ") {
        let open_end = rest[start..]
            .find('>')
            .map(|end| start + end + 1)
            .ok_or("icons.svg: ячейка не закрыта")?;
        let open = &rest[start..open_end];
        let close = rest[open_end..]
            .find("</svg>")
            .map(|end| open_end + end)
            .ok_or("icons.svg: ячейка не закрыта")?;
        let id = attr(open, "id").ok_or("icons.svg: ячейка без id")?;
        let view_box = attr(open, "viewBox")
            .and_then(|view| view.split(' ').nth(2))
            .and_then(|side| side.parse::<f64>().ok())
            .ok_or_else(|| format!("icons.svg: viewBox ячейки {id}"))?;
        cells.push(CalibrationIcon {
            id: format!("icon.{id}"),
            x: real(open, "x")?,
            y: real(open, "y")?,
            size: real(open, "width")?,
            view_box,
            stroke: real(open, "stroke-width")?,
            shapes: shapes(&rest[open_end..close])?,
        });
        rest = &rest[close + "</svg>".len()..];
    }
    Ok((sheet_width, cells))
}

fn shapes(mut body: &str) -> Result<Vec<CalibrationShape>, String> {
    let mut shapes = Vec::new();
    while let Some(start) = body.find('<') {
        let end = body[start..]
            .find("/>")
            .map(|end| start + end)
            .ok_or("icons.svg: фигура не закрыта")?;
        let tag = &body[start..end];
        let name = tag[1..].split(' ').next().unwrap_or_default();
        let figure = match name {
            "line" => CalibrationFigure::Line {
                from: (real(tag, "x1")?, real(tag, "y1")?),
                to: (real(tag, "x2")?, real(tag, "y2")?),
            },
            "circle" => CalibrationFigure::Circle {
                center: (real(tag, "cx")?, real(tag, "cy")?),
                radius: real(tag, "r")?,
            },
            "rect" => CalibrationFigure::RoundRect {
                x: real(tag, "x")?,
                y: real(tag, "y")?,
                width: real(tag, "width")?,
                height: real(tag, "height")?,
                radius: attr(tag, "rx").map_or(Ok(0.0), |_| real(tag, "rx"))?,
            },
            "path" => {
                CalibrationFigure::Path(path(attr(tag, "d").ok_or("icons.svg: контур без d")?)?)
            }
            other => return Err(format!("icons.svg: фигура {other} вне подмножества")),
        };
        shapes.push(CalibrationShape {
            figure,
            fill: attr(tag, "fill") == Some("currentColor"),
            secondary: attr(tag, "data-tone") == Some("secondary"),
        });
        body = &body[end + "/>".len()..];
    }
    Ok(shapes)
}

/// Контур из абсолютных `M`, `L` и `C`.
fn path(d: &str) -> Result<Vec<CalibrationOp>, String> {
    let mut ops = Vec::new();
    let mut rest = d.trim();
    while let Some(command) = rest.chars().next() {
        rest = &rest[command.len_utf8()..];
        let end = rest
            .find(|c: char| c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let numbers = rest[..end]
            .split([' ', ','])
            .filter(|number| !number.is_empty())
            .map(|number| {
                number
                    .parse::<f64>()
                    .map_err(|_| format!("icons.svg: число {number} в контуре"))
            })
            .collect::<Result<Vec<f64>, String>>()?;
        rest = &rest[end..];
        ops.push(match (command, numbers.as_slice()) {
            ('M', &[x, y]) => CalibrationOp::Move(x, y),
            ('L', &[x, y]) => CalibrationOp::Line(x, y),
            ('C', &[x1, y1, x2, y2, x, y]) => CalibrationOp::Cubic {
                c1: (x1, y1),
                c2: (x2, y2),
                to: (x, y),
            },
            _ => return Err(format!("icons.svg: команда {command} вне подмножества")),
        });
    }
    Ok(ops)
}

/// Значение атрибута `name` тега: имя ищется после пробела, поэтому `width` не находит
/// `stroke-width`.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')? + start;
    Some(&tag[start..end])
}

fn real(tag: &str, name: &str) -> Result<f64, String> {
    attr(tag, name)
        .and_then(|value| value.parse::<f64>().ok())
        .ok_or_else(|| format!("icons.svg: нет числа {name}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const WIDTH: f64 = 1100.0;

    fn icon<'a>(spec: &'a CalibrationSpec, id: &str) -> &'a CalibrationIcon {
        spec.icons.iter().find(|icon| icon.id == id).unwrap()
    }

    #[test]
    fn sheet_carries_every_case_of_the_corpus_and_the_padding_control() {
        let spec = sheet(WIDTH).unwrap();
        let corpus: Value = serde_json::from_str(TEXT).unwrap();
        let styles = corpus["styles"].as_array().unwrap().len();
        let strings = corpus["strings"].as_array().unwrap();
        let holdout = strings.iter().filter(|s| s["half"] == HOLDOUT).count();
        assert!(holdout > 0 && holdout < strings.len());
        let plain: Vec<_> = spec.texts.iter().filter(|t| t.pad == 0.0).collect();
        let padded: Vec<_> = spec.texts.iter().filter(|t| t.pad > 0.0).collect();
        assert_eq!(plain.len(), styles * strings.len());
        assert_eq!(padded.len(), 2 * holdout);
        assert!(plain.iter().all(|t| t.id.starts_with("text.")));
        for text in &padded {
            assert!((text.pad - 2.0).abs() < f64::EPSILON);
            assert!(
                text.id.starts_with("control.padding-2pt.ui-500-12.5.")
                    || text.id.starts_with("control.padding-2pt.mono-400-10.5."),
                "{}",
                text.id
            );
        }
        let ids: BTreeSet<&str> = spec
            .texts
            .iter()
            .map(|t| t.id.as_str())
            .chain(spec.icons.iter().map(|i| i.id.as_str()))
            .collect();
        assert_eq!(ids.len(), spec.texts.len() + spec.icons.len());
        let tracked = spec
            .texts
            .iter()
            .find(|t| t.id.starts_with("text.ui-700-11-track-0.08."))
            .unwrap();
        assert!(!tracked.mono && tracked.weight == 700);
        assert!((tracked.tracking_em - 0.08).abs() < f64::EPSILON);
        assert!(
            spec.texts
                .iter()
                .any(|t| t.mono && t.weight == 700 && (t.size - 11.0).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn icon_cells_keep_the_places_and_figures_of_the_sheet() {
        let spec = sheet(WIDTH).unwrap();
        assert_eq!(spec.icons.len(), 9 * 4);
        let line = icon(&spec, "icon.line-10");
        assert_eq!((line.x, line.y, line.size), (8.0, 8.0, 10.0));
        assert_eq!((line.view_box, line.stroke), (16.0, 1.4));
        assert_eq!(
            line.shapes,
            [CalibrationShape {
                figure: CalibrationFigure::Line {
                    from: (3.5, 12.5),
                    to: (12.5, 3.5)
                },
                fill: false,
                secondary: false,
            }]
        );
        let rrect = icon(&spec, "icon.rrect-16");
        assert_eq!((rrect.x, rrect.y, rrect.size), (80.0, 56.0, 16.0));
        assert_eq!(
            rrect.shapes[0].figure,
            CalibrationFigure::RoundRect {
                x: 2.5,
                y: 3.5,
                width: 11.0,
                height: 9.0,
                radius: 1.5
            }
        );
        assert!(icon(&spec, "icon.fill-13").shapes[0].fill);
        let tones = icon(&spec, "icon.two-tone-14");
        assert_eq!(
            tones
                .shapes
                .iter()
                .map(|s| (s.secondary, s.fill))
                .collect::<Vec<_>>(),
            [(true, false), (false, false), (false, true)]
        );
        assert!(icon(&spec, "icon.ctl-empty-10").shapes.is_empty());
        assert_eq!(icon(&spec, "icon.strokes-16").shapes.len(), 2);
        assert_eq!(icon(&spec, "icon.ctl-strokes-missing-16").shapes.len(), 1);
        for cell in &spec.icons {
            assert!(
                cell.x.fract() == 0.0 && cell.y.fract() == 0.0,
                "{}",
                cell.id
            );
            assert!(cell.x + cell.size <= 112.0);
        }
    }

    #[test]
    fn labels_flow_in_rows_right_of_the_icons_on_whole_points() {
        let spec = sheet(WIDTH).unwrap();
        let mut previous: Option<&CalibrationText> = None;
        for text in &spec.texts {
            assert!(text.x >= 128.0, "{} заходит на лист значков", text.id);
            assert!(text.x.fract() == 0.0 && text.y.fract() == 0.0);
            if let Some(previous) = previous {
                assert!(
                    text.y > previous.y || (text.y == previous.y && text.x > previous.x),
                    "{} не дальше {}",
                    text.id,
                    previous.id
                );
            }
            previous = Some(text);
        }
        assert_eq!((spec.texts[0].x, spec.texts[0].y), (128.0, 0.0));
        let narrow = sheet(128.0).unwrap();
        assert!(narrow.texts.iter().all(|text| text.x == 128.0));
    }

    #[test]
    fn path_takes_absolute_move_line_and_cubic_only() {
        assert_eq!(
            path("M2.5 11.5C5 3 11 3 13.5 11.5").unwrap(),
            [
                CalibrationOp::Move(2.5, 11.5),
                CalibrationOp::Cubic {
                    c1: (5.0, 3.0),
                    c2: (11.0, 3.0),
                    to: (13.5, 11.5)
                }
            ]
        );
        assert_eq!(
            path("M5 4.5L8.5 8L5 11.5").unwrap(),
            [
                CalibrationOp::Move(5.0, 4.5),
                CalibrationOp::Line(8.5, 8.0),
                CalibrationOp::Line(5.0, 11.5)
            ]
        );
        assert!(path("M9.5 11.5H13").is_err());
        assert!(path("m1 1l2 2").is_err());
        assert!(path("M1 2 3").is_err());
        assert!(shapes("<polygon points=\"1,2\"/>").is_err());
    }

    #[test]
    fn attribute_name_does_not_match_a_longer_name() {
        let tag = "<svg id=\"a\" stroke-width=\"1.4\" width=\"10\"";
        assert_eq!(attr(tag, "width"), Some("10"));
        assert_eq!(attr(tag, "stroke-width"), Some("1.4"));
        assert_eq!(attr(tag, "height"), None);
    }
}
