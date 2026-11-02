//! Transient, commit-coupled presentation data. This is never authoritative state.
use torse_content::ComponentField;
use torse_types::{CommitCursor, ContentKey, EntityId, Fault, SectionPos, WorldEpoch};

use crate::{ComponentError, ComponentFields, ComponentSchemas, component_size};

pub const MAX_PRESENTATION_PER_PRODUCER: usize = 16;
pub const MAX_PRESENTATION_PER_COMMIT: usize = 32;
pub const MAX_PRESENTATION_BYTES: u64 = 4096;
pub const MAX_PRESENTATION_COMMIT_BYTES: u64 = 32768;

/// Trusted startup policy, independent of component visibility. An authorized
/// kind can never be downgraded to public by changing a draft's recipient.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PresentationVisibility {
    Public,
    Authorized { permission: ContentKey },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentationSchema {
    pub id: ContentKey,
    pub version: u32,
    pub owner: ContentKey,
    pub visibility: PresentationVisibility,
    pub fields: Vec<ComponentField>,
}

/// Anchor is checked against the readable pre-commit world. Authorized drafts
/// also name an existing entity at that anchor. Fields are explicitly authored;
/// no component, inventory or transaction metadata is copied implicitly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentationDraft {
    pub kind: ContentKey,
    pub version: u32,
    pub section: SectionPos,
    pub recipient: Option<EntityId>,
    pub fields: ComponentFields,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PresentationId {
    pub epoch: WorldEpoch,
    pub cursor: CommitCursor,
    /// Index in canonical source/member order, including events filtered out.
    pub index: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentationEvent {
    pub id: PresentationId,
    pub draft: PresentationDraft,
}

/// Check nested values before cloning or traversing them elsewhere. Size also
/// charges identity keys, envelope, recipient and anchor conservatively.
pub fn presentation_size(draft: &PresentationDraft) -> Result<u64, Fault> {
    if draft.fields.len() > 32
        || draft.fields.values().any(|value| {
            matches!(
                value,
                crate::ComponentValue::List(_) | crate::ComponentValue::Record(_)
            )
        })
    {
        return Err(Fault::invalid_data(
            "presentation requires flat primitive fields",
        ));
    }
    let bytes = component_size(&draft.fields)
        .map_err(|_| Fault::resource_limit("invalid presentation payload"))?
        .checked_add(128 + draft.kind.namespace().len() as u64 + draft.kind.path().len() as u64)
        .filter(|n| *n <= MAX_PRESENTATION_BYTES)
        .ok_or_else(|| Fault::resource_limit("presentation payload budget"))?;
    if draft.version == 0 || draft.recipient.is_some_and(|id| id.0 == 0) {
        return Err(Fault::invalid_data("invalid presentation identity"));
    }
    Ok(bytes)
}

pub fn presentation_budget<'a>(
    drafts: impl IntoIterator<Item = &'a PresentationDraft>,
    max_count: usize,
) -> Result<u64, Fault> {
    let mut total = 0u64;
    for (index, draft) in drafts.into_iter().enumerate() {
        if index >= max_count {
            return Err(Fault::resource_limit("presentation count budget"));
        }
        total = total
            .checked_add(presentation_size(draft)?)
            .filter(|n| *n <= MAX_PRESENTATION_COMMIT_BYTES)
            .ok_or_else(|| Fault::resource_limit("presentation commit budget"))?;
    }
    Ok(total)
}

impl ComponentSchemas {
    /// Called only by trusted native startup before authority assembly. This
    /// declaration is separate from components and participates in compatibility.
    pub fn register_presentation(
        &mut self,
        schema: PresentationSchema,
    ) -> Result<(), ComponentError> {
        if self.compatibility.is_some() {
            return Err(ComponentError::ConfigurationFrozen);
        }
        // Flat primitive fields make the disclosure boundary explicit. Opaque
        // records and lists cannot smuggle unreviewed instance data into a cue.
        if schema.version == 0
            || schema.fields.len() > 32
            || self.presentation.len() >= 256
            || schema.fields.iter().any(|f| {
                matches!(
                    f.field_type,
                    torse_content::ComponentFieldType::List { .. }
                        | torse_content::ComponentFieldType::Record
                )
            })
            || self.presentation.contains_key(&schema.id)
            || schema
                .fields
                .iter()
                .map(|f| &f.id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != schema.fields.len()
        {
            return Err(ComponentError::InvalidConfiguration { id: schema.id });
        }
        self.presentation.insert(schema.id.clone(), schema);
        Ok(())
    }

    pub fn presentation(&self, id: &ContentKey) -> Option<&PresentationSchema> {
        self.presentation.get(id)
    }

    pub fn validate_presentation(
        &self,
        source: &ContentKey,
        draft: &PresentationDraft,
    ) -> Result<(), Fault> {
        presentation_size(draft)?;
        let schema = self
            .presentation(&draft.kind)
            .ok_or_else(|| Fault::invalid_data("unknown presentation kind"))?;
        if &schema.owner != source
            || schema.version != draft.version
            || matches!(schema.visibility, PresentationVisibility::Authorized { .. })
                != draft.recipient.is_some()
        {
            return Err(Fault::invalid_data(
                "presentation owner, version or audience mismatch",
            ));
        }
        for (id, value) in &draft.fields {
            let field = schema
                .fields
                .iter()
                .find(|f| &f.id == id)
                .ok_or_else(|| Fault::invalid_data("undeclared presentation field"))?;
            crate::component::validate_value(id, field.field_type, value, 0)
                .map_err(|_| Fault::invalid_data("invalid presentation field"))?;
        }
        if schema
            .fields
            .iter()
            .any(|f| f.required && !draft.fields.contains_key(&f.id))
        {
            return Err(Fault::invalid_data("missing presentation field"));
        }
        Ok(())
    }
}
