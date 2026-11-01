//! Input parity, host authorization, and native handler boundary tests.
use super::support::*;
use torse_inventory::{Inventory, ItemCatalog};
use torse_sim::action::{
    ActionHandler, ActionOrigin, ActionOutput, ActionRequest, ActionService, ActionTarget,
    ActionView, ActorContext,
};
use torse_sim::{Phase, Transaction, block_actions};
use torse_types::{BlockPos, EntityId, RejectCode};
use torse_world::{ComponentSet, ComponentValue, TerrainQuery};

#[test]
fn console_ui_network_and_mod_share_one_transfer_plan() {
    let (world, service, _, ids) = setup();
    let mut expected = None;
    for origin in [
        ActionOrigin::Console,
        ActionOrigin::Ui,
        ActionOrigin::Network,
        ActionOrigin::Mod,
    ] {
        let mut req = request(ids[0], ids[1], 1);
        req.origin = origin;
        if origin == ActionOrigin::Console {
            req = service
                .command("inventory:command.transfer", req)
                .expect("command resolves");
        }
        let output = service
            .plan(&world, &actor(&ids), Phase::Interaction, &req)
            .expect("plan");
        if let Some(expected) = &expected {
            assert_eq!(expected, &output);
        } else {
            expected = Some(output);
        }
    }
    assert_eq!(world.seq().0, 0);
}

#[test]
fn every_origin_requires_permission_and_secondary_target_access() {
    let (world, service, catalog, ids) = setup();
    for origin in [
        ActionOrigin::Console,
        ActionOrigin::Ui,
        ActionOrigin::Network,
        ActionOrigin::Mod,
    ] {
        let mut req = request(ids[0], ids[1], 1);
        req.origin = origin;
        let missing_permission = ActorContext::new(key("base:actor"))
            .target(ActionTarget::Entity(ids[0]))
            .target(ActionTarget::Entity(ids[1]));
        assert_eq!(
            service.plan(&world, &missing_permission, Phase::Interaction, &req),
            Err(RejectCode::PermissionDenied)
        );
        assert_eq!(
            service.plan(&world, &actor(&ids[..1]), Phase::Interaction, &req),
            Err(RejectCode::PermissionDenied)
        );
        let operation = torse_inventory::Transfer {
            source: ids[0],
            source_slot: 0,
            destination: ids[1],
            destination_slot: 0,
            count: 1,
        };
        assert_eq!(
            torse_inventory::plan_transfer(
                &missing_permission.view(&world),
                &catalog,
                operation,
                req.transaction,
                req.ordering
            ),
            Err(RejectCode::PermissionDenied)
        );
        let mut ordering = req.ordering;
        ordering.phase = Phase::Publish;
        assert_eq!(
            torse_inventory::plan_transfer(
                &actor(&ids).view(&world),
                &catalog,
                operation,
                req.transaction,
                ordering
            ),
            Err(RejectCode::InvalidData)
        );
    }
}

#[test]
fn input_versions_phases_and_schemas_are_checked_before_execution() {
    let (world, service, _, ids) = setup();
    for variant in 0..6 {
        let mut req = request(ids[0], ids[1], 1);
        match variant {
            0 => req.api_version += 1,
            1 => req.ordering.phase = Phase::Simulation,
            2 => {
                req.parameters
                    .insert(key("inventory:field/count"), ComponentValue::Bool(true));
            }
            3 => {
                req.parameters.remove(&key("inventory:field/count"));
            }
            4 => req.api_version = 1,
            _ => {
                req.parameters
                    .insert(key("unknown:field"), ComponentValue::U32(1));
            }
        }
        assert_eq!(
            service.plan(&world, &actor(&ids), Phase::Interaction, &req),
            Err(RejectCode::InvalidData)
        );
    }
    let req = request(ids[0], ids[1], 1);
    assert_eq!(
        service.plan(&world, &actor(&ids), Phase::Simulation, &req),
        Err(RejectCode::InvalidData)
    );
    assert!(service.command("shell:unknown", req).is_err());
}

#[test]
fn queries_are_read_only_and_nested_output_obeys_entry_and_byte_limits() {
    let (world, service, catalog, ids) = setup();
    let before = world.capture_snapshot(meta(&world)).expect("snapshot");
    let mut req = request(ids[0], ids[1], 1);
    req.action = key("inventory:action/inspect");
    req.parameters.clear();
    let ActionOutput::Query(fields) = service
        .plan(&world, &actor(&ids), Phase::Interaction, &req)
        .expect("query")
    else {
        panic!("query output");
    };
    assert_eq!(
        Inventory::from_fields(&fields, &catalog)
            .expect("inventory")
            .slots()
            .len(),
        2
    );
    for bytes in [false, true] {
        let mut descriptors = torse_inventory::descriptors();
        let query = descriptors
            .iter_mut()
            .find(|d| d.id == req.action)
            .expect("query descriptor");
        if bytes {
            query.max_bytes = 1;
        } else {
            query.max_items = 1;
        }
        let service = ActionService::bind(
            descriptors,
            torse_inventory::handlers(catalog.clone()),
            world.schemas(),
        )
        .expect("bind limited service");
        assert_eq!(
            service.plan(&world, &actor(&ids), Phase::Interaction, &req),
            Err(RejectCode::QuotaExceeded)
        );
    }
    assert_eq!(
        world.capture_snapshot(meta(&world)).expect("snapshot"),
        before
    );
}

#[test]
fn missing_duplicate_and_wrong_owner_bindings_fail_before_start() {
    let (world, _, catalog, _) = setup();
    for variant in 0..4 {
        let mut descriptors = torse_inventory::descriptors();
        let mut handlers = torse_inventory::handlers(catalog.clone());
        match variant {
            0 => {
                handlers.pop();
            }
            1 => {
                handlers[0].owner = key("base:wrong_owner");
            }
            2 => {
                handlers.extend(torse_inventory::handlers(catalog.clone()));
            }
            _ => {
                descriptors[0].parameter_schema = key("unknown:schema");
            }
        }
        assert!(ActionService::bind(descriptors, handlers, world.schemas()).is_err());
    }
}

struct ReadOther(EntityId);
impl ActionHandler for ReadOther {
    fn plan(
        &self,
        view: &ActionView<'_>,
        _request: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode> {
        Ok(ActionOutput::Query(
            view.component(self.0, &torse_inventory::schema())?.clone(),
        ))
    }
}

#[test]
fn native_query_view_rejects_reads_outside_host_grants() {
    let (world, _, catalog, ids) = setup();
    let mut handlers = torse_inventory::handlers(catalog);
    handlers[0].implementation = Box::new(ReadOther(ids[1]));
    let service = ActionService::bind(torse_inventory::descriptors(), handlers, world.schemas())
        .expect("bind");
    let mut req = request(ids[0], ids[1], 1);
    req.action = key("inventory:action/inspect");
    req.parameters.clear();
    assert_eq!(
        service.plan(&world, &actor(&ids[..1]), Phase::Interaction, &req),
        Err(RejectCode::PermissionDenied)
    );
}

struct WrongMutation {
    source: bool,
}
impl ActionHandler for WrongMutation {
    fn plan(
        &self,
        _view: &ActionView<'_>,
        req: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode> {
        let mut tx = Transaction::new(
            req.transaction,
            if self.source {
                key("base:wrong_owner")
            } else {
                torse_inventory::owner()
            },
            req.ordering,
        );
        tx.write_block(BlockPos::new(12, 2, 2), torse_world::BlockStateId::EMPTY);
        Ok(ActionOutput::Mutation(Box::new(tx)))
    }
}

#[test]
fn native_output_cannot_change_owner_or_write_ungranted_objects() {
    let (world, _, catalog, ids) = setup();
    for source in [false, true] {
        let mut handlers = torse_inventory::handlers(catalog.clone());
        handlers[1].implementation = Box::new(WrongMutation { source });
        let service =
            ActionService::bind(torse_inventory::descriptors(), handlers, world.schemas())
                .expect("bind");
        assert_eq!(
            service.plan(
                &world,
                &actor(&ids),
                Phase::Interaction,
                &request(ids[0], ids[1], 1)
            ),
            Err(if source {
                RejectCode::InvalidData
            } else {
                RejectCode::PermissionDenied
            })
        );
    }
}

struct LifecycleMutation;

impl ActionHandler for LifecycleMutation {
    fn plan(&self, _: &ActionView<'_>, req: &ActionRequest) -> Result<ActionOutput, RejectCode> {
        let mut tx = Transaction::new(req.transaction, torse_inventory::owner(), req.ordering);
        tx.spawn_entity(
            torse_world::Position::new(2.0, 2.0, 2.0),
            Default::default(),
        );
        Ok(ActionOutput::Mutation(Box::new(tx)))
    }
}

#[test]
fn native_actions_cannot_emit_lifecycle_without_declared_creation_grants() {
    let (world, _, catalog, ids) = setup();
    let mut handlers = torse_inventory::handlers(catalog);
    handlers[1].implementation = Box::new(LifecycleMutation);
    let service =
        ActionService::bind(torse_inventory::descriptors(), handlers, world.schemas()).unwrap();
    assert_eq!(
        service.plan(
            &world,
            &actor(&ids),
            Phase::Interaction,
            &request(ids[0], ids[1], 1)
        ),
        Err(RejectCode::PermissionDenied)
    );
    assert_eq!(world.entities().next_entity_id(), 4);
    assert_eq!(world.seq().0, 0);
}

#[test]
fn place_and_break_actions_use_the_same_authorized_transaction_path() {
    let (mut world, service, _, ids) = setup();
    let pos = BlockPos::new(8, 2, 2);
    let mut req = request(ids[0], ids[1], 1);
    req.target = ActionTarget::Block(pos);
    req.action = key("world:action/place");
    req.api_version = 1;
    req.parameters = [(
        key("world:field/block"),
        ComponentValue::Content(key("base:stone")),
    )]
    .into_iter()
    .collect();
    let ActionOutput::Mutation(tx) = service
        .plan(&world, &actor(&ids), Phase::Interaction, &req)
        .expect("place")
    else {
        panic!("mutation");
    };
    world.commit(&tx).expect("commit place");
    assert!(matches!(world.store().query(pos), TerrainQuery::Ready(state) if !state.is_empty()));
    assert_eq!(
        service.plan(&world, &actor(&ids), Phase::Interaction, &req),
        Err(RejectCode::Conflict)
    );
    req.action = key("world:action/break");
    req.parameters.clear();
    req.transaction.0 += 1;
    let ActionOutput::Mutation(tx) = service
        .plan(&world, &actor(&ids), Phase::Interaction, &req)
        .expect("break")
    else {
        panic!("mutation");
    };
    world.commit(&tx).expect("commit break");
    assert_eq!(
        world.store().query(pos),
        TerrainQuery::Ready(torse_world::BlockStateId::EMPTY)
    );
    assert_eq!(world.seq().0, 2);
}

#[test]
fn building_world_runs_without_the_inventory_module() {
    use torse_content::{compile, m0_fixture_pack};
    use torse_sim::WorldAuthority;
    use torse_types::{Compatibility, SectionPos, WorldId};
    use torse_world::{BlockStateRegistry, ComponentSchemas, Section, WorldBounds};
    let mut pack = m0_fixture_pack();
    block_actions::register_content(&mut pack);
    let content = compile(pack).expect("building content");
    assert!(!content.modules.contains_key(&torse_inventory::owner()));
    let schemas = ComponentSchemas::build(&content);
    let service = ActionService::bind(
        block_actions::descriptors(),
        block_actions::handlers(),
        &schemas,
    )
    .expect("block-only binding");
    let mut world = WorldAuthority::new(
        WorldId(51),
        BlockStateRegistry::build(&content).expect("registry"),
        schemas,
        16,
        WorldBounds::new(0, 32).expect("bounds"),
        Compatibility {
            authority_schema: 3,
            rules_digest: content.authority_digest,
            content_digest: content.presentation_digest,
        },
    )
    .expect("building world");
    world
        .store_mut()
        .request(SectionPos::ZERO)
        .expect("request");
    world
        .store_mut()
        .install(SectionPos::ZERO, Section::empty(16).expect("section"))
        .expect("install");
    world
        .store_mut()
        .finish_load(SectionPos::ZERO)
        .expect("load");
    let pos = BlockPos::new(8, 2, 2);
    let actor = ActorContext::new(key("base:builder"))
        .permission(key("world:permission/edit"))
        .target(ActionTarget::Block(pos));
    let mut req = request(EntityId(1), EntityId(2), 1);
    req.action = key("world:action/place");
    req.api_version = 1;
    req.target = ActionTarget::Block(pos);
    req.parameters = [(
        key("world:field/block"),
        ComponentValue::Content(key("base:stone")),
    )]
    .into_iter()
    .collect();
    let ActionOutput::Mutation(tx) = service
        .plan(&world, &actor, Phase::Interaction, &req)
        .expect("plan")
    else {
        panic!("expected mutation");
    };
    world.commit(&tx).expect("commit");
    assert_eq!(world.seq().0, 1);
    assert!(world.entities().is_empty());
    assert!(matches!(world.store().query(pos), TerrainQuery::Ready(state) if !state.is_empty()));
}

#[test]
fn nested_component_values_have_aggregate_budgets() {
    let (world, _, _, _) = setup();
    for value in [
        ComponentValue::List(vec![ComponentValue::Text("x".repeat(65_536))]),
        ComponentValue::List(vec![ComponentValue::Bool(false); 4097]),
    ] {
        let fields = [(key("inventory:field/slots"), value)]
            .into_iter()
            .collect();
        assert!(
            ComponentSet::validate(world.schemas(), &torse_inventory::schema(), &fields).is_err()
        );
    }
    assert!(ItemCatalog::new([(key("base:item"), 0)]).is_err());
    assert!(Inventory::new(65).is_err());
}
