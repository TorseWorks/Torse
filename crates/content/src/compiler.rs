//! Deterministic validation and compilation for the M0 content fixture.
//!
//! Compilation validates names, references, finite-state bounds, geometry,
//! actions, and modules, then normalizes unordered collections before producing
//! stable digests. Invalid packs return all independently detectable diagnostics
//! and never expose a partially compiled registry.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sha2::{Digest as ShaDigest, Sha256};
use torse_actions::{ActionDescriptor, ActionKind, validate_actions};
use torse_types::{ContentKey, Digest};

use crate::descriptor::{
    AlphaMode, BlockDefinition, CapabilityDescriptor, ComponentFieldType, ComponentSchema,
    ExecutionContract, Face, ModuleDescriptor, ModuleSide, PersistencePolicy, Shape, StateSchema,
};

/// Version of the canonical content model that participates in every digest.
///
/// Changing the meaning or byte encoding of an existing field requires bumping
/// this value so consumers cannot confuse old and new summaries.
pub const CONTENT_FORMAT_VERSION: u32 = 1;

/// Upper bound on the Cartesian product of finite state fields.
///
/// The limit prevents a small source description from expanding into an
/// unbounded registry. It is a safety boundary, not a gameplay tuning value.
pub const MAX_FINITE_STATES: u64 = 4096;

/// Source location retained for diagnostics.
///
/// M0 consumes structured fixture data rather than parsed files, but preserving
/// a source label here lets a future parser reuse the same diagnostics.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SourceRef {
    /// Human-readable source identifier, usually a relative asset path.
    pub source: String,
    /// One-based line number, or zero for generated/compiled diagnostics.
    pub line: u32,
}

impl SourceRef {
    /// Construct a diagnostic location. `line` may be zero for generated data.
    pub fn new(source: impl Into<String>, line: u32) -> Self {
        Self {
            source: source.into(),
            line,
        }
    }
}

/// A definition paired with the location that produced it.
#[derive(Clone, Debug)]
pub struct Sourced<T> {
    /// Origin used when the wrapped value produces a diagnostic.
    pub source: SourceRef,
    /// Definition value supplied by the source.
    pub value: T,
}

impl<T> Sourced<T> {
    /// Wrap a value and its diagnostic origin.
    pub fn new(source: SourceRef, value: T) -> Self {
        Self { source, value }
    }
}

/// Uncompiled content input.
///
/// The vectors are treated as unordered source collections. Compilation sorts
/// semantically unordered nested data before digesting it, so loader or file
/// enumeration order cannot change the resulting summaries.
#[derive(Clone, Debug, Default)]
pub struct ContentPack {
    /// State and component schema declarations.
    pub state_schemas: Vec<Sourced<StateSchema>>,
    /// Gameplay capability declarations.
    pub capabilities: Vec<Sourced<CapabilityDescriptor>>,
    /// Typed action declarations.
    pub actions: Vec<Sourced<ActionDescriptor>>,
    /// Module dependency and registration declarations.
    pub modules: Vec<Sourced<ModuleDescriptor>>,
    /// Static block definitions.
    pub blocks: Vec<Sourced<BlockDefinition>>,
}

/// Frozen content registry produced only after every validation succeeds.
///
/// The authority digest covers simulation meaning. The presentation digest
/// covers visual resources and may change independently when only appearance
/// changes.
#[derive(Clone, Debug, Default)]
pub struct CompiledContent {
    /// Validated schemas keyed by stable identity.
    pub state_schemas: BTreeMap<ContentKey, StateSchema>,
    /// Validated capabilities keyed by stable identity.
    pub capabilities: BTreeMap<ContentKey, CapabilityDescriptor>,
    /// Validated actions keyed by stable identity.
    pub actions: BTreeMap<ContentKey, ActionDescriptor>,
    /// Validated modules keyed by stable identity.
    pub modules: BTreeMap<ContentKey, ModuleDescriptor>,
    /// Validated blocks keyed by stable identity.
    pub blocks: BTreeMap<ContentKey, BlockDefinition>,
    /// Digest of authoritative simulation behavior and compatibility data.
    pub authority_digest: Digest,
    /// Digest of client-visible presentation resources.
    pub presentation_digest: Digest,
}

/// One validation failure with source, content ID, and field context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentDiagnostic {
    /// Source location that produced the failure.
    pub source: SourceRef,
    /// Content identity associated with the failure, when one exists.
    pub content_id: Option<ContentKey>,
    /// Field path within the content declaration.
    pub field: String,
    /// Human-readable explanation and expected constraint.
    pub message: String,
}

impl fmt::Display for ContentDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.source.source, self.source.line)?;
        if let Some(content_id) = &self.content_id {
            write!(f, " [{content_id}]")?;
        }
        write!(f, " {}: {}", self.field, self.message)
    }
}

impl Ord for ContentDiagnostic {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (&self.source, &self.content_id, &self.field, &self.message).cmp(&(
            &other.source,
            &other.content_id,
            &other.field,
            &other.message,
        ))
    }
}

impl PartialOrd for ContentDiagnostic {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Validate and compile a content pack.
///
/// Validation continues after individual failures so callers receive as many
/// actionable diagnostics as possible. Only a fully valid pack is converted
/// into [`CompiledContent`].
///
/// The returned registry and digests are independent of source enumeration
/// order. Definitions are keyed by [`ContentKey`], unordered nested lists are
/// normalized, and digest encoding uses explicit lengths and fixed-width
/// little-endian primitives.
pub fn compile(pack: ContentPack) -> Result<CompiledContent, Vec<ContentDiagnostic>> {
    let mut diagnostics = Vec::new();
    let mut state_schemas = collect_unique(pack.state_schemas, |value| &value.id, &mut diagnostics);
    let mut capabilities = collect_unique(pack.capabilities, |value| &value.id, &mut diagnostics);
    let mut actions = collect_unique(pack.actions, |value| &value.id, &mut diagnostics);
    let mut modules = collect_unique(pack.modules, |value| &value.id, &mut diagnostics);

    // Validate definitions before cross-references. A malformed schema should
    // be reported as malformed rather than only as an unknown reference from
    // every block that uses it.
    for sourced in state_schemas.values() {
        validate_state_schema(&sourced.source, &sourced.value, &mut diagnostics);
    }
    for sourced in capabilities.values() {
        validate_capability(&sourced.source, &sourced.value, &mut diagnostics);
    }
    for sourced in modules.values() {
        validate_module(&sourced.source, &sourced.value, &mut diagnostics);
    }
    // Action validation also checks command ownership across the whole pack.
    // Reattach source locations because the action registry deliberately has no
    // dependency on content-file diagnostics.
    match validate_actions(
        actions
            .values()
            .map(|sourced| sourced.value.clone())
            .collect(),
    ) {
        Ok(_) => {}
        Err(action_errors) => {
            for error in action_errors {
                let source = actions
                    .get(&error.action)
                    .map(|sourced| sourced.source.clone())
                    .unwrap_or_else(|| SourceRef::new("compiled-registry", 0));
                diagnostics.push(diagnostic(source, error.action, error.field, error.message));
            }
        }
    }

    // Components are declared inside schemas but referenced globally. Preserve
    // the declaring schema so duplicate ownership can be diagnosed even when it
    // appears in two different source files.
    let mut component_ids = BTreeMap::<ContentKey, ContentKey>::new();
    for sourced in state_schemas.values() {
        for component in &sourced.value.component_schemas {
            if component.id.to_string() == "core:schema/block_instance" {
                diagnostics.push(diagnostic(
                    sourced.source.clone(),
                    sourced.value.id.clone(),
                    "component_schemas.id",
                    "block instance bindings are reserved by the kernel",
                ));
            }
            if let Some(previous) =
                component_ids.insert(component.id.clone(), sourced.value.id.clone())
            {
                diagnostics.push(diagnostic(
                    sourced.source.clone(),
                    sourced.value.id.clone(),
                    "component_schemas.id",
                    format!("component {} is also declared by {previous}", component.id),
                ));
            }
        }
    }

    validate_module_references(
        &modules,
        &state_schemas,
        &capabilities,
        &actions,
        &mut diagnostics,
    );

    // Normalize each block before insertion. Validation is order-insensitive,
    // and digest encoding must not depend on the order of source vectors.
    let mut blocks = BTreeMap::new();
    let mut block_sources = BTreeMap::new();
    for sourced in pack.blocks {
        let source = sourced.source;
        let mut block = sourced.value;
        normalize_block(&mut block);
        let id = block.id.clone();
        validate_block(
            &block,
            &source,
            &state_schemas,
            &capabilities,
            &component_ids,
            &mut diagnostics,
        );
        if blocks.insert(id.clone(), block).is_some() {
            diagnostics.push(diagnostic(
                source.clone(),
                id.clone(),
                "id",
                "duplicate block definition",
            ));
        }
        block_sources.insert(id, source);
    }

    // Completion targets are block references; check them after every block is
    // collected so the rule does not depend on source order.
    for (id, block) in &blocks {
        if let Some(target) = &block.interaction_profile.completes_to
            && !blocks.contains_key(target)
        {
            diagnostics.push(diagnostic(
                block_sources
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| SourceRef::new("compiled-registry", 0)),
                id.clone(),
                "interaction_profile.completes_to",
                format!("unknown completion target {target}"),
            ));
        }
    }

    diagnostics.sort();
    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    // Normalize only after validation succeeds. These operations change
    // representation order, not meaning, and make digest output independent of
    // JSON object iteration, filesystem enumeration, or parallel loading.
    for sourced in state_schemas.values_mut() {
        normalize_state_schema(&mut sourced.value);
    }
    for sourced in capabilities.values_mut() {
        normalize_capability(&mut sourced.value);
    }
    for sourced in actions.values_mut() {
        normalize_action(&mut sourced.value);
    }
    for sourced in modules.values_mut() {
        normalize_module(&mut sourced.value);
    }

    let plain_schemas = into_plain(state_schemas);
    let plain_capabilities = into_plain(capabilities);
    let plain_actions = into_plain(actions);
    let plain_modules = into_plain(modules);
    let authority_digest = authority_digest(
        &plain_schemas,
        &plain_capabilities,
        &plain_actions,
        &plain_modules,
        &blocks,
    );
    let presentation_digest = presentation_digest(&blocks);
    Ok(CompiledContent {
        state_schemas: plain_schemas,
        capabilities: plain_capabilities,
        actions: plain_actions,
        modules: plain_modules,
        blocks,
        authority_digest,
        presentation_digest,
    })
}

/// Collect definitions by key while retaining their original source location.
///
/// A duplicate is retained only to keep the in-memory operation deterministic;
/// the emitted diagnostic still prevents the map from being returned as
/// compiled content.
fn collect_unique<T>(
    values: Vec<Sourced<T>>,
    key: impl Fn(&T) -> &ContentKey,
    diagnostics: &mut Vec<ContentDiagnostic>,
) -> BTreeMap<ContentKey, Sourced<T>> {
    let mut output = BTreeMap::new();
    for sourced in values {
        let id = key(&sourced.value).clone();
        if let Some(existing) = output.insert(id.clone(), sourced) {
            diagnostics.push(diagnostic(
                existing.source,
                id,
                "id",
                "duplicate definition; retained one deterministic entry",
            ));
        }
    }
    output
}

/// Strip diagnostic-only source wrappers after validation succeeds.
fn into_plain<T>(values: BTreeMap<ContentKey, Sourced<T>>) -> BTreeMap<ContentKey, T> {
    values
        .into_iter()
        .map(|(id, sourced)| (id, sourced.value))
        .collect()
}

/// Validate finite-state fields and component declarations in one schema.
///
/// State cardinality is checked as a product, not per field, because individually
/// small fields can still create an impractically large registry together.
fn validate_state_schema(
    source: &SourceRef,
    schema: &StateSchema,
    diagnostics: &mut Vec<ContentDiagnostic>,
) {
    if schema.schema_version == 0 {
        diagnostics.push(diagnostic(
            source.clone(),
            schema.id.clone(),
            "schema_version",
            "schema version must be greater than zero",
        ));
    }

    let mut states = 1_u64;
    let mut field_ids = BTreeSet::new();
    for field in &schema.finite_fields {
        if !field_ids.insert(field.id.clone()) {
            diagnostics.push(diagnostic(
                source.clone(),
                schema.id.clone(),
                "finite_fields.id",
                format!("duplicate finite field {}", field.id),
            ));
        }
        if field.values.is_empty() {
            diagnostics.push(diagnostic(
                source.clone(),
                schema.id.clone(),
                "finite_fields.values",
                format!("{} has no legal values", field.id),
            ));
            continue;
        }
        if !field.values.contains(&field.default_value) {
            diagnostics.push(diagnostic(
                source.clone(),
                schema.id.clone(),
                "finite_fields.default_value",
                format!("{} default is not in its legal values", field.id),
            ));
        }
        // Duplicates are diagnosed before normalization. Sorting the values for
        // digest stability must not hide a malformed source declaration.
        let unique_values: BTreeSet<_> = field.values.iter().copied().collect();
        if unique_values.len() != field.values.len() {
            diagnostics.push(diagnostic(
                source.clone(),
                schema.id.clone(),
                "finite_fields.values",
                format!("{} contains duplicate values", field.id),
            ));
        }
        // Saturating multiplication makes overflow fail closed as "over limit"
        // rather than wrapping to a small and apparently valid state count.
        states = states.saturating_mul(unique_values.len() as u64);
    }
    if states > MAX_FINITE_STATES {
        diagnostics.push(diagnostic(
            source.clone(),
            schema.id.clone(),
            "finite_fields",
            format!("state combinations {states} exceed limit {MAX_FINITE_STATES}"),
        ));
    }

    for component in &schema.component_schemas {
        validate_component(source, schema, component, diagnostics);
    }
}

/// Validate versioning, duplicate field IDs, and bounded container fields.
fn validate_component(
    source: &SourceRef,
    schema: &StateSchema,
    component: &ComponentSchema,
    diagnostics: &mut Vec<ContentDiagnostic>,
) {
    if component.schema_version == 0 {
        diagnostics.push(diagnostic(
            source.clone(),
            schema.id.clone(),
            "component.schema_version",
            format!("{} version must be greater than zero", component.id),
        ));
    }
    let mut field_ids = BTreeSet::new();
    for field in &component.fields {
        if !field_ids.insert(field.id.clone()) {
            diagnostics.push(diagnostic(
                source.clone(),
                schema.id.clone(),
                "component.fields.id",
                format!("duplicate field {} in {}", field.id, component.id),
            ));
        }
        if let ComponentFieldType::String { max_bytes: 0 }
        | ComponentFieldType::List { max_items: 0 } = field.field_type
        {
            diagnostics.push(diagnostic(
                source.clone(),
                schema.id.clone(),
                "component.fields.field_type",
                format!("{} must have a positive container limit", field.id),
            ));
        }
    }
}

/// Validate a capability API version and the uniqueness of its method IDs.
fn validate_capability(
    source: &SourceRef,
    capability: &CapabilityDescriptor,
    diagnostics: &mut Vec<ContentDiagnostic>,
) {
    if capability.api_version == 0 {
        diagnostics.push(diagnostic(
            source.clone(),
            capability.id.clone(),
            "api_version",
            "capability API version must be greater than zero",
        ));
    }
    let mut methods = BTreeSet::new();
    for method in capability
        .query_methods
        .iter()
        .chain(&capability.mutation_methods)
    {
        if !methods.insert(method.id.clone()) {
            diagnostics.push(diagnostic(
                source.clone(),
                capability.id.clone(),
                "methods.id",
                format!("duplicate method {}", method.id),
            ));
        }
    }
}

/// Validate module versions and the module-local service declarations.
///
/// Cross-module service resolution is intentionally deferred to the module
/// resolver so this function stays independent of the complete dependency graph.
fn validate_module(
    source: &SourceRef,
    module: &ModuleDescriptor,
    diagnostics: &mut Vec<ContentDiagnostic>,
) {
    if module.module_version == 0 || module.framework_api == 0 {
        diagnostics.push(diagnostic(
            source.clone(),
            module.id.clone(),
            "version",
            "module and framework versions must be greater than zero",
        ));
    }
    let mut services = BTreeSet::new();
    for service in &module.provided_services {
        if !services.insert(service.id.clone()) {
            diagnostics.push(diagnostic(
                source.clone(),
                module.id.clone(),
                "provided_services.id",
                format!("duplicate provided service {}", service.id),
            ));
        }
        if service.api_version == 0 {
            diagnostics.push(diagnostic(
                source.clone(),
                module.id.clone(),
                "provided_services.api_version",
                format!("{} API version must be positive", service.id),
            ));
        }
    }
}

/// Resolve module registrations against the compiled content registry.
///
/// Registered schemas may refer either to a top-level state schema or to a
/// component schema nested inside one. Capabilities and actions are resolved
/// from their own global registries.
fn validate_module_references(
    modules: &BTreeMap<ContentKey, Sourced<ModuleDescriptor>>,
    schemas: &BTreeMap<ContentKey, Sourced<StateSchema>>,
    capabilities: &BTreeMap<ContentKey, Sourced<CapabilityDescriptor>>,
    actions: &BTreeMap<ContentKey, Sourced<ActionDescriptor>>,
    diagnostics: &mut Vec<ContentDiagnostic>,
) {
    let mut known_schemas: BTreeSet<_> = schemas.keys().cloned().collect();
    for schema in schemas.values() {
        known_schemas.extend(
            schema
                .value
                .component_schemas
                .iter()
                .map(|component| component.id.clone()),
        );
    }
    for module in modules.values() {
        for schema in &module.value.registered_schemas {
            if !known_schemas.contains(schema) {
                diagnostics.push(diagnostic(
                    module.source.clone(),
                    module.value.id.clone(),
                    "registered_schemas",
                    format!("unknown schema {schema}"),
                ));
            }
        }
        for capability in &module.value.registered_capabilities {
            if !capabilities.contains_key(capability) {
                diagnostics.push(diagnostic(
                    module.source.clone(),
                    module.value.id.clone(),
                    "registered_capabilities",
                    format!("unknown capability {capability}"),
                ));
            }
        }
        for action in &module.value.registered_actions {
            if !actions.contains_key(action) {
                diagnostics.push(diagnostic(
                    module.source.clone(),
                    module.value.id.clone(),
                    "registered_actions",
                    format!("unknown action {action}"),
                ));
            }
        }
    }
}

/// Validate a block and resolve all references that must exist in the pack.
///
/// Shape validation is repeated for render, collision, and selection because
/// these concerns are allowed to differ but each one must still be well formed.
fn validate_block(
    block: &BlockDefinition,
    source: &SourceRef,
    schemas: &BTreeMap<ContentKey, Sourced<StateSchema>>,
    capabilities: &BTreeMap<ContentKey, Sourced<CapabilityDescriptor>>,
    components: &BTreeMap<ContentKey, ContentKey>,
    diagnostics: &mut Vec<ContentDiagnostic>,
) {
    if block.definition_version == 0 {
        diagnostics.push(diagnostic(
            source.clone(),
            block.id.clone(),
            "definition_version",
            "definition version must be greater than zero",
        ));
    }
    if let Err(message) = block.face_support.validate() {
        diagnostics.push(diagnostic(
            source.clone(),
            block.id.clone(),
            "face_support",
            message,
        ));
    }
    if !schemas.contains_key(&block.state_schema) {
        diagnostics.push(diagnostic(
            source.clone(),
            block.id.clone(),
            "state_schema",
            format!("unknown state schema {}", block.state_schema),
        ));
    }
    if let Some(schema) = schemas.get(&block.state_schema) {
        let mut fields: Vec<_> = schema.value.finite_fields.iter().collect();
        fields.sort_by(|left, right| left.id.cmp(&right.id));
        let mut seen = BTreeSet::new();
        for profile in &block.state_overrides {
            if let Some(light) = profile.light_transport {
                for (field, value) in [
                    (
                        "state_overrides.light_transport.absorption",
                        light.absorption,
                    ),
                    ("state_overrides.light_transport.emission", light.emission),
                ] {
                    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                        diagnostics.push(diagnostic(
                            source.clone(),
                            block.id.clone(),
                            field,
                            "value must be finite and between 0 and 1",
                        ));
                    }
                }
            }
            if let Some(support) = profile.face_support
                && let Err(message) = support.validate()
            {
                diagnostics.push(diagnostic(
                    source.clone(),
                    block.id.clone(),
                    "state_overrides.face_support",
                    message,
                ));
            }
            if profile.values.len() != fields.len()
                || profile
                    .values
                    .iter()
                    .zip(&fields)
                    .any(|(value, field)| !field.values.contains(value))
                || !seen.insert(&profile.values)
            {
                diagnostics.push(diagnostic(
                    source.clone(),
                    block.id.clone(),
                    "state_overrides",
                    "state override values must be legal, canonical, and unique",
                ));
            }
            for (shape, field) in [
                (
                    profile.render_shape.as_ref(),
                    "state_overrides.render_shape",
                ),
                (
                    profile.collision_shape.as_ref(),
                    "state_overrides.collision_shape",
                ),
                (
                    profile.selection_shape.as_ref(),
                    "state_overrides.selection_shape",
                ),
            ] {
                if let Some(shape) = shape {
                    validate_shape(shape, source, &block.id, field, diagnostics);
                }
            }
            if profile.occlusion.is_some_and(|mask| {
                mask & !claimable_occlusion(
                    profile
                        .render_shape
                        .as_ref()
                        .unwrap_or(&block.render_profile.shape),
                ) != 0
            }) {
                diagnostics.push(diagnostic(
                    source.clone(),
                    block.id.clone(),
                    "state_overrides.occlusion",
                    "state occlusion exceeds proven boundary coverage",
                ));
            }
        }
    }
    validate_shape(
        &block.render_profile.shape,
        source,
        &block.id,
        "render_profile.shape",
        diagnostics,
    );
    if let Some(shape) = &block.collision_profile.shape {
        validate_shape(
            shape,
            source,
            &block.id,
            "collision_profile.shape",
            diagnostics,
        );
    }
    if let Some(shape) = &block.selection_profile.shape {
        validate_shape(
            shape,
            source,
            &block.id,
            "selection_profile.shape",
            diagnostics,
        );
    }
    if block.occlusion_profile.face_mask > 0b11_1111 {
        diagnostics.push(diagnostic(
            source.clone(),
            block.id.clone(),
            "occlusion_profile.face_mask",
            "face mask may only contain six face bits",
        ));
    }
    if block.occlusion_profile.face_mask & !claimable_occlusion(&block.render_profile.shape) != 0 {
        diagnostics.push(diagnostic(
            source.clone(),
            block.id.clone(),
            "occlusion_profile.face_mask",
            "mask claims faces the render shape does not fully cover; \
             culling would delete visible neighbour faces",
        ));
    }
    if block.render_profile.alpha_mode != AlphaMode::Opaque
        && block.occlusion_profile.face_mask != 0
    {
        diagnostics.push(diagnostic(
            source.clone(),
            block.id.clone(),
            "occlusion_profile.face_mask",
            "cutout and blended materials may not claim occlusion; \
             neighbours stay visible through them",
        ));
    }
    for (name, value) in [
        (
            "light_transport.absorption",
            block.light_transport.absorption,
        ),
        ("light_transport.emission", block.light_transport.emission),
    ] {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            diagnostics.push(diagnostic(
                source.clone(),
                block.id.clone(),
                name,
                "value must be finite and between 0 and 1",
            ));
        }
    }
    for component in &block.component_defaults {
        if !components.contains_key(component) {
            diagnostics.push(diagnostic(
                source.clone(),
                block.id.clone(),
                "component_defaults",
                format!("unknown component schema {component}"),
            ));
        }
    }
    if block
        .component_defaults
        .windows(2)
        .any(|pair| pair[0] == pair[1])
        || block.component_defaults.len() >= 64
    {
        diagnostics.push(diagnostic(
            source.clone(),
            block.id.clone(),
            "component_defaults",
            "block defaults must be unique and leave room for the instance binding",
        ));
    }
    for binding in &block.capability_bindings {
        if !capabilities.contains_key(&binding.capability) {
            diagnostics.push(diagnostic(
                source.clone(),
                block.id.clone(),
                "capability_bindings",
                format!("unknown capability {}", binding.capability),
            ));
        }
        if binding.face_mask > 0b11_1111 {
            diagnostics.push(diagnostic(
                source.clone(),
                block.id.clone(),
                "capability_bindings.face_mask",
                "capability face mask may only contain six face bits",
            ));
        }
    }
}

/// Faces a render shape may claim in an occlusion mask.
///
/// Mesh culling removes whole neighbour faces, so a claim is only sound where
/// the shape reaches the boundary and spans the face's full cross-section.
/// Partial coverage must stay unclaimed: the neighbour then keeps its face,
/// which overdraws but never deletes a visible surface.
fn claimable_occlusion(shape: &Shape) -> u8 {
    match shape {
        Shape::FullCube => 0b11_1111,
        Shape::Model(model) => model.occlusion_mask(),
        // A lower slab spans the cell footprint only at its bottom boundary.
        Shape::LowerSlab => 0b000100,
        Shape::Panel | Shape::Cross | Shape::Corner | Shape::Stair | Shape::Compound(_) => 0,
        Shape::Box(bounds) => {
            let spans = |a: usize| bounds.min[a] <= 0.0 && bounds.max[a] >= 1.0;
            let mut mask = 0u8;
            for (bit, axis, negative) in [
                (0u8, 0usize, true),
                (1, 0, false),
                (2, 1, true),
                (3, 1, false),
                (4, 2, true),
                (5, 2, false),
            ] {
                let (first, second) = match axis {
                    0 => (1, 2),
                    1 => (0, 2),
                    _ => (0, 1),
                };
                let touches = if negative {
                    bounds.min[axis] <= 0.0
                } else {
                    bounds.max[axis] >= 1.0
                };
                if touches && spans(first) && spans(second) {
                    mask |= 1 << bit;
                }
            }
            mask
        }
    }
}

/// Report a shape validation failure with the caller's field context.
fn validate_shape(
    shape: &Shape,
    source: &SourceRef,
    content_id: &ContentKey,
    field: &str,
    diagnostics: &mut Vec<ContentDiagnostic>,
) {
    if matches!(shape, Shape::Model(_)) && !field.contains("render") {
        diagnostics.push(diagnostic(
            source.clone(),
            content_id.clone(),
            field,
            "visual models cannot be used for collision or selection",
        ));
    }
    if let Err(message) = shape.validate() {
        diagnostics.push(diagnostic(
            source.clone(),
            content_id.clone(),
            field,
            message,
        ));
    }
}

/// Put semantically unordered block collections into canonical order.
fn normalize_block(block: &mut BlockDefinition) {
    block
        .state_overrides
        .sort_by(|left, right| left.values.cmp(&right.values));
    block.component_defaults.sort();
    block
        .capability_bindings
        .sort_by(|left, right| left.capability.cmp(&right.capability));
    block.behavior_bindings.sort();
    block.tags.sort();
}

/// Canonicalize finite fields, component schemas, and nested component fields.
///
/// Legal finite values are treated as a set for digest purposes. Their order has
/// no behavioral meaning after validation.
fn normalize_state_schema(schema: &mut StateSchema) {
    schema
        .finite_fields
        .sort_by(|left, right| left.id.cmp(&right.id));
    for field in &mut schema.finite_fields {
        field.values.sort_unstable();
    }
    schema
        .component_schemas
        .sort_by(|left, right| left.id.cmp(&right.id));
    for component in &mut schema.component_schemas {
        component
            .fields
            .sort_by(|left, right| left.id.cmp(&right.id));
    }
}

/// Canonicalize method and target-constraint order within a capability.
fn normalize_capability(capability: &mut CapabilityDescriptor) {
    capability
        .query_methods
        .sort_by(|left, right| left.id.cmp(&right.id));
    capability
        .mutation_methods
        .sort_by(|left, right| left.id.cmp(&right.id));
    capability.target_schema_constraints.sort();
}

/// Canonicalize action targets, commands, and command aliases.
fn normalize_action(action: &mut ActionDescriptor) {
    action.target_constraints.sort();
    action
        .commands
        .sort_by(|left, right| left.name.cmp(&right.name));
    for command in &mut action.commands {
        command.aliases.sort();
    }
}

/// Canonicalize every module collection whose order is not semantic.
///
/// Required-module ordering is resolved later, but sorting here ensures the
/// declaration digest remains stable before graph resolution occurs.
fn normalize_module(module: &mut ModuleDescriptor) {
    module.required_modules.sort();
    module.optional_modules.sort();
    module
        .provided_services
        .sort_by(|left, right| left.id.cmp(&right.id));
    module
        .required_services
        .sort_by(|left, right| left.id.cmp(&right.id));
    module.registered_schemas.sort();
    module.registered_capabilities.sort();
    module.registered_actions.sort();
    module
        .registered_systems
        .sort_by(|left, right| left.id.cmp(&right.id));
    module
        .requested_permissions
        .sort_by(|left, right| left.id.cmp(&right.id));
}

/// Hash all fields that can affect authoritative simulation behavior.
///
/// Presentation-only material choices, alpha mode, and tags are excluded so a
/// texture-only change does not invalidate a server's authority compatibility
/// digest.
fn authority_digest(
    schemas: &BTreeMap<ContentKey, StateSchema>,
    capabilities: &BTreeMap<ContentKey, CapabilityDescriptor>,
    actions: &BTreeMap<ContentKey, ActionDescriptor>,
    modules: &BTreeMap<ContentKey, ModuleDescriptor>,
    blocks: &BTreeMap<ContentKey, BlockDefinition>,
) -> Digest {
    let mut writer = CanonicalWriter::new();
    // Every digest starts with an explicit schema version. Length-prefixed keys
    // prevent ambiguous concatenations such as `a` + `bc` versus `ab` + `c`.
    writer.u32(CONTENT_FORMAT_VERSION);
    writer.u32(schemas.len() as u32);
    for schema in schemas.values() {
        writer.key(&schema.id);
        writer.u32(schema.schema_version);
        writer.u32(schema.finite_fields.len() as u32);
        for field in &schema.finite_fields {
            writer.key(&field.id);
            writer.u16(field.default_value);
            writer.u32(field.values.len() as u32);
            for value in &field.values {
                writer.u16(*value);
            }
        }
        writer.u32(schema.component_schemas.len() as u32);
        for component in &schema.component_schemas {
            writer.key(&component.id);
            writer.key(&component.owner_module);
            writer.u32(component.schema_version);
            writer.u32(component.fields.len() as u32);
            for field in &component.fields {
                writer.key(&field.id);
                writer.component_field_type(field.field_type);
                writer.bool(field.required);
            }
            writer.u8(match component.persistence {
                PersistencePolicy::Never => 0,
                PersistencePolicy::Always => 1,
                PersistencePolicy::OwnerDefined => 2,
            });
            // Persistence and replication policy are authority concerns because
            // they change what survives restart and what may be disclosed.
            writer.replication(component.replication);
            writer.optional_key(component.migration_binding.as_ref());
        }
    }
    writer.u32(capabilities.len() as u32);
    for capability in capabilities.values() {
        writer.key(&capability.id);
        writer.u32(capability.api_version);
        writer.key(&capability.access_contract);
        writer.u8(execution_tag(capability.execution_contract));
        writer.u32((capability.query_methods.len() + capability.mutation_methods.len()) as u32);
        for method in capability
            .query_methods
            .iter()
            .chain(&capability.mutation_methods)
        {
            writer.key(&method.id);
            writer.key(&method.parameter_schema);
            writer.key(&method.result_schema);
        }
        writer.u32(capability.target_schema_constraints.len() as u32);
        for constraint in &capability.target_schema_constraints {
            writer.key(constraint);
        }
    }
    // Action schemas and handlers affect what the authority may execute, even
    // though the handler implementation itself lives in another crate.
    writer.u32(actions.len() as u32);
    for action in actions.values() {
        writer.key(&action.id);
        writer.u32(action.api_version);
        writer.u8(match action.kind {
            ActionKind::Query => 0,
            ActionKind::Mutation => 1,
        });
        writer.u8(action.phase.index());
        writer.u32(action.max_items);
        writer.u64(action.max_bytes);
        writer.key(&action.parameter_schema);
        writer.key(&action.result_schema);
        writer.optional_key(action.permission.permission.as_ref());
        writer.key(&action.handler.handler);
        writer.key(&action.handler.owner_module);
        writer.u32(action.target_constraints.len() as u32);
        for target in &action.target_constraints {
            writer.key(target);
        }
        writer.u32(action.commands.len() as u32);
        for command in &action.commands {
            writer.key(&command.name);
            writer.key(&command.usage_key);
            writer.u32(command.aliases.len() as u32);
            for alias in &command.aliases {
                writer.key(alias);
            }
        }
    }
    // Module registrations affect service ownership and content composition, so
    // they belong in the authority summary rather than only in a lock file.
    writer.u32(modules.len() as u32);
    for module in modules.values() {
        writer.key(&module.id);
        writer.u32(module.module_version);
        writer.u32(module.framework_api);
        writer.u8(module_side_tag(module.side));
        writer.u32(module.required_modules.len() as u32);
        for dependency in &module.required_modules {
            writer.key(dependency);
        }
        writer.u32(module.optional_modules.len() as u32);
        for dependency in &module.optional_modules {
            writer.key(dependency);
        }
        writer.u32(module.provided_services.len() as u32);
        for service in &module.provided_services {
            writer.key(&service.id);
            writer.u32(service.api_version);
        }
        writer.u32(module.required_services.len() as u32);
        for service in &module.required_services {
            writer.key(&service.id);
            writer.u32(service.api_version);
        }
        writer.keys(&module.registered_schemas);
        writer.keys(&module.registered_capabilities);
        writer.keys(&module.registered_actions);
        writer.u32(module.registered_systems.len() as u32);
        for system in &module.registered_systems {
            writer.key(&system.id);
            writer.u8(system.phase);
            writer.u32(system.max_items);
            writer.u64(system.max_bytes);
        }
        writer.u32(module.requested_permissions.len() as u32);
        for permission in &module.requested_permissions {
            writer.key(&permission.id);
        }
        writer.optional_key(module.configuration_schema.as_ref());
        writer.optional_key(module.lifecycle_binding.as_ref());
    }
    writer.u32(blocks.len() as u32);
    for block in blocks.values() {
        writer.key(&block.id);
        writer.u32(block.definition_version);
        writer.key(&block.state_schema);
        writer.optional_shape(block.collision_profile.shape.as_ref());
        writer.optional_shape(block.selection_profile.shape.as_ref());
        writer.u8(block.occlusion_profile.face_mask);
        writer.u32(block.light_transport.absorption.to_bits());
        writer.u32(block.light_transport.emission.to_bits());
        writer.optional_key(block.interaction_profile.action.as_ref());
        writer.optional_key(block.interaction_profile.completes_to.as_ref());
        writer.u32(block.component_defaults.len() as u32);
        for component in &block.component_defaults {
            writer.key(component);
        }
        writer.u32(block.behavior_bindings.len() as u32);
        for behavior in &block.behavior_bindings {
            writer.key(behavior);
        }
        writer.u32(block.capability_bindings.len() as u32);
        for binding in &block.capability_bindings {
            writer.key(&binding.capability);
            writer.u8(binding.face_mask);
        }
    }
    hash_state_profiles(&mut writer, blocks, false);
    hash_face_support(&mut writer, blocks);
    hash_state_optics(&mut writer, blocks);
    writer.finish()
}

/// A versioned authority-only extension. Packs without support declarations keep
/// their previous identity; presentation streams never include this extension.
fn hash_face_support(writer: &mut CanonicalWriter, blocks: &BTreeMap<ContentKey, BlockDefinition>) {
    let selected: Vec<_> = blocks
        .values()
        .filter(|block| {
            block.face_support != crate::FaceSupport::NONE
                || block
                    .state_overrides
                    .iter()
                    .any(|profile| profile.face_support.is_some())
        })
        .collect();
    if selected.is_empty() {
        return;
    }
    writer.u8(0xf2);
    writer.u32(1);
    writer.u32(selected.len() as u32);
    for block in selected {
        writer.key(&block.id);
        writer.u8(block.face_support.attachment);
        writer.u8(block.face_support.structural);
        let profiles: Vec<_> = block
            .state_overrides
            .iter()
            .filter_map(|profile| profile.face_support.map(|support| (profile, support)))
            .collect();
        writer.u32(profiles.len() as u32);
        for (profile, support) in profiles {
            writer.u32(profile.values.len() as u32);
            for value in &profile.values {
                writer.u32(u32::from(*value));
            }
            writer.u8(support.attachment);
            writer.u8(support.structural);
        }
    }
}

/// Optional authority extension; absent declarations preserve historical identities.
fn hash_state_optics(writer: &mut CanonicalWriter, blocks: &BTreeMap<ContentKey, BlockDefinition>) {
    let selected: Vec<_> = blocks
        .values()
        .filter_map(|block| {
            let profiles: Vec<_> = block
                .state_overrides
                .iter()
                .filter_map(|profile| profile.light_transport.map(|light| (profile, light)))
                .collect();
            (!profiles.is_empty()).then_some((block, profiles))
        })
        .collect();
    if selected.is_empty() {
        return;
    }
    writer.u8(0xf3);
    writer.u32(1);
    writer.u32(selected.len() as u32);
    for (block, profiles) in selected {
        writer.key(&block.id);
        writer.u32(profiles.len() as u32);
        for (profile, light) in profiles {
            writer.u32(profile.values.len() as u32);
            for value in &profile.values {
                writer.u32(u32::from(*value));
            }
            writer.u32(light.absorption.to_bits());
            writer.u32(light.emission.to_bits());
        }
    }
}

/// Hash visual material, alpha, render geometry, and presentation-only tags.
///
/// A client may reject or renegotiate a visual mismatch without changing the
/// server's authoritative block behavior.
fn presentation_digest(blocks: &BTreeMap<ContentKey, BlockDefinition>) -> Digest {
    let mut writer = CanonicalWriter::new();
    writer.u32(CONTENT_FORMAT_VERSION);
    writer.u32(blocks.len() as u32);
    for block in blocks.values() {
        writer.key(&block.id);
        writer.key(&block.render_profile.material);
        // Each face carries explicit base and overlay presence tags.
        for face in Face::ALL {
            match block.render_profile.face_materials.get(face) {
                Some(material) => {
                    writer.u8(1);
                    writer.key(material);
                }
                None => writer.u8(0),
            }
            match block.render_profile.face_overlays.get(face) {
                Some(overlay) => {
                    writer.u8(1);
                    writer.key(&overlay.material);
                    writer.u8(match overlay.alpha_mode {
                        AlphaMode::Opaque => 0,
                        AlphaMode::Mask => 1,
                        AlphaMode::Blend => 2,
                    });
                }
                None => writer.u8(0),
            }
        }
        writer.u8(match block.render_profile.alpha_mode {
            AlphaMode::Opaque => 0,
            AlphaMode::Mask => 1,
            AlphaMode::Blend => 2,
        });
        writer.shape(&block.render_profile.shape);
        writer.u32(block.tags.len() as u32);
        for tag in &block.tags {
            writer.key(tag);
        }
    }
    hash_state_profiles(&mut writer, blocks, true);
    writer.finish()
}

/// Append only when effective state overrides exist, preserving streams without them.
fn hash_state_profiles(
    writer: &mut CanonicalWriter,
    blocks: &BTreeMap<ContentKey, BlockDefinition>,
    visual: bool,
) {
    let selected: Vec<_> = blocks
        .values()
        .filter_map(|block| {
            let profiles: Vec<_> = block
                .state_overrides
                .iter()
                .filter(|profile| {
                    if visual {
                        profile.render_shape.is_some()
                    } else {
                        profile.collision_shape.is_some()
                            || profile.selection_shape.is_some()
                            || profile.occlusion.is_some()
                    }
                })
                .collect();
            (!profiles.is_empty()).then_some((block, profiles))
        })
        .collect();
    if selected.is_empty() {
        return;
    }
    writer.u8(0xf1);
    writer.u32(selected.len() as u32);
    for (block, profiles) in selected {
        writer.key(&block.id);
        writer.u32(profiles.len() as u32);
        for profile in profiles {
            writer.u32(profile.values.len() as u32);
            for value in &profile.values {
                writer.u32(u32::from(*value));
            }
            if visual {
                writer.optional_shape(profile.render_shape.as_ref());
            } else {
                writer.optional_shape(profile.collision_shape.as_ref());
                writer.optional_shape(profile.selection_shape.as_ref());
                writer.bool(profile.occlusion.is_some());
                if let Some(mask) = profile.occlusion {
                    writer.u8(mask);
                }
            }
        }
    }
}

/// Minimal append-only writer for a platform-independent canonical byte stream.
///
/// All integers use little-endian encoding and all variable-length values carry
/// an explicit length prefix. The writer is not a save format; it exists only to
/// feed a deterministic hash.
struct CanonicalWriter {
    bytes: Vec<u8>,
}

impl CanonicalWriter {
    /// Start an empty canonical byte stream.
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    fn key(&mut self, value: &ContentKey) {
        self.u32(value.namespace().len() as u32);
        self.bytes.extend_from_slice(value.namespace().as_bytes());
        self.u32(value.path().len() as u32);
        self.bytes.extend_from_slice(value.path().as_bytes());
    }

    /// Encode an optional key without colliding with an empty-key sentinel.
    fn optional_key(&mut self, value: Option<&ContentKey>) {
        self.bool(value.is_some());
        if let Some(value) = value {
            self.key(value);
        }
    }

    /// Encode a sorted key collection with a count prefix.
    fn keys(&mut self, values: &[ContentKey]) {
        self.u32(values.len() as u32);
        for value in values {
            self.key(value);
        }
    }

    /// Encode a component field tag plus any type-specific bound.
    fn component_field_type(&mut self, field_type: ComponentFieldType) {
        let tag = component_field_tag(field_type);
        self.u8(tag);
        match field_type {
            ComponentFieldType::String { max_bytes } => self.u32(max_bytes),
            ComponentFieldType::List { max_items } => self.u32(max_items),
            _ => {}
        }
    }

    /// Encode all four dimensions of a replication declaration.
    fn replication(&mut self, declaration: torse_types::ReplicationDeclaration) {
        self.u8(match declaration.authority {
            torse_types::AuthorityMode::Server => 0,
            torse_types::AuthorityMode::ClientPresentation => 1,
        });
        self.u8(match declaration.visibility {
            torse_types::ReplicationVisibility::Private => 0,
            torse_types::ReplicationVisibility::Authorized => 1,
            torse_types::ReplicationVisibility::Public => 2,
        });
        self.u8(match declaration.update_mode {
            torse_types::UpdateMode::CompleteTransaction => 0,
            torse_types::UpdateMode::LatestValue => 1,
        });
        self.u8(match declaration.presentation_hint {
            torse_types::PresentationHint::None => 0,
            torse_types::PresentationHint::Discrete => 1,
            torse_types::PresentationHint::Interpolate => 2,
        });
    }

    fn model_quad(&mut self, quad: &crate::RenderQuad) {
        for value in quad
            .positions
            .iter()
            .flatten()
            .chain(quad.uvs.iter().flatten())
            .chain(&quad.normal)
        {
            self.u32(value.to_bits());
        }
        self.key(&quad.material);
        self.u8(quad.alpha_mode as u8);
        self.bool(quad.overlay.is_some());
        if let Some(overlay) = &quad.overlay {
            self.key(&overlay.material);
            self.u8(overlay.alpha_mode as u8);
        }
        self.u8(quad.cull_face.map_or(255, |face| face.index() as u8));
        self.u8(quad.tint_channel.unwrap_or(255));
        self.bool(quad.shade);
        self.u8(quad.shade_direction.map_or(255, |face| face.index() as u8));
        self.bool(quad.double_sided);
    }

    fn shape(&mut self, shape: &Shape) {
        match shape {
            Shape::Compound(boxes) => {
                self.u8(7);
                self.u32(boxes.len() as u32);
                for bounds in boxes {
                    for value in bounds.min.iter().chain(&bounds.max) {
                        self.u32(value.to_bits());
                    }
                }
            }
            Shape::Model(model) => {
                self.u8(8);
                self.u32(model.quads.len() as u32);
                for quad in &model.quads {
                    self.model_quad(quad);
                }
                self.u32(model.parts.len() as u32);
                for part in &model.parts {
                    self.u32(part.choices.len() as u32);
                    for choice in &part.choices {
                        self.u32(choice.weight);
                        self.u32(choice.quads.len() as u32);
                        for quad in choice.quads.iter() {
                            self.model_quad(quad);
                        }
                    }
                }
            }
            Shape::FullCube => self.u8(0),
            Shape::LowerSlab => self.u8(1),
            Shape::Panel => self.u8(2),
            Shape::Corner => self.u8(3),
            Shape::Stair => self.u8(4),
            Shape::Cross => self.u8(6),
            Shape::Box(bounds) => {
                self.u8(5);
                for value in bounds.min.iter().chain(bounds.max.iter()) {
                    self.u32(value.to_bits());
                }
            }
        }
    }

    /// Encode an optional shape using a presence byte before the shape payload.
    fn optional_shape(&mut self, shape: Option<&Shape>) {
        self.bool(shape.is_some());
        if let Some(shape) = shape {
            self.shape(shape);
        }
    }

    /// Finish the stream and return its SHA-256 digest.
    fn finish(self) -> Digest {
        Digest(Sha256::digest(self.bytes).into())
    }
}

/// Stable numeric tag for a component field kind in the canonical stream.
fn component_field_tag(field_type: ComponentFieldType) -> u8 {
    match field_type {
        ComponentFieldType::Bool => 0,
        ComponentFieldType::Unsigned32 => 1,
        ComponentFieldType::Signed64 => 2,
        ComponentFieldType::ContentReference => 3,
        ComponentFieldType::EntityReference => 4,
        ComponentFieldType::String { .. } => 5,
        ComponentFieldType::List { .. } => 6,
        ComponentFieldType::Record => 7,
    }
}

/// Stable numeric tag for a capability execution contract.
fn execution_tag(contract: ExecutionContract) -> u8 {
    match contract {
        ExecutionContract::NoRuntimeCode => 0,
        ExecutionContract::BoundedNative => 1,
        ExecutionContract::Sandboxed => 2,
    }
}

/// Stable numeric tag for a module side.
fn module_side_tag(side: ModuleSide) -> u8 {
    match side {
        ModuleSide::Server => 0,
        ModuleSide::Client => 1,
        ModuleSide::Shared => 2,
    }
}

/// Build a diagnostic while keeping its content ID optional at the type level.
fn diagnostic(
    source: SourceRef,
    content_id: ContentKey,
    field: impl Into<String>,
    message: impl Into<String>,
) -> ContentDiagnostic {
    ContentDiagnostic {
        source,
        content_id: Some(content_id),
        field: field.into(),
        message: message.into(),
    }
}
