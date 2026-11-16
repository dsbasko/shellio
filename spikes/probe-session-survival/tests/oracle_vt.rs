//! FFI-тесты обратного маршрута оракула на настоящем PTY (функция `vt`, ADR-0035). Кандидата между
//! нагрузкой и приёмником нет: фикстура сама сидит в PTY attach-клиента, как при кандидате, который
//! передаёт байты без изменений. Синхронизация — квитанции фикстуры и условия над приёмником, без
//! `sleep`.

#![cfg(feature = "vt")]

#[cfg(test)]
mod route {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use probe_session_survival::fixture::channel::{Command, Event, Link, Listener, Ready, unhex};
    use probe_session_survival::fixture::query::{Query, ResponseKind, ResponseParser, tally};
    use probe_session_survival::fixture::{self, Mode};
    use probe_session_survival::oracle::model::{Outcome, compare};
    use probe_session_survival::oracle::suffix::{self, Step};
    use probe_session_survival::oracle::vt::{
        Capture, Config, JournalEntry, Journaled, Receiver, ReceiverProfile, Reference, Route,
        RouteEnd, ThroughCandidate,
    };
    use probe_session_survival::pty::{self, PtyChild, Spawn, Winsize};

    const WAIT: Duration = Duration::from_secs(20);
    const FIXTURE: &str = env!("CARGO_BIN_EXE_probe-session-fixture");
    const COLS: u16 = 40;
    const ROWS: u16 = 10;

    struct Stand {
        dir: PathBuf,
        listener: Listener,
    }

    impl Stand {
        fn new(name: &str) -> Stand {
            let dir = std::env::temp_dir().join(format!(
                "pss-or-{name}-{}-{}",
                std::process::id(),
                probe_session_survival::sys::uptime_ns()
            ));
            std::fs::create_dir(&dir).unwrap();
            let listener = Listener::bind(&dir.join("c.sock")).unwrap();
            Stand { dir, listener }
        }

        fn spawn(&self, mode: Mode, nonce: &str) -> (PtyChild, Link, Ready) {
            let args = fixture::command_args(
                mode,
                self.listener.path(),
                nonce,
                Some(Duration::from_secs(60)),
            );
            let env = vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())];
            let child = pty::spawn(&Spawn {
                program: Path::new(FIXTURE),
                args: &args,
                env: &env,
                size: Winsize::cells(ROWS, COLS),
            })
            .unwrap();
            let mut link = self.listener.accept(WAIT).unwrap();
            let ready = link
                .wait_for(WAIT, |receipt| match &receipt.event {
                    Event::Ready(ready) => Some(ready.clone()),
                    _ => None,
                })
                .unwrap();
            assert_eq!(ready.pid, child.pid());
            assert!(ready.raw);
            (child, link, ready)
        }
    }

    impl Drop for Stand {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn route(child: &PtyChild, client: &str) -> Route {
        let config = Config::new(COLS, ROWS);
        let receiver = Receiver::new(client, &config, ReceiverProfile::for_client(client)).unwrap();
        Route::start(child.master.try_clone().unwrap(), receiver).unwrap()
    }

    /// Запрос и ответ контрольного CPR: когда пришла его квитанция, ответы на всё, что фикстура
    /// отправила раньше, уже прочитаны ею.
    fn sentinel(link: &mut Link) {
        link.request(
            Command::Query {
                query: Query::CursorPosition,
                split: None,
            },
            WAIT,
        )
        .unwrap();
        // Каждый вызов забирает ровно одну квитанцию CPR: k-я квитанция — ответ на k-й запрос.
        link.wait_for(WAIT, |receipt| match &receipt.event {
            Event::Response {
                response: ResponseKind::CursorPosition,
                ..
            } => Some(()),
            _ => None,
        })
        .unwrap();
    }

    /// Ответы приёмника из его журнала по видам. Одна запись журнала — ответы на одно чтение
    /// маршрута, поэтому считаются разобранные ответы, а не записи.
    fn reply_kinds(journal: &[JournalEntry]) -> BTreeMap<ResponseKind, u64> {
        let mut bytes = Vec::new();
        for entry in journal {
            if let Journaled::Reply { hex } = &entry.event {
                bytes.extend(unhex(hex).unwrap());
            }
        }
        let mut kinds = BTreeMap::new();
        for (kind, _) in ResponseParser::default().feed(&bytes) {
            *kinds.entry(kind).or_insert(0) += 1;
        }
        kinds
    }

    /// Команда и её подтверждение без отказа; возвращает номер команды.
    fn acked(link: &mut Link, command: Command) -> u64 {
        let id = link.send(command).unwrap();
        let error = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::Ack {
                    id: acked, error, ..
                } if *acked == id => Some(error.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(error, None, "команда {id}");
        id
    }

    fn exit(mut child: PtyChild, mut link: Link, mut route: Route) {
        assert_eq!(link.request(Command::Exit { code: 0 }, WAIT), Ok(0));
        assert!(child.child.wait().unwrap().success());
        // Нагрузка вышла, PTY закрыт с той стороны: маршрут заканчивается сам.
        let ended = route.wait_until(WAIT, |_, _| false);
        assert!(
            matches!(
                ended,
                Err(probe_session_survival::oracle::vt::WaitError::Ended(
                    RouteEnd::Closed
                ))
            ),
            "{ended:?}"
        );
        assert_eq!(route.stop(), RouteEnd::Closed);
    }

    #[test]
    fn receiver_answers_da_once_through_return_route() {
        let stand = Stand::new("da");
        let (child, mut link, _) = stand.spawn(Mode::Query, "da1");
        let route = route(&child, "c1");

        let bytes = Query::Da1.bytes();
        let mut sent = 0u64;
        // Целиком и разрезанным на каждой границе: ответ — один на запрос.
        for cut in std::iter::once(None).chain((1..bytes.len()).map(Some)) {
            let split = cut.map(|cut| u32::try_from(cut).unwrap());
            let id = acked(
                &mut link,
                Command::Query {
                    query: Query::Da1,
                    split,
                },
            );
            if split.is_some() {
                link.request(Command::Rest { query: id }, WAIT).unwrap();
            }
            sent += 1;
            sentinel(&mut link);
            let counts = tally(link.log());
            assert_eq!(
                counts.get(&ResponseKind::Da1),
                Some(&sent),
                "разрез {cut:?}: {counts:?}"
            );
        }
        let da1: Vec<String> = link
            .log()
            .iter()
            .filter_map(|receipt| match &receipt.event {
                Event::Response {
                    response: ResponseKind::Da1,
                    hex,
                    ..
                } => Some(hex.clone()),
                _ => None,
            })
            .collect();
        let profile = probe_session_survival::fixture::channel::hex(b"\x1b[?62;22;52c");
        assert!(da1.iter().all(|hex| *hex == profile), "{da1:?}");
        let replies = route.with(|rx| reply_kinds(rx.journal()));
        assert_eq!(
            replies.get(&ResponseKind::Da1),
            Some(&sent),
            "журнал приёмника: {replies:?}"
        );
        let stats = route.stats();
        assert!(stats.read > 0 && stats.written > 0, "{stats:?}");
        exit(child, link, route);
    }

    #[test]
    fn two_routes_answer_their_own_fixture_and_keep_separate_journals() {
        let stand = Stand::new("two");
        let (first_child, mut first_link, _) = stand.spawn(Mode::Query, "two1");
        let (second_child, mut second_link, _) = stand.spawn(Mode::Query, "two2");
        let first = route(&first_child, "c1");
        let second = route(&second_child, "c2");

        first_link
            .request(
                Command::Query {
                    query: Query::XtVersion,
                    split: None,
                },
                WAIT,
            )
            .unwrap();
        sentinel(&mut first_link);
        sentinel(&mut second_link);
        let version = |link: &Link| -> Vec<String> {
            link.log()
                .iter()
                .filter_map(|receipt| match &receipt.event {
                    Event::Response {
                        response: ResponseKind::XtVersion,
                        hex,
                        ..
                    } => Some(hex.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            version(&first_link),
            [probe_session_survival::fixture::channel::hex(
                b"\x1bP>|shellio-probe-receiver(c1)\x1b\\"
            )]
        );
        assert!(version(&second_link).is_empty());
        let first_replies = first.with(|rx| reply_kinds(rx.journal()));
        let second_replies = second.with(|rx| reply_kinds(rx.journal()));
        let cpr =
            |kinds: &BTreeMap<ResponseKind, u64>| kinds.get(&ResponseKind::CursorPosition).copied();
        assert_eq!(first_replies.get(&ResponseKind::XtVersion), Some(&1));
        assert_eq!(cpr(&first_replies), Some(1));
        assert_eq!(second_replies.get(&ResponseKind::XtVersion), None);
        assert_eq!(cpr(&second_replies), Some(1));
        exit(first_child, first_link, first);
        exit(second_child, second_link, second);
    }

    #[test]
    fn suffixes_through_identity_candidate_match_the_reference() {
        let stand = Stand::new("sfx");
        let (child, mut link, _) = stand.spawn(Mode::Emitter, "sfx1");
        let route = route(&child, "c1");
        let mut reference = Reference::new(&Config::new(COLS, ROWS)).unwrap();

        // Состояние до attach: перо, гиперссылка, DECSC, поля прокрутки с DECOM, табулятор, стек Kitty,
        // alternate screen и отложенный перенос.
        let prefix =
            b"\x1b[1;32mgreen\x1b[0m \x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\\r\n\
    \x1b[3;5H\x1b7\x1b[2;8r\x1b[?6h\x1b[1;20H\x1bH\x1b[>1u\x1b[>3u\x1b[?1049h\x1b[>5u\
    alt screen\x1b[?1h\x1b[?2004h\x1b[10;1H0123456789012345678901234567890123456789";
        let steps = vec![Step::Output(prefix.to_vec())];
        suffix::apply(&steps, &mut reference).unwrap();
        {
            let mut through = ThroughCandidate {
                link: &mut link,
                route: &route,
                timeout: WAIT,
            };
            suffix::apply(&steps, &mut through).unwrap();
        }
        let compare_now = |reference: &Reference, stage: &str| {
            let a = reference.capture(Capture::default()).unwrap();
            let b = route.with(|rx| rx.capture(Capture::default())).unwrap();
            let comparison = compare(&a, &b);
            assert_eq!(
                comparison.outcome(),
                Outcome::Equal,
                "{stage}: {comparison}"
            );
            a
        };
        let before = compare_now(&reference, "до суффиксов");
        assert!(before.cursor.pending_wrap);

        for suffix in suffix::catalog(COLS, ROWS) {
            suffix::apply(&suffix.steps, &mut reference).unwrap();
            let mut through = ThroughCandidate {
                link: &mut link,
                route: &route,
                timeout: WAIT,
            };
            suffix::apply(&suffix.steps, &mut through).unwrap();
            compare_now(&reference, suffix.name);
        }
        let after = compare_now(&reference, "после суффиксов");
        assert_eq!((after.cols, after.rows), (COLS, ROWS));
        assert_eq!(
            pty::winsize(std::os::fd::AsFd::as_fd(&child.master)).unwrap(),
            Config::new(COLS, ROWS).winsize(COLS, ROWS)
        );
        exit(child, link, route);
    }
}
