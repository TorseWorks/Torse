//! Real nested inventory data travels only through A7's authorized projection.
use super::support::*;
use torse_sim::replica::{ReplicaLimits, ReplicaScope, ReplicaSource};
use torse_types::{
    ActorContextId, Binding, FaultCode, OutputBudget, ReplicaSeq, SectionPos, SessionId, Viewer,
    WorldEpoch,
};

fn budget() -> OutputBudget {
    OutputBudget {
        max_items: 10_000,
        max_bytes: 1_000_000,
    }
}
fn viewer(id: u128) -> Viewer {
    Viewer {
        session: SessionId(id),
        actor: ActorContextId(id),
    }
}
fn limits() -> ReplicaLimits {
    ReplicaLimits {
        max_subscriptions: 4,
        max_sections: 1,
        max_queued_batches: 4,
        snapshot: budget(),
        queue: budget(),
    }
}

#[test]
fn inventory_seed_and_transfer_delta_preserve_nested_slots_and_hide_ungranted_containers() {
    let (mut world, service, catalog, ids) = setup();
    install_instance(&mut world, &catalog, ids[0]);
    let binding = Binding {
        world: world.world(),
        epoch: WorldEpoch(10),
        compatibility: world.coordinator().compatibility(),
    };
    let mut source = ReplicaSource::new(world, binding, limits()).unwrap();
    let scope = ReplicaScope {
        sections: [SectionPos::ZERO].into(),
    };
    let owner = source
        .open_replica(viewer(1), actor(&ids), scope.clone(), budget())
        .unwrap();
    let owner = codec_roundtrip(&owner);
    let observer = source
        .open_replica(viewer(2), actor(&[]), scope.clone(), budget())
        .unwrap();
    assert!(
        observer
            .state
            .entities
            .iter()
            .all(|entity| entity.components.is_empty())
    );
    for entity in &owner.state.entities {
        assert_eq!(
            &entity.components[&torse_inventory::schema()],
            fields(source.world(), entity.id)
        );
    }
    let tx = plan(&service, source.world(), &ids, &request(ids[0], ids[1], 1));
    let prepared = source.prepare(&tx).unwrap();
    source.publish(prepared).unwrap();
    let batches = source
        .take_replica_batches(binding, owner.subscription, budget())
        .unwrap();
    let batches = codec_roundtrip(&batches);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].seq, ReplicaSeq(1));
    assert_eq!(batches[0].changes.entities.len(), 2);
    for entity in &batches[0].changes.entities {
        assert_eq!(
            &entity.components[&torse_inventory::schema()],
            fields(source.world(), entity.id)
        );
    }
    assert!(
        source
            .take_replica_batches(binding, observer.subscription, budget())
            .unwrap()
            .is_empty()
    );
    assert_eq!(total(source.world(), &ids, &catalog), 1);
    source.close_replica(binding, owner.subscription).unwrap();
    let reconnected = source
        .open_replica(viewer(1), actor(&ids), scope, budget())
        .unwrap();
    let reconnected = codec_roundtrip(&reconnected);
    let destination = reconnected
        .state
        .entities
        .iter()
        .find(|entity| entity.id == ids[1])
        .unwrap();
    let inventory = torse_inventory::Inventory::from_fields(
        &destination.components[&torse_inventory::schema()],
        &catalog,
    )
    .unwrap();
    assert_eq!(inventory.slots()[0].as_ref().unwrap().data, instance_data());
}

#[test]
fn nested_inventory_nodes_are_charged_to_seed_budget() {
    let (world, _, _, ids) = setup();
    let binding = Binding {
        world: world.world(),
        epoch: WorldEpoch(10),
        compatibility: world.coordinator().compatibility(),
    };
    let mut source = ReplicaSource::new(world, binding, limits()).unwrap();
    let scope = ReplicaScope {
        sections: [SectionPos::ZERO].into(),
    };
    // Root + section + 3 entities fit; their nested slots must also be charged.
    let small = OutputBudget {
        max_items: 6,
        max_bytes: 1_000_000,
    };
    assert_eq!(
        source
            .open_replica(viewer(1), actor(&ids), scope.clone(), small)
            .unwrap_err()
            .code,
        FaultCode::ResourceLimit
    );
    let observer = source
        .open_replica(viewer(2), actor(&[]), scope.clone(), small)
        .unwrap();
    source
        .close_replica(binding, observer.subscription)
        .unwrap();
    let owner = source
        .open_replica(viewer(1), actor(&ids), scope, budget())
        .unwrap();
    assert_eq!(owner.subscription.0, 2);
}
