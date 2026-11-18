//! Host-approved display ranges never alter actor grants or residency intention.
use super::*;
use crate::{
    section_pages::SectionPages,
    visual::{VisualSession, VisualStart, VisualTerrainApproval, VisualTerrainLimits},
};

/// Limits for one optional display service call. Each visited session can send
/// at most one frame, whose bytes/pages/control size retain their own limits.
/// A light admission is an atomic operation bounded by that session's window
/// and lighting configuration; the host must budget for that unit separately.
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayWorkBudget {
    pub sessions: usize,
    pub light_admissions: usize,
    pub metadata_checks_per_session: usize,
    pub light_checks_per_session: usize,
    pub lighting: crate::lighting::HostLightWorkBudget,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayWorkProgress {
    pub sessions: usize,
    pub light_admissions: usize,
    pub metadata_checks: usize,
    pub light_checks: usize,
    pub lighting: crate::lighting::HostLightProgress,
}

impl<T: Transport> NetworkHub<T> {
    /// Opt-in shared field. Configure before enabling any display sessions.
    pub fn enable_shared_lighting<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &mut self,
        server: &Server<ExecutionHost, J, S>,
        pages: SectionPages,
        limits: crate::lighting::HostLightLimits,
        work_per_poll: usize,
    ) -> Result<(), Fault> {
        self.configure_shared_lighting(server, pages, limits, work_per_poll, None)
    }

    /// Opt in with an immutable table whose identity clients configure locally.
    pub fn enable_shared_lighting_with_optics<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &mut self,
        server: &Server<ExecutionHost, J, S>,
        pages: SectionPages,
        limits: crate::lighting::HostLightLimits,
        work_per_poll: usize,
        optics: std::sync::Arc<torse_world_services::lighting::LightOpticsTable>,
    ) -> Result<(), Fault> {
        self.configure_shared_lighting(server, pages, limits, work_per_poll, Some(optics))
    }

    fn configure_shared_lighting<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &mut self,
        server: &Server<ExecutionHost, J, S>,
        pages: SectionPages,
        limits: crate::lighting::HostLightLimits,
        work_per_poll: usize,
        optics: Option<std::sync::Arc<torse_world_services::lighting::LightOpticsTable>>,
    ) -> Result<(), Fault> {
        if self.lighting.is_some()
            || self.peers.values().any(|p| p.visual.is_some())
            || work_per_poll == 0
            || work_per_poll > 1_000_000
            || server.binding() != self.binding
        {
            return Err(Fault::invalid_data(
                "shared lighting must precede display sessions",
            ));
        }
        let host = server.authority()?;
        let field = match optics {
            Some(table) => {
                crate::lighting::HostLightField::new_with_optics(host, pages, limits, table)
            }
            None => crate::lighting::HostLightField::new(host, pages, limits),
        }?;
        self.lighting = Some((field, work_per_poll));
        Ok(())
    }
    pub fn shared_light_diagnostics(
        &self,
    ) -> Option<torse_world_services::lighting::LightFieldDiagnostics> {
        self.lighting.as_ref().map(|(f, _)| f.diagnostics())
    }
    pub fn shared_light_resources(&self) -> Option<crate::lighting::HostLightDiagnostics> {
        self.lighting
            .as_ref()
            .map(|(field, _)| field.resource_diagnostics())
    }
    pub fn visual_terrain_diagnostics(
        &self,
        id: u128,
    ) -> Option<crate::visual::VisualTerrainDiagnostics> {
        self.peers
            .get(&id)?
            .visual
            .as_ref()
            .map(VisualSession::diagnostics)
    }
    /// Enable once for this connection. The approved display envelope is
    /// immutable: revoke the connection to tighten it. No entity/interaction
    /// authorization is added. Delivery begins after a normal subscription.
    /// Pass a compact approval for a large rectangle. Only `desired` uses the
    /// configured window page budget.
    #[allow(clippy::too_many_arguments)]
    pub fn enable_visual_interest<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &mut self,
        server: &Server<ExecutionHost, J, S>,
        id: u128,
        pages: SectionPages,
        approved: impl Into<VisualTerrainApproval>,
        desired: ReplicaScope,
        center: SectionPos,
        limits: VisualTerrainLimits,
    ) -> Result<(), Fault> {
        if server.binding() != self.binding {
            return Err(Fault::invalid_data("visual binding mismatch"));
        }
        let peer = self
            .peers
            .get_mut(&id)
            .ok_or_else(|| Fault::invalid_data("unknown visual peer"))?;
        if peer.closed || peer.visual.is_some() {
            return Err(Fault::invalid_data(
                "visual approval already set or peer closed",
            ));
        }
        let visual = VisualSession::new(
            server.authority()?,
            pages,
            approved.into(),
            desired,
            VisualStart {
                center,
                generation: peer
                    .visual_generation_floor
                    .checked_add(1)
                    .ok_or_else(|| Fault::resource_limit("visual generation exhausted"))?,
            },
            limits,
            self.limits,
        )?;
        if let Some((field, _)) = &mut self.lighting {
            field.subscribe_near(id, &visual.desired_positions(), center)?;
        }
        peer.visual = Some(visual);
        peer.light_interest_pending = false;
        Ok(())
    }
    /// Move inside the immutable display approval. Pending output stays ordered;
    /// superseded background results are discarded, not applied to the new view.
    pub fn set_visual_interest(
        &mut self,
        id: u128,
        scope: ReplicaScope,
        center: SectionPos,
    ) -> Result<(), Fault> {
        self.set_visual_interest_inner(id, scope, center, false)
    }
    /// Admit the terrain request now, but coalesce optional light admission to
    /// the newest view when the host next grants a display-work admission unit.
    /// Scope/proof control-frame checks still precede terrain mutation.
    pub fn set_visual_interest_deferred(
        &mut self,
        id: u128,
        scope: ReplicaScope,
        center: SectionPos,
    ) -> Result<(), Fault> {
        self.set_visual_interest_inner(id, scope, center, true)
    }
    fn set_visual_interest_inner(
        &mut self,
        id: u128,
        scope: ReplicaScope,
        center: SectionPos,
        defer: bool,
    ) -> Result<(), Fault> {
        let peer = self
            .peers
            .get_mut(&id)
            .ok_or_else(|| Fault::invalid_data("unknown visual peer"))?;
        if peer.closed {
            return Err(Fault::invalid_data("closed visual peer"));
        }
        let visual = peer
            .visual
            .as_mut()
            .ok_or_else(|| Fault::invalid_data("display terrain not enabled"))?;
        visual.check_request(&scope)?;
        if !defer && let Some((field, _)) = &mut self.lighting {
            field.subscribe_near(
                id,
                &scope.sections.iter().copied().collect::<Vec<_>>(),
                center,
            )?;
        }
        visual.request_admitted(scope, center, false)?;
        peer.light_interest_pending = defer && self.lighting.is_some();
        Ok(())
    }
    /// Explicitly retry missing/corrupt/budget-denied sources at the current view.
    pub fn refresh_visual_interest(&mut self, id: u128) -> Result<(), Fault> {
        let peer = self
            .peers
            .get_mut(&id)
            .ok_or_else(|| Fault::invalid_data("unknown visual peer"))?;
        if peer.closed {
            return Err(Fault::invalid_data("closed visual peer"));
        }
        if let Some((field, _)) = &mut self.lighting {
            field.refresh()?;
        }
        peer.visual
            .as_mut()
            .ok_or_else(|| Fault::invalid_data("display terrain not enabled"))?
            .refresh()
    }
    pub(super) fn poll_visual<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        id: u128,
        peer: &mut Peer<T>,
        server: &Server<ExecutionHost, J, S>,
        limits: NetworkLimits,
        lighting: Option<&crate::lighting::HostLightField>,
        metadata_checks: usize,
        light_checks: usize,
    ) -> Result<(), Fault> {
        if peer.subscriptions.is_empty() {
            return Ok(());
        }
        if let Some(mut visual) = peer.visual.take() {
            let update = if metadata_checks == usize::MAX {
                visual.poll(server.authority()?, peer.transport.output_idle())
            } else {
                visual.poll_with_budget(
                    server.authority()?,
                    peer.transport.output_idle(),
                    metadata_checks,
                )
            };
            if let Some(mut update) = update? {
                if let Some(lighting) = lighting {
                    visual.attach_lights(id, lighting, server.authority()?, &mut update);
                }
                let inline = update.pages.iter().any(|page| page.light.is_some());
                let mut message = ServerMessage::Terrain(update);
                if inline
                    && let Err(error) =
                        crate::visual::check_frame(&message, limits.budget, limits.codec)
                {
                    if error.code != FaultCode::ResourceLimit {
                        return Err(error);
                    }
                    let ServerMessage::Terrain(update) = &mut message else {
                        unreachable!()
                    };
                    visual.defer_inline_lights(update);
                }
                Self::send(peer, message, limits)?;
            } else if peer.transport.output_idle()
                && let Some(lighting) = lighting
                && let Some(update) = visual.light_update(
                    id,
                    lighting,
                    server.authority()?,
                    limits.budget,
                    limits.codec,
                    light_checks,
                )?
            {
                Self::send(peer, ServerMessage::Light(update), limits)?;
            }
            peer.visual = Some(visual);
        }
        Ok(())
    }

    /// Advance optional display work independently from control/durable polls.
    /// Metadata budgets count source/target inspections, not elapsed time; page
    /// validation, solver preparation and frame encoding are non-preemptible
    /// units bounded by their existing size/geometry limits. Zero freezes the
    /// corresponding stage. Stalled peers consume no publication work.
    ///
    /// ```no_run
    /// use torse_server::{bootstrap, lighting::HostLightWorkBudget, network::*};
    /// fn service(hub: &mut NetworkHub<torse_net::MemoryTransport>, server: &mut bootstrap::Files)
    ///     -> Result<(), torse_types::Fault>
    /// {
    ///     // A host may drain durable/control output repeatedly between batches.
    ///     for _ in 0..17 { hub.poll_control(server)?; }
    ///     hub.poll_display(server, DisplayWorkBudget {
    ///         sessions: 2, light_admissions: 1,
    ///         metadata_checks_per_session: 64, light_checks_per_session: 16,
    ///         lighting: HostLightWorkBudget {
    ///             input_checks: 64, validation_checks: 8,
    ///             readiness_checks: 16, solver_work: 20_000,
    ///         },
    ///     })?;
    ///     Ok(())
    /// }
    /// ```
    pub fn poll_display<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &mut self,
        server: &Server<ExecutionHost, J, S>,
        budget: DisplayWorkBudget,
    ) -> Result<DisplayWorkProgress, Fault> {
        if server.binding() != self.binding {
            return Err(Fault::invalid_data("display hub binding mismatch"));
        }
        let mut progress = DisplayWorkProgress::default();
        if let Some((field, _)) = &mut self.lighting {
            match field.poll_with_budget(server.authority()?, budget.lighting) {
                Ok(value) => progress.lighting = value,
                Err(error) if error.code == FaultCode::ResourceLimit => {}
                Err(error) => return Err(error),
            }
        }
        use std::ops::Bound::{Excluded, Unbounded};
        let start = self.display_cursor.map_or(Unbounded, Excluded);
        let ids: Vec<_> = self
            .peers
            .range((start, Unbounded))
            .chain(self.peers.iter())
            .take(budget.sessions.min(self.peers.len()))
            .map(|(&id, _)| id)
            .collect();
        for id in ids {
            self.display_cursor = Some(id);
            progress.sessions += 1;
            let peer = self.peers.get_mut(&id).expect("display peer exists");
            if peer.closed
                || peer.subscriptions.is_empty()
                || peer.visual.is_none()
                || !peer.transport.output_idle()
            {
                continue;
            }
            if peer.light_interest_pending && progress.light_admissions < budget.light_admissions {
                if let Some((field, _)) = &mut self.lighting {
                    let visual = peer.visual.as_ref().expect("display session");
                    field.subscribe_near(id, &visual.desired_positions(), visual.center())?;
                    progress.light_admissions += 1;
                }
                peer.light_interest_pending = false;
            }
            peer.codec = self.codec.take();
            let result = Self::poll_visual(
                id,
                peer,
                server,
                self.limits,
                self.lighting.as_ref().map(|(f, _)| f),
                budget.metadata_checks_per_session,
                budget.light_checks_per_session,
            );
            self.codec = peer.codec.take();
            if let Some(visual) = &peer.visual {
                let diagnostics = visual.diagnostics();
                progress.metadata_checks += diagnostics.metadata_scanned;
                progress.light_checks += diagnostics.light_scanned;
            }
            if result.is_err() {
                peer.closed = true;
                peer.transport.close();
            }
        }
        Ok(progress)
    }
}
