//! FFI-тесты моста (функция `vt`, ADR-0035): исходный терминал, извлечение, байты кодировщика,
//! свежий приёмник и сравнение с эталоном и с извлечённым состоянием.

use super::*;
use crate::bridge::{ReportPolicy, differences, encode, without_report_modes};
use crate::matrix::capi;
use crate::oracle::model::{Outcome, compare};
use crate::oracle::vt::{Capture, Config, Journaled, Receiver, ReceiverProfile, Reference};

const BUDGET: usize = 1 << 20;

struct Trip {
    reference: Reference,
    receiver: Receiver,
    state: State,
    bytes: Vec<u8>,
}

fn trip(steps: &[Vec<u8>]) -> Trip {
    let config = Config::new(80, 24);
    let source_config = Config {
        continuation_max: BUDGET,
        ..config.clone()
    };
    let mut reference = Reference::new(&config).unwrap();
    let mut source =
        Receiver::new("host", &source_config, ReceiverProfile::for_client("host")).unwrap();
    for step in steps {
        reference.feed(step);
        source.feed(step);
    }
    let state = extract(source.terminal(), BUDGET).unwrap();
    let bytes = encode(&state, ReportPolicy::Canonical);
    // Отслеживание continuation у приёмника нужно только повторному извлечению: снимок терминала
    // внутри последовательности без него не кодируется.
    let mut receiver = Receiver::new(
        "bridge",
        &source_config,
        ReceiverProfile::for_client("bridge"),
    )
    .unwrap();
    receiver.feed(&bytes);
    Trip {
        reference,
        receiver,
        state,
        bytes,
    }
}

/// Эталон и приёмник равны, извлечённое у приёмника равно извлечённому у источника (без режимов
/// отчётов), воспроизведение не дало ни ответа, ни эффекта кроме заголовка.
fn assert_restored(trip: &Trip, label: &str) {
    let expected = trip.reference.capture(Capture::default()).unwrap();
    let actual = trip.receiver.capture(Capture::default()).unwrap();
    let comparison = compare(&expected, &actual);
    assert_eq!(
        comparison.outcome(),
        Outcome::Equal,
        "{label}: {comparison}\nбайты: {:?}",
        String::from_utf8_lossy(&trip.bytes)
    );
    let again = extract(trip.receiver.terminal(), BUDGET).unwrap();
    let (a, b) = (
        without_report_modes(&trip.state),
        without_report_modes(&again),
    );
    assert!(
        a == b,
        "{label}: извлечённое состояние разошлось: {:?}",
        differences(&a, &b)
    );
    let effects: Vec<&Journaled> = trip
        .receiver
        .journal()
        .iter()
        .map(|entry| &entry.event)
        .filter(|event| !matches!(event, Journaled::TitleChanged { .. }))
        .collect();
    assert!(
        effects.is_empty(),
        "{label}: эффекты воспроизведения {effects:?}"
    );
}

fn suffix(trip: &mut Trip, bytes: &[u8], label: &str) {
    trip.reference.feed(bytes);
    trip.receiver.feed(bytes);
    let expected = trip.reference.capture(Capture::default()).unwrap();
    let actual = trip.receiver.capture(Capture::default()).unwrap();
    let comparison = compare(&expected, &actual);
    assert_eq!(
        comparison.outcome(),
        Outcome::Equal,
        "{label} после суффикса: {comparison}\nбайты: {:?}",
        String::from_utf8_lossy(&trip.bytes)
    );
}

#[test]
fn capi_stories_round_trip_through_bytes() {
    for story in capi::stories() {
        let mut trip = trip(&story.scene);
        assert_restored(&trip, story.cell);
        for (name, steps) in &story.suffixes {
            for step in steps {
                match step {
                    crate::oracle::suffix::Step::Output(bytes) => {
                        suffix(&mut trip, bytes, &format!("{} {name}", story.cell));
                    }
                    crate::oracle::suffix::Step::Resize { cols, rows } => {
                        trip.reference.resize(*cols, *rows).unwrap();
                        trip.receiver.resize(*cols, *rows).unwrap();
                    }
                }
            }
        }
    }
}

#[test]
fn unfinished_sequence_is_the_tail_of_the_bytes() {
    for (variant, prefix, expected, rest) in capi::B7_VARIANTS {
        let mut trip = trip(&[b"b7 scene\r\n\x1b[1mbold ".to_vec(), prefix.to_vec()]);
        assert_eq!(trip.state.continuation, expected, "{variant}");
        assert!(trip.bytes.ends_with(expected), "{variant}");
        assert_restored(&trip, variant);
        suffix(&mut trip, rest, variant);
    }
}

#[test]
fn inactive_alternate_screen_survives() {
    let mut trip = trip(&[
        b"\x1b[?1049h\x1b[2;3Hstale alt\x1b[?1049l".to_vec(),
        b"main text".to_vec(),
    ]);
    assert!(!trip.state.alternate.is_blank());
    assert_restored(&trip, "inactive-alt");
    suffix(&mut trip, b"\x1b[?47hA", "inactive-alt");
}

#[test]
fn alternate_screens_entered_by_47_and_1047_survive() {
    for entry in ["47", "1047"] {
        let mut trip = trip(&[
            b"main\x1b[3;3H\x1b[32m\x1b7\x1b[0m\x1b[5;5H".to_vec(),
            format!("\x1b[?{entry}h\x1b[Halt {entry}\x1b[4;4H").into_bytes(),
        ]);
        assert_restored(&trip, entry);
        suffix(&mut trip, format!("\x1b[?{entry}l\x1b8R").as_bytes(), entry);
    }
}

#[test]
fn background_only_cells_survive() {
    let trip = trip(&[b"a\x1b[44m\x1b[3X\x1b[48;2;1;2;3m\x1b[5C\x1b[2X\x1b[0mb".to_vec()]);
    let row = &trip.state.primary.active[0];
    assert!(row.cells[1].bg_only.is_some(), "{:?}", row.cells[1]);
    assert_restored(&trip, "bg-only");
}

#[test]
fn mouse_event_and_format_follow_the_last_enabled_mode() {
    let trip = trip(&[b"\x1b[?1003h\x1b[?1000h\x1b[?1015h\x1b[?1006h".to_vec()]);
    assert_eq!(trip.state.mouse_event, MouseEvent::Normal);
    assert_eq!(trip.state.mouse_format, MouseFormat::Sgr);
    assert_restored(&trip, "mouse");
    let trip2 = self::trip(&[b"\x1b[?1000h\x1b[?1002l".to_vec()]);
    assert_eq!(trip2.state.mouse_event, MouseEvent::None);
    assert_restored(&trip2, "mouse-off");
}

#[test]
fn origin_margins_and_saved_cursor_with_origin_survive() {
    let mut trip = trip(&[
        b"text\x1b[5;15r\x1b[?6h\x1b[3;4H\x1b[1m\x1b7\x1b[0m\x1b[?6l\x1b[20;1Hlow".to_vec(),
    ]);
    assert_eq!(trip.state.margins.top, 4);
    assert_eq!(trip.state.margins.bottom, 14);
    assert!(trip.state.primary.saved.origin);
    assert_restored(&trip, "origin");
    suffix(&mut trip, b"\x1b8S\x1b[999B\n\nM", "origin");
}

#[test]
fn left_right_margins_survive() {
    let mut trip = trip(&[b"\x1b[?69h\x1b[5;30s\x1b[3;20r\x1b[?6h\x1b[2;2Hin".to_vec()]);
    assert_eq!(trip.state.margins.left, 4);
    assert_eq!(trip.state.margins.right, 29);
    assert_restored(&trip, "lr-margins");
    suffix(&mut trip, b"\x1b[999C\x1b[999B\nE", "lr-margins");
}

#[test]
fn pending_wrap_in_saved_and_current_cursor_survives() {
    let mut line = vec![b'q'; 80];
    line.extend_from_slice(b"\x1b7\r\n");
    line.extend(vec![b'r'; 80]);
    let mut trip = trip(&[line]);
    assert!(trip.state.primary.saved.pending_wrap);
    assert!(trip.state.primary.cursor.pending_wrap);
    assert_restored(&trip, "pending-wrap");
    suffix(&mut trip, b"W\x1b8V", "pending-wrap");
}

#[test]
fn full_kitty_ring_survives() {
    let pushes: String = (1..=9).map(|flags| format!("\x1b[>{flags}u")).collect();
    let mut trip = trip(&[pushes.into_bytes()]);
    assert_eq!(trip.state.primary.kitty.len(), 8);
    assert!(trip.state.primary.kitty.iter().all(|flags| *flags != 0));
    assert_restored(&trip, "kitty-ring");
    suffix(&mut trip, b"\x1b[<3u", "kitty-ring");
}

#[test]
fn protection_hyperlink_pen_and_colors_survive() {
    let trip = trip(&[
        b"\x1b[1\"qprot\x1b[0\"q plain ".to_vec(),
        b"\x1b]4;1;rgb:12/34/56\x1b\\\x1b]10;rgb:aa/bb/cc\x1b\\\x1b]12;rgb:01/02/03\x1b\\".to_vec(),
        b"\x1b]7;file://host/tmp/x\x1b\\\x1b]2;title\x07".to_vec(),
        b"\x1b]8;;https://pen.test\x1b\\\x1b[1\"q\x1b[3;38;2;9;8;7m".to_vec(),
    ]);
    assert!(trip.state.primary.active[0].cells[0].protected);
    assert_eq!(
        trip.state.primary.cursor.pen.hyperlink.as_deref(),
        Some("https://pen.test")
    );
    assert!(trip.state.primary.cursor.pen.protected);
    assert_eq!(trip.state.palette.len(), 1);
    assert!(trip.state.foreground.is_some());
    assert_restored(&trip, "pen");
}

#[test]
fn tabstops_survive() {
    let mut trip = trip(&[b"\x1b[3g\x1b[4G\x1bH\x1b[31G\x1bH".to_vec()]);
    assert_eq!(trip.state.tabstops, vec![3, 30]);
    assert_restored(&trip, "tabs");
    suffix(&mut trip, b"\r\tA\tB\tC", "tabs");
}

#[test]
fn report_modes_are_not_replayed_and_nothing_replies() {
    let trip = trip(&[b"\x1b[?1004h\x1b[?2048h\x1b[?2031h\x1b[?2033h\x1b[?2004h".to_vec()]);
    assert_eq!(trip.state.modes.get("?2048"), Some(&true));
    let text = String::from_utf8_lossy(&trip.bytes);
    for mode in ["1004", "2031", "2033", "2048"] {
        assert!(!text.contains(mode), "{mode}: {text:?}");
    }
    assert_restored(&trip, "report-modes");
}
