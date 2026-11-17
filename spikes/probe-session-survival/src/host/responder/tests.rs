use super::*;

/// VT-заглушка: отвечает меткой на каждый известный запрос, который закончился в поданном шаге.
/// OSC с терминатором ST VT исполняет уже на ESC, как разборщик ghostty, поэтому ключ OSC 11 —
/// без `\`.
struct Fake {
    seen: Vec<u8>,
}

const KNOWN: [(&[u8], &[u8]); 13] = [
    (b"\x1b[c", b"<da1>"),
    (b"\x1b[>c", b"<da2>"),
    (b"\x1b[6n", b"<cpr>"),
    (b"\x1b[5n", b"<dsr>"),
    (b"\x1b[?2004$p", b"<decrqm>"),
    (b"\x1b[>0q", b"<xtversion>"),
    (b"\x1b]11;?\x1b", b"<osc11>"),
    (b"\x1b]52;c;?\x07", b"<clip>"),
    (b"\x1b[?2048h", b"<size>"),
    // Вне набора: на них отвечает терминал клиента, ответ VT хоста отбрасывается.
    (b"\x1b]12;?\x07", b"<osc12>"),
    (b"\x1b]12;?\x1b", b"<osc12>"),
    (b"\x1b]4;1;?;2;rgb:00/00/00\x07", b"<osc4>"),
    (b"\x1b_Gi=1,a=q\x1b", b"<apc>"),
];

impl Answer for Fake {
    fn answer(&mut self, raw: &[u8]) -> Vec<u8> {
        let before = self.seen.len();
        self.seen.extend_from_slice(raw);
        let mut out = Vec::new();
        for end in before + 1..=self.seen.len() {
            for (query, reply) in KNOWN {
                if self.seen[..end].ends_with(query) {
                    out.extend_from_slice(reply);
                }
            }
        }
        out
    }
}

/// Поток со всеми видами: обслуженные запросы, барьер вне набора, режим отчётов в смешанной CSI,
/// гиперссылка, запись OSC 52, DCS и UTF-8.
fn stream() -> Vec<u8> {
    let mut s = Vec::new();
    for part in [
        &b"hi \xd0\xbf\x1b[1m"[..],
        b"\x1b[c",
        b"\x1b[>c",
        b"\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\",
        b"\x1b[6n",
        b"\x1b[5n",
        b"\x1b[?2004$p",
        b"\x1b]52;c;aGVsbG8=\x07",
        b"\x1b[>0q",
        b"\x1b]11;?\x1b\\",
        b"\x1b]2;title\x07",
        b"\x1b[?1004;2004h",
        b"\x1b]52;c;?\x07",
        b"\x1bP$qm\x1b\\",
        b"\x1b[?2048h",
        b"end",
    ] {
        s.extend_from_slice(part);
    }
    s
}

fn expected_client() -> Vec<u8> {
    let mut s = Vec::new();
    for part in [
        &b"hi \xd0\xbf\x1b[1m"[..],
        b"\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\",
        b"\x1b[5n",
        b"\x1b]52;c;aGVsbG8=\x07",
        b"\x1b]2;title\x07",
        b"\x1b[?1004;2004h",
        b"\x1bP$qm\x1b\\",
        b"\x1b[?2048h",
        b"end",
    ] {
        s.extend_from_slice(part);
    }
    s
}

const EXPECTED_REPLIES: &[u8] = b"<da1><da2><cpr><decrqm><xtversion><osc11><clip><size>";

fn run(chunks: &[&[u8]]) -> (Fed, Vec<u8>, Responder) {
    let mut responder = Responder::new();
    let mut fake = Fake { seen: Vec::new() };
    let mut total = Fed::default();
    for chunk in chunks {
        let fed = responder.feed(&mut fake, chunk);
        total.replies.extend(fed.replies);
        total.client.extend(fed.client);
        total.dropped.extend(fed.dropped);
        total.served.extend(fed.served);
    }
    (total, fake.seen, responder)
}

#[test]
fn every_cut_gives_the_same_replies_and_client_stream() {
    let stream = stream();
    for cut in 0..=stream.len() {
        let (fed, seen, responder) = run(&[&stream[..cut], &stream[cut..]]);
        assert_eq!(seen, stream, "VT получает поток целиком, разрез {cut}");
        assert_eq!(
            String::from_utf8_lossy(&fed.replies),
            String::from_utf8_lossy(EXPECTED_REPLIES),
            "ответы хоста, разрез {cut}"
        );
        assert_eq!(
            fed.dropped, b"<dsr>",
            "ответ на барьер отброшен, разрез {cut}"
        );
        assert_eq!(
            String::from_utf8_lossy(&fed.client),
            String::from_utf8_lossy(&expected_client()),
            "поток клиенту, разрез {cut}"
        );
        assert!(responder.held().is_empty());
    }
}

#[test]
fn byte_by_byte_feeding_matches_one_chunk() {
    let stream = stream();
    let chunks: Vec<&[u8]> = stream.chunks(1).collect();
    let (fed, _, _) = run(&chunks);
    assert_eq!(fed.replies, EXPECTED_REPLIES);
    assert_eq!(fed.client, expected_client());
    assert_eq!(
        fed.served,
        [
            Served::Da1,
            Served::Da2,
            Served::CursorPosition,
            Served::ModeQuery,
            Served::XtVersion,
            Served::Color,
            Served::ReportMode,
            Served::ClipboardRead,
            Served::ReportMode,
        ]
    );
}

#[test]
fn served_query_prefix_is_held_and_never_reaches_the_client() {
    let query = b"\x1b[>0q";
    for at in 1..query.len() {
        let mut responder = Responder::new();
        let mut fake = Fake { seen: Vec::new() };
        let first = responder.feed(&mut fake, &query[..at]);
        assert!(first.client.is_empty(), "префикс {at} удержан");
        assert_eq!(responder.held(), &query[..at]);
        assert_eq!(
            client_continuation(&query[..at], responder.held()).unwrap(),
            b""
        );
        let rest = responder.feed(&mut fake, &query[at..]);
        assert!(rest.client.is_empty(), "остаток {at} вырезан");
        assert_eq!(rest.replies, b"<xtversion>");
    }
}

#[test]
fn passed_through_strings_are_not_held() {
    let mut responder = Responder::new();
    let mut fake = Fake { seen: Vec::new() };
    let fed = responder.feed(&mut fake, b"\x1b]8;;http://ex");
    assert_eq!(fed.client, b"\x1b]8;;http://ex");
    assert!(responder.held().is_empty());
    assert_eq!(
        client_continuation(b"\x1b]8;;http://ex", responder.held()).unwrap(),
        b"\x1b]8;;http://ex"
    );
    // Чтение буфера обмена не решено, пока не пришёл терминатор.
    let mut responder = Responder::new();
    let fed = responder.feed(&mut fake, b"\x1b]52;c;?");
    assert!(fed.client.is_empty());
    assert_eq!(responder.held(), b"\x1b]52;c;?");
    // Запись в буфер обмена решается первым байтом данных.
    let mut responder = Responder::new();
    let fed = responder.feed(&mut fake, b"\x1b]52;c;aG");
    assert_eq!(fed.client, b"\x1b]52;c;aG");
}

#[test]
fn mixed_color_requests_and_sets_pass_to_the_client() {
    let mut responder = Responder::new();
    let mut fake = Fake { seen: Vec::new() };
    let mixed = b"\x1b]4;1;?;2;rgb:00/00/00\x07\x1b]10;rgb:11/22/33\x07";
    let fed = responder.feed(&mut fake, mixed);
    assert_eq!(fed.client, mixed);
    assert!(fed.served.is_empty());
    let fed = responder.feed(&mut fake, b"\x1b]4;1;?;2;?\x07\x1b]10;?;?\x07");
    assert!(fed.client.is_empty());
    assert_eq!(fed.served, [Served::Color, Served::Color]);
}

#[test]
fn controls_inside_a_served_csi_still_reach_the_client() {
    let mut responder = Responder::new();
    let mut fake = Fake { seen: Vec::new() };
    let fed = responder.feed(&mut fake, b"\x1b[\x086n");
    assert_eq!(fed.client, b"\x08");
    assert_eq!(fed.served, [Served::CursorPosition]);
}

#[test]
fn cancelled_and_overlong_sequences_pass_unchanged() {
    let mut responder = Responder::new();
    let mut fake = Fake { seen: Vec::new() };
    let fed = responder.feed(&mut fake, b"\x1b[6\x18x");
    assert_eq!(fed.client, b"\x1b[6\x18x");
    assert!(fed.served.is_empty());
    let mut long = b"\x1b[".to_vec();
    long.extend(std::iter::repeat_n(b'1', MAX_CSI + 4));
    let fed = responder.feed(&mut fake, &long);
    assert_eq!(fed.client, long);
    assert!(responder.held().is_empty());
}

#[test]
fn continuation_mismatch_is_reported() {
    assert_eq!(client_continuation(b"\x1b[1", b"\x1b[1").unwrap(), b"");
    assert!(client_continuation(b"\x1b]2;x", b"\x1b[").is_err());
    assert_eq!(client_continuation(b"\xd0", b"").unwrap(), b"\xd0");
}

#[test]
fn served_osc_closed_by_escape_starts_the_next_sequence() {
    // ESC закрыл запрос OSC 11 без `\`: VT ответил на ESC, а ESC начал CSI клиенту.
    let (fed, _, responder) = run(&[b"\x1b]11;?\x1b", b"[1mX"]);
    assert_eq!(fed.replies, b"<osc11>");
    assert_eq!(fed.served, [Served::Color]);
    assert_eq!(fed.client, b"\x1b[1mX");
    assert!(responder.held().is_empty());
    // До следующего байта ESC удержан: continuation клиента его не содержит.
    let (_, _, waiting) = run(&[b"\x1b]11;?\x1b"]);
    assert_eq!(waiting.held(), b"\x1b");
    assert_eq!(client_continuation(b"\x1b", waiting.held()), Ok(Vec::new()));
}

#[test]
fn reply_to_a_forwarded_string_never_joins_the_next_served_reply() {
    for (query, reply) in [
        (&b"\x1b]12;?\x07"[..], &b"<osc12>"[..]),
        (b"\x1b]4;1;?;2;rgb:00/00/00\x07", b"<osc4>"),
        (b"\x1b_Gi=1,a=q\x1b\\", b"<apc>"),
        // ESC без `\` закрыл строку и начал следующую последовательность.
        (b"\x1b]12;?\x1b", b"<osc12>"),
    ] {
        let mut stream = query.to_vec();
        stream.extend_from_slice(b"\x1b[c");
        for cut in 0..=stream.len() {
            let (fed, _, _) = run(&[&stream[..cut], &stream[cut..]]);
            let label = format!("{:?}, разрез {cut}", String::from_utf8_lossy(query));
            assert_eq!(fed.replies, b"<da1>", "{label}");
            assert_eq!(fed.dropped, reply, "{label}");
            assert_eq!(fed.served, [Served::Da1], "{label}");
        }
    }
}
