//! Run with `cargo run -p torse-types --example memory_policy`.
//! The host normally derives observations from its own system pressure sampler.
use std::time::Duration;
use torse_types::{
    AdaptiveMemoryPolicy, EvictionRank, MemoryBudget, MemoryClass, MemoryPolicy, MemoryPool,
    MemoryPressure,
};

fn main() -> Result<(), torse_types::Fault> {
    let pool = MemoryPool::new(MemoryBudget::new(100));
    let world = pool.reserve(MemoryClass::World, 40, EvictionRank::Pinned)?;
    // Keep each lease with the data it describes. These are logical bytes.
    let cache = pool.reserve(
        MemoryClass::Mesh,
        40,
        EvictionRank::Mesh { distance_sq: 100 },
    )?;
    pool.set_budget(MemoryBudget::new(160))?;
    pool.set_policy(MemoryPolicy::Adaptive(AdaptiveMemoryPolicy {
        min_bytes: 40,
        max_bytes: 160,
        headroom_bytes: 20,
        grow_step_bytes: 10,
        shrink_step_bytes: 40,
        hysteresis_bytes: 10,
        sample_interval: Duration::from_secs(1),
    }))?;
    pool.observe_pressure(
        Duration::ZERO,
        MemoryPressure {
            available_bytes: 0,
            pressured: true,
        },
    )?;
    // Target is 60; the hard cap is still 160. The plan cannot release data itself.
    assert_eq!(pool.status().target_bytes, 60);
    assert_eq!(pool.reclaim_plan().charges, [cache.id().unwrap()]);
    drop(cache); // Real hosts remove the selected owned data before releasing its lease.
    pool.set_policy(MemoryPolicy::Fixed { target_bytes: 60 })?;
    pool.set_budget(MemoryBudget::new(60))?;
    println!("{:?}", pool.status());
    drop(world);
    Ok(())
}
