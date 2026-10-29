//! Assembled authoritative world and its recovery path.
//!
//! This is the concrete A-side object a host drives: it owns the world store,
//! the entity store, the compiled registry, and the commit coordinator. Storage
//! hands it prepared changes, snapshots travel through the codec, and recovery
//! rebuilds an authority from a snapshot plus its journal tail.
//!
//! Recovery is a builder, not a world: nothing is usable until `finish`. A
//! snapshot that does not match the expected world, or a journal tail that does
//! not continue the snapshot position, fails the builder and no half-restored
//! authority escapes.

use torse_types::{
    CommitCursor, CommitSeq, Compatibility, ResumePoint, SnapshotMeta, TransactionOutcome,
    TransactionResult, WorldId,
};
use torse_world::{
    BlockStateRegistry, ChangeSet, ComponentSchemas, EntityStore, ExchangeError, RestoredWorld,
    SectionStore, Snapshot, WorldBounds,
};

use crate::coordinator::{
    CommitCoordinator, CommitFault, Committed, PrepareError, PreparedChange, Publication,
};
use crate::transaction::Transaction;

/// Failure produced while assembling an authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorityError {
    /// The section side or bounds are unsupported.
    World(ExchangeError),
    /// The entity store rejected its configuration.
    Store(String),
}

impl std::fmt::Display for AuthorityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::World(error) => write!(f, "{error}"),
            Self::Store(detail) => write!(f, "entity store: {detail}"),
        }
    }
}

impl std::error::Error for AuthorityError {}

/// Failure produced while restoring from a snapshot and journal tail.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryError {
    /// No snapshot has been applied yet.
    MissingSnapshot,
    /// The snapshot or change belongs to another world.
    WrongWorld {
        /// Expected world identity.
        expected: WorldId,
        /// Identified by the payload.
        actual: WorldId,
    },
    /// A change set does not continue the current position.
    UnexpectedPosition {
        /// Position the change set claims as previous.
        previous: CommitSeq,
        /// Position the recovery currently holds.
        expected: CommitSeq,
    },
    /// A change set jumps past the next commit position.
    NonConsecutive {
        /// Position the change set would assign.
        cursor: CommitSeq,
        /// Position expected after the current one.
        expected: CommitSeq,
    },
    /// The payload could not be applied.
    Payload(ExchangeError),
    /// The restored world cannot be assembled.
    Authority(AuthorityError),
    /// The commit sequence cannot advance.
    SequenceExhausted,
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSnapshot => write!(f, "recovery has no snapshot"),
            Self::WrongWorld { expected, actual } => {
                write!(
                    f,
                    "payload belongs to world {}, expected {}",
                    actual.0, expected.0
                )
            }
            Self::UnexpectedPosition { previous, expected } => write!(
                f,
                "change closes {} but recovery is at {}",
                previous.0, expected.0
            ),
            Self::NonConsecutive { cursor, expected } => {
                write!(f, "change assigns {} but {} is next", cursor.0, expected.0)
            }
            Self::Payload(error) => write!(f, "{error}"),
            Self::Authority(error) => write!(f, "{error}"),
            Self::SequenceExhausted => write!(f, "commit sequence exhausted"),
        }
    }
}

impl std::error::Error for RecoveryError {}

/// The assembled authoritative world.
pub struct WorldAuthority {
    world: WorldId,
    registry: BlockStateRegistry,
    schemas: ComponentSchemas,
    store: SectionStore,
    entities: EntityStore,
    coordinator: CommitCoordinator,
}

impl WorldAuthority {
    /// Resident logical world charge, separate from rebuildable projections and jobs.
    pub fn accounted_bytes(&self) -> u64 {
        self.store
            .accounted_bytes()
            .saturating_add(self.entities.accounted_bytes())
    }

    /// Additional reservation that covers changed section buffers and entity-store growth.
    /// Keep the old resident charge until publication, then reconcile to actual residency.
    pub fn growth_reservation(&self, prepared: &PreparedChange) -> u64 {
        let section = (self.store.side() as u64)
            .pow(3)
            .saturating_mul(8)
            .saturating_add(8192);
        let mut bytes = section.saturating_mul(prepared.touched().len() as u64);
        if !prepared.entity_moves().is_empty() || prepared.entity_lifecycle().is_some() {
            bytes = bytes.saturating_add(self.entities.accounted_bytes());
        }
        for edit in prepared.component_edits() {
            if let Some(fields) = &edit.fields {
                bytes = bytes.saturating_add(torse_world::component_memory_bytes(fields));
            }
        }
        if let Some(lifecycle) = prepared.entity_lifecycle() {
            bytes = bytes.saturating_add(
                (lifecycle.spawned.len() + lifecycle.despawned.len()) as u64 * 16384,
            );
            for entity in &lifecycle.spawned {
                for fields in entity.components.values() {
                    bytes = bytes.saturating_add(torse_world::component_memory_bytes(fields));
                }
            }
        }
        bytes
    }
    pub fn section_growth_reservation(&self, page: &Snapshot) -> u64 {
        let mut bytes = (self.store.side() as u64)
            .pow(3)
            .saturating_mul(8)
            .saturating_add(8192)
            .saturating_add(self.entities.accounted_bytes());
        for entity in &page.entities {
            bytes = bytes.saturating_add(16384);
            for component in &entity.components {
                bytes = bytes.saturating_add(component.accounted_bytes());
            }
        }
        bytes
    }
    pub fn capture_section(
        &self,
        pos: torse_types::SectionPos,
        budget: torse_types::OutputBudget,
    ) -> Result<Snapshot, torse_types::Fault> {
        Snapshot::capture_section_with_budget(
            crate::streaming::page_meta(self),
            &self.store,
            &self.entities,
            &self.registry,
            Some(&self.schemas),
            pos,
            budget,
        )
    }

    pub fn prepare_section(
        &self,
        page: &Snapshot,
        load: bool,
        budget: torse_types::OutputBudget,
    ) -> Result<crate::streaming::PreparedSection, torse_types::Fault> {
        crate::streaming::prepare(self, page, load, false, budget)
    }

    /// Prepare trusted generated cells and consecutive new identities in one publication.
    pub fn prepare_generated_section(
        &self,
        page: &Snapshot,
        budget: torse_types::OutputBudget,
    ) -> Result<crate::streaming::PreparedSection, torse_types::Fault> {
        crate::streaming::prepare(self, page, true, true, budget)
    }

    pub fn publish_section(
        &mut self,
        mut prepared: crate::streaming::PreparedSection,
    ) -> Result<Publication, CommitFault> {
        prepared.validate(self)?;
        let pos = prepared.position();
        let mut publication = self.coordinator.publish_with_entities(
            &mut self.store,
            &mut self.entities,
            prepared.barrier,
        )?;
        if prepared.load {
            self.store
                .request(pos)
                .map_err(|_| CommitFault::Invariant("section request failed"))?;
            self.store
                .install(pos, prepared.section.take().expect("validated page"))
                .map_err(|_| CommitFault::Invariant("section install failed"))?;
            self.store
                .finish_load(pos)
                .map_err(|_| CommitFault::Invariant("section activation failed"))?;
            for entity in prepared.entities {
                self.entities
                    .insert_restored(entity.id, entity.position, entity.components)
                    .map_err(|_| CommitFault::Invariant("section entity activation failed"))?;
            }
        } else {
            self.store
                .unload(pos)
                .map_err(|_| CommitFault::Invariant("section unload failed"))?;
            for entity in prepared.entities {
                let handle = self
                    .entities
                    .handle_of(entity.id)
                    .ok_or(CommitFault::Invariant("section entity vanished"))?;
                self.entities
                    .despawn(handle)
                    .ok_or(CommitFault::Invariant("section entity removal failed"))?;
            }
        }
        publication.touched.push((
            pos,
            self.store
                .section(pos)
                .map_or(torse_types::Revision::INITIAL, |s| s.revision()),
        ));
        Ok(publication)
    }

    pub fn prevalidate_components(
        &self,
        group: &crate::TransactionGroup,
    ) -> Result<crate::ComponentPrevalidation, PrepareError> {
        self.coordinator.prevalidate_components(
            &self.store,
            &self.entities,
            &self.registry,
            &self.schemas,
            group,
        )
    }
    pub fn refresh_components(
        &self,
        token: crate::ComponentPrevalidation,
    ) -> Result<PreparedChange, PrepareError> {
        self.coordinator.refresh_components(&self.entities, token)
    }

    /// Compute a pure movement on a borrowed world. It still requires serial
    /// refresh and the usual reservation/publication/durability path.
    pub fn prevalidate_movement(
        &self,
        group: &crate::TransactionGroup,
    ) -> Result<crate::MovementPrevalidation, PrepareError> {
        self.coordinator.prevalidate_movement(
            &self.store,
            &self.entities,
            &self.registry,
            &self.schemas,
            group,
        )
    }

    /// Refresh only when the phase, terrain, identity generations and exact
    /// source positions still match; otherwise callers must prepare normally.
    pub fn refresh_movement(
        &self,
        token: &crate::MovementPrevalidation,
    ) -> Result<PreparedChange, PrepareError> {
        self.coordinator
            .refresh_movement(&self.store, &self.entities, token)
    }
    /// Prepare an empty world change carrying an atomic host execution-state update.
    pub fn prepare_barrier(&self) -> Result<PreparedChange, CommitFault> {
        self.coordinator.prepare_barrier()
    }
    /// Prepare a durable whole-phase completion after the driver settles its work.
    pub fn prepare_phase_advance(&self) -> Result<PreparedChange, CommitFault> {
        self.coordinator.prepare_phase_advance()
    }

    /// Assemble an empty world.
    pub fn new(
        world: WorldId,
        registry: BlockStateRegistry,
        schemas: ComponentSchemas,
        side: u32,
        bounds: WorldBounds,
        compatibility: Compatibility,
    ) -> Result<Self, AuthorityError> {
        let (schemas, compatibility) = schemas
            .bind_compatibility(compatibility)
            .map_err(|error| AuthorityError::Store(error.to_string()))?;
        let mut store = SectionStore::new(side, bounds).map_err(|error| {
            AuthorityError::World(ExchangeError::Store {
                detail: error.to_string(),
            })
        })?;
        store.set_exterior(schemas.exterior_policy());
        let entities =
            EntityStore::new(side).map_err(|error| AuthorityError::Store(error.to_string()))?;
        let mut coordinator = CommitCoordinator::new(world);
        coordinator.set_compatibility(compatibility);
        Ok(Self {
            world,
            registry,
            schemas,
            store,
            entities,
            coordinator,
        })
    }

    /// World identity.
    pub const fn world(&self) -> WorldId {
        self.world
    }

    /// Last committed position.
    pub const fn seq(&self) -> CommitSeq {
        self.coordinator.seq()
    }

    /// Terrain store.
    pub const fn store(&self) -> &SectionStore {
        &self.store
    }

    /// Mutable terrain access for load and generation paths.
    pub fn store_mut(&mut self) -> &mut SectionStore {
        &mut self.store
    }

    /// Configure storage faces as known empty space or closed boundaries.
    ///
    /// The default opens the upper face and closes the lower face. Run before the first
    /// commit, and it changes the authority compatibility identity when the
    /// policy is not the default. Unloaded sections inside the store stay
    /// unloaded. Entity coordinates stay on the movement limit, not these bounds.
    pub fn set_exterior_policy(
        &mut self,
        policy: torse_world::ExteriorPolicy,
    ) -> Result<(), AuthorityError> {
        if self.coordinator.seq() != CommitSeq::INITIAL {
            return Err(AuthorityError::Store(
                "exterior policy is fixed after the first commit".to_owned(),
            ));
        }
        if self.store.exterior() != torse_world::ExteriorPolicy::default()
            || self.schemas.exterior_policy() != torse_world::ExteriorPolicy::default()
        {
            return Err(AuthorityError::Store(
                "exterior policy is already set".to_owned(),
            ));
        }
        let compatibility = self
            .schemas
            .bind_exterior_policy(policy)
            .map_err(|error| AuthorityError::Store(error.to_string()))?;
        self.store.set_exterior(policy);
        self.coordinator.set_compatibility(compatibility);
        Ok(())
    }

    /// Entity store.
    pub const fn entities(&self) -> &EntityStore {
        &self.entities
    }

    /// Mutable entity access for initialization and load paths.
    ///
    /// Authoritative movement should use transactions so journal reservation,
    /// conflict checks, and recovery observe the same publication boundary.
    pub fn entities_mut(&mut self) -> &mut EntityStore {
        &mut self.entities
    }

    /// Compiled registry.
    pub const fn registry(&self) -> &BlockStateRegistry {
        &self.registry
    }

    /// Component schemas.
    pub const fn schemas(&self) -> &ComponentSchemas {
        &self.schemas
    }

    /// Commit coordinator.
    pub const fn coordinator(&self) -> &CommitCoordinator {
        &self.coordinator
    }

    /// Mutable coordinator access for compatibility and resume updates.
    pub fn coordinator_mut(&mut self) -> &mut CommitCoordinator {
        &mut self.coordinator
    }

    /// Validate a transaction without changing the world.
    pub fn prepare(&self, transaction: &Transaction) -> Result<PreparedChange, PrepareError> {
        self.coordinator.prepare_with_components(
            &self.store,
            &self.entities,
            &self.registry,
            &self.schemas,
            transaction,
        )
    }

    /// Prepare one multi-source atomic group; see the cross-module contract.
    pub fn prepare_group(
        &self,
        group: &crate::TransactionGroup,
    ) -> Result<PreparedChange, PrepareError> {
        self.coordinator.prepare_group(
            &self.store,
            &self.entities,
            &self.registry,
            &self.schemas,
            group,
        )
    }

    /// Publish a prepared change.
    pub fn publish(&mut self, prepared: PreparedChange) -> Result<Publication, CommitFault> {
        self.coordinator
            .publish_with_entities(&mut self.store, &mut self.entities, prepared)
    }

    /// Prepare and publish one transaction.
    pub fn commit(
        &mut self,
        transaction: &Transaction,
    ) -> Result<TransactionResult<Committed>, CommitFault> {
        self.coordinator.commit_with_components(
            &mut self.store,
            &mut self.entities,
            &self.registry,
            &self.schemas,
            transaction,
        )
    }

    /// Relocate a wedged entity in one hop. The transaction carries a move
    /// whose start overlaps terrain; the destination must be free. Returns
    /// `Ok(false)` when the move was rejected.
    pub fn commit_escape(&mut self, transaction: &crate::Transaction) -> Result<bool, CommitFault> {
        let result = self.coordinator.commit_escape(
            &mut self.store,
            &mut self.entities,
            &self.registry,
            &self.schemas,
            transaction,
        )?;
        Ok(matches!(
            result.outcome,
            TransactionOutcome::Committed { .. }
        ))
    }

    pub fn commit_group(
        &mut self,
        group: &crate::TransactionGroup,
    ) -> Result<TransactionResult<Committed>, CommitFault> {
        self.coordinator.commit_group(
            &mut self.store,
            &mut self.entities,
            &self.registry,
            &self.schemas,
            group,
        )
    }

    /// Capture durable world state, omitting runtime-only attached components.
    pub fn capture_snapshot(&self, meta: SnapshotMeta) -> Result<Snapshot, ExchangeError> {
        crate::block_instances::validate(&self.store, &self.entities, &self.registry)?;
        Snapshot::capture_persistent(
            meta,
            &self.store,
            &self.entities,
            &self.registry,
            &self.schemas,
        )
    }

    /// Check the complete owned output budget before allocating a world snapshot.
    pub fn capture_snapshot_with_budget(
        &self,
        meta: SnapshotMeta,
        budget: torse_types::OutputBudget,
    ) -> Result<Snapshot, torse_types::Fault> {
        crate::block_instances::validate(&self.store, &self.entities, &self.registry)
            .map_err(|error| torse_types::Fault::invalid_data(error.to_string()))?;
        Snapshot::capture_persistent_with_budget(
            meta,
            &self.store,
            &self.entities,
            &self.registry,
            &self.schemas,
            budget,
        )
    }

    /// Start building a recovered world.
    pub fn begin_restore(&self) -> Recovery<'_> {
        Recovery::new(self.world, &self.registry, &self.schemas)
    }

    /// Consume the authority into its parts.
    pub fn into_parts(
        self,
    ) -> (
        SectionStore,
        EntityStore,
        BlockStateRegistry,
        ComponentSchemas,
        CommitCoordinator,
    ) {
        (
            self.store,
            self.entities,
            self.registry,
            self.schemas,
            self.coordinator,
        )
    }
}

/// Builder that turns a snapshot and its journal tail into an authority.
pub struct Recovery<'a> {
    world: WorldId,
    registry: &'a BlockStateRegistry,
    schemas: &'a ComponentSchemas,
    restored: Option<RestoredWorld>,
    seq: CommitSeq,
    compatibility: Compatibility,
    resume: ResumePoint,
}

impl<'a> Recovery<'a> {
    /// Apply an isolated residency record without executing gameplay or generation.
    pub fn replay_section(
        &mut self,
        changes: &ChangeSet,
        page: &Snapshot,
        load: bool,
        budget: torse_types::OutputBudget,
    ) -> Result<(), torse_types::Fault> {
        self.replay_section_impl(changes, page, load, false, budget)
    }

    /// Replay a generated page with the same allocator checks as its original publication.
    pub fn replay_generated_section(
        &mut self,
        changes: &ChangeSet,
        page: &Snapshot,
        budget: torse_types::OutputBudget,
    ) -> Result<(), torse_types::Fault> {
        self.replay_section_impl(changes, page, true, true, budget)
    }

    fn replay_section_impl(
        &mut self,
        changes: &ChangeSet,
        page: &Snapshot,
        load: bool,
        generated: bool,
        budget: torse_types::OutputBudget,
    ) -> Result<(), torse_types::Fault> {
        let invalid = || torse_types::Fault::invalid_data("illegal section recovery transition");
        if changes.meta.cursor.world != self.world
            || changes.meta.previous != self.seq
            || self.seq.next() != Some(changes.meta.cursor.seq)
            || changes.meta.resume_at != self.resume
            || changes.meta.compatibility != self.compatibility
            || !changes.edits.is_empty()
            || !changes.entity_moves.is_empty()
            || !changes.component_edits.is_empty()
            || changes.entity_lifecycle.is_some()
        {
            return Err(invalid());
        }
        let restored = self.restored.take().ok_or_else(invalid)?;
        let mut coordinator = CommitCoordinator::new(self.world).resumed_at(self.seq);
        coordinator.set_compatibility(self.compatibility);
        coordinator.set_resume_point(self.resume);
        let mut world = WorldAuthority {
            world: self.world,
            registry: self.registry.clone(),
            schemas: self.schemas.clone(),
            store: restored.store,
            entities: restored.entities,
            coordinator,
        };
        let prepared = if generated {
            world.prepare_generated_section(page, budget)?
        } else {
            world.prepare_section(page, load, budget)?
        };
        if prepared.meta() != changes.meta {
            return Err(invalid());
        }
        world
            .publish_section(prepared)
            .map_err(crate::streaming::fault)?;
        self.seq = changes.meta.cursor.seq;
        self.restored = Some(RestoredWorld {
            store: world.store,
            entities: world.entities,
            resume: self.resume,
        });
        Ok(())
    }

    pub fn parked_page_valid(&self, page: &Snapshot) -> Result<(), torse_types::Fault> {
        let restored = self
            .restored
            .as_ref()
            .ok_or_else(|| torse_types::Fault::invalid_data("missing snapshot"))?;
        if page.sections.len() != 1
            || restored.store.phase(page.sections[0].pos).is_some()
            || page.side != restored.store.side()
            || page.bounds != restored.store.bounds()
            || page.meta.cursor.world != self.world
            || page.meta.compatibility != self.compatibility
            || page.meta.cursor.seq > self.seq
            || page.next_entity_id > restored.entities.next_entity_id()
            || page
                .entities
                .iter()
                .any(|e| restored.entities.handle_of(e.id).is_some())
        {
            return Err(torse_types::Fault::invalid_data(
                "parked page conflicts with resident world",
            ));
        }
        let data = page
            .restore(self.registry, self.schemas)
            .map_err(crate::streaming::fault)?;
        crate::block_instances::validate(&data.store, &data.entities, self.registry)
            .map_err(crate::streaming::fault)?;
        if page.entities.iter().any(|e| {
            torse_world::Position::new(
                f64::from_bits(e.position_bits[0]),
                f64::from_bits(e.position_bits[1]),
                f64::from_bits(e.position_bits[2]),
            )
            .section(page.side)
                != Some(page.sections[0].pos)
        }) {
            return Err(torse_types::Fault::invalid_data(
                "parked entity outside section",
            ));
        }
        Ok(())
    }

    /// Create a recovery bound to one world's registry.
    pub fn new(
        world: WorldId,
        registry: &'a BlockStateRegistry,
        schemas: &'a ComponentSchemas,
    ) -> Self {
        Self {
            world,
            registry,
            schemas,
            restored: None,
            seq: CommitSeq::INITIAL,
            compatibility: Compatibility {
                authority_schema: 0,
                rules_digest: torse_types::Digest::default(),
                content_digest: torse_types::Digest::default(),
            },
            resume: ResumePoint {
                tick: torse_types::Tick(0),
                next_phase: 0,
                next_step: 0,
            },
        }
    }

    /// Apply the base snapshot exactly once.
    pub fn restore_snapshot(&mut self, snapshot: &Snapshot) -> Result<(), RecoveryError> {
        if self.restored.is_some() {
            return Err(RecoveryError::Payload(ExchangeError::Store {
                detail: "snapshot already applied".to_owned(),
            }));
        }
        if snapshot.meta.cursor.world != self.world {
            return Err(RecoveryError::WrongWorld {
                expected: self.world,
                actual: snapshot.meta.cursor.world,
            });
        }
        self.schemas
            .validate_compatibility(snapshot.meta.compatibility)
            .map_err(|error| {
                RecoveryError::Payload(ExchangeError::Store {
                    detail: error.to_string(),
                })
            })?;
        let snapshot = snapshot
            .persistent_view(self.schemas)
            .map_err(RecoveryError::Payload)?;
        let restored = snapshot
            .restore(self.registry, self.schemas)
            .map_err(RecoveryError::Payload)?;
        self.seq = snapshot.meta.cursor.seq;
        self.compatibility = snapshot.meta.compatibility;
        self.resume = snapshot.meta.resume_at;
        self.restored = Some(restored);
        Ok(())
    }

    /// Validate archive versions before rebuilding an isolated world.
    pub fn restore_archive(
        &mut self,
        archive: &torse_world::SnapshotArchive,
        content: &torse_content::CompiledContent,
        budget: torse_types::OutputBudget,
    ) -> Result<(), RecoveryError> {
        archive
            .validate_for_restore(content, budget)
            .map_err(|error| {
                RecoveryError::Payload(ExchangeError::Store {
                    detail: error.to_string(),
                })
            })?;
        self.restore_snapshot(&archive.snapshot)
    }

    /// Apply one committed change set onto the restored world.
    ///
    /// The change must continue the current commit position exactly. Applying a
    /// gap or a duplicate is rejected before anything is written.
    pub fn replay(&mut self, changes: &ChangeSet) -> Result<CommitSeq, RecoveryError> {
        let restored = self
            .restored
            .as_mut()
            .ok_or(RecoveryError::MissingSnapshot)?;
        if changes.meta.cursor.world != self.world {
            return Err(RecoveryError::WrongWorld {
                expected: self.world,
                actual: changes.meta.cursor.world,
            });
        }
        if changes.meta.previous != self.seq {
            return Err(RecoveryError::UnexpectedPosition {
                previous: changes.meta.previous,
                expected: self.seq,
            });
        }
        let expected = self.seq.next().ok_or(RecoveryError::SequenceExhausted)?;
        if changes.meta.cursor.seq != expected {
            return Err(RecoveryError::NonConsecutive {
                cursor: changes.meta.cursor.seq,
                expected,
            });
        }
        if changes.meta.compatibility != self.compatibility {
            return Err(RecoveryError::Payload(ExchangeError::Store {
                detail: "journal compatibility changed".to_owned(),
            }));
        }
        let changes = changes
            .persistent_view(self.schemas)
            .map_err(RecoveryError::Payload)?;
        changes
            .apply_with_components(
                &mut restored.store,
                &mut restored.entities,
                self.registry,
                self.schemas,
            )
            .map_err(RecoveryError::Payload)?;
        self.seq = expected;
        self.compatibility = changes.meta.compatibility;
        self.resume = changes.meta.resume_at;
        Ok(self.seq)
    }

    /// Position the recovery currently holds.
    pub const fn seq(&self) -> CommitSeq {
        self.seq
    }

    /// Check kernel-derived block instance effects during execution replay.
    /// Explicit lifecycle prefixes have already been matched to the frozen
    /// plan by the host. This does not execute actions, collision, or module code.
    pub fn validate_derived_lifecycle(
        &self,
        changes: &ChangeSet,
        spawns: usize,
        despawns: usize,
    ) -> Result<(), ExchangeError> {
        let invalid = || ExchangeError::Store {
            detail: "block instance lifecycle differs from cell writes".into(),
        };
        let restored = self.restored.as_ref().ok_or_else(invalid)?;
        let projected = changes.persistent_view(self.schemas)?;
        let changes = projected.as_ref();
        let existing = if spawns == 0 && despawns == 0 {
            None
        } else {
            let life = changes.entity_lifecycle.as_ref().ok_or_else(invalid)?;
            let spawned = life.spawned.get(..spawns).ok_or_else(invalid)?.to_vec();
            let despawned = life.despawned.get(..despawns).ok_or_else(invalid)?.to_vec();
            if spawned.iter().chain(&despawned).any(|e| {
                e.components
                    .contains_key(&torse_world::block_instance_schema())
            }) {
                return Err(invalid());
            }
            let explicit = torse_world::EntityLifecycle {
                previous_next_id: life.previous_next_id,
                next_id: life
                    .previous_next_id
                    .checked_add(spawns as u128)
                    .ok_or_else(invalid)?,
                spawned,
                despawned,
            };
            Some(
                restored
                    .entities
                    .prepare_lifecycle(&explicit, self.schemas)
                    .map_err(|_| invalid())?,
            )
        };
        let edits = changes
            .edits
            .iter()
            .map(|edit| {
                let state = match &edit.content {
                    torse_world::CellContent::Empty => torse_world::BlockStateId::EMPTY,
                    torse_world::CellContent::Block { block, values } => {
                        self.registry.state_of(block, values).ok_or_else(invalid)?
                    }
                };
                Ok(torse_world::BlockEdit {
                    pos: edit.pos,
                    state,
                })
            })
            .collect::<Result<Vec<_>, ExchangeError>>()?;
        let expected = crate::block_instances::prepare(
            &restored.store,
            &restored.entities,
            self.registry,
            self.schemas,
            crate::block_instances::Writes {
                edits: &edits,
                components: &changes.component_edits,
                movements: &changes.entity_moves,
            },
            existing,
        )
        .map_err(|_| invalid())?;
        let mut expected = expected.map(|b| b.change().clone());
        if let Some(life) = &mut expected {
            for entity in life.spawned.iter_mut().chain(&mut life.despawned) {
                entity.retain_persistent(self.schemas)?;
            }
        }
        if expected != changes.entity_lifecycle {
            return Err(invalid());
        }
        Ok(())
    }

    /// Immutable schema configuration used by durable projection validation.
    pub fn schemas(&self) -> &ComponentSchemas {
        self.schemas
    }

    /// Finish the builder and produce a usable authority.
    pub fn finish(self) -> Result<WorldAuthority, RecoveryError> {
        let restored = self.restored.ok_or(RecoveryError::MissingSnapshot)?;
        crate::block_instances::validate(&restored.store, &restored.entities, self.registry)
            .map_err(RecoveryError::Payload)?;
        let mut coordinator = CommitCoordinator::new(self.world);
        coordinator.set_compatibility(self.compatibility);
        coordinator.set_resume_point(self.resume);
        // The coordinator starts at the recovered position, so the next prepared
        // change receives exactly the following sequence number.
        let coordinator = coordinator.resumed_at(self.seq);
        let mut store = restored.store;
        // The snapshot bytes keep the historical layout. The host schemas carry
        // the opened faces, and the compatibility check above rejected a digest
        // that does not include that same policy.
        store.set_exterior(self.schemas.exterior_policy());
        Ok(WorldAuthority {
            world: self.world,
            registry: self.registry.clone(),
            schemas: self.schemas.clone(),
            store,
            entities: restored.entities,
            coordinator,
        })
    }
}

impl Recovery<'_> {
    /// Commit cursor of the recovered position.
    pub fn cursor(&self) -> CommitCursor {
        CommitCursor {
            world: self.world,
            seq: self.seq,
        }
    }

    /// Abort the builder without producing a world.
    ///
    /// Dropping the value has the same effect; the method exists so a caller can
    /// make the intent explicit at the failure site.
    pub fn abort(self) {}
}
#[cfg(test)]
mod tests {
    use super::*;
    use torse_content::{compile, m0_fixture_pack};
    use torse_types::SectionPos;
    use torse_types::{
        BlockPos, CommitCursor, ContentKey, Digest, Tick, TransactionId, TransactionOutcome,
    };
    use torse_world::{BlockStateId, Section, TerrainQuery};

    use crate::phase::Phase;
    use crate::transaction::OrderingKey;

    fn key(value: &str) -> ContentKey {
        ContentKey::parse(value).expect("test key is valid")
    }

    fn compatibility() -> Compatibility {
        Compatibility {
            authority_schema: 1,
            rules_digest: Digest([1; 32]),
            content_digest: Digest([2; 32]),
        }
    }

    fn authority() -> WorldAuthority {
        let content = compile(m0_fixture_pack()).expect("fixture content compiles");
        let registry = BlockStateRegistry::build(&content).expect("registry builds");
        let schemas = ComponentSchemas::build(&content);
        let mut authority = WorldAuthority::new(
            WorldId(5),
            registry,
            schemas,
            16,
            WorldBounds::new(0, 64).expect("bounds are valid"),
            compatibility(),
        )
        .expect("authority assembles");
        let store = authority.store_mut();
        store.request(SectionPos::ZERO).expect("request works");
        store
            .install(SectionPos::ZERO, Section::empty(16).expect("side is valid"))
            .expect("install works");
        store.finish_load(SectionPos::ZERO).expect("finish works");
        authority
    }

    fn transaction(id: u128, pos: BlockPos, state: BlockStateId) -> Transaction {
        let mut transaction = Transaction::new(
            TransactionId(id),
            key("base:test"),
            OrderingKey {
                phase: Phase::Interaction,
                rule_priority: 0,
                stable_task_key: id as u64,
                local_sequence: 0,
            },
        );
        transaction
            .read_block(pos, BlockStateId::EMPTY)
            .write_block(pos, state);
        transaction
    }

    fn snapshot_meta(authority: &WorldAuthority) -> SnapshotMeta {
        SnapshotMeta {
            cursor: CommitCursor {
                world: authority.world(),
                seq: authority.seq(),
            },
            compatibility: authority.coordinator().compatibility(),
            resume_at: ResumePoint {
                tick: Tick(1),
                next_phase: 3,
                next_step: 0,
            },
        }
    }

    #[test]
    fn snapshot_plus_journal_tail_recovers_the_same_world() {
        let mut authority = authority();
        let stone = authority
            .registry()
            .default_state(&key("base:stone"))
            .expect("stone exists");
        let glass = authority
            .registry()
            .default_state(&key("base:glass_slab"))
            .expect("glass exists");

        authority
            .commit(&transaction(1, BlockPos::new(1, 1, 1), stone))
            .expect("commit runs");
        let snapshot = authority
            .capture_snapshot(snapshot_meta(&authority))
            .expect("snapshot captures");
        assert_eq!(snapshot.meta.cursor.seq, CommitSeq(1));

        let committed = authority
            .commit(&transaction(2, BlockPos::new(2, 1, 1), glass))
            .expect("commit runs");
        let change = match &committed.outcome {
            TransactionOutcome::Committed { value, .. } => {
                crate::payload::changes_from_committed(authority.registry(), value)
                    .expect("payload converts")
            }
            TransactionOutcome::Rejected(code) => panic!("unexpected rejection {code:?}"),
        };
        assert_eq!(change.meta.cursor.seq, CommitSeq(2));

        let mut recovery = authority.begin_restore();
        recovery
            .restore_snapshot(&snapshot)
            .expect("snapshot applies");
        recovery.replay(&change).expect("journal tail applies");
        assert_eq!(recovery.seq(), CommitSeq(2));
        let mut recovered = recovery.finish().expect("finish works");

        assert_eq!(recovered.seq(), CommitSeq(2));
        assert_eq!(
            recovered.store().query(BlockPos::new(1, 1, 1)),
            TerrainQuery::Ready(stone)
        );
        assert_eq!(
            recovered.store().query(BlockPos::new(2, 1, 1)),
            TerrainQuery::Ready(glass)
        );
        assert_eq!(
            recovered.coordinator().compatibility(),
            compatibility(),
            "compatibility survives recovery"
        );

        // The recovered authority continues numbering where the journal ended.
        recovered
            .commit(&transaction(3, BlockPos::new(3, 1, 1), stone))
            .expect("commit runs");
        assert_eq!(recovered.seq(), CommitSeq(3));
    }

    #[test]
    fn replay_rejects_a_gap_in_the_journal() {
        let mut authority = authority();
        let stone = authority
            .registry()
            .default_state(&key("base:stone"))
            .expect("stone exists");
        authority
            .commit(&transaction(4, BlockPos::new(1, 1, 1), stone))
            .expect("commit runs");
        let snapshot = authority
            .capture_snapshot(snapshot_meta(&authority))
            .expect("snapshot captures");

        let mut recovery = authority.begin_restore();
        recovery
            .restore_snapshot(&snapshot)
            .expect("snapshot applies");
        let change = ChangeSet {
            entity_lifecycle: None,
            component_edits: Vec::new(),
            entity_moves: Vec::new(),
            meta: torse_types::CommitMeta {
                cursor: CommitCursor {
                    world: WorldId(5),
                    seq: CommitSeq(2),
                },
                previous: CommitSeq(0),
                compatibility: compatibility(),
                resume_at: ResumePoint {
                    tick: Tick(2),
                    next_phase: 0,
                    next_step: 0,
                },
            },
            edits: Vec::new(),
        };
        let error = recovery.replay(&change).expect_err("gap is rejected");
        assert!(matches!(error, RecoveryError::UnexpectedPosition { .. }));
        assert_eq!(recovery.seq(), CommitSeq(1));
    }

    #[test]
    fn payload_from_another_world_is_rejected() {
        let authority = authority();
        let mut recovery = authority.begin_restore();
        let mut snapshot = authority
            .capture_snapshot(SnapshotMeta {
                cursor: CommitCursor {
                    world: WorldId(9),
                    seq: CommitSeq::INITIAL,
                },
                compatibility: compatibility(),
                resume_at: ResumePoint {
                    tick: Tick(0),
                    next_phase: 0,
                    next_step: 0,
                },
            })
            .expect("snapshot captures");
        let error = recovery
            .restore_snapshot(&snapshot)
            .expect_err("foreign world is rejected");
        assert!(matches!(error, RecoveryError::WrongWorld { .. }));
        snapshot.meta.cursor.world = WorldId(5);
        recovery
            .restore_snapshot(&snapshot)
            .expect("own world applies");
    }

    #[test]
    fn finish_requires_a_snapshot() {
        let authority = authority();
        let recovery = authority.begin_restore();
        assert!(matches!(
            recovery.finish(),
            Err(RecoveryError::MissingSnapshot)
        ));
    }
}
