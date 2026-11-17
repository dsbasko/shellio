//! FFI-тесты обёртки над набором `libghostty-vt` пробы (функция `vt`, ADR-0035): идут в цели пробы,
//! а не в `make check`.

use super::*;

fn terminal() -> Terminal {
    Terminal::new(20, 5).expect("терминал создаётся")
}

#[test]
fn smoke_links_registers_write_pty_typed_get_and_input_encoder() {
    let pin = include_str!("../../../../scripts/ghostty/pin");
    let pinned = pin
        .lines()
        .find_map(|line| line.strip_prefix("GHOSTTY_VERSION_STRING="))
        .expect("в pin нет GHOSTTY_VERSION_STRING");
    assert_eq!(library_version().expect("версия библиотеки"), pinned);

    let mut term = terminal();
    term.enable_effects(Responder::default())
        .expect("колбэки регистрируются");
    // DSR состояния отвечает сам терминал через WRITE_PTY.
    term.write(b"\x1b[5n");
    assert_eq!(term.take_pty_output(), b"\x1b[0n");
    assert_eq!(term.cols().expect("cols"), 20);
    assert_eq!(term.rows().expect("rows"), 5);
    assert!(term.vt_ground().expect("ground"));

    let mut keys = KeyEncoder::new().expect("кодировщик клавиш");
    keys.sync_from(&term);
    assert_eq!(
        keys.encode(&KeyInput::press(Key::ENTER, "\r"))
            .expect("Enter"),
        b"\r"
    );
}

#[test]
fn writes_and_reads_cells_cursor_and_hyperlink() {
    let mut term = terminal();
    term.write(
        "\x1b[1;31mh\x1b[0mé 世\x1b]8;;https://example.test\x1b\\L\x1b]8;;\x1b\\".as_bytes(),
    );

    let h = term.cell(Area::Active, 0, 0).expect("клетка h");
    assert_eq!(h.text_string(), "h");
    assert!(h.style.bold);
    assert_eq!(h.style.fg, Color::Palette(1));

    let e = term.cell(Area::Active, 1, 0).expect("клетка é");
    assert_eq!(e.text, vec![0xE9]);
    assert_eq!(e.style.fg, Color::None);

    let blank = term.cell(Area::Active, 2, 0).expect("пробел");
    assert_eq!(blank.text_string(), " ");

    let wide = term.cell(Area::Active, 3, 0).expect("широкий");
    assert_eq!(wide.wide, Wide::Wide);
    assert_eq!(wide.text_string(), "世");
    let tail = term.cell(Area::Active, 4, 0).expect("хвост широкого");
    assert_eq!(tail.wide, Wide::SpacerTail);

    let link = term.cell(Area::Active, 5, 0).expect("ссылка");
    assert_eq!(link.text_string(), "L");
    assert_eq!(link.hyperlink.as_deref(), Some("https://example.test"));

    let empty = term.cell(Area::Active, 6, 0).expect("пустая");
    assert!(empty.text.is_empty());
    assert_eq!(empty.hyperlink, None);

    let cursor = term.cursor().expect("курсор");
    assert_eq!((cursor.x, cursor.y), (6, 0));
    assert!(cursor.visible);
    assert!(!cursor.pending_wrap);
    assert_eq!(term.row_text(0).expect("строка"), "hé 世L");
}

#[test]
fn soft_wrap_and_pending_wrap_are_visible() {
    let mut term = terminal();
    term.write(b"0123456789abcdefghij");
    let cursor = term.cursor().expect("курсор");
    assert!(cursor.pending_wrap);
    assert_eq!(cursor.x, 19);
    term.write(b"k");
    let first = term.cell(Area::Active, 0, 0).expect("первая строка");
    assert!(first.row_wrap);
    let second = term.cell(Area::Active, 0, 1).expect("вторая строка");
    assert!(second.row_wrap_continuation);
    assert_eq!(second.text_string(), "k");
}

#[test]
fn alternate_screen_restores_primary() {
    let mut term = terminal();
    term.write(b"main");
    term.write(b"\x1b[?1049h");
    assert_eq!(term.active_screen().expect("экран"), Screen::Alternate);
    assert!(term.mode(Mode::ALT_SCREEN_SAVE).expect("режим 1049"));
    // 1049 сохраняет позицию курсора, поэтому текст альтернативного экрана — с начала строки.
    term.write(b"\x1b[Halt");
    assert_eq!(term.row_text(0).expect("строка"), "alt");
    term.write(b"\x1b[?1049l");
    assert_eq!(term.active_screen().expect("экран"), Screen::Primary);
    assert!(!term.mode(Mode::ALT_SCREEN_SAVE).expect("режим 1049"));
    assert_eq!(term.row_text(0).expect("строка"), "main");
    assert_eq!(
        term.format(FormatOptions::PLAIN).expect("formatter"),
        b"main"
    );
}

#[test]
fn continuation_restores_split_csi_directly_and_through_snapshot() {
    let mut source = terminal();
    source
        .set_continuation_max_bytes(64)
        .expect("continuation включается");
    source.write(b"ab\x1b[3");
    assert!(!source.vt_ground().expect("ground"));
    let continuation = source.continuation().expect("continuation");
    assert_eq!(continuation, b"\x1b[3");

    let mut direct = terminal();
    direct.write(&continuation);
    let mut restored =
        Terminal::from_snapshot(&source.snapshot().expect("снимок"), 64).expect("восстановление");
    assert_eq!(
        restored.continuation().expect("continuation снимка"),
        b"\x1b[3"
    );
    assert_eq!(restored.row_text(0).expect("строка снимка"), "ab");

    for term in [&mut source, &mut direct, &mut restored] {
        term.write(b"1mX");
        assert!(term.vt_ground().expect("ground"));
    }
    let x_source = source.cell(Area::Active, 2, 0).expect("X источника");
    let x_direct = direct.cell(Area::Active, 0, 0).expect("X прямого");
    let x_restored = restored.cell(Area::Active, 2, 0).expect("X снимка");
    assert_eq!(x_source.style.fg, Color::Palette(1));
    assert_eq!(x_direct.style, x_source.style);
    assert_eq!(x_restored, x_source);
    assert_eq!(
        restored.cursor().expect("курсор"),
        source.cursor().expect("курсор")
    );
}

#[test]
fn continuation_is_unavailable_when_enabled_after_split() {
    let mut term = terminal();
    term.write(b"\x1b[3");
    term.set_continuation_max_bytes(64)
        .expect("continuation включается");
    assert!(term.continuation().is_err());
}

#[test]
fn device_attributes_callback_answers_once_through_write_pty() {
    let mut term = terminal();
    term.enable_effects(Responder {
        device_attributes: Some(DeviceAttributes {
            conformance_level: 62,
            features: vec![22],
            device_type: 1,
            firmware_version: 10,
            unit_id: 0xAB,
        }),
        ..Responder::default()
    })
    .expect("колбэки");
    term.write(b"\x1b[c");
    assert_eq!(term.take_pty_output(), b"\x1b[?62;22c");
    term.write(b"\x1b[>c");
    assert_eq!(term.take_pty_output(), b"\x1b[>1;10;0c");
    // Запрос, разрезанный между записями, получает один ответ.
    term.write(b"\x1b[");
    assert!(term.take_pty_output().is_empty());
    term.write(b"c");
    assert_eq!(term.take_pty_output(), b"\x1b[?62;22c");
}

#[test]
fn size_and_color_scheme_queries_without_responder_stay_silent() {
    let mut term = terminal();
    term.enable_effects(Responder::default()).expect("колбэки");
    for query in [&b"\x1b[18t"[..], b"\x1b[14t", b"\x1b[?996n"] {
        term.write(query);
        let reply = term.take_pty_output();
        assert!(
            reply.is_empty(),
            "{query:?}: {}",
            String::from_utf8_lossy(&reply)
        );
    }
}

// Вопреки terminal.h запрос DA при установленном WRITE_PTY не глушится ни колбэком DA, вернувшим
// false, ни снятым колбэком: терминал отвечает атрибутами по умолчанию.
#[test]
fn device_attributes_are_answered_by_default_without_responder() {
    let mut term = terminal();
    term.enable_effects(Responder::default()).expect("колбэки");
    term.write(b"\x1b[c");
    assert_eq!(term.take_pty_output(), b"\x1b[?62;22c");

    term.set_callback(
        sys::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
        "ghostty_terminal_set(DEVICE_ATTRIBUTES)",
        ptr::null(),
    )
    .expect("колбэк DA снимается");
    term.write(b"\x1b[c");
    assert_eq!(term.take_pty_output(), b"\x1b[?62;22c");
}

#[test]
fn title_changes_in_one_write_keep_each_title() {
    let mut term = terminal();
    term.enable_effects(Responder::default()).expect("колбэки");
    term.write(b"\x1b]2;first\x07text\x1b]0;second\x1b\\");
    let titles: Vec<Vec<u8>> = term
        .take_effects()
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::TitleChanged { title } => Some(title),
            _ => None,
        })
        .collect();
    assert_eq!(titles, [b"first".to_vec(), b"second".to_vec()]);
    assert_eq!(term.title().expect("title"), b"second");
}

#[test]
fn clipboard_effects_copy_data_and_answer_reads() {
    let mut term = terminal();
    term.enable_effects(Responder {
        clipboard: Some(b"hi".to_vec()),
        ..Responder::default()
    })
    .expect("колбэки");
    term.write(b"\x1b]52;c;aGVsbG8=\x07");
    let effects = term.take_effects();
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ClipboardWrite { contents, .. }] if contents == &[("text/plain".to_owned(), b"hello".to_vec())]
        ),
        "{effects:?}"
    );
    term.write(b"\x1b]52;c;?\x07");
    let effects = term.take_effects();
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ClipboardRead { answered: true, .. }]
        ),
        "{effects:?}"
    );
    let reply = term.take_pty_output();
    assert!(
        reply.starts_with(b"\x1b]52;c;aGk="),
        "{}",
        String::from_utf8_lossy(&reply)
    );
}

#[test]
fn key_and_mouse_encoders_follow_terminal_modes() {
    let mut term = terminal();
    let mut keys = KeyEncoder::new().expect("кодировщик клавиш");
    keys.sync_from(&term);
    let up = KeyInput {
        unshifted: None,
        ..KeyInput::press(Key::ARROW_UP, "")
    };
    assert_eq!(keys.encode(&up).expect("стрелка"), b"\x1b[A");
    term.write(b"\x1b[?1h");
    keys.sync_from(&term);
    assert_eq!(keys.encode(&up).expect("стрелка DECCKM"), b"\x1bOA");
    term.write(b"\x1b[>1u");
    assert_eq!(
        term.kitty_keyboard_flags().expect("флаги Kitty"),
        KITTY_DISAMBIGUATE
    );
    keys.sync_from(&term);
    let escape = KeyInput {
        unshifted: None,
        ..KeyInput::press(Key::ESCAPE, "")
    };
    assert_eq!(keys.encode(&escape).expect("Escape Kitty"), b"\x1b[27u");

    let geometry = MouseGeometry {
        screen_width: 200,
        screen_height: 100,
        cell_width: 10,
        cell_height: 20,
    };
    let mut mouse = MouseEncoder::new(geometry).expect("кодировщик мыши");
    term.write(b"\x1b[?1000h\x1b[?1006h");
    assert!(term.mouse_tracking().expect("мышь"));
    mouse.sync_from(&term);
    let press = MouseInput {
        action: MouseAction::Press,
        button: Some(MouseButton::Left),
        mods: 0,
        x: 25.0,
        y: 45.0,
    };
    assert_eq!(mouse.encode(&press).expect("нажатие"), b"\x1b[<0;3;3M");
}

#[test]
fn osc_color_queries_need_default_colors() {
    let mut term = terminal();
    term.enable_effects(Responder::default()).expect("колбэки");
    let colors = term.colors().expect("цвета");
    assert_eq!(
        (colors.foreground, colors.background),
        (None, None),
        "без цветов по умолчанию"
    );
    assert_eq!(colors.palette.len(), 256);
    term.write(b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
    assert!(term.take_pty_output().is_empty(), "OSC 10 и 11 молчат");
    term.write(b"\x1b]4;1;?\x1b\\");
    assert!(
        term.take_pty_output().starts_with(b"\x1b]4;1;rgb:"),
        "палитра задана всегда"
    );

    let white = Rgb {
        r: 0xff,
        g: 0xff,
        b: 0xff,
    };
    term.set_default_colors(Some(white), None, None)
        .expect("цвета по умолчанию");
    assert_eq!(term.colors().expect("цвета").foreground, Some(white));
    term.write(b"\x1b]10;?\x1b\\");
    assert_eq!(term.take_pty_output(), b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\");
    term.write(b"\x1b]11;?\x1b\\");
    assert!(
        term.take_pty_output().is_empty(),
        "фон по-прежнему не задан"
    );
}

#[test]
fn focus_reports_are_encoded_by_event() {
    assert_eq!(focus_report(true).expect("фокус"), b"\x1b[I");
    assert_eq!(focus_report(false).expect("фокус"), b"\x1b[O");
}

// --- находки ранней проверки C API (задача 8, сценарий capi) ---

/// Терминал 10×4 с историей и с потоком `bytes`.
fn fed(bytes: &[u8]) -> Terminal {
    let mut term = Terminal::new(10, 4).expect("терминал");
    term.set_scrollback_max_lines(100).expect("история");
    term.write(bytes);
    term
}

/// Терминал клиента из formatter VT исходного.
fn from_formatter(source: &Terminal) -> Terminal {
    let mut term = Terminal::new(source.cols().expect("cols"), source.rows().expect("rows"))
        .expect("терминал клиента");
    term.set_scrollback_max_lines(100).expect("история");
    term.write(&source.format(FormatOptions::VT_FULL).expect("formatter"));
    term
}

fn from_snapshot(source: &Terminal) -> Terminal {
    Terminal::from_snapshot(&source.snapshot().expect("снимок"), 0).expect("декодирование")
}

fn wraps(term: &Terminal, y: u32) -> (bool, bool) {
    let cell = term.cell(Area::Active, 0, y).expect("клетка");
    (cell.row_wrap, cell.row_wrap_continuation)
}

#[test]
fn formatter_loses_soft_wrap_flags_snapshot_keeps_them() {
    let source = fed(b"0123456789abc");
    assert_eq!(wraps(&source, 0), (true, false));
    assert_eq!(wraps(&source, 1), (false, true));
    let formatted = from_formatter(&source);
    assert_eq!(formatted.row_text(1).expect("строка"), "abc");
    assert_eq!(wraps(&formatted, 0), (false, false), "перенос стал жёстким");
    assert_eq!(wraps(&formatted, 1), (false, false));
    let snapshot = from_snapshot(&source);
    assert_eq!(wraps(&snapshot, 0), (true, false));
    assert_eq!(wraps(&snapshot, 1), (false, true));
}

#[test]
fn formatter_omits_title_snapshot_keeps_it() {
    let source = fed(b"\x1b]2;session title\x07x");
    assert!(from_formatter(&source).title().expect("title").is_empty());
    assert_eq!(
        from_snapshot(&source).title().expect("title"),
        b"session title"
    );
}

#[test]
fn formatter_loses_saved_cursor_snapshot_keeps_it() {
    let source = fed(b"\x1b[3;4H\x1b[1m\x1b7\x1b[0m\x1b[1;1H");
    for (name, mut term, expected) in [
        ("formatter", from_formatter(&source), (0, 0, false)),
        ("snapshot", from_snapshot(&source), (3, 2, true)),
    ] {
        term.write(b"\x1b8");
        let cursor = term.cursor().expect("курсор");
        assert_eq!((cursor.x, cursor.y, cursor.style.bold), expected, "{name}");
    }
}

#[test]
fn formatter_keeps_only_kitty_top_snapshot_keeps_stack() {
    let source = fed(b"\x1b[>1u\x1b[>3u");
    for (name, mut term, below) in [
        ("formatter", from_formatter(&source), 0),
        ("snapshot", from_snapshot(&source), KITTY_DISAMBIGUATE),
    ] {
        assert_eq!(term.kitty_keyboard_flags().expect("флаги"), 3, "{name}");
        term.write(b"\x1b[<u");
        assert_eq!(term.kitty_keyboard_flags().expect("флаги"), below, "{name}");
    }
}

#[test]
fn formatter_of_alternate_screen_loses_primary_snapshot_keeps_it() {
    let source = fed(b"main\x1b[?1049h\x1b[Halt");
    for (name, mut term, primary) in [
        ("formatter", from_formatter(&source), ""),
        ("snapshot", from_snapshot(&source), "main"),
    ] {
        assert_eq!(
            term.active_screen().expect("экран"),
            Screen::Alternate,
            "{name}"
        );
        assert_eq!(term.row_text(0).expect("строка"), "alt", "{name}");
        term.write(b"\x1b[?1049l");
        assert_eq!(term.row_text(0).expect("строка"), primary, "{name}");
    }
}

#[test]
fn formatter_loses_spacer_head_of_wide_char_at_line_end() {
    let source = fed("123456789世".as_bytes());
    let head = |term: &Terminal| term.cell(Area::Active, 9, 0).expect("клетка").wide;
    assert_eq!(head(&source), Wide::SpacerHead);
    assert_eq!(head(&from_snapshot(&source)), Wide::SpacerHead);
    assert_ne!(head(&from_formatter(&source)), Wide::SpacerHead);
}

#[test]
fn formatter_payload_makes_client_report_size_snapshot_does_not() {
    let source = fed(b"\x1b[?2048h");
    let size = Responder {
        size: Some(SizeReport {
            rows: 4,
            columns: 10,
            cell_width: 10,
            cell_height: 20,
        }),
        ..Responder::default()
    };
    let mut client = Terminal::new(10, 4).expect("клиент");
    client.enable_effects(size.clone()).expect("колбэки");
    client.write(&source.format(FormatOptions::VT_FULL).expect("formatter"));
    assert!(
        client.take_pty_output().starts_with(b"\x1b[48;4;10;"),
        "восстановление режима 2048 отвечает размером без запроса"
    );
    let mut restored = from_snapshot(&source);
    restored.enable_effects(size).expect("колбэки");
    assert!(restored.mode(Mode::IN_BAND_RESIZE).expect("режим 2048"));
    assert!(restored.take_pty_output().is_empty());
}

#[test]
fn snapshot_carries_default_and_dynamic_colors_but_not_scrollback_limit() {
    let white = Rgb {
        r: 0xff,
        g: 0xff,
        b: 0xff,
    };
    let mut source = Terminal::new(10, 3).expect("терминал");
    source.set_scrollback_max_lines(5).expect("история");
    source
        .set_default_colors(None, Some(white), None)
        .expect("цвета");
    source.write(b"\x1b]10;#123456\x1b\\");
    let mut restored = from_snapshot(&source);
    let colors = restored.colors().expect("цвета");
    assert_eq!(
        colors.foreground,
        Some(Rgb {
            r: 0x12,
            g: 0x34,
            b: 0x56
        })
    );
    assert_eq!(colors.background, Some(white), "цвет по умолчанию хоста");
    for line in 0..20 {
        restored.write(format!("l{line}\r\n").as_bytes());
    }
    assert!(
        restored.scrollback_rows().expect("история") > 5,
        "предел истории клиент задаёт сам"
    );
}

#[test]
fn continuation_and_snapshot_are_unavailable_over_budget_until_ground() {
    let mut term = Terminal::new(10, 3).expect("терминал");
    term.set_continuation_max_bytes(16).expect("continuation");
    term.write(b"\x1b]2;0123456789abcdefghij");
    assert_eq!(
        term.continuation().map_err(|err| err.code),
        Err(sys::GhosttyResult_GHOSTTY_INVALID_VALUE)
    );
    assert_eq!(
        term.snapshot().map_err(|err| err.code),
        Err(sys::GhosttyResult_GHOSTTY_INVALID_VALUE)
    );
    term.write(b"KL\x07X");
    assert_eq!(term.title().expect("title"), b"0123456789abcdefghijKL");
    assert_eq!(term.continuation().expect("continuation"), b"");
    assert!(term.snapshot().is_ok());
    term.write(b"\x1b[3");
    assert_eq!(term.continuation().expect("continuation"), b"\x1b[3");
}

#[test]
fn snapshot_decoder_rejects_continuation_over_its_limit() {
    let mut term = Terminal::new(10, 3).expect("терминал");
    term.set_continuation_max_bytes(64).expect("continuation");
    term.write(b"\x1b]2;0123456789");
    let snapshot = term.snapshot().expect("снимок");
    assert_eq!(
        Terminal::from_snapshot(&snapshot, 13)
            .err()
            .map(|err| err.code),
        Some(sys::GhosttyResult_GHOSTTY_LIMIT_EXCEEDED)
    );
    let restored = Terminal::from_snapshot(&snapshot, 14).expect("декодирование");
    assert_eq!(
        restored.continuation().expect("continuation"),
        b"\x1b]2;0123456789"
    );
}

#[test]
fn clipboard_read_without_reply_answers_empty_and_without_callback_stays_silent() {
    let mut term = terminal();
    term.enable_effects(Responder::default()).expect("колбэки");
    term.write(b"\x1b]52;c;?\x07");
    assert_eq!(term.take_pty_output(), b"\x1b]52;c;\x07");
    term.unset_clipboard_read().expect("колбэк снимается");
    term.write(b"\x1b]52;c;?\x07");
    assert!(term.take_pty_output().is_empty());
}

#[test]
fn clipboard_source_answers_from_closure_or_returns_without_reply() {
    let mut term = terminal();
    term.enable_effects(Responder::default()).expect("колбэки");
    let mut calls = 0;
    term.set_clipboard_source(Some(Box::new(move |request: &ClipboardRequest| {
        calls += 1;
        assert_eq!(request.mimes, ["text/plain"]);
        (calls == 1).then(|| b"hi".to_vec())
    })));
    term.write(b"\x1b]52;c;?\x07");
    assert_eq!(term.take_pty_output(), b"\x1b]52;c;aGk=\x07");
    term.write(b"\x1b]52;c;?\x07");
    assert_eq!(term.take_pty_output(), b"\x1b]52;c;\x07");
}

#[test]
fn xtversion_without_callback_answers_libghostty() {
    let mut term = terminal();
    term.enable_effects(Responder::default()).expect("колбэки");
    term.write(b"\x1b[>0q");
    assert_eq!(term.take_pty_output(), b"\x1bP>|libghostty\x1b\\");
}

/// Предел истории в строках действует, только если снят байтовый предел по умолчанию.
#[test]
fn scrollback_lines_limit_needs_byte_limit_removed() {
    let fill = |term: &mut Terminal| {
        let line = format!("{}\r\n", "x".repeat(118));
        for _ in 0..10_100 {
            term.write(line.as_bytes());
        }
        term.scrollback_rows().expect("история")
    };
    let mut capped = Terminal::new(120, 40).expect("терминал");
    capped.set_scrollback_max_lines(10_000).expect("история");
    let capped_rows = fill(&mut capped);
    assert!(
        capped_rows < 1_000,
        "{capped_rows} строк при пределе по умолчанию"
    );

    let mut full = Terminal::new(120, 40).expect("терминал");
    full.set_scrollback_max_lines(10_000).expect("история");
    full.unset_scrollback_max_bytes().expect("байтовый предел");
    let full_rows = fill(&mut full);
    // Предел строк срезает историю целыми страницами: остаток ниже предела не больше страницы.
    assert!(
        (9_000..=10_100).contains(&full_rows),
        "{full_rows} строк без байтового предела"
    );
}
