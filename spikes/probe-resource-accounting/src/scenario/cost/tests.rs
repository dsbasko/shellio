use super::*;
use crate::sampler::{Numbers, PassCounts, PassRecord, Steps, Wakeups};

#[test]
fn points_match_names_and_build_their_sets() {
    let names: Vec<&str> = POINTS.iter().map(|p| p.name).collect();
    assert_eq!(names, POINT_NAMES);
    let children: Vec<u32> = POINTS.iter().map(|p| p.children().unwrap()).collect();
    assert_eq!(children, [0, 6, 30, 0, 6, 6]);
    for point in POINTS {
        let per_panel = 2 + point.children().unwrap() as usize;
        assert_eq!(per_panel * point.panels, point.pids, "{}", point.name);
    }
    assert!(find_point("idle").is_ok_and(|p| !p.work));
    assert!(find_point("background").is_ok_and(|p| p.visibility == Visibility::Background));
    assert!(find_point("nope").is_err());
    let odd = point("odd", 9, 4, Visibility::Visible, true);
    assert!(odd.children().is_err());
}

#[test]
fn deadline_counts_from_the_run_sh_stamp() {
    let attempt = 25 * 60 * SECOND;
    // Отметка 7 минут назад: блокировка и тишина съели 7 минут, остаётся 18.
    assert_eq!(
        attempt_deadline(attempt, Some(1000), 1000 + 7 * 60, 5 * SECOND),
        5 * SECOND + 18 * 60 * SECOND
    );
    assert_eq!(
        attempt_deadline(attempt, None, 1000, 5 * SECOND),
        5 * SECOND + attempt
    );
    assert_eq!(
        attempt_deadline(attempt, Some(0), 40 * 60, 5 * SECOND),
        5 * SECOND
    );
}

#[test]
fn passes_per_window_are_fixed_by_the_half_step_start() {
    assert_eq!(passes_expected(30 * SECOND, sampler::STEPS.visible_ns), 15);
    assert_eq!(
        passes_expected(30 * SECOND, sampler::STEPS.background_ns),
        3
    );
}

fn log(passes: usize, set_size: usize, failed: bool) -> SamplerLog {
    let total = Numbers {
        members: set_size,
        memory_bytes: 0,
        cpu_pct: None,
        partial: false,
    };
    SamplerLog {
        steps: Steps {
            visible_ns: 2 * SECOND,
            background_ns: 10 * SECOND,
        },
        started_ns: 0,
        stopped_ns: 30 * SECOND,
        wakeups: Wakeups::default(),
        skipped_deadlines: 0,
        switches: Vec::new(),
        claims_changes: 0,
        passes: (0..passes)
            .map(|i| PassRecord {
                index: i as u64 + 1,
                visibility: Visibility::Visible,
                due_ns: 0,
                start_ns: 0,
                end_ns: 0,
                duration_ns: 0,
                late_ns: 0,
                skipped: 0,
                outcome: if failed && i == 0 {
                    Err("список".to_owned())
                } else {
                    Ok(PassCounts {
                        listed: 800,
                        set_size,
                        partial_reads: 0,
                        getsid_calls: 800,
                        attribution_notes: 0,
                        panels: 4,
                        cpu_interval_ns: None,
                        total: total.clone(),
                    })
                },
            })
            .collect(),
    }
}

#[test]
fn window_is_invalid_on_ops_passes_or_set() {
    let point = find_point("pids-8").unwrap();
    let step = sampler::STEPS.visible_ns;
    let wall = 30 * SECOND;
    assert!(window_problems(&point, &PROTOCOL, wall, 6000, None, step).is_empty());
    assert!(window_problems(&point, &PROTOCOL, wall, 5941, None, step).is_empty());
    let few = window_problems(&point, &PROTOCOL, wall, 5939, None, step);
    assert!(few[0].starts_with("операции"), "{few:?}");
    assert!(!window_problems(&point, &PROTOCOL, wall, 6061, None, step).is_empty());
    assert!(
        window_problems(
            &point,
            &PROTOCOL,
            wall,
            6000,
            Some(&log(15, 8, false)),
            step
        )
        .is_empty()
    );
    let passes = window_problems(
        &point,
        &PROTOCOL,
        wall,
        6000,
        Some(&log(16, 8, false)),
        step,
    );
    assert!(passes[0].starts_with("проходы"), "{passes:?}");
    let set = window_problems(
        &point,
        &PROTOCOL,
        wall,
        6000,
        Some(&log(15, 7, false)),
        step,
    );
    assert!(set.iter().all(|p| p.contains("множество 7")), "{set:?}");
    assert!(
        !window_problems(&point, &PROTOCOL, wall, 6000, Some(&log(15, 8, true)), step).is_empty()
    );
    let idle = find_point("idle").unwrap();
    assert!(window_problems(&idle, &PROTOCOL, wall, 0, None, step).is_empty());
}

fn window(state: State, cpu_pct: f64) -> WindowRecord {
    WindowRecord {
        run: 0,
        slot: 0,
        position: 0,
        state,
        start_ns: 0,
        end_ns: 30 * SECOND,
        stand_cpu_ns: 0.0,
        cpu_pct,
        ops: 6000,
        ops_expected: Some(6000.0),
        cpu_per_op_us: Some(cpu_pct),
        passes_expected: None,
        sampler: None,
        footprint: 0,
        quiet: true,
        problems: Vec::new(),
    }
}

#[test]
fn block_values_take_clean_blocks_by_state() {
    let blocks = vec![
        BlockRecord {
            run: 0,
            slot: 1,
            repeat: false,
            order: stats::order(1),
            clean: true,
            windows: stats::order(1)
                .into_iter()
                .map(|s| window(s, 10.0 + s.index() as f64))
                .collect(),
        },
        BlockRecord {
            run: 1,
            slot: 2,
            repeat: false,
            order: stats::order(2),
            clean: false,
            windows: Vec::new(),
        },
    ];
    let values = block_values(&blocks, |w| Some(w.cpu_pct));
    assert_eq!(values, vec![[10.0, 11.0, 12.0, 13.0]]);
}
