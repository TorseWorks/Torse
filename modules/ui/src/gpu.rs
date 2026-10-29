//! Alpha-blended overlay over the existing world render target.
use super::canvas::Canvas;
use wgpu::*;

const SHADER: &str = r#"
@group(0) @binding(0) var image: texture_2d<f32>;
@group(0) @binding(1) var<uniform> params: vec4<u32>;
@vertex fn vertex(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let points = array<vec2<f32>,3>(vec2(-1.0,-1.0),vec2(3.0,-1.0),vec2(-1.0,3.0));
    return vec4(points[i],0.0,1.0);
}
@fragment fn fragment(@builtin(position) p: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(image, vec2<i32>(p.xy) / i32(params.x), 0);
}
"#;

pub struct Overlay {
    pipeline: RenderPipeline,
    layout: BindGroupLayout,
    params: Buffer,
    image: Texture,
    binding: BindGroup,
}
impl Overlay {
    pub fn new(device: &Device, format: TextureFormat) -> Self {
        let layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("UI overlay bindings"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: false },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("UI overlay shader"),
            source: ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("UI overlay layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("UI overlay pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vertex"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fragment"),
                compilation_options: Default::default(),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: Some(BlendState::ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: PrimitiveState::default(),
            depth_stencil: None,
            multisample: MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let params = device.create_buffer(&BufferDescriptor {
            label: Some("UI scale"),
            size: 16,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let image = Self::texture(device, 1, 1);
        let binding = Self::bind(device, &layout, &image, &params);
        Self {
            pipeline,
            layout,
            params,
            image,
            binding,
        }
    }
    fn texture(device: &Device, width: u32, height: u32) -> Texture {
        device.create_texture(&TextureDescriptor {
            label: Some("UI logical pixels"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8UnormSrgb,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }
    fn bind(
        device: &Device,
        layout: &BindGroupLayout,
        image: &Texture,
        params: &Buffer,
    ) -> BindGroup {
        device.create_bind_group(&BindGroupDescriptor {
            label: Some("UI overlay resources"),
            layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(
                        &image.create_view(&TextureViewDescriptor::default()),
                    ),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: params.as_entire_binding(),
                },
            ],
        })
    }
    pub fn upload(
        &mut self,
        device: &Device,
        queue: &Queue,
        canvas: &Canvas,
        scale: u32,
    ) -> Result<(), String> {
        let pixel_width = canvas.pixel_width();
        let pixel_height = canvas.pixel_height();
        if pixel_width > device.limits().max_texture_dimension_2d
            || pixel_height > device.limits().max_texture_dimension_2d
            || scale == 0
        {
            return Err("UI overlay exceeds device dimensions or has zero scale".into());
        }
        if self.image.width() != pixel_width || self.image.height() != pixel_height {
            self.image = Self::texture(device, pixel_width, pixel_height);
            self.binding = Self::bind(device, &self.layout, &self.image, &self.params);
        }
        let mut params = [0; 16];
        params[..4].copy_from_slice(&scale.to_le_bytes());
        queue.write_buffer(&self.params, 0, &params);
        queue.write_texture(
            TexelCopyTextureInfo {
                texture: &self.image,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            canvas.as_pixels(),
            TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(pixel_width * 4),
                rows_per_image: Some(pixel_height),
            },
            Extent3d {
                width: pixel_width,
                height: pixel_height,
                depth_or_array_layers: 1,
            },
        );
        Ok(())
    }
    pub fn draw(&self, encoder: &mut CommandEncoder, target: &TextureView) {
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("UI overlay pass"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Load,
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.binding, &[]);
        pass.draw(0..3, 0..1);
    }
}
