//! Server-authoritative player movement domain, per the movement contract.
//!
//! [`plan_step`] freezes persistent server movement; [`predict_step`] uses the
//! same fixed integration against read-only terrain. [`walker_step`] remains
//! the immediate local-world adapter. Input sequencing and reconciliation never
//! grant authority to a network mirror.

use std::collections::BTreeMap;

use torse_types::{
    EntityHandle, EntityId, RejectCode, SessionId, TransactionId, TransactionOutcome,
};
use torse_world::Position;

use crate::coordinator::CommitFault;
use crate::{OrderingKey, Phase, Transaction, WorldAuthority};

/// Network walking always integrates exactly one 50 ms simulation tick.
pub const NETWORK_STEP_SECONDS: f64 = 0.05;

fn integrate(motion: torse_world::PlayerMotion, input: MoveInput) -> ([f64; 3], i64) {
    let speed = if motion.grounded && input.jump {
        8_400_000
    } else {
        (motion.vertical_micros - 1_400_000).max(-torse_world::PlayerMotion::MAX_SPEED)
    };
    let wish = input.direction();
    (
        [
            wish[0] * WALK_SPEED * NETWORK_STEP_SECONDS,
            speed as f64 / 1_000_000.0 * NETWORK_STEP_SECONDS,
            wish[2] * WALK_SPEED * NETWORK_STEP_SECONDS,
        ],
        speed,
    )
}
fn settled_motion(
    previous: Position,
    settled: Position,
    delta: [f64; 3],
    speed: i64,
) -> torse_world::PlayerMotion {
    let difference = settled.y - (previous.y + delta[1]);
    torse_world::PlayerMotion {
        vertical_micros: if difference.abs() > 1e-9 { 0 } else { speed },
        grounded: difference > 1e-9,
    }
}

/// Predict against a read-only terrain mirror, keeping position and velocity detached.
pub fn predict_step(
    world: &WorldAuthority,
    position: Position,
    half: [f64; 3],
    motion: torse_world::PlayerMotion,
    input: MoveInput,
) -> Result<(Position, torse_world::PlayerMotion), RejectCode> {
    torse_world::PlayerMotion::from_fields(&motion.fields())
        .map_err(|_| RejectCode::InvalidData)?;
    let (delta, speed) = integrate(motion, input);
    let settled =
        crate::movement::predict_at(world.store(), world.registry(), position, half, delta)?;
    Ok((settled, settled_motion(position, settled, delta, speed)))
}

/// Freeze one authoritative step as an ordinary movement plus protected motion edit.
/// Collision observations and the body declaration become preconditions, so a changed
/// phase-start world cannot publish velocity calculated for a different displacement.
pub fn plan_step(
    world: &WorldAuthority,
    handle: EntityHandle,
    input: MoveInput,
    transaction: TransactionId,
    ordering: OrderingKey,
) -> Result<Transaction, RejectCode> {
    use torse_world::{ComponentEdit, EntityCollider, PlayerMotion, TerrainQuery};
    let entity = world
        .entities()
        .get(handle)
        .ok_or(RejectCode::MissingTarget)?;
    let motion = PlayerMotion::of(entity)
        .map_err(|_| RejectCode::InvalidData)?
        .ok_or(RejectCode::MissingTarget)?;
    let half = EntityCollider::of(entity)
        .map_err(|_| RejectCode::InvalidData)?
        .and_then(EntityCollider::half)
        .ok_or(RejectCode::InvalidData)?;
    let (delta, speed) = integrate(motion, input);
    let mut tx = Transaction::new(transaction, torse_world::collider_owner(), ordering);
    tx.move_entity(handle, entity.id, entity.position, delta, half);
    let resolved = crate::movement::prepare_movements(
        world.store(),
        world.entities(),
        world.registry(),
        &[],
        &tx.movements,
        crate::movement::MovementMode::Normal,
    )?;
    let settled = Position::from_bits(resolved.moves[0].position_bits);
    for (pos, query) in resolved.reads {
        match query {
            TerrainQuery::Ready(state) => {
                tx.read_block(pos, state);
            }
            TerrainQuery::Exterior => {}
            TerrainQuery::NotLoaded | TerrainQuery::OutOfBounds => {
                return Err(RejectCode::NotLoaded);
            }
        }
    }
    tx.components.push(ComponentEdit {
        id: entity.id,
        schema: torse_world::player_motion_schema(),
        previous: Some(motion.fields()),
        fields: Some(settled_motion(entity.position, settled, delta, speed).fields()),
    });
    let body = entity
        .components
        .component(&torse_world::collider_schema())
        .cloned();
    tx.components.push(ComponentEdit {
        id: entity.id,
        schema: torse_world::collider_schema(),
        previous: body.clone(),
        fields: body,
    });
    Ok(tx)
}

/// Walk speed in blocks per second.
pub const WALK_SPEED: f64 = 4.3;
/// Gravity in blocks per second squared.
pub const GRAVITY: f64 = 28.0;
/// Jump launch speed in blocks per second.
pub const JUMP_SPEED: f64 = 8.4;
/// Player collider half extents: 0.6 wide, 1.8 tall, centre at body middle.
pub const WALKER_HALF: [f64; 3] = [0.3, 0.9, 0.3];
/// Default reconciliation tolerance in blocks; beyond it the client snaps.
pub const RECONCILE_TOLERANCE: f64 = 0.05;

/// Client-side integration state for one walking entity.
#[derive(Clone, Copy, Debug)]
pub struct WalkerState {
    /// Runtime handle of the player entity.
    pub handle: EntityHandle,
    /// Velocity in blocks per second; vertical persists between ticks.
    pub velocity: [f64; 3],
    /// Whether the last step rested on ground or a ceiling stopped the rise.
    pub grounded: bool,
}

impl WalkerState {
    /// Bind integration state to a freshly spawned entity at rest.
    pub const fn new(handle: EntityHandle) -> Self {
        Self {
            handle,
            velocity: [0.0; 3],
            grounded: false,
        }
    }
}

/// Why one walker step could not run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StepError {
    /// The observed entity no longer exists.
    EntityGone,
    /// The request was rejected; the entity stays where it is.
    Rejected(RejectCode),
    /// Publication failed; the world stopped by invariant.
    Fault(CommitFault),
}

/// Integrate one walk tick and commit the displacement as a movement
/// transaction; the authority resolves terrain collision in `prepare`.
///
/// `wish` is a normalized horizontal direction, `jump` requests a jump while
/// grounded, and `source` names the settling system in the record. Rejections
/// (a stale observation, for instance) drop the tick and keep the entity at
/// its committed position.
pub fn walker_step(
    world: &mut WorldAuthority,
    state: &mut WalkerState,
    wish: [f64; 3],
    jump: bool,
    dt: f32,
    source: &torse_types::ContentKey,
    transaction: TransactionId,
) -> Result<(Position, bool), StepError> {
    let Some(entity) = world.entities().get(state.handle) else {
        return Err(StepError::EntityGone);
    };
    let (id, previous) = (entity.id, entity.position);
    let half = crate::body::of(entity, &[], WALKER_HALF)
        .map_err(StepError::Rejected)?
        .unwrap_or(WALKER_HALF);

    state.velocity[0] = wish[0] * WALK_SPEED;
    state.velocity[2] = wish[2] * WALK_SPEED;
    state.velocity[1] -= GRAVITY * f64::from(dt);
    if state.grounded && jump {
        state.velocity[1] = JUMP_SPEED;
    }
    let dt = f64::from(dt);
    let requested = previous.translated([
        state.velocity[0] * dt,
        state.velocity[1] * dt,
        state.velocity[2] * dt,
    ]);

    let mut tx = Transaction::new(
        transaction,
        source.clone(),
        OrderingKey {
            phase: Phase::Simulation,
            rule_priority: 0,
            stable_task_key: u64::try_from(transaction.0).unwrap_or(u64::MAX),
            local_sequence: 0,
        },
    );
    tx.move_entity(
        state.handle,
        id,
        previous,
        [
            requested.x - previous.x,
            requested.y - previous.y,
            requested.z - previous.z,
        ],
        half,
    );
    let result = world.commit(&tx);

    // A rejection with the entity embedded in terrain is the wedged state:
    // movement refuses colliders that already overlap. Try to escape to the
    // nearest free position; when no neighbour is free the walker stays put
    // by contract. The wedged check runs while the entity borrow is alive,
    // then the escape takes the world mutably.
    let wedged_now = match &result {
        Ok(outcome) => {
            matches!(outcome.outcome, TransactionOutcome::Rejected(_))
                && wedged(world, state.handle, id) == Ok(true)
        }
        Err(_) => false,
    };

    if wedged_now
        && let Ok(Some(escaped)) = unstuck(
            world,
            state,
            source,
            TransactionId(transaction.0.wrapping_add(1)),
        )
    {
        return Ok((escaped, false));
    }

    let Some(entity) = world.entities().get(state.handle) else {
        return Err(StepError::EntityGone);
    };
    match result {
        Ok(outcome) => {
            if let TransactionOutcome::Rejected(code) = outcome.outcome {
                // Drop this tick: the entity stays at its committed position.
                let _ = code;
                state.velocity[1] = 0.0;
                return Ok((entity.position, state.grounded));
            }
        }
        Err(fault) => return Err(StepError::Fault(fault)),
    }

    // Resting above the request means the descent was blocked: that is the
    // floor. Ending below the request means a ceiling stopped the rise.
    let settled = entity.position;
    if settled.y - requested.y > 1e-9 {
        state.grounded = true;
        state.velocity[1] = 0.0;
    } else if requested.y - settled.y > 1e-9 {
        state.velocity[1] = 0.0;
        state.grounded = false;
    } else {
        state.grounded = false;
    }
    Ok((settled, state.grounded))
}

/// Escape a wedged entity: relocate it to the nearest free position and
/// commit the move.
///
/// The start may overlap terrain -- that is the condition being escaped.
/// `None` means no free neighbour was reachable within the escape budget;
/// the entity stays where it is.
pub fn unstuck(
    world: &mut WorldAuthority,
    state: &mut WalkerState,
    source: &torse_types::ContentKey,
    transaction: TransactionId,
) -> Result<Option<Position>, StepError> {
    let Some(entity) = world.entities().get(state.handle) else {
        return Err(StepError::EntityGone);
    };
    let (id, previous) = (entity.id, entity.position);
    let half = crate::body::of(entity, &[], WALKER_HALF)
        .map_err(StepError::Rejected)?
        .unwrap_or(WALKER_HALF);
    let move_result = crate::movement::prepare_escape(
        world.store(),
        world.entities(),
        world.registry(),
        state.handle,
        id,
        half,
    );
    let Some(entity_move) = move_result.map_err(StepError::Rejected)? else {
        return Ok(None);
    };
    let target = Position::from_bits(entity_move.position_bits);
    let delta = [
        target.x - previous.x,
        target.y - previous.y,
        target.z - previous.z,
    ];
    let mut tx = Transaction::new(
        transaction,
        source.clone(),
        OrderingKey {
            phase: Phase::Simulation,
            rule_priority: 0,
            stable_task_key: u64::try_from(transaction.0).unwrap_or(u64::MAX),
            local_sequence: 0,
        },
    );
    tx.movements.push(crate::Movement {
        handle: state.handle,
        id,
        previous_bits: previous.to_bits(),
        delta_bits: delta.map(f64::to_bits),
        half_bits: half.map(f64::to_bits),
    });
    let result = world.commit_escape(&tx);
    let Some(entity) = world.entities().get(state.handle) else {
        return Err(StepError::EntityGone);
    };
    match result {
        Ok(committed) => {
            if !committed {
                return Ok(None);
            }
            state.velocity = [0.0; 3];
            state.grounded = false;
            Ok(Some(entity.position))
        }
        Err(fault) => Err(StepError::Fault(fault)),
    }
}

/// Whether the entity's collider currently overlaps terrain.
fn wedged(world: &WorldAuthority, handle: EntityHandle, id: EntityId) -> Result<bool, RejectCode> {
    let entity = world
        .entities()
        .get(handle)
        .ok_or(RejectCode::MissingTarget)?;
    if entity.id != id {
        return Err(RejectCode::MissingTarget);
    }
    let Some(half) = crate::body::of(entity, &[], WALKER_HALF)? else {
        return Ok(false);
    };
    match crate::movement::observe_body(world.store(), world.registry(), entity.position, half) {
        Ok(_) => Ok(false),
        Err(RejectCode::Conflict) => Ok(true),
        Err(error) => Err(error),
    }
}

/// One tick of aggregated movement intent.
///
/// The wish direction is quantized to unit axes, matching key input; the
/// settling side normalizes it before integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MoveInput {
    /// Session-scoped sequence, monotonic from one.
    pub seq: u64,
    /// Horizontal intent in world axes, each component -1, 0, or 1.
    pub wish: [i8; 3],
    /// Whether the jump key is held this tick.
    pub jump: bool,
}

impl MoveInput {
    /// Normalized world-space wish direction for [`walker_step`].
    pub fn direction(&self) -> [f64; 3] {
        // Horizontal only: vertical motion comes from gravity and jumps,
        // never from network input. A client sending a nonzero wish y gains
        // nothing.
        let raw = [f64::from(self.wish[0]), 0.0, f64::from(self.wish[2])];
        let length = (raw[0] * raw[0] + raw[2] * raw[2]).sqrt();
        if length == 0.0 {
            return [0.0; 3];
        }
        [raw[0] / length, 0.0, raw[2] / length]
    }
}

/// Bounded server-side buffer of pending movement inputs.
///
/// Sequence numbers are monotonic per session: duplicates and inputs at or
/// below the executed watermark are dropped on arrival, never re-executed.
/// A full buffer evicts the oldest pending input and raises the resync flag,
/// per the movement contract: the client replays its unconfirmed inputs
/// after the reset instead of the server guessing which intent to keep.
#[derive(Debug)]
pub struct MoveInputBuffer {
    capacity: usize,
    watermark: u64,
    pending: BTreeMap<u64, MoveInput>,
    resync_required: bool,
}

impl MoveInputBuffer {
    /// Buffer holding at most `capacity` unexecuted inputs.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            watermark: 0,
            pending: BTreeMap::new(),
            resync_required: false,
        }
    }

    /// Highest sequence already handed to execution.
    pub const fn watermark(&self) -> u64 {
        self.watermark
    }

    /// Queue one input; stale and duplicate sequences are dropped, and a full
    /// buffer evicts the oldest pending input and raises the resync flag.
    pub fn push(&mut self, input: MoveInput) {
        if self.capacity == 0 {
            self.resync_required = true;
            return;
        }
        if input.seq <= self.watermark || self.pending.contains_key(&input.seq) {
            return;
        }
        while self.pending.len() >= self.capacity {
            let oldest = *self.pending.keys().next().expect("non-empty map");
            self.pending.remove(&oldest);
            self.resync_required = true;
        }
        self.pending.insert(input.seq, input);
    }

    /// Whether an overflow evicted pending inputs since the last check.
    /// The host sends a resync demand (ResetRequired) when this is set.
    pub fn take_resync(&mut self) -> bool {
        std::mem::take(&mut self.resync_required)
    }

    /// Inspect the oldest input without advancing the admitted watermark.
    pub fn peek(&self) -> Option<MoveInput> {
        self.pending.first_key_value().map(|(_, input)| *input)
    }

    /// Take up to `limit` inputs in sequence order for this tick's
    /// settlement, advancing the watermark past them.
    pub fn settle(&mut self, limit: usize) -> Vec<MoveInput> {
        let taken: Vec<u64> = self.pending.keys().copied().take(limit).collect();
        taken
            .into_iter()
            .map(|seq| {
                self.watermark = seq;
                self.pending.remove(&seq).expect("key came from the map")
            })
            .collect()
    }
}

/// What a client should do with its prediction after an authoritative result.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Correction {
    /// Prediction is close enough: keep the local position and rebase future
    /// integration onto the authoritative one.
    Keep { base: Position },
    /// Diverged beyond the tolerance: jump to the authoritative position.
    Snap(Position),
}

/// Compare a predicted position against the authoritative one.
pub fn reconcile(predicted: Position, authoritative: Position, tolerance: f64) -> Correction {
    let drift = (predicted.x - authoritative.x).abs()
        + (predicted.y - authoritative.y).abs()
        + (predicted.z - authoritative.z).abs();
    if drift <= tolerance {
        Correction::Keep {
            base: authoritative,
        }
    } else {
        Correction::Snap(authoritative)
    }
}

/// Session-to-player-entity bindings: one each way, per the contract.
#[derive(Debug, Default)]
pub struct PlayerBindings {
    by_session: BTreeMap<u128, EntityId>,
    by_entity: BTreeMap<EntityId, u128>,
}

impl PlayerBindings {
    /// Bind a session to a player entity; both sides must be free.
    pub fn bind(&mut self, session: SessionId, entity: EntityId) -> Result<(), RejectCode> {
        if session.0 == 0 || entity.0 == 0 {
            return Err(RejectCode::InvalidData);
        }
        if self.by_session.contains_key(&session.0) || self.by_entity.contains_key(&entity) {
            return Err(RejectCode::Conflict);
        }
        self.by_session.insert(session.0, entity);
        self.by_entity.insert(entity, session.0);
        Ok(())
    }

    /// Drop a binding, if present; revocation and disconnect both land here.
    pub fn unbind(&mut self, session: SessionId) {
        if let Some(entity) = self.by_session.remove(&session.0) {
            self.by_entity.remove(&entity);
        }
    }

    /// The player entity bound to a session.
    pub fn entity_of(&self, session: SessionId) -> Option<EntityId> {
        self.by_session.get(&session.0).copied()
    }

    /// The session that owns a player entity.
    pub fn session_of(&self, entity: EntityId) -> Option<SessionId> {
        self.by_entity.get(&entity).copied().map(SessionId)
    }
}
