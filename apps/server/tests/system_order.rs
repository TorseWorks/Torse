//! Rule admission must reject contradictory dependencies before any planning.
use torse_server::{bootstrap as base, execution::*};
use torse_sim::{Phase, SystemSpec};
use torse_types::{Binding, FaultCode, WorldEpoch};

struct Declaration(SystemSpec);

impl DurableRule for Declaration {
    fn spec(&self) -> SystemSpec {
        self.0.clone()
    }
}

#[test]
fn execution_host_rejects_contradictory_declarations_in_every_phase() {
    for phase in Phase::ALL {
        for contradictory in [false, true] {
            let (world, actions) = base::world().unwrap();
            let binding = Binding {
                world: base::WORLD,
                epoch: WorldEpoch(1),
                compatibility: world.coordinator().compatibility(),
            };
            let first = SystemSpec {
                id: base::key("ordering:first"),
                phase,
                rule_priority: 1,
                before: vec![base::key("ordering:second")],
                after: if contradictory {
                    vec![base::key("ordering:second")]
                } else {
                    vec![]
                },
            };
            let second = SystemSpec {
                id: base::key("ordering:second"),
                phase,
                rule_priority: 0,
                before: vec![],
                after: vec![first.id.clone()],
            };
            let result = ExecutionHost::new(
                binding,
                world,
                actions,
                vec![Box::new(Declaration(second)), Box::new(Declaration(first))],
                base::execution_limits(),
            );
            if contradictory {
                let error = result.err().expect("contradictory rules were admitted");
                assert_eq!(error.code, FaultCode::InvalidData);
                assert!(error.context.contains("cycle"));
            } else {
                assert!(result.is_ok(), "consistent reciprocal declarations failed");
            }
        }
    }
}
