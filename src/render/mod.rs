//! GPU presentation of decoded frames.
//!
//! The video plane is ours, not a toolkit widget: iced hands us a wgpu render
//! pass scissored to the widget bounds, and we draw one triangle into it with a
//! shader that does the YUV -> RGB conversion. That keeps colour handling in one
//! place and leaves room for tone mapping and better scaling later.

use std::num::NonZeroU64;

use iced::advanced::graphics::Viewport;
use iced::mouse;
use iced::widget::shader::{self, Primitive};
use iced::Rectangle;

use crate::engine::FrameSlot;

/// Uniform block shared with `nv12.wgsl`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    scale: [f32; 2],
    srgb: f32,
    _pad: f32,
}

/// The widget: a video surface backed by whatever is currently in the slot.
#[derive(Debug)]
pub struct VideoSurface {
    slot: FrameSlot,
}

impl VideoSurface {
    pub fn new(slot: FrameSlot) -> Self {
        Self { slot }
    }
}

impl<Message> shader::Program<Message> for VideoSurface {
    type State = ();
    type Primitive = VideoPrimitive;

    fn draw(&self, _state: &Self::State, _cursor: mouse::Cursor, _bounds: Rectangle) -> VideoPrimitive {
        VideoPrimitive {
            slot: self.slot.clone(),
        }
    }
}

#[derive(Debug)]
pub struct VideoPrimitive {
    slot: FrameSlot,
}

impl Primitive for VideoPrimitive {
    type Pipeline = VideoPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        _viewport: &Viewport,
    ) {
        pipeline.upload(device, queue, &self.slot);
        pipeline.fit(queue, bounds, &self.slot);
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        pipeline.draw(render_pass);
        // We always own this pass; nothing to hand back to iced.
        true
    }
}

/// Textures for one video size. Rebuilt whenever the stream resolution changes.
struct Planes {
    width: u32,
    height: u32,
    luma: wgpu::Texture,
    chroma: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

pub struct VideoPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,
    planes: Option<Planes>,
    /// Generation of the frame currently on the GPU.
    uploaded: u64,
    /// Uploads since the last report, for measuring the real display rate.
    shown: u64,
    reported_at: Option<std::time::Instant>,
    srgb: bool,
}

impl shader::Pipeline for VideoPipeline {
    fn new(device: &wgpu::Device, _queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("myvid nv12"),
            source: wgpu::ShaderSource::Wgsl(include_str!("nv12.wgsl").into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("myvid video bind group layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: NonZeroU64::new(
                                std::mem::size_of::<Uniforms>() as u64
                            ),
                        },
                        count: None,
                    },
                    plane_binding(1),
                    plane_binding(2),
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("myvid video pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("myvid video pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("myvid video sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("myvid video uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            bind_group_layout,
            sampler,
            uniforms,
            planes: None,
            uploaded: 0,
            shown: 0,
            reported_at: None,
            srgb: format.is_srgb(),
        }
    }
}

impl VideoPipeline {
    /// Push the newest decoded frame to the GPU, reallocating if the stream
    /// resolution changed.
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, slot: &FrameSlot) {
        slot.read(|frame| {
            if frame.is_empty() {
                self.planes = None;
                self.uploaded = 0;
                return;
            }

            let (width, height) = (frame.width(), frame.height());

            let resized = self
                .planes
                .as_ref()
                .map(|p| p.width != width || p.height != height)
                .unwrap_or(true);

            if resized {
                self.planes = Some(self.allocate(device, width, height));
                self.uploaded = 0;
            }

            if self.uploaded == frame.generation() {
                return;
            }

            let Some(planes) = self.planes.as_ref() else {
                return;
            };

            // Straight from decoder memory; `write_texture` honours an arbitrary
            // row stride, so no repacking is needed.
            if let Some((luma, stride)) = frame.plane(0) {
                write_plane(queue, &planes.luma, luma, stride, width, height);
            }
            if let Some((chroma, stride)) = frame.plane(1) {
                write_plane(
                    queue,
                    &planes.chroma,
                    chroma,
                    stride,
                    width.div_ceil(2),
                    height.div_ceil(2),
                );
            }

            self.uploaded = frame.generation();

            if std::env::var_os("MYVID_DIAG").is_some() {
                self.shown += 1;
                let started = self.reported_at.get_or_insert_with(std::time::Instant::now);
                if started.elapsed() >= std::time::Duration::from_secs(1) {
                    eprintln!("[diag] displayed {} frames in the last second", self.shown);
                    self.shown = 0;
                    self.reported_at = Some(std::time::Instant::now());
                }
            }
        });
    }

    fn allocate(&self, device: &wgpu::Device, width: u32, height: u32) -> Planes {
        let luma = plane_texture(device, "myvid luma", width, height, wgpu::TextureFormat::R8Unorm);
        let chroma = plane_texture(
            device,
            "myvid chroma",
            width.div_ceil(2),
            height.div_ceil(2),
            wgpu::TextureFormat::Rg8Unorm,
        );

        let luma_view = luma.create_view(&wgpu::TextureViewDescriptor::default());
        let chroma_view = chroma.create_view(&wgpu::TextureViewDescriptor::default());

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("myvid video bind group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&luma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&chroma_view),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        Planes {
            width,
            height,
            luma,
            chroma,
            bind_group,
        }
    }

    /// Letterbox the frame inside the widget bounds without cropping.
    fn fit(&self, queue: &wgpu::Queue, bounds: &Rectangle, slot: &FrameSlot) {
        let scale = match slot.aspect() {
            Some(video) if bounds.width > 0.0 && bounds.height > 0.0 => {
                let box_aspect = bounds.width / bounds.height;
                if video > box_aspect {
                    [1.0, box_aspect / video]
                } else {
                    [video / box_aspect, 1.0]
                }
            }
            _ => [1.0, 1.0],
        };

        let uniforms = Uniforms {
            scale,
            srgb: if self.srgb { 1.0 } else { 0.0 },
            _pad: 0.0,
        };

        queue.write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&uniforms));
    }

    fn draw(&self, render_pass: &mut wgpu::RenderPass<'_>) {
        let Some(planes) = self.planes.as_ref() else {
            return;
        };

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &planes.bind_group, &[]);
        render_pass.draw(0..3, 0..1);
    }
}

fn plane_binding(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn plane_texture(
    device: &wgpu::Device,
    label: &str,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn write_plane(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    data: &[u8],
    pitch: u32,
    width: u32,
    height: u32,
) {
    if width == 0 || height == 0 || data.is_empty() {
        return;
    }

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(pitch),
            rows_per_image: Some(height),
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
}
