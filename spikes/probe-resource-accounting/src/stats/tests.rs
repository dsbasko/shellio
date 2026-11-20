use super::*;

const MINUTE: u64 = 60_000_000_000;

fn rules() -> Rules {
    Rules {
        blocks: 5,
        max_repeats: 2,
        min_clean: 3,
        guard_ns: 3 * MINUTE,
    }
}

/// Блок с CPU% состояний `00 10 01 11`.
fn block(s00: f64, s10: f64, s01: f64, s11: f64) -> BlockValues {
    [s00, s10, s01, s11]
}

#[test]
fn estimate_verdict_follows_the_budget() {
    assert_eq!(estimate_verdict(&[0.1, 0.2, 0.4], 0.5), Verdict::Pass);
    assert_eq!(estimate_verdict(&[0.6, 0.9, 1.4], 0.5), Verdict::Fail);
    assert_eq!(
        estimate_verdict(&[0.2, 0.7, 0.3], 0.5),
        Verdict::Unconfirmed
    );
    assert_eq!(estimate_verdict(&[0.5, 0.5, 0.5], 0.5), Verdict::Pass);
    assert_eq!(
        estimate_verdict(&[0.5, 0.6, 0.7], 0.5),
        Verdict::Unconfirmed
    );
    assert_eq!(estimate_verdict(&[], 0.5), Verdict::Unconfirmed);
    assert_eq!(
        estimate_verdict(&[0.1, f64::NAN], 0.5),
        Verdict::Unconfirmed
    );
}

#[test]
fn effect_passes_only_when_both_estimates_pass() {
    let blocks = [
        block(10.0, 10.2, 10.5, 10.6),
        block(10.1, 10.3, 10.6, 10.9),
        block(10.0, 10.5, 10.4, 10.8),
    ];
    let sampler = effect_verdict(Effect::Sampler, &blocks, 3);
    assert_eq!(sampler.verdict, Verdict::Pass, "{}", sampler.reason);
    assert_eq!(sampler.estimates[0].id, "10-00");
    assert_eq!(sampler.estimates[1].id, "11-01");
    let spans = effect_verdict(Effect::Spans, &blocks, 3);
    assert_eq!(spans.verdict, Verdict::Pass, "{}", spans.reason);
    assert_eq!(spans.estimates[0].id, "01-00");
    assert_eq!(spans.estimates[1].id, "11-10");
}

#[test]
fn diverging_estimates_fail_the_sampler_with_spans_on() {
    // Сэмплер: 0,3 п. п. при выключенном учёте и 0,8 п. п. при включённом в каждом блоке.
    let blocks = [
        block(10.0, 10.3, 11.0, 11.8),
        block(10.2, 10.5, 11.1, 11.9),
        block(10.1, 10.4, 11.2, 12.0),
    ];
    let verdict = effect_verdict(Effect::Sampler, &blocks, 3);
    assert_eq!(verdict.estimates[0].verdict, Verdict::Pass);
    assert_eq!(verdict.estimates[1].verdict, Verdict::Fail);
    assert_eq!(verdict.verdict, Verdict::Fail);
    assert!(
        verdict.reason.contains("при включённом учёте участков"),
        "{}",
        verdict.reason
    );
    assert!(
        !verdict
            .reason
            .contains("превышает бюджет 0.5 п. п. при выключенном")
    );
    assert_eq!(verdict.interaction.len(), 3);
    for diff in &verdict.interaction {
        assert!((diff - 0.5).abs() < 1e-9, "{diff}");
    }
}

#[test]
fn one_estimate_on_both_sides_is_unconfirmed() {
    let blocks = [
        block(10.0, 10.2, 10.0, 10.2),
        block(10.0, 10.9, 10.0, 10.2),
        block(10.0, 10.1, 10.0, 10.2),
    ];
    let verdict = effect_verdict(Effect::Sampler, &blocks, 3);
    assert_eq!(verdict.estimates[0].verdict, Verdict::Unconfirmed);
    assert_eq!(verdict.estimates[1].verdict, Verdict::Pass);
    assert_eq!(verdict.verdict, Verdict::Unconfirmed);
}

#[test]
fn fewer_than_three_clean_blocks_is_noisy() {
    let blocks = [block(10.0, 10.1, 10.0, 10.1), block(10.0, 10.1, 10.0, 10.1)];
    let verdict = effect_verdict(Effect::Sampler, &blocks, 3);
    assert_eq!(verdict.verdict, Verdict::Unconfirmed);
    assert!(verdict.reason.starts_with("шумно"), "{}", verdict.reason);
    // Даже явное превышение не даёт fail без трёх чистых блоков.
    let over = [block(10.0, 12.0, 10.0, 12.0), block(10.0, 12.0, 10.0, 12.0)];
    assert_eq!(
        effect_verdict(Effect::Sampler, &over, 3).verdict,
        Verdict::Unconfirmed
    );
    let empty = effect_verdict(Effect::Sampler, &[], 3);
    assert_eq!(empty.verdict, Verdict::Unconfirmed);
    assert!(
        empty.reason.contains("нет чистых блоков"),
        "{}",
        empty.reason
    );
    assert!(!empty.reason.contains("по обе стороны"), "{}", empty.reason);
}

#[test]
fn order_is_balanced_by_shift() {
    let blocks: Vec<[State; 4]> = (0..4).map(order).collect();
    for position in 0..4 {
        let mut seen: Vec<State> = blocks.iter().map(|b| b[position]).collect();
        seen.sort();
        assert_eq!(seen, State::ALL.to_vec(), "позиция {position}");
    }
    for b in &blocks {
        let mut states = b.to_vec();
        states.sort();
        assert_eq!(states, State::ALL.to_vec());
    }
    assert_eq!(order(0), order(4));
    assert_eq!(order(1), [State::S10, State::S01, State::S11, State::S00]);
}

#[test]
fn contaminated_block_repeats_in_the_same_slot_at_most_twice() {
    let rules = rules();
    let mut progress = Progress::default();
    let mut slots = Vec::new();
    // Слот 0 загрязнён трижды подряд: два повтора, затем слот закрывается без чистого блока.
    let outcomes = [false, false, false, true, false, true, true];
    for clean in outcomes {
        match progress.next(&rules, 0, u64::MAX) {
            Next::Block {
                slot,
                order: o,
                repeat,
            } => {
                assert_eq!(o, order(slot));
                slots.push((slot, repeat));
            }
            Next::Done(stop) => panic!("протокол закончился раньше: {stop:?}"),
        }
        progress.record(&rules, clean);
    }
    assert_eq!(
        slots,
        vec![
            (0, false),
            (0, true),
            (0, true),
            (1, false),
            (2, false),
            (3, false),
            (4, false)
        ]
    );
    assert_eq!(progress.repeats, 2);
    assert_eq!(progress.run, 7);
    assert_eq!(progress.clean, 3);
    assert_eq!(
        progress.next(&rules, 0, u64::MAX),
        Next::Done(Stop::Complete)
    );
}

#[test]
fn clean_series_runs_five_blocks() {
    let rules = rules();
    let mut progress = Progress::default();
    let mut run = 0;
    while let Next::Block { .. } = progress.next(&rules, 0, u64::MAX) {
        progress.record(&rules, true);
        run += 1;
    }
    assert_eq!((run, progress.clean, progress.repeats), (5, 5, 0));
}

#[test]
fn no_block_starts_with_less_than_the_guard_left() {
    let rules = rules();
    let progress = Progress::default();
    let deadline = 25 * MINUTE;
    assert!(matches!(
        progress.next(&rules, deadline - 3 * MINUTE, deadline),
        Next::Block { .. }
    ));
    assert_eq!(
        progress.next(&rules, deadline - 3 * MINUTE + 1, deadline),
        Next::Done(Stop::Deadline)
    );
    assert_eq!(
        progress.next(&rules, deadline + MINUTE, deadline),
        Next::Done(Stop::Deadline)
    );
}

/// Блок по 2 минуты от 15-й минуты попытки 25 минут: сколько блоков успеет и какой вердикт.
fn run_to_deadline(cleanliness: &[bool]) -> (Progress, Stop, Vec<BlockValues>) {
    let rules = rules();
    let deadline = 25 * MINUTE;
    let mut now = 15 * MINUTE;
    let mut progress = Progress::default();
    let mut clean_blocks = Vec::new();
    let mut outcomes = cleanliness.iter().copied();
    loop {
        match progress.next(&rules, now, deadline) {
            Next::Block { .. } => {
                let clean = outcomes.next().unwrap_or(true);
                if clean {
                    clean_blocks.push(block(10.0, 10.2, 10.4, 10.6));
                }
                progress.record(&rules, clean);
                now += 2 * MINUTE;
            }
            Next::Done(stop) => return (progress, stop, clean_blocks),
        }
    }
}

#[test]
fn verdict_at_the_attempt_deadline_uses_clean_blocks_only() {
    // 10 минут до срока: блоки в 15, 17, 19, 21 мин, в 23 мин до срока 2 минуты — стоп.
    let (progress, stop, blocks) = run_to_deadline(&[true, false, true, true]);
    assert_eq!(stop, Stop::Deadline);
    assert_eq!((progress.run, progress.clean), (4, 3));
    assert_eq!(
        effect_verdict(Effect::Sampler, &blocks, 3).verdict,
        Verdict::Pass
    );

    let (progress, stop, blocks) = run_to_deadline(&[false, true, false, true]);
    assert_eq!(stop, Stop::Deadline);
    assert_eq!((progress.run, progress.clean), (4, 2));
    let verdict = effect_verdict(Effect::Sampler, &blocks, 3);
    assert_eq!(verdict.verdict, Verdict::Unconfirmed);
    assert!(verdict.reason.starts_with("шумно"), "{}", verdict.reason);
}

#[test]
fn cost_row_round_trips() {
    let blocks = [
        block(10.0, 10.2, 10.5, 10.6),
        block(10.1, 10.3, 10.6, 10.9),
        block(10.0, 10.5, 10.4, 10.8),
    ];
    let row = CostRow {
        point: "pids-8".to_owned(),
        pids: 8,
        panels: 4,
        step_s: 2.0,
        work: true,
        blocks_run: 5,
        clean_blocks: 3,
        stop: Stop::Complete,
        sampler: EffectRow::of(&effect_verdict(Effect::Sampler, &blocks, 3)),
        spans: Some(EffectRow::of(&effect_verdict(Effect::Spans, &blocks, 3))),
        cpu_per_op_us: Some([Some(500.0), Some(505.0), Some(510.0), None]),
        pass_duration_median_us: Some(900.0),
        wakeups_per_minute: Some(30.0),
        set_size: Some(8),
        partial_reads: 0,
        footprint_max_mib: Some(12.5),
    };
    let json = serde_json::to_value(&row).unwrap();
    assert_eq!(json["stop"], "complete");
    assert_eq!(json["sampler"]["verdict"], "pass");
    let back: CostRow = serde_json::from_value(json).unwrap();
    assert_eq!(back, row);
    assert_eq!(serde_json::to_value(State::S10).unwrap(), "10");
}
