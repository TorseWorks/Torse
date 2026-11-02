use crate::{PrepareError, Transaction};
use torse_types::RejectCode;
use torse_world::{ComponentSchemas, EntityStore, SectionPhase, SectionStore};

pub(crate) fn validate(
    store: &SectionStore,
    entities: &EntityStore,
    schemas: &ComponentSchemas,
    tx: &Transaction,
) -> Result<(), PrepareError> {
    torse_world::presentation_budget(&tx.presentation, torse_world::MAX_PRESENTATION_PER_PRODUCER)
        .map_err(|_| PrepareError::Rejected(RejectCode::QuotaExceeded))?;
    for draft in &tx.presentation {
        schemas
            .validate_presentation(&tx.source, draft)
            .map_err(|_| PrepareError::Rejected(RejectCode::InvalidData))?;
        if !matches!(
            store.phase(draft.section),
            Some(SectionPhase::Ready | SectionPhase::Active)
        ) {
            return Err(PrepareError::Rejected(RejectCode::NotLoaded));
        }
        if let Some(id) = draft.recipient {
            let entity = entities
                .handle_of(id)
                .and_then(|h| entities.get(h))
                .ok_or(PrepareError::Rejected(RejectCode::MissingTarget))?;
            if entity.position.section(store.side()) != Some(draft.section) {
                return Err(PrepareError::Rejected(RejectCode::InvalidData));
            }
        }
    }
    Ok(())
}
