//! A6 acceptance: four adapters, competing machines, block actions, and replay.

use torse_content::{compile, m0_fixture_pack};
use torse_inventory::{Inventory, ItemCatalog, ItemStack};
use torse_sim::action::{
    ActionOrigin, ActionOutput, ActionRequest, ActionService, ActionTarget, ActorContext,
};
use torse_sim::{OrderingKey, Phase, WorldAuthority, block_actions, changes_from_prepared};
use torse_types::{
    BlockPos, CommitCursor, Compatibility, ContentKey, EntityId, SectionPos, SnapshotMeta,
    TransactionId, TransactionOutcome, WorldId,
};
use torse_world::{
    BlockStateRegistry, ComponentSchemas, ComponentValue, Position, Section, WorldBounds,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn key(value: &str) -> ContentKey {
    ContentKey::parse(value).expect("scenario key")
}
fn check(valid: bool, message: &'static str) -> Result<()> {
    if valid { Ok(()) } else { Err(message.into()) }
}

fn meta(world: &WorldAuthority) -> SnapshotMeta {
    SnapshotMeta {
        cursor: CommitCursor {
            world: world.world(),
            seq: world.seq(),
        },
        compatibility: world.coordinator().compatibility(),
        resume_at: world.coordinator().resume_point(),
    }
}

fn request(source: EntityId, destination: EntityId, id: u128) -> ActionRequest {
    ActionRequest {
        action: key("inventory:action/transfer"),
        api_version: 2,
        origin: ActionOrigin::Network,
        target: ActionTarget::Entity(source),
        transaction: TransactionId(id),
        ordering: OrderingKey {
            phase: Phase::Interaction,
            rule_priority: 0,
            stable_task_key: id as u64,
            local_sequence: 0,
        },
        parameters: [
            (
                key("inventory:field/destination"),
                ComponentValue::Entity(destination),
            ),
            (key("inventory:field/source_slot"), ComponentValue::U32(0)),
            (
                key("inventory:field/destination_slot"),
                ComponentValue::U32(0),
            ),
            (key("inventory:field/count"), ComponentValue::U32(1)),
        ]
        .into_iter()
        .collect(),
    }
}

fn total(world: &WorldAuthority, ids: &[EntityId], catalog: &ItemCatalog) -> Result<u64> {
    let mut total = 0;
    for id in ids {
        let handle = world
            .entities()
            .handle_of(*id)
            .ok_or("missing inventory entity")?;
        let fields = world
            .entities()
            .get(handle)
            .ok_or("stale inventory handle")?
            .components
            .component(&torse_inventory::schema())
            .ok_or("missing inventory component")?;
        let inventory =
            Inventory::from_fields(fields, catalog).map_err(|e| format!("inventory: {e:?}"))?;
        total += inventory
            .slots()
            .iter()
            .flatten()
            .map(|stack| u64::from(stack.count))
            .sum::<u64>();
    }
    Ok(total)
}

fn run() -> Result<()> {
    let mut pack = m0_fixture_pack();
    block_actions::register_content(&mut pack);
    torse_inventory::register_content(&mut pack);
    let content = compile(pack).map_err(|e| format!("content: {e:?}"))?;
    let mut schemas = ComponentSchemas::build(&content);
    let catalog =
        ItemCatalog::new([(key("base:stone"), 64)]).map_err(|e| format!("catalog: {e:?}"))?;
    catalog.bind_schemas(&mut schemas)?;
    let mut descriptors = block_actions::descriptors();
    descriptors.extend(torse_inventory::descriptors());
    let mut handlers = block_actions::handlers();
    handlers.extend(torse_inventory::handlers(catalog.clone()));
    let service = ActionService::bind(descriptors, handlers, &schemas)?;
    let mut world = WorldAuthority::new(
        WorldId(37),
        BlockStateRegistry::build(&content)?,
        schemas.clone(),
        16,
        WorldBounds::new(0, 32)?,
        Compatibility {
            authority_schema: 3,
            rules_digest: content.authority_digest,
            content_digest: content.presentation_digest,
        },
    )?;
    world.store_mut().request(SectionPos::ZERO)?;
    world
        .store_mut()
        .install(SectionPos::ZERO, Section::empty(16)?)?;
    world.store_mut().finish_load(SectionPos::ZERO)?;
    let mut ids = Vec::new();
    for index in 0..3 {
        let handle = world
            .entities_mut()
            .spawn(Position::new(2.0 + index as f64, 2.0, 2.0))?;
        ids.push(world.entities().get(handle).ok_or("spawn missing")?.id);
        let mut inventory = Inventory::new(2).map_err(|e| format!("capacity: {e:?}"))?;
        if index == 0 {
            inventory
                .set(0, Some(ItemStack::new(key("base:stone"), 1)), &catalog)
                .map_err(|e| format!("initial stack: {e:?}"))?;
        }
        world
            .entities_mut()
            .components_mut(handle)
            .ok_or("components missing")?
            .insert(&schemas, &torse_inventory::schema(), inventory.fields())?;
    }
    let pos = BlockPos::new(8, 2, 2);
    let actor = ids.iter().fold(
        ActorContext::new(key("base:scenario_actor"))
            .permission(key("inventory:permission/transfer"))
            .permission(key("world:permission/edit"))
            .target(ActionTarget::Block(pos)),
        |actor, id| actor.target(ActionTarget::Entity(*id)),
    );
    let baseline = world.capture_snapshot(meta(&world))?;
    let mut first = None;
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
                .map_err(|e| format!("command: {e:?}"))?;
        }
        let ActionOutput::Mutation(tx) = service
            .plan(&world, &actor, Phase::Interaction, &req)
            .map_err(|e| format!("action: {e:?}"))?
        else {
            return Err("expected mutation plan".into());
        };
        if let Some(first) = &first {
            check(first == &tx, "adapters planned different transactions")?;
        } else {
            first = Some(tx);
        }
    }
    let first = first.ok_or("missing plan")?;
    let discarded = world
        .prepare(&first)
        .map_err(|e| format!("prepare: {e:?}"))?;
    drop(discarded);
    check(
        world.capture_snapshot(meta(&world))? == baseline,
        "discard changed authority",
    )?;
    let ActionOutput::Mutation(second) = service
        .plan(
            &world,
            &actor,
            Phase::Interaction,
            &request(ids[0], ids[2], 2),
        )
        .map_err(|e| format!("second action: {e:?}"))?
    else {
        return Err("expected second mutation".into());
    };
    let prepared = world
        .prepare(&first)
        .map_err(|e| format!("prepare: {e:?}"))?;
    let mut tail = vec![changes_from_prepared(world.registry(), &prepared)?];
    world.publish(prepared)?;
    check(
        matches!(
            world.commit(&second)?.outcome,
            TransactionOutcome::Rejected(torse_types::RejectCode::Conflict)
        ),
        "both machines spent the same input",
    )?;
    check(
        total(&world, &ids, &catalog)? == 1 && world.seq().0 == 1,
        "item total or sequence changed",
    )?;
    let mut block = request(ids[0], ids[1], 3);
    block.action = key("world:action/place");
    block.api_version = 1;
    block.target = ActionTarget::Block(pos);
    block.parameters = [(
        key("world:field/block"),
        ComponentValue::Content(key("base:stone")),
    )]
    .into_iter()
    .collect();
    let ActionOutput::Mutation(tx) = service
        .plan(&world, &actor, Phase::Interaction, &block)
        .map_err(|e| format!("place: {e:?}"))?
    else {
        return Err("expected place plan".into());
    };
    let prepared = world
        .prepare(&tx)
        .map_err(|e| format!("place prepare: {e:?}"))?;
    tail.push(changes_from_prepared(world.registry(), &prepared)?);
    world.publish(prepared)?;
    let mut recovery = world.begin_restore();
    recovery.restore_snapshot(&baseline)?;
    for record in tail {
        recovery.replay(&record)?;
    }
    let mut recovered = recovery.finish()?;
    check(
        recovered.capture_snapshot(meta(&recovered))? == world.capture_snapshot(meta(&world))?,
        "component and terrain replay differs",
    )?;
    block.action = key("world:action/break");
    block.parameters.clear();
    block.transaction = TransactionId(4);
    let ActionOutput::Mutation(tx) = service
        .plan(&recovered, &actor, Phase::Interaction, &block)
        .map_err(|e| format!("break: {e:?}"))?
    else {
        return Err("expected break plan".into());
    };
    recovered.commit(&tx)?;
    check(
        recovered.seq().0 == 3 && total(&recovered, &ids, &catalog)? == 1,
        "continuation failed",
    )?;
    println!(
        "adapters_equal=true\ndiscarded_preparations=1\nmachine_commits=1\n\
         machine_conflicts=1\nitem_total=1\nrecovered_seq=2\ncontinued_seq=3\nreplayed_equal=true"
    );
    Ok(())
}

fn main() -> Result<()> {
    run()
}

#[cfg(test)]
mod tests {
    #[test]
    fn competition_scenario_holds_acceptance_properties() {
        super::run().expect("competition scenario");
    }
}
