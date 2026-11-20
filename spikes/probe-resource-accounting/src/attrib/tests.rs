use super::*;
use crate::proc::fake::{FakeSource, start};
use crate::proc::units::Timebase;
use crate::proc::{ReadError, Source, Unread, Usage};
use crate::snapshot::{self, Gap, Snapshot, Value};

const MIB: u64 = 1 << 20;

/// Подставной список с ответами `getsid`. Каждый процесс задаёт `pgid` и сессию явно:
/// значения подставного источника по умолчанию сделали бы всех членами одной группы и сессии.
#[derive(Default)]
struct World {
    listing: Vec<Listed>,
    sids: BTreeMap<i32, Result<i32, ReadError>>,
}

impl World {
    fn spawn(&mut self, pid: i32, ppid: i32, pgid: i32, sid: i32, sec: u64, comm: &str) -> Key {
        let listed = Listed {
            pid,
            ppid,
            pgid,
            start: start(sec),
            ruid: 501,
            uid: 501,
            comm: comm.to_owned(),
            zombie: false,
        };
        let key = Key::of(&listed);
        self.listing.push(listed);
        self.sids.insert(pid, Ok(sid));
        key
    }

    fn exit(&mut self, key: Key) {
        self.listing.retain(|listed| Key::of(listed) != key);
        self.sids.remove(&key.pid);
    }

    fn reparent(&mut self, key: Key, ppid: i32) {
        for listed in self.listing.iter_mut().filter(|l| Key::of(l) == key) {
            listed.ppid = ppid;
        }
    }

    fn attribute(&self, claims: &Claims) -> Attribution {
        attribute(&self.listing, claims, |pid| {
            assert_ne!(pid, 0, "getsid(0) отвечает за вызывающий процесс");
            self.sids
                .get(&pid)
                .copied()
                .unwrap_or(Err(ReadError::Exited))
        })
    }
}

struct Keys {
    login: Key,
    attach: Key,
    host: Key,
    shell: Key,
    child: Key,
    grandchild: Key,
    login_b: Key,
    attach_b: Key,
    host_b: Key,
    shell_b: Key,
    foreign: Key,
    foreign_child: Key,
}

/// GUI 10 с поверхностями двух панелей (`login` → attach-клиент), два хоста вне дерева GUI со
/// своими сессиями нагрузки и посторонний процесс того же пользователя со своим ребёнком.
fn world() -> (World, Keys) {
    let mut w = World::default();
    w.spawn(0, 0, 0, 0, 1, "kernel_task");
    w.spawn(1, 0, 1, 1, 1, "launchd");
    w.spawn(10, 1, 10, 10, 100, "shellio");
    let keys = Keys {
        login: w.spawn(201, 10, 201, 201, 110, "login"),
        attach: w.spawn(202, 201, 201, 201, 110, "shellio-attach"),
        host: w.spawn(300, 1, 300, 300, 105, "shellio-host"),
        shell: w.spawn(301, 300, 301, 301, 106, "bash"),
        child: w.spawn(302, 301, 301, 301, 107, "cargo"),
        grandchild: w.spawn(303, 302, 301, 301, 108, "rustc"),
        login_b: w.spawn(211, 10, 211, 211, 111, "login"),
        attach_b: w.spawn(212, 211, 211, 211, 111, "shellio-attach"),
        host_b: w.spawn(500, 1, 500, 500, 105, "shellio-host"),
        shell_b: w.spawn(501, 500, 501, 501, 106, "zsh"),
        foreign: w.spawn(400, 1, 400, 400, 90, "zsh"),
        foreign_child: w.spawn(401, 400, 400, 400, 91, "vim"),
    };
    (w, keys)
}

fn record(host: Key, shell: Key) -> OwnerRecord {
    OwnerRecord {
        host,
        programs: vec![shell],
        group: shell,
        session: shell,
    }
}

fn panel_a(k: &Keys) -> Panel {
    Panel {
        id: "a".to_owned(),
        surface: vec![k.login],
        record: Some(record(k.host, k.shell)),
    }
}

fn panel_b(k: &Keys) -> Panel {
    Panel {
        id: "b".to_owned(),
        surface: vec![k.login_b],
        record: Some(record(k.host_b, k.shell_b)),
    }
}

fn panels(panels: Vec<Panel>) -> Claims {
    Claims {
        panels,
        git: Vec::new(),
    }
}

fn a() -> Owner {
    Owner::Panel("a".to_owned())
}

fn b() -> Owner {
    Owner::Panel("b".to_owned())
}

fn set(keys: &[Key]) -> BTreeSet<Key> {
    keys.iter().copied().collect()
}

fn origins(at: &Attribution, owner: &Owner, key: Key) -> Vec<Origin> {
    at.member(owner, key)
        .unwrap_or_else(|| panic!("{key:?} не найден у {owner:?}: {at:#?}"))
        .origins
        .iter()
        .copied()
        .collect()
}

/// Сколько раз ключ встречается во всех итогах.
fn occurrences(at: &Attribution, key: Key) -> usize {
    at.shares
        .iter()
        .flat_map(|share| &share.members)
        .filter(|member| member.key == key)
        .count()
}

fn session_a(k: &Keys) -> BTreeSet<Key> {
    set(&[k.host, k.shell, k.child, k.grandchild])
}

#[test]
fn union_counts_each_process_once_and_records_every_source() {
    let (w, k) = world();
    let at = w.attribute(&panels(vec![panel_a(&k)]));

    let mut expected = session_a(&k);
    expected.extend([k.login, k.attach]);
    assert_eq!(at.members(&a()), expected);
    assert_eq!(at.share(&a()).unwrap().members.len(), expected.len());
    assert_eq!(
        origins(&at, &a(), k.shell),
        [Origin::Record, Origin::Tree, Origin::Session, Origin::Group]
    );
    assert_eq!(
        origins(&at, &a(), k.child),
        [Origin::Tree, Origin::Session, Origin::Group]
    );
    assert_eq!(origins(&at, &a(), k.host), [Origin::Record]);
    assert_eq!(origins(&at, &a(), k.login), [Origin::Root]);
    assert_eq!(origins(&at, &a(), k.attach), [Origin::Tree]);
    assert_eq!(at.all(), expected);
    assert!(at.members(&Owner::Git).is_empty());
    assert!(at.notes.is_empty(), "{:?}", at.notes);
}

#[test]
fn two_sessions_keep_their_members_and_a_foreign_process_stays_out() {
    let (w, k) = world();
    let at = w.attribute(&panels(vec![panel_a(&k), panel_b(&k)]));

    let mut first = session_a(&k);
    first.extend([k.login, k.attach]);
    let second = set(&[k.login_b, k.attach_b, k.host_b, k.shell_b]);
    assert_eq!(at.members(&a()), first);
    assert_eq!(at.members(&b()), second);
    assert_eq!(at.all(), &first | &second);
    for key in [k.foreign, k.foreign_child] {
        assert_eq!(occurrences(&at, key), 0, "{key:?}");
    }
}

#[test]
fn process_that_left_the_tree_is_found_by_session_and_group() {
    let (mut w, k) = world();
    // Двойной `fork`: промежуточный процесс вышел, внук перешёл к launchd с группой и сессией.
    let orphan = w.spawn(304, 1, 301, 301, 120, "worker");
    // Демон со своим `setsid`: покинул и дерево, и сессию.
    let daemon = w.spawn(305, 1, 305, 305, 121, "daemon");

    let at = w.attribute(&panels(vec![panel_a(&k)]));
    assert_eq!(origins(&at, &a(), orphan), [Origin::Session, Origin::Group]);
    // ADR-0004: на программы, ставшие демонами вне дерева сессии, гарантии не распространяются.
    assert_eq!(occurrences(&at, daemon), 0);

    let mut recorded = panel_a(&k);
    if let Some(record) = recorded.record.as_mut() {
        record.programs.push(daemon);
    }
    let at = w.attribute(&panels(vec![recorded]));
    assert_eq!(origins(&at, &a(), daemon), [Origin::Record]);
}

#[test]
fn panel_move_keeps_host_and_programs_in_the_session() {
    let (mut w, k) = world();
    let before = w.attribute(&panels(vec![panel_a(&k)]));

    // Вид закрыт: attach-клиент и его `login` вышли. Панель открыта в другой зоне новым видом.
    w.exit(k.attach);
    w.exit(k.login);
    let login = w.spawn(221, 10, 221, 221, 130, "login");
    let attach = w.spawn(222, 221, 221, 221, 130, "shellio-attach");
    let mut moved = panel_a(&k);
    moved.surface = vec![login];
    let after = w.attribute(&panels(vec![moved]));

    for key in session_a(&k) {
        assert_eq!(before.member(&a(), key), after.member(&a(), key), "{key:?}");
    }
    let mut expected = session_a(&k);
    expected.extend([login, attach]);
    assert_eq!(after.members(&a()), expected);
    assert!(after.notes.is_empty(), "{:?}", after.notes);

    // Старый процесс поверхности, переданный после выхода, — заметка, а не тихий пропуск.
    let stale = w.attribute(&panels(vec![panel_a(&k)]));
    assert_eq!(stale.members(&a()), session_a(&k));
    assert_eq!(
        stale.notes,
        [Note::Exited {
            owner: a(),
            role: Role::Surface,
            key: k.login,
        }]
    );
}

#[test]
fn git_group_takes_shared_work_once_and_not_the_requesting_panels() {
    let (mut w, k) = world();
    let git = w.spawn(600, 10, 10, 10, 140, "git");
    let remote = w.spawn(601, 600, 10, 10, 140, "git-remote-https");
    let typed = w.spawn(310, 301, 301, 301, 141, "git");
    // Два запроса панелей на одну работу: один дочерний `git`.
    let claims = Claims {
        panels: vec![panel_a(&k), panel_b(&k)],
        git: vec![
            GitWork {
                child: git,
                requested_by: vec!["a".to_owned()],
            },
            GitWork {
                child: git,
                requested_by: vec!["b".to_owned()],
            },
        ],
    };
    let at = w.attribute(&claims);

    assert_eq!(at.members(&Owner::Git), set(&[git, remote]));
    assert_eq!(origins(&at, &Owner::Git, git), [Origin::Root]);
    assert_eq!(origins(&at, &Owner::Git, remote), [Origin::Tree]);
    assert_eq!(
        at.share(&Owner::Git).unwrap().requested_by,
        ["a".to_owned(), "b".to_owned()].into()
    );
    for key in [git, remote] {
        assert_eq!(occurrences(&at, key), 1, "{key:?}");
    }
    // `git`, набранный в терминале панели, — программа её сессии, а не общая работа.
    assert!(at.members(&a()).contains(&typed));
    assert!(!at.members(&Owner::Git).contains(&typed));
    assert!(at.notes.is_empty(), "{:?}", at.notes);
}

#[test]
fn git_child_registered_by_the_first_requesting_panel_goes_to_the_group() {
    let (mut w, k) = world();
    let git = w.spawn(600, 10, 10, 10, 140, "git");
    let remote = w.spawn(601, 600, 10, 10, 140, "git-remote-https");
    // Антиобразец: первая запросившая панель записала дочерний `git` своим корнем.
    let mut first = panel_a(&k);
    first.surface.push(git);
    let claims = Claims {
        panels: vec![first],
        git: vec![GitWork {
            child: git,
            requested_by: vec!["a".to_owned()],
        }],
    };
    let at = w.attribute(&claims);

    assert_eq!(at.members(&Owner::Git), set(&[git, remote]));
    assert!(at.members(&a()).is_disjoint(&set(&[git, remote])));
    assert_eq!(
        at.notes,
        [
            Note::Ceded {
                key: git,
                from: a()
            },
            Note::Ceded {
                key: remote,
                from: a()
            },
        ]
    );
}

#[test]
fn shared_session_of_two_panels_is_counted_once_overall() {
    let (w, k) = world();
    let view = |id: &str| Panel {
        id: id.to_owned(),
        surface: Vec::new(),
        record: Some(record(k.host, k.shell)),
    };
    let at = w.attribute(&panels(vec![view("a"), view("b")]));

    assert_eq!(at.members(&a()), session_a(&k));
    assert_eq!(at.members(&b()), session_a(&k));
    assert_eq!(at.all(), session_a(&k));
}

#[test]
fn pid_held_by_another_process_is_not_attributed() {
    let (mut w, k) = world();
    // Хост вышел, его программа перешла к launchd, PID хоста занял посторонний процесс.
    w.exit(k.host);
    w.reparent(k.shell, 1);
    let stranger = w.spawn(300, 1, 300, 300, 200, "stranger");
    let stranger_child = w.spawn(320, 300, 300, 300, 201, "stranger-child");

    let at = w.attribute(&panels(vec![panel_a(&k)]));
    for key in [stranger, stranger_child] {
        assert_eq!(occurrences(&at, key), 0, "{key:?}");
    }
    assert_eq!(
        origins(&at, &a(), k.shell),
        [Origin::Record, Origin::Session, Origin::Group]
    );
    assert_eq!(
        at.notes,
        [Note::Reused {
            owner: a(),
            role: Role::Host,
            key: k.host,
            listed: start(200),
        }]
    );
}

#[test]
fn ended_session_whose_number_went_to_another_leader_matches_nothing() {
    let (mut w, k) = world();
    // Сессия нагрузки кончилась целиком, PID её лидера занял новый лидер своей сессии.
    for key in [k.grandchild, k.child, k.shell] {
        w.exit(key);
    }
    let newcomer = w.spawn(301, 1, 301, 301, 210, "newcomer");
    let newcomer_child = w.spawn(330, 301, 301, 301, 211, "newcomer-child");

    let at = w.attribute(&panels(vec![panel_a(&k)]));
    assert_eq!(at.members(&a()), set(&[k.login, k.attach, k.host]));
    for key in [newcomer, newcomer_child] {
        assert_eq!(occurrences(&at, key), 0, "{key:?}");
    }
    let reused = |leader| Note::LeaderReused {
        owner: a(),
        leader,
        key: k.shell,
        listed: start(210),
    };
    assert_eq!(
        at.notes,
        [
            Note::Reused {
                owner: a(),
                role: Role::Program,
                key: k.shell,
                listed: start(210),
            },
            reused(Leader::Group),
            reused(Leader::Session),
        ]
    );
}

#[test]
fn failed_getsid_is_noted_and_the_group_still_finds_the_member() {
    let (mut w, k) = world();
    let orphan = w.spawn(304, 1, 301, 301, 120, "worker");
    w.sids.insert(orphan.pid, Err(ReadError::Denied));

    let at = w.attribute(&panels(vec![panel_a(&k)]));
    assert_eq!(origins(&at, &a(), orphan), [Origin::Group]);
    assert_eq!(
        at.notes,
        [Note::SessionUnknown {
            key: orphan,
            error: ReadError::Denied,
        }]
    );
}

#[test]
fn getsid_is_asked_once_per_listed_pid_except_kernel_task() {
    let (w, k) = world();
    let mut asked = Vec::new();
    attribute(&w.listing, &panels(vec![panel_a(&k), panel_b(&k)]), |pid| {
        asked.push(pid);
        w.sids[&pid]
    });
    let mut expected: Vec<i32> = w
        .listing
        .iter()
        .map(|l| l.pid)
        .filter(|&pid| pid != 0)
        .collect();
    expected.sort_unstable();
    asked.sort_unstable();
    assert_eq!(asked, expected);
}

#[test]
fn no_getsid_without_an_ownership_record() {
    let (w, k) = world();
    let claims = Claims {
        panels: vec![Panel {
            id: "a".to_owned(),
            surface: vec![k.login],
            record: None,
        }],
        git: Vec::new(),
    };
    let at = attribute(&w.listing, &claims, |pid| {
        panic!("getsid({pid}) без записи владения")
    });
    assert_eq!(at.members(&a()), set(&[k.login, k.attach]));
}

fn usage(pid: i32, cpu_mach: u64) -> Usage {
    Usage {
        start_abstime: 1_000 + pid as u64,
        user_mach: cpu_mach,
        system_mach: 0,
        phys_footprint: pid as u64 * MIB,
    }
}

/// Подставной источник с тем же списком и сессиями; `login` с setuid не читается.
fn source(w: &World, cpu_mach: u64) -> FakeSource {
    let mut source = FakeSource::default();
    for listed in &w.listing {
        source.add_usage(listed.clone(), usage(listed.pid, cpu_mach));
        source.session.insert(listed.pid, w.sids[&listed.pid]);
    }
    source.usage.insert(201, Err(ReadError::Denied));
    source
}

fn take(source: &FakeSource, marks: [u64; 2], claims: &Claims) -> (Snapshot, Attribution) {
    let mut marks = marks.into_iter();
    let mut found = None;
    let snapshot = Snapshot::take(
        source,
        || marks.next().unwrap(),
        |listing| {
            let at = attribute(listing, claims, |pid| source.session(pid));
            let all = at.all();
            found = Some(at);
            all
        },
    )
    .unwrap();
    (snapshot, found.unwrap())
}

#[test]
fn snapshot_counts_a_member_found_by_every_source_once_and_keeps_the_unreadable_one() {
    let (w, k) = world();
    let claims = panels(vec![panel_a(&k)]);
    let (base, _) = take(&source(&w, 0), [0, 2_000_000], &claims);
    let (now, at) = take(
        &source(&w, 24_000_000),
        [2_000_000_000, 2_002_000_000],
        &claims,
    );
    let timebase = Timebase {
        numer: 125,
        denom: 3,
    };
    let summary = snapshot::summarize(Some(&base), &now, &at.members(&a()), timebase).unwrap();

    // Пять прочитанных членов, `bash` найден четырьмя источниками и учтён один раз.
    let readable = [k.attach, k.host, k.shell, k.child, k.grandchild];
    let bytes: u64 = readable.iter().map(|key| key.pid as u64 * MIB).sum();
    assert_eq!(summary.memory.bytes, bytes);
    assert_eq!(summary.memory.coverage.counted, 5);
    assert_eq!(summary.memory.coverage.expected, 6);
    assert_eq!(summary.cpu.coverage.counted, 5);
    assert!(summary.partial);
    let login = summary.entries.iter().find(|e| e.key == k.login).unwrap();
    let denied = Gap::Unread {
        unread: Unread::Failed {
            error: ReadError::Denied,
        },
    };
    assert_eq!(login.memory, Some(Value::Missing(denied)));
    assert_eq!(login.cpu, Value::Missing(denied));
    // Каждый член за 2 с потратил 1 с CPU: 50% одного ядра на член.
    assert!((summary.cpu.pct.unwrap() - 250.0).abs() < 1e-9);
}

#[test]
fn attribution_serializes_to_json() {
    let (mut w, k) = world();
    let git = w.spawn(600, 10, 10, 10, 140, "git");
    let claims = Claims {
        panels: vec![panel_a(&k)],
        git: vec![GitWork {
            child: git,
            requested_by: vec!["a".to_owned()],
        }],
    };
    let mut at = w.attribute(&claims);
    at.notes.push(Note::SessionUnknown {
        key: k.foreign,
        error: ReadError::Denied,
    });
    let value = serde_json::to_value(&at).unwrap();
    let back: Attribution = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(back, at);
    assert_eq!(value["shares"][0]["owner"]["kind"], "git");
    assert_eq!(value["shares"][1]["owner"]["id"], "a");
}
