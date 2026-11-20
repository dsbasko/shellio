//! Тесты аргументов фикстуры, темпа нагрузки, командной строки `login` и записи событий.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;

use super::receipts::{self, Event, ExitReason, KeySource};
use super::*;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "probe-resources-fixture-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn parse_argv(argv: Vec<String>) -> Result<Args, String> {
    assert_eq!(argv[0], "--fixture");
    parse(argv[1..].to_vec())
}

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

/// Слова строки оболочки для проверки кавычек: пробелы делят слова, `'…'` — буквально, `\'`
/// вне кавычек — кавычка. Другого синтаксиса [`shell_quote`] не порождает.
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                for inner in chars.by_ref() {
                    if inner == '\'' {
                        break;
                    }
                    word.push(inner);
                }
            }
            '\\' => {
                started = true;
                word.push(chars.next().unwrap());
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

#[test]
fn spec_argv_round_trips_through_parse() {
    let mut load = Spec::new(Mode::Load, "load-150");
    load.percent = Some(150.0);
    let mut memory = Spec::new(Mode::Memory, "mem-512");
    memory.mib = Some(512);
    let mut tree = Spec::new(Mode::Tree, "s1/p");
    tree.children = 2;
    tree.grandchildren = 1;
    tree.double_fork = 1;
    let mut host = Spec::new(Mode::Host, "host-a");
    host.children = 1;
    let mut login_host = Spec::new(Mode::Host, "host-l");
    login_host.program = Program::Login;
    for spec in [
        Spec::new(Mode::Idle, "attach-a"),
        load,
        memory,
        tree,
        Spec::new(Mode::QuickExit, "churn.q1"),
        Spec::new(Mode::DoubleFork, "s1/p/d1"),
        host,
        login_host,
        Spec::new(Mode::LoginChild, "host-l/p/c"),
    ] {
        let argv = spec.argv("/tmp/s dir/fx.sock", "/x/out", Duration::from_secs(90));
        let args = parse_argv(argv).unwrap();
        assert_eq!(args.spec, spec);
        assert_eq!(args.channel, "/tmp/s dir/fx.sock");
        assert_eq!(args.out, "/x/out");
        assert_eq!(args.lifetime, Duration::from_secs(90));
    }
}

#[test]
fn parse_rejects_bad_arguments() {
    let base = ["--channel", "/tmp/c", "--name", "f", "--out", "/tmp/o"];
    let with = |mode: &str, extra: &[&str]| -> Result<Args, String> {
        let mut argv = vec![mode.to_owned()];
        argv.extend(strings(&base));
        argv.extend(strings(extra));
        parse(argv)
    };
    assert!(with("idle", &[]).is_ok());
    assert_eq!(with("idle", &[]).unwrap().lifetime, DEFAULT_LIFETIME);
    let cases: [(&str, &[&str], &str); 16] = [
        ("loader", &[], "неизвестный режим"),
        ("load", &[], "--percent"),
        ("load", &["--percent", "0"], "(0, 200]"),
        ("load", &["--percent", "200.5"], "(0, 200]"),
        ("load", &["--percent", "NaN"], "(0, 200]"),
        ("idle", &["--percent", "10"], "только у load"),
        ("memory", &[], "--mib"),
        ("memory", &["--mib", "0"], "1..="),
        ("idle", &["--mib", "16"], "только у memory"),
        ("idle", &["--children", "1"], "только у tree и host"),
        ("tree", &["--children", "65"], "больше"),
        ("host", &[], "--program"),
        ("tree", &["--program", "tree"], "только у host"),
        (
            "host",
            &["--program", "login", "--children", "1"],
            "под login",
        ),
        ("idle", &["--lifetime-s", "0"], "--lifetime-s"),
        ("idle", &["--bogus", "1"], "неизвестный флаг"),
    ];
    for (mode, extra, expected) in cases {
        let err = with(mode, extra).unwrap_err();
        assert!(err.contains(expected), "{mode} {extra:?}: {err}");
    }
    assert!(
        with("idle", &["--lifetime-s", "3601"])
            .unwrap_err()
            .contains("1..=3600")
    );
    assert!(parse(strings(&["idle", "--name", "bad name"])).is_err());
    assert!(
        parse(strings(&["idle", "--name", "f", "--out", "/o"]))
            .unwrap_err()
            .contains("--channel")
    );
    assert!(
        parse(strings(&["--channel", "/c"]))
            .unwrap_err()
            .contains("режим")
    );
    assert!(
        parse(strings(&["idle", "--name"]))
            .unwrap_err()
            .contains("значение")
    );
}

#[test]
fn pace_uses_one_worker_up_to_one_core_and_two_above() {
    let cases = [
        (10.0, 1, 0.10),
        (50.0, 1, 0.50),
        (100.0, 1, 1.00),
        (150.0, 2, 0.75),
        (200.0, 2, 1.00),
    ];
    for (percent, workers, share) in cases {
        let pace = Pace::new(percent).unwrap();
        assert_eq!(pace.workers, workers, "{percent}");
        assert!((pace.share - share).abs() < 1e-12, "{percent}");
    }
    assert_eq!(Pace::new(10.0).unwrap().quota_ns(10_000_000), 1_000_000);
    assert_eq!(Pace::new(150.0).unwrap().quota_ns(10_000_000), 7_500_000);
    for bad in [0.0, -5.0, 200.01, f64::INFINITY, f64::NAN] {
        assert!(Pace::new(bad).is_err(), "{bad}");
    }
}

#[test]
fn next_period_keeps_the_schedule_and_skips_missed_periods() {
    assert_eq!(next_period(0, 10), 1);
    assert_eq!(next_period(9, 10), 1);
    assert_eq!(next_period(10, 10), 2);
    assert_eq!(next_period(35, 10), 4);
    assert_eq!(next_period(5, 0), 6);
}

#[test]
fn shell_quote_survives_spaces_quotes_and_metacharacters() {
    for word in [
        "",
        "plain",
        "/Users/a b/shellio-p08/target/debug/probe-resources",
        "it's",
        "''",
        "$HOME `id` \\n \"x\" ; &&",
    ] {
        let quoted = shell_quote(word);
        assert!(
            quoted.starts_with('\'') && quoted.ends_with('\''),
            "{quoted}"
        );
        assert_eq!(shell_words(&quoted), vec![word.to_owned()], "{quoted}");
    }
    assert_eq!(shell_quote("it's"), r"'it'\''s'");
}

#[test]
fn login_argv_matches_the_ghostty_surface_command() {
    let command = strings(&[
        "/Users/a b/it's/probe-resources",
        "--fixture",
        "login-child",
        "--channel",
        "/var/folders/T/shellio-probe-resources.x/fx-1.sock",
    ]);
    let argv = login_argv("dev", false, &command);
    assert_eq!(
        argv[..8],
        strings(&[
            "/usr/bin/login",
            "-flp",
            "dev",
            "/bin/bash",
            "--noprofile",
            "--norc",
            "-c",
            &argv[7],
        ])[..]
    );
    assert_eq!(argv.len(), 8);
    let script = &argv[7];
    let words = script
        .strip_prefix("exec -l ")
        .unwrap_or_else(|| panic!("{script}"));
    assert_eq!(shell_words(words), command);
    assert_eq!(
        script,
        r"exec -l '/Users/a b/it'\''s/probe-resources' '--fixture' 'login-child' '--channel' '/var/folders/T/shellio-probe-resources.x/fx-1.sock'"
    );

    let hushed = login_argv("dev", true, &command);
    assert_eq!(hushed[..3], strings(&["/usr/bin/login", "-q", "-flp"])[..]);
    assert_eq!(hushed[4..], argv[3..]);
}

#[test]
fn login_child_argv_from_the_host_is_parseable_after_the_shell() {
    let child = Spec::new(Mode::LoginChild, "host-l/p/c");
    let mut command = vec!["/x/probe-resources".to_owned()];
    command.extend(child.argv("/tmp/fx.sock", "/x/o u t", Duration::from_secs(30)));
    let argv = login_argv("dev", false, &command);
    let words = shell_words(argv[7].strip_prefix("exec -l ").unwrap());
    assert_eq!(words[0], "/x/probe-resources");
    let args = parse_argv(words[1..].to_vec()).unwrap();
    assert_eq!(args.spec, child);
    assert_eq!(args.out, "/x/o u t");
}

#[test]
fn child_key_reads_own_process_and_reports_missing_pid() {
    let own = std::process::id() as i32;
    let found = child_key(own).unwrap();
    assert_eq!(found.key.pid, own);
    assert_eq!(found.source, KeySource::PidInfo);
    let listed = proc::list_all()
        .unwrap()
        .into_iter()
        .find(|listed| listed.pid == own)
        .unwrap();
    assert_eq!(found.key, Key::of(&listed));
    assert!(child_key(99_999_999).is_err());
}

#[test]
fn reporter_writes_the_file_before_the_frame_and_stops_after_exit() {
    let out = temp_dir("reporter");
    let (ours, theirs) = UnixStream::pair().unwrap();
    let mut spec = Spec::new(Mode::Memory, "mem/16");
    spec.mib = Some(16);
    let reporter = Reporter::new(ours, &out, &spec).unwrap();
    reporter
        .send(Event::Hello {
            name: spec.name.clone(),
            mode: spec.mode,
            pid: std::process::id() as i32,
        })
        .unwrap();
    let before = reporter.receipt(&spec, "before", None, 0, 0).unwrap();
    let ready = reporter.receipt(&spec, "ready", None, 16 << 20, 0).unwrap();
    let marked = reporter.receipt(&spec, "t0", Some(5), 16 << 20, 0).unwrap();
    assert_eq!((before.seq, ready.seq, marked.seq), (1, 2, 3));
    assert!(ready.boundary.t_begin_ns <= ready.boundary.t_end_ns);
    assert!(before.boundary.t_end_ns <= ready.boundary.t_begin_ns);
    assert_eq!(ready.process.pid, std::process::id() as i32);
    assert_eq!(ready.start_abstime(), marked.start_abstime());
    assert!(ready.boundary.own.is_ok());
    reporter.close(ExitReason::Command).unwrap();
    reporter
        .send(Event::ChildExit {
            pid: 1,
            status: WaitStatus {
                code: Some(0),
                signal: None,
            },
        })
        .unwrap();

    let frames: Vec<Event> = BufReader::new(theirs)
        .lines()
        .map(|line| receipts::decode_event(&line.unwrap()).unwrap())
        .collect();
    let log = receipts::read_log(&out, &spec.name).unwrap();
    assert_eq!(log.events, frames);
    assert_eq!(frames.len(), 5);
    assert!(
        matches!(&frames[1], Event::Receipt { id: None, receipt } if receipt.label == "before")
    );
    assert!(matches!(&frames[2], Event::Ready { receipt } if receipt.allocated_bytes == 16 << 20));
    assert!(matches!(&frames[3], Event::Receipt { id: Some(5), .. }));
    assert!(matches!(
        &frames[4],
        Event::Exit {
            reason: ExitReason::Command
        }
    ));
    std::fs::remove_dir_all(&out).unwrap();
}

#[test]
fn reporter_keeps_the_file_when_the_channel_is_gone() {
    let out = temp_dir("broken");
    let (ours, theirs) = UnixStream::pair().unwrap();
    drop(theirs);
    let spec = Spec::new(Mode::Idle, "idle-1");
    let reporter = Reporter::new(ours, &out, &spec).unwrap();
    reporter
        .send(Event::Hello {
            name: spec.name.clone(),
            mode: spec.mode,
            pid: 1,
        })
        .unwrap();
    reporter.receipt(&spec, "ready", None, 0, 0).unwrap();
    reporter.close(ExitReason::ChannelClosed).unwrap();
    let log = receipts::read_log(&out, &spec.name).unwrap();
    assert_eq!(log.events.len(), 3);
    std::fs::remove_dir_all(&out).unwrap();
}

#[test]
fn stop_wakes_waiters_without_waiting_for_the_deadline() {
    let stop = Stop::new();
    assert!(!stop.wait_until(Instant::now()));
    stop.stop();
    assert!(stop.wait_until(Instant::now() + Duration::from_secs(3600)));
}
