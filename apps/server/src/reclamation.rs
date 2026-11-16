//! Reference tracing is serialized with authority and checkpoint publication.
use std::collections::BTreeSet;
use torse_codec::StorageCodec;
use torse_types::{Digest, Fault, FaultCode};

use crate::execution::{ExecutionHost, ExecutionModel};
use crate::section_pages::{PageCollection, SectionPages};
use crate::{FileServer, HostAuthority};

impl<C: StorageCodec<ExecutionModel> + Clone> FileServer<ExecutionHost, C> {
    /// Collect pages in this world's exclusively owned namespace. Capture the current
    /// authority, retain checkpoint roots, reclaim covered logs, then trace every surviving
    /// checkpoint and replay reference before the first page is removed.
    pub fn collect_section_pages(
        &mut self,
        pages: &SectionPages,
        retain: usize,
    ) -> Result<PageCollection, Fault> {
        if retain == 0
            || self.parking.is_some()
            || !self.authority()?.residency_ready(false)
            || self
                .residency
                .as_ref()
                .is_some_and(|scheduler| !scheduler.is_idle())
        {
            return Err(Fault::invalid_data(
                "page collection requires positive retention and an idle tick boundary",
            ));
        }
        self.save_checkpoint_retaining(retain)?;
        self.prune_checkpoints(retain)?;
        let mut references = BTreeSet::<Digest>::new();
        let mut trace = |digest| {
            if references.len() >= pages.max_files() && !references.contains(&digest) {
                return Err(Fault::resource_limit("page reference capacity exceeded"));
            }
            references.insert(digest);
            Ok(())
        };
        self.checkpoints.visit_retained(|snapshot| {
            for page in snapshot.state.parked.values() {
                trace(page.digest)?;
            }
            Ok(())
        })?;
        self.journal.visit_retained(|record| {
            for section in record
                .changes
                .sections
                .iter()
                .filter(|section| !section.load)
            {
                trace(section.reference.digest)?;
            }
            if let Some(section) = record.changes.section
                && !section.fresh
            {
                trace(section.reference.digest)?;
            }
            Ok(())
        })?;
        // All persisted roots have been checked. The live capture includes parked pages;
        // pending residency plans were excluded before checkpointing.
        if !self.authority.at_tick_boundary() {
            return Err(Fault::new(
                FaultCode::InvalidState,
                "authority moved during page tracing",
            ));
        }
        pages.collect(&references)
    }
}
