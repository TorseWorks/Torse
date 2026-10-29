//! Content descriptors, validation, and deterministic compilation.

#![forbid(unsafe_code)]

mod builtin;
mod compiler;
mod cooked;
mod descriptor;
pub mod presets;

#[cfg(test)]
mod compiler_tests;

pub use builtin::m0_fixture_pack;
pub use compiler::{
    CONTENT_FORMAT_VERSION, CompiledContent, ContentDiagnostic, ContentPack, MAX_FINITE_STATES,
    SourceRef, Sourced, compile,
};
pub use cooked::{
    CookError, CookImage, CookLimits, CookedLocale, CookedMaterial, CookedState, CookedTables,
    cook, parse_locale, read_cooked, read_cooked_with_limits, write_cooked,
};
pub use descriptor::{
    AlphaMode, BlockDefinition, BoxBounds, CapabilityBinding, CapabilityDescriptor,
    CollisionProfile, ComponentField, ComponentFieldType, ComponentSchema, ExecutionContract, Face,
    FaceMaterials, FaceOverlay, FaceOverlays, FiniteField, InteractionProfile, LightTransport,
    MAX_COMPOUND_BOXES, MAX_MODEL_QUADS, MethodDescriptor, ModelChoice, ModelPart,
    ModuleDescriptor, ModuleSide, OcclusionProfile, PersistencePolicy, RenderModel, RenderProfile,
    RenderQuad, RequestedPermission, SelectionProfile, ServiceDescriptor, Shape, StateProfile,
    StateSchema, SystemDescriptor,
};
