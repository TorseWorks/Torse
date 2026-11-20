//! Tick phases and deterministic system ordering.
//!
//! Phases are the public contract between the framework and every system. The
//! order inside a phase is not registration order: systems declare dependencies
//! and the assembler derives a topological order with a stable tie-break, so the
//! same declarations always produce the same execution order.
//!
//! Phase order itself is fixed for M1. A game definition may later disable
//! optional modules, which removes their systems rather than reordering the
//! remaining ones.

use std::collections::{BTreeMap, BTreeSet};

use torse_types::{ContentKey, ResumePoint, Tick};

/// Next whole-phase boundary, or `None` for unsupported steps or tick overflow.
pub fn next_resume_point(point: ResumePoint) -> Option<ResumePoint> {
    Phase::from_index(point.next_phase)?;
    if point.next_step != 0 {
        return None;
    }
    let (tick, next_phase) = if point.next_phase == Phase::Publish.index_u16() {
        (Tick(point.tick.0.checked_add(1)?), 0)
    } else {
        (point.tick, point.next_phase + 1)
    };
    Some(ResumePoint {
        tick,
        next_phase,
        next_step: 0,
    })
}

/// Simulation phase, in execution order.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Phase {
    /// Accept input, movement, and administrative requests for a target tick.
    Input,
    /// Apply accepted load and migration requests; build the active set.
    Lifecycle,
    /// Commit interactions accepted by the action service.
    Interaction,
    /// Timed machines, creatures, and machine networks.
    Simulation,
    /// Bounded follow-up rules produced by earlier phases.
    Followup,
    /// Publish committed changes to replication, persistence, and extraction.
    Publish,
}

impl Phase {
    /// All phases in execution order.
    pub const ALL: [Self; 6] = [
        Self::Input,
        Self::Lifecycle,
        Self::Interaction,
        Self::Simulation,
        Self::Followup,
        Self::Publish,
    ];

    /// Position of this phase in the tick.
    pub const fn index(self) -> usize {
        match self {
            Self::Input => 0,
            Self::Lifecycle => 1,
            Self::Interaction => 2,
            Self::Simulation => 3,
            Self::Followup => 4,
            Self::Publish => 5,
        }
    }

    /// Position of this phase as the `u16` used by `ResumePoint::next_phase`.
    pub const fn index_u16(self) -> u16 {
        self.index() as u16
    }

    /// Resolve a `ResumePoint::next_phase` value.
    ///
    /// Values outside `0..=5` are rejected so a corrupt or future record cannot
    /// silently resume at an arbitrary phase.
    pub const fn from_index(index: u16) -> Option<Self> {
        Some(match index {
            0 => Self::Input,
            1 => Self::Lifecycle,
            2 => Self::Interaction,
            3 => Self::Simulation,
            4 => Self::Followup,
            5 => Self::Publish,
            _ => return None,
        })
    }

    /// Stable name used in diagnostics and recorded rule summaries.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Lifecycle => "lifecycle",
            Self::Interaction => "interaction",
            Self::Simulation => "simulation",
            Self::Followup => "followup",
            Self::Publish => "publish",
        }
    }
}

/// Declared system and its ordering constraints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemSpec {
    /// Stable system identity.
    pub id: ContentKey,
    /// Phase in which the system runs.
    pub phase: Phase,
    /// Tie-break priority inside the phase; lower runs first.
    pub rule_priority: i32,
    /// Systems that must run after this one.
    pub before: Vec<ContentKey>,
    /// Systems that must run before this one.
    pub after: Vec<ContentKey>,
}

/// Failure produced while ordering systems.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OrderError {
    /// Two systems declared the same identity.
    Duplicate {
        /// Repeated identity.
        id: ContentKey,
    },
    /// A system referenced an identity that is not declared in the same phase.
    UnknownDependency {
        /// Declaring system.
        system: ContentKey,
        /// Missing dependency.
        target: ContentKey,
    },
    /// The declared constraints form a cycle.
    Cycle {
        /// Systems that could not be ordered.
        remaining: Vec<ContentKey>,
    },
}

impl std::fmt::Display for OrderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate { id } => write!(f, "system {id} is declared twice"),
            Self::UnknownDependency { system, target } => {
                write!(f, "system {system} depends on undeclared {target}")
            }
            Self::Cycle { remaining } => {
                let names: Vec<String> = remaining.iter().map(ToString::to_string).collect();
                write!(f, "system ordering cycle among {}", names.join(", "))
            }
        }
    }
}

impl std::error::Error for OrderError {}

/// Derive the execution order of systems inside one phase.
///
/// Only systems of the same phase may reference each other; a dependency on
/// another phase is reported as unknown, because cross-phase order is already
/// fixed by [`Phase::ALL`]. Ties are broken by (rule priority, identity), which
/// makes the result independent of declaration order.
pub fn order_phase(specs: &[SystemSpec]) -> Result<Vec<ContentKey>, OrderError> {
    let mut index: BTreeMap<&ContentKey, usize> = BTreeMap::new();
    for (position, spec) in specs.iter().enumerate() {
        if index.insert(&spec.id, position).is_some() {
            return Err(OrderError::Duplicate {
                id: spec.id.clone(),
            });
        }
    }

    let mut edges: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); specs.len()];
    let mut incoming = vec![0usize; specs.len()];
    for (position, spec) in specs.iter().enumerate() {
        for (target, before) in spec
            .before
            .iter()
            .map(|target| (target, true))
            .chain(spec.after.iter().map(|target| (target, false)))
        {
            let Some(target_position) = index.get(target) else {
                return Err(OrderError::UnknownDependency {
                    system: spec.id.clone(),
                    target: target.clone(),
                });
            };
            if specs[*target_position].phase != spec.phase {
                return Err(OrderError::UnknownDependency {
                    system: spec.id.clone(),
                    target: target.clone(),
                });
            }
            let (from, to) = if before {
                (position, *target_position)
            } else {
                (*target_position, position)
            };
            if from == to {
                return Err(OrderError::Cycle {
                    remaining: vec![spec.id.clone()],
                });
            }
            if edges[from].insert(to) {
                incoming[to] += 1;
            }
        }
    }

    // Ready set ordered by (priority, id) keeps the result deterministic even
    // when several systems are independent.
    let mut ready: BTreeSet<(i32, ContentKey, usize)> = BTreeSet::new();
    for (position, spec) in specs.iter().enumerate() {
        if incoming[position] == 0 {
            ready.insert((spec.rule_priority, spec.id.clone(), position));
        }
    }

    let mut order = Vec::with_capacity(specs.len());
    while let Some((_, _, position)) = ready.pop_first() {
        order.push(specs[position].id.clone());
        for next in edges[position].clone() {
            incoming[next] -= 1;
            if incoming[next] == 0 {
                let spec = &specs[next];
                ready.insert((spec.rule_priority, spec.id.clone(), next));
            }
        }
    }

    if order.len() != specs.len() {
        let ordered: BTreeSet<&ContentKey> = order.iter().collect();
        let remaining = specs
            .iter()
            .filter(|spec| !ordered.contains(&spec.id))
            .map(|spec| spec.id.clone())
            .collect();
        return Err(OrderError::Cycle { remaining });
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_progress_wraps_only_after_publish_and_rejects_invalid_points() {
        let mut point = ResumePoint {
            tick: Tick(9),
            next_phase: 0,
            next_step: 0,
        };
        for phase in 1..6 {
            point = next_resume_point(point).unwrap();
            assert_eq!(
                point,
                ResumePoint {
                    tick: Tick(9),
                    next_phase: phase,
                    next_step: 0
                }
            );
        }
        assert_eq!(
            next_resume_point(point),
            Some(ResumePoint {
                tick: Tick(10),
                next_phase: 0,
                next_step: 0,
            })
        );
        for invalid in [
            ResumePoint {
                tick: Tick(0),
                next_phase: 6,
                next_step: 0,
            },
            ResumePoint {
                tick: Tick(0),
                next_phase: 0,
                next_step: 1,
            },
            ResumePoint {
                tick: Tick(u64::MAX),
                next_phase: 5,
                next_step: 0,
            },
        ] {
            assert_eq!(next_resume_point(invalid), None);
        }
    }

    fn key(value: &str) -> ContentKey {
        ContentKey::parse(value).expect("test key is valid")
    }

    fn spec(id: &str, priority: i32, before: &[&str], after: &[&str]) -> SystemSpec {
        SystemSpec {
            id: key(id),
            phase: Phase::Simulation,
            rule_priority: priority,
            before: before.iter().map(|value| key(value)).collect(),
            after: after.iter().map(|value| key(value)).collect(),
        }
    }

    #[test]
    fn dependencies_override_priority_and_registration_order() {
        let specs = vec![
            spec("base:ticks", 0, &[], &[]),
            spec("base:machines", 10, &[], &["base:ticks"]),
            spec("base:logistics", 5, &[], &["base:machines"]),
        ];
        let order = order_phase(&specs).expect("order succeeds");
        assert_eq!(
            order,
            vec![
                key("base:ticks"),
                key("base:machines"),
                key("base:logistics")
            ]
        );
    }

    #[test]
    fn independent_systems_order_by_priority_then_identity() {
        let specs = vec![
            spec("base:b", 5, &[], &[]),
            spec("base:c", 1, &[], &[]),
            spec("base:a", 5, &[], &[]),
        ];
        let order = order_phase(&specs).expect("order succeeds");
        assert_eq!(order, vec![key("base:c"), key("base:a"), key("base:b")]);
    }

    #[test]
    fn duplicate_identity_is_rejected() {
        let specs = vec![spec("base:a", 0, &[], &[]), spec("base:a", 1, &[], &[])];
        assert!(matches!(
            order_phase(&specs),
            Err(OrderError::Duplicate { .. })
        ));
    }

    #[test]
    fn unknown_dependency_is_rejected() {
        let specs = vec![spec("base:a", 0, &[], &["base:missing"])];
        assert!(matches!(
            order_phase(&specs),
            Err(OrderError::UnknownDependency { .. })
        ));
    }

    #[test]
    fn cross_phase_dependency_is_rejected() {
        let mut other = spec("base:other", 0, &[], &[]);
        other.phase = Phase::Publish;
        let specs = vec![spec("base:a", 0, &[], &["base:other"]), other];
        assert!(matches!(
            order_phase(&specs),
            Err(OrderError::UnknownDependency { .. })
        ));
    }

    #[test]
    fn contradictory_before_and_after_dependencies_are_rejected() {
        for reverse in [false, true] {
            let mut specs = vec![
                spec("base:a", 0, &["base:b"], &["base:b"]),
                spec("base:b", 1, &[], &[]),
            ];
            if reverse {
                specs.reverse();
            }
            match order_phase(&specs) {
                Err(OrderError::Cycle { remaining }) => {
                    assert_eq!(remaining.len(), 2);
                    assert!(remaining.contains(&key("base:a")));
                    assert!(remaining.contains(&key("base:b")));
                }
                other => panic!("expected contradictory edges to form a cycle, got {other:?}"),
            }
        }
    }

    #[test]
    fn repeated_consistent_dependencies_count_each_edge_once() {
        let specs = vec![
            spec("base:b", -1, &[], &["base:a", "base:a"]),
            spec("base:a", 1, &["base:b", "base:b"], &[]),
        ];
        assert_eq!(
            order_phase(&specs).unwrap(),
            vec![key("base:a"), key("base:b")]
        );
    }

    #[test]
    fn cycles_are_reported_with_the_remaining_systems() {
        let specs = vec![
            spec("base:a", 0, &["base:b"], &[]),
            spec("base:b", 0, &["base:a"], &[]),
        ];
        match order_phase(&specs) {
            Err(OrderError::Cycle { remaining }) => {
                assert_eq!(remaining.len(), 2);
            }
            other => panic!("expected a cycle, got {other:?}"),
        }
    }
}
