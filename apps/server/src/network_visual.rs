//! Host-approved display ranges never alter actor grants or residency intention.
use super::*;
use crate::{
    section_pages::SectionPages,
    visual::{VisualSession, VisualTerrainApproval, VisualTerrainLimits},
};

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
        self.lighting = Some((
            crate::lighting::HostLightField::new(server.authority()?, pages, limits)?,
            work_per_poll,
        ));
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
        if self.lighting.is_some() {
            let cells = u64::from(server.authority()?.world().store().side()).pow(3);
            if limits.batch_bytes < 128 + 2 * cells
                || self.limits.budget.max_bytes < 256 + 2 * cells
                || u64::from(self.limits.budget.max_items) < 256 + 2 * cells
            {
                return Err(Fault::resource_limit(
                    "display budget cannot fit one light section",
                ));
            }
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
            center,
            limits,
        )?;
        if let Some((field, _)) = &mut self.lighting {
            field.subscribe(id, &visual.desired_positions())?;
        }
        peer.visual = Some(visual);
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
        if let Some((field, _)) = &mut self.lighting {
            field.subscribe(id, &scope.sections.iter().copied().collect::<Vec<_>>())?;
        }
        visual.request(scope, center, false)
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
    ) -> Result<(), Fault> {
        if peer.subscriptions.is_empty() {
            return Ok(());
        }
        if let Some(mut visual) = peer.visual.take() {
            let update = visual.poll(server.authority()?, peer.transport.output_idle());
            if let Some(mut update) = update? {
                if let Some(lighting) = lighting {
                    visual.attach_lights(id, lighting, server.authority()?, &mut update);
                }
                Self::send(peer, ServerMessage::Terrain(update), limits)?;
            } else if peer.transport.output_idle()
                && let Some(lighting) = lighting
                && let Some(update) = visual.light_update(id, lighting, server.authority()?)?
            {
                Self::send(peer, ServerMessage::Light(update), limits)?;
            }
            peer.visual = Some(visual);
        }
        Ok(())
    }
}
