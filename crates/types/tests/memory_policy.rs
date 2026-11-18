use std::sync::{Arc, Barrier};
use std::time::Duration;
use torse_types::{
    AdaptiveMemoryPolicy, EvictionRank, FaultCode, MemoryBudget, MemoryClass, MemoryLedger,
    MemoryPolicy, MemoryPool, MemoryPressure, MemoryResident, RetainFailure,
};

fn adaptive() -> AdaptiveMemoryPolicy {
    AdaptiveMemoryPolicy {
        min_bytes: 20,
        max_bytes: 100,
        headroom_bytes: 10,
        grow_step_bytes: 10,
        shrink_step_bytes: 20,
        hysteresis_bytes: 5,
        sample_interval: Duration::from_secs(2),
    }
}

fn sample(pool: &MemoryPool, seconds: u64, available_bytes: u64, pressured: bool) {
    pool.observe_pressure(
        Duration::from_secs(seconds),
        MemoryPressure {
            available_bytes,
            pressured,
        },
    )
    .unwrap();
}

#[test]
fn budget_updates_preserve_live_leases_and_peaks_and_refuse_atomically() {
    let pool = MemoryPool::new(MemoryBudget::new(100));
    let mut held = pool
        .reserve(MemoryClass::World, 80, EvictionRank::Pinned)
        .unwrap();
    pool.set_policy(MemoryPolicy::Fixed { target_bytes: 40 })
        .unwrap();
    let before = pool.status();
    for budget in [
        MemoryBudget::new(79),
        MemoryBudget::new(100).with_class_limit(MemoryClass::World, 79),
        MemoryBudget::new(39),
    ] {
        assert!(pool.set_budget(budget).is_err());
        assert_eq!(pool.status(), before);
    }
    pool.set_budget(MemoryBudget::new(160)).unwrap();
    let copy = pool.clone();
    let extra = copy
        .reserve(MemoryClass::Resource, 80, EvictionRank::ReloadableResource)
        .unwrap();
    assert_eq!(pool.status().usage.total_peak, 160);
    drop(extra);
    held.resize(40).unwrap();
    pool.set_budget(MemoryBudget::new(40)).unwrap();
    assert_eq!(copy.budget(), MemoryBudget::new(40));
    assert_eq!(copy.status().target_bytes, 40);
    assert!(held.resize(41).is_err());
    assert_eq!(held.bytes(), 40);
    drop(held);
    assert_eq!(pool.usage().total, 0);
    assert_eq!(pool.usage().total_peak, 160);
    pool.set_policy(MemoryPolicy::Fixed { target_bytes: 0 })
        .unwrap();
    pool.set_budget(MemoryBudget::new(0)).unwrap();
    assert!(
        pool.reserve(MemoryClass::World, 1, EvictionRank::Pinned)
            .is_err()
    );
}

#[test]
fn retained_demands_survive_updates_and_must_be_forgotten_before_incompatible_shrink() {
    let mut ledger = MemoryLedger::new(MemoryBudget::new(10).with_retained_limit(2));
    let held = ledger
        .charge(MemoryResident {
            class: MemoryClass::World,
            bytes: 10,
            rank: EvictionRank::Pinned,
        })
        .unwrap();
    let demand = match ledger.charge_retained(MemoryResident {
        class: MemoryClass::Mesh,
        bytes: 8,
        rank: EvictionRank::UnsubscribedDerived,
    }) {
        Err(RetainFailure::Retained(failure)) => failure.id,
        other => panic!("expected waiting demand, got {other:?}"),
    };
    let before = ledger.budget();
    assert!(ledger.set_budget(before.with_retained_limit(0)).is_err());
    assert!(
        ledger
            .set_budget(before.with_class_limit(MemoryClass::Mesh, 7))
            .is_err()
    );
    assert_eq!(ledger.budget(), before);
    assert_eq!(ledger.retained_len(), 1);
    ledger.release(held).unwrap();
    assert!(ledger.set_budget(MemoryBudget::new(7)).is_err());
    ledger.set_budget(MemoryBudget::new(18)).unwrap();
    let report = ledger.retry_retained();
    assert_eq!(report.admitted[0].demand, demand);
    assert_eq!(ledger.total_usage(), 8);
    ledger.release(report.admitted[0].charge).unwrap();
    let held = ledger
        .charge(MemoryResident {
            class: MemoryClass::World,
            bytes: 18,
            rank: EvictionRank::Pinned,
        })
        .unwrap();
    let demand = match ledger.charge_retained(MemoryResident {
        class: MemoryClass::Mesh,
        bytes: 8,
        rank: EvictionRank::UnsubscribedDerived,
    }) {
        Err(RetainFailure::Retained(failure)) => failure.id,
        other => panic!("expected waiting demand, got {other:?}"),
    };
    ledger.release(held).unwrap();
    assert!(ledger.set_budget(MemoryBudget::new(0)).is_err());
    ledger.forget_retained(demand).unwrap();
    ledger
        .set_budget(MemoryBudget::new(0).with_retained_limit(0))
        .unwrap();
}

#[test]
fn soft_shrink_reclaims_rebuildable_data_and_keeps_authority_and_pinned_work() {
    let pool = MemoryPool::new(MemoryBudget::new(100));
    let world = pool
        .reserve(
            MemoryClass::World,
            30,
            EvictionRank::Mesh { distance_sq: 100 },
        )
        .unwrap();
    let job = pool
        .reserve(MemoryClass::Executor, 10, EvictionRank::Pinned)
        .unwrap();
    let far = pool
        .reserve(
            MemoryClass::Mesh,
            40,
            EvictionRank::Mesh { distance_sq: 10 },
        )
        .unwrap();
    let near = pool
        .reserve(MemoryClass::Mesh, 20, EvictionRank::Mesh { distance_sq: 0 })
        .unwrap();
    pool.set_policy(MemoryPolicy::Fixed { target_bytes: 60 })
        .unwrap();
    assert_eq!(pool.status().reclaim_bytes(), 40);
    assert_eq!(pool.reclaim_plan().charges, [far.id().unwrap()]);
    drop(far);
    assert_eq!(pool.status().reclaim_bytes(), 0);
    pool.set_budget(MemoryBudget::new(60)).unwrap();
    assert_eq!(world.bytes(), 30);
    assert_eq!(job.bytes(), 10);
    assert_eq!(near.bytes(), 20);
    pool.set_policy(MemoryPolicy::Fixed { target_bytes: 0 })
        .unwrap();
    let plan = pool.reclaim_plan();
    assert_eq!(plan.charges, [near.id().unwrap()]);
    assert!(!plan.covers(pool.status().reclaim_bytes()));
    drop((near, job, world));
    assert_eq!(pool.usage().total, 0);
}

#[test]
fn adaptive_target_uses_host_allowance_hysteresis_and_bounded_steps() {
    let pool = MemoryPool::new(MemoryBudget::new(100));
    let _world = pool
        .reserve(MemoryClass::World, 40, EvictionRank::Pinned)
        .unwrap();
    pool.set_policy(MemoryPolicy::Fixed { target_bytes: 60 })
        .unwrap();
    pool.set_policy(MemoryPolicy::Adaptive(adaptive())).unwrap();
    sample(&pool, 0, 35, false); // Desired 65 is inside hysteresis.
    assert_eq!(pool.status().target_bytes, 60);
    sample(&pool, 1, 1000, false); // Too early, not another step.
    assert_eq!(pool.status().target_bytes, 60);
    sample(&pool, 2, 1000, false);
    assert_eq!(pool.status().target_bytes, 70);
    sample(&pool, 100, u64::MAX, false); // No catch-up or overflow.
    assert_eq!(pool.status().target_bytes, 80);
    sample(&pool, 102, u64::MAX, true);
    assert_eq!(pool.status().target_bytes, 60);
    sample(&pool, 104, 0, false);
    assert_eq!(pool.status().target_bytes, 40);
    sample(&pool, 106, 0, true);
    assert_eq!(pool.status().target_bytes, 20);
    sample(&pool, 108, 0, true);
    assert_eq!(pool.status().target_bytes, 20);
    assert_eq!(pool.status().budget.total_bytes(), 100);
    assert_eq!(pool.status().usage.total, 40); // Advisory target can be below pinned usage.
    let before = pool.status();
    for seconds in [108, 105] {
        assert_eq!(
            pool.observe_pressure(
                Duration::from_secs(seconds),
                MemoryPressure {
                    available_bytes: 1000,
                    pressured: false
                }
            )
            .unwrap_err()
            .code,
            FaultCode::InvalidData
        );
        assert_eq!(pool.status(), before);
    }
    sample(&pool, 110, 1000, false);
    assert_eq!(pool.status().target_bytes, 30);
    pool.set_policy(MemoryPolicy::Fixed { target_bytes: 50 })
        .unwrap();
    sample(&pool, 0, 0, true); // Policy replacement resets host sample clock.
    assert_eq!(pool.status().target_bytes, 50);
}

#[test]
fn invalid_policy_and_incompatible_hard_shrink_leave_policy_and_clock_unchanged() {
    let pool = MemoryPool::new(MemoryBudget::new(100));
    pool.set_policy(MemoryPolicy::Adaptive(adaptive())).unwrap();
    sample(&pool, 10, 0, true);
    let before = pool.status();
    let mut policies = vec![MemoryPolicy::Fixed { target_bytes: 101 }];
    for invalid in [
        AdaptiveMemoryPolicy {
            min_bytes: 101,
            ..adaptive()
        },
        AdaptiveMemoryPolicy {
            max_bytes: 101,
            ..adaptive()
        },
        AdaptiveMemoryPolicy {
            grow_step_bytes: 0,
            ..adaptive()
        },
        AdaptiveMemoryPolicy {
            shrink_step_bytes: 0,
            ..adaptive()
        },
        AdaptiveMemoryPolicy {
            sample_interval: Duration::ZERO,
            ..adaptive()
        },
    ] {
        policies.push(MemoryPolicy::Adaptive(invalid));
    }
    for policy in policies {
        assert!(pool.set_policy(policy).is_err());
        assert_eq!(pool.status(), before);
    }
    assert!(pool.set_budget(MemoryBudget::new(99)).is_err());
    assert_eq!(pool.status(), before);
    sample(&pool, 11, 0, true);
    assert_eq!(pool.status(), before); // Failed writes did not reset the sample clock.
}

#[test]
fn racing_reservations_and_budget_updates_obey_one_atomic_account() {
    let pool = MemoryPool::new(MemoryBudget::new(100));
    pool.set_policy(MemoryPolicy::Fixed { target_bytes: 20 })
        .unwrap();
    let start = Arc::new(Barrier::new(5));
    let mut workers = Vec::new();
    for _ in 0..4 {
        let pool = pool.clone();
        let start = start.clone();
        workers.push(std::thread::spawn(move || {
            start.wait();
            for _ in 0..500 {
                if let Ok(mut lease) = pool.reserve(MemoryClass::Executor, 10, EvictionRank::Pinned)
                {
                    let _ = lease.resize(20);
                    let snapshot = pool.status();
                    assert!(snapshot.usage.total <= snapshot.budget.total_bytes());
                    assert!(
                        snapshot.usage.current[MemoryClass::Executor.index()]
                            <= snapshot.budget.class_limit(MemoryClass::Executor)
                    );
                    std::thread::yield_now();
                }
            }
        }));
    }
    start.wait();
    for n in 0..1000 {
        let limit = if n % 2 == 0 { 20 } else { 100 };
        let _ = pool
            .set_budget(MemoryBudget::new(limit).with_class_limit(MemoryClass::Executor, limit));
        let snapshot = pool.status();
        assert!(snapshot.usage.total <= snapshot.budget.total_bytes());
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(pool.usage().total, 0);
    assert!(pool.usage().total_peak <= 100);
    pool.set_budget(MemoryBudget::new(20)).unwrap();
}
