//! P2 authority integration tests. Like the roundtrip tests these require a
//! feature (`authority`), build the machine guest through cargo, and are
//! skipped in default workspace runs.

#![cfg(feature = "authority")]

use std::env;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use torse_inventory::{Inventory, ItemStack};
use torse_mod_host::authority::{DispatchOutcome, ModInstance, ScenarioConfig, machine_scenario};
use torse_mod_host::component::{CallBudget, encode_component, engine_with};
use torse_types::{ContentKey, EntityId};
use torse_world::ComponentValue;
use wasmtime::component::Component;

/// Guest builds share one artifact path, so builds must not overlap.
static BUILD_LOCK: Mutex<()> = Mutex::new(());

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("test key")
}

fn workspace_root() -> PathBuf {
    let mut dir = env::current_dir().expect("current directory readable");
    for _ in 0..2 {
        dir.pop();
    }
    dir
}

fn guest_artifact_path() -> PathBuf {
    workspace_root()
        .join("target")
        .join("wasm32-unknown-unknown")
        .join("release")
        .join("torse_mod_machine.wasm")
}

fn build_guest(features: &[&str]) -> Vec<u8> {
    let _guard = BUILD_LOCK.lock().expect("build lock");
    let mut command = Command::new("cargo");
    command
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg("wasm32-unknown-unknown")
        .arg("-p")
        .arg("torse-mod-machine")
        .current_dir(workspace_root());
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
    let status = command.status().expect("cargo command spawnable");
    assert!(status.success(), "machine build failed with {features:?}");
    std::fs::read(guest_artifact_path()).expect("machine wasm artifact readable")
}

struct Fixture {
    instance: ModInstance,
    machine: EntityId,
    input: EntityId,
    output: EntityId,
    catalog: torse_inventory::ItemCatalog,
}

fn fixture(features: &[&str], stock_input: bool) -> Fixture {
    fixture_with_budget(features, stock_input, CallBudget::prototype())
}

fn fixture_with_budget(features: &[&str], stock_input: bool, budget: CallBudget) -> Fixture {
    let core = build_guest(features);
    let engine = engine_with(false).expect("engine");
    let bytes = encode_component(&core).expect("component encodes");
    let component = Component::new(&engine, &bytes).expect("component compiles");
    let config = ScenarioConfig {
        package: "machine:module",
        schema: "machine:schema/state",
        stock_input,
        granted_permissions: &["inventory:permission/transfer", "inventory:permission/read"],
    };
    let world = machine_scenario(&config).expect("scenario assembles");
    let machine = world.machine;
    let input = world.input;
    let output = world.output;
    let catalog = world.catalog;
    let instance = ModInstance::new(
        key("machine:module"),
        &engine,
        component,
        world.host,
        budget,
    )
    .expect("instance builds");
    let mut fixture = Fixture {
        instance,
        machine,
        input,
        output,
        catalog,
    };
    fixture
        .instance
        .host_mut()
        .seed_wakeup(1, machine, "machine:tick", key("machine:module"));
    fixture
}

fn slot0(instance: &ModInstance, entity: EntityId) -> Option<ItemStack> {
    let authority = instance.host().authority();
    let handle = authority.entities().handle_of(entity).expect("entity");
    let fields = authority
        .entities()
        .get(handle)
        .expect("entity")
        .components
        .component(&torse_inventory::schema())
        .expect("inventory component");
    let inventory = Inventory::from_fields(fields, &dummy_catalog()).expect("inventory");
    inventory.slots()[0].clone()
}

fn dummy_catalog() -> torse_inventory::ItemCatalog {
    torse_inventory::ItemCatalog::new([(key("base:stone"), 64)]).expect("catalog")
}

fn machine_progress(instance: &ModInstance, machine: EntityId) -> u32 {
    let authority = instance.host().authority();
    let handle = authority.entities().handle_of(machine).expect("entity");
    let fields = authority
        .entities()
        .get(handle)
        .expect("entity")
        .components
        .component(&key("machine:schema/state"))
        .expect("machine component");
    match fields.get(&key("machine:field/progress")) {
        Some(ComponentValue::U32(value)) => *value,
        other => panic!("unexpected progress field: {other:?}"),
    }
}

#[test]
fn machine_processes_and_transfers() {
    let mut fixture = fixture(&[], true);
    let data = [
        (key("test:durability"), ComponentValue::U32(23)),
        (
            key("extension:opaque"),
            ComponentValue::Text("preserved through item port".into()),
        ),
    ]
    .into();
    let stack = ItemStack {
        item: key("base:stone"),
        count: 1,
        data,
    };
    let mut input = Inventory::new(2).unwrap();
    input.set(0, Some(stack.clone()), &fixture.catalog).unwrap();
    let authority = fixture.instance.host_mut().authority_mut();
    let schemas = authority.schemas().clone();
    let handle = authority.entities().handle_of(fixture.input).unwrap();
    authority
        .entities_mut()
        .components_mut(handle)
        .unwrap()
        .insert(&schemas, &torse_inventory::schema(), input.fields())
        .unwrap();
    let outcomes = fixture.instance.run_ticks(1, 6).expect("ticks run");
    assert_eq!(outcomes.len(), 6);
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, DispatchOutcome::Committed { .. })),
        "unexpected outcomes: {outcomes:?}"
    );
    // Four wakeups advance the counter, the fourth also transfers and resets.
    assert_eq!(machine_progress(&fixture.instance, fixture.machine), 2);
    assert_eq!(slot0(&fixture.instance, fixture.input), None);
    assert_eq!(slot0(&fixture.instance, fixture.output), Some(stack));
    assert!(fixture.instance.host().seq().0 >= 6);
}

#[test]
fn failed_transfer_leaves_no_partial_processing() {
    let mut fixture = fixture(&[], false);
    let outcomes = fixture.instance.run_ticks(1, 3).expect("ticks run");
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, DispatchOutcome::Committed { .. }))
    );
    assert_eq!(machine_progress(&fixture.instance, fixture.machine), 3);
    let seq_before = fixture.instance.host().seq().0;

    // The fourth step tries to transfer from an empty chest.
    let outcomes = fixture.instance.run_ticks(4, 1).expect("tick runs");
    assert!(
        matches!(&outcomes[0], DispatchOutcome::Rejected { reason } if reason.contains("transfer rejected")),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
    assert_eq!(
        machine_progress(&fixture.instance, fixture.machine),
        3,
        "rejected plan must not advance progress"
    );
    assert_eq!(fixture.instance.host().seq().0, seq_before);
    assert_eq!(
        fixture
            .instance
            .host()
            .consecutive_failures(&key("machine:module")),
        1
    );
    assert_eq!(slot0(&fixture.instance, fixture.output), None);

    // Restock the input chest and let the rescheduled wakeup finish the job.
    restock(&mut fixture);
    let outcomes = fixture.instance.run_ticks(5, 1).expect("tick runs");
    assert!(
        matches!(&outcomes[0], DispatchOutcome::Committed { .. }),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
    assert_eq!(machine_progress(&fixture.instance, fixture.machine), 0);
    assert_eq!(
        slot0(&fixture.instance, fixture.output),
        Some(ItemStack {
            data: Default::default(),
            item: key("base:stone"),
            count: 1,
        })
    );
}

fn restock(fixture: &mut Fixture) {
    let authority = fixture.instance.host_mut().authority_mut();
    let handle = authority
        .entities()
        .handle_of(fixture.input)
        .expect("entity");
    let mut inventory = Inventory::new(2).expect("inventory");
    inventory
        .set(
            0,
            Some(ItemStack {
                data: Default::default(),
                item: key("base:stone"),
                count: 1,
            }),
            &fixture.catalog,
        )
        .expect("stock");
    let schemas = authority.schemas().clone();
    authority
        .entities_mut()
        .components_mut(handle)
        .expect("components")
        .insert(&schemas, &torse_inventory::schema(), inventory.fields())
        .expect("install");
}

#[test]
fn foreign_schema_ownership_is_rejected() {
    let mut fixture = fixture(&["intrude"], true);
    let outcomes = fixture.instance.run_ticks(1, 1).expect("tick runs");
    assert!(
        matches!(&outcomes[0], DispatchOutcome::Rejected { reason } if reason.contains("owned by")),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
    assert_eq!(fixture.instance.host().seq().0, 0, "nothing commits");
    assert_eq!(
        fixture
            .instance
            .host()
            .consecutive_failures(&key("machine:module")),
        1
    );
}

#[test]
fn consecutive_failures_deactivate_the_handler() {
    let mut fixture = fixture(&[], false);
    let outcomes = fixture.instance.run_ticks(1, 11).expect("ticks run");
    // Ticks 1-3 commit; ticks 4-11 are eight consecutive rejections, the last
    // of which reaches the deactivation threshold.
    assert!(
        fixture
            .instance
            .host()
            .is_deactivated(&key("machine:module")),
        "{outcomes:?}"
    );
    let committed = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, DispatchOutcome::Committed { .. }))
        .count();
    assert_eq!(committed, 3);
    assert_eq!(
        fixture
            .instance
            .host()
            .consecutive_failures(&key("machine:module")),
        8,
        "{outcomes:?}"
    );

    // The eighth failure deactivated the handler; later dispatch is refused.
    fixture.instance.host_mut().seed_wakeup(
        50,
        fixture.machine,
        "machine:tick",
        key("machine:module"),
    );
    let outcomes = fixture.instance.run_ticks(50, 1).expect("tick runs");
    assert_eq!(outcomes, vec![DispatchOutcome::Deactivated]);
}

#[test]
fn trap_rebuilds_the_instance_from_committed_state() {
    let mut fixture = fixture(&["spin"], true);
    let outcomes = fixture.instance.run_ticks(1, 1).expect("tick runs");
    assert!(
        matches!(&outcomes[0], DispatchOutcome::Trapped { reason } if reason.contains("fuel")),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
    assert_eq!(fixture.instance.host().seq().0, 0);
    assert_eq!(
        fixture
            .instance
            .host()
            .consecutive_failures(&key("machine:module")),
        1
    );
    assert_eq!(machine_progress(&fixture.instance, fixture.machine), 0);

    // The rebuilt instance still executes (and traps again on the spin guest),
    // proving the store was replaced rather than left corrupt.
    let outcomes = fixture.instance.run_ticks(2, 1).expect("tick runs");
    assert!(
        matches!(&outcomes[0], DispatchOutcome::Trapped { .. }),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
    assert_eq!(
        fixture
            .instance
            .host()
            .consecutive_failures(&key("machine:module")),
        2
    );
}

#[test]
fn budget_errors_cannot_commit_even_when_the_guest_ignores_them() {
    let mut fixture = fixture_with_budget(
        &["budget-probe"],
        true,
        CallBudget {
            max_host_work: 4,
            max_input_bytes: 2048,
            max_guest_instances: 4,
            ..CallBudget::prototype()
        },
    );
    for tick in 1..=5 {
        let event = torse_mod_host::authority::InvokeEvent::Wakeup(
            torse_mod_host::authority::WakeupTarget {
                entity: torse_mod_host::authority::WitEntityId::from_entity_id(fixture.machine),
                key: "machine:tick".to_string(),
            },
        );
        let outcome = fixture
            .instance
            .dispatch(tick, event)
            .expect("budget recovery succeeds");
        assert!(
            matches!(
                outcome,
                DispatchOutcome::Rejected { .. } | DispatchOutcome::Trapped { .. }
            ),
            "tick {tick}: {outcome:?}"
        );
        assert_eq!(fixture.instance.host().seq().0, 0);
        assert_eq!(machine_progress(&fixture.instance, fixture.machine), 0);
    }
    let event =
        torse_mod_host::authority::InvokeEvent::Wakeup(torse_mod_host::authority::WakeupTarget {
            entity: torse_mod_host::authority::WitEntityId::from_entity_id(fixture.machine),
            key: "machine:tick".to_string(),
        });
    assert!(matches!(
        fixture.instance.dispatch(6, event).unwrap(),
        DispatchOutcome::Committed { .. }
    ));
    assert_eq!(machine_progress(&fixture.instance, fixture.machine), 1);
}

#[cfg(feature = "package")]
fn resolved_machine() -> torse_mod_host::package::ResolvedModSet {
    torse_mod_host::package::resolve(
        &[workspace_root().join("examples/mods/machine")],
        &torse_mod_host::scenario::engine_base(),
    )
    .unwrap()
}

#[cfg(feature = "package")]
#[test]
fn durable_dispatch_discards_ignored_budget_errors_and_recovers() {
    use std::sync::Arc;
    use torse_mod_host::durable_dispatch::{DurableGuest, GuestOutcome, SymbolTable};
    let bytes = encode_component(&build_guest(&["budget-probe"])).unwrap();
    let set = resolved_machine();
    let symbols = Arc::new(SymbolTable::build(
        &set.content,
        &["machine:schema/state".to_string()],
    ));
    let world = machine_scenario(&ScenarioConfig {
        package: "machine:module",
        schema: "machine:schema/state",
        stock_input: true,
        granted_permissions: &["inventory:permission/transfer", "inventory:permission/read"],
    })
    .unwrap();
    let mut guest = DurableGuest::new(
        &bytes,
        symbols,
        CallBudget {
            max_host_work: 140,
            max_input_bytes: 2048,
            max_guest_instances: 4,
            ..CallBudget::prototype()
        },
    )
    .unwrap();
    let owned = [key("machine:schema/state")];
    for tick in 1..=5 {
        let result = guest
            .dispatch(
                world.host.authority(),
                tick,
                world.machine,
                "machine:tick",
                &owned,
            )
            .unwrap();
        assert!(
            matches!(result, GuestOutcome::Trapped(_)),
            "tick {tick} must discard the plan"
        );
        assert_eq!(world.host.seq().0, 0);
    }
    assert!(matches!(
        guest
            .dispatch(
                world.host.authority(),
                6,
                world.machine,
                "machine:tick",
                &owned
            )
            .unwrap(),
        GuestOutcome::Plan(_)
    ));
}

#[cfg(feature = "package")]
#[test]
fn shared_runtime_reclaims_trapped_instances_before_rebuilding() {
    use torse_mod_host::scenario::BehaviorRuntime;
    let bytes = encode_component(&build_guest(&["budget-probe"])).unwrap();
    let set = resolved_machine();
    let world = machine_scenario(&ScenarioConfig {
        package: "machine:module",
        schema: "machine:schema/state",
        stock_input: true,
        granted_permissions: &["inventory:permission/transfer", "inventory:permission/read"],
    })
    .unwrap();
    let machine = world.machine;
    let mut runtime = BehaviorRuntime::new(
        &set,
        world.host,
        [(key("machine:module"), bytes)].into(),
        CallBudget {
            max_host_work: 4,
            max_input_bytes: 2048,
            max_guest_instances: 4,
            ..CallBudget::prototype()
        },
    )
    .unwrap();
    for tick in 1..=6 {
        let event = torse_mod_host::authority::InvokeEvent::Wakeup(
            torse_mod_host::authority::WakeupTarget {
                entity: torse_mod_host::authority::WitEntityId::from_entity_id(machine),
                key: "machine:tick".to_string(),
            },
        );
        let result = runtime
            .dispatch(&key("machine:module"), tick, event)
            .unwrap();
        if tick <= 5 {
            assert!(matches!(
                result,
                DispatchOutcome::Rejected { .. } | DispatchOutcome::Trapped { .. }
            ));
            assert_eq!(runtime.host().seq().0, 0);
        } else {
            assert!(matches!(result, DispatchOutcome::Committed { .. }));
        }
    }
}
