//! Host-selected replica windows inside immutable peer authorization envelopes.
use super::*;

/// Horizontal section radius and per-player section cap. Vertical sections are
/// selected from the approved envelope, nearest to the durable player first.
#[derive(Clone, Copy, Debug)]
pub struct PlayerFollow {
    pub radius: u32,
    pub max_sections: usize,
}

impl PlayerFollow {
    fn scope(self, center: SectionPos, approved: &ReplicaScope) -> ReplicaScope {
        let mut sections: Vec<_> = approved
            .sections
            .iter()
            .copied()
            .filter(|p| {
                p.x.abs_diff(center.x) <= u64::from(self.radius)
                    && p.z.abs_diff(center.z) <= u64::from(self.radius)
            })
            .collect();
        sections.sort_by_key(|p| {
            (
                u128::from(p.x.abs_diff(center.x))
                    + u128::from(p.y.abs_diff(center.y))
                    + u128::from(p.z.abs_diff(center.z)),
                *p,
            )
        });
        sections.truncate(self.max_sections);
        ReplicaScope {
            sections: sections.into_iter().collect(),
        }
    }
}

impl<T: Transport> NetworkHub<T> {
    /// Enable before accepting peers, after configuring the server's bounded
    /// residency scheduler. This never changes actor grants or session identity.
    pub fn enable_player_follow<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &mut self,
        server: &Server<ExecutionHost, J, S>,
        config: PlayerFollow,
    ) -> Result<(), Fault> {
        if !self.peers.is_empty()
            || self.follow.is_some()
            || self.binding != server.binding()
            || server.residency.is_none()
            || config.radius > 64
            || config.max_sections == 0
            || config.max_sections > server.authority()?.replica_section_limit()
        {
            return Err(Fault::invalid_data("invalid player follow configuration"));
        }
        self.follow = Some(config);
        Ok(())
    }

    pub(super) fn update_follow<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        peer: &mut Peer<T>,
        server: &mut Server<ExecutionHost, J, S>,
        config: PlayerFollow,
        limits: NetworkLimits,
    ) -> Result<(), Fault> {
        let viewer = peer
            .viewer
            .ok_or_else(|| Fault::invalid_data("follow requires a session"))?;
        let authority = server.authority()?;
        let player = authority
            .player_state(viewer.session)
            .ok_or_else(|| Fault::invalid_data("player outside approved envelope"))?;
        let center = player
            .position
            .section(authority.world().store().side())
            .ok_or_else(|| Fault::invalid_data("unrepresentable player position"))?;
        let desired = config.scope(center, &peer.policy.scope);
        if peer.follow_scope.as_ref().map(|s| &s.sections) != Some(&desired.sections) {
            peer.follow_scope = Some(desired);
            peer.follow_ready = false;
        }
        if peer.follow_ready {
            return Ok(());
        }
        let scope = peer.follow_scope.as_ref().expect("selected follow scope");
        let all_ready = scope.sections.iter().all(|pos| {
            server
                .authority()
                .expect("live server")
                .world()
                .store()
                .section(*pos)
                .is_some()
        });
        if all_ready && !peer.subscriptions.is_empty() {
            // The approved grants are unchanged during movement. Send preceding
            // ordinary updates in order, then the same-stream interest cut.
            let mut continuous = true;
            for id in peer.subscriptions.iter().copied().collect::<Vec<_>>() {
                match server.take_replica(SubscriptionId(id), limits.budget) {
                    Ok(batches) => {
                        for batch in batches {
                            Self::send(peer, ServerMessage::Batch(batch), limits)?;
                        }
                    }
                    Err(error) if error.code == FaultCode::ResetRequired => {
                        continuous = false;
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
            let scope = peer
                .follow_scope
                .as_ref()
                .expect("selected follow scope")
                .clone();
            if continuous {
                for batch in server.change_session_interest(
                    viewer.session,
                    peer.policy.actor.clone(),
                    scope,
                    limits.budget,
                )? {
                    Self::send(peer, ServerMessage::Interest(batch), limits)?;
                }
                peer.follow_ready = true;
                return Ok(());
            }
        }
        // Retire the old projection before the residency union can evict it.
        // A failed seed retains the new intention and never replays movement.
        for old in std::mem::take(&mut peer.subscriptions) {
            server.close_replica(SubscriptionId(old))?;
        }
        peer.deferred = None;
        let scope = peer.follow_scope.as_ref().expect("selected follow scope");
        if scope.sections.iter().any(|p| {
            server
                .authority()
                .expect("live server")
                .world()
                .store()
                .section(*p)
                .is_none()
        }) {
            return Ok(());
        }
        let seed = match server.open_replica(viewer.session, scope.clone(), limits.budget) {
            Ok(seed) => seed,
            Err(error) if error.code == FaultCode::ResourceLimit => return Ok(()),
            Err(error) => return Err(error),
        };
        let subscription = seed.subscription;
        if let Err(error) = Self::send(peer, ServerMessage::Seed(seed), limits) {
            server.close_replica(subscription)?;
            if error.code == FaultCode::ResourceLimit {
                return Ok(());
            }
            return Err(error);
        }
        peer.subscriptions.insert(subscription.0);
        peer.follow_ready = true;
        Ok(())
    }

    pub(super) fn update_follow_residency<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &self,
        server: &mut Server<ExecutionHost, J, S>,
    ) -> Result<(), Fault> {
        if self.follow.is_none() {
            return Ok(());
        }
        let mut desired = BTreeSet::new();
        for peer in self
            .peers
            .values()
            .filter(|p| !p.closed && p.viewer.is_some())
        {
            let scope = peer.follow_scope.as_ref().unwrap_or(&peer.policy.scope);
            for position in &scope.sections {
                if desired.len() >= 4096 && !desired.contains(position) {
                    return Ok(());
                }
                desired.insert(*position);
            }
        }
        match server.set_residency_interest(&desired.into_iter().collect::<Vec<_>>()) {
            // Preserve the previous union on capacity denial; the next poll
            // recomputes it, including disconnects or trusted policy changes.
            Err(error) if error.code == FaultCode::ResourceLimit => Ok(()),
            result => result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_selection_is_bounded_deterministic_and_overflow_safe() {
        let center = SectionPos::new(i64::MAX, -2, i64::MIN);
        let approved = ReplicaScope {
            sections: [
                center,
                SectionPos::new(i64::MIN, 0, i64::MAX),
                SectionPos::new(i64::MAX - 1, 0, i64::MIN),
            ]
            .into(),
        };
        let config = PlayerFollow {
            radius: 1,
            max_sections: 1,
        };
        assert_eq!(config.scope(center, &approved).sections, [center].into());
        let selected = PlayerFollow {
            max_sections: 4,
            ..config
        }
        .scope(center, &approved);
        assert_eq!(selected.sections.len(), 2);
        assert!(selected.sections.is_subset(&approved.sections));
    }
}
