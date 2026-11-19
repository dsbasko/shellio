//! Оракул сценария `pairs`: у каждой строки показа «Рядом» обе половины из одной пары правила
//! плана, номера в гуттере совпадают с адресами сырого diff, строка слева с переносом на 3
//! визуальные строки и строка справа на 1 занимают одну высоту.
//!
//! Перенос посимвольный по клеткам (`model::wrap`), у ASCII без табуляции клетка — символ,
//! поэтому оракул считает визуальные строки как `⌈символов / колонок⌉`.

use std::fmt::Write as _;
use std::sync::Arc;

use probe_diff_view::fixtures::Fixture;
use probe_diff_view::model::{LineAddr, Mode, Side};

use super::contract::Raw;

/// Строк старой стороны малой фикстуры пары 3 и 1.
pub const PAIR31_LINES: u32 = 480;
/// Каждая такая строка изменена: длинная слева, короткая справа.
pub const PAIR31_EVERY: u32 = 8;
/// Длины изменённых строк слева идут по кругу от и до.
pub const PAIR31_MIN_CHARS: usize = 121;
pub const PAIR31_MAX_CHARS: usize = 480;
/// Длина строки справа.
pub const PAIR31_NEW_CHARS: usize = 24;

/// Визуальных строк у текста из `cells` клеток при `columns` колонках, не меньше одной.
pub fn visual_lines(cells: usize, columns: u32) -> u32 {
    let columns = columns.max(1) as usize;
    cells.div_ceil(columns).max(1) as u32
}

/// Номера в гуттере строки показа, которые вид нарисовал: старый и новый.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Gutter {
    pub old: Option<u32>,
    pub new: Option<u32>,
}

/// Номера из нарисованного текста гуттера (формат `paint::gutter_text`): у unified две
/// колонки номеров, у половины «Рядом» одна — номер её стороны. `None` — текст не по формату.
pub fn parse_gutter(text: &str, half: Option<Side>) -> Option<Gutter> {
    let chars: Vec<char> = text.chars().collect();
    let column = |from: usize, digits: usize| -> Option<Option<u32>> {
        let value: String = chars.get(from..from + digits)?.iter().collect();
        let value = value.trim();
        if value.is_empty() {
            Some(None)
        } else {
            value.parse::<u32>().ok().map(Some)
        }
    };
    match half {
        None => {
            let digits = (chars.len().checked_sub(2)? / 2).checked_sub(1)?;
            Some(Gutter {
                old: column(0, digits)?,
                new: column(digits + 1, digits)?,
            })
        }
        Some(side) => {
            let number = column(0, chars.len().checked_sub(3)?)?;
            Some(match side {
                Side::Old => Gutter {
                    old: number,
                    new: None,
                },
                Side::New => Gutter {
                    old: None,
                    new: number,
                },
            })
        }
    }
}

/// Номера, которых требует сырой diff у строки показа `row`.
pub fn expected_gutter(raw: &Raw, mode: Mode, row: usize) -> Option<Gutter> {
    let addrs = raw.row_addrs(mode, row);
    if addrs.is_empty() {
        return None;
    }
    let pick = |side: Side| {
        addrs
            .iter()
            .find(|addr| addr.side == side)
            .map(|addr| addr.line)
    };
    Some(Gutter {
        old: pick(Side::Old),
        new: pick(Side::New),
    })
}

/// Половины строки показа «Рядом» из модели против пары сырого diff: адреса старой и новой
/// половины.
pub fn pair_matches(
    raw: &Raw,
    row: usize,
    old: Option<LineAddr>,
    new: Option<LineAddr>,
) -> Result<(), String> {
    let pair = raw
        .pairs
        .get(row)
        .ok_or_else(|| format!("строки {row} нет среди пар сырого diff"))?;
    let want =
        |index: Option<usize>, side: Side| index.and_then(|index| raw.lines[index].addr(side));
    let (want_old, want_new) = (want(pair.old, Side::Old), want(pair.new, Side::New));
    if (want_old, want_new) == (old, new) {
        Ok(())
    } else {
        Err(format!(
            "строка {row}: половины модели {old:?} и {new:?}, пара сырого diff {want_old:?} и {want_new:?}"
        ))
    }
}

/// Адреса строки показа из модели против сырого diff: в «Рядом» — пара правила плана, в
/// unified — адреса строки diff.
pub fn model_matches(
    raw: &Raw,
    mode: Mode,
    row: usize,
    old: Option<LineAddr>,
    new: Option<LineAddr>,
) -> Result<(), String> {
    if mode == Mode::SideBySide {
        return pair_matches(raw, row, old, new);
    }
    let want = expected_gutter(raw, mode, row)
        .ok_or_else(|| format!("строки {row} нет среди строк сырого diff"))?;
    let addr = |side, line: Option<u32>| line.map(|line| LineAddr { side, line });
    let (want_old, want_new) = (addr(Side::Old, want.old), addr(Side::New, want.new));
    if (want_old, want_new) == (old, new) {
        Ok(())
    } else {
        Err(format!(
            "строка {row}: адреса модели {old:?} и {new:?}, строка сырого diff {want_old:?} и {want_new:?}"
        ))
    }
}

/// Номера, которые показывает половина `half` строки с номерами `expected`: unified — обе
/// колонки, половина «Рядом» — номер своей стороны.
pub fn half_gutter(expected: Gutter, half: Option<Side>) -> Gutter {
    match half {
        None => expected,
        Some(Side::Old) => Gutter {
            old: expected.old,
            new: None,
        },
        Some(Side::New) => Gutter {
            old: None,
            new: expected.new,
        },
    }
}

/// Нарисованный гуттер половины `half` строки `row` против номеров `want`. `drawn` — поколение
/// и текст из журнала, `generation` — требуемое поколение строки в учёте покрытия. Пустая
/// половина «Рядом» гуттер не рисует.
pub fn gutter_matches(
    row: u32,
    half: Option<Side>,
    want: Gutter,
    drawn: Option<(u64, &str)>,
    generation: u64,
) -> Result<(), String> {
    let Some((drawn_generation, text)) = drawn else {
        return if want == Gutter::default() {
            Ok(())
        } else {
            Err(format!(
                "строка {row} {half:?}: номера {want:?} не нарисованы"
            ))
        };
    };
    if drawn_generation < generation {
        return Err(format!(
            "строка {row} {half:?}: гуттер отрисован поколением {drawn_generation}, требуется \
             {generation}"
        ));
    }
    match parse_gutter(text, half) {
        Some(got) if got == want => Ok(()),
        Some(got) => Err(format!(
            "строка {row} {half:?}: нарисовано {got:?} ({text:?}), адреса {want:?}"
        )),
        None => Err(format!(
            "строка {row} {half:?}: текст гуттера {text:?} не по формату"
        )),
    }
}

/// Нарисованная высота строки пары 3 и 1 против 3 высот визуальной строки.
pub fn height31_matches(row: u32, drawn: f64, line_height: u32) -> Result<(), String> {
    let want = 3.0 * f64::from(line_height);
    if (drawn - want).abs() <= HEIGHT_TOLERANCE {
        Ok(())
    } else {
        Err(format!(
            "строка {row}: нарисована высота {drawn:.1}, ожидалось 3 × {line_height} = {want:.1}"
        ))
    }
}

/// Допуск высоты нарисованной строки, точки: рамки AppKit идут в дробных точках.
pub const HEIGHT_TOLERANCE: f64 = 0.5;

/// Строка символов длины `chars`, похожая на код: без табуляции и широких символов.
fn code_line(seed: u32, chars: usize) -> String {
    let mut line = format!("let value_{seed} = ");
    let mut term = 0u32;
    while line.len() < chars {
        let _ = write!(line, "a{seed}_{term} + ");
        term += 1;
    }
    line.truncate(chars);
    line
}

/// Длина `i`-й изменённой строки слева.
fn pair31_chars(index: u32) -> usize {
    let span = PAIR31_MAX_CHARS - PAIR31_MIN_CHARS + 1;
    PAIR31_MIN_CHARS + (index as usize * 7) % span
}

/// Малая фикстура пары 3 и 1: каждая `PAIR31_EVERY`-я строка заменена, слева длинная строка
/// длиной от `PAIR31_MIN_CHARS` до `PAIR31_MAX_CHARS`, справа короткая. Diff одним ханком на
/// весь файл (контекст 3 при промежутке 7 ханки сливает).
pub fn pair31() -> Fixture {
    let path = "src/pair31.rs".to_owned();
    let mut old = String::new();
    let mut new = String::new();
    let mut body = String::new();
    for line in 0..PAIR31_LINES {
        let number = line + 1;
        if number % PAIR31_EVERY == 0 {
            let left = code_line(number, pair31_chars(number / PAIR31_EVERY));
            let right = code_line(number, PAIR31_NEW_CHARS);
            let _ = writeln!(old, "{left}");
            let _ = writeln!(new, "{right}");
            let _ = writeln!(body, "-{left}");
            let _ = writeln!(body, "+{right}");
        } else {
            let text = format!("    // context {number}");
            let _ = writeln!(old, "{text}");
            let _ = writeln!(new, "{text}");
            let _ = writeln!(body, " {text}");
        }
    }
    let diff = format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1,{PAIR31_LINES} +1,{PAIR31_LINES} @@\n{body}"
    );
    Fixture {
        name: "pair31",
        path,
        diff: Arc::from(diff.into_bytes()),
        old: Arc::from(old.into_bytes()),
        new: Arc::from(new.into_bytes()),
        threads: Vec::new(),
    }
}

/// Пары «Рядом», у которых старая половина переносится на 3 визуальные строки, а новая на 1
/// при `columns` колонках текста половины. Длины берутся из байтов сторон: ASCII.
pub fn rows_3_and_1(raw: &Raw, old: &[u8], new: &[u8], columns: u32) -> Vec<usize> {
    let text = |bytes: &[u8]| -> Vec<usize> {
        std::str::from_utf8(bytes)
            .map(|text| text.lines().map(str::len).collect())
            .unwrap_or_default()
    };
    let (old_lens, new_lens) = (text(old), text(new));
    let len = |lens: &[usize], line: Option<u32>| {
        line.and_then(|line| lens.get(line.checked_sub(1)? as usize).copied())
    };
    raw.pairs
        .iter()
        .enumerate()
        .filter(|(_, pair)| {
            let (Some(left), Some(right)) = (pair.old, pair.new) else {
                return false;
            };
            if left == right {
                return false;
            }
            let left = len(&old_lens, raw.lines[left].old);
            let right = len(&new_lens, raw.lines[right].new);
            matches!((left, right), (Some(left), Some(right))
                if visual_lines(left, columns) == 3 && visual_lines(right, columns) == 1)
        })
        .map(|(row, _)| row)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drawn_gutter_text_parses_back_to_numbers() {
        use crate::paint::gutter_text;
        use probe_diff_view::model::LineKind;
        use probe_diff_view::model::layout::Gutter as Columns;
        let both = gutter_text(
            Columns::Both,
            16,
            Some(12),
            Some(345_678),
            LineKind::Context,
        );
        assert_eq!(
            parse_gutter(&both, None),
            Some(Gutter {
                old: Some(12),
                new: Some(345_678)
            })
        );
        let added = gutter_text(Columns::Both, 16, None, Some(7), LineKind::Added);
        assert_eq!(
            parse_gutter(&added, None),
            Some(Gutter {
                old: None,
                new: Some(7)
            })
        );
        let half = gutter_text(Columns::Single, 9, Some(9), None, LineKind::Removed);
        assert_eq!(
            parse_gutter(&half, Some(Side::Old)),
            Some(Gutter {
                old: Some(9),
                new: None
            })
        );
        assert_eq!(parse_gutter("   1x2 345678   ", None), None);
    }

    #[test]
    fn visual_lines_round_up_and_keep_one_for_empty() {
        assert_eq!(visual_lines(0, 70), 1);
        assert_eq!(visual_lines(70, 70), 1);
        assert_eq!(visual_lines(71, 70), 2);
        assert_eq!(visual_lines(210, 70), 3);
        assert_eq!(visual_lines(211, 70), 4);
    }

    #[test]
    fn pair31_fixture_parses_and_has_3_and_1_rows_for_plausible_columns() {
        let fixture = pair31();
        let raw = Raw::parse(&fixture.diff).expect("разбор");
        assert_eq!(raw.pairs.len(), PAIR31_LINES as usize);
        for columns in 60..=150 {
            let rows = rows_3_and_1(&raw, &fixture.old, &fixture.new, columns);
            assert!(
                rows.len() >= 3,
                "колонок {columns}: строк 3 и 1 {}",
                rows.len()
            );
        }
    }

    #[test]
    fn model_addresses_follow_the_raw_diff_in_both_modes() {
        let fixture = pair31();
        let raw = Raw::parse(&fixture.diff).expect("разбор");
        let at = |side, line| Some(LineAddr { side, line });
        assert!(model_matches(&raw, Mode::Unified, 0, at(Side::Old, 1), at(Side::New, 1)).is_ok());
        let removed = (PAIR31_EVERY - 1) as usize;
        assert!(
            model_matches(
                &raw,
                Mode::Unified,
                removed,
                at(Side::Old, PAIR31_EVERY),
                None
            )
            .is_ok()
        );
        assert!(
            model_matches(
                &raw,
                Mode::Unified,
                removed + 1,
                None,
                at(Side::New, PAIR31_EVERY)
            )
            .is_ok()
        );
        assert!(
            model_matches(
                &raw,
                Mode::Unified,
                removed,
                at(Side::Old, PAIR31_EVERY),
                at(Side::New, PAIR31_EVERY)
            )
            .is_err()
        );
        assert!(
            model_matches(
                &raw,
                Mode::SideBySide,
                removed,
                at(Side::Old, PAIR31_EVERY),
                at(Side::New, PAIR31_EVERY)
            )
            .is_ok()
        );
        assert!(model_matches(&raw, Mode::Unified, usize::MAX, None, None).is_err());
    }

    #[test]
    fn drawn_gutter_is_checked_per_half_and_generation() {
        use crate::paint::gutter_text;
        use probe_diff_view::model::LineKind;
        use probe_diff_view::model::layout::Gutter as Columns;
        let both = Gutter {
            old: Some(12),
            new: Some(14),
        };
        assert_eq!(
            half_gutter(both, Some(Side::Old)),
            Gutter {
                old: Some(12),
                new: None
            }
        );
        assert_eq!(half_gutter(both, None), both);
        let text = gutter_text(Columns::Both, 8, Some(12), Some(14), LineKind::Context);
        assert!(gutter_matches(3, None, both, Some((5, &text)), 5).is_ok());
        let stale = gutter_matches(3, None, both, Some((4, &text)), 5).unwrap_err();
        assert!(stale.contains("поколением 4"), "{stale}");
        let other = Gutter {
            old: Some(12),
            new: Some(15),
        };
        assert!(gutter_matches(3, None, other, Some((5, &text)), 5).is_err());
        assert!(gutter_matches(3, Some(Side::Old), Gutter::default(), None, 5).is_ok());
        assert!(gutter_matches(3, None, both, None, 5).is_err());
        assert!(gutter_matches(3, None, both, Some((5, "garbage")), 5).is_err());
    }

    #[test]
    fn height31_allows_half_a_point() {
        assert!(height31_matches(1, 48.0, 16).is_ok());
        assert!(height31_matches(1, 48.5, 16).is_ok());
        assert!(height31_matches(1, 49.0, 16).is_err());
        assert!(height31_matches(1, 16.0, 16).is_err());
    }

    #[test]
    fn gutter_and_pairs_follow_the_raw_diff() {
        let fixture = pair31();
        let raw = Raw::parse(&fixture.diff).expect("разбор");
        let row = (PAIR31_EVERY - 1) as usize;
        let gutter = expected_gutter(&raw, Mode::SideBySide, row).expect("строка");
        assert_eq!(
            gutter,
            Gutter {
                old: Some(PAIR31_EVERY),
                new: Some(PAIR31_EVERY)
            }
        );
        let old = Some(LineAddr {
            side: Side::Old,
            line: PAIR31_EVERY,
        });
        let new = Some(LineAddr {
            side: Side::New,
            line: PAIR31_EVERY,
        });
        assert!(pair_matches(&raw, row, old, new).is_ok());
        assert!(pair_matches(&raw, row, new, old).is_err());
        assert!(pair_matches(&raw, row + 1, old, new).is_err());
        let unified = expected_gutter(&raw, Mode::Unified, row).expect("строка");
        assert_eq!(
            unified,
            Gutter {
                old: Some(PAIR31_EVERY),
                new: None
            }
        );
    }
}
