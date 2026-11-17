use super::*;
use crate::bridge::ReportFilter;

#[test]
fn marker_is_found_bracketed_first_then_bare() {
    let marker = Stage::Live.marker();
    let mut buffer = b"\x1b[I".to_vec();
    buffer.extend_from_slice(b"\x1b[200~");
    buffer.extend_from_slice(marker.as_bytes());
    buffer.extend_from_slice(b"\x1b[201~tail");
    let fence = split_at_marker(&buffer, &marker).unwrap();
    assert_eq!(fence.before, b"\x1b[I");
    assert!(fence.bracketed);
    assert_eq!(&buffer[fence.end..], b"tail");

    let bare = format!("x{marker}y");
    let fence = split_at_marker(bare.as_bytes(), &marker).unwrap();
    assert_eq!(fence.before, b"x");
    assert!(!fence.bracketed);
    assert_eq!(&bare.as_bytes()[fence.end..], b"y");

    assert_eq!(split_at_marker(b"pss-mark-liv", &marker), None);
}

#[test]
fn unexpected_removes_each_allowed_token_once() {
    let before = b"\x1b[5;16R\x1bOA\x1b[O\x1bOA".to_vec();
    let rest = unexpected(&before, &[b"\x1b[5;16R".to_vec(), b"\x1bOA".to_vec()]);
    assert_eq!(rest, b"\x1b[O\x1bOA");
    assert!(unexpected(b"\x1b[5;16R", &[b"\x1b[5;16R".to_vec()]).is_empty());
}

#[test]
fn report_kinds_name_surface_reports() {
    let bytes = b"\x1b[I\x1b[O\x1b[48;24;80;480;800t\x1b[?999;2n\x1b[?997;1n\x1b[5;1Rzz";
    assert_eq!(
        report_kinds(bytes),
        [
            "focus-in",
            "focus-out",
            "size",
            "visibility",
            "scheme",
            "other:1b5b353b3152",
            "other:7a7a",
        ]
    );
    assert!(report_kinds(b"").is_empty());
    assert_eq!(report_kinds(b"\x1bO"), ["other:1b", "other:4f"]);
}

#[test]
fn cpr_parses_row_and_column() {
    assert_eq!(parse_cpr(b"\x1b[5;16R"), Some((5, 16)));
    assert_eq!(parse_cpr(b"\x1b[5R"), None);
    assert_eq!(parse_cpr(b"\x1b[?5;16R"), None);
}

#[test]
fn private_modes_scan_mixed_sequences() {
    let bytes = b"ab\x1b[?1004;2004h\x1b[?2048l\x1b[?2031s\x1b[1;2H\x1b[?25$p\x1b[?1h";
    assert_eq!(
        private_modes(bytes),
        [
            (1004, b'h'),
            (2004, b'h'),
            (2048, b'l'),
            (2031, b's'),
            (1, b'h')
        ]
    );
    assert_eq!(
        leaked_report_modes(bytes),
        [(1004, b'h'), (2048, b'l'), (2031, b's')]
    );
    assert!(sets_mode(bytes, 2004));
    assert!(sets_mode(bytes, 1));
    assert!(!sets_mode(b"\x1b[?2004h\x1b[?2004l", 2004));
    assert!(!sets_mode(bytes, 2048));
}

#[test]
fn filter_output_of_live_stream_is_clean_and_keeps_other_modes() {
    let mut filter = ReportFilter::new();
    let out = filter.push(LIVE.as_bytes());
    assert!(leaked_report_modes(&out).is_empty(), "{out:?}");
    for mode in KEPT_MODES {
        assert!(sets_mode(&out, mode), "режим {mode} потерян: {out:?}");
    }
    assert!(out.ends_with(b"@"));
    assert!(!leaked_report_modes(LIVE.as_bytes()).is_empty());
}

#[test]
fn scene_sources_cover_policy_cases() {
    let separate = SCENES[0].source.as_bytes();
    let mixed = SCENES[1].source.as_bytes();
    for mode in [1004, 2048, 2033, 2031] {
        assert!(sets_mode(separate, mode), "separate: {mode}");
    }
    for mode in KEPT_MODES {
        assert!(
            sets_mode(separate, mode) && sets_mode(mixed, mode),
            "{mode}"
        );
    }
    assert!(
        private_modes(mixed)
            .iter()
            .any(|(mode, final_byte)| *mode == 1004 && *final_byte == b's')
    );
    assert!(!sets_mode(mixed, 2048), "2048 в mixed выключен к концу");
}

#[test]
fn rows_normalize_and_mark_cursor() {
    let rows = text_rows("a  \nкрасный текст\n\n\n");
    assert_eq!(rows, ["a", "красный текст"]);
    assert_eq!(
        with_mark(&rows, 2, 4, CURSOR_MARK),
        ["a", "красный текст", "", "", "  @"]
    );
    assert_eq!(with_mark(&rows, 1, 1, '@'), ["a", "к@асный текст"]);
    assert_eq!(normalize_rows(&["", ""]), Vec::<String>::new());
}

#[test]
fn causes_follow_emitted_bytes() {
    assert_eq!(report_cause(b"\x1b[?2004h@"), Cause::SurfaceEffect);
    assert_eq!(report_cause(b"\x1b[?2004;1004h"), Cause::Filter);
    assert_eq!(kept_cause(b"\x1b[?2004h", 2004), Cause::Bytes);
    assert_eq!(kept_cause(b"\x1b[?1004h", 2004), Cause::Filter);
    assert_eq!(Cause::SurfaceEffect.name(), "surface-effect");
}

#[test]
fn deliveries_and_stages_are_named() {
    let names: Vec<&str> = Delivery::ALL.iter().map(|d| d.name()).collect();
    assert_eq!(names, ["control", "policy", "filter"]);
    assert!(!Delivery::Control.filtered());
    assert_eq!(Delivery::Policy.encoding(), ReportPolicy::Canonical);
    assert_eq!(Delivery::Filter.encoding(), ReportPolicy::Passthrough);
    assert!(
        Stage::ALL
            .iter()
            .filter(|s| !s.reports_in_control())
            .eq([&Stage::Key])
    );
    assert_eq!(Stage::FocusOut.marker(), "pss-mark-focus-out");
}

#[test]
fn manifest_round_trips() {
    let manifest = Manifest {
        profile: 2,
        vt_stamp: Some("abc".to_owned()),
        vt_version: None,
        cols: COLS,
        rows: ROWS,
        prefix_hex: "1b63".to_owned(),
        live_hex: String::new(),
        scenes: vec![SceneEntry {
            name: "separate".to_owned(),
            description: "d".to_owned(),
            files: BTreeMap::from([("canonical".to_owned(), "separate.canonical.bin".to_owned())]),
            rows: vec!["a".to_owned()],
            cursor_x: 1,
            cursor_y: 2,
            report_modes: BTreeMap::from([("?1004".to_owned(), true)]),
            kept_modes: BTreeMap::from([("?2004".to_owned(), true)]),
            inexpressible: Vec::new(),
        }],
    };
    let text = serde_json::to_string(&manifest).unwrap();
    let back: Manifest = serde_json::from_str(&text).unwrap();
    assert_eq!(back, manifest);
}

#[test]
fn shell_command_quotes_every_argument() {
    assert_eq!(
        shell_command("/a b/fx", &["--scene".to_owned(), "it's".to_owned()]),
        "'/a b/fx' '--scene' 'it'\\''s'"
    );
}
