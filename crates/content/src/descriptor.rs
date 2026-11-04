//! Public content contracts shared by content compilation and runtime registration.
//!
//! These types describe content without embedding world storage details. A block
//! keeps visual, collision, selection, occlusion, light, interaction, component,
//! capability, and behavior data separate so one concern cannot silently imply
//! another. In particular, visual transparency does not imply pass-through
//! collision, and providing a gameplay capability does not grant a Mod or player
//! permission to use it.

use torse_types::{ContentKey, ReplicationDeclaration};

/// Material blending mode consumed by the presentation pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum AlphaMode {
    /// The material is fully opaque.
    Opaque,
    /// Alpha is interpreted as a cutoff mask.
    Mask,
    /// Alpha is blended with the already-rendered surface.
    Blend,
}

/// Axis-aligned bounds expressed in the block's local unit cell.
///
/// Components are expected to remain in `0.0..=1.0`; validation is performed by
/// the surrounding [`Shape`] so malformed geometry never reaches a renderer or
/// collision index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxBounds {
    /// Inclusive lower corner components in cell-local space.
    pub min: [f32; 3],
    /// Exclusive upper corner components in cell-local space.
    pub max: [f32; 3],
}

/// Maximum precompiled surface count in one static visual model.
pub const MAX_MODEL_QUADS: usize = 1024;
/// Maximum independent boxes in an authoritative compound shape.
pub const MAX_COMPOUND_BOXES: usize = 64;

/// A complete static visual model selected by one finite state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderModel {
    pub quads: Vec<RenderQuad>,
    /// Independent parts, each selecting one positive-weight alternative.
    pub parts: Vec<ModelPart>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelPart {
    pub choices: Vec<ModelChoice>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelChoice {
    pub weight: u32,
    pub quads: std::sync::Arc<Vec<RenderQuad>>,
}

/// A pretransformed outward-facing quad, with independent material and UVs.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderQuad {
    pub positions: [[f32; 3]; 4],
    pub uvs: [[f32; 2]; 4],
    pub normal: [f32; 3],
    pub material: ContentKey,
    pub alpha_mode: AlphaMode,
    pub overlay: Option<FaceOverlay>,
    pub cull_face: Option<Face>,
    pub tint_channel: Option<u8>,
    pub shade: bool,
    pub shade_direction: Option<Face>,
    pub double_sided: bool,
}

impl RenderModel {
    /// Conservative whole-face coverage common to every weighted alternative.
    pub fn occlusion_mask(&self) -> u8 {
        fn mask(quads: &[RenderQuad]) -> u8 {
            quads.iter().fold(0, |mask, quad| {
                if quad.alpha_mode != AlphaMode::Opaque {
                    return mask;
                }
                for face in Face::ALL {
                    let axis = face.index() / 2;
                    let boundary = (face.index() % 2) as f32;
                    let other: Vec<_> = (0..3).filter(|i| *i != axis).collect();
                    if (quad.normal[axis] - (boundary * 2.0 - 1.0)).abs() < 1e-6
                        && quad
                            .positions
                            .iter()
                            .all(|p| (p[axis] - boundary).abs() < 1e-6)
                        && [(0.0, 0.0), (0.0, 1.0), (1.0, 0.0), (1.0, 1.0)]
                            .iter()
                            .all(|(a, b)| {
                                quad.positions.iter().any(|p| {
                                    (p[other[0]] - *a).abs() < 1e-6
                                        && (p[other[1]] - *b).abs() < 1e-6
                                })
                            })
                    {
                        return mask | (1 << face.index());
                    }
                }
                mask
            })
        }
        self.parts.iter().fold(mask(&self.quads), |coverage, part| {
            coverage
                | if part.choices.is_empty() {
                    0
                } else {
                    part.choices
                        .iter()
                        .fold(0b11_1111, |common, c| common & mask(&c.quads))
                }
        })
    }

    /// Deterministic independent selection from an application-owned position seed.
    pub fn selected_quads(&self, seed: u64) -> impl Iterator<Item = &RenderQuad> {
        self.quads.iter().chain(
            self.parts
                .iter()
                .enumerate()
                .flat_map(move |(index, part)| {
                    let mut hash =
                        seed.wrapping_add((index as u64).wrapping_mul(0x9e3779b97f4a7c15));
                    hash = (hash ^ (hash >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                    hash = (hash ^ (hash >> 27)).wrapping_mul(0x94d049bb133111eb);
                    hash ^= hash >> 31;
                    let total: u64 = part.choices.iter().map(|c| u64::from(c.weight)).sum();
                    let mut value = if total == 0 { 0 } else { hash % total };
                    part.choices
                        .iter()
                        .find(|choice| {
                            if value < u64::from(choice.weight) {
                                true
                            } else {
                                value -= u64::from(choice.weight);
                                false
                            }
                        })
                        .into_iter()
                        .flat_map(|choice| choice.quads.iter())
                }),
        )
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.parts.len() > 64
            || self.parts.iter().any(|part| {
                part.choices.is_empty()
                    || part.choices.len() > 64
                    || part.choices.iter().any(|choice| choice.weight == 0)
            })
        {
            return Err("visual model has invalid weighted parts");
        }
        let active = self.quads.len()
            + self
                .parts
                .iter()
                .map(|part| {
                    part.choices
                        .iter()
                        .map(|choice| choice.quads.len())
                        .max()
                        .unwrap_or(0)
                })
                .sum::<usize>();
        let stored = self.quads.len()
            + self
                .parts
                .iter()
                .flat_map(|p| &p.choices)
                .map(|choice| choice.quads.len())
                .sum::<usize>();
        if active > MAX_MODEL_QUADS || stored > MAX_MODEL_QUADS * 64 {
            return Err("visual model exceeds the quad limit");
        }
        for quad in self.quads.iter().chain(
            self.parts
                .iter()
                .flat_map(|part| part.choices.iter().flat_map(|choice| choice.quads.iter())),
        ) {
            if quad
                .positions
                .iter()
                .flatten()
                .any(|v| !v.is_finite() || !(-1.0..=2.0).contains(v))
                || quad
                    .uvs
                    .iter()
                    .flatten()
                    .any(|v| !v.is_finite() || v.abs() > 16.0)
                || quad.normal.iter().any(|v| !v.is_finite())
                || quad.tint_channel.is_some_and(|value| value > 7)
            {
                return Err("visual quad contains invalid coordinates, UVs, or tint channel");
            }
            let normal_length: f32 = quad.normal.iter().map(|v| v * v).sum();
            if (normal_length - 1.0).abs() > 0.001 {
                return Err("visual quad normal must have unit length");
            }
            for triangle in [[0, 1, 2], [0, 2, 3]] {
                let a = std::array::from_fn::<_, 3, _>(|i| {
                    quad.positions[triangle[1]][i] - quad.positions[triangle[0]][i]
                });
                let b = std::array::from_fn::<_, 3, _>(|i| {
                    quad.positions[triangle[2]][i] - quad.positions[triangle[0]][i]
                });
                let cross = [
                    a[1] * b[2] - a[2] * b[1],
                    a[2] * b[0] - a[0] * b[2],
                    a[0] * b[1] - a[1] * b[0],
                ];
                if cross
                    .iter()
                    .zip(quad.normal)
                    .map(|(a, b)| a * b)
                    .sum::<f32>()
                    <= 1e-10
                {
                    return Err("visual quad triangles must have outward nondegenerate winding");
                }
            }
        }
        Ok(())
    }
}

/// Static geometry preset available to block definitions.
///
/// Presets compile to deterministic shape data. Arbitrary meshes and moving
/// multi-cell structures are intentionally outside the M0 contract.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    FullCube,
    LowerSlab,
    Panel,
    Cross,
    Corner,
    Stair,
    Box(BoxBounds),
    /// Visual-only precompiled surfaces. Physics must be declared independently.
    Model(std::sync::Arc<RenderModel>),
    /// Non-nested collection of independently validated cell-local boxes.
    Compound(Vec<BoxBounds>),
}

impl Shape {
    /// Validate the local geometry encoded by this shape.
    ///
    /// The error is a stable English description suitable for content
    /// diagnostics. It does not identify the containing file; the compiler adds
    /// source and field context.
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Box(bounds) => validate_box(bounds),
            Self::Model(model) => model.validate(),
            Self::Compound(boxes) => {
                if boxes.len() > MAX_COMPOUND_BOXES {
                    return Err("compound shape exceeds the box limit");
                }
                for bounds in boxes {
                    if !(0..3).all(|axis| {
                        bounds.min[axis].is_finite()
                            && bounds.max[axis].is_finite()
                            && bounds.min[axis] >= -1.0
                            && bounds.max[axis] <= 2.0
                            && bounds.min[axis] < bounds.max[axis]
                    }) {
                        return Err("compound boxes must be finite and bounded by [-1,2]");
                    }
                }
                Ok(())
            }
            Self::FullCube
            | Self::LowerSlab
            | Self::Panel
            | Self::Cross
            | Self::Corner
            | Self::Stair => Ok(()),
        }
    }
}

fn validate_box(bounds: &BoxBounds) -> Result<(), &'static str> {
    for (minimum, maximum) in bounds.min.iter().zip(bounds.max.iter()) {
        if !minimum.is_finite() || !maximum.is_finite() {
            return Err("box bounds must be finite");
        }
        if *minimum < 0.0 || *maximum > 1.0 {
            return Err("box bounds must stay within the cell");
        }
        if minimum >= maximum {
            return Err("box minimum must be smaller than maximum");
        }
    }
    Ok(())
}

/// Cell faces in occlusion-mask order: `-x, +x, -y, +y, -z, +z`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Face {
    /// Negative x.
    NegX,
    /// Positive x.
    PosX,
    /// Negative y.
    NegY,
    /// Positive y.
    PosY,
    /// Negative z.
    NegZ,
    /// Positive z.
    PosZ,
}

impl Face {
    /// All faces in occlusion-mask order.
    pub const ALL: [Self; 6] = [
        Self::NegX,
        Self::PosX,
        Self::NegY,
        Self::PosY,
        Self::NegZ,
        Self::PosZ,
    ];

    /// Position of this face in a mask or material slot array.
    pub const fn index(self) -> usize {
        match self {
            Self::NegX => 0,
            Self::PosX => 1,
            Self::NegY => 2,
            Self::PosY => 3,
            Self::NegZ => 4,
            Self::PosZ => 5,
        }
    }
}

/// Per-face material overrides for one block.
///
/// A missing slot falls back to [`RenderProfile::material`]. Overrides let one
/// block show different surfaces per face without registering separate block
/// definitions, for example grass with a top material and a side material.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FaceMaterials {
    slots: [Option<ContentKey>; 6],
}

impl FaceMaterials {
    /// No overrides; every face uses the block material.
    pub const fn none() -> Self {
        Self {
            slots: [None, None, None, None, None, None],
        }
    }

    /// Set the material of one face.
    pub fn set(&mut self, face: Face, material: ContentKey) {
        self.slots[face.index()] = Some(material);
    }

    /// Override declared for one face, if any.
    pub fn get(&self, face: Face) -> Option<&ContentKey> {
        self.slots[face.index()].as_ref()
    }

    /// Material of one face, falling back to the block material.
    pub fn resolve<'a>(&'a self, face: Face, default: &'a ContentKey) -> &'a ContentKey {
        self.get(face).unwrap_or(default)
    }

    /// Whether no face carries an override.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }
}

/// One independent overlay layer on a face.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FaceOverlay {
    /// Material resource for the overlay.
    pub material: ContentKey,
    /// Alpha behavior for this layer, independent of the base material.
    pub alpha_mode: AlphaMode,
}

/// Optional overlay materials for the six block faces.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FaceOverlays {
    slots: [Option<FaceOverlay>; 6],
}

impl FaceOverlays {
    /// No face has an overlay.
    pub const fn none() -> Self {
        Self {
            slots: [None, None, None, None, None, None],
        }
    }

    /// Set the overlay of one face.
    pub fn set(&mut self, face: Face, overlay: FaceOverlay) {
        self.slots[face.index()] = Some(overlay);
    }

    /// Overlay declared for one face, if any.
    pub fn get(&self, face: Face) -> Option<&FaceOverlay> {
        self.slots[face.index()].as_ref()
    }

    /// Whether no face has an overlay.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(Option::is_none)
    }
}

/// Geometry and material selected for rendering.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderProfile {
    /// Geometry used for visual meshing.
    pub shape: Shape,
    /// Material resource selected for faces without an override.
    pub material: ContentKey,
    /// Per-face material overrides.
    pub face_materials: FaceMaterials,
    /// Optional material overlay for each face.
    pub face_overlays: FaceOverlays,
    /// Material blending mode.
    pub alpha_mode: AlphaMode,
}

/// Geometry used by authoritative collision and client prediction.
///
/// `None` means the block does not contribute a collision surface, which is a
/// valid choice for plants and similar decorative content.
#[derive(Clone, Debug, PartialEq)]
pub struct CollisionProfile {
    /// Collision geometry, or `None` for non-solid content.
    pub shape: Option<Shape>,
}

/// Geometry used for ray selection and interaction targeting.
///
/// Selection may intentionally differ from collision, for example a plant can
/// have no collision while remaining clickable.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectionProfile {
    /// Ray-selection geometry, or `None` for non-selectable content.
    pub shape: Option<Shape>,
}

/// Conservative face coverage used by mesh occlusion calculations.
///
/// The mask uses six bits in `-x, +x, -y, +y, -z, +z` order. A missing bit must
/// produce extra visible faces rather than incorrectly removing a neighbor.
/// The mask uses six bits in `-x, +x, -y, +y, -z, +z` order. A missing bit must
/// produce extra visible faces rather than incorrectly removing a neighbor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OcclusionProfile {
    /// Coverage of the six cell boundaries.
    pub face_mask: u8,
}

/// Built-in predicates that rules may require of a block face.
///
/// These categories have no implication for geometry, strength, or permission.
/// Applications explicitly author the data and decide which predicate a rule uses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportCategory {
    /// A rule may attach an object to this face.
    Attachment,
    /// A rule may use this face as a structural support.
    Structural,
}

/// Independent static support declarations in `-x, +x, -y, +y, -z, +z` order.
///
/// Each category has exactly six bits. Missing declarations deny all support;
/// neither collision nor visual coverage supplies an implicit default.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct FaceSupport {
    pub attachment: u8,
    pub structural: u8,
}

impl FaceSupport {
    pub const NONE: Self = Self {
        attachment: 0,
        structural: 0,
    };

    /// Return the face mask for one typed category.
    pub const fn mask(self, category: SupportCategory) -> u8 {
        match category {
            SupportCategory::Attachment => self.attachment,
            SupportCategory::Structural => self.structural,
        }
    }

    /// Whether the selected face explicitly declares this predicate.
    pub const fn supports(self, face: Face, category: SupportCategory) -> bool {
        self.mask(category) & (1 << face.index()) != 0
    }

    /// Reject bits outside the fixed six-face vocabulary.
    pub fn validate(self) -> Result<(), &'static str> {
        if (self.attachment | self.structural) & !0x3f != 0 {
            return Err("support masks may only contain six face bits");
        }
        Ok(())
    }
}

/// Per-state profile overrides for blocks whose shape varies by state
/// values, such as slabs with an upper and a lower half.
///
/// Every listed entry pins one exact value combination in the schema's
/// canonical field order. Fields not overridden fall back to the block
/// definition's own profiles.
#[derive(Clone, Debug, PartialEq)]
pub struct StateProfile {
    /// Exact finite field values this override applies to, in the schema's
    /// canonical field order.
    pub values: Vec<u16>,
    /// Render shape override.
    pub render_shape: Option<Shape>,
    /// Collision shape override.
    pub collision_shape: Option<Shape>,
    /// Selection shape override.
    pub selection_shape: Option<Shape>,
    /// Occlusion mask override.
    pub occlusion: Option<u8>,
    /// Exact replacement of both support masks. `None` inherits the definition;
    /// `Some(FaceSupport::NONE)` explicitly denies every category on every face.
    pub face_support: Option<FaceSupport>,
    /// Exact optical replacement. None inherits; zero explicitly disables emission.
    pub light_transport: Option<LightTransport>,
}

/// Light absorption and emission values for the content definition.
///
/// Both values are normalized to `0.0..=1.0` and are independent of collision
/// and visual alpha.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LightTransport {
    /// Fraction of incident light absorbed; validated to `0.0..=1.0`.
    pub absorption: f32,
    /// Normalized emission contribution; validated to `0.0..=1.0`.
    pub emission: f32,
}

/// Action binding used when a player or system interacts with the block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractionProfile {
    /// Registered action invoked by a normal interaction.
    pub action: Option<ContentKey>,
    /// Block written when an identical block is placed into a cell that
    /// already holds this one, filling the cell's remaining volume. A lower
    /// slab completes into its full block; `None` keeps placement into an
    /// occupied cell rejected.
    pub completes_to: Option<ContentKey>,
}

/// One finite-valued state field such as orientation or growth stage.
///
/// The legal values are explicit so the compiler can calculate the total state
/// combination count and reject definitions that expand without bound.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FiniteField {
    /// Stable field identity within the content schema.
    pub id: ContentKey,
    /// Default finite value and the value used for newly placed blocks.
    pub default_value: u16,
    /// Complete legal value set for this field.
    pub values: Vec<u16>,
}

/// Primitive field kinds supported by the initial component schema model.
///
/// Container limits are part of the type rather than an unconstrained runtime
/// convention, which lets validation and migration reason about worst-case size.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentFieldType {
    Bool,
    Unsigned32,
    Signed64,
    ContentReference,
    EntityReference,
    String { max_bytes: u32 },
    List { max_items: u32 },
    Record,
}

/// Named, typed field inside a component schema.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentField {
    /// Stable field identity within the component schema.
    pub id: ContentKey,
    /// Primitive storage and validation type.
    pub field_type: ComponentFieldType,
    /// Whether the field must be present when the component is instantiated.
    pub required: bool,
}

/// Whether a component is expected to survive a save/reload cycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersistencePolicy {
    /// The component is runtime-only.
    Never,
    /// The component must be saved.
    Always,
    /// The owning module decides according to a narrower policy.
    OwnerDefined,
}

/// Versioned component schema owned by one module.
///
/// Ownership makes mutation responsibility explicit. Other modules are expected
/// to use capabilities or actions instead of directly rewriting protected
/// fields, even though the descriptor itself is only data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentSchema {
    /// Stable component schema identity.
    pub id: ContentKey,
    /// Schema version used by migration and compatibility checks.
    pub schema_version: u32,
    /// Module allowed to define and mutate protected fields.
    pub owner_module: ContentKey,
    /// Declared fields and their bounds.
    pub fields: Vec<ComponentField>,
    /// Save policy for this component.
    pub persistence: PersistencePolicy,
    /// Replication contract for this component.
    pub replication: ReplicationDeclaration,
    /// Optional migration handler binding.
    pub migration_binding: Option<ContentKey>,
}

/// Complete state schema for a content definition.
///
/// The finite fields determine the compact block-state space. Component schemas
/// describe larger or sparse instance data that should not be expanded into the
/// block registry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateSchema {
    /// Stable state schema identity.
    pub id: ContentKey,
    /// Schema version used by content and persistence compatibility.
    pub schema_version: u32,
    /// Compact finite fields expanded into the block-state registry.
    pub finite_fields: Vec<FiniteField>,
    /// Sparse instance components associated with the state.
    pub component_schemas: Vec<ComponentSchema>,
}

/// Binding from a content object to a gameplay capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityBinding {
    /// Capability interface attached to the object.
    pub capability: ContentKey,
    /// Bit mask matching [`OcclusionProfile::face_mask`] ordering.
    pub face_mask: u8,
}

/// Top-level static block definition.
///
/// Profiles are separate fields on purpose. A content author can use a blended
/// material, full collision, and partial light transport without any one of
/// those choices being inferred from another.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockDefinition {
    /// Stable block identity across content and save versions.
    pub id: ContentKey,
    /// Version of this block definition.
    pub definition_version: u32,
    /// State schema used to interpret finite block state.
    pub state_schema: ContentKey,
    /// Visual geometry and material.
    pub render_profile: RenderProfile,
    /// Authoritative collision geometry.
    pub collision_profile: CollisionProfile,
    /// Interaction targeting geometry.
    pub selection_profile: SelectionProfile,
    /// Face coverage used by conservative mesh culling.
    pub occlusion_profile: OcclusionProfile,
    /// Rule predicates, independent of collision, rendering and occlusion.
    pub face_support: FaceSupport,
    /// Light behavior independent of rendering alpha.
    pub light_transport: LightTransport,
    /// Interaction action binding.
    pub interaction_profile: InteractionProfile,
    /// Per-state profile overrides; an empty list means every state uses the
    /// block's own profiles.
    pub state_overrides: Vec<StateProfile>,
    /// Component schemas instantiated by default.
    pub component_defaults: Vec<ContentKey>,
    /// Gameplay capabilities exposed by this block.
    pub capability_bindings: Vec<CapabilityBinding>,
    /// Behavior module hooks associated with this block.
    pub behavior_bindings: Vec<ContentKey>,
    /// Filtering tags; tags do not grant capabilities or permissions.
    pub tags: Vec<ContentKey>,
}

/// Execution class of a capability implementation.
///
/// This is descriptive M0 metadata; permission checking and sandbox execution
/// are enforced by later host layers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionContract {
    /// The capability is declarative and executes no arbitrary code.
    NoRuntimeCode,
    /// A trusted native implementation performs bounded work.
    BoundedNative,
    /// A sandboxed implementation must run under host resource controls.
    Sandboxed,
}

/// Typed method exposed by a gameplay capability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MethodDescriptor {
    /// Stable method identity within the capability API.
    pub id: ContentKey,
    /// Parameter schema used to validate and encode the call.
    pub parameter_schema: ContentKey,
    /// Result schema used to validate and encode the response.
    pub result_schema: ContentKey,
}

/// Versioned gameplay capability such as an item port or energy receiver.
///
/// A capability describes what an object can expose. It does not grant a caller
/// permission to use that capability; permissions are a separate module/host
/// concern.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityDescriptor {
    /// Stable gameplay capability identity.
    pub id: ContentKey,
    /// Capability API version used for compatibility negotiation.
    pub api_version: u32,
    /// Read-only methods exposed by the capability.
    pub query_methods: Vec<MethodDescriptor>,
    /// Mutating methods exposed through the authority transaction path.
    pub mutation_methods: Vec<MethodDescriptor>,
    /// Schemas an object must satisfy to bind this capability.
    pub target_schema_constraints: Vec<ContentKey>,
    /// Contract key describing who may call the capability.
    pub access_contract: ContentKey,
    /// Execution class of the implementation.
    pub execution_contract: ExecutionContract,
}

/// Side on which a module or its implementation is available.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModuleSide {
    Server,
    Client,
    Shared,
}

/// Stable service identity and API version provided or required by a module.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceDescriptor {
    /// Stable service identity.
    pub id: ContentKey,
    /// API version required for provider/consumer compatibility.
    pub api_version: u32,
}

/// Registered simulation system with an explicit phase and output budget.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemDescriptor {
    /// Stable system identity used for scheduling and diagnostics.
    pub id: ContentKey,
    /// Simulation phase in which the system is eligible to run.
    pub phase: u8,
    /// Maximum logical output entries allowed per execution.
    pub max_items: u32,
    /// Maximum owned or emitted bytes allowed per execution.
    pub max_bytes: u64,
}

/// Permission requested by a module, distinct from object capabilities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestedPermission {
    /// Host permission requested by the module declaration.
    pub id: ContentKey,
}

/// Complete module registration and dependency declaration.
///
/// M0 stores the descriptor and validates its references. Lifecycle execution,
/// permission approval, and runtime service binding are implemented by later
/// milestones.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModuleDescriptor {
    /// Stable module identity.
    pub id: ContentKey,
    /// Version of this module package/declaration.
    pub module_version: u32,
    /// Framework API version expected by the module.
    pub framework_api: u32,
    /// Server, client, or shared availability.
    pub side: ModuleSide,
    /// Modules that must be present and started first.
    pub required_modules: Vec<ContentKey>,
    /// Modules that alter capabilities but are not required for startup.
    pub optional_modules: Vec<ContentKey>,
    /// Services supplied by this module.
    pub provided_services: Vec<ServiceDescriptor>,
    /// Services this module expects another module to supply.
    pub required_services: Vec<ServiceDescriptor>,
    /// State schema IDs registered by this module.
    pub registered_schemas: Vec<ContentKey>,
    /// Capability IDs registered by this module.
    pub registered_capabilities: Vec<ContentKey>,
    /// Action IDs registered by this module.
    pub registered_actions: Vec<ContentKey>,
    /// Simulation systems registered by this module.
    pub registered_systems: Vec<SystemDescriptor>,
    /// Host permissions requested by this module.
    pub requested_permissions: Vec<RequestedPermission>,
    /// Optional schema used to validate module configuration.
    pub configuration_schema: Option<ContentKey>,
    /// Optional lifecycle implementation binding.
    pub lifecycle_binding: Option<ContentKey>,
}
