//! Fixed native planning load, shared by the benchmark and semantic regression.

use torse_codec::{StorageCodec, wire};
use torse_server::{HostAuthority, bootstrap as base, execution::*};
use torse_sim::{OrderingKey, Phase, SystemSpec, Transaction, WorldAuthority};
use torse_types::*;
use torse_world::{BlockStateId, Snapshot, TerrainQuery};

#[derive(Clone, Copy)]
pub struct Workload {
    pub tasks: usize,
    pub contended: bool,
}

impl Workload {
    pub fn name(self) -> String {
        format!(
            "{}-{}",
            if self.contended {
                "contended"
            } else {
                "independent"
            },
            self.tasks
        )
    }

    pub fn rules(self) -> Vec<Box<dyn DurableRule>> {
        if self.tasks == 0 {
            Vec::new()
        } else {
            vec![Box::new(self)]
        }
    }

    pub fn successes(self) -> usize {
        if self.contended {
            self.tasks / 2
        } else {
            self.tasks
        }
    }

    pub fn verify(self, world: &WorldAuthority, ticks: u64) {
        let expected = if ticks.is_multiple_of(2) {
            BlockStateId::EMPTY
        } else {
            world
                .registry()
                .default_state(&base::key("base:stone"))
                .unwrap()
        };
        for index in 0..self.successes() {
            assert_eq!(
                world.store().query(position(index)),
                TerrainQuery::Ready(expected)
            );
        }
    }
}

fn position(index: usize) -> BlockPos {
    BlockPos::new((index % 32) as i64, 1, (index / 32 + 1) as i64)
}

impl DurableRule for Workload {
    fn spec(&self) -> SystemSpec {
        SystemSpec {
            id: base::key("baseline:edits"),
            phase: Phase::Simulation,
            rule_priority: 0,
            before: Vec::new(),
            after: Vec::new(),
        }
    }

    fn plan(&self, world: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        let stone = world
            .registry()
            .default_state(&base::key("base:stone"))
            .unwrap();
        let mut plans = Vec::with_capacity(self.tasks);
        for index in 0..self.tasks {
            let pos = position(if self.contended { index / 2 } else { index });
            let TerrainQuery::Ready(previous) = world.store().query(pos) else {
                return Err(Fault::invalid_data("baseline cell is not ready"));
            };
            let mut tx = Transaction::new(
                TransactionId(u128::from(tick.0) * 256 + index as u128 + 1),
                base::key("baseline:edits"),
                OrderingKey {
                    phase: Phase::Simulation,
                    rule_priority: 0,
                    stable_task_key: index as u64,
                    local_sequence: 0,
                },
            );
            tx.read_block(pos, previous).write_block(
                pos,
                if previous.is_empty() {
                    stone
                } else {
                    BlockStateId::EMPTY
                },
            );
            plans.push(WorkPlan::transaction(tx));
        }
        Ok(plans)
    }
}

pub fn snapshot(host: &ExecutionHost) -> Snapshot {
    host.world()
        .capture_snapshot(SnapshotMeta {
            cursor: host.cursor(),
            compatibility: host.binding().compatibility,
            resume_at: host.resume_point(),
        })
        .unwrap()
}

pub fn state_bytes(host: &ExecutionHost) -> Result<Vec<u8>, Fault> {
    let snapshot = WorldSnapshot {
        meta: SnapshotMeta {
            cursor: host.cursor(),
            compatibility: host.binding().compatibility,
            resume_at: host.resume_point(),
        },
        state: torse_sim::durable::ExecutionSnapshot {
            parked: Default::default(),
            parking_reservation: None,
            world: snapshot(host),
            runtime: host.runtime().clone(),
        },
    };
    let mut bytes = Vec::new();
    base::Codec::new(base::codec_limits())?.encode_snapshot(
        &snapshot,
        &mut bytes,
        base::budget(),
    )?;
    // Also validate the complete final execution state outside the measured path.
    let mut runtime = Vec::new();
    wire::encode(
        host.runtime(),
        &mut runtime,
        base::budget(),
        base::codec_limits(),
    )?;
    Ok(bytes)
}
