//! Reject forged native lifecycle output before any entity or allocator change.
use torse_server::bootstrap as base;
use torse_sim::action::*;
use torse_sim::{OrderingKey, Phase, Transaction, WorldAuthority};
use torse_types::*;
use torse_world::*;

struct Planned(Transaction);
impl ActionHandler for Planned {
    fn plan(&self, _: &ActionView<'_>, _: &ActionRequest) -> Result<ActionOutput, RejectCode> {
        Ok(ActionOutput::Mutation(Box::new(self.0.clone())))
    }
}
fn key(value: &str) -> ContentKey {
    base::key(value)
}
fn request() -> ActionRequest {
    ActionRequest {
        action: key("inventory:action/transfer"),
        api_version: 2,
        origin: ActionOrigin::Network,
        target: ActionTarget::Block(BlockPos::new(8, 1, 1)),
        parameters: ComponentFields::new(),
        transaction: TransactionId(1),
        ordering: OrderingKey {
            phase: Phase::Interaction,
            rule_priority: 0,
            stable_task_key: 0,
            local_sequence: 0,
        },
    }
}
fn actor() -> ActorContext {
    ActorContext::new(key("test:actor"))
        .permission(key("inventory:permission/transfer"))
        .permission(spawn_permission())
        .permission(despawn_permission())
        .target(request().target)
        .target(ActionTarget::Entity(EntityId(1)))
        .target(ActionTarget::Entity(EntityId(2)))
        .target(ActionTarget::Block(BlockPos::new(5, 2, 2)))
}
fn tx() -> Transaction {
    Transaction::new(
        request().transaction,
        torse_inventory::owner(),
        request().ordering,
    )
}
fn reject(
    world: &WorldAuthority,
    actor: &ActorContext,
    tx: Transaction,
    max_items: u32,
    expected: RejectCode,
) {
    let mut descriptor = torse_inventory::descriptors()[1].clone();
    descriptor.parameter_schema = key("world:schema/result");
    descriptor.target_constraints.clear();
    descriptor.max_items = max_items;
    descriptor.max_bytes = u64::MAX;
    let handler = NativeHandler {
        handler: descriptor.handler.handler.clone(),
        owner: torse_inventory::owner(),
        implementation: Box::new(Planned(tx)),
    };
    let service = ActionService::bind(vec![descriptor], vec![handler], world.schemas()).unwrap();
    let before: Vec<_> = world
        .entities()
        .iter()
        .map(|(_, entity)| EntityState::capture(entity))
        .collect();
    let seq = world.seq();
    let next = world.entities().next_entity_id();
    assert_eq!(
        service.plan(world, actor, Phase::Interaction, &request()),
        Err(expected)
    );
    assert_eq!(world.seq(), seq);
    assert_eq!(world.entities().next_entity_id(), next);
    assert_eq!(
        world
            .entities()
            .iter()
            .map(|(_, entity)| EntityState::capture(entity))
            .collect::<Vec<_>>(),
        before
    );
}

#[test]
fn spawn_requires_granted_cell_owner_safe_components_and_bounded_output() {
    let (world, _) = base::world().unwrap();
    let mut plan = tx();
    plan.spawn_entity(Position::new(5.0, 2.0, 2.0), Default::default());
    let ungranted = ActorContext::new(key("test:actor"))
        .permission(key("inventory:permission/transfer"))
        .permission(spawn_permission())
        .target(request().target);
    reject(
        &world,
        &ungranted,
        plan.clone(),
        64,
        RejectCode::PermissionDenied,
    );
    let mut foreign = plan.clone();
    foreign.spawns[0]
        .components
        .insert(key("world:schema/result"), ComponentFields::new());
    reject(&world, &actor(), foreign, 64, RejectCode::PermissionDenied);
    let mut protected = plan.clone();
    protected.spawns[0]
        .components
        .insert(block_instance_schema(), ComponentFields::new());
    reject(
        &world,
        &actor(),
        protected,
        64,
        RejectCode::PermissionDenied,
    );
    let mut invalid = plan.clone();
    invalid.spawns[0].position_bits[0] = f64::NAN.to_bits();
    reject(&world, &actor(), invalid, 64, RejectCode::InvalidData);
    let mut over_budget = plan.clone();
    over_budget.spawns.push(plan.spawns[0].clone());
    reject(&world, &actor(), over_budget, 1, RejectCode::QuotaExceeded);
    let mut global_limit = plan.clone();
    global_limit.spawns = vec![plan.spawns[0].clone(); MAX_ENTITY_LIFECYCLE + 1];
    reject(
        &world,
        &actor(),
        global_limit,
        u32::MAX,
        RejectCode::QuotaExceeded,
    );
}

#[test]
fn despawn_requires_exact_target_generation_identity_and_full_observation() {
    let (world, _) = base::world().unwrap();
    let handle = world.entities().handle_of(EntityId(1)).unwrap();
    let mut plan = tx();
    plan.despawn_entity(handle, world.entities().get(handle).unwrap());
    let ungranted = ActorContext::new(key("test:actor"))
        .permission(key("inventory:permission/transfer"))
        .permission(despawn_permission())
        .target(request().target);
    reject(
        &world,
        &ungranted,
        plan.clone(),
        64,
        RejectCode::PermissionDenied,
    );
    let mut wrong_handle = plan.clone();
    wrong_handle.despawns[0].handle = world.entities().handle_of(EntityId(2)).unwrap();
    reject(
        &world,
        &actor(),
        wrong_handle,
        64,
        RejectCode::MissingTarget,
    );
    let mut stale_generation = plan.clone();
    stale_generation.despawns[0].handle.generation += 1;
    reject(
        &world,
        &actor(),
        stale_generation,
        64,
        RejectCode::MissingTarget,
    );
    let mut wrong_id = plan.clone();
    wrong_id.despawns[0].previous.id = EntityId(2);
    reject(&world, &actor(), wrong_id, 64, RejectCode::MissingTarget);
    let mut stale = plan.clone();
    stale.despawns[0].previous.position_bits[0] = 2.5f64.to_bits();
    reject(&world, &actor(), stale, 64, RejectCode::Conflict);
    let mut foreign = plan;
    foreign.source = key("world:module");
    reject(&world, &actor(), foreign, 64, RejectCode::InvalidData);
}
