//! FFI-тесты оракула (функция `vt`, ADR-0035): профиль колбэков приёмника, журналы, снятие
//! состояния и скрытого состояния повтором, сравнение эталона с приёмником на настоящих
//! терминалах. Маршрут через PTY и фикстуру — в `tests/oracle_vt.rs`.

use super::*;
use crate::fixture::query::{Query, ResponseKind, ResponseParser};
use crate::oracle::model::{Color, Outcome, compare};
use crate::oracle::suffix::{self, Step};

fn config() -> Config {
    Config::new(20, 5)
}

fn receiver(client: &str) -> Receiver {
    Receiver::new(client, &config(), ReceiverProfile::for_client(client)).expect("приёмник")
}

fn reference() -> Reference {
    Reference::new(&config()).expect("эталон")
}

fn kinds(replies: &[u8]) -> Vec<ResponseKind> {
    ResponseParser::default()
        .feed(replies)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect()
}

#[test]
fn receiver_answers_every_profile_query_exactly_once() {
    let queries = [
        Query::Da1,
        Query::Da2,
        Query::CursorPosition,
        Query::ModeDec { mode: 2004 },
        Query::ModeAnsi { mode: 4 },
        Query::ForegroundColor,
        Query::BackgroundColor,
        Query::Palette { index: 1 },
        Query::XtVersion,
        Query::ClipboardRead,
    ];
    for query in queries {
        let mut rx = receiver("c1");
        let replies = rx.feed(&query.bytes());
        assert_eq!(
            kinds(&replies),
            [query.response()],
            "{query:?}: {}",
            String::from_utf8_lossy(&replies)
        );
    }
    let mut rx = receiver("c1");
    assert_eq!(rx.feed(b"\x1b[c"), b"\x1b[?62;22;52c");
    let version = rx.feed(b"\x1b[>0q");
    assert_eq!(version, b"\x1bP>|shellio-probe-receiver(c1)\x1b\\");
    assert_eq!(rx.feed(b"\x1b[18t"), b"\x1b[8;5;20t");
}

#[test]
fn reference_has_no_return_route() {
    let mut reference = reference();
    reference.feed(b"\x1b[c\x1b[6nab");
    assert_eq!(reference.terminal().row_text(0).expect("строка"), "ab");
    // Ответов эталон не копит: колбэка WRITE_PTY у него нет.
    let mut term = vt::Terminal::new(4, 2).expect("терминал");
    term.write(b"\x1b[c");
    assert!(term.take_pty_output().is_empty());
}

#[test]
fn receiver_reports_focus_and_size_only_when_enabled() {
    let mut rx = receiver("c1");
    assert!(rx.focus(true).expect("фокус").is_empty());
    assert!(rx.resize(30, 6).expect("ресайз").is_empty());
    // Включение режима 2048 само отвечает текущим размером.
    let enabled = rx.feed(b"\x1b[?1004h\x1b[?2048h");
    assert_eq!(String::from_utf8_lossy(&enabled), "\x1b[48;6;30;120;300t");
    assert_eq!(rx.focus(true).expect("фокус"), b"\x1b[I");
    assert_eq!(rx.focus(false).expect("фокус"), b"\x1b[O");
    let report = rx.resize(40, 8).expect("ресайз");
    assert_eq!(kinds(&report), [ResponseKind::SizeReport]);
    assert_eq!(report, b"\x1b[48;8;40;160;400t");
    // Размер колбэка следует за ресайзом.
    assert_eq!(rx.feed(b"\x1b[18t"), b"\x1b[8;8;40t");
    let replies: Vec<&JournalEntry> = rx
        .journal()
        .iter()
        .filter(|entry| matches!(entry.event, Journaled::Reply { .. }))
        .collect();
    assert_eq!(replies.len(), 5);
}

#[test]
fn two_receivers_keep_separate_journals() {
    let mut first = receiver("c1");
    let mut second = receiver("c2");
    first.feed(b"\x1b]52;c;aGVsbG8=\x07");
    second.feed(b"x\x1b]52;c;?\x07");
    assert_eq!(
        first.journal(),
        [JournalEntry {
            at: 0,
            event: Journaled::ClipboardWrite {
                location: first_location(first.journal()),
                contents: vec![("text/plain".to_owned(), channel::hex(b"hello"))],
            },
        }]
    );
    let events: Vec<&Journaled> = second.journal().iter().map(|entry| &entry.event).collect();
    assert!(
        matches!(
            events.as_slice(),
            [
                Journaled::ClipboardRead { answered: true, .. },
                Journaled::Reply { hex }
            ] if hex.starts_with(&channel::hex(b"\x1b]52;c;"))
        ),
        "{events:?}"
    );
    assert!(second.journal().iter().all(|entry| entry.at == 0));
    second.feed(b"\x07");
    assert_eq!(second.journal().last().map(|entry| entry.at), Some(10));
    assert_eq!(
        second.journal().last().map(|entry| &entry.event),
        Some(&Journaled::Bell)
    );
    assert_eq!(first.journal().len(), 1, "журнал первого не изменился");
}

#[test]
fn managed_receiver_journals_role_reports_and_keeps_the_session_title() {
    let profile = ReceiverProfile::for_client("c1").with_role_prefix("zmx-role;t1:");
    let mut rx = Receiver::new("c1", &config(), profile).expect("приёмник");
    let mut reference = reference();
    reference.feed(b"\x1b]2;work\x07ab");
    // Снимок повторного attach: роль приходит раньше снимка, заголовок нагрузки — в нём.
    rx.feed(b"\x1b]2;zmx-role;t1:leader:3\x1b\\ab\x1b]2;work\x07");
    let state = rx.capture(Capture::default()).expect("состояние");
    assert_eq!(state.title, "work");
    assert_eq!(
        compare(
            &reference.capture(Capture::default()).expect("эталон"),
            &state
        )
        .outcome(),
        Outcome::Equal
    );
    // Роль после заголовка нагрузки: терминал показывает заголовок нагрузки, роль — в журнале.
    rx.feed(b"\x1b]2;zmx-role;t1:follower:4\x1b\\");
    assert_eq!(
        rx.terminal().title().expect("title"),
        b"zmx-role;t1:follower:4"
    );
    assert_eq!(
        rx.capture(Capture::default()).expect("состояние").title,
        "work"
    );
    assert_eq!(rx.last_role(), Some("follower:4"));
    let events: Vec<&Journaled> = rx.journal().iter().map(|entry| &entry.event).collect();
    assert_eq!(
        events,
        [
            &Journaled::Role {
                report: "leader:3".to_owned()
            },
            &Journaled::TitleChanged {
                title: "work".to_owned()
            },
            &Journaled::Role {
                report: "follower:4".to_owned()
            },
        ]
    );
    // Чужой токен — обычный заголовок нагрузки.
    rx.feed(b"\x1b]2;zmx-role;other:leader:1\x07");
    assert_eq!(
        rx.capture(Capture::default()).expect("состояние").title,
        "zmx-role;other:leader:1"
    );
    // Без префикса в профиле заголовок роли — заголовок.
    let mut plain = receiver("c2");
    plain.feed(b"\x1b]2;zmx-role;t1:leader:1\x07");
    assert_eq!(
        plain.capture(Capture::default()).expect("состояние").title,
        "zmx-role;t1:leader:1"
    );
    assert_eq!(plain.last_role(), None);
}

fn first_location(journal: &[JournalEntry]) -> i32 {
    match journal.first().map(|entry| &entry.event) {
        Some(Journaled::ClipboardWrite { location, .. }) => *location,
        other => panic!("ожидалась запись буфера обмена: {other:?}"),
    }
}

#[test]
fn clipboard_read_reply_reenters_write_pty_once() {
    let mut rx = receiver("c1");
    let replies = rx.feed(b"\x1b]52;c;?\x07\x1b]52;c;?\x07");
    assert_eq!(
        kinds(&replies),
        [ResponseKind::Clipboard, ResponseKind::Clipboard]
    );
    let reads = rx
        .journal()
        .iter()
        .filter(|entry| matches!(entry.event, Journaled::ClipboardRead { answered: true, .. }))
        .count();
    assert_eq!(reads, 2);
}

/// Поток со всем, что оракул снимает напрямую и повтором.
const RICH: &[u8] = b"\x1b]0;title\x07\x1b]7;file://host/tmp\x07\
\x1b[1;31mred\x1b[0m \x1b[38;2;1;2;3mrgb\x1b[0m \xe4\xb8\x96 \
\x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\\r\n\
0123456789abcdefghijklmn\r\n\
line3\r\nline4\r\nline5\r\nline6\
\x1b[2;3H\x1b[4m\x1b7\x1b[0m\x1b[5;1H\
\x1b[>1u\x1b[>3u\x1b[?1h\x1b=\x1b[?2004h\x1b[?1000h\x1b[?1006h";

#[test]
fn reference_and_receiver_fed_same_bytes_compare_equal() {
    let mut reference = reference();
    let mut rx = receiver("c1");
    reference.feed(RICH);
    rx.feed(RICH);
    let a = reference.capture(Capture::default()).expect("эталон");
    let b = rx.capture(Capture::default()).expect("приёмник");
    let comparison = compare(&a, &b);
    assert_eq!(comparison.outcome(), Outcome::Equal, "{comparison}");

    assert_eq!(a.title, "title");
    assert_eq!(a.pwd, "file://host/tmp");
    // Семь строк на пяти строках экрана: две первые ушли в историю.
    assert_eq!(a.history_total, 2);
    let first = &a.history[1];
    assert_eq!(first.text(), "red rgb 世 link");
    assert_eq!(first.cells[0].style.fg, Color::Palette(1));
    assert!(first.cells[0].style.bold);
    assert_eq!(
        first.cells[4].style.fg,
        Color::Rgb(model::Rgb { r: 1, g: 2, b: 3 })
    );
    assert_eq!(first.cells[8].width, model::Width::Wide);
    assert_eq!(first.cells[9].width, model::Width::SpacerTail);
    assert_eq!(first.cells[10].hyperlink, None);
    assert_eq!(first.cells[11].text, "l");
    assert_eq!(
        first.cells[11].hyperlink.as_deref(),
        Some("https://example.test")
    );
    assert!(first.cells[19].text.is_empty());
    assert!(a.history[0].wrap);
    assert!(a.active[0].wrap_continuation);
    assert_eq!(a.active[0].text(), "klmn");
    assert_eq!(a.modes.get("?2004"), Some(&true));
    assert_eq!(a.modes.get("?66"), Some(&true));
    assert_eq!(a.kitty_flags, 3);
    assert_eq!(a.kitty_stack, Observed::Value(vec![3, 1]));
    // Флаги Kitty главнее DECCKM: стрелка кодируется CSI u с типом события.
    assert_eq!(
        a.input.get("key:up").map(String::as_str),
        Some(channel::hex(b"\x1b[1;1:1A").as_str())
    );
    assert_eq!(
        a.input.get("mouse:left-press").map(String::as_str),
        Some(channel::hex(b"\x1b[<0;3;2M").as_str())
    );
    let Observed::Value(saved) = a.saved_cursor else {
        panic!("сохранённый курсор наблюдается");
    };
    assert_eq!((saved.x, saved.y), (2, 1));
    assert_eq!(saved.style.underline, 1);
    assert!(a.ground);
}

#[test]
fn capture_does_not_change_the_terminal() {
    let mut reference = reference();
    reference.feed(RICH);
    let before = reference
        .capture(Capture::default())
        .expect("первое снятие");
    let again = reference
        .capture(Capture::default())
        .expect("второе снятие");
    assert_eq!(before, again);
    assert_eq!(
        reference.terminal().kitty_keyboard_flags().expect("Kitty"),
        3
    );
}

#[test]
fn hidden_state_differences_are_found_by_replay() {
    let mut reference = reference();
    let mut rx = receiver("c1");
    reference.feed(b"ab\x1b7cd\x1b[>1u\x1b[>5u");
    // Приёмник получил тот же экран, но без DECSC и без нижнего уровня стека Kitty.
    rx.feed(b"abcd\x1b[>5u");
    let a = reference.capture(Capture::default()).expect("эталон");
    let b = rx.capture(Capture::default()).expect("приёмник");
    let comparison = compare(&a, &b);
    let paths: Vec<&str> = comparison
        .mismatches
        .iter()
        .map(|d| d.path.as_str())
        .collect();
    assert_eq!(paths, ["saved_cursor.x", "kitty_stack"], "{comparison}");
}

#[test]
fn replay_probe_cancels_a_partial_sequence_first() {
    let mut reference = reference();
    reference.feed(b"ab\x1b7cd\x1b]52;c;aGk");
    assert!(!reference.terminal().vt_ground().expect("ground"));
    let state = reference.capture(Capture::default()).expect("снятие");
    assert!(!state.ground);
    let Observed::Value(saved) = state.saved_cursor else {
        panic!("сохранённый курсор наблюдается");
    };
    assert_eq!((saved.x, saved.y), (2, 0));
}

#[test]
fn history_is_captured_nearest_first() {
    let mut reference = reference();
    for line in 0..12 {
        reference.feed(format!("line{line}\r\n").as_bytes());
    }
    let state = reference.capture(Capture::default()).expect("снятие");
    // 12 строк и пустая строка курсора на 5 строках экрана: 8 строк ушли в историю.
    assert_eq!(state.history_total, 8);
    assert_eq!(state.history[0].text(), "line7");
    assert_eq!(state.history[7].text(), "line0");
    assert_eq!(state.text()[0], "line8");
    let shallow = reference
        .capture(Capture {
            history: 2,
            hidden: false,
        })
        .expect("снятие");
    assert_eq!(shallow.history.len(), 2);
    assert_eq!(shallow.history_total, 8);
    assert!(matches!(shallow.saved_cursor, Observed::NotObserved(_)));
}

#[test]
fn log_overflow_leaves_hidden_state_unobserved() {
    let mut small = config();
    small.log_cap = 8;
    let mut reference = Reference::new(&small).expect("эталон");
    reference.feed(b"0123456789");
    let state = reference.capture(Capture::default()).expect("снятие");
    let reason = "журнал ввода превысил 8 байт".to_owned();
    assert_eq!(state.kitty_stack, Observed::NotObserved(reason.clone()));
    assert_eq!(state.saved_cursor, Observed::NotObserved(reason));
    let mut whole = Reference::new(&config()).expect("эталон");
    whole.feed(b"0123456789");
    let comparison = compare(&state, &whole.capture(Capture::default()).expect("снятие"));
    assert_eq!(comparison.outcome(), Outcome::Incomplete, "{comparison}");
    assert_eq!(comparison.unobserved.len(), 2);
}

#[test]
fn palette_index_and_its_rgb_are_equal_on_real_terminals() {
    let mut reference = reference();
    let mut rx = receiver("c1");
    reference.feed(b"\x1b[31mX");
    let palette = reference.terminal().colors().expect("цвета").palette;
    let red = palette[1];
    rx.feed(format!("\x1b[38;2;{};{};{}mX", red.r, red.g, red.b).as_bytes());
    let comparison = compare(
        &reference.capture(Capture::default()).expect("эталон"),
        &rx.capture(Capture::default()).expect("приёмник"),
    );
    assert_eq!(comparison.outcome(), Outcome::Equal, "{comparison}");
    assert_eq!(comparison.normalized_count, 2, "клетка и перо курсора");
}

#[test]
fn input_probes_are_all_defined() {
    for name in model::INPUT_PROBES {
        assert!(input_probe(name, &config()).is_some(), "{name}");
    }
    let state = reference().capture(Capture::default()).expect("снятие");
    assert_eq!(state.input.len(), model::INPUT_PROBES.len());
}

#[test]
fn suffixes_reveal_state_on_the_reference() {
    let mut reference = reference();
    reference.feed(b"main\x1b[?1049halt\x1b7\x1b[3;1H\x1b[>1u\x1b[>3u");
    for suffix in suffix::catalog(20, 5) {
        suffix::apply(&suffix.steps, &mut reference).expect(suffix.name);
    }
    let state = reference.capture(Capture::default()).expect("снятие");
    assert_eq!(state.screen, model::Screen::Primary);
    assert_eq!((state.cols, state.rows), (20, 5));
    assert_eq!(state.kitty_flags, 0, "стек основного экрана пуст");
    // DECRC вернул курсор, сохранённый входом в 1049, и R затёр A; W, T и M легли следом.
    assert_eq!(
        state.history.last().map(model::Row::text).as_deref(),
        Some("mainRW  T       T")
    );
    let steps = [Step::Resize { cols: 0, rows: 5 }];
    assert!(suffix::apply(&steps, &mut reference).is_err());
}

#[test]
fn receiver_from_snapshot_replays_hidden_state_from_the_same_snapshot() {
    let scene = b"main\x1b[2;3H\x1b[1m\x1b7\x1b[0m\x1b[>1u\x1b[>3u\x1b[4;1Hx\x1b[3";
    let mut source = Reference::new(&Config {
        continuation_max: 64,
        ..config()
    })
    .expect("источник");
    source.feed(scene);
    let snapshot = source.terminal().snapshot().expect("снимок");
    let mut restored = Receiver::from_snapshot(
        "snap",
        &config(),
        ReceiverProfile::for_client("snap"),
        &snapshot,
        64,
    )
    .expect("приёмник из снимка");
    assert!(
        restored.journal().is_empty(),
        "декодирование не вызывает колбэков"
    );
    let mut expected = reference();
    expected.feed(scene);
    let comparison = compare(
        &expected.capture(Capture::default()).expect("эталон"),
        &restored.capture(Capture::default()).expect("приёмник"),
    );
    assert_eq!(comparison.outcome(), Outcome::Equal, "{comparison}");

    // Суффикс через Sink приёмника и эталона: остаток CSI и снятие стека.
    let steps = [Step::Output(b"1mR\x1b[<u\x1b8S".to_vec())];
    suffix::apply(&steps, &mut expected).expect("эталон");
    suffix::apply(&steps, &mut restored).expect("приёмник");
    let comparison = compare(
        &expected.capture(Capture::default()).expect("эталон"),
        &restored.capture(Capture::default()).expect("приёмник"),
    );
    assert_eq!(comparison.outcome(), Outcome::Equal, "{comparison}");
}
