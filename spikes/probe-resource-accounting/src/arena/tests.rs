use std::sync::Barrier;

use super::*;

fn panel(id: &str) -> Owner {
    Owner::Panel(id.to_owned())
}

/// Занятые байты, ёмкость и живые структуры владельца.
fn totals(arenas: &Arenas, owner: &Owner) -> (u64, u64, u64) {
    let usage = arenas.usage_of(owner);
    (usage.used_bytes, usage.capacity_bytes, usage.leases)
}

fn bytes(len: usize, fill: u8) -> Vec<u8> {
    vec![fill; len]
}

#[test]
fn growth_fills_free_space_before_adding_chunks() {
    let arenas = Arenas::new();
    let mut arena = Arena::new(arenas.lease(&panel("A")), 4096);
    let first = arena.alloc(&bytes(100, 1)).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (100, 4096, 1));
    // Больше куска — отдельный кусок ровно своего размера.
    let large = arena.alloc(&bytes(5000, 2)).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (5100, 9096, 1));
    // Остаток первого куска занимается без роста ёмкости.
    let tail = arena.alloc(&bytes(3996, 3)).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (9096, 9096, 1));
    let next = arena.alloc(&bytes(1, 4)).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (9097, 13192, 1));
    assert_eq!(arena.get(first), Some(&bytes(100, 1)[..]));
    assert_eq!(arena.get(large), Some(&bytes(5000, 2)[..]));
    assert_eq!(arena.get(tail), Some(&bytes(3996, 3)[..]));
    assert_eq!(arena.get(next), Some(&[4][..]));
}

#[test]
fn reset_frees_used_and_keeps_capacity_for_reuse() {
    let arenas = Arenas::new();
    let mut arena = Arena::new(arenas.lease(&panel("A")), 4096);
    let old = arena.alloc(&bytes(4000, 1)).unwrap();
    arena.alloc(&bytes(5000, 2)).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (9000, 9096, 1));
    arena.reset();
    assert_eq!(totals(&arenas, &panel("A")), (0, 9096, 1));
    assert_eq!(arena.get(old), None);
    // Переиспользование: оба выделения помещаются в оставшиеся куски.
    let reused = arena.alloc(&bytes(5000, 3)).unwrap();
    arena.alloc(&bytes(4096, 4)).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (9096, 9096, 1));
    assert_eq!(arena.get(reused), Some(&bytes(5000, 3)[..]));
}

#[test]
fn trim_releases_only_empty_chunks_and_keeps_live_slots() {
    let arenas = Arenas::new();
    let mut arena = Arena::new(arenas.lease(&panel("A")), 1000);
    for fill in 1..=3 {
        arena.alloc(&bytes(600, fill)).unwrap();
    }
    assert_eq!(totals(&arenas, &panel("A")), (1800, 3000, 1));
    // Без пустых кусков освобождать нечего.
    assert_eq!(arena.trim(), Ok(0));
    arena.reset();
    let kept = arena.alloc(&bytes(900, 7)).unwrap();
    assert_eq!(arena.trim(), Ok(2000));
    assert_eq!(totals(&arenas, &panel("A")), (900, 1000, 1));
    assert_eq!(arena.get(kept), Some(&bytes(900, 7)[..]));
    // Новый кусок после освобождённых: прежние места по-прежнему находятся.
    let added = arena.alloc(&bytes(200, 8)).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (1100, 2000, 1));
    assert_eq!(arena.get(kept), Some(&bytes(900, 7)[..]));
    assert_eq!(arena.get(added), Some(&bytes(200, 8)[..]));
    arena.reset();
    assert_eq!(arena.trim(), Ok(2000));
    assert_eq!(totals(&arenas, &panel("A")), (0, 0, 1));
}

#[test]
fn drop_returns_exactly_its_share_and_owners_stay_isolated() {
    let arenas = Arenas::new();
    let mut a1 = Arena::new(arenas.lease(&panel("A")), 1024);
    let mut a2 = Arena::new(arenas.lease(&panel("A")), 2048);
    let mut b = Arena::new(arenas.lease(&panel("B")), 512);
    a1.alloc(&bytes(300, 1)).unwrap();
    a2.alloc(&bytes(1500, 2)).unwrap();
    a2.alloc(&bytes(700, 3)).unwrap();
    b.alloc(&bytes(512, 4)).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (2500, 5120, 2));
    assert_eq!(totals(&arenas, &panel("B")), (512, 512, 1));
    drop(a2);
    assert_eq!(totals(&arenas, &panel("A")), (300, 1024, 1));
    assert_eq!(totals(&arenas, &panel("B")), (512, 512, 1));
    drop((a1, b));
    // Владелец остаётся в списке с нулями: освобождение видно, а не пропадает.
    let usage = arenas.usage();
    assert_eq!(usage.len(), 2);
    assert!(usage.iter().all(|owner| owner.usage == Usage::default()));
    assert_eq!(totals(&arenas, &panel("C")), (0, 0, 0));
}

#[test]
fn shared_git_cache_is_charged_to_git_group() {
    let arenas = Arenas::new();
    let mut own = Arena::new(arenas.lease(&panel("A")), 256);
    own.alloc(&bytes(100, 1)).unwrap();
    // Первой запросила панель B, затем A и B вместе: кэши общие, начисление одно.
    let mut first = Arena::new(arenas.shared_git_lease(&["B"]), 4096);
    let mut second = Arena::new(arenas.shared_git_lease(&["A", "B"]), 1024);
    first.alloc(&bytes(3000, 2)).unwrap();
    second.alloc(&bytes(1000, 3)).unwrap();
    let git = arenas.usage_of(&Owner::Git);
    assert_eq!(
        (git.used_bytes, git.capacity_bytes, git.leases),
        (4000, 5120, 2)
    );
    assert_eq!(
        git.requested_by,
        BTreeSet::from(["A".to_owned(), "B".to_owned()])
    );
    assert_eq!(totals(&arenas, &panel("A")), (100, 256, 1));
    assert_eq!(totals(&arenas, &panel("B")), (0, 0, 0));
    assert!(arenas.usage_of(&panel("A")).requested_by.is_empty());
}

#[test]
fn overfill_and_over_release_are_rejected_without_changes() {
    let arenas = Arenas::new();
    let mut lease = arenas.lease(&panel("A"));
    lease.grow(100);
    lease.fill(60).unwrap();
    let overfill = ArenaError::Overfill {
        used: 60,
        capacity: 100,
        bytes: 41,
    };
    assert_eq!(lease.fill(41), Err(overfill));
    let over_release = ArenaError::OverRelease {
        used: 60,
        capacity: 100,
        bytes: 41,
    };
    assert_eq!(lease.release(41), Err(over_release));
    assert_eq!(totals(&arenas, &panel("A")), (60, 100, 1));
    assert!(overfill.to_string().contains("сверх ёмкости"));
    assert!(over_release.to_string().contains("свободной ёмкости"));
    lease.release(40).unwrap();
    lease.fill(0).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (60, 60, 1));
    lease.reset();
    assert_eq!(totals(&arenas, &panel("A")), (0, 60, 1));
    drop(lease);
    assert_eq!(totals(&arenas, &panel("A")), (0, 0, 0));
}

#[test]
fn empty_alloc_takes_no_space() {
    let arenas = Arenas::new();
    let mut arena = Arena::new(arenas.lease(&panel("A")), 4096);
    let empty = arena.alloc(&[]).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (0, 0, 1));
    assert_eq!(arena.get(empty), Some(&[][..]));
    arena.reset();
    assert_eq!(arena.get(empty), None);
}

#[test]
fn zero_chunk_size_gives_each_allocation_its_own_chunk() {
    let arenas = Arenas::new();
    let mut arena = Arena::new(arenas.lease(&panel("A")), 0);
    let first = arena.alloc(&bytes(10, 1)).unwrap();
    let second = arena.alloc(&bytes(20, 2)).unwrap();
    assert_eq!(totals(&arenas, &panel("A")), (30, 30, 1));
    assert_eq!(arena.get(first), Some(&bytes(10, 1)[..]));
    assert_eq!(arena.get(second), Some(&bytes(20, 2)[..]));
}

#[test]
fn arenas_of_one_owner_on_many_threads_sum_exactly() {
    const THREADS: usize = 8;
    let arenas = Arc::new(Arenas::new());
    let barrier = Arc::new(Barrier::new(THREADS + 1));
    let workers: Vec<_> = (0..THREADS)
        .map(|index| {
            let arenas = Arc::clone(&arenas);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut arena = Arena::new(arenas.lease(&panel("A")), 1000);
                for _ in 0..3 {
                    arena.alloc(&bytes((index + 1) * 100, 1)).unwrap();
                }
                // Арены живы, пока главный поток читает счёт.
                barrier.wait();
                barrier.wait();
            })
        })
        .collect();
    barrier.wait();
    // Занято Σ 3 × 100 × (i + 1) = 300 × 36. Куски по 1000 Б: три выделения по 100–300 Б
    // помещаются в один, по 400–500 Б — в два, по 600–800 Б — в три.
    let capacity = 3 * 1000 + 2 * 2000 + 3 * 3000;
    assert_eq!(
        totals(&arenas, &panel("A")),
        (10_800, capacity, THREADS as u64)
    );
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(totals(&arenas, &panel("A")), (0, 0, 0));
}

#[test]
fn usage_serializes_with_owner_tags() {
    let arenas = Arenas::new();
    let mut git = Arena::new(arenas.shared_git_lease(&["A"]), 64);
    git.alloc(&bytes(10, 1)).unwrap();
    let json = serde_json::to_value(arenas.usage()).unwrap();
    assert_eq!(json[0]["owner"]["kind"], "git");
    assert_eq!(json[0]["usage"]["used_bytes"], 10);
    assert_eq!(json[0]["usage"]["capacity_bytes"], 64);
    assert_eq!(json[0]["usage"]["requested_by"][0], "A");
}
