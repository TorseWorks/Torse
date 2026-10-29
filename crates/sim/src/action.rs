//! Frozen native action dispatch with a host-issued authorization context.
//!
//! Commands, UI, network adapters, and Mods supply the same typed request. The
//! origin label grants no authority. A host derives the actor and finite target
//! grants from an authenticated session or trusted task, then dispatches at the
//! declared phase. Mutation output is only a transaction plan, never a success
//! acknowledgment; journal reservation and durable confirmation belong to B6.

use std::collections::{BTreeMap, BTreeSet};

use torse_actions::{ActionDescriptor, ActionKind, ActionRegistry, validate_actions};
use torse_types::{
    BlockPos, ContentKey, EntityHandle, EntityId, RejectCode, SectionPos, TransactionId,
};
use torse_world::{
    BlockStateId, ComponentFields, ComponentSchemas, ComponentSet, ComponentValue, Entity,
    Position, TerrainQuery, component_size,
};
use torse_world_services::Aabb;

use crate::{MAX_GROUP_MEMBERS, OrderingKey, Phase, Transaction, TransactionGroup, WorldAuthority};

/// Add one optional native module's schema and action declarations to a pack.
///
/// Content compilation remains responsible for duplicate identities and missing
/// references. Adding this metadata does not bind or start native handlers.
pub fn register_action_module(
    pack: &mut torse_content::ContentPack,
    owner: ContentKey,
    state: ContentKey,
    components: Vec<torse_content::ComponentSchema>,
    actions: Vec<ActionDescriptor>,
    capabilities: Vec<torse_content::CapabilityDescriptor>,
) {
    use torse_content::{ModuleDescriptor, ModuleSide, SourceRef, Sourced, StateSchema};
    let source = SourceRef::new("native/module", 0);
    pack.modules.push(Sourced::new(
        source.clone(),
        ModuleDescriptor {
            id: owner,
            module_version: 1,
            framework_api: 1,
            side: ModuleSide::Server,
            required_modules: Vec::new(),
            optional_modules: Vec::new(),
            provided_services: Vec::new(),
            required_services: Vec::new(),
            registered_schemas: vec![state.clone()],
            registered_capabilities: capabilities.iter().map(|c| c.id.clone()).collect(),
            registered_actions: actions.iter().map(|a| a.id.clone()).collect(),
            registered_systems: Vec::new(),
            requested_permissions: Vec::new(),
            configuration_schema: None,
            lifecycle_binding: None,
        },
    ));
    pack.state_schemas.push(Sourced::new(
        source.clone(),
        StateSchema {
            id: state,
            schema_version: 1,
            finite_fields: Vec::new(),
            component_schemas: components,
        },
    ));
    pack.actions
        .extend(actions.into_iter().map(|a| Sourced::new(source.clone(), a)));
    pack.capabilities.extend(
        capabilities
            .into_iter()
            .map(|c| Sourced::new(source.clone(), c)),
    );
}

/// Input adapter metadata, deliberately independent of authorization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionOrigin {
    Console,
    Ui,
    Network,
    Mod,
}

/// Primary target named by one request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionTarget {
    Block(BlockPos),
    Entity(EntityId),
}

/// Upper bound on section grants one actor may carry at registration.
///
/// A resident 5 by 5 column scope reaches 600 grants once a world fills 24
/// vertical sections per column, so the bound must cover full-height worlds.
/// It stays a host mistake backstop rather than a scope policy; the replica
/// subscription budget remains independent.
pub const MAX_ACTOR_SECTIONS: usize = 1024;

/// Identity and finite access grants established by the trusted host.
///
/// Never deserialize this object from client assertions. A grant can represent
/// a reach/visibility check, container ownership, or a scheduled machine's ports.
/// Handlers receive read-only world access; all output targets are checked again.
#[derive(Clone, Debug)]
pub struct ActorContext {
    principal: ContentKey,
    permissions: BTreeSet<ContentKey>,
    targets: BTreeSet<TargetKey>,
    sections: BTreeSet<SectionPos>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
enum TargetKey {
    Block(BlockPos),
    Entity(EntityId),
}

impl ActorContext {
    pub fn accounted_bytes(&self) -> u64 {
        4096u64.saturating_add(
            (self.permissions.len() + self.targets.len() + self.sections.len()) as u64 * 4096,
        )
    }
    /// Create a host identity with no permissions or object access.
    pub fn new(principal: ContentKey) -> Self {
        Self {
            principal,
            permissions: BTreeSet::new(),
            targets: BTreeSet::new(),
            sections: BTreeSet::new(),
        }
    }
    /// Add an independently verified permission.
    pub fn permission(mut self, key: ContentKey) -> Self {
        self.permissions.insert(key);
        self
    }
    /// Add one independently verified target grant.
    pub fn target(mut self, target: ActionTarget) -> Self {
        self.targets.insert(match target {
            ActionTarget::Block(pos) => TargetKey::Block(pos),
            ActionTarget::Entity(id) => TargetKey::Entity(id),
        });
        self
    }
    /// Grant every cell of one section. Hosts use this for scope-wide access
    /// such as editing a subscribed area; it never widens entity access.
    /// Registration rejects actors carrying more than [`MAX_ACTOR_SECTIONS`]
    /// grants; the builder itself stays infallible.
    pub fn section(mut self, pos: SectionPos) -> Self {
        self.sections.insert(pos);
        self
    }
    /// How many section grants this actor carries.
    pub fn section_count(&self) -> usize {
        self.sections.len()
    }
    /// Authenticated caller identity, available for module-specific policies.
    pub fn principal(&self) -> &ContentKey {
        &self.principal
    }
    /// Test a host-issued permission for a native module operation.
    pub fn permits(&self, permission: &ContentKey) -> bool {
        self.permissions.contains(permission)
    }
    /// Restrict a module grant to a caller without delegating the caller's
    /// identity or adding permissions. Exact cells covered by the other side's
    /// section grant remain usable; entity access always requires both grants.
    pub fn intersect(&self, caller: &Self, side: u32) -> Self {
        let mut restricted = Self::new(self.principal.clone());
        restricted.permissions = self
            .permissions
            .intersection(&caller.permissions)
            .cloned()
            .collect();
        restricted.sections = self
            .sections
            .intersection(&caller.sections)
            .copied()
            .collect();
        for target in self.targets.iter().chain(&caller.targets) {
            let allowed = match *target {
                TargetKey::Block(pos) => {
                    self.allows_block(pos, side) && caller.allows_block(pos, side)
                }
                TargetKey::Entity(id) => {
                    self.allows(ActionTarget::Entity(id)) && caller.allows(ActionTarget::Entity(id))
                }
            };
            if allowed {
                restricted.targets.insert(*target);
            }
        }
        restricted
    }
    /// Open the same restricted view for a scheduled native operation.
    pub fn view<'a>(&'a self, world: &'a WorldAuthority) -> ActionView<'a> {
        ActionView {
            world,
            actor: self,
            caller: None,
        }
    }
    /// Whether the host granted access to this exact target. Section grants
    /// are not consulted; use [`ActorContext::allows_block`] for cells.
    pub fn allows(&self, target: ActionTarget) -> bool {
        self.targets.contains(&match target {
            ActionTarget::Block(pos) => TargetKey::Block(pos),
            ActionTarget::Entity(id) => TargetKey::Entity(id),
        })
    }
    /// Whether a block target is granted by exact cell or a covering
    /// section grant. `side` is the world's section side.
    pub fn allows_block(&self, pos: BlockPos, side: u32) -> bool {
        if self.targets.contains(&TargetKey::Block(pos)) {
            return true;
        }
        match pos.to_section(side) {
            Ok((section, _)) => self.sections.contains(&section),
            Err(_) => false,
        }
    }
}

/// Typed request shared by all adapters; actor authority is supplied separately.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionRequest {
    pub action: ContentKey,
    pub api_version: u32,
    pub origin: ActionOrigin,
    pub target: ActionTarget,
    pub parameters: ComponentFields,
    pub transaction: TransactionId,
    pub ordering: OrderingKey,
}

/// A subordinate action. Identity, origin and arbitration order are inherited
/// from the outer request; callers cannot supply a transaction source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionInvocation {
    pub action: ContentKey,
    pub api_version: u32,
    pub target: ActionTarget,
    pub parameters: ComponentFields,
}

/// Maximum nested dispatch depth and total invocations in one request.
pub const MAX_ACTION_DEPTH: usize = 4;
pub const MAX_ACTION_INVOCATIONS: usize = 16;

/// Explicit host permission for creating ordinary entities at granted cells.
pub fn spawn_permission() -> ContentKey {
    ContentKey::parse("world:permission/spawn").expect("static permission")
}

/// Explicit host permission for removing individually granted ordinary entities.
pub fn despawn_permission() -> ContentKey {
    ContentKey::parse("world:permission/despawn").expect("static permission")
}

/// Explicit host permission for relocating one granted entity.
///
/// Inventory, visibility, and the caller's module name do not grant it.
pub fn relocate_permission() -> ContentKey {
    ContentKey::parse("world:permission/relocate").expect("static permission")
}

/// Source recorded on a relocation plan so another module can share the group.
pub fn relocation_source() -> ContentKey {
    ContentKey::parse("world:module/relocation").expect("static source")
}

/// A bounded query result or an unpublished mutation plan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionOutput {
    Query(ComponentFields),
    Mutation(Box<Transaction>),
    /// Native composition requests. Every child passes normal dispatch checks
    /// using the same actor; all children observe the same authoritative state.
    Invoke(Vec<ActionInvocation>),
    /// Normalized dispatcher output. Native handlers cannot return this variant:
    /// only dispatch may combine separately authorized owner-authored plans.
    Atomic(Box<TransactionGroup>),
}

/// Read-only authority access restricted to host-granted objects.
///
/// Content definitions are public configuration; terrain and component reads
/// pass through target grants. No raw store or mutable world reference escapes.
pub struct ActionView<'a> {
    world: &'a WorldAuthority,
    actor: &'a ActorContext,
    caller: Option<&'a ContentKey>,
}

impl ActionView<'_> {
    /// Immediate parent established by dispatch, absent for an external request
    /// or a scheduled native view. A child that produces external resources
    /// must restrict its caller to the trusted composition that removes them.
    /// This provenance never grants permissions or target access by itself.
    pub fn caller_action(&self) -> Option<&ContentKey> {
        self.caller
    }
    pub fn actor(&self) -> &ActorContext {
        self.actor
    }
    pub fn terrain(&self, pos: BlockPos) -> Result<TerrainQuery, RejectCode> {
        let side = self.world.store().side();
        if !self.actor.allows_block(pos, side) {
            return Err(RejectCode::PermissionDenied);
        }
        Ok(self.world.store().query(pos))
    }
    /// Plan an unpublished body transition authored by the physics module.
    ///
    /// `expected` must match the entity's current collider. Enabling or resizing
    /// a body that overlaps terrain is rejected here; this call does not move
    /// the entity. A pose change is a separate transaction the caller composes.
    /// Permission and the entity grant must both already be on this restricted
    /// view. Inventory, visibility, and the caller's name are not consulted.
    pub fn plan_collider_transition(
        &self,
        entity: EntityId,
        expected: torse_world::EntityCollider,
        requested: torse_world::EntityCollider,
        transaction: TransactionId,
        ordering: OrderingKey,
    ) -> Result<Transaction, RejectCode> {
        crate::collider_plan::plan(
            self.world,
            self.actor,
            entity,
            expected,
            requested,
            transaction,
            ordering,
        )
    }

    /// Plan an exact move that keeps the entity identity.
    ///
    /// The result is not published. Ordinary movement limits are unchanged.
    /// Section grants do not cover the entity.
    pub fn plan_relocation(
        &self,
        entity: EntityId,
        expected: Position,
        destination: Position,
        policy: crate::DestinationPolicy,
        transaction: TransactionId,
        ordering: OrderingKey,
    ) -> Result<Transaction, RejectCode> {
        crate::relocation::plan(
            self.world,
            self.actor,
            entity,
            expected,
            destination,
            policy,
            transaction,
            ordering,
        )
    }
    pub fn component(
        &self,
        id: EntityId,
        schema: &ContentKey,
    ) -> Result<&ComponentFields, RejectCode> {
        if !self.actor.allows(ActionTarget::Entity(id)) {
            return Err(RejectCode::PermissionDenied);
        }
        let handle = self
            .world
            .entities()
            .handle_of(id)
            .ok_or(RejectCode::MissingTarget)?;
        self.world
            .entities()
            .get(handle)
            .and_then(|entity| entity.components.component(schema))
            .ok_or(RejectCode::MissingTarget)
    }
    /// Observe a granted entity and its current runtime generation. Lifecycle
    /// planners capture both its identity and full state as a removal condition.
    pub fn entity(&self, id: EntityId) -> Result<(EntityHandle, &Entity), RejectCode> {
        if !self.actor.allows(ActionTarget::Entity(id)) {
            return Err(RejectCode::PermissionDenied);
        }
        let handle = self
            .world
            .entities()
            .handle_of(id)
            .ok_or(RejectCode::MissingTarget)?;
        let entity = self
            .world
            .entities()
            .get(handle)
            .ok_or(RejectCode::MissingTarget)?;
        Ok((handle, entity))
    }
    pub fn default_state(&self, block: &ContentKey) -> Option<BlockStateId> {
        self.world.registry().default_state(block)
    }

    /// Resolve the state a placement produces for one entry face.
    ///
    /// Blocks whose state schema carries a `base:field/half` field switch
    /// between their lower (0) and upper (1) halves. Blocks without the
    /// field resolve to their default state.
    pub fn state_with_half(&self, block: &ContentKey, half: usize) -> Option<BlockStateId> {
        let registry = self.world.registry();
        let state = registry.default_state(block)?;
        let values = registry.state_values(state)?;
        // Blocks without a half field always place their default state.
        let Some(half_index) = registry
            .fields_of(state)?
            .iter()
            .position(|field| field == &ContentKey::parse("base:field/half").expect("static key"))
        else {
            return Some(state);
        };
        let mut resolved = values.to_vec();
        resolved[half_index] = u16::try_from(half).unwrap_or(0);
        registry.state_of(block, &resolved)
    }

    /// Whether placing `state` into `pos` would overlap an entity's collider.
    ///
    /// Burying a walker wedges it: movement refuses colliders that already
    /// overlap terrain, so the placement must refuse first. The cell's
    /// collision shape for `state` is tested against every entity collider.
    pub fn cell_blockers(&self, pos: BlockPos, state: BlockStateId) -> Result<bool, RejectCode> {
        if state.is_empty() {
            // Removing a block never buries anyone.
            return Ok(false);
        }
        let registry = self.world.registry();
        if registry.definition(state).is_none() {
            return Err(RejectCode::InvalidData);
        }
        let Some(shape) = registry.collision_shape(state) else {
            // No collision shape: air-like, can never bury anyone.
            return Ok(false);
        };
        let cell_boxes = crate::movement::boxes(&shape);
        let overlapping = self.world.entities().iter().any(|(_, entity)| {
            let half = match crate::body::of(entity, &[], crate::player::WALKER_HALF) {
                Ok(Some(half)) => half,
                Ok(None) => return false,
                Err(_) => return true,
            };
            let collider = Aabb::from_center(
                [entity.position.x, entity.position.y, entity.position.z],
                half,
            );
            cell_boxes
                .iter()
                .any(|local| local.at(pos).overlaps(&collider))
        });
        Ok(overlapping)
    }
    /// State written when an identical `block` is placed into a cell that
    /// already holds it, or `None` when the occupied cell must stay rejected.
    ///
    /// Completion is content-defined: the held block names the block its
    /// remaining volume turns into, which is how two half slabs become one
    /// full block.
    pub fn completion_state(
        &self,
        block: &ContentKey,
        current: BlockStateId,
    ) -> Option<BlockStateId> {
        let definition = self.world.registry().definition(current)?;
        if &definition.id != block {
            return None;
        }
        let target = definition.interaction_profile.completes_to.as_ref()?;
        self.world.registry().default_state(target)
    }
}

/// Native handlers may observe authority but cannot mutate it through this API.
///
/// Implementations must enforce module rules and check every secondary read
/// against actor grants. The dispatcher also checks every emitted write target.
pub trait ActionHandler {
    fn plan(
        &self,
        view: &ActionView<'_>,
        request: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode>;
}

/// Explicit implementation binding checked before a service becomes usable.
pub struct NativeHandler {
    pub handler: ContentKey,
    pub owner: ContentKey,
    pub implementation: Box<dyn ActionHandler>,
}

/// Startup failure; no partially bound service is returned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionBindError(pub String);

impl std::fmt::Display for ActionBindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ActionBindError {}

/// Immutable action catalog and native implementations selected by the host.
pub struct ActionService {
    registry: ActionRegistry,
    handlers: BTreeMap<ContentKey, NativeHandler>,
}

impl ActionService {
    /// Declared execution phase; lookup does not authorize a caller.
    pub fn phase(&self, action: &ContentKey) -> Option<Phase> {
        self.registry
            .actions()
            .get(action)
            .and_then(|a| Phase::from_index(a.phase.index() as u16))
    }
    /// Validate aliases, schemas, ownership, and exact handler coverage at startup.
    pub fn bind(
        actions: Vec<ActionDescriptor>,
        handlers: Vec<NativeHandler>,
        schemas: &ComponentSchemas,
    ) -> Result<Self, ActionBindError> {
        let registry =
            validate_actions(actions).map_err(|errors| ActionBindError(format!("{errors:?}")))?;
        let mut bound = BTreeMap::new();
        for handler in handlers {
            if bound.insert(handler.handler.clone(), handler).is_some() {
                return Err(ActionBindError("duplicate native handler".to_owned()));
            }
        }
        let mut used = BTreeSet::new();
        for action in registry.actions().values() {
            let handler = bound
                .get(&action.handler.handler)
                .ok_or_else(|| ActionBindError(format!("missing handler for {}", action.id)))?;
            if handler.owner != action.handler.owner_module
                || schemas.get(&action.parameter_schema).is_none()
                || schemas.get(&action.result_schema).is_none()
            {
                return Err(ActionBindError(format!(
                    "invalid binding for {}",
                    action.id
                )));
            }
            used.insert(action.handler.handler.clone());
        }
        if used.len() != bound.len() {
            return Err(ActionBindError("unreferenced native handler".to_owned()));
        }
        Ok(Self {
            registry,
            handlers: bound,
        })
    }

    /// Resolve a namespaced command into the same request consumed by other adapters.
    ///
    /// This method accepts already typed parameters. Shell execution, free-form
    /// text parsing, remote transport, and admission queues are outside A6.
    pub fn command(
        &self,
        command: &str,
        mut request: ActionRequest,
    ) -> Result<ActionRequest, RejectCode> {
        let descriptor = self
            .registry
            .command(command)
            .ok_or(RejectCode::MissingTarget)?;
        request.action = descriptor.id.clone();
        request.api_version = descriptor.api_version;
        request.origin = ActionOrigin::Console;
        Ok(request)
    }

    /// Authorize, validate, and plan an action without changing authoritative state.
    pub fn plan(
        &self,
        world: &WorldAuthority,
        actor: &ActorContext,
        phase: Phase,
        request: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode> {
        self.dispatch(world, actor, phase, request, &mut Vec::new(), &mut 0)
    }

    fn dispatch(
        &self,
        world: &WorldAuthority,
        actor: &ActorContext,
        phase: Phase,
        request: &ActionRequest,
        path: &mut Vec<ContentKey>,
        calls: &mut usize,
    ) -> Result<ActionOutput, RejectCode> {
        *calls += 1;
        if path.len() >= MAX_ACTION_DEPTH || *calls > MAX_ACTION_INVOCATIONS {
            return Err(RejectCode::QuotaExceeded);
        }
        let action = self
            .registry
            .actions()
            .get(&request.action)
            .ok_or(RejectCode::MissingTarget)?;
        if action.api_version != request.api_version
            || action.phase.index() as usize != phase.index()
            || request.ordering.phase != phase
        {
            return Err(RejectCode::InvalidData);
        }
        let side = world.store().side();
        let target_allowed = match request.target {
            ActionTarget::Block(pos) => actor.allows_block(pos, side),
            ActionTarget::Entity(id) => actor.allows(ActionTarget::Entity(id)),
        };
        if !target_allowed
            || action
                .permission
                .permission
                .as_ref()
                .is_some_and(|permission| !actor.permissions.contains(permission))
        {
            return Err(RejectCode::PermissionDenied);
        }
        if !action.target_constraints.is_empty() {
            let matches = match request.target {
                ActionTarget::Block(pos) => world
                    .store()
                    .query(pos)
                    .ready()
                    .and_then(|state| world.registry().definition(state))
                    .is_some_and(|definition| {
                        action.target_constraints.contains(&definition.state_schema)
                    }),
                ActionTarget::Entity(id) => world
                    .entities()
                    .handle_of(id)
                    .and_then(|handle| world.entities().get(handle))
                    .is_some_and(|entity| {
                        action
                            .target_constraints
                            .iter()
                            .any(|schema| entity.components.component(schema).is_some())
                    }),
            };
            if !matches {
                return Err(RejectCode::MissingTarget);
            }
        }
        ComponentSet::validate(
            world.schemas(),
            &action.parameter_schema,
            &request.parameters,
        )
        .map_err(|_| RejectCode::InvalidData)?;
        let handler = self
            .handlers
            .get(&action.handler.handler)
            .expect("frozen handler exists");
        let output = handler.implementation.plan(
            &ActionView {
                world,
                actor,
                caller: path.last(),
            },
            request,
        )?;
        if let ActionOutput::Invoke(invocations) = output {
            if action.kind != ActionKind::Mutation {
                return Err(RejectCode::InvalidData);
            }
            if invocations.is_empty() || invocations.len() > MAX_GROUP_MEMBERS {
                return Err(RejectCode::QuotaExceeded);
            }
            let mut members = Vec::new();
            path.push(request.action.clone());
            for invocation in invocations {
                let child = ActionRequest {
                    action: invocation.action,
                    api_version: invocation.api_version,
                    origin: request.origin,
                    target: invocation.target,
                    parameters: invocation.parameters,
                    transaction: request.transaction,
                    ordering: request.ordering,
                };
                match self.dispatch(world, actor, phase, &child, path, calls)? {
                    ActionOutput::Mutation(tx) => members.push(*tx),
                    ActionOutput::Atomic(group) => members.extend(group.members),
                    _ => return Err(RejectCode::InvalidData),
                }
                if members.len() > MAX_GROUP_MEMBERS {
                    return Err(RejectCode::QuotaExceeded);
                }
            }
            path.pop();
            check_group_budget(&members, action.max_items, action.max_bytes)?;
            let group = TransactionGroup::new(request.transaction, request.ordering, members)?;
            world.prepare_group(&group).map_err(prepare_rejection)?;
            return Ok(ActionOutput::Atomic(Box::new(group)));
        }
        match &output {
            ActionOutput::Atomic(_) => return Err(RejectCode::PermissionDenied),
            ActionOutput::Query(fields) if action.kind == ActionKind::Query => {
                ComponentSet::validate(world.schemas(), &action.result_schema, fields)
                    .map_err(|_| RejectCode::InvalidData)?;
                fn items(value: &ComponentValue) -> usize {
                    1 + match value {
                        ComponentValue::List(values) => values.iter().map(items).sum(),
                        ComponentValue::Record(values) => values.values().map(items).sum(),
                        _ => 0,
                    }
                }
                if fields.values().map(items).sum::<usize>() > action.max_items as usize
                    || component_size(fields).map_err(|_| RejectCode::QuotaExceeded)?
                        > action.max_bytes
                {
                    return Err(RejectCode::QuotaExceeded);
                }
            }
            ActionOutput::Mutation(tx) if action.kind == ActionKind::Mutation => {
                if (!tx.spawns.is_empty() && !actor.permits(&spawn_permission()))
                    || (!tx.despawns.is_empty() && !actor.permits(&despawn_permission()))
                    || (!tx.relocations.is_empty() && !actor.permits(&relocate_permission()))
                {
                    return Err(RejectCode::PermissionDenied);
                }
                if tx
                    .relocations
                    .iter()
                    .any(|relocation| !actor.allows(ActionTarget::Entity(relocation.id)))
                {
                    return Err(RejectCode::PermissionDenied);
                }
                if tx.source != handler.owner
                    || tx.id != request.transaction
                    || tx.ordering != request.ordering
                {
                    return Err(RejectCode::InvalidData);
                }
                for spawn in &tx.spawns {
                    let pos = Position::from_bits(spawn.position_bits)
                        .checked_block()
                        .ok_or(RejectCode::InvalidData)?;
                    if !actor.allows_block(pos, side) {
                        return Err(RejectCode::PermissionDenied);
                    }
                }
                if tx
                    .despawns
                    .iter()
                    .any(|despawn| !actor.allows(ActionTarget::Entity(despawn.previous.id)))
                {
                    return Err(RejectCode::PermissionDenied);
                }
                if !tx.spawns.is_empty() || !tx.despawns.is_empty() {
                    check_group_budget(
                        std::slice::from_ref(tx.as_ref()),
                        action.max_items,
                        action.max_bytes,
                    )?;
                }
                let blocks = tx
                    .preconditions
                    .iter()
                    .map(|read| read.pos)
                    .chain(tx.mutations.iter().map(|write| write.pos));
                if blocks.into_iter().any(|pos| !actor.allows_block(pos, side))
                    || tx
                        .movements
                        .iter()
                        .any(|m| !actor.allows(ActionTarget::Entity(m.id)))
                    || tx
                        .components
                        .iter()
                        .any(|c| !actor.allows(ActionTarget::Entity(c.id)))
                {
                    return Err(RejectCode::PermissionDenied);
                }
                // Reuse preparation to reject malformed native output. The
                // returned plan must still be prepared again at commit time,
                // after earlier sorted transactions have changed the world.
                world.prepare(tx).map_err(prepare_rejection)?;
            }
            _ => return Err(RejectCode::InvalidData),
        }
        Ok(output)
    }
}

fn prepare_rejection(error: crate::PrepareError) -> RejectCode {
    match error {
        crate::PrepareError::Rejected(code) => code,
        crate::PrepareError::SequenceExhausted => RejectCode::QuotaExceeded,
    }
}

// Conservative in-memory plan accounting, independent of storage or wire
// encoding. Each composed descriptor caps the entire subtree it authorizes.
fn check_group_budget(
    members: &[Transaction],
    max_items: u32,
    max_bytes: u64,
) -> Result<(), RejectCode> {
    let mut items = 0u64;
    let mut bytes = 0u64;
    for tx in members {
        let entries = tx.preconditions.len()
            + tx.mutations.len()
            + tx.clearance.len()
            + tx.movements.len()
            + tx.components.len()
            + tx.spawns.len()
            + tx.despawns.len();
        items = items.saturating_add(entries as u64);
        bytes = bytes
            .saturating_add(128 + key_bytes(&tx.source))
            .saturating_add((entries as u64).saturating_mul(128));
        for edit in &tx.components {
            bytes = bytes.saturating_add(key_bytes(&edit.schema));
            for fields in edit.previous.iter().chain(edit.fields.iter()) {
                bytes = bytes
                    .saturating_add(component_size(fields).map_err(|_| RejectCode::QuotaExceeded)?);
            }
        }
        for components in tx.spawns.iter().map(|spawn| &spawn.components).chain(
            tx.despawns
                .iter()
                .map(|despawn| &despawn.previous.components),
        ) {
            items = items.saturating_add(components.len() as u64);
            for (schema, fields) in components {
                bytes = bytes
                    .saturating_add(key_bytes(schema))
                    .saturating_add(component_size(fields).map_err(|_| RejectCode::QuotaExceeded)?);
            }
        }
        if items > u64::from(max_items) || bytes > max_bytes {
            return Err(RejectCode::QuotaExceeded);
        }
    }
    Ok(())
}

fn key_bytes(key: &ContentKey) -> u64 {
    (key.namespace().len() + 1 + key.path().len()) as u64
}
