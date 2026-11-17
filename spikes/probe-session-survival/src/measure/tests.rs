use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

fn sample(cpu_ns: u64, footprint: u64, wakeups: u64) -> Option<Sample> {
    Some(Sample {
        cpu_ns,
        footprint: Some(footprint),
        wakeups: Some(wakeups),
        units: 1,
    })
}

fn reading(t_ms: u64, values: Vec<Option<Sample>>) -> Reading {
    Reading {
        t_ns: t_ms * 1_000_000,
        values,
    }
}

/// Хост и клиент, три показания через 2 с: два окна.
fn series() -> Vec<Reading> {
    vec![
        reading(
            0,
            vec![sample(0, 10 << 20, 0), sample(5_000_000, 4 << 20, 10)],
        ),
        reading(
            2_000,
            vec![
                sample(20_000_000, 11 << 20, 4),
                sample(5_000_000, 4 << 20, 12),
            ],
        ),
        reading(
            4_000,
            vec![
                sample(60_000_000, 12 << 20, 8),
                sample(6_000_000, 4 << 20, 12),
            ],
        ),
    ]
}

#[test]
fn cpu_percent_of_interval() {
    assert_eq!(cpu_percent(0, 500_000_000, 0, 1_000_000_000), Some(50.0));
    assert_eq!(cpu_percent(0, 2_000_000_000, 0, 1_000_000_000), Some(200.0));
    assert_eq!(cpu_percent(10, 5, 0, 1_000), None, "CPU убывает");
    assert_eq!(cpu_percent(0, 5, 7, 7), None, "окно без времени");
    assert_eq!(per_second(4, 10, 0, 2_000_000_000), Some(3.0));
    assert_eq!(per_second(10, 4, 0, 1), None);
}

#[test]
fn windows_of_synthetic_series() {
    let readings = series();
    assert_eq!(window_cpu(&readings, &[0]).unwrap(), [1.0, 2.0]);
    assert_eq!(window_cpu(&readings, &[1]).unwrap(), [0.0, 0.05]);
    assert_eq!(window_cpu(&readings, &[0, 1]).unwrap(), [1.0, 2.05]);
    assert_eq!(window_wakeups(&readings, &[0]).unwrap(), [2.0, 2.0]);
    assert_eq!(window_wakeups(&readings, &[0, 1]).unwrap(), [3.0, 2.0]);
    assert!(window_cpu(&readings, &[]).is_err());
    assert!(window_cpu(&readings, &[2]).is_err(), "наблюдения нет");
    assert!(window_cpu(&readings[..1], &[0]).unwrap().is_empty());
}

#[test]
fn broken_series_is_an_error_not_a_value() {
    let mut missing = series();
    missing[1].values[0] = None;
    let error = window_cpu(&missing, &[0]).unwrap_err();
    assert!(error.contains("не прочитано в окне 0"), "{error}");

    let mut reused = series();
    reused[2].values[0] = sample(1_000, 1 << 20, 0);
    let error = window_cpu(&reused, &[0]).unwrap_err();
    assert!(error.contains("CPU убывает"), "{error}");
    assert!(window_wakeups(&reused, &[0]).is_err());

    let mut threads = series();
    if let Some(value) = threads[2].values[1].as_mut() {
        value.units = 2;
        value.wakeups = None;
    }
    let error = window_cpu(&threads, &[1]).unwrap_err();
    assert!(error.contains("1 единиц в начале и 2 в конце"), "{error}");

    let mut no_wakeups = series();
    for reading in &mut no_wakeups {
        if let Some(value) = reading.values[1].as_mut() {
            value.wakeups = None;
        }
    }
    assert!(window_wakeups(&no_wakeups, &[1]).is_err());
    assert_eq!(window_cpu(&no_wakeups, &[1]).unwrap(), [0.0, 0.05]);
}

#[test]
fn timebase_converts_ticks() {
    let apple = Timebase {
        numer: 125,
        denom: 3,
    };
    assert_eq!(apple.ns(24_000_000), 1_000_000_000);
    assert_eq!(apple.ns(0), 0);
    let unit = Timebase { numer: 1, denom: 1 };
    assert_eq!(unit.ns(u64::MAX), u64::MAX);
    assert_eq!(
        apple.ns(u64::MAX),
        u64::MAX,
        "переполнение упирается в предел"
    );
    assert!(Timebase::system().is_ok());
}

#[test]
fn units_ratio_needs_enough_work() {
    assert_eq!(units_ratio(98, 100, 100), Some(0.98));
    assert_eq!(units_ratio(98, 99, 100), None);
    assert!(UNITS_RANGE.contains(&0.98));
    assert!(
        !UNITS_RANGE.contains(&41.6),
        "тики без перевода дают отношение 125/3"
    );
}

#[test]
fn flood_bytes_interpolate_between_receipts() {
    let points = [(1_000, 0), (3_000, 4_000), (4_000, 4_000)];
    assert_eq!(bytes_at(&points, 1_000), Some(0.0));
    assert_eq!(bytes_at(&points, 2_000), Some(2_000.0));
    assert_eq!(bytes_at(&points, 3_500), Some(4_000.0), "поток стоял");
    assert_eq!(bytes_at(&points, 999), None, "до первой квитанции");
    assert_eq!(bytes_at(&points, 4_001), None, "после последней");
    assert_eq!(bytes_at(&[], 1), None);
}

#[test]
fn cpu_per_mib_of_synthetic_flood() {
    let readings = series();
    // Квитанции хода: 1 МиБ за первое окно, 4 МиБ за второе.
    let points = [(0, 0), (2_000_000_000, 1 << 20), (4_000_000_000, 5 << 20)];
    let bytes = window_bytes(&readings, &points).unwrap();
    assert_eq!(bytes, [MIB, 4.0 * MIB]);
    // Хост: 20 мс CPU в первом окне и 40 мс во втором.
    assert_eq!(
        window_cpu_per_mib(&readings, &[0], &bytes).unwrap(),
        [20.0, 10.0]
    );
    // Хост и клиент вместе: во втором окне клиент добавил 1 мс.
    assert_eq!(
        window_cpu_per_mib(&readings, &[0, 1], &bytes).unwrap(),
        [20.0, 10.25]
    );
}

#[test]
fn cpu_per_mib_refuses_uncovered_or_empty_windows() {
    let readings = series();
    let short = [(0, 0), (3_000_000_000, 1 << 20)];
    assert!(
        window_bytes(&readings, &short)
            .unwrap_err()
            .contains("вне квитанций")
    );
    let unordered = [(0, 0), (2_000_000_000, 5), (1_000_000_000, 6)];
    assert!(
        window_bytes(&readings, &unordered)
            .unwrap_err()
            .contains("не по порядку")
    );
    assert!(
        window_cpu_per_mib(&readings, &[0], &[MIB, 0.0])
            .unwrap_err()
            .contains("нет вывода")
    );
    assert!(window_cpu_per_mib(&readings, &[0], &[MIB]).is_err());
    assert!(window_cpu_per_mib(&readings, &[], &[MIB, MIB]).is_err());
}

#[test]
fn memory_per_session() {
    assert_eq!(mib(3 << 20), 3.0);
    assert_eq!(marginal((1, 10 << 20), (8, 31 << 20)), Some(3.0 * MIB));
    assert_eq!(marginal((8, 31 << 20), (8, 40 << 20)), None);
    assert_eq!(marginal((8, 40), (1, 10)), None);
    assert_eq!(marginal((1, 20), (3, 10)), Some(-5.0));
}

#[test]
fn echo_legs_and_order() {
    let echo = Echo {
        sent_ns: 1_000_000,
        read_ns: 1_250_000,
        echoed_ns: 1_750_000,
    };
    let legs = echo.legs().unwrap();
    assert_eq!(
        (legs.input_ms, legs.output_ms, legs.total_ms),
        (0.25, 0.5, 0.75)
    );
    assert_eq!(
        Echo {
            read_ns: 900_000,
            ..echo
        }
        .legs(),
        None,
        "чтение раньше отправки"
    );
    assert_eq!(
        Echo {
            echoed_ns: 1_200_000,
            ..echo
        }
        .legs(),
        None,
        "эхо раньше чтения"
    );
    assert_eq!(median(&[3.0, 1.0, 2.0, 10.0]), Some(2.5));
    assert_eq!(median(&[]), None);
}

#[test]
fn series_runs_in_its_own_thread() {
    let calls = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&calls);
    let watches = vec![
        Watch::Threads {
            part: Part::Surface,
            name: "x".to_owned(),
        },
        Watch::Threads {
            part: Part::Client,
            name: "y".to_owned(),
        },
    ];
    let series = Series::start(watches, 4, Duration::from_millis(1), move |watch| {
        let call = counter.fetch_add(1, Ordering::SeqCst);
        (watch.part() == Part::Surface).then_some(Sample {
            cpu_ns: call * 10,
            footprint: None,
            wakeups: None,
            units: 1,
        })
    })
    .unwrap();
    let readings = series.join().unwrap();
    assert_eq!(readings.len(), 4);
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    assert!(readings.windows(2).all(|pair| pair[1].t_ns > pair[0].t_ns));
    assert!(readings.iter().all(|reading| reading.values[1].is_none()));
    assert_eq!(window_cpu(&readings, &[0]).unwrap().len(), 3);
    assert!(Series::start(Vec::new(), 1, Duration::ZERO, |_| None).is_err());
}

#[test]
fn dropped_series_stops_without_waiting_for_the_pace() {
    let started = std::time::Instant::now();
    let series = Series::start(Vec::new(), 3, Duration::from_secs(600), |_| None).unwrap();
    drop(series);
    assert!(started.elapsed() < Duration::from_secs(60));
}

#[test]
fn rust_lines_split_code_and_tests() {
    let text = "\
//! Модуль.

use std::fmt;

pub fn one() -> u8 {
    1
}

#[cfg(test)]
mod tests;
";
    assert_eq!(rust_lines(text), Lines { code: 5, tests: 2 });

    let inline = "\
pub struct A;

impl A {
    #[cfg(test)]
    fn helper() {
        let _ = 1;
    }

    pub fn real(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn works() {
        assert!(true);
    }
}
";
    assert_eq!(rust_lines(inline), Lines { code: 4, tests: 12 });
    assert_eq!(rust_lines(""), Lines::default());
    assert_eq!(rust_lines("#[cfg(test)]"), Lines { code: 0, tests: 1 });
    let unclosed = "#[cfg(test)]\nmod tests {\n    fn a() {}\n";
    assert_eq!(rust_lines(unclosed), Lines { code: 0, tests: 3 });
}

#[test]
fn patch_lines_skip_headers() {
    let patch = "\
From abc Mon Sep 17 00:00:00 2001
Subject: [PATCH] leadership
---
 src/role.zig | 3 +++
--- a/src/main.zig
+++ b/src/main.zig
@@ -1,3 +1,4 @@
 const std = @import(\"std\");
-const old = 1;
+const new = 2;
+
+const role = @import(\"role.zig\");
";
    assert_eq!(
        patch_lines(patch),
        PatchLines {
            added: 2,
            removed: 1
        }
    );
}

#[test]
fn file_kinds() {
    assert_eq!(kind_of(Path::new("src/host/mod.rs")), Kind::Rust);
    assert_eq!(kind_of(Path::new("src/vt/bindings.rs")), Kind::Generated);
    assert_eq!(
        kind_of(Path::new("src/proto/vectors/v1.txt")),
        Kind::TestData
    );
    assert_eq!(
        kind_of(Path::new("zmx-patches/0001-explicit-leadership.patch")),
        Kind::Patch
    );
    assert_eq!(kind_of(Path::new("scripts/zmx-pin")), Kind::Script);
    assert_eq!(kind_of(Path::new("scripts/vt.sh")), Kind::Script);
    assert_eq!(kind_of(Path::new("zmx-patches/NOTICE")), Kind::Notice);
}

#[test]
fn group_lines_by_kind() {
    let root = std::env::temp_dir().join(format!(
        "pss-loc-{}-{}",
        std::process::id(),
        sys::uptime_ns()
    ));
    let host = root.join("src/host");
    std::fs::create_dir_all(host.join("io")).unwrap();
    std::fs::write(
        host.join("mod.rs"),
        "pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
    )
    .unwrap();
    std::fs::write(host.join("io/tests.rs"), "#[test]\nfn x() {}\n").unwrap();
    std::fs::write(root.join("NOTICE"), "MIT\n").unwrap();
    let group = Group {
        name: "хост",
        paths: &["src/host", "NOTICE"],
    };
    let mut counted = GroupLines::default();
    for file in group_files(&root, &group).unwrap() {
        let text = std::fs::read_to_string(&file).unwrap();
        counted.count(file.strip_prefix(&root).unwrap(), &text);
    }
    assert_eq!(counted.files, 2);
    assert_eq!(counted.rust, Lines { code: 1, tests: 7 });
    let missing = Group {
        name: "нет",
        paths: &["src/none"],
    };
    assert!(group_files(&root, &missing).is_err());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn candidate_groups_exist_in_this_crate() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for candidate in Candidate::ALL {
        let counted = count_lines(root, candidate).unwrap();
        assert_eq!(counted.len(), groups(candidate).len());
        assert!(counted.iter().all(|group| group.files > 0), "{counted:?}");
    }
    let rust = count_lines(root, Candidate::Rust).unwrap();
    assert!(rust[0].rust.code > 0 && rust[0].rust.tests > 0);
    assert!(rust[0].test_data > 0, "векторы v1");
    assert!(rust[1].generated > 0, "привязки");
    let zmx = count_lines(root, Candidate::Zmx).unwrap();
    assert!(zmx[0].patch.added > 0);
    assert_eq!(
        zmx[0].files, 5,
        "четыре патча zmx и патч его ghostty, без NOTICE"
    );
}
