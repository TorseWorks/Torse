//! Optional demo adapter for durable physical item rules and scoped pickup.
use crate::execution::{DurableRule, PlanningContext, WorkPlan};
use torse_items::{Catalog, drops, key};
use torse_sim::{Phase, SystemSpec, WorldAuthority};
use torse_types::{Fault, Tick};

pub struct DropPhysics {
    catalog: Catalog,
}
impl DropPhysics {
    pub fn new(catalog: Catalog) -> Self {
        Self { catalog }
    }
}
impl DurableRule for DropPhysics {
    fn parallel_planner(&self) -> Option<&(dyn DurableRule + Sync)> {
        Some(self)
    }
    fn spec(&self) -> SystemSpec {
        SystemSpec {
            id: key("items:system/physics"),
            phase: Phase::Simulation,
            rule_priority: 10,
            before: vec![],
            after: vec![],
        }
    }
    fn plan(&self, world: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        Ok(drops::plans(world, &self.catalog, tick)
            .into_iter()
            .map(|group| WorkPlan {
                transaction: Some(group),
                schedule: vec![],
                cancel: vec![],
            })
            .collect())
    }
    fn plan_parallel(
        &self,
        world: &WorldAuthority,
        tick: Tick,
        context: &PlanningContext,
    ) -> Result<Vec<WorkPlan>, Fault> {
        let batch = drops::prepare(world, &self.catalog, tick);
        let jobs = batch.jobs();
        let budgets = jobs
            .iter()
            .map(|job| job.budget())
            .collect::<Option<Vec<_>>>();
        let results = if let Some(budgets) = budgets.filter(|_| jobs.len() >= 8) {
            let (output, scratch) = budgets
                .into_iter()
                .fold((0, 0), |(output, scratch), (o, s)| {
                    (output.max(o), scratch.max(s))
                });
            // Typed inputs and accumulated outputs survive all lane calculations.
            context.map(jobs.len(), output.saturating_mul(2), scratch, |i| {
                jobs[i].plan()
            })?
        } else {
            None
        };
        let results = results.unwrap_or_else(|| jobs.iter().map(drops::DropMotion::plan).collect());
        Ok(batch
            .finish(results)
            .map_err(|error| Fault::invalid_data(format!("drop planning results: {error:?}")))?
            .into_iter()
            .map(|group| WorkPlan {
                transaction: Some(group),
                schedule: vec![],
                cancel: vec![],
            })
            .collect())
    }
}

/// Extend exactly one pickup after checking the host's approved scope and reach.
pub(crate) fn pickup_actor(
    world: &WorldAuthority,
    actor: &torse_sim::action::ActorContext,
    scope: &torse_sim::replica::ReplicaScope,
    call: &torse_sim::durable::Call,
) -> Result<torse_sim::action::ActorContext, torse_types::RejectCode> {
    use torse_sim::action::ActionTarget;
    use torse_types::RejectCode;
    use torse_world::ComponentValue;
    if call.action != key("items:action/pickup") {
        return Ok(actor.clone());
    }
    if !actor.permits(&key("items:permission/pickup")) {
        return Err(RejectCode::PermissionDenied);
    }
    let ActionTarget::Entity(drop) = call.target else {
        return Err(RejectCode::InvalidData);
    };
    let Some(ComponentValue::Entity(inventory)) =
        call.parameters.get(&key("items:field/inventory"))
    else {
        return Err(RejectCode::InvalidData);
    };
    if !actor.allows(ActionTarget::Entity(*inventory)) {
        return Err(RejectCode::PermissionDenied);
    }
    let find = |id| {
        world
            .entities()
            .handle_of(id)
            .and_then(|h| world.entities().get(h))
    };
    let player = find(*inventory).ok_or(RejectCode::MissingTarget)?;
    let item = find(drop).ok_or(RejectCode::MissingTarget)?;
    let cell = item
        .position
        .checked_block()
        .ok_or(RejectCode::InvalidData)?;
    if item.components.component(&drops::schema()).is_none()
        || !item
            .position
            .section(world.store().side())
            .is_some_and(|p| scope.sections.contains(&p))
        || !actor.allows_block(cell, world.store().side())
        || !drops::near(player.position, item.position, drops::PICKUP_REACH)
    {
        return Err(RejectCode::PermissionDenied);
    }
    Ok(actor.clone().target(ActionTarget::Entity(drop)))
}
