//! Native presentation acceptance through authority, wire, client and real files.
mod support;
use std::cell::Cell;
use std::rc::Rc;
use support::Directory;
use torse_codec::wire;
use torse_content::{ComponentField, ComponentFieldType, compile, m0_fixture_pack};
use torse_net::ReplicaMirror;
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::action::*;
use torse_sim::durable::*;
use torse_sim::replica::*;
use torse_sim::{OrderingKey, Phase, SystemSpec, Transaction, TransactionGroup, WorldAuthority};
use torse_types::*;
use torse_world::*;

fn key(s: &str) -> ContentKey {
    base::key(s)
}
fn draft(kind: &str) -> PresentationDraft {
    PresentationDraft {
        kind: key(kind),
        version: 1,
        section: SectionPos::ZERO,
        recipient: None,
        fields: [(key("test:value"), ComponentValue::U32(7))].into(),
    }
}
fn world() -> (WorldAuthority, ActionService) {
    let mut pack = m0_fixture_pack();
    torse_sim::block_actions::register_content(&mut pack);
    let content = compile(pack).unwrap();
    let mut schemas = ComponentSchemas::build(&content);
    for (id, owner, visibility) in [
        ("test:cue", "test:rule", PresentationVisibility::Public),
        (
            "test:other",
            "test:other-rule",
            PresentationVisibility::Public,
        ),
        (
            "test:action-cue",
            "world:module",
            PresentationVisibility::Public,
        ),
        (
            "test:private-cue",
            "test:rule",
            PresentationVisibility::Authorized {
                permission: key("test:hear"),
            },
        ),
    ] {
        schemas
            .register_presentation(PresentationSchema {
                id: key(id),
                version: 1,
                owner: key(owner),
                visibility,
                fields: vec![ComponentField {
                    id: key("test:value"),
                    field_type: ComponentFieldType::Unsigned32,
                    required: true,
                }],
            })
            .unwrap();
    }
    let mut descriptors = torse_sim::block_actions::descriptors();
    for d in &mut descriptors {
        d.max_items = 32;
        d.max_bytes = 8192;
    }
    let handlers = torse_sim::block_actions::handlers()
        .into_iter()
        .map(|mut h| {
            h.implementation = Box::new(CueHandler(h.implementation));
            h
        })
        .collect();
    let actions = ActionService::bind(descriptors, handlers, &schemas).unwrap();
    let mut world = WorldAuthority::new(
        base::WORLD,
        BlockStateRegistry::build(&content).unwrap(),
        schemas,
        16,
        WorldBounds::new(0, 32).unwrap(),
        Compatibility {
            authority_schema: 4,
            rules_digest: content.authority_digest,
            content_digest: content.presentation_digest,
        },
    )
    .unwrap();
    for p in [SectionPos::ZERO, SectionPos::new(1, 0, 0)] {
        world.store_mut().request(p).unwrap();
        world
            .store_mut()
            .install(p, Section::empty(16).unwrap())
            .unwrap();
        world.store_mut().finish_load(p).unwrap();
    }
    world
        .entities_mut()
        .spawn(Position::new(2.0, 2.0, 2.0))
        .unwrap();
    (world, actions)
}
struct CueHandler(Box<dyn ActionHandler>);
impl ActionHandler for CueHandler {
    fn plan(
        &self,
        view: &ActionView<'_>,
        request: &ActionRequest,
    ) -> Result<ActionOutput, RejectCode> {
        let mut result = self.0.plan(view, request)?;
        if let ActionOutput::Mutation(tx) = &mut result {
            tx.presentation.push(draft("test:action-cue"));
        }
        Ok(result)
    }
}
fn transaction(id: u128) -> Transaction {
    let mut tx = Transaction::new(
        TransactionId(id),
        key("test:rule"),
        OrderingKey {
            phase: Phase::Simulation,
            rule_priority: 0,
            stable_task_key: id as u64,
            local_sequence: 0,
        },
    );
    tx.presentation.push(draft("test:cue"));
    tx
}
fn viewer(id: u128) -> Viewer {
    Viewer {
        session: SessionId(id),
        actor: ActorContextId(id),
    }
}
fn actor() -> ActorContext {
    ActorContext::new(key("test:player")).section(SectionPos::ZERO)
}
fn scope() -> ReplicaScope {
    ReplicaScope {
        sections: [SectionPos::ZERO].into(),
    }
}
fn source() -> (ReplicaSource, Binding) {
    let (world, _) = world();
    let binding = Binding {
        world: world.world(),
        epoch: WorldEpoch(1),
        compatibility: world.coordinator().compatibility(),
    };
    (
        ReplicaSource::new(world, binding, base::execution_limits().replicas).unwrap(),
        binding,
    )
}
fn open(
    source: &mut ReplicaSource,
    id: u128,
    actor: ActorContext,
    scope: ReplicaScope,
) -> WorldReplicaSeed {
    source
        .open_replica(viewer(id), actor, scope, base::budget())
        .unwrap()
}
fn take(
    source: &mut ReplicaSource,
    binding: Binding,
    seed: &WorldReplicaSeed,
) -> Vec<WorldReplicaBatch> {
    source
        .take_replica_batches(binding, seed.subscription, base::budget())
        .unwrap()
}

#[test]
fn event_only_commits_authorize_before_queue_and_client_releases_once() {
    for incremental in [false, true] {
        let (mut source, binding) = source();
        source.set_projection_optimization(incremental);
        let allowed = open(
            &mut source,
            1,
            actor()
                .target(ActionTarget::Entity(EntityId(1)))
                .permission(key("test:hear")),
            scope(),
        );
        let denied = open(
            &mut source,
            2,
            actor().target(ActionTarget::Entity(EntityId(1))),
            scope(),
        );
        let outside = open(
            &mut source,
            3,
            actor(),
            ReplicaScope {
                sections: [SectionPos::new(1, 0, 0)].into(),
            },
        );
        let mut tx = transaction(1);
        let mut private = draft("test:private-cue");
        private.recipient = Some(EntityId(1));
        tx.presentation.push(private);
        let unreserved = source.prepare(&tx).unwrap();
        drop(unreserved);
        assert!(take(&mut source, binding, &allowed).is_empty());
        let p = source.prepare(&tx).unwrap();
        let cursor = p.meta().cursor;
        source.publish(p).unwrap();
        let batches = take(&mut source, binding, &allowed);
        assert_eq!(batches.len(), 1);
        assert!(batches[0].changes.sections.is_empty());
        assert!(batches[0].changes.entities.is_empty());
        assert_eq!(batches[0].changes.presentation.len(), 2);
        assert_eq!(
            take(&mut source, binding, &denied)[0]
                .changes
                .presentation
                .len(),
            1
        );
        assert!(take(&mut source, binding, &outside).is_empty());
        let mut encoded = Vec::new();
        wire::encode(
            &batches[0],
            &mut encoded,
            base::budget(),
            base::codec_limits(),
        )
        .unwrap();
        let decoded: WorldReplicaBatch =
            wire::decode(&encoded, base::budget(), base::codec_limits()).unwrap();
        let mut mirror = ReplicaMirror::from_seed(allowed).unwrap();
        let before = mirror.state.clone();
        let mut invalid: WorldReplicaBatch =
            wire::decode(&encoded, base::budget(), base::codec_limits()).unwrap();
        invalid.changes.presentation[0].id.epoch = WorldEpoch(2);
        assert!(mirror.apply_with_presentation(invalid).is_err());
        assert_eq!(mirror.state, before);
        let effects = mirror
            .apply_with_presentation(
                wire::decode(&encoded, base::budget(), base::codec_limits()).unwrap(),
            )
            .unwrap();
        assert_eq!(
            effects.iter().map(|e| e.id.index).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert!(effects.iter().all(|e| e.id.cursor == cursor));
        assert!(mirror.apply_with_presentation(decoded).is_err());
        source.close_replica(binding, outside.subscription).unwrap();
        let new = open(&mut source, 4, actor(), scope());
        assert!(take(&mut source, binding, &new).is_empty());
        assert_eq!(mirror.state, new.state);
    }
}

#[test]
fn group_order_failure_staleness_and_revocation_are_atomic() {
    let (mut source, binding) = source();
    let seed = open(&mut source, 1, actor(), scope());
    let a = transaction(1);
    let mut b = transaction(2);
    b.source = key("test:other-rule");
    b.presentation[0].kind = key("test:other");
    let group = TransactionGroup::new(TransactionId(3), a.ordering, vec![a.clone(), b]).unwrap();
    let stored = StoredTransactionGroup::capture(source.world(), &group).unwrap();
    let frozen = FrozenTask {
        transaction: Some(stored),
        request: None,
        consume: None,
        schedule: vec![],
        cancel: vec![],
        status: ReplyStatus::Committed,
        output: ComponentFields::new(),
    };
    let mut bytes = Vec::new();
    wire::encode(&frozen, &mut bytes, base::budget(), base::codec_limits()).unwrap();
    assert_eq!(bytes[2], 5);
    let decoded: FrozenTask = wire::decode(&bytes, base::budget(), base::codec_limits()).unwrap();
    assert_eq!(decoded, frozen);
    let restored = decoded
        .transaction
        .unwrap()
        .restore(source.world())
        .unwrap();
    let p = source.world().prepare_group(&restored).unwrap();
    let retry = source.world().prepare_group(&restored).unwrap();
    assert_eq!(p, retry);
    source.publish(p).unwrap();
    assert!(source.publish(retry).is_err());
    let batches = take(&mut source, binding, &seed);
    assert_eq!(batches.len(), 1);
    assert_eq!(
        batches[0]
            .changes
            .presentation
            .iter()
            .map(|e| e.draft.kind.clone())
            .collect::<Vec<_>>(),
        vec![key("test:other"), key("test:cue")]
    );
    let mut invalid = group;
    invalid.members[1].presentation[0].version = 2;
    assert!(source.world().prepare_group(&invalid).is_err());
    assert!(take(&mut source, binding, &seed).is_empty());
    let p = source.prepare(&a).unwrap();
    source.publish(p).unwrap();
    assert_eq!(source.invalidate_viewer(viewer(1)), vec![seed.subscription]);
    assert!(
        source
            .take_replica_batches(binding, seed.subscription, base::budget())
            .is_err()
    );
}

#[test]
fn payload_ownership_schema_and_count_budgets_reject_without_side_effects() {
    let (mut source, binding) = source();
    let seed = open(&mut source, 1, actor(), scope());
    for mode in 0..8 {
        let mut tx = transaction(1);
        match mode {
            0 => tx.source = key("test:other-rule"),
            1 => tx.presentation[0]
                .fields
                .insert(key("test:private-instance"), ComponentValue::U32(1))
                .map(|_| ())
                .unwrap_or(()),
            2 => tx.presentation[0].version = 0,
            3 => tx.presentation[0].recipient = Some(EntityId(1)),
            4 => tx.presentation[0].kind = key("test:private-cue"),
            5 => tx.presentation = vec![draft("test:cue"); 17],
            6 => {
                tx.presentation[0]
                    .fields
                    .insert(key("test:value"), ComponentValue::Text("x".repeat(4096)));
            }
            _ => tx.presentation[0].section = SectionPos::new(100, 0, 0),
        }
        assert!(source.prepare(&tx).is_err(), "mode {mode}");
        assert_eq!(source.world().seq(), CommitSeq(0));
        assert!(take(&mut source, binding, &seed).is_empty());
    }
    let mut tx = transaction(1);
    tx.presentation = vec![draft("test:cue"); 16];
    let p = source.prepare(&tx).unwrap();
    source.publish(p).unwrap();
    assert_eq!(
        take(&mut source, binding, &seed)[0]
            .changes
            .presentation
            .len(),
        16
    );
    let mut schema = source.world().schemas().clone();
    assert!(
        schema
            .register_presentation(PresentationSchema {
                id: key("test:new"),
                version: 1,
                owner: key("test:rule"),
                visibility: PresentationVisibility::Public,
                fields: vec![]
            })
            .is_err()
    );
}

struct CueRule;
impl DurableRule for CueRule {
    fn spec(&self) -> SystemSpec {
        SystemSpec {
            id: key("test:rule"),
            phase: Phase::Simulation,
            rule_priority: 0,
            before: vec![],
            after: vec![],
        }
    }
    fn plan(&self, _: &WorldAuthority, tick: Tick) -> Result<Vec<WorkPlan>, Fault> {
        Ok(if tick == Tick(0) {
            vec![WorkPlan::transaction(transaction(1))]
        } else {
            vec![]
        })
    }
}
fn rules() -> Vec<Box<dyn DurableRule>> {
    vec![Box::new(CueRule)]
}
fn create(dir: &Directory) -> base::Files {
    let (world, actions) = world();
    base::Files::create_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| ExecutionHost::new(binding, world, actions, rules(), base::execution_limits()),
    )
    .unwrap()
}
fn reopen(dir: &Directory) -> base::Files {
    let (world, actions) = world();
    base::Files::open_file(
        dir.acquire(),
        world.coordinator().compatibility(),
        base::Codec::new(base::codec_limits()).unwrap(),
        base::file_policy(),
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                rules(),
                base::execution_limits(),
            ))
        },
    )
    .unwrap()
}
fn subscribe(server: &mut base::Files) -> WorldReplicaSeed {
    server
        .register_session(
            viewer(1),
            actor().permission(key("world:permission/edit")),
            scope(),
        )
        .unwrap();
    server
        .open_replica(SessionId(1), scope(), base::budget())
        .unwrap()
}

#[test]
fn real_file_restart_preserves_pending_drafts_without_replaying_committed_events() {
    for cut in 0..=9 {
        let dir = Directory::new();
        let mut server = create(&dir);
        let seed = subscribe(&mut server);
        for _ in 0..cut {
            server.drive_one(&mut NoReplicas).unwrap();
        }
        let old = server
            .take_replica(seed.subscription, base::budget())
            .unwrap()
            .iter()
            .map(|b| b.changes.presentation.len())
            .sum::<usize>();
        assert!(old <= 1);
        drop(server);
        let mut server = reopen(&dir);
        let seed = subscribe(&mut server);
        assert!(
            server
                .take_replica(seed.subscription, base::budget())
                .unwrap()
                .is_empty()
        );
        server.drive_tick(&mut NoReplicas, 1024).unwrap();
        let batches = server
            .take_replica(seed.subscription, base::budget())
            .unwrap();
        let new = batches
            .iter()
            .map(|b| b.changes.presentation.len())
            .sum::<usize>();
        assert_eq!(old + new, 1, "cut {cut}");
        for event in batches.iter().flat_map(|b| &b.changes.presentation) {
            assert_eq!(event.id.epoch, seed.binding.epoch);
        }
    }
}

#[test]
fn successful_request_deduplication_emits_no_second_cue() {
    let dir = Directory::new();
    let mut server = create(&dir);
    let seed = subscribe(&mut server);
    let request = || WorldRequest {
        id: RequestId {
            world: base::WORLD,
            epoch: seed.binding.epoch,
            session: SessionId(1),
            seq: RequestSeq(1),
        },
        actor: ActorContextId(1),
        input: Call {
            action: key("world:action/place"),
            version: 1,
            target: ActionTarget::Block(BlockPos::new(8, 1, 1)),
            parameters: [(
                key("world:field/block"),
                ComponentValue::Content(key("base:stone")),
            )]
            .into(),
        },
    };
    assert!(matches!(server.submit(request()), Admission::Queued { .. }));
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    assert_eq!(
        server.receipt(request().id).unwrap().unwrap().reply.status,
        ReplyStatus::Committed
    );
    let batches = server
        .take_replica(seed.subscription, base::budget())
        .unwrap();
    assert_eq!(
        batches
            .iter()
            .flat_map(|b| &b.changes.presentation)
            .filter(|e| e.draft.kind == key("test:action-cue"))
            .count(),
        1
    );
    assert_eq!(server.submit(request()), Admission::Duplicate);
    server.drive_tick(&mut NoReplicas, 1024).unwrap();
    assert!(
        server
            .take_replica(seed.subscription, base::budget())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn slow_consumers_reset_without_retaining_historical_effects() {
    let (world, _) = world();
    let binding = Binding {
        world: world.world(),
        epoch: WorldEpoch(1),
        compatibility: world.coordinator().compatibility(),
    };
    let mut limits = base::execution_limits().replicas;
    limits.max_queued_batches = 1;
    let mut source = ReplicaSource::new(world, binding, limits).unwrap();
    let seed = open(&mut source, 1, actor(), scope());
    for id in 1..=2 {
        let prepared = source.prepare(&transaction(id)).unwrap();
        let result = source.publish(prepared).unwrap();
        assert_eq!(result.invalidated_replicas.len(), usize::from(id == 2));
    }
    assert!(
        source
            .take_replica_batches(binding, seed.subscription, base::budget())
            .is_err()
    );
    let new = open(&mut source, 2, actor(), scope());
    assert!(take(&mut source, binding, &new).is_empty());
}

#[test]
fn private_only_commit_produces_no_unauthorized_batch_and_take_budget_is_retryable() {
    let (mut source, binding) = source();
    let authorized = open(
        &mut source,
        1,
        actor()
            .target(ActionTarget::Entity(EntityId(1)))
            .permission(key("test:hear")),
        scope(),
    );
    let denied = open(&mut source, 2, actor(), scope());
    let mut tx = transaction(1);
    tx.presentation[0] = draft("test:private-cue");
    tx.presentation[0].recipient = Some(EntityId(1));
    let prepared = source.prepare(&tx).unwrap();
    source.publish(prepared).unwrap();
    assert!(take(&mut source, binding, &denied).is_empty());
    assert!(
        source
            .take_replica_batches(
                binding,
                authorized.subscription,
                OutputBudget {
                    max_items: 1,
                    max_bytes: 1
                }
            )
            .is_err()
    );
    let batches = take(&mut source, binding, &authorized);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].changes.presentation.len(), 1);
    assert_eq!(batches[0].seq, ReplicaSeq(1));
}

#[test]
fn schema_compatibility_and_aggregate_limits_are_independent_of_components() {
    let content = compile(m0_fixture_pack()).unwrap();
    let base = Compatibility {
        authority_schema: 4,
        rules_digest: content.authority_digest,
        content_digest: content.presentation_digest,
    };
    let schema = PresentationSchema {
        id: key("test:caption"),
        owner: key("test:rule"),
        version: 1,
        visibility: PresentationVisibility::Public,
        fields: vec![ComponentField {
            id: key("test:caption"),
            field_type: ComponentFieldType::String { max_bytes: 3500 },
            required: true,
        }],
    };
    let mut schemas = ComponentSchemas::build(&content);
    let mut opaque = schema.clone();
    opaque.fields[0].field_type = ComponentFieldType::Record;
    assert!(schemas.register_presentation(opaque).is_err());
    schemas.register_presentation(schema.clone()).unwrap();
    assert!(schemas.register_presentation(schema).is_err());
    let (schemas, effective) = schemas.bind_compatibility(base).unwrap();
    assert_ne!(effective, base);
    assert!(schemas.validate_compatibility(base).is_err());
    let (_, unchanged) = ComponentSchemas::build(&content)
        .bind_compatibility(base)
        .unwrap();
    assert_eq!(unchanged, base);
    let mut event = draft("test:caption");
    event.fields = [(key("test:caption"), ComponentValue::Text("a".repeat(3000)))].into();
    schemas
        .validate_presentation(&key("test:rule"), &event)
        .unwrap();
    assert!(
        presentation_budget(std::iter::repeat_n(&event, 11), MAX_PRESENTATION_PER_COMMIT).is_err()
    );
    event.fields = [(
        key("test:caption"),
        ComponentValue::Record(ComponentFields::new()),
    )]
    .into();
    assert!(presentation_size(&event).is_err());
    let (mut source, _) = source();
    let mut a = transaction(1);
    a.presentation = vec![draft("test:cue"); 16];
    let mut b = transaction(2);
    b.source = key("test:other-rule");
    b.presentation = vec![draft("test:other"); 16];
    let mut c = transaction(3);
    c.source = key("world:module");
    c.presentation = vec![draft("test:action-cue")];
    let group = TransactionGroup::new(TransactionId(4), a.ordering, vec![a, b, c]).unwrap();
    assert!(source.world().prepare_group(&group).is_err());
    assert!(StoredTransactionGroup::capture(source.world(), &group).is_err());
    let seed = open(&mut source, 1, actor(), scope());
    let mut bounded = group;
    bounded.members.pop();
    let prepared = source.world().prepare_group(&bounded).unwrap();
    source.publish(prepared).unwrap();
    let binding = seed.binding;
    assert_eq!(
        take(&mut source, binding, &seed)[0]
            .changes
            .presentation
            .len(),
        32
    );
}

struct Controlled<J> {
    inner: J,
    busy: Rc<Cell<bool>>,
    fail: Rc<Cell<bool>>,
}
impl<J: torse_storage::CommitJournal<ExecutionModel>> torse_storage::CommitJournal<ExecutionModel>
    for Controlled<J>
{
    type Staged = J::Staged;
    fn try_stage(
        &mut self,
        record: &CommitRecord<ExecutionChanges>,
    ) -> Result<Self::Staged, StageError> {
        if self.busy.get() {
            Err(StageError::Busy)
        } else {
            self.inner.try_stage(record)
        }
    }
    fn accept(&mut self, staged: Self::Staged) -> Result<(), Fault> {
        self.inner.accept(staged)
    }
    fn flush_through(&mut self, cursor: CommitCursor) -> Result<Durability, Fault> {
        if self.fail.get() {
            Err(Fault::new(
                FaultCode::StorageUnavailable,
                "injected presentation flush failure",
            ))
        } else {
            self.inner.flush_through(cursor)
        }
    }
    fn durability(&self) -> Durability {
        self.inner.durability()
    }
}

#[test]
fn journal_refusal_and_failed_persistence_expose_no_events() {
    for fail_flush in [false, true] {
        let dir = Directory::new();
        let directory = dir.acquire();
        let (world, actions) = world();
        let binding = Binding {
            world: world.world(),
            epoch: directory.epoch(),
            compatibility: world.coordinator().compatibility(),
        };
        let codec = base::Codec::new(base::codec_limits()).unwrap();
        let policy = base::file_policy();
        let busy = Rc::new(Cell::new(false));
        let fail = Rc::new(Cell::new(false));
        let journal = Controlled {
            inner: torse_storage::MemoryJournal::memory(
                binding,
                CommitCursor::initial(base::WORLD),
                codec,
                policy.journal,
                64_000_000,
            )
            .unwrap(),
            busy: busy.clone(),
            fail: fail.clone(),
        };
        let checkpoints = torse_storage::Checkpoints::open(
            binding,
            codec,
            policy.checkpoint,
            torse_storage::MemoryCheckpointBackend::new(64_000_000),
        )
        .unwrap();
        let host =
            ExecutionHost::new(binding, world, actions, rules(), base::execution_limits()).unwrap();
        let mut server =
            Server::initialize(directory, host, journal, checkpoints, policy.server).unwrap();
        server
            .register_session(viewer(1), actor(), scope())
            .unwrap();
        let seed = server
            .open_replica(SessionId(1), scope(), base::budget())
            .unwrap();
        loop {
            let host = server.authority().unwrap();
            if host.resume_point().next_phase == Phase::Simulation.index_u16()
                && host.runtime().work.is_some()
            {
                break;
            }
            server.drive_one(&mut NoReplicas).unwrap();
        }
        let before = server.authority().unwrap().cursor();
        busy.set(true);
        for _ in 0..2 {
            assert!(matches!(
                server.drive_one(&mut NoReplicas).unwrap(),
                CommitStep::Blocked(StageError::Busy)
            ));
            assert_eq!(server.authority().unwrap().cursor(), before);
            assert!(
                server
                    .take_replica(seed.subscription, base::budget())
                    .unwrap()
                    .is_empty()
            );
        }
        busy.set(false);
        fail.set(fail_flush);
        if fail_flush {
            assert!(server.drive_one(&mut NoReplicas).is_err());
            assert_eq!(server.state(), ServerState::Faulted);
            assert!(
                server
                    .take_replica(seed.subscription, base::budget())
                    .is_err()
            );
        } else {
            server.drive_one(&mut NoReplicas).unwrap();
            let batches = server
                .take_replica(seed.subscription, base::budget())
                .unwrap();
            assert_eq!(batches.len(), 1);
            assert_eq!(batches[0].changes.presentation.len(), 1);
            assert_eq!(
                batches[0].changes.presentation[0].id.cursor.seq,
                before.seq.next().unwrap()
            );
            assert!(
                server
                    .take_replica(seed.subscription, base::budget())
                    .unwrap()
                    .is_empty()
            );
        }
    }
}
