//! wgpu rendering for the client.
//!
//! [`GpuContext`] owns the device-facing half: pipelines, the camera uniform,
//! the material table, and terrain buffers. Both targets reuse it: [`Renderer`]
//! draws into an offscreen texture so a frame can be captured without a window,
//! and the windowed app draws into a surface view. The pass is identical, only
//! the target differs.
//!
//! Terrain vertices carry a material slot. The fragment shader looks the slot up
//! in a fixed-size material table, samples the texture atlas through the
//! stored rectangle, multiplies the result by the material tint, and applies the
//! material's alpha mode. Materials come from [`crate::texture::Palette`].

use std::collections::BTreeMap;
mod residency;
use residency::{Bounds, admission, priority};
use std::fs::File;
use std::time::{Duration, Instant};

use wgpu::{
    AddressMode, BindGroup, BindGroupLayout, BindGroupLayoutDescriptor, BindGroupLayoutEntry,
    BindingType, BlendState, Buffer, BufferBindingType, BufferDescriptor, BufferUsages, Color,
    ColorTargetState, ColorWrites, CommandEncoder, CommandEncoderDescriptor, Device, Extent3d,
    FilterMode, FragmentState, LoadOp, MapMode, MultisampleState, Operations, Origin3d, PollType,
    PrimitiveState, Queue, RenderPassColorAttachment, RenderPassDepthStencilAttachment,
    RenderPassDescriptor, RenderPipeline, RenderPipelineDescriptor, SamplerBindingType,
    SamplerDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, StoreOp,
    TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture, TextureAspect,
    TextureDescriptor, TextureDimension, TextureFormat, TextureSampleType, TextureUsages,
    TextureView, TextureViewDescriptor, VertexState,
};

use crate::texture::{MAX_MATERIALS, Palette};

const TERRAIN_SHADER: &str = r#"
struct Camera {
    view_proj: mat4x4<f32>,
    eye: vec4<f32>,
};

struct Surface {
    rect: vec4<f32>,
    tint: vec4<f32>,
    next_rect: vec4<f32>,
    frame: vec4<f32>,
};

struct Materials {
    entries: array<Surface, 512>,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

@group(1) @binding(0)
var<uniform> materials: Materials;

@group(1) @binding(1)
var atlas: texture_2d<f32>;

@group(1) @binding(2)
var atlas_sampler: sampler;

struct VertexIn {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) uv: vec2<f32>,
    @location(3) material: u32,
    @location(4) overlay_material: u32,
    @location(5) tint: vec3<f32>,
    @location(6) overlay_tint: vec3<f32>,
    @location(7) light: f32,
};

struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) normal: vec3<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) @interpolate(flat) material: u32,
    @location(3) @interpolate(flat) overlay_material: u32,
    @location(4) tint: vec3<f32>,
    @location(5) overlay_tint: vec3<f32>,
    @location(6) world: vec3<f32>,
    @location(7) light: f32,
};

fn sample_rect(rect: vec4<f32>, uv: vec2<f32>, level: f32) -> vec4<f32> {
    // Each mip has a larger filter footprint. Base-level padding does not
    // prevent linear filtering from reading the atlas gutter at a distance.
    let texel = 0.5 / vec2<f32>(textureDimensions(atlas, u32(level)));
    let inset = min(texel, rect.zw * 0.5);
    let point = clamp(rect.xy + fract(uv) * rect.zw,
        rect.xy + inset, rect.xy + rect.zw - inset);
    return textureSampleLevel(atlas, atlas_sampler, point, level);
}

fn sample_surface(surface: Surface, uv: vec2<f32>) -> vec4<f32> {
    let size = surface.rect.zw * vec2<f32>(textureDimensions(atlas, 0));
    let dx = dpdx(uv) * size;
    let dy = dpdy(uv) * size;
    let lod = clamp(0.5 * log2(max(max(dot(dx, dx), dot(dy, dy)), 1.0)),
        0.0, f32(textureNumLevels(atlas) - 1u));
    let low = select(floor(lod), 0.0, surface.frame.y > 0.5);
    let high = select(ceil(lod), 0.0, surface.frame.y > 0.5);
    let first = mix(sample_rect(surface.rect, uv, low),
        sample_rect(surface.rect, uv, high), fract(lod));
    if (surface.frame.x <= 0.0) {
        return first;
    }
    let second = sample_rect(surface.next_rect, uv, 0.0);
    return mix(first, second, surface.frame.x);
}

fn srgb_encode(color: vec3<f32>) -> vec3<f32> {
    let high = 1.055 * pow(max(color, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(high, color * 12.92, color <= vec3<f32>(0.0031308));
}

fn srgb_decode(color: vec3<f32>) -> vec3<f32> {
    let high = pow((color + 0.055) / 1.055, vec3<f32>(2.4));
    return select(high, color / 12.92, color <= vec3<f32>(0.04045));
}

// Brightness of one face. A block surface is lit by its axis, not by a dot
// product with a sun direction, which keeps every voxel face equally readable.
fn face_shade(normal: vec3<f32>) -> f32 {
    if (normal.y > 0.5) {
        return 1.0;
    }
    if (normal.y < -0.5) {
        return 0.5;
    }
    if (abs(normal.x) > 0.5) {
        return 0.6;
    }
    return 0.8;
}

@vertex
fn vs_main(input: VertexIn) -> VertexOut {
    var out: VertexOut;
    out.clip = camera.view_proj * vec4<f32>(input.position, 1.0);
    out.normal = input.normal;
    out.uv = input.uv;
    out.material = input.material;
    out.overlay_material = input.overlay_material;
    out.tint = input.tint;
    out.overlay_tint = input.overlay_tint;
    out.world = input.position;
    out.light = input.light;
    return out;
}

@fragment
fn fs_main(
    input: VertexOut,
    @builtin(front_facing) front_facing: bool,
) -> @location(0) vec4<f32> {
    let surface = materials.entries[input.material];
    let texel = sample_surface(surface, input.uv);

    // tint.w encodes the alpha mode: 0 opaque, 1 mask, 2 blend.
    let mode = surface.tint.w;
    if (mode > 0.5 && mode < 1.5 && texel.a < 0.5) {
        discard;
    }
    // Colours combine in sRGB space, the space the textures were authored in;
    // the linear result is re-encoded for the sRGB target.
    var color = srgb_encode(texel.rgb) * srgb_encode(surface.tint.rgb) * srgb_encode(input.tint);
    var alpha = select(1.0, texel.a, mode > 1.5);
    if (input.overlay_material != 0xffffffffu) {
        let overlay = materials.entries[input.overlay_material];
        let overlay_texel = sample_surface(overlay, input.uv);
        let overlay_mode = overlay.tint.w;
        let overlay_color = srgb_encode(overlay_texel.rgb)
            * srgb_encode(overlay.tint.rgb)
            * srgb_encode(input.overlay_tint);
        if (overlay_mode < 0.5) {
            color = overlay_color;
            alpha = 1.0;
        } else if (overlay_mode < 1.5) {
            if (overlay_texel.a >= 0.5) {
                color = overlay_color;
                alpha = 1.0;
            }
        } else {
            let combined_alpha = overlay_texel.a + alpha * (1.0 - overlay_texel.a);
            if (combined_alpha > 0.0) {
                color = (overlay_color * overlay_texel.a
                    + color * alpha * (1.0 - overlay_texel.a))
                    / combined_alpha;
            }
            alpha = combined_alpha;
        }
    }
    let normal = select(-input.normal, input.normal, front_facing);
    let sky = srgb_encode(vec3<f32>(0.66, 0.74, 0.80));
    let distance = length(input.world - camera.eye.xyz);
    let fog = clamp(1.0 - exp(-distance * 0.0028), 0.0, 0.72);
    let lit = color * select(input.light, face_shade(normal), input.light < 0.0);
    return vec4<f32>(srgb_decode(mix(lit, sky, fog)), alpha);
}
"#;

const SKY_SHADER: &str = r#"
struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) height: f32,
};

@vertex
fn vs_main(@builtin(vertex_index) index: u32) -> VertexOut {
    var corners = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let corner = corners[index];
    var out: VertexOut;
    out.clip = vec4<f32>(corner, 0.0, 1.0);
    out.height = corner.y * 0.5 + 0.5;
    return out;
}

@fragment
fn fs_main(input: VertexOut) -> @location(0) vec4<f32> {
    let horizon = vec3<f32>(0.66, 0.74, 0.80);
    let zenith = vec3<f32>(0.34, 0.52, 0.80);
    let blend = smoothstep(0.30, 0.95, input.height);
    return vec4<f32>(mix(horizon, zenith, blend), 1.0);
}
"#;

const LINE_SHADER: &str = r#"
struct Camera {
    view_proj: mat4x4<f32>,
    eye: vec4<f32>,
};

@group(0) @binding(0)
var<uniform> camera: Camera;

struct LineOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec3<f32>,
};
@vertex
fn vs_main(@location(0) position: vec3<f32>, @location(1) color: vec3<f32>) -> LineOut {
    var out: LineOut;
    out.position = camera.view_proj * vec4<f32>(position, 1.0);
    out.color = color;
    return out;
}
@fragment
fn fs_main(input: LineOut) -> @location(0) vec4<f32> {
    return vec4<f32>(input.color, 1.0);
}
"#;

/// Sky colour shared by both targets.
pub const SKY: Color = Color {
    r: 0.66,
    g: 0.74,
    b: 0.80,
    a: 1.0,
};

/// Selection outline capacity: 12 cube edges of 2 vertices each.
const MAX_LINE_VERTICES: usize = 24;
const MAX_BORDER_VERTICES: usize = 384;

/// Bytes per terrain vertex: position, normal, uv, and one material slot.
const VERTEX_STRIDE: usize = 68;

/// Material table bytes: rect plus tint per surface.
const SURFACE_BYTES: u64 = 64;

/// Synthetic key carrying the entity box buffers; retention never drops it.
pub const ENTITY_SLOT: torse_types::SectionPos =
    torse_types::SectionPos::new(i64::MAX, 0, i64::MAX);
pub const SURFACE_SLOT: torse_types::SectionPos =
    torse_types::SectionPos::new(i64::MAX - 1, 0, i64::MAX);

/// Device, queue, pipelines, camera uniform, and terrain buffers.
pub struct GpuContext {
    device: Device,
    queue: Queue,
    pipeline: RenderPipeline,
    transparent_pipeline: RenderPipeline,
    memory: Option<torse_types::MemoryPool>,
    view_projection: std::cell::Cell<[f32; 16]>,
    eye: std::cell::Cell<[f32; 3]>,
    focus: torse_types::SectionPos,
    geometry_limit: u64,
    geometry_bytes: u64,
    geometry_evictions: u64,
    frontier: std::cell::Cell<Option<(u128, torse_types::SectionPos)>>,
    draw_stats: std::cell::Cell<DrawStats>,
    line_pipeline: RenderPipeline,
    border_pipeline: RenderPipeline,
    sky_pipeline: RenderPipeline,
    camera: Buffer,
    camera_bind: BindGroup,
    palette_layout: BindGroupLayout,
    palette: Option<PaletteBinding>,
    #[allow(dead_code)]
    camera_layout: BindGroupLayout,
    /// One vertex/index buffer pair per resident section; edits rewrite only
    /// the changed sections instead of re-uploading the whole scene.
    terrain: BTreeMap<torse_types::SectionPos, SectionBuffers>,
    line_buffer: Buffer,
    line_count: u32,
    border_count: u32,
    animation_origin: Instant,
}

/// GPU resources for the material table and atlas.
struct PaletteBinding {
    #[allow(dead_code)]
    materials: Buffer,
    #[allow(dead_code)]
    texture: Texture,
    surfaces: Vec<crate::texture::Surface>,
    bind: BindGroup,
}

/// GPU buffers for one section's mesh.
struct SectionBuffers {
    vertices: Buffer,
    indices: Buffer,
    opaque_count: u32,
    transparent: Vec<TransparentFace>,
    _memory: Option<torse_types::MemoryLease>,
    bytes: u64,
    bounds: Bounds,
}

/// Logical published vertex/index payload, independent of CPU memory accounts.
#[derive(Clone, Copy, Debug, Default)]
pub struct GeometryUsage {
    pub limit: u64,
    pub bytes: u64,
    pub sections: usize,
    pub evictions: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DrawStats {
    pub visible_sections: usize,
    pub culled_sections: usize,
}

struct TransparentFace {
    center: [f32; 3],
    first_index: u32,
}

struct GeometryAdmission {
    victims: Vec<torse_types::SectionPos>,
    _memory: Option<torse_types::MemoryLease>,
}

fn split_faces(
    bytes: &[u8],
    indices: &[u32],
    surfaces: &[crate::texture::Surface],
) -> Option<(Vec<u32>, u32, Vec<TransparentFace>)> {
    if !bytes.len().is_multiple_of(VERTEX_STRIDE)
        || !indices.len().is_multiple_of(6)
        || indices.len() > u32::MAX as usize
    {
        return None;
    }
    let mut opaque = Vec::with_capacity(indices.len());
    let mut blended = Vec::new();
    let mut faces = Vec::new();
    let u32_at = |vertex: &[u8], offset| {
        u32::from_le_bytes(vertex[offset..offset + 4].try_into().expect("vertex field"))
    };
    for quad in indices.chunks_exact(6) {
        let mut center = [0.0; 3];
        let first = bytes.get(
            quad[0] as usize * VERTEX_STRIDE..quad[0] as usize * VERTEX_STRIDE + VERTEX_STRIDE,
        )?;
        let blend = [u32_at(first, 32), u32_at(first, 36)]
            .into_iter()
            .any(|slot| {
                surfaces
                    .get(slot as usize)
                    .is_some_and(|s| s.alpha == torse_content::AlphaMode::Blend)
            });
        for index in quad {
            let vertex = bytes.get(
                *index as usize * VERTEX_STRIDE..*index as usize * VERTEX_STRIDE + VERTEX_STRIDE,
            )?;
            for axis in 0..3 {
                center[axis] += f32::from_le_bytes(
                    vertex[axis * 4..axis * 4 + 4]
                        .try_into()
                        .expect("position field"),
                ) / 6.0;
            }
        }
        if center.iter().any(|v| !v.is_finite()) {
            return None;
        }
        if blend {
            faces.push(TransparentFace {
                center,
                first_index: blended.len() as u32,
            });
            blended.extend_from_slice(quad);
        } else {
            opaque.extend_from_slice(quad);
        }
    }
    let count = opaque.len() as u32;
    for face in &mut faces {
        face.first_index += count;
    }
    opaque.extend_from_slice(&blended);
    faces.shrink_to_fit();
    Some((opaque, count, faces))
}

fn face_depth(matrix: &[f32; 16], center: [f32; 3]) -> f32 {
    matrix[3] * center[0] + matrix[7] * center[1] + matrix[11] * center[2] + matrix[15]
}

fn compare_faces(
    a: (f32, torse_types::SectionPos, u32),
    b: (f32, torse_types::SectionPos, u32),
) -> std::cmp::Ordering {
    b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2))
}

impl GpuContext {
    /// Build the pipelines and camera binding on an already connected device.
    ///
    /// `color_format` is the format of the views the pass will draw into; the
    /// offscreen target picks it itself while a surface inherits it from the
    /// adapter.
    pub fn new(device: Device, queue: Queue, color_format: TextureFormat) -> Self {
        let camera_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("client camera layout"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let palette_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("client palette layout"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let camera = device.create_buffer(&BufferDescriptor {
            label: Some("client camera uniform"),
            size: 80,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let camera_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("client camera bind group"),
            layout: &camera_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera.as_entire_binding(),
            }],
        });

        let terrain_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("client terrain pipeline layout"),
                bind_group_layouts: &[Some(&camera_layout), Some(&palette_layout)],
                immediate_size: 0,
            });
        let line_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("client outline pipeline layout"),
            bind_group_layouts: &[Some(&camera_layout)],
            immediate_size: 0,
        });

        let terrain_shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("client terrain shader"),
            source: ShaderSource::Wgsl(TERRAIN_SHADER.into()),
        });
        let line_shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("client outline shader"),
            source: ShaderSource::Wgsl(LINE_SHADER.into()),
        });
        let attributes = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 0,
                shader_location: 0,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 12,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 24,
                shader_location: 2,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Uint32,
                offset: 32,
                shader_location: 3,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Uint32,
                offset: 36,
                shader_location: 4,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 40,
                shader_location: 5,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x3,
                offset: 52,
                shader_location: 6,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32,
                offset: 64,
                shader_location: 7,
            },
        ];
        let terrain_vertex_state = VertexState {
            module: &terrain_shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[Some(wgpu::VertexBufferLayout {
                array_stride: VERTEX_STRIDE as u64,
                step_mode: wgpu::VertexStepMode::Vertex,
                attributes: &attributes,
            })],
        };
        let make_pipeline = |transparent| {
            device.create_render_pipeline(&RenderPipelineDescriptor {
                label: Some("client terrain pipeline"),
                layout: Some(&terrain_pipeline_layout),
                vertex: terrain_vertex_state.clone(),
                fragment: Some(FragmentState {
                    module: &terrain_shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(ColorTargetState {
                        format: color_format,
                        blend: Some(if transparent {
                            BlendState::ALPHA_BLENDING
                        } else {
                            BlendState::REPLACE
                        }),
                        write_mask: ColorWrites::ALL,
                    })],
                }),
                primitive: PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: TextureFormat::Depth32Float,
                    depth_write_enabled: Some(!transparent),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let pipeline = make_pipeline(false);
        let transparent_pipeline = make_pipeline(true);
        // The outline only reads depth, so it never occludes the terrain it
        // marks. It ignores texture coordinates and material slots.
        let make_line = |label, compare| {
            device.create_render_pipeline(&RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&line_pipeline_layout),
                vertex: VertexState {
                    module: &line_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: VERTEX_STRIDE as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &[
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x3,
                                offset: 0,
                                shader_location: 0,
                            },
                            wgpu::VertexAttribute {
                                format: wgpu::VertexFormat::Float32x3,
                                offset: 40,
                                shader_location: 1,
                            },
                        ],
                    })],
                },
                fragment: Some(FragmentState {
                    module: &line_shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(ColorTargetState {
                        format: color_format,
                        blend: Some(BlendState::REPLACE),
                        write_mask: ColorWrites::ALL,
                    })],
                }),
                primitive: PrimitiveState {
                    topology: wgpu::PrimitiveTopology::LineList,
                    ..PrimitiveState::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: TextureFormat::Depth32Float,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(compare),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let line_pipeline = make_line("client outline pipeline", wgpu::CompareFunction::Less);
        let border_pipeline = make_line(
            "client section border pipeline",
            wgpu::CompareFunction::LessEqual,
        );
        // The sky covers the whole target behind the terrain, so it carries no
        // bind group and never writes depth.
        let sky_shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("client sky shader"),
            source: ShaderSource::Wgsl(SKY_SHADER.into()),
        });
        let sky_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("client sky pipeline layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let sky_pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("client sky pipeline"),
            layout: Some(&sky_pipeline_layout),
            vertex: VertexState {
                module: &sky_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(FragmentState {
                module: &sky_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(ColorTargetState {
                    format: color_format,
                    blend: Some(BlendState::REPLACE),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: TextureFormat::Depth32Float,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let line_buffer = device.create_buffer(&BufferDescriptor {
            label: Some("client outline vertices"),
            size: ((MAX_LINE_VERTICES + MAX_BORDER_VERTICES) * VERTEX_STRIDE) as u64,
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            device,
            queue,
            pipeline,
            transparent_pipeline,
            memory: None,
            view_projection: std::cell::Cell::new([0.0; 16]),
            eye: std::cell::Cell::new([0.0; 3]),
            focus: torse_types::SectionPos::ZERO,
            geometry_limit: 64 * 1024 * 1024,
            geometry_bytes: 0,
            geometry_evictions: 0,
            frontier: std::cell::Cell::new(None),
            draw_stats: std::cell::Cell::new(DrawStats::default()),
            line_pipeline,
            border_pipeline,
            sky_pipeline,
            camera,
            camera_bind,
            palette_layout,
            palette: None,
            camera_layout,
            terrain: BTreeMap::new(),
            line_buffer,
            line_count: 0,
            border_count: 0,
            animation_origin: Instant::now(),
        }
    }

    /// Connected device and queue.
    pub fn parts(&self) -> (&Device, &Queue) {
        (&self.device, &self.queue)
    }

    pub fn attach_memory(&mut self, pool: torse_types::MemoryPool) {
        assert!(self.terrain.is_empty());
        self.memory = Some(pool);
    }

    pub fn set_geometry_limit(&mut self, bytes: u64) -> bool {
        if bytes < self.geometry_bytes {
            return false;
        }
        self.geometry_limit = bytes;
        true
    }

    pub fn geometry_usage(&self) -> GeometryUsage {
        GeometryUsage {
            limit: self.geometry_limit,
            bytes: self.geometry_bytes,
            sections: self.terrain.len(),
            evictions: self.geometry_evictions,
        }
    }

    pub fn draw_stats(&self) -> DrawStats {
        self.draw_stats.get()
    }

    pub fn set_section_focus(&mut self, center: torse_types::SectionPos) {
        if self.focus != center {
            self.frontier.set(None);
        }
        self.focus = center;
    }

    pub fn has_section(&self, pos: torse_types::SectionPos) -> bool {
        self.terrain.contains_key(&pos)
    }

    fn admission(&self, section: torse_types::SectionPos, bytes: u64) -> Option<GeometryAdmission> {
        let memory = self
            .memory
            .as_ref()
            .map(|pool| {
                pool.reserve(
                    torse_types::MemoryClass::Codec,
                    self.terrain.len() as u64 * 128 + 512,
                    torse_types::EvictionRank::Pinned,
                )
            })
            .transpose()
            .ok()?;
        let rank = priority(section, self.focus);
        let victims = admission(
            self.geometry_limit,
            self.geometry_bytes,
            self.terrain.get(&section).map_or(0, |b| b.bytes),
            bytes,
            !self.terrain.contains_key(&section) && self.terrain.len() >= 4096,
            self.terrain
                .iter()
                .filter(|(pos, _)| {
                    **pos != section && **pos != ENTITY_SLOT && **pos != SURFACE_SLOT
                })
                .filter(|(pos, _)| {
                    section == ENTITY_SLOT
                        || section == SURFACE_SLOT
                        || priority(**pos, self.focus) > rank
                })
                .map(|(pos, b)| (*pos, b.bytes, priority(*pos, self.focus))),
        )?;
        Some(GeometryAdmission {
            victims,
            _memory: memory,
        })
    }

    /// Borrow-only upload admission. Keep deferred CPU meshes until visible and
    /// closer geometry can fit; no GPU or expanded vertex data is allocated.
    pub fn should_upload(&self, section: torse_types::SectionPos, side: u32, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        if !Bounds::section(section, side).visible(&self.view_projection.get(), self.eye.get()) {
            return false;
        }
        let old = self.terrain.get(&section).map_or(0, |b| b.bytes);
        let need_entry = !self.terrain.contains_key(&section) && self.terrain.len() >= 4096;
        if bytes as u64 <= self.geometry_limit
            && self.geometry_bytes - old <= self.geometry_limit - bytes as u64
            && !need_entry
        {
            return true;
        }
        let rank = priority(section, self.focus);
        let frontier = self.frontier.get().or_else(|| {
            let farthest = self
                .terrain
                .keys()
                .filter(|p| **p != ENTITY_SLOT && **p != SURFACE_SLOT)
                .map(|p| priority(*p, self.focus))
                .max();
            self.frontier.set(farthest);
            farthest
        });
        if frontier.is_none_or(|farthest| farthest <= rank) {
            return false;
        }
        residency::can_admit(
            self.geometry_limit,
            self.geometry_bytes,
            old,
            bytes as u64,
            need_entry,
            self.terrain
                .iter()
                .filter(|(p, _)| {
                    **p != section
                        && **p != ENTITY_SLOT
                        && **p != SURFACE_SLOT
                        && priority(**p, self.focus) > rank
                })
                .map(|(_, buffers)| buffers.bytes),
        )
    }

    /// Upload the atlas and material table used by the terrain pass.
    ///
    /// Replacing the pack clears sections because alpha classification and
    /// material slots belong to that pack. The host uploads its new meshes.
    pub fn set_palette(&mut self, palette: &Palette) {
        self.terrain.clear();
        self.frontier.set(None);
        self.geometry_bytes = 0;
        self.animation_origin = Instant::now();
        let materials = self.device.create_buffer(&BufferDescriptor {
            label: Some("client material table"),
            size: MAX_MATERIALS as u64 * SURFACE_BYTES,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(
            &materials,
            0,
            &surface_bytes(palette.surfaces(), Duration::ZERO),
        );

        let mips = palette.mips();
        let base = mips.first().expect("palette always carries the base level");
        let texture = self.device.create_texture(&TextureDescriptor {
            label: Some("client texture atlas"),
            size: Extent3d {
                width: base.width,
                height: base.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: mips.len() as u32,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8UnormSrgb,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (level_index, level) in mips.iter().enumerate() {
            let row_bytes = level.width * 4;
            let padded = align_256(row_bytes);
            let mut pixels = vec![0u8; (padded * level.height) as usize];
            for row in 0..level.height {
                let source = (row * row_bytes) as usize;
                let target = (row * padded) as usize;
                pixels[target..target + row_bytes as usize]
                    .copy_from_slice(&level.pixels[source..source + row_bytes as usize]);
            }
            self.queue.write_texture(
                TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: level_index as u32,
                    origin: Origin3d::ZERO,
                    aspect: TextureAspect::All,
                },
                &pixels,
                TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(level.height),
                },
                Extent3d {
                    width: level.width,
                    height: level.height,
                    depth_or_array_layers: 1,
                },
            );
        }

        let view = texture.create_view(&TextureViewDescriptor::default());
        let sampler = self.device.create_sampler(&SamplerDescriptor {
            label: Some("client atlas sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            address_mode_w: AddressMode::ClampToEdge,
            // Nearest magnification keeps the pixel-art look; linear
            // minification blends between the generated mip levels.
            mag_filter: FilterMode::Nearest,
            min_filter: FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..SamplerDescriptor::default()
        });
        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("client palette bind group"),
            layout: &self.palette_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: materials.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        self.palette = Some(PaletteBinding {
            materials,
            texture,
            surfaces: palette.surfaces().to_vec(),
            bind,
        });
    }

    /// Replace one section's terrain buffers, keeping other sections as-is.
    ///
    /// Empty content removes the entry instead: wgpu forbids slicing a
    /// zero-size buffer, and an empty mesh simply has nothing to draw.
    pub fn upload_section(
        &mut self,
        section: torse_types::SectionPos,
        vertex_bytes: &[u8],
        indices: &[u32],
    ) -> bool {
        if vertex_bytes.is_empty() || indices.is_empty() {
            if let Some(old) = self.terrain.remove(&section) {
                self.frontier.set(None);
                self.geometry_bytes -= old.bytes;
            }
            return true;
        }
        if indices.len() > u32::MAX as usize || !indices.len().is_multiple_of(6) {
            return false;
        }
        let Some(bounds) = Bounds::vertices(vertex_bytes, VERTEX_STRIDE) else {
            return false;
        };
        let Some(bytes) = (indices.len() as u64)
            .checked_mul(4)
            .and_then(|n| n.checked_add(vertex_bytes.len() as u64))
        else {
            return false;
        };
        if vertex_bytes.len() as u64 > self.device.limits().max_buffer_size
            || indices.len() as u64 * 4 > self.device.limits().max_buffer_size
        {
            return false;
        }
        let Some(plan) = self.admission(section, bytes) else {
            return false;
        };
        use torse_types::{EvictionRank, MemoryClass};
        let metadata = match self
            .memory
            .as_ref()
            .map(|pool| {
                pool.reserve(
                    MemoryClass::Mesh,
                    (indices.len() / 6 * size_of::<TransparentFace>() + 512) as u64,
                    EvictionRank::Pinned,
                )
            })
            .transpose()
        {
            Ok(lease) => lease,
            Err(_) => return false,
        };
        let _scratch = match self
            .memory
            .as_ref()
            .map(|pool| {
                pool.reserve(
                    MemoryClass::Codec,
                    (indices.len() * 12 + 512) as u64,
                    EvictionRank::Pinned,
                )
            })
            .transpose()
        {
            Ok(lease) => lease,
            Err(_) => return false,
        };
        let surfaces = self
            .palette
            .as_ref()
            .map_or(&[][..], |p| p.surfaces.as_slice());
        let Some((indices, opaque_count, transparent)) =
            split_faces(vertex_bytes, indices, surfaces)
        else {
            return false;
        };
        use wgpu::util::DeviceExt;
        let buffers = SectionBuffers {
            vertices: self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("client terrain vertices"),
                    contents: vertex_bytes,
                    usage: BufferUsages::VERTEX,
                }),
            indices: self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("client terrain indices"),
                    contents: &by_index_bytes(&indices),
                    usage: BufferUsages::INDEX,
                }),
            opaque_count,
            transparent,
            _memory: metadata,
            bytes,
            bounds,
        };
        // Validation and resource construction finish before any old entry is
        // removed. wgpu retains submitted resources until in-flight work ends.
        for victim in plan.victims {
            if let Some(old) = self.terrain.remove(&victim) {
                self.geometry_bytes -= old.bytes;
                self.geometry_evictions += 1;
            }
        }
        if let Some(old) = self.terrain.insert(section, buffers) {
            self.geometry_bytes -= old.bytes;
        }
        self.geometry_bytes += bytes;
        self.frontier.set(None);
        true
    }

    /// Drop the buffers of unloaded sections.
    ///
    /// The entity slot is synthetic and carries no section coordinate, so it
    /// survives every retention; callers re-upload its contents when entity
    /// positions change.
    pub fn retain_sections(&mut self, keep: &[torse_types::SectionPos]) {
        let membership: std::collections::BTreeSet<_> = keep.iter().copied().collect();
        let count = self.terrain.len();
        self.terrain.retain(|pos, _| {
            membership.contains(pos) || *pos == ENTITY_SLOT || *pos == SURFACE_SLOT
        });
        if self.terrain.len() != count {
            self.frontier.set(None);
        }
        self.geometry_bytes = self.terrain.values().map(|b| b.bytes).sum();
    }

    /// Stage the camera matrix for the next frame.
    pub fn write_camera(&self, view_projection: [f32; 16], eye: [f32; 3]) {
        self.view_projection.set(view_projection);
        self.eye.set(eye);
        self.queue
            .write_buffer(&self.camera, 0, &matrix_bytes(view_projection));
        let mut position = [0f32; 4];
        position[..3].copy_from_slice(&eye);
        let mut bytes = Vec::with_capacity(16);
        for value in position {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        self.queue.write_buffer(&self.camera, 64, &bytes);
    }

    /// Replace the selection outline. `vertex_bytes` uses the terrain layout
    /// and is truncated to the outline capacity.
    pub fn write_lines(&mut self, vertex_bytes: &[u8]) {
        let limit = vertex_bytes.len().min(MAX_LINE_VERTICES * VERTEX_STRIDE);
        self.queue
            .write_buffer(&self.line_buffer, 0, &vertex_bytes[..limit]);
        self.line_count = (limit / VERTEX_STRIDE) as u32;
    }

    pub fn write_borders(&mut self, vertex_bytes: &[u8]) {
        let limit = vertex_bytes.len().min(MAX_BORDER_VERTICES * VERTEX_STRIDE);
        self.queue.write_buffer(
            &self.line_buffer,
            (MAX_LINE_VERTICES * VERTEX_STRIDE) as u64,
            &vertex_bytes[..limit],
        );
        self.border_count = (limit / VERTEX_STRIDE) as u32;
    }

    /// Encode the one terrain pass onto any colour and depth view pair.
    pub fn encode_draw(
        &self,
        encoder: &mut CommandEncoder,
        color: &TextureView,
        depth: &TextureView,
    ) -> bool {
        let matrix = self.view_projection.get();
        let _visibility = match self
            .memory
            .as_ref()
            .map(|pool| {
                pool.reserve(
                    torse_types::MemoryClass::Codec,
                    (self.terrain.len() * size_of::<(&torse_types::SectionPos, &SectionBuffers)>()
                        + 512) as u64,
                    torse_types::EvictionRank::Pinned,
                )
            })
            .transpose()
        {
            Ok(lease) => lease,
            Err(_) => return false,
        };
        let visible: Vec<_> = self
            .terrain
            .iter()
            .filter(|(_, section)| section.bounds.visible(&matrix, self.eye.get()))
            .collect();
        let visible_sections = visible.len();
        self.draw_stats.set(DrawStats {
            visible_sections,
            culled_sections: self.terrain.len() - visible_sections,
        });
        let count = visible
            .iter()
            .map(|(_, section)| section.transparent.len())
            .sum::<usize>();
        let _sort = match self
            .memory
            .as_ref()
            .map(|pool| {
                pool.reserve(
                    torse_types::MemoryClass::Codec,
                    (count * size_of::<(f32, torse_types::SectionPos, &SectionBuffers, u32)>())
                        as u64,
                    torse_types::EvictionRank::Pinned,
                )
            })
            .transpose()
        {
            Ok(lease) => lease,
            Err(_) => return false,
        };
        let mut transparent = Vec::with_capacity(count);
        for &(pos, section) in &visible {
            for face in &section.transparent {
                transparent.push((
                    face_depth(&matrix, face.center),
                    *pos,
                    section,
                    face.first_index,
                ));
            }
        }
        transparent.sort_unstable_by(|a, b| compare_faces((a.0, a.1, a.3), (b.0, b.1, b.3)));
        if let Some(palette) = &self.palette {
            self.queue.write_buffer(
                &palette.materials,
                0,
                &surface_bytes(&palette.surfaces, self.animation_origin.elapsed()),
            );
        }
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("client terrain pass"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: color,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(SKY),
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(Operations {
                    load: LoadOp::Clear(1.0),
                    store: StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.sky_pipeline);
        pass.draw(0..3, 0..1);
        if let Some(palette) = &self.palette {
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.camera_bind, &[]);
            pass.set_bind_group(1, &palette.bind, &[]);
            for &(_, section) in &visible {
                if section.opaque_count == 0 {
                    continue;
                }
                pass.set_vertex_buffer(0, section.vertices.slice(..));
                pass.set_index_buffer(section.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..section.opaque_count, 0, 0..1);
            }
            pass.set_pipeline(&self.transparent_pipeline);
            for (_, _, section, index) in transparent {
                pass.set_vertex_buffer(0, section.vertices.slice(..));
                pass.set_index_buffer(section.indices.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(index..index + 6, 0, 0..1);
            }
        }
        if self.line_count > 0 {
            pass.set_pipeline(&self.line_pipeline);
            pass.set_bind_group(0, &self.camera_bind, &[]);
            pass.set_vertex_buffer(0, self.line_buffer.slice(..));
            pass.draw(0..self.line_count, 0..1);
        }
        if self.border_count > 0 {
            pass.set_pipeline(&self.border_pipeline);
            pass.set_bind_group(0, &self.camera_bind, &[]);
            pass.set_vertex_buffer(0, self.line_buffer.slice(..));
            pass.draw(
                MAX_LINE_VERTICES as u32..MAX_LINE_VERTICES as u32 + self.border_count,
                0..1,
            );
        }
        true
    }
}

/// Offscreen renderer.
pub struct Renderer {
    context: GpuContext,
    color: Texture,
    color_view: TextureView,
    depth_view: TextureView,
    width: u32,
    height: u32,
}

impl Renderer {
    pub fn set_geometry_limit(&mut self, bytes: u64) -> bool {
        self.context.set_geometry_limit(bytes)
    }
    pub fn geometry_usage(&self) -> GeometryUsage {
        self.context.geometry_usage()
    }
    pub fn draw_stats(&self) -> DrawStats {
        self.context.draw_stats()
    }
    pub fn attach_memory(&mut self, pool: torse_types::MemoryPool) {
        self.context.attach_memory(pool);
    }
    /// Create a headless renderer.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let session = torse_render::open_headless()?;
        let info = session.info();
        eprintln!("{}", torse_render::startup_line(&info, None));
        let (device, queue) = (session.device, session.queue);

        let color = device.create_texture(&TextureDescriptor {
            label: Some("client color target"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8UnormSrgb,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let color_view = color.create_view(&TextureViewDescriptor::default());
        let depth = device.create_texture(&TextureDescriptor {
            label: Some("client depth target"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Depth32Float,
            usage: TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let depth_view = depth.create_view(&TextureViewDescriptor::default());

        let context = GpuContext::new(device, queue, TextureFormat::Rgba8UnormSrgb);
        Ok(Self {
            context,
            color,
            color_view,
            depth_view,
            width,
            height,
        })
    }

    /// Upload the palette used by the terrain pass.
    pub fn set_palette(&mut self, palette: &Palette) {
        self.context.set_palette(palette);
    }

    /// Replace one section's terrain buffers.
    pub fn upload_section(
        &mut self,
        section: torse_types::SectionPos,
        vertex_bytes: &[u8],
        indices: &[u32],
    ) -> bool {
        self.context.upload_section(section, vertex_bytes, indices)
    }

    /// Render one frame into the offscreen target.
    pub fn render(&mut self, view_projection: [f32; 16], eye: [f32; 3]) -> bool {
        self.context.write_camera(view_projection, eye);
        let mut encoder = self
            .context
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("client frame"),
            });
        if !self
            .context
            .encode_draw(&mut encoder, &self.color_view, &self.depth_view)
        {
            return false;
        }
        self.context.queue.submit([encoder.finish()]);
        true
    }

    /// Draw the same UI used by the window into an offscreen capture.
    pub fn render_ui(
        &self,
        mode: crate::ui::Mode,
        scene: &crate::scene::Scene,
    ) -> Result<(), String> {
        let (device, queue) = self.context.parts();
        let mut layer = crate::ui::Layer::new(device, TextureFormat::Rgba8UnormSrgb)?;
        let mut state = crate::ui::State::default();
        state.set_mode(mode);
        layer.prepare(
            device,
            queue,
            [self.width, self.height],
            &state,
            crate::ui::Hud {
                scene: Some(scene),
                debug_lines: &[],
                inventory: scene.inventory_slots(),
                bar: scene.inventory_slots().into_iter().take(9).collect(),
                material: 0,
                network: false,
                ready: true,
                result: "",
                names: Vec::new(),
                sensitivity: 1.0,
            },
        );
        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("UI capture"),
        });
        layer.draw(&mut encoder, &self.color_view);
        queue.submit([encoder.finish()]);
        Ok(())
    }

    /// Copy the rendered frame into a PNG file.
    pub fn save_png(&self, path: &str) -> Result<(), String> {
        let padded_row = align_256(self.width * 4);
        let buffer_size = u64::from(padded_row) * u64::from(self.height);
        let readback = self.context.device.create_buffer(&BufferDescriptor {
            label: Some("client readback"),
            size: buffer_size,
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .context
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("client readback"),
            });
        encoder.copy_texture_to_buffer(
            TexelCopyTextureInfo {
                texture: &self.color,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            TexelCopyBufferInfo {
                buffer: &readback,
                layout: TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(self.height),
                },
            },
            Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
        self.context.queue.submit([encoder.finish()]);

        let slice = readback.slice(..);
        slice.map_async(MapMode::Read, |_| {});
        self.context
            .device
            .poll(PollType::wait_indefinitely())
            .map_err(|error| format!("device poll failed: {error}"))?;
        let view = slice
            .get_mapped_range()
            .map_err(|error| format!("map failed: {error}"))?;
        let mut pixels = Vec::with_capacity((self.width * self.height * 4) as usize);
        for row in 0..self.height {
            let start = (row * padded_row) as usize;
            let end = start + (self.width * 4) as usize;
            pixels.extend_from_slice(&view[start..end]);
        }
        drop(view);
        readback.unmap();

        write_png(path, self.width, self.height, &pixels)
    }
}

/// Encode tight RGBA rows into a PNG file.
pub fn write_png(path: &str, width: u32, height: u32, pixels: &[u8]) -> Result<(), String> {
    let file = File::create(path).map_err(|error| format!("create failed: {error}"))?;
    let mut writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(&mut writer, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder
        .write_header()
        .map_err(|error| format!("png header failed: {error}"))?;
    png_writer
        .write_image_data(pixels)
        .map_err(|error| format!("png write failed: {error}"))?;
    Ok(())
}

fn matrix_bytes(matrix: [f32; 16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(64);
    for value in matrix {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn by_index_bytes(indices: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(indices.len() * 4);
    for value in indices {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn surface_bytes(surfaces: &[crate::texture::Surface], elapsed: Duration) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(MAX_MATERIALS * SURFACE_BYTES as usize);
    for surface in surfaces {
        let resolved = torse_assets::texture::MaterialSurface {
            rect: surface.rect,
            tint: surface.tint,
            tint_source: surface.tint_source,
            animation: surface.animation,
        };
        let frames = resolved.frame_sample(elapsed);
        for value in frames.current {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in surface.tint {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let mode: f32 = match surface.alpha {
            torse_content::AlphaMode::Opaque => 0.0,
            torse_content::AlphaMode::Mask => 1.0,
            torse_content::AlphaMode::Blend => 2.0,
        };
        bytes.extend_from_slice(&mode.to_le_bytes());
        for value in frames.next {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let animated = if surface.animation.is_some() {
            1.0
        } else {
            0.0
        };
        for value in [frames.blend, animated, 0.0, 0.0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
    }
    bytes.resize(MAX_MATERIALS * SURFACE_BYTES as usize, 0);
    bytes
}

const fn align_256(value: u32) -> u32 {
    (value + 255) & !255
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "gpu-tests")]
    #[test]
    fn minified_tiles_keep_their_color_and_world_borders_obey_depth() {
        use crate::scene::{Scene, Vertex};
        use torse_assets::texture::MipLevel;
        use torse_types::SectionPos;
        let levels = (0..3)
            .map(|level| {
                let width = 64 >> level;
                let height = 32 >> level;
                let mut pixels = vec![32; width * height * 4];
                for pixel in pixels.chunks_exact_mut(4) {
                    pixel[3] = 255;
                }
                for y in (8 >> level)..(24 >> level) {
                    for x in (16 >> level)..(32 >> level) {
                        pixels[(y * width + x) * 4..(y * width + x + 1) * 4].fill(255);
                    }
                }
                MipLevel {
                    width: width as u32,
                    height: height as u32,
                    pixels,
                }
            })
            .collect();
        let surface = crate::texture::Surface {
            rect: [0.25, 0.25, 0.25, 0.5],
            tint: [1.; 3],
            tint_source: None,
            animation: None,
            alpha: torse_content::AlphaMode::Opaque,
        };
        let mut renderer = Renderer::new(128, 128).unwrap();
        renderer.set_palette(&Palette::mips_for_tests(vec![surface], levels));
        let positions = [
            [-1., -1., 0.5],
            [1., -1., 0.5],
            [1., 1., 0.5],
            [-1., 1., 0.5],
        ];
        // Offset the repeats so pixel centers hit a tile boundary at mip two.
        let uv = [
            [0.875, 0.875],
            [32.875, 0.875],
            [32.875, 32.875],
            [0.875, 32.875],
        ];
        let vertices = std::array::from_fn::<_, 4, _>(|i| Vertex {
            position: positions[i],
            normal: [0., 0., -1.],
            uv: uv[i],
            material: 0,
            overlay_material: u32::MAX,
            tint: [1.; 3],
            overlay_tint: [1.; 3],
            light: 1.,
        });
        assert!(renderer.upload_section(
            SectionPos::ZERO,
            &Scene::vertex_bytes(&vertices),
            &[0, 1, 2, 0, 2, 3]
        ));
        let matrix = [
            1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.,
        ];
        let folder =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.work/cln014-gpu");
        std::fs::create_dir_all(&folder).unwrap();
        for (name, depth) in [
            ("tile", None),
            ("hidden-border", Some(0.75)),
            ("visible-border", Some(0.25)),
        ] {
            let lines: Vec<_> = depth
                .into_iter()
                .flat_map(|z| {
                    [-0.8, 0.8].map(|x| Vertex {
                        position: [x, 0., z],
                        tint: [1., 0., 0.],
                        ..Vertex::default()
                    })
                })
                .collect();
            renderer.context.write_borders(&Scene::vertex_bytes(&lines));
            assert!(renderer.render(matrix, [0.; 3]));
            let path = folder.join(format!("{name}.png"));
            renderer.save_png(path.to_str().unwrap()).unwrap();
            let image =
                torse_assets::texture::Atlas::decode(&std::fs::read(path).unwrap()).unwrap();
            let red = image
                .pixels()
                .chunks_exact(4)
                .filter(|p| p[0] > 200 && p[1] < 20)
                .count();
            if name == "visible-border" {
                assert!(red > 50);
            } else {
                assert_eq!(red, 0);
                assert!(
                    image
                        .pixels()
                        .chunks_exact(4)
                        .all(|p| p[0] > 240 && p[1] > 240),
                    "atlas gutter leaked at minification"
                );
            }
        }
    }

    #[cfg(feature = "gpu-tests")]
    #[test]
    fn gpu_geometry_budget_preserves_replacements_and_recovers_evicted_sections() {
        use crate::scene::{Scene, Vertex};
        use torse_types::{MemoryBudget, MemoryPool, SectionPos};
        let pool = MemoryPool::new(MemoryBudget::new(32 * 1024 * 1024));
        let mut renderer = Renderer::new(64, 64).unwrap();
        renderer.attach_memory(pool.clone());
        renderer.set_palette(&Palette::pixels_for_tests(
            vec![crate::texture::Surface {
                rect: [0., 0., 1., 1.],
                tint: [1.; 3],
                tint_source: None,
                animation: None,
                alpha: torse_content::AlphaMode::Opaque,
            }],
            [255; 4],
        ));
        let quad = |offset: f32| {
            Scene::vertex_bytes(
                &[
                    [-0.5 + offset, -0.5, 0.5],
                    [0.5 + offset, -0.5, 0.5],
                    [0.5 + offset, 0.5, 0.5],
                    [-0.5 + offset, 0.5, 0.5],
                ]
                .map(|position| Vertex {
                    position,
                    normal: [0., 0., -1.],
                    uv: [0.5; 2],
                    material: 0,
                    overlay_material: u32::MAX,
                    tint: [1.; 3],
                    overlay_tint: [1.; 3],
                    light: 1.,
                }),
            )
        };
        let bytes = quad(0.);
        let indices = [0, 1, 2, 0, 2, 3];
        let payload = bytes.len() as u64 + 24;
        assert!(renderer.set_geometry_limit(payload * 2));
        let near = SectionPos::ZERO;
        let far = SectionPos::new(8, 0, 0);
        let middle = SectionPos::new(1, 0, 0);
        assert!(renderer.upload_section(near, &bytes, &indices));
        assert!(renderer.upload_section(far, &quad(4.), &indices));
        let matrix = [
            1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1.,
        ];
        assert!(renderer.render(matrix, [10.; 3]));
        assert_eq!(renderer.draw_stats().visible_sections, 1);
        assert_eq!(renderer.draw_stats().culled_sections, 1);
        let folder =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.work/krn031-gpu");
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("01-frustum.png");
        renderer.save_png(path.to_str().unwrap()).unwrap();
        let image = torse_assets::texture::Atlas::decode(&std::fs::read(path).unwrap()).unwrap();
        assert!(image.pixels()[(32 * 64 + 32) * 4] > 240);
        assert!(renderer.upload_section(middle, &bytes, &indices));
        assert!(!renderer.context.has_section(far));
        assert_eq!(renderer.geometry_usage().evictions, 1);
        assert_eq!(renderer.geometry_usage().bytes, payload * 2);
        assert!(!renderer.upload_section(SectionPos::new(9, 0, 0), &bytes, &indices));
        assert!(!renderer.upload_section(near, &bytes, &[999; 6]));
        let oversized = [bytes.clone(), bytes.clone(), bytes.clone()].concat();
        assert!(!renderer.upload_section(near, &oversized, &indices));
        assert!(renderer.context.has_section(near));
        assert!(renderer.context.has_section(middle));
        assert!(!renderer.set_geometry_limit(payload));
        renderer.context.set_section_focus(far);
        assert!(renderer.upload_section(far, &bytes, &indices));
        assert!(renderer.context.has_section(far));
        assert!(!renderer.context.has_section(near));
        assert!(renderer.upload_section(ENTITY_SLOT, &bytes, &indices));
        renderer.context.set_section_focus(near);
        assert!(renderer.upload_section(near, &bytes, &indices));
        assert!(renderer.context.has_section(ENTITY_SLOT));
        renderer.context.retain_sections(&[]);
        assert_eq!(renderer.geometry_usage().bytes, payload);
        assert!(renderer.upload_section(ENTITY_SLOT, &[], &[]));
        assert_eq!(renderer.geometry_usage().bytes, 0);
        drop(renderer);
        assert_eq!(pool.usage().total, 0);
    }

    #[cfg(feature = "gpu-tests")]
    #[test]
    fn gpu_brightness_and_global_transparency_are_order_independent() {
        use crate::scene::{Scene, Vertex};
        use torse_types::SectionPos;
        let folder =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.work/krn026-gpu");
        std::fs::create_dir_all(&folder).unwrap();
        let pool = torse_types::MemoryPool::new(torse_types::MemoryBudget::new(128 * 1024 * 1024));
        let mut renderer = Renderer::new(128, 64).unwrap();
        renderer.attach_memory(pool.clone());
        let surface = crate::texture::Surface {
            rect: [0.0, 0.0, 1.0, 1.0],
            tint: [1.0; 3],
            tint_source: None,
            animation: None,
            alpha: torse_content::AlphaMode::Blend,
        };
        renderer.set_palette(&Palette::pixels_for_tests(
            vec![surface],
            [255, 255, 255, 128],
        ));
        let indices = [0, 1, 2, 0, 2, 3];
        let quad = |z: f32, tint, light, min: f32, max: f32| {
            [
                [min * z, -0.8 * z, z],
                [max * z, -0.8 * z, z],
                [max * z, 0.8 * z, z],
                [min * z, 0.8 * z, z],
            ]
            .map(|position| Vertex {
                position,
                normal: [0.0, 0.0, -1.0],
                material: 0,
                overlay_material: u32::MAX,
                tint,
                overlay_tint: [1.0; 3],
                light,
                uv: [0.5; 2],
            })
        };
        let mut matrix = [0.0; 16];
        matrix[0] = 1.0;
        matrix[5] = 1.0;
        matrix[10] = 1.0;
        matrix[11] = 1.0;
        matrix[14] = -0.5;
        let near = Scene::vertex_bytes(&quad(1.0, [1.0, 0.0, 0.0], 1.0, -0.8, 0.8));
        let far = Scene::vertex_bytes(&quad(2.0, [0.0, 0.0, 1.0], 1.0, -0.8, 0.8));
        let mut center = Vec::new();
        for reversed in [false, true] {
            let (a, b) = if reversed {
                (&far, &near)
            } else {
                (&near, &far)
            };
            assert!(renderer.upload_section(SectionPos::ZERO, a, &indices));
            assert!(renderer.upload_section(SectionPos::new(1, 0, 0), b, &indices));
            assert!(renderer.render(matrix, [0.0; 3]));
            let path = folder.join(if reversed {
                "02-transparent-reversed.png"
            } else {
                "01-transparent.png"
            });
            renderer.save_png(path.to_str().unwrap()).unwrap();
            let image =
                torse_assets::texture::Atlas::decode(&std::fs::read(path).unwrap()).unwrap();
            let pixel = &image.pixels()[(32 * 128 + 64) * 4..(32 * 128 + 64) * 4 + 3];
            assert!(pixel[0] > pixel[2]);
            center.push(pixel.to_vec());
        }
        assert_eq!(center[0], center[1]);
        renderer.set_palette(&Palette::pixels_for_tests(
            vec![crate::texture::Surface {
                alpha: torse_content::AlphaMode::Opaque,
                ..surface
            }],
            [255; 4],
        ));
        assert!(renderer.upload_section(
            SectionPos::ZERO,
            &Scene::vertex_bytes(&quad(1.0, [1.0; 3], 0.2, -0.9, -0.1)),
            &indices
        ));
        assert!(renderer.upload_section(
            SectionPos::new(1, 0, 0),
            &Scene::vertex_bytes(&quad(1.0, [1.0; 3], 1.0, 0.1, 0.9)),
            &indices
        ));
        assert!(renderer.render(matrix, [0.0; 3]));
        let path = folder.join("03-brightness.png");
        renderer.save_png(path.to_str().unwrap()).unwrap();
        let image = torse_assets::texture::Atlas::decode(&std::fs::read(path).unwrap()).unwrap();
        let left = image.pixels()[(32 * 128 + 32) * 4];
        let right = image.pixels()[(32 * 128 + 96) * 4];
        assert!(right > left + 100);
        drop(renderer);
        assert_eq!(pool.usage().total, 0);
    }

    #[test]
    fn opaque_and_masked_faces_precede_blended_faces_and_invalid_uploads_fail() {
        let base = Palette::single_for_tests(
            [0.0, 0.0, 1.0, 1.0],
            [1.0; 3],
            torse_content::AlphaMode::Opaque,
        )
        .surfaces()[0];
        let surfaces = [
            base,
            crate::texture::Surface {
                alpha: torse_content::AlphaMode::Blend,
                ..base
            },
            crate::texture::Surface {
                alpha: torse_content::AlphaMode::Mask,
                ..base
            },
        ];
        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        for (quad, material) in [1, 0, 2].into_iter().enumerate() {
            let z = quad as f32;
            for position in [[0.0, 0.0, z], [1.0, 0.0, z], [1.0, 1.0, z], [0.0, 1.0, z]] {
                vertices.push(crate::scene::Vertex {
                    position,
                    material,
                    overlay_material: u32::MAX,
                    light: 1.0,
                    ..Default::default()
                });
            }
            let start = (quad * 4) as u32;
            indices.extend([start, start + 1, start + 2, start, start + 2, start + 3]);
        }
        let bytes = crate::scene::Scene::vertex_bytes(&vertices);
        let (sorted, opaque, faces) = split_faces(&bytes, &indices, &surfaces).unwrap();
        assert_eq!(opaque, 12);
        assert_eq!(faces.len(), 1);
        assert_eq!(faces[0].first_index, 12);
        assert_eq!(&sorted[12..], &indices[..6]);
        assert!((faces[0].center[0] - 0.5).abs() < 1e-6);
        assert!(split_faces(&bytes[..bytes.len() - 1], &indices, &surfaces).is_none());
        assert!(split_faces(&bytes, &[999; 6], &surfaces).is_none());
    }
    #[test]
    fn transparent_order_uses_camera_depth_and_changes_when_view_reverses() {
        let mut matrix = [0.0; 16];
        matrix[11] = 1.0;
        let mut faces = [
            (
                face_depth(&matrix, [0.0, 0.0, 2.0]),
                torse_types::SectionPos::ZERO,
                0,
            ),
            (
                face_depth(&matrix, [0.0, 0.0, 10.0]),
                torse_types::SectionPos::new(1, 0, 0),
                6,
            ),
        ];
        faces.sort_by(|a, b| compare_faces(*a, *b));
        assert_eq!(faces[0].2, 6);
        matrix[11] = -1.0;
        matrix[15] = 20.0;
        for face in &mut faces {
            face.0 = face_depth(&matrix, [0.0, 0.0, if face.2 == 6 { 10.0 } else { 2.0 }]);
        }
        faces.sort_by(|a, b| compare_faces(*a, *b));
        assert_eq!(faces[0].2, 0);
    }

    #[test]
    fn write_png_reports_file_errors() {
        // A path with a missing directory must surface the OS error, not panic.
        let result = write_png("no-such-dir/frame.png", 1, 1, &[0, 0, 0, 255]);
        assert!(result.is_err());
    }

    #[test]
    fn surface_table_is_padded_to_the_budget() {
        let palette = Palette::empty_for_tests();
        let bytes = surface_bytes(palette.surfaces(), Duration::ZERO);
        assert_eq!(bytes.len(), MAX_MATERIALS * SURFACE_BYTES as usize);
    }

    #[test]
    fn surface_bytes_encode_rect_tint_and_alpha_mode() {
        let palette = Palette::single_for_tests(
            [0.0, 0.0, 0.5, 0.5],
            [0.2, 0.4, 0.6],
            torse_content::AlphaMode::Blend,
        );
        let bytes = surface_bytes(palette.surfaces(), Duration::ZERO);
        let read =
            |offset: usize| f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        assert_eq!(read(0), 0.0);
        assert_eq!(read(8), 0.5);
        assert_eq!(read(12), 0.5);
        assert_eq!(read(16), 0.2);
        assert_eq!(read(24), 0.6);
        assert_eq!(read(28), 2.0);
        assert_eq!(read(32), 0.0);
        assert_eq!(read(48), 0.0);
        assert_eq!(read(52), 0.0);
    }

    #[test]
    fn animated_surface_table_updates_frame_rectangles_and_blend() {
        const FRAME_DURATION: Duration = Duration::from_millis(100);
        let surface = crate::texture::Surface {
            rect: [0.25, 0.0, 0.5, 1.0],
            tint: [1.0; 3],
            tint_source: None,
            animation: Some(torse_assets::texture::MaterialAnimation {
                frame_count: 4,
                frame_duration: FRAME_DURATION,
                frame_order: None,
                frame_durations: None,
                interpolate: true,
            }),
            alpha: torse_content::AlphaMode::Opaque,
        };
        let initial = surface_bytes(&[surface], Duration::ZERO);
        let halfway = surface_bytes(&[surface], Duration::from_millis(50));
        let read = |bytes: &[u8], offset: usize| {
            f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
        };
        assert_eq!(read(&initial, 12), 0.25);
        assert_eq!(read(&initial, 44), 0.25);
        assert_eq!(read(&initial, 48), 0.0);
        assert_eq!(read(&initial, 52), 1.0);
        assert!((read(&halfway, 48) - 0.5).abs() < 1e-6);
    }
}
