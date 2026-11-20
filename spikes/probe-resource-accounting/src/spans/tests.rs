use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};

use super::clock::fake::{self, FakeClock};
use super::*;

fn panel(id: &str) -> Owner {
    Owner::Panel(id.to_owned())
}

fn poll_once<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    let mut cx = Context::from_waker(Waker::noop());
    Pin::new(future).poll(&mut cx)
}

/// Future, которая на каждом `poll` тратит очередную долю CPU потока и готова после последней.
fn steps(costs: Vec<u64>) -> impl Future<Output = ()> + Send {
    let mut index = 0;
    poll_fn(move |_| {
        fake::spend(costs[index]);
        index += 1;
        if index == costs.len() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
}

fn stack_depth() -> usize {
    STACK.with(|stack| stack.borrow().len())
}

#[test]
fn waiting_between_polls_is_not_charged() {
    fake::set(1_000);
    let ledger = Ledger::new(FakeClock);
    let mut a = ledger.wrap(&panel("A"), steps(vec![3, 4]));
    let mut b = ledger.wrap(&panel("B"), steps(vec![50]));
    assert!(poll_once(&mut a).is_pending());
    // A ждёт: поток занят работой B и работой вне участков.
    assert!(poll_once(&mut b).is_ready());
    fake::spend(100);
    assert!(poll_once(&mut a).is_ready());
    drop((a, b));
    let a = ledger.charge(&panel("A"));
    assert_eq!((a.cpu_ns, a.spans, a.unmeasured_spans), (7, 2, 0));
    assert_eq!(ledger.charge(&panel("B")).cpu_ns, 50);
    assert_eq!(stack_depth(), 0);
}

#[test]
fn nested_span_of_another_owner_is_subtracted_from_outer() {
    fake::set(77);
    let ledger = Arc::new(Ledger::new(FakeClock));
    let inner_ledger = Arc::clone(&ledger);
    let mut outer = ledger.wrap(
        &panel("A"),
        poll_fn(move |cx| {
            fake::spend(3);
            let mut inner = inner_ledger.wrap(&panel("B"), steps(vec![7]));
            assert!(Pin::new(&mut inner).poll(cx).is_ready());
            // Тот же владелец внутри себя: начисляется один раз.
            let mut same = inner_ledger.wrap(&panel("A"), steps(vec![11]));
            assert!(Pin::new(&mut same).poll(cx).is_ready());
            fake::spend(2);
            Poll::Ready(())
        }),
    );
    let before = FakeClock.now_ns().unwrap();
    assert!(poll_once(&mut outer).is_ready());
    let total = FakeClock.now_ns().unwrap() - before;
    drop(outer);
    assert_eq!(ledger.charge(&panel("A")).cpu_ns, 3 + 11 + 2);
    assert_eq!(ledger.charge(&panel("A")).spans, 2);
    assert_eq!(ledger.charge(&panel("B")).cpu_ns, 7);
    let charged: u64 = ledger.charges().iter().map(|c| c.charge.cpu_ns).sum();
    assert_eq!(charged, total);
    assert_eq!(stack_depth(), 0);
}

#[test]
fn parts_of_one_future_on_two_threads_add_up() {
    let ledger = Ledger::new(FakeClock);
    let future = ledger.wrap(&panel("A"), steps(vec![10, 20, 30]));
    let (to_second, at_second) = mpsc::channel::<Accounted<_, FakeClock>>();
    let (to_first, at_first) = mpsc::channel();
    let second = std::thread::spawn(move || {
        fake::set(5_000_000);
        let mut future = at_second.recv().unwrap();
        fake::spend(1_000);
        assert!(poll_once(&mut future).is_pending());
        fake::spend(1_000);
        to_first.send(future).unwrap();
        thread_index()
    });
    fake::set(42);
    let first = thread_index();
    let mut future = future;
    assert!(poll_once(&mut future).is_pending());
    fake::spend(500);
    to_second.send(future).unwrap();
    let mut future = at_first.recv().unwrap();
    fake::spend(500);
    assert!(poll_once(&mut future).is_ready());
    let second = second.join().unwrap();
    drop(future);
    let charge = ledger.charge(&panel("A"));
    assert_eq!(charge.cpu_ns, 60);
    assert_eq!(charge.spans, 3);
    assert_eq!(charge.migrations, 2);
    assert_eq!(
        charge.per_thread_ns,
        BTreeMap::from([(first, 40), (second, 20)])
    );
}

#[test]
fn parallel_spans_of_one_owner_sum_over_threads() {
    let ledger = Arc::new(Ledger::new(FakeClock));
    let workers: Vec<_> = [(13u64, 1_000u64), (29, 9_000)]
        .into_iter()
        .map(|(cost, base)| {
            let ledger = Arc::clone(&ledger);
            std::thread::spawn(move || {
                fake::set(base);
                let mut future = ledger.wrap(&panel("A"), steps(vec![cost]));
                assert!(poll_once(&mut future).is_ready());
                (thread_index(), cost)
            })
        })
        .collect();
    let parts: BTreeMap<u64, u64> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    let charge = ledger.charge(&panel("A"));
    assert_eq!(charge.cpu_ns, 42);
    assert_eq!(charge.migrations, 0);
    assert_eq!(charge.per_thread_ns, parts);
}

#[test]
fn shared_git_work_is_charged_to_git_not_to_first_requesting_panel() {
    fake::set(9);
    let ledger = Arc::new(Ledger::new(FakeClock));
    let inner = Arc::clone(&ledger);
    // Панель A запросила работу первой и опрашивает её у себя, B запросила ту же работу.
    let mut a = ledger.wrap(
        &panel("A"),
        poll_fn(move |cx| {
            fake::spend(4);
            let mut work = inner.wrap_shared_git(&["A", "B"], steps(vec![7]));
            assert!(Pin::new(&mut work).poll(cx).is_ready());
            fake::spend(1);
            Poll::Ready(())
        }),
    );
    assert!(poll_once(&mut a).is_ready());
    drop(a);
    // То же через замыкание для spawn_blocking.
    let nested = Arc::clone(&ledger);
    let outer = ledger.blocking(&panel("A"), move || {
        fake::spend(2);
        nested.blocking(&Owner::Git, || fake::spend(20))();
    });
    outer();
    let git = ledger.charge(&Owner::Git);
    assert_eq!(git.cpu_ns, 27);
    assert_eq!(git.requested_by, ["A", "B"]);
    assert_eq!(ledger.charge(&panel("A")).cpu_ns, 4 + 1 + 2);
    let owners: Vec<Owner> = ledger.charges().into_iter().map(|c| c.owner).collect();
    assert_eq!(owners, [Owner::Git, panel("A")]);
}

#[test]
fn failed_clock_read_marks_span_and_outer_unmeasured() {
    fake::set(0);
    let ledger = Arc::new(Ledger::new(FakeClock));
    let inner_ledger = Arc::clone(&ledger);
    let mut outer = ledger.wrap(
        &panel("A"),
        poll_fn(move |cx| {
            fake::spend(3);
            let mut inner = inner_ledger.wrap(
                &panel("B"),
                poll_fn(|_| {
                    fake::spend(5);
                    fake::break_reads(true);
                    Poll::Ready(())
                }),
            );
            assert!(Pin::new(&mut inner).poll(cx).is_ready());
            fake::break_reads(false);
            Poll::Ready(())
        }),
    );
    assert!(poll_once(&mut outer).is_ready());
    for owner in [panel("A"), panel("B")] {
        let charge = ledger.charge(&owner);
        assert_eq!(
            (charge.cpu_ns, charge.spans, charge.unmeasured_spans),
            (0, 1, 1)
        );
    }
    // Следующий участок на том же потоке снова измерен.
    let mut next = ledger.wrap(&panel("A"), steps(vec![6]));
    assert!(poll_once(&mut next).is_ready());
    assert_eq!(ledger.charge(&panel("A")).cpu_ns, 6);
}

#[test]
fn panic_inside_span_leaves_thread_stack_clean() {
    let ledger = Ledger::new(FakeClock);
    let mut broken = ledger.wrap(
        &panel("A"),
        poll_fn(|_| -> Poll<()> {
            fake::spend(1);
            panic!("сбой участка")
        }),
    );
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll_once(&mut broken)));
    assert!(caught.is_err());
    assert_eq!(stack_depth(), 0);
    assert_eq!(ledger.charge(&panel("A")).cpu_ns, 1);
}

#[test]
fn spawn_blocking_on_tokio_charges_owner() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .unwrap();
    let ledger = Ledger::new(FakeClock);
    let handle = ledger.spawn_blocking(runtime.handle(), &panel("A"), || {
        fake::spend(42);
        thread_index()
    });
    let thread = runtime.block_on(handle).unwrap();
    let charge = ledger.charge(&panel("A"));
    assert_eq!((charge.cpu_ns, charge.spans), (42, 1));
    assert_eq!(charge.per_thread_ns, BTreeMap::from([(thread, 42)]));
    assert_ne!(thread, thread_index());
}

#[test]
fn charges_serialize_with_owner_tags() {
    let ledger = Ledger::new(FakeClock);
    ledger.blocking(&panel("A"), || fake::spend(3))();
    let json = serde_json::to_value(ledger.charges()).unwrap();
    assert_eq!(json[0]["owner"]["kind"], "panel");
    assert_eq!(json[0]["charge"]["cpu_ns"], 3);
}
