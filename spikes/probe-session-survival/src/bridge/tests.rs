use super::*;
use crate::oracle::model::{Color, Rgb, Screen, Style, Width};

fn text_cell(text: &str) -> Cell {
    Cell {
        text: text.to_owned(),
        ..Cell::blank()
    }
}

fn row_of(text: &str, cols: u16) -> Row {
    let mut row = Row::blank(cols);
    for (x, ch) in text.chars().enumerate() {
        row.cells[x] = text_cell(&ch.to_string());
    }
    row
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn position(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[test]
fn blank_state_encodes_without_content_or_report_modes() {
    let state = State::blank(10, 3);
    let bytes = encode(&state, ReportPolicy::Canonical);
    let text = String::from_utf8_lossy(&bytes);
    assert!(!text.contains("1004"), "{text}");
    assert!(!text.contains("2048"), "{text}");
    assert!(text.contains("\x1b[?7;25h") || text.contains(";7;") || text.contains("?7"));
}

#[test]
fn never_saved_cursor_is_not_saved_and_a_saved_one_is() {
    let mut state = State::blank(10, 3);
    assert!(!contains(
        &encode(&state, ReportPolicy::Canonical),
        b"\x1b7"
    ));
    state.primary.saved.x = 4;
    assert!(contains(&encode(&state, ReportPolicy::Canonical), b"\x1b7"));
}

#[test]
fn soft_wrapped_rows_are_printed_through_the_edge_without_crlf() {
    let cols = 4;
    let mut state = State::blank(cols, 2);
    state.primary.active[0] = row_of("abcd", cols);
    state.primary.active[0].wrap = true;
    state.primary.active[1] = row_of("ef", cols);
    state.primary.active[1].wrap_continuation = true;
    let bytes = encode(&state, ReportPolicy::Canonical);
    assert!(
        contains(&bytes, b"abcdef"),
        "{:?}",
        String::from_utf8_lossy(&bytes)
    );
}

#[test]
fn hard_rows_end_with_crlf_and_empty_cells_are_skipped_with_cuf() {
    let cols = 8;
    let mut state = State::blank(cols, 2);
    let mut row = row_of("a", cols);
    row.cells[4] = text_cell("b");
    row.cells[2] = text_cell(" ");
    state.primary.active[0] = row;
    state.primary.active[1] = row_of("c", cols);
    let bytes = encode(&state, ReportPolicy::Canonical);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("a\x1b[1C \x1b[1Cb\r\nc"), "{text:?}");
}

#[test]
fn history_rows_precede_active_rows() {
    let cols = 4;
    let mut state = State::blank(cols, 1);
    state.primary.history = vec![row_of("h0", cols), row_of("h1", cols)];
    state.primary.active = vec![row_of("a", cols)];
    let bytes = encode(&state, ReportPolicy::Canonical);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("h0\r\nh1\r\na"), "{text:?}");
}

#[test]
fn spacer_head_wraps_by_printing_the_wide_char_at_the_last_column() {
    let cols = 3;
    let mut state = State::blank(cols, 2);
    let mut first = row_of("ab", cols);
    first.cells[2].width = Width::SpacerHead;
    first.wrap = true;
    let mut second = Row::blank(cols);
    second.cells[0] = Cell {
        width: Width::Wide,
        ..text_cell("世")
    };
    second.cells[1].width = Width::SpacerTail;
    second.wrap_continuation = true;
    state.primary.active = vec![first, second];
    let bytes = encode(&state, ReportPolicy::Canonical);
    assert!(
        contains(&bytes, "ab世".as_bytes()),
        "{:?}",
        String::from_utf8_lossy(&bytes)
    );
}

#[test]
fn styled_cells_get_sgr_and_hyperlinks_open_and_close() {
    let cols = 4;
    let mut state = State::blank(cols, 1);
    let mut row = Row::blank(cols);
    row.cells[0] = Cell {
        style: Style {
            bold: true,
            fg: Color::Palette(3),
            underline: 3,
            underline_color: Color::Rgb(Rgb { r: 1, g: 2, b: 3 }),
            ..Style::default()
        },
        hyperlink: Some("https://x.test".to_owned()),
        ..text_cell("L")
    };
    row.cells[1] = text_cell("p");
    state.primary.active = vec![row];
    let bytes = encode(&state, ReportPolicy::Canonical);
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("\x1b[0;1;4:3;38;5;3;58;2;1;2;3m\x1b]8;;https://x.test\x1b\\\x1b[0\"qL"),
        "{text:?}"
    );
    assert!(text.contains("\x1b[0m\x1b]8;;\x1b\\p"), "{text:?}");
}

#[test]
fn bg_only_cells_are_erased_with_background_not_printed() {
    let cols = 4;
    let mut state = State::blank(cols, 1);
    state.primary.active[0].cells[1].bg_only = Some(Color::Palette(4));
    let bytes = encode(&state, ReportPolicy::Canonical);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\x1b[1C\x1b[0;48;5;4m"), "{text:?}");
    assert!(text.contains("\x1b[X"), "{text:?}");
}

#[test]
fn kitty_ring_is_pushed_from_deepest_to_current() {
    let mut state = State::blank(4, 1);
    state.primary.kitty = vec![3, 1, 0, 0, 0, 0, 0, 0];
    let bytes = encode(&state, ReportPolicy::Canonical);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\x1b[>1u\x1b[>3u"), "{text:?}");
    assert!(!text.contains("\x1b[="), "{text:?}");

    state.primary.kitty = vec![1, 2, 3, 4, 5, 6, 7, 9];
    let bytes = encode(&state, ReportPolicy::Canonical);
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("\x1b[=9;1u\x1b[>7u\x1b[>6u\x1b[>5u\x1b[>4u\x1b[>3u\x1b[>2u\x1b[>1u"),
        "{text:?}"
    );
}

#[test]
fn alternate_screen_follows_primary_and_1049_saves_the_primary_saved_cursor() {
    let cols = 6;
    let mut state = State::blank(cols, 2);
    state.screen = Screen::Alternate;
    state.modes.insert("?1049".to_owned(), true);
    state.primary.active[0] = row_of("main", cols);
    state.primary.saved = SavedCursor {
        x: 2,
        y: 1,
        ..SavedCursor::default()
    };
    state.alternate.active[1] = row_of("alt", cols);
    let bytes = encode(&state, ReportPolicy::Canonical);
    let main = position(&bytes, b"main").unwrap();
    let saved = position(&bytes, b"\x1b[2;3H").unwrap();
    let entry = position(&bytes, b"\x1b[?1049h").unwrap();
    let row = entry + position(&bytes[entry..], b"\x1b[2H").unwrap();
    let alt = position(&bytes, b"alt").unwrap();
    assert!(main < saved && saved < entry && entry < row && row < alt);
}

#[test]
fn inactive_alternate_is_written_through_mode_47_before_primary() {
    let cols = 6;
    let mut state = State::blank(cols, 2);
    state.alternate.active[0] = row_of("stale", cols);
    state.primary.active[0] = row_of("main", cols);
    let bytes = encode(&state, ReportPolicy::Canonical);
    let enter = position(&bytes, b"\x1b[?47h").unwrap();
    let stale = position(&bytes, b"stale").unwrap();
    let leave = position(&bytes, b"\x1b[?47l").unwrap();
    let main = position(&bytes, b"main").unwrap();
    assert!(enter < stale && stale < leave && leave < main);
}

#[test]
fn pending_wrap_reprints_the_cell_under_the_cursor_then_sets_the_pen() {
    let cols = 4;
    let mut state = State::blank(cols, 1);
    state.primary.active[0] = row_of("wxyz", cols);
    state.primary.cursor = Cursor {
        x: 3,
        y: 0,
        pending_wrap: true,
        pen: Pen {
            style: Style {
                italic: true,
                ..Style::default()
            },
            ..Pen::default()
        },
    };
    let bytes = encode(&state, ReportPolicy::Canonical);
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("\x1b[1;4Hz\x1b[0;3m"), "{text:?}");
}

#[test]
fn margins_tabstops_and_origin_follow_content() {
    let cols = 20;
    let mut state = State::blank(cols, 10);
    state.primary.active[0] = row_of("x", cols);
    state.tabstops = vec![4, 16];
    state.margins = Margins {
        top: 2,
        bottom: 7,
        left: 0,
        right: cols - 1,
    };
    state.modes.insert("?6".to_owned(), true);
    state.primary.cursor.y = 3;
    let bytes = encode(&state, ReportPolicy::Canonical);
    let text = String::from_utf8_lossy(&bytes);
    let content = text.find('x').unwrap();
    let tabs = text.find("\x1b[3g\x1b[5G\x1bH\x1b[17G\x1bH").unwrap();
    let margins = text.find("\x1b[3;8r").unwrap();
    let origin = text.rfind("\x1b[?6h\x1b[2;1H").unwrap();
    assert!(
        content < tabs && tabs < margins && margins < origin,
        "{text:?}"
    );
}

#[test]
fn mouse_modes_reset_first_and_current_event_is_set_last() {
    let mut state = State::blank(4, 1);
    state.modes.insert("?1000".to_owned(), true);
    state.modes.insert("?1003".to_owned(), true);
    state.modes.insert("?1006".to_owned(), true);
    state.mouse_event = MouseEvent::Normal;
    state.mouse_format = MouseFormat::Sgr;
    let bytes = encode(&state, ReportPolicy::Canonical);
    let text = String::from_utf8_lossy(&bytes);
    let resets = text.find("l\x1b[").unwrap();
    let sets = text.find(";1003;1000;1006h").unwrap();
    assert!(resets < sets, "{text:?}");
}

#[test]
fn report_modes_follow_the_policy() {
    let mut state = State::blank(4, 1);
    state.modes.insert("?1004".to_owned(), true);
    state.modes.insert("?2004".to_owned(), true);
    let canonical = String::from_utf8_lossy(&encode(&state, ReportPolicy::Canonical)).into_owned();
    assert!(!canonical.contains("1004"), "{canonical:?}");
    assert!(canonical.contains("2004"), "{canonical:?}");
    let passthrough =
        String::from_utf8_lossy(&encode(&state, ReportPolicy::Passthrough)).into_owned();
    assert!(passthrough.contains("\x1b[?1004h"), "{passthrough:?}");
}

#[test]
fn continuation_is_the_last_bytes() {
    let mut state = State::blank(4, 1);
    state.continuation = b"\x1b]2;tit".to_vec();
    state.title = "old".to_owned();
    state.modes.insert("?2026".to_owned(), true);
    let bytes = encode(&state, ReportPolicy::Canonical);
    assert!(bytes.ends_with(b"\x1b[?2026h\x1b]2;tit"));
}

#[test]
fn colors_title_and_pwd_are_osc_sequences() {
    let mut state = State::blank(4, 1);
    state.palette = vec![(
        1,
        Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56,
        },
    )];
    state.background = Some(Rgb {
        r: 0,
        g: 0,
        b: 0xff,
    });
    state.title = "t".to_owned();
    state.pwd = "file://h/tmp".to_owned();
    let text = String::from_utf8_lossy(&encode(&state, ReportPolicy::Canonical)).into_owned();
    assert!(text.contains("\x1b]4;1;rgb:12/34/56\x1b\\"), "{text:?}");
    assert!(text.contains("\x1b]11;rgb:00/00/ff\x1b\\"), "{text:?}");
    assert!(
        text.contains("\x1b]2;t\x1b\\\x1b]7;file://h/tmp\x1b\\"),
        "{text:?}"
    );
}

#[test]
fn default_tabstops_every_eight_columns_inside_the_row() {
    assert_eq!(default_tabstops(20), vec![8, 16]);
    assert_eq!(default_tabstops(17), vec![8]);
}

// --- фильтр ---

fn filtered(chunks: &[&[u8]]) -> (Vec<u8>, ReportFilter) {
    let mut filter = ReportFilter::new();
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(filter.push(chunk));
    }
    (out, filter)
}

#[test]
fn filter_removes_report_modes_and_keeps_the_rest_of_a_mixed_csi() {
    let (out, filter) = filtered(&[b"a\x1b[?1004;2004hb"]);
    assert_eq!(out, b"a\x1b[?2004hb");
    assert_eq!(filter.canonical(1004), Some(true));
    assert_eq!(filter.removed(), 1);
}

#[test]
fn filter_drops_a_csi_with_only_report_modes() {
    let (out, filter) = filtered(&[b"\x1b[?2048;1004h\x1b[?2048l"]);
    assert_eq!(out, b"");
    assert_eq!(filter.canonical(2048), Some(false));
    assert_eq!(filter.canonical(1004), Some(true));
}

#[test]
fn filter_gives_the_same_output_for_every_byte_split() {
    let stream: &[u8] =
        b"x\x1b[?1004;2004h\x1b]2;?1004h\x07\x1b[?2031h\x1b[?25l\x1b[1;2H\x1b[?2033;6l";
    let (whole, whole_filter) = filtered(&[stream]);
    assert_eq!(
        whole,
        b"x\x1b[?2004h\x1b]2;?1004h\x07\x1b[?25l\x1b[1;2H\x1b[?6l".to_vec()
    );
    for cut in 0..=stream.len() {
        let (out, filter) = filtered(&[&stream[..cut], &stream[cut..]]);
        assert_eq!(out, whole, "разрез {cut}");
        for mode in REPORT_MODES {
            assert_eq!(filter.canonical(mode), whole_filter.canonical(mode));
        }
    }
    for cut in 0..stream.len() {
        for second in cut..stream.len() {
            let (out, _) = filtered(&[&stream[..cut], &stream[cut..second], &stream[second..]]);
            assert_eq!(out, whole, "разрезы {cut} и {second}");
        }
    }
}

#[test]
fn filter_holds_an_unfinished_csi_as_continuation() {
    let mut filter = ReportFilter::new();
    assert_eq!(filter.push(b"a\x1b[?10"), b"a");
    assert_eq!(filter.held(), b"\x1b[?10");
    assert_eq!(filter.push(b"04;1h"), b"\x1b[?1h");
    assert_eq!(filter.held(), b"");
    assert_eq!(filter.canonical(1004), Some(true));
}

#[test]
fn filter_save_and_restore_track_canonical_values() {
    let (out, filter) = filtered(&[b"\x1b[?1004h\x1b[?1004;1s\x1b[?1004l\x1b[?1004;1r"]);
    assert_eq!(out, b"\x1b[?1s\x1b[?1r");
    assert_eq!(filter.canonical(1004), Some(true));
}

#[test]
fn filter_reset_turns_report_modes_off_and_passes_ris() {
    let (out, filter) = filtered(&[b"\x1b[?2048h\x1bc"]);
    assert_eq!(out, b"\x1bc");
    assert_eq!(filter.canonical(2048), Some(false));
}

#[test]
fn filter_leaves_queries_other_csi_and_strings_untouched() {
    let stream: &[u8] =
        b"\x1b[?1004$p\x1b[>1004h\x1b[1004h\x1b[?1004:1h\x1bP$q?1004h\x1b\\\x1b[3;5r\x1b[s";
    let (out, filter) = filtered(&[stream]);
    assert_eq!(out, stream);
    assert_eq!(filter.canonical(1004), Some(false));
}

#[test]
fn filter_keeps_controls_inside_a_rewritten_csi() {
    let (out, _) = filtered(&[b"\x1b[?10\r04;7h"]);
    assert_eq!(out, b"\r\x1b[?7h");
}

#[test]
fn filter_cancel_and_new_escape_abort_the_held_csi() {
    let (out, filter) = filtered(&[b"\x1b[?1004\x18\x1b[?2048\x1b[?1004h"]);
    assert_eq!(out, b"\x1b[?1004\x18\x1b[?2048");
    assert_eq!(filter.canonical(1004), Some(true));
    assert_eq!(filter.canonical(2048), Some(false));
}

#[test]
fn filter_with_canonical_starts_from_restored_modes() {
    let mut modes = BTreeMap::new();
    modes.insert("?2048".to_owned(), true);
    let filter = ReportFilter::with_canonical(&modes);
    assert_eq!(filter.canonical(2048), Some(true));
    assert_eq!(filter.canonical(1004), Some(false));
}
