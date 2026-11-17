//! Фикстуры на настоящих PTY: квитанции готовности, байтов, `SIGWINCH` и сигналов, разрез
//! запроса и счётчик ответов, поток, темп чтения, управление заданиями `shell`, потомок `agent`,
//! переживающий закрытие PTY, завершение по сроку. Синхронизация — квитанции, без `sleep`.

#[cfg(test)]
mod pty_fixtures {
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::Duration;

    use probe_session_survival::EXIT_EXECUTION;
    use probe_session_survival::fixture::channel::{
        Command, Event, JobChange, Link, Listener, Ready, WaitStatus, unhex,
    };
    use probe_session_survival::fixture::query::{Query, ResponseKind, tally};
    use probe_session_survival::fixture::{self, Mode, flood_index};
    use probe_session_survival::observer;
    use probe_session_survival::pty::{self, PtyChild, Spawn, Winsize};

    const WAIT: Duration = Duration::from_secs(20);
    const FIXTURE: &str = env!("CARGO_BIN_EXE_probe-session-fixture");

    struct Stand {
        dir: PathBuf,
        listener: Listener,
    }

    impl Stand {
        fn new(name: &str) -> Stand {
            let dir = std::env::temp_dir().join(format!(
                "pss-fx-{name}-{}-{}",
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
                size: Winsize::cells(24, 80),
            })
            .unwrap();
            let (link, ready) = self.accept();
            assert_eq!(ready.nonce, nonce);
            assert_eq!(ready.mode, mode);
            assert_eq!(ready.pid, child.pid());
            (child, link, ready)
        }

        fn accept(&self) -> (Link, Ready) {
            let mut link = self.listener.accept(WAIT).unwrap();
            let ready = link
                .wait_for(WAIT, |receipt| match &receipt.event {
                    Event::Ready(ready) => Some(ready.clone()),
                    _ => None,
                })
                .unwrap();
            (link, ready)
        }
    }

    impl Drop for Stand {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn master(child: &PtyChild) -> File {
        File::from(child.master.try_clone().unwrap())
    }

    fn read_exact(file: &mut File, len: usize) -> Vec<u8> {
        let mut buf = vec![0; len];
        file.read_exact(&mut buf).unwrap();
        buf
    }

    /// Вывод PTY читается до конца в фоне: лидер сессии при выходе ждёт, пока его вывод прочитают.
    fn drain(child: &PtyChild) -> thread::JoinHandle<Vec<u8>> {
        let mut file = master(child);
        thread::spawn(move || {
            let mut out = Vec::new();
            let _ = file.read_to_end(&mut out);
            out
        })
    }

    fn read_bytes(link: &mut Link) -> Vec<u8> {
        link.wait_for(WAIT, |receipt| match &receipt.event {
            Event::Read { hex, .. } => unhex(hex),
            _ => None,
        })
        .unwrap()
    }

    fn exit_reason(link: &mut Link) -> String {
        link.wait_for(WAIT, |receipt| match &receipt.event {
            Event::Exit { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .unwrap()
    }

    /// Команда и её подтверждение по номеру команды.
    fn acked(link: &mut Link, command: Command) -> (u64, u64) {
        let id = link.send(command).unwrap();
        let (bytes, error) = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::Ack {
                    id: acked,
                    bytes,
                    error,
                } if *acked == id => Some((*bytes, error.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(error, None, "команда {id}");
        (id, bytes)
    }

    #[test]
    fn emitter_reports_bytes_and_winch_without_redraw() {
        let stand = Stand::new("emit");
        let (mut child, mut link, ready) = stand.spawn(Mode::Emitter, "emit1");
        assert!(ready.raw);
        assert_eq!(ready.tty.as_deref(), Some(child.tty.as_str()));
        assert_eq!(ready.foreground_pgid, Some(child.pid()));
        assert_eq!(ready.size, Some(Winsize::cells(24, 80)));
        let mut out = master(&child);

        out.write_all(b"ab\x03c").unwrap();
        let mut got = Vec::new();
        while got.len() < 4 {
            got.extend(read_bytes(&mut link));
        }
        assert_eq!(
            got, b"ab\x03c",
            "raw-режим: Ctrl-C приходит байтом, а не сигналом"
        );

        pty::set_winsize(child.master.as_fd(), Winsize::cells(30, 100)).unwrap();
        let size = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::Winch { size } => Some(*size),
                _ => None,
            })
            .unwrap();
        assert_eq!(size, Some(Winsize::cells(30, 100)));
        // После `SIGWINCH` в PTY не ушло ничего: первый байт вывода — байт команды стенда.
        let (_, bytes) = acked(
            &mut link,
            Command::Write {
                hex: "4d".to_owned(),
            },
        );
        assert_eq!(bytes, 1);
        assert_eq!(read_exact(&mut out, 1), b"M");

        assert_eq!(link.request(Command::Exit { code: 0 }, WAIT), Ok(0));
        assert_eq!(exit_reason(&mut link), "command");
        assert!(child.child.wait().unwrap().success());
    }

    #[test]
    fn query_writes_split_requests_and_counts_responses() {
        let stand = Stand::new("query");
        let (mut child, mut link, ready) = stand.spawn(Mode::Query, "q1");
        assert!(ready.raw);
        let mut out = master(&child);

        for query in [Query::Da1, Query::BackgroundColor, Query::XtVersion] {
            let bytes = query.bytes();
            for cut in 1..bytes.len() {
                let split = u32::try_from(cut).unwrap();
                let (id, written) = acked(
                    &mut link,
                    Command::Query {
                        query,
                        split: Some(split),
                    },
                );
                assert_eq!(written, cut as u64);
                assert_eq!(
                    read_exact(&mut out, cut),
                    bytes[..cut],
                    "{query:?} до {cut}"
                );
                let (_, rest) = acked(&mut link, Command::Rest { query: id });
                assert_eq!(rest, (bytes.len() - cut) as u64);
                assert_eq!(read_exact(&mut out, bytes.len() - cut), bytes[cut..]);
            }
        }
        let (_, whole) = acked(
            &mut link,
            Command::Query {
                query: Query::CursorPosition,
                split: None,
            },
        );
        assert_eq!(read_exact(&mut out, whole as usize), b"\x1b[6n");
        assert!(link.request(Command::Rest { query: 999 }, WAIT).is_err());
        let bad_split = Command::Query {
            query: Query::Da1,
            split: Some(3),
        };
        assert!(link.request(bad_split, WAIT).is_err());

        // Ответы разрезаны между чтениями фикстуры и перемешаны с клавишами: каждый считается
        // ровно один раз.
        out.write_all(b"\x1b[?62;").unwrap();
        let _ = read_bytes(&mut link);
        out.write_all(b"22cx\x1bOA\x1b]11;rgb:0000/0000/0000\x1b")
            .unwrap();
        let _ = read_bytes(&mut link);
        out.write_all(b"\\\x1b[12;40R").unwrap();
        let mut responses = Vec::new();
        while responses.len() < 3 {
            responses.push(
                link.wait_for(WAIT, |receipt| match &receipt.event {
                    Event::Response {
                        response, index, ..
                    } => Some((*index, *response)),
                    _ => None,
                })
                .unwrap(),
            );
        }
        assert_eq!(
            responses,
            [
                (1, ResponseKind::Da1),
                (2, ResponseKind::BackgroundColor),
                (3, ResponseKind::CursorPosition)
            ]
        );
        let counts = tally(link.log());
        assert_eq!(counts.values().sum::<u64>(), 3, "{counts:?}");
        assert_eq!(counts.get(&ResponseKind::Da1), Some(&1));

        assert_eq!(link.request(Command::Exit { code: 0 }, WAIT), Ok(0));
        assert!(child.child.wait().unwrap().success());
    }

    #[test]
    fn flood_writes_numbered_lines_with_progress() {
        let stand = Stand::new("flood");
        let (mut child, mut link, _) = stand.spawn(Mode::Flood, "fl1");
        let mut out = master(&child);
        link.request(
            Command::Flood {
                lines: 5,
                width: 30,
                interval_us: 0,
                report_every: 2,
            },
            WAIT,
        )
        .unwrap();
        let done = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::FloodDone {
                    lines,
                    bytes,
                    cancelled,
                    writes,
                    ..
                } => Some((*lines, *cancelled, *bytes, writes.bytes)),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            done,
            (5, false, 5 * 32, 5 * 32),
            "строк, отмена, байты, записи"
        );
        let progress: Vec<u64> = link
            .log()
            .iter()
            .filter_map(|receipt| match receipt.event {
                Event::FloodProgress { lines, .. } => Some(lines),
                _ => None,
            })
            .collect();
        assert_eq!(progress, [2, 4]);
        let text = read_exact(&mut out, 5 * 32);
        let indices: Vec<u64> = text
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| flood_index("fl1", line).unwrap())
            .collect();
        assert_eq!(indices, [1, 2, 3, 4, 5]);
        link.request(Command::Exit { code: 0 }, WAIT).unwrap();
        assert!(child.child.wait().unwrap().success());
    }

    #[test]
    fn slow_reader_pauses_and_resumes_reading() {
        let stand = Stand::new("slow");
        let (mut child, mut link, _) = stand.spawn(Mode::SlowReader, "sr1");
        let mut out = master(&child);
        acked(
            &mut link,
            Command::ReadRate {
                bytes_per_sec: Some(0),
            },
        );
        out.write_all(b"ab").unwrap();
        acked(
            &mut link,
            Command::ReadRate {
                bytes_per_sec: None,
            },
        );
        let resumed_at = link.log().last().unwrap().seq;
        let mut got = Vec::new();
        while got.len() < 2 {
            got.extend(read_bytes(&mut link));
        }
        assert_eq!(got, b"ab");
        let reads: Vec<u64> = link
            .log()
            .iter()
            .filter(|receipt| matches!(receipt.event, Event::Read { .. }))
            .map(|receipt| receipt.seq)
            .collect();
        assert!(
            reads.iter().all(|seq| *seq > resumed_at),
            "на паузе ничего не прочитано: чтения {reads:?}, возобновление {resumed_at}"
        );
        link.request(Command::Exit { code: 0 }, WAIT).unwrap();
        assert!(child.child.wait().unwrap().success());
    }

    #[test]
    fn echo_reflects_reads_only_while_enabled() {
        let stand = Stand::new("echo");
        let (mut child, mut link, _) = stand.spawn(Mode::SlowReader, "ec1");
        let mut out = master(&child);
        acked(&mut link, Command::Echo { on: true });
        out.write_all(b"q").unwrap();
        assert_eq!(read_bytes(&mut link), b"q");
        assert_eq!(read_exact(&mut out, 1), b"q");
        acked(&mut link, Command::Echo { on: false });
        out.write_all(b"z").unwrap();
        assert_eq!(read_bytes(&mut link), b"z");
        acked(
            &mut link,
            Command::Write {
                hex: probe_session_survival::fixture::channel::hex(b"!"),
            },
        );
        assert_eq!(
            read_exact(&mut out, 1),
            b"!",
            "после выключения эха «z» не отражён"
        );
        link.request(Command::Exit { code: 0 }, WAIT).unwrap();
        assert!(child.child.wait().unwrap().success());
    }

    #[test]
    fn echo_is_refused_in_canonical_mode() {
        let stand = Stand::new("echo-shell");
        let (mut child, mut link, _) = stand.spawn(Mode::Shell, "ec2");
        let error = link.request(Command::Echo { on: true }, WAIT).unwrap_err();
        assert!(error.to_string().contains("raw"), "{error}");
        let _drain = drain(&child);
        link.request(Command::Exit { code: 0 }, WAIT).unwrap();
        assert!(child.child.wait().unwrap().success());
    }

    fn job_change(link: &mut Link) -> (i32, JobChange, Option<i32>) {
        link.wait_for(WAIT, |receipt| match &receipt.event {
            Event::Job {
                pid,
                change,
                foreground_pgid,
                ..
            } => Some((*pid, change.clone(), *foreground_pgid)),
            _ => None,
        })
        .unwrap_or_else(|err| panic!("{err}: {:#?}", link.log()))
    }

    fn shell_done(link: &mut Link, line: &str) -> Option<i32> {
        let (error, foreground) = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::Shell {
                    line: done,
                    error,
                    foreground_pgid,
                    ..
                } if done == line => Some((error.clone(), *foreground_pgid)),
                _ => None,
            })
            .unwrap();
        assert_eq!(error, None, "{line}");
        foreground
    }

    #[test]
    fn shell_runs_job_control_on_its_pty() {
        let stand = Stand::new("shell");
        let (mut child, mut link, ready) = stand.spawn(Mode::Shell, "sh1");
        assert!(!ready.raw);
        let shell = child.pid();
        let mut out = master(&child);
        let output = drain(&child);

        out.write_all(b"run /bin/sleep 600\n").unwrap();
        let (job, change, foreground) = job_change(&mut link);
        assert_eq!(change, JobChange::Started { foreground: true });
        assert_eq!(foreground, Some(job), "задание — группа переднего плана");
        assert_eq!(
            pty::foreground_pgid(child.master.as_fd()).unwrap(),
            job,
            "та же группа со стороны мастера"
        );
        assert_eq!(observer::inspect(job).unwrap().unwrap().pgid, job);

        out.write_all(b"\x1a").unwrap();
        let (_, change, foreground) = job_change(&mut link);
        assert_eq!(
            change,
            JobChange::Stopped {
                signal: "SIGTSTP".to_owned()
            }
        );
        assert_eq!(foreground, Some(shell), "терминал вернулся к shell");
        assert_eq!(pty::foreground_pgid(child.master.as_fd()).unwrap(), shell);

        out.write_all(b"bg 1\n").unwrap();
        assert_eq!(shell_done(&mut link, "bg 1"), Some(shell));
        let (_, change, _) = job_change(&mut link);
        assert_eq!(change, JobChange::Continued { foreground: false });

        out.write_all(b"fg 1\n").unwrap();
        assert_eq!(shell_done(&mut link, "fg 1"), Some(job));
        let (_, change, _) = job_change(&mut link);
        assert_eq!(change, JobChange::Continued { foreground: true });

        out.write_all(b"\x03").unwrap();
        let (_, change, foreground) = job_change(&mut link);
        assert_eq!(
            change,
            JobChange::Signaled {
                signal: "SIGINT".to_owned()
            }
        );
        assert_eq!(foreground, Some(shell));

        out.write_all(b"\x03").unwrap();
        let signal = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::Signal { signal } => Some(signal.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(signal, "SIGINT", "без задания Ctrl-C получает сама shell");

        out.write_all(b"exit 0\n").unwrap();
        assert_eq!(exit_reason(&mut link), "command");
        drop(out);
        assert!(child.child.wait().unwrap().success());
        drop(child);
        let _ = output.join();
    }

    #[test]
    fn agent_child_ignores_hangup_and_outlives_the_pty() {
        let stand = Stand::new("agent");
        let (child, mut link, _) = stand.spawn(Mode::Agent, "ag1");
        let spawned = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::Spawned { pid, mode } => Some((*pid, *mode)),
                _ => None,
            })
            .unwrap();
        assert_eq!(spawned.1, Mode::NohupChild);
        let (mut child_link, child_ready) = stand.accept();
        assert_eq!(child_ready.nonce, "ag1-child");
        assert_eq!(child_ready.pid, spawned.0);
        assert_eq!(child_ready.ppid, child.pid());
        assert_eq!(child_ready.sid, child.pid(), "потомок в сессии агента");
        assert_eq!(child_ready.tty, None);
        let key = observer::inspect(spawned.0).unwrap().unwrap().key;

        // Закрытие мастера — потеря терминала: агент выходит, потомок живёт.
        let PtyChild {
            master,
            child: mut process,
            ..
        } = child;
        drop(master);
        let reason = exit_reason(&mut link);
        assert!(reason == "hangup" || reason == "pty-closed", "{reason}");
        assert!(process.wait().unwrap().success());
        assert!(observer::identify(key).unwrap().is_alive());

        assert_eq!(child_link.request(Command::Exit { code: 0 }, WAIT), Ok(0));
        assert_eq!(exit_reason(&mut child_link), "command");
    }

    fn exit_code(link: &mut Link) -> (String, u8) {
        link.wait_for(WAIT, |receipt| match &receipt.event {
            Event::Exit { reason, code, .. } => Some((reason.clone(), *code)),
            _ => None,
        })
        .unwrap()
    }

    #[test]
    fn fixtures_end_by_their_lifetime_without_descendants() {
        let stand = Stand::new("life");
        let (mut child, mut link, _) = stand.spawn(Mode::Agent, "lf1");
        let output = drain(&child);
        let spawned = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::Spawned { pid, .. } => Some(*pid),
                _ => None,
            })
            .unwrap();
        let (mut child_link, child_ready) = stand.accept();
        assert_eq!(child_ready.pid, spawned);

        // Срок потомка истёк: он выходит сам, агент прибирает его и сообщает статус.
        assert_eq!(child_link.request(Command::Lifetime { ms: 0 }, WAIT), Ok(0));
        assert_eq!(
            exit_code(&mut child_link),
            ("timeout".to_owned(), EXIT_EXECUTION)
        );
        let status = link
            .wait_for(WAIT, |receipt| match &receipt.event {
                Event::ChildExit { pid, status } if *pid == spawned => Some(*status),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            status,
            WaitStatus::Exited {
                code: i32::from(EXIT_EXECUTION)
            }
        );
        assert_eq!(observer::descendants(child.pid()).unwrap(), []);

        assert_eq!(link.request(Command::Lifetime { ms: 0 }, WAIT), Ok(0));
        assert_eq!(exit_code(&mut link), ("timeout".to_owned(), EXIT_EXECUTION));
        assert_eq!(
            child.child.wait().unwrap().code(),
            Some(i32::from(EXIT_EXECUTION))
        );
        drop(child);
        let _ = output.join();
    }
}
