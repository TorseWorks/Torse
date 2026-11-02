use super::*;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

fn context(bytes: usize) -> PlanningContext {
    PlanningContext {
        config: PlanningConfig {
            workers: 2,
            max_in_flight: 2,
        },
        max_work_bytes: bytes,
        stats: Default::default(),
    }
}

#[test]
fn native_lanes_overlap_join_and_return_in_input_order() {
    let context = context(4096);
    let arrived = (Mutex::new(0), Condvar::new());
    let results = context
        .map(8, 128, 128, |index| {
            if index < 2 {
                let mut count = arrived.0.lock().unwrap();
                *count += 1;
                arrived.1.notify_all();
                let (_guard, timeout) = arrived
                    .1
                    .wait_timeout_while(count, Duration::from_secs(5), |n| *n < 2)
                    .unwrap();
                assert!(!timeout.timed_out(), "native lanes did not overlap");
            }
            index
        })
        .unwrap()
        .unwrap();
    assert_eq!(results, (0..8).collect::<Vec<_>>());
    assert_eq!(context.stats.get().peak_lanes, 2);
    assert_eq!(context.stats.get().jobs, 8);
}

#[test]
fn rejected_native_work_invokes_no_callback() {
    let context = context(1024);
    for (count, retained, scratch) in [(8, 128, 128), (8, usize::MAX, 1), (1, 128, usize::MAX)] {
        assert!(
            context
                .map::<(), _>(count, retained, scratch, |_| panic!("unadmitted job ran"))
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(context.stats.get().budget_fallbacks, 3);
    assert_eq!(context.stats.get().batches, 0);
}

#[test]
fn worker_panic_joins_the_other_lane_before_returning_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let context = context(4096);
    let completed = AtomicUsize::new(0);
    assert!(
        context
            .map(8, 128, 128, |index| {
                if index == 0 {
                    panic!("native calculation failed");
                }
                completed.fetch_add(1, Ordering::SeqCst);
            })
            .is_err()
    );
    assert_eq!(completed.load(Ordering::SeqCst), 4);
}
