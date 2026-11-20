use super::*;

fn charge(cpu_ns: u64, spans: u64, parts: &[(u64, u64)]) -> Charge {
    Charge {
        cpu_ns,
        spans,
        per_thread_ns: parts.iter().copied().collect(),
        ..Charge::default()
    }
}

#[test]
fn sample_tolerance_is_absolute_plus_relative_and_inclusive() {
    // 50 мс: допуск 0,2 + 1,0 мс; на границе — в допуске.
    assert!(Sample::new("на границе", 51_200_000, 50_000_000).share() <= 1.0);
    assert!(Sample::new("вне", 51_300_000, 50_000_000).share() > 1.0);
    assert!(Sample::new("ниже", 48_800_000, 50_000_000).share() <= 1.0);
    // Ожидание, начисленное задаче, не прячется в относительной части короткого участка.
    assert!(Sample::new("ожидание", 5_000_000 + 70_000_000, 5_000_000).share() > 1.0);
}

#[test]
fn samples_cell_needs_every_comparison_inside_and_no_problems() {
    let inside = [
        Sample::new("a", 500_000, 600_000),
        Sample::new("b", 5_000_000, 5_050_000),
    ];
    assert_eq!(samples_cell("x", "o", &inside, &[]).status, Status::Pass);
    let cell = samples_cell("x", "o", &inside, &["участков 3".to_owned()]);
    assert_eq!(cell.status, Status::Fail);
    assert!(cell.reason.contains("участков 3"));
    let outside = [inside[0].clone(), Sample::new("c", 6_000_000, 5_000_000)];
    let cell = samples_cell("x", "o", &outside, &[]);
    assert_eq!(cell.status, Status::Fail);
    assert!(cell.reason.contains("вне допуска 1"));
    assert!(cell.reason.contains("худшее — c"));
    assert_eq!(samples_cell("x", "o", &[], &[]).status, Status::Fail);
}

#[test]
fn charge_problems_catch_span_count_unmeasured_and_parts() {
    assert!(charge_problems("p", &charge(30, 3, &[(0, 10), (1, 20)]), 3).is_empty());
    assert_eq!(charge_problems("p", &charge(30, 2, &[(0, 30)]), 3).len(), 1);
    let mut unmeasured = charge(30, 3, &[(0, 30)]);
    unmeasured.unmeasured_spans = 1;
    assert_eq!(charge_problems("p", &unmeasured, 3).len(), 1);
    let parts = charge_problems("p", &charge(30, 3, &[(0, 10)]), 3);
    assert_eq!(parts.len(), 1);
    assert!(parts[0].contains("сумма частей"));
}

#[test]
fn wait_idle_is_not_run_without_cpu_between_polls() {
    let samples = [
        Sample::new("A poll 1", 5_000_000, 5_010_000),
        Sample::new("A poll 2", 5_000_000, 5_010_000),
        Sample::new("сумма wait-a", 10_000_000, 10_020_000),
    ];
    assert_eq!(
        wait_idle_cell(&samples, &[], 70_000_000).status,
        Status::Pass
    );
    let idle = wait_idle_cell(&samples, &[], 100_000);
    assert_eq!(idle.status, Status::NotRun);
    assert_eq!(idle.not_run_cause, Some(NotRunCause::NotObserved));
    // Ожидание начислено A: сумма вне допуска.
    let charged = [
        samples[0].clone(),
        samples[1].clone(),
        Sample::new("сумма wait-a", 80_000_000, 10_020_000),
    ];
    assert_eq!(
        wait_idle_cell(&charged, &[], 70_000_000).status,
        Status::Fail
    );
}

#[test]
fn series_cell_is_not_run_when_condition_did_not_happen() {
    let sample = Sample::new("серия", 1_990_000_000, 2_000_000_000);
    assert_eq!(series_cell("s", &sample, &[], Ok(())).status, Status::Pass);
    let cell = series_cell("s", &sample, &[], Err("смены потока нет".to_owned()));
    assert_eq!(cell.status, Status::NotRun);
    assert!(cell.reason.starts_with("смены потока нет"));
    // Планировщик съел больше 2%: вне допуска.
    let lost = Sample::new("серия", 1_900_000_000, 2_000_000_000);
    assert_eq!(series_cell("s", &lost, &[], Ok(())).status, Status::Fail);
}

#[test]
fn usage_problems_compare_exactly() {
    let usage = Usage {
        used_bytes: 10,
        capacity_bytes: 16,
        leases: 1,
        requested_by: ["A".to_owned()].into_iter().collect(),
    };
    assert!(usage_problems("g", &usage, 10, 16, 1, &["A"]).is_empty());
    assert_eq!(usage_problems("g", &usage, 11, 16, 1, &["A"]).len(), 1);
    assert_eq!(usage_problems("g", &usage, 10, 15, 1, &["A"]).len(), 1);
    assert_eq!(usage_problems("g", &usage, 10, 16, 2, &["A"]).len(), 1);
    assert_eq!(usage_problems("g", &usage, 10, 16, 1, &[]).len(), 1);
}

#[test]
fn arena_scenario_matches_known_sizes() {
    let report = run_arena().unwrap();
    let ids: Vec<&str> = report.cells.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        ["arena-grow", "arena-reuse", "arena-release", "arena-git"]
    );
    for cell in &report.cells {
        assert_eq!(cell.status, Status::Pass, "{}: {}", cell.id, cell.reason);
    }
    assert!(report.raw.contains_key("steps"));
}

#[test]
fn steps_future_is_pending_until_the_last_step() {
    let mut future = steps(vec![(Load::User, Duration::ZERO); 3]);
    assert!(poll_once(&mut future).is_pending());
    assert!(poll_once(&mut future).is_pending());
    assert!(matches!(poll_once(&mut future), Poll::Ready(Ok(()))));
    let mut once = Box::pin(yield_once());
    assert!(poll_once(&mut once).is_pending());
    assert!(poll_once(&mut once).is_ready());
}
