use super::*;
use crate::proc::fake::{FakeSource, info_of, listed, start};
use crate::proc::{ReadError, Usage};

const APPLE: Timebase = Timebase {
    numer: 125,
    denom: 3,
};
/// Секунда CPU в тиках Mach при `timebase` 125/3.
const SECOND: u64 = 24_000_000;
const MIB: u64 = 1 << 20;

fn usage(start_abstime: u64, user_mach: u64, system_mach: u64, footprint: u64) -> Usage {
    Usage {
        start_abstime,
        user_mach,
        system_mach,
        phys_footprint: footprint,
    }
}

fn key(pid: i32, sec: u64) -> Key {
    Key {
        pid,
        start: start(sec),
    }
}

fn snapshot_of(
    source: &FakeSource,
    marks: [u64; 2],
    select: impl FnOnce(&[Listed]) -> BTreeSet<Key>,
) -> Snapshot {
    let mut marks = marks.into_iter();
    Snapshot::take(source, || marks.next().unwrap(), select).unwrap()
}

/// Снимок со всеми процессами списка.
fn snapshot(source: &FakeSource, marks: [u64; 2]) -> Snapshot {
    snapshot_of(source, marks, |listing| {
        listing.iter().map(Key::of).collect()
    })
}

fn members(keys: &[Key]) -> BTreeSet<Key> {
    keys.iter().copied().collect()
}

fn entry(summary: &Summary, key: Key) -> &Entry {
    summary.entries.iter().find(|e| e.key == key).unwrap()
}

fn cpu_gap(summary: &Summary, key: Key) -> Gap {
    match entry(summary, key).cpu {
        Value::Missing(gap) => gap,
        Value::Measured(delta) => panic!("{key:?}: ожидался пропуск, учтено {delta:?}"),
    }
}

fn cpu_pct(summary: &Summary, key: Key) -> f64 {
    match entry(summary, key).cpu {
        Value::Measured(delta) => delta.pct,
        Value::Missing(gap) => panic!("{key:?}: ожидалась дельта, пропуск {gap:?}"),
    }
}

/// Базовая граница с серединой в 1 мс и текущая с серединой ровно через 2 с.
const BASE: [u64; 2] = [0, 2_000_000];
const NOW: [u64; 2] = [2_000_000_000, 2_002_000_000];

const DENIED: Unread = Unread::Failed {
    error: ReadError::Denied,
};

#[test]
fn snapshot_reads_only_the_chosen_subset_between_marks() {
    let mut source = FakeSource::default();
    for pid in [10, 11, 12] {
        source.add(listed(pid, 1, 100), 5_000, 0);
    }
    let snapshot = snapshot_of(&source, [1_000, 3_001], |listing| {
        assert_eq!(listing.len(), 3, "выбор видит весь список");
        members(&[key(11, 100)])
    });
    assert_eq!(snapshot.listed.len(), 3);
    assert_eq!(snapshot.reads.keys().collect::<Vec<_>>(), [&key(11, 100)]);
    assert_eq!(snapshot.boundary.collect_start_ns, 1_000);
    assert_eq!(snapshot.boundary.collect_end_ns, 3_001);
    assert_eq!(snapshot.boundary.t_ns(), 2_000);

    // Выбранный член прочитан, невыбранный член списка — пропуск памяти и CPU с причиной.
    let summary = summarize(
        None,
        &snapshot,
        &members(&[key(10, 100), key(11, 100)]),
        APPLE,
    )
    .unwrap();
    let skipped = entry(&summary, key(10, 100));
    assert_eq!(skipped.memory, Some(Value::Missing(Gap::NotRead)));
    assert_eq!(skipped.cpu, Value::Missing(Gap::NotRead));
    assert_eq!(skipped.identity, None);
    assert_eq!(
        summary.memory.coverage,
        Coverage {
            counted: 1,
            expected: 2
        }
    );
    assert_eq!(summary.memory.bytes, MIB);
}

#[test]
fn formulas_sum_footprint_and_user_plus_system_over_the_interval() {
    let (a, b) = (listed(10, 1, 100), listed(11, 1, 101));
    let mut before = FakeSource::default();
    before.add_usage(a.clone(), usage(5_000, SECOND, 0, 16 * MIB));
    before.add_usage(b.clone(), usage(6_000, 2 * SECOND, SECOND, 128 * MIB));
    let mut after = FakeSource::default();
    after.add_usage(a, usage(5_000, 3 * SECOND / 2, SECOND / 2, 20 * MIB));
    after.add_usage(b, usage(6_000, 4 * SECOND, 2 * SECOND, 512 * MIB));
    let (base, now) = (snapshot(&before, BASE), snapshot(&after, NOW));
    let set = members(&[key(10, 100), key(11, 101)]);
    let summary = summarize(Some(&base), &now, &set, APPLE).unwrap();

    assert_eq!(summary.cpu.interval_ns, Some(2_000_000_000));
    // 1 с за 2 с — 50%, 3 с за 2 с — 150% одного ядра без среза, итог 200%.
    assert!((cpu_pct(&summary, key(10, 100)) - 50.0).abs() < 1e-9);
    assert!((cpu_pct(&summary, key(11, 101)) - 150.0).abs() < 1e-9);
    assert_eq!(summary.cpu.delta_mach, 4 * SECOND);
    assert_eq!(summary.cpu.delta_ns, 4_000_000_000);
    assert!((summary.cpu.pct.unwrap() - 200.0).abs() < 1e-9);
    assert_eq!(summary.memory.bytes, 532 * MIB);
    assert!(!summary.partial);
    assert!(summary.memory.coverage.complete() && summary.cpu.coverage.complete());
    assert_eq!(summary.cpu.coverage.share(), 1.0);
    assert_eq!(summary.base, Some(base.boundary));
    assert_eq!(
        entry(&summary, key(11, 101)).identity,
        Some(Identity {
            pid: 11,
            start_abstime: 6_000
        })
    );
}

#[test]
fn fresh_process_has_memory_but_no_cpu_delta_and_marks_cpu_partial() {
    let mut before = FakeSource::default();
    before.add_usage(listed(10, 1, 100), usage(5_000, SECOND, 0, MIB));
    let mut after = FakeSource::default();
    after.add_usage(listed(10, 1, 100), usage(5_000, 2 * SECOND, 0, MIB));
    after.add_usage(listed(12, 1, 103), usage(7_000, 5 * SECOND, 0, 64 * MIB));
    let (base, now) = (snapshot(&before, BASE), snapshot(&after, NOW));
    let set = members(&[key(10, 100), key(12, 103)]);
    let summary = summarize(Some(&base), &now, &set, APPLE).unwrap();

    let fresh = entry(&summary, key(12, 103));
    assert_eq!(fresh.memory, Some(Value::Measured(64 * MIB)));
    assert_eq!(
        cpu_gap(&summary, key(12, 103)),
        Gap::Fresh { reused_pid: false }
    );
    // Накопленные 5 с свежего процесса за дельту не выдаются.
    assert_eq!(summary.cpu.delta_mach, SECOND);
    assert!(summary.memory.coverage.complete());
    assert_eq!(
        summary.cpu.coverage,
        Coverage {
            counted: 1,
            expected: 2
        }
    );
    assert_eq!(summary.cpu.coverage.share(), 0.5);
    assert!(summary.partial);
}

#[test]
fn exited_and_unlisted_members_are_cpu_gaps_outside_memory() {
    let mut before = FakeSource::default();
    before.add_usage(listed(10, 1, 100), usage(5_000, SECOND, 0, MIB));
    before.add_usage(listed(11, 1, 101), usage(6_000, SECOND, 0, 32 * MIB));
    let mut after = FakeSource::default();
    after.add_usage(listed(10, 1, 100), usage(5_000, 2 * SECOND, 0, MIB));
    let (base, now) = (snapshot(&before, BASE), snapshot(&after, NOW));
    let set = members(&[key(10, 100), key(11, 101), key(13, 104)]);
    let summary = summarize(Some(&base), &now, &set, APPLE).unwrap();

    assert_eq!(entry(&summary, key(11, 101)).memory, None);
    assert_eq!(cpu_gap(&summary, key(11, 101)), Gap::Exited);
    assert_eq!(entry(&summary, key(13, 104)).memory, None);
    assert_eq!(cpu_gap(&summary, key(13, 104)), Gap::NotListed);
    assert_eq!(summary.memory.bytes, MIB);
    assert!(summary.memory.coverage.complete());
    assert_eq!(
        summary.cpu.coverage,
        Coverage {
            counted: 1,
            expected: 3
        }
    );
    assert!(summary.partial);
}

#[test]
fn read_failures_on_either_boundary_leave_no_delta() {
    let (a, b, c) = (listed(10, 1, 100), listed(11, 1, 101), listed(12, 1, 102));
    let mut before = FakeSource::default();
    before.add_usage(a.clone(), usage(5_000, SECOND, 0, MIB));
    before.add_usage(b.clone(), usage(6_000, SECOND, 0, MIB));
    before.usage.insert(11, Err(ReadError::Denied));
    before.add_usage(c.clone(), usage(7_000, SECOND, 0, MIB));
    let mut after = FakeSource::default();
    after.add_usage(a, usage(5_000, 2 * SECOND, 0, MIB));
    after.usage.insert(10, Err(ReadError::Denied));
    after.add_usage(b, usage(6_000, 2 * SECOND, 0, MIB));
    after.add_usage(c, usage(7_000, 2 * SECOND, 0, MIB));
    // На базовой границе `c` в списке, но не выбран для чтения.
    let base = snapshot_of(&before, BASE, |_| members(&[key(10, 100), key(11, 101)]));
    let now = snapshot(&after, NOW);
    let set = members(&[key(10, 100), key(11, 101), key(12, 102)]);
    let summary = summarize(Some(&base), &now, &set, APPLE).unwrap();

    let denied = entry(&summary, key(10, 100));
    assert_eq!(
        denied.memory,
        Some(Value::Missing(Gap::Unread { unread: DENIED }))
    );
    assert_eq!(denied.cpu, Value::Missing(Gap::Unread { unread: DENIED }));
    assert_eq!(
        cpu_gap(&summary, key(11, 101)),
        Gap::BaseUnread { unread: DENIED }
    );
    assert_eq!(cpu_gap(&summary, key(12, 102)), Gap::BaseNotRead);
    assert_eq!(summary.cpu.coverage.counted, 0);
    assert_eq!(summary.cpu.delta_mach, 0);
    assert!(summary.partial);
}

#[test]
fn reused_pid_is_a_new_identity_and_the_delta_is_not_bridged() {
    // PID 10 на базовой границе — процесс со стартом 100 и 1000 с CPU, на текущей — новый
    // процесс со стартом 105 и 2 с CPU.
    let mut before = FakeSource::default();
    before.add_usage(listed(10, 1, 100), usage(5_000, 1_000 * SECOND, 0, MIB));
    let mut after = FakeSource::default();
    after.add_usage(listed(10, 1, 105), usage(9_000, 2 * SECOND, 0, 8 * MIB));
    let (base, now) = (snapshot(&before, BASE), snapshot(&after, NOW));

    let summary = summarize(
        Some(&base),
        &now,
        &members(&[key(10, 100), key(10, 105)]),
        APPLE,
    )
    .unwrap();
    assert_eq!(cpu_gap(&summary, key(10, 100)), Gap::Exited);
    assert_eq!(
        cpu_gap(&summary, key(10, 105)),
        Gap::Fresh { reused_pid: true }
    );
    assert_eq!(
        entry(&summary, key(10, 105)).memory,
        Some(Value::Measured(8 * MIB))
    );
    assert_eq!(summary.cpu.delta_mach, 0);
    assert_eq!(summary.cpu.coverage.counted, 0);
    assert!(summary.partial);

    // Только новый ключ во множестве: тот же вывод, дельта через PID не протягивается.
    let only_new = summarize(Some(&base), &now, &members(&[key(10, 105)]), APPLE).unwrap();
    assert_eq!(
        cpu_gap(&only_new, key(10, 105)),
        Gap::Fresh { reused_pid: true }
    );
    assert_eq!(only_new.cpu.delta_mach, 0);
}

#[test]
fn another_life_under_the_same_key_and_a_decreasing_counter_are_gaps() {
    let (a, b) = (listed(10, 1, 100), listed(11, 1, 101));
    let mut before = FakeSource::default();
    before.add_usage(a.clone(), usage(5_000, SECOND, 0, MIB));
    before.add_usage(b.clone(), usage(6_000, 3 * SECOND, 0, MIB));
    let mut after = FakeSource::default();
    after.add_usage(a, usage(5_001, 2 * SECOND, 0, MIB));
    after.add_usage(b, usage(6_000, 2 * SECOND, 0, MIB));
    let (base, now) = (snapshot(&before, BASE), snapshot(&after, NOW));
    let summary = summarize(
        Some(&base),
        &now,
        &members(&[key(10, 100), key(11, 101)]),
        APPLE,
    )
    .unwrap();
    assert_eq!(
        cpu_gap(&summary, key(10, 100)),
        Gap::IdentityChanged {
            base: 5_000,
            current: 5_001
        }
    );
    assert_eq!(
        cpu_gap(&summary, key(11, 101)),
        Gap::CounterDecreased {
            base_mach: 3 * SECOND,
            current_mach: 2 * SECOND
        }
    );
    assert_eq!(
        summary.cpu.delta_mach, 0,
        "убыль счётчика не превращается в ноль"
    );
    assert!(summary.partial);
}

#[test]
fn first_boundary_has_memory_and_unknown_cpu() {
    let mut source = FakeSource::default();
    source.add_usage(listed(10, 1, 100), usage(5_000, SECOND, 0, 16 * MIB));
    let now = snapshot(&source, NOW);
    let summary = summarize(None, &now, &members(&[key(10, 100)]), APPLE).unwrap();
    assert_eq!(summary.memory.bytes, 16 * MIB);
    assert!(summary.memory.coverage.complete());
    assert_eq!(cpu_gap(&summary, key(10, 100)), Gap::NoPrevious);
    assert_eq!(summary.base, None);
    assert_eq!(summary.cpu.interval_ns, None);
    assert_eq!(summary.cpu.pct, None, "неизвестный CPU — не 0%");
    assert!(summary.partial);
}

#[test]
fn non_monotonic_boundary_and_unread_timebase_are_errors() {
    let mut source = FakeSource::default();
    source.add(listed(10, 1, 100), 5_000, SECOND);
    let set = members(&[key(10, 100)]);
    let (base, same) = (snapshot(&source, NOW), snapshot(&source, NOW));
    let error = summarize(Some(&base), &same, &set, APPLE).unwrap_err();
    assert!(error.contains("не позже"), "{error}");
    let earlier = snapshot(&source, BASE);
    assert!(summarize(Some(&base), &earlier, &set, APPLE).is_err());
    let broken = Timebase { numer: 0, denom: 0 };
    let error = summarize(None, &base, &set, broken).unwrap_err();
    assert!(error.contains("mach_timebase_info"), "{error}");
}

#[test]
fn summary_survives_a_json_round_trip() {
    let mut before = FakeSource::default();
    before.add_usage(listed(10, 1, 100), usage(5_000, SECOND, 0, MIB));
    before.add_usage(listed(11, 1, 101), usage(6_000, SECOND, 0, MIB));
    before
        .usage
        .insert(11, Err(ReadError::Other { errno: libc::EIO }));
    let mut after = FakeSource::default();
    after.add_usage(listed(10, 1, 100), usage(5_000, 2 * SECOND, 0, MIB));
    after.add_usage(listed(11, 1, 101), usage(6_000, 2 * SECOND, 0, MIB));
    let (base, now) = (snapshot(&before, BASE), snapshot(&after, NOW));
    let set = members(&[key(10, 100), key(11, 101)]);
    let summary = summarize(Some(&base), &now, &set, APPLE).unwrap();
    let json = serde_json::to_string(&summary).unwrap();
    let back: Summary = serde_json::from_str(&json).unwrap();
    assert_eq!(back, summary);
}

/// Антиобразец `detached/crates/core/src/os.rs:78–97`: неудачные чтения пропускались, а сумма
/// остальных выглядела полной. Здесь каждый отказ остаётся в итоге с причиной.
#[test]
fn failed_measurement_is_not_hidden() {
    let (a, b, c, d, e) = (
        listed(10, 1, 100),
        listed(11, 1, 101),
        listed(12, 1, 102),
        listed(13, 1, 103),
        listed(14, 1, 104),
    );
    let mut source = FakeSource::default();
    source.add_usage(a, usage(5_000, SECOND, 0, 16 * MIB));
    // Нет прав: `login` с setuid.
    source.add_usage(b, usage(6_000, SECOND, 0, 128 * MIB));
    source.usage.insert(11, Err(ReadError::Denied));
    source.info.insert(11, Err(ReadError::Denied));
    // PID сменился между перечислением и чтением.
    source.add_usage(c.clone(), usage(7_000, SECOND, 0, 512 * MIB));
    let mut replacement = info_of(&c);
    replacement.start = start(110);
    source.info.insert(12, Ok(replacement));
    // `rusage` прочитан, а `proc_pidinfo` после него уже не отвечает.
    source.add_usage(d, usage(8_000, SECOND, 0, 64 * MIB));
    source.info.insert(13, Err(ReadError::Exited));
    // Прочий отказ с `errno`.
    source.add_usage(e, usage(9_000, SECOND, 0, 32 * MIB));
    source
        .usage
        .insert(14, Err(ReadError::Other { errno: libc::EIO }));

    let now = snapshot(&source, NOW);
    let set: BTreeSet<Key> = now.listed.keys().copied().collect();
    let summary = summarize(None, &now, &set, APPLE).unwrap();

    // Сумма только прочитанного, но покрытие и признак неполноты говорят о четырёх отказах.
    assert_eq!(summary.memory.bytes, 16 * MIB);
    assert_eq!(
        summary.memory.coverage,
        Coverage {
            counted: 1,
            expected: 5
        }
    );
    assert!(!summary.memory.coverage.complete());
    assert!(summary.partial);
    let reasons: Vec<Gap> = summary
        .entries
        .iter()
        .filter_map(|e| match e.memory {
            Some(Value::Missing(gap)) => Some(gap),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasons,
        [
            Gap::Unread { unread: DENIED },
            Gap::Unread {
                unread: Unread::Replaced {
                    listed: start(102),
                    read: start(110)
                }
            },
            Gap::Unread {
                unread: Unread::Unconfirmed {
                    error: ReadError::Exited
                }
            },
            Gap::Unread {
                unread: Unread::Failed {
                    error: ReadError::Other { errno: libc::EIO }
                }
            },
        ]
    );
    let json = serde_json::to_string(&summary).unwrap();
    for needle in [
        r#""partial":true"#,
        r#""kind":"denied""#,
        r#""kind":"replaced""#,
        r#""kind":"unconfirmed""#,
        &format!(r#""errno":{}"#, libc::EIO),
    ] {
        assert!(json.contains(needle), "{needle} нет в {json}");
    }

    // Ни одного прочитанного: ноль байт не выдаётся за полный ноль.
    let mut none = FakeSource::default();
    none.add_usage(listed(20, 1, 200), usage(1, SECOND, 0, MIB));
    none.usage.insert(20, Err(ReadError::Denied));
    let now = snapshot(&none, NOW);
    let all_unread = summarize(None, &now, &members(&[key(20, 200)]), APPLE).unwrap();
    assert_eq!(all_unread.memory.bytes, 0);
    assert_eq!(all_unread.memory.coverage.share(), 0.0);
    assert!(!all_unread.memory.coverage.complete());
    assert!(all_unread.partial);

    // Отказ списка — ошибка всего снимка, а не пустая сводка.
    let broken = FakeSource {
        list_error: Some("sysctl(KERN_PROC_ALL): errno 12".to_owned()),
        ..FakeSource::default()
    };
    let error = Snapshot::take(&broken, || 0, |_| BTreeSet::new()).unwrap_err();
    assert!(error.contains("KERN_PROC_ALL"), "{error}");
}
