use std::io::{self, Write};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use super::*;

fn size(cols: u16, rows: u16) -> Winsize {
    Winsize::cells(rows, cols)
}

#[test]
fn bounded_queue_refuses_whole_frames_over_the_limit() {
    let queue = ByteQueue::new(10);
    queue.push(b"abcd".to_vec(), Room::Limit).unwrap();
    queue.push(b"efgh".to_vec(), Room::Limit).unwrap();
    assert_eq!(
        queue.push(b"ijk".to_vec(), Room::Limit),
        Err(Refusal::Full {
            len: 3,
            counted: 8,
            limit: 10
        })
    );
    // Запас и кадры без предела помещаются сверх него.
    queue.push(b"ij".to_vec(), Room::Limit).unwrap();
    assert!(queue.push(b"k".to_vec(), Room::Limit).is_err());
    queue.push(b"klm".to_vec(), Room::Reserve(4)).unwrap();
    assert!(queue.push(b"no".to_vec(), Room::Reserve(4)).is_err());
    queue.push(vec![0; 100], Room::Always).unwrap();
    assert_eq!(queue.counted(), 113);
    assert_eq!(queue.peak(), 113);
}

#[test]
fn bytes_in_writing_count_until_the_frame_is_written() {
    let queue = ByteQueue::new(8);
    queue.push(b"abcdef".to_vec(), Room::Limit).unwrap();
    assert_eq!(queue.pop(), Some(b"abcdef".to_vec()));
    // Кадр в записи держит место: остановленный приёмник не освобождает очередь.
    assert_eq!(queue.counted(), 6);
    assert!(queue.push(b"ghi".to_vec(), Room::Limit).is_err());
    queue.written();
    assert_eq!(queue.counted(), 0);
    queue.push(b"ghi".to_vec(), Room::Limit).unwrap();
}

#[test]
fn eviction_drops_queued_frames_and_ends_with_the_notice() {
    let queue = ByteQueue::new(16);
    queue.push(b"live1".to_vec(), Room::Limit).unwrap();
    assert_eq!(queue.pop(), Some(b"live1".to_vec()));
    queue.push(b"live2".to_vec(), Room::Limit).unwrap();
    queue.push(b"live3".to_vec(), Room::Limit).unwrap();
    assert_eq!(queue.evict(b"bye".to_vec()), 10);
    assert_eq!(queue.counted(), 5 + 3);
    assert_eq!(
        queue.push(b"late".to_vec(), Room::Always),
        Err(Refusal::Closed)
    );
    queue.written();
    assert_eq!(queue.pop(), Some(b"bye".to_vec()));
    queue.written();
    assert_eq!(queue.pop(), None);
    assert_eq!(queue.counted(), 0);
    // Повторный разрыв второго уведомления не ставит.
    assert_eq!(queue.evict(b"again".to_vec()), 0);
    assert_eq!(queue.pop(), None);
}

/// Приёмник, который берёт не больше `step` байт за вызов и через раз прерывается.
struct Partial {
    got: Vec<u8>,
    step: usize,
    calls: usize,
    fail_after: Option<usize>,
}

impl Write for Partial {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        if self.calls.is_multiple_of(2) {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        if self.fail_after.is_some_and(|limit| self.got.len() >= limit) {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        let taken = bytes.len().min(self.step);
        self.got.extend_from_slice(&bytes[..taken]);
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn partial_writes_deliver_every_frame_whole_and_in_order() {
    let queue = ByteQueue::new(4 << 10);
    let frames: Vec<Vec<u8>> = (0..20u8)
        .map(|index| vec![index; 1 + usize::from(index) * 7])
        .collect();
    for frame in &frames {
        queue.push(frame.clone(), Room::Limit).unwrap();
    }
    queue.close();
    let mut sink = Partial {
        got: Vec::new(),
        step: 3,
        calls: 0,
        fail_after: None,
    };
    drain(&queue, &mut sink).unwrap();
    assert_eq!(sink.got, frames.concat());
    assert_eq!(queue.counted(), 0);
    assert!(queue.wait_finished(Instant::now()));
}

#[test]
fn failed_write_releases_the_queue_and_finishes_the_writer() {
    let queue = ByteQueue::new(1 << 10);
    queue.push(vec![1; 40], Room::Limit).unwrap();
    queue.push(vec![2; 40], Room::Limit).unwrap();
    let mut sink = Partial {
        got: Vec::new(),
        step: 16,
        calls: 0,
        fail_after: Some(20),
    };
    let err = drain(&queue, &mut sink).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(sink.got.len(), 32);
    assert_eq!(queue.counted(), 0);
    assert_eq!(queue.push(vec![3], Room::Always), Err(Refusal::Closed));
    assert!(queue.wait_finished(Instant::now()));
}

#[test]
fn writer_thread_drains_concurrent_pushes() {
    let queue = Arc::new(ByteQueue::new(64));
    let writer = {
        let queue = Arc::clone(&queue);
        thread::spawn(move || {
            let mut sink = Vec::new();
            drain(&queue, &mut sink).map(|()| sink)
        })
    };
    let mut sent = Vec::new();
    for index in 0..200u32 {
        let frame = index.to_be_bytes().to_vec();
        // Место освобождает писатель: ждём его, а не время.
        while let Err(Refusal::Full { .. }) = queue.push(frame.clone(), Room::Limit) {
            thread::yield_now();
        }
        sent.extend_from_slice(&frame);
    }
    queue.close();
    assert_eq!(writer.join().unwrap().unwrap(), sent);
    assert!(queue.peak() <= 64);
}

#[test]
fn retried_input_is_a_duplicate_only_after_it_was_accepted() {
    let mut ledger = InputLedger::new(2);
    let first = InputId { source: 7, seq: 1 };
    assert!(!ledger.is_duplicate(first));
    // Отвергнутый ввод не запоминается: его повтор принимается заново.
    assert!(!ledger.is_duplicate(first));
    ledger.record(first);
    assert!(ledger.is_duplicate(first));
    assert!(!ledger.is_duplicate(InputId { source: 7, seq: 2 }));
    // Номер другого источника — новый ввод.
    assert!(!ledger.is_duplicate(InputId { source: 8, seq: 1 }));
    ledger.record(InputId { source: 7, seq: 3 });
    assert!(ledger.is_duplicate(InputId { source: 7, seq: 2 }));
    // Опоздавший старый номер последний номер не уменьшает.
    ledger.record(InputId { source: 7, seq: 2 });
    assert_eq!(ledger.last(7), Some(3));
}

#[test]
fn ledger_forgets_the_oldest_source_first() {
    let mut ledger = InputLedger::new(2);
    for source in [1, 2] {
        ledger.record(InputId { source, seq: 1 });
    }
    // Свежий ввод источника 1 делает его самым новым.
    ledger.record(InputId { source: 1, seq: 2 });
    ledger.record(InputId { source: 3, seq: 1 });
    assert_eq!(ledger.last(2), None);
    assert_eq!(ledger.last(1), Some(2));
    assert_eq!(ledger.last(3), Some(1));
}

#[test]
fn two_clients_of_different_size_have_one_size_source() {
    let mut lead = Leadership::<u64>::default();
    let first = lead.attach(1, Lead::IfFree, size(80, 24));
    assert_eq!(first.role, Role::Leader);
    assert_eq!(first.generation, 1);
    assert_eq!(first.resize, Some(size(80, 24)));
    // Второй клиент другого размера ведомый: размер сессии не меняется.
    let second = lead.attach(2, Lead::IfFree, size(100, 30));
    assert_eq!(second.role, Role::Follower);
    assert_eq!(second.resize, None);
    assert_eq!(second.generation, 1);
    assert!(lead.accepts_resize(Origin::Client(1)));
    assert!(!lead.accepts_resize(Origin::Client(2)));
    assert!(!lead.accepts_input(Origin::Client(2)));
    // Ввод и ресайз из CLI лидерства не берут.
    assert!(lead.accepts_input(Origin::Control));
    assert!(lead.accepts_resize(Origin::Control));
    assert_eq!(lead.leader(), Some(1));
}

#[test]
fn claim_makes_the_previous_leader_resize_stale() {
    let mut lead = Leadership::<u64>::default();
    lead.attach(1, Lead::IfFree, size(80, 24));
    lead.attach(2, Lead::IfFree, size(100, 30));
    let third = lead.attach(3, Lead::Claim, size(110, 32));
    assert_eq!(third.role, Role::Leader);
    assert_eq!(third.demoted, Some(1));
    assert_eq!(third.generation, 2);
    assert_eq!(third.resize, Some(size(110, 32)));
    assert!(!lead.accepts_resize(Origin::Client(1)));
    assert!(lead.accepts_resize(Origin::Client(3)));
    // Уход ведомого лидерство не меняет, уход лидера оставляет сессию без лидера.
    assert!(!lead.detach(2));
    assert!(lead.detach(3));
    assert_eq!(lead.leader(), None);
    assert!(!lead.accepts_resize(Origin::Client(1)));
    let back = lead.attach(1, Lead::IfFree, size(80, 24));
    assert_eq!(
        (back.role, back.generation, back.demoted),
        (Role::Leader, 3, None)
    );
}
