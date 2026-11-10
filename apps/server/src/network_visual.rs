//! Host-approved display ranges never alter actor grants or residency intention.
use super::*;
use crate::{
    section_pages::SectionPages,
    visual::{VisualSession, VisualTerrainLimits},
};

impl<T: Transport> NetworkHub<T> {
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
    #[allow(clippy::too_many_arguments)]
    pub fn enable_visual_interest<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        &mut self,
        server: &Server<ExecutionHost, J, S>,
        id: u128,
        pages: SectionPages,
        approved: ReplicaScope,
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
        peer.visual = Some(VisualSession::new(
            server.authority()?,
            pages,
            approved,
            desired,
            center,
            limits,
        )?);
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
        peer.visual
            .as_mut()
            .ok_or_else(|| Fault::invalid_data("display terrain not enabled"))?
            .request(scope, center, false)
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
        peer.visual
            .as_mut()
            .ok_or_else(|| Fault::invalid_data("display terrain not enabled"))?
            .refresh()
    }
    pub(super) fn poll_visual<
        J: CommitJournal<ExecutionModel>,
        S: HostCheckpointStore<ExecutionModel>,
    >(
        peer: &mut Peer<T>,
        server: &Server<ExecutionHost, J, S>,
        limits: NetworkLimits,
    ) -> Result<(), Fault> {
        if peer.subscriptions.is_empty() {
            return Ok(());
        }
        if let Some(mut visual) = peer.visual.take() {
            let update = visual.poll(server.authority()?, peer.transport.output_idle());
            peer.visual = Some(visual);
            if let Some(update) = update? {
                Self::send(peer, ServerMessage::Terrain(update), limits)?;
            }
        }
        Ok(())
    }
}
