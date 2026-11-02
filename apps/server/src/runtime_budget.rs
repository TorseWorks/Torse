//! Cache immutable tables and exact event values; recheck dynamic state each time.
use super::*;
use torse_codec::wire::WireCost;

#[derive(Default)]
pub(super) struct RuntimeBudget {
    rules: Option<(Arc<Vec<SystemSpec>>, WireCost)>,
    tasks: Option<(Arc<Vec<FrozenTask>>, WireCost)>,
    empty_rules: Arc<Vec<SystemSpec>>,
    empty_tasks: Arc<Vec<FrozenTask>>,
    events: BTreeMap<EventId, (TimedEvent, WireCost)>,
    context: Option<(OutputBudget, CodecLimits, usize)>,
}

fn table_cost<T: wire::Wire>(
    cache: &mut Option<(Arc<T>, WireCost)>,
    value: &Arc<T>,
    limits: ExecutionLimits,
) -> Result<WireCost, Fault> {
    if let Some((old, cost)) = cache
        && Arc::ptr_eq(old, value)
    {
        return Ok(*cost);
    }
    let cost = wire::measure_inner(value, limits.runtime, limits.codec)?;
    *cache = Some((Arc::clone(value), cost));
    Ok(cost)
}

impl RuntimeBudget {
    pub(super) fn check(
        &mut self,
        state: &RuntimeState,
        limits: ExecutionLimits,
    ) -> Result<(), Fault> {
        let context = (limits.runtime, limits.codec, limits.events);
        if self.context != Some(context) {
            *self = Self::default();
            self.context = Some(context);
        }
        let rules = table_cost(&mut self.rules, &state.rules, limits)?;
        let empty_rules = wire::measure_inner(&self.empty_rules, limits.runtime, limits.codec)?;
        let mut short = RuntimeState {
            rules: Arc::clone(&self.empty_rules),
            next_event: state.next_event,
            events: Vec::new(),
            windows: state.windows.clone(),
            work: state.work.clone(),
        };
        let empty_events = wire::measure_inner(&short.events, limits.runtime, limits.codec)?;
        let events = self.event_cost(state, limits, empty_events)?;
        let mut additions = vec![(rules, empty_rules), (events, empty_events)];
        if let Some(work) = &mut short.work {
            let tasks = table_cost(&mut self.tasks, &work.tasks, limits)?;
            additions.push((
                tasks,
                wire::measure_inner(&self.empty_tasks, limits.runtime, limits.codec)?,
            ));
            work.tasks = Arc::clone(&self.empty_tasks);
        } else {
            self.tasks = None;
        }
        let mut total = wire::measure_inner(&short, limits.runtime, limits.codec)?;
        for (full, empty) in additions {
            total.encoded_bytes = total
                .encoded_bytes
                .checked_sub(empty.encoded_bytes)
                .and_then(|n| n.checked_add(full.encoded_bytes))
                .ok_or_else(overflow)?;
            total.owned_bytes = total
                .owned_bytes
                .checked_sub(empty.owned_bytes)
                .and_then(|n| n.checked_add(full.owned_bytes))
                .ok_or_else(overflow)?;
            total.items = total
                .items
                .checked_sub(empty.items)
                .and_then(|n| n.checked_add(full.items))
                .ok_or_else(overflow)?;
        }
        let encoded = total.encoded_bytes.checked_add(2).ok_or_else(overflow)?;
        let owned = total
            .owned_bytes
            .checked_add(size_of::<RuntimeState>() as u64)
            .ok_or_else(overflow)?;
        let items = total.items.checked_add(1).ok_or_else(overflow)?;
        if encoded
            > limits
                .runtime
                .max_bytes
                .min(u64::from(limits.codec.max_frame_bytes))
            || !limits.runtime.allows(items, owned)
        {
            return Err(Fault::resource_limit("complete runtime budget exceeded"));
        }
        Ok(())
    }

    fn event_cost(
        &mut self,
        state: &RuntimeState,
        limits: ExecutionLimits,
        empty: WireCost,
    ) -> Result<WireCost, Fault> {
        if state.events.len() > limits.events {
            return Err(Fault::resource_limit("event cost cache capacity"));
        }
        // Removed events cannot accumulate in this process-local cache.
        self.events
            .retain(|id, _| state.events.iter().any(|e| e.id == *id));
        let mut retained = self.events.values().try_fold(0u64, |sum, (_, cost)| {
            sum.checked_add(cost.owned_bytes).ok_or_else(overflow)
        })?;
        let mut total = empty;
        for event in &state.events {
            let cost = match self.events.get(&event.id) {
                Some((old, cost)) if old == event => *cost,
                _ => {
                    // Measure a singleton with the real Vec codec, including its
                    // element storage/items, rather than duplicating wire layout.
                    let cost =
                        wire::measure_inner(&vec![event.clone()], limits.runtime, limits.codec)?;
                    if let Some((_, old)) = self.events.remove(&event.id) {
                        retained = retained.checked_sub(old.owned_bytes).ok_or_else(overflow)?;
                    }
                    if let Some(next) = retained.checked_add(cost.owned_bytes)
                        && next <= limits.runtime.max_bytes
                    {
                        self.events.insert(event.id, (event.clone(), cost));
                        retained = next;
                    }
                    cost
                }
            };
            total.encoded_bytes = total
                .encoded_bytes
                .checked_add(
                    cost.encoded_bytes
                        .checked_sub(empty.encoded_bytes)
                        .ok_or_else(overflow)?,
                )
                .ok_or_else(overflow)?;
            total.owned_bytes = total
                .owned_bytes
                .checked_add(
                    cost.owned_bytes
                        .checked_sub(empty.owned_bytes)
                        .ok_or_else(overflow)?,
                )
                .ok_or_else(overflow)?;
            total.items = total
                .items
                .checked_add(cost.items.checked_sub(empty.items).ok_or_else(overflow)?)
                .ok_or_else(overflow)?;
        }
        Ok(total)
    }
}

fn overflow() -> Fault {
    Fault::resource_limit("runtime cost arithmetic overflow")
}

#[cfg(test)]
mod tests {
    use super::*;
    use torse_world::ComponentValue;

    #[test]
    fn event_cost_reuse_matches_full_codec_on_mutation_removal_and_exact_limits() {
        fn reference(state: &RuntimeState, limits: ExecutionLimits) -> bool {
            let mut encoded = vec![];
            wire::encode(state, &mut encoded, limits.runtime, limits.codec)
                .and_then(|_| wire::decode::<RuntimeState>(&encoded, limits.runtime, limits.codec))
                .is_ok()
        }
        let mut state = RuntimeState {
            rules: vec![SystemSpec {
                id: crate::bootstrap::key("cost:rule"),
                phase: Phase::Simulation,
                rule_priority: 0,
                before: vec![],
                after: vec![],
            }]
            .into(),
            next_event: 129,
            events: vec![],
            windows: vec![],
            work: None,
        };
        let mut cache = RuntimeBudget::default();
        let limits = crate::bootstrap::execution_limits();
        for count in [0, 1, 16, 64, 128] {
            state.events = (1..=count)
                .map(|id| TimedEvent {
                    id: EventId(id),
                    event: EventDraft {
                        tick: Tick(3),
                        rule: crate::bootstrap::key("cost:rule"),
                        payload: [(
                            crate::bootstrap::key("cost:value"),
                            ComponentValue::List(vec![ComponentValue::Record(
                                [
                                    (
                                        crate::bootstrap::key("cost:text"),
                                        ComponentValue::Text("x".repeat(80)),
                                    ),
                                    (
                                        crate::bootstrap::key("cost:entity"),
                                        ComponentValue::Entity(EntityId(id)),
                                    ),
                                ]
                                .into(),
                            )]),
                        )]
                        .into(),
                    },
                })
                .collect();
            for change in 0..3 {
                if let Some(event) = state.events.first_mut() {
                    event.event.payload.insert(
                        crate::bootstrap::key("cost:changed"),
                        ComponentValue::Text("y".repeat(change * 90)),
                    );
                }
                assert!(cache.check(&state, limits).is_ok());
                let cost = wire::measure_inner(&state, limits.runtime, limits.codec).unwrap();
                let required_bytes = (cost.owned_bytes + size_of::<RuntimeState>() as u64)
                    .max(cost.encoded_bytes + 2);
                for items in [cost.items, cost.items + 1, cost.items + 2] {
                    for bytes in [required_bytes - 1, required_bytes, required_bytes + 1] {
                        let l = ExecutionLimits {
                            runtime: OutputBudget::new(items, bytes),
                            ..limits
                        };
                        assert_eq!(
                            cache.check(&state, l).is_ok(),
                            reference(&state, l),
                            "count={count} change={change} items={items} bytes={bytes}"
                        );
                        assert!(
                            cache
                                .events
                                .values()
                                .map(|(_, c)| c.owned_bytes)
                                .sum::<u64>()
                                <= bytes
                        );
                    }
                }
                for frame in [cost.encoded_bytes + 1, cost.encoded_bytes + 2] {
                    let l = ExecutionLimits {
                        codec: CodecLimits {
                            max_frame_bytes: frame as u32,
                            ..limits.codec
                        },
                        ..limits
                    };
                    assert_eq!(cache.check(&state, l).is_ok(), reference(&state, l));
                }
                let l = ExecutionLimits {
                    codec: CodecLimits {
                        max_field_bytes: 64,
                        ..limits.codec
                    },
                    ..limits
                };
                assert_eq!(cache.check(&state, l).is_ok(), reference(&state, l));
            }
            state.events.retain(|event| event.id.0 % 2 == 0);
            assert_eq!(
                cache.check(&state, limits).is_ok(),
                reference(&state, limits)
            );
            assert!(
                cache
                    .events
                    .keys()
                    .all(|id| state.events.iter().any(|event| event.id == *id))
            );
        }
    }

    #[test]
    fn cached_cost_matches_full_wire_validation_at_capacity_boundaries_and_after_cow() {
        let mut state = RuntimeState {
            rules: vec![SystemSpec {
                id: crate::bootstrap::key("cost:rule"),
                phase: Phase::Simulation,
                rule_priority: 0,
                before: vec![],
                after: vec![],
            }]
            .into(),
            next_event: 2,
            events: vec![TimedEvent {
                id: EventId(1),
                event: EventDraft {
                    tick: Tick(3),
                    rule: crate::bootstrap::key("cost:rule"),
                    payload: [(
                        crate::bootstrap::key("cost:text"),
                        torse_world::ComponentValue::Text("x".repeat(120)),
                    )]
                    .into(),
                },
            }],
            windows: vec![],
            work: None,
        };
        let mut cache = RuntimeBudget::default();
        for count in [0, 1, 16, 128] {
            state.work = Some(PhaseWork {
                event_batch: false,
                tasks: (0..count)
                    .map(|_| FrozenTask {
                        transaction: None,
                        request: None,
                        consume: None,
                        schedule: vec![],
                        cancel: vec![],
                        status: ReplyStatus::Committed,
                        output: [(
                            crate::bootstrap::key("cost:value"),
                            torse_world::ComponentValue::Text("z".repeat(40)),
                        )]
                        .into(),
                    })
                    .collect::<Vec<_>>()
                    .into(),
                next: 0,
            });
            for items in [1, 4, 16, 128, 1024, 32000] {
                for bytes in [64, 256, 512, 1024, 4096, 16384, 65536] {
                    let mut limits = crate::bootstrap::execution_limits();
                    limits.runtime = OutputBudget::new(items, bytes);
                    let mut encoded = vec![];
                    let reference =
                        wire::encode(&state, &mut encoded, limits.runtime, limits.codec)
                            .and_then(|_| {
                                wire::decode::<RuntimeState>(&encoded, limits.runtime, limits.codec)
                            })
                            .is_ok();
                    assert_eq!(
                        cache.check(&state, limits).is_ok(),
                        reference,
                        "count={count} items={items} bytes={bytes}"
                    );
                }
            }
            // Retained cache references force copy-on-write and invalidate the key.
            let work = state.work.as_mut().unwrap();
            if !work.tasks.is_empty() {
                Arc::make_mut(&mut work.tasks)[0].output.insert(
                    crate::bootstrap::key("cost:extra"),
                    torse_world::ComponentValue::Text("q".repeat(4096)),
                );
                let mut limits = crate::bootstrap::execution_limits();
                limits.runtime.max_bytes = 4096;
                assert!(cache.check(&state, limits).is_err());
            }
        }
    }
}
