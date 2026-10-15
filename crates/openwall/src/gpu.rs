// SPDX-License-Identifier: AGPL-3.0-or-later
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, mpsc},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use bytemuck::{Pod, Zeroable};
use openwall_core::Scene;
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex {
    position: [f32; 2],
    uv: [f32; 2],
    opacity: f32,
}

pub struct Gpu {
    pub _instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    pub fn instance() -> wgpu::Instance {
        wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: if cfg!(target_os = "macos") {
                wgpu::Backends::METAL
            } else {
                wgpu::Backends::VULKAN
            },
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        })
    }

    pub async fn new(
        instance: wgpu::Instance,
        surface: Option<&wgpu::Surface<'_>>,
    ) -> Result<Self> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::LowPower,
                compatible_surface: surface,
                force_fallback_adapter: false,
                ..Default::default()
            })
            .await
            .context("no compatible GPU adapter")?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("OpenWall"),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                ..Default::default()
            })
            .await
            .context("could not create GPU device")?;
        eprintln!(
            "OpenWall GPU: {} ({:?})",
            adapter.get_info().name,
            adapter.get_info().backend
        );
        Ok(Self {
            _instance: instance,
            adapter,
            device,
            queue,
        })
    }

    pub fn configure(
        &self,
        surface: &wgpu::Surface<'_>,
        width: u32,
        height: u32,
    ) -> Result<wgpu::SurfaceConfiguration> {
        let mut config = surface
            .get_default_config(&self.adapter, width.max(1), height.max(1))
            .context("surface is incompatible with GPU")?;
        if let Some(format) = surface
            .get_capabilities(&self.adapter)
            .formats
            .iter()
            .find(|x| x.is_srgb())
        {
            config.format = *format;
        }
        config.present_mode = wgpu::PresentMode::Fifo;
        config.desired_maximum_frame_latency = 2;
        surface.configure(&self.device, &config);
        Ok(config)
    }
}

pub struct Painter {
    pub format: wgpu::TextureFormat,
    pipeline: wgpu::RenderPipeline,
    layers: Vec<DrawLayer>,
    background: wgpu::Color,
}

struct DrawLayer {
    vertices: wgpu::Buffer,
    bindings: wgpu::BindGroup,
}

impl Painter {
    pub fn new(
        gpu: &Gpu,
        scene: &Scene,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("OpenWall original sprite shader"),
                source: wgpu::ShaderSource::Wgsl(include_str!("sprite.wgsl").into()),
            });
        let attributes = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32];
        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("OpenWall sprite pipeline"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vertex_main"),
                    compilation_options: Default::default(),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Vertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &attributes,
                    })],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fragment_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            });
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("OpenWall sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let layout = pipeline.get_bind_group_layout(0);
        let mut textures: BTreeMap<usize, wgpu::BindGroup> = BTreeMap::new();
        let layers = scene
            .layers
            .iter()
            .map(|layer| {
                let key = Arc::as_ptr(&layer.pixels) as usize;
                let bindings = textures
                    .entry(key)
                    .or_insert_with(|| {
                        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
                            label: Some("wallpaper texture"),
                            size: wgpu::Extent3d {
                                width: layer.pixels.width(),
                                height: layer.pixels.height(),
                                depth_or_array_layers: 1,
                            },
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: wgpu::TextureFormat::Rgba8UnormSrgb,
                            usage: wgpu::TextureUsages::TEXTURE_BINDING
                                | wgpu::TextureUsages::COPY_DST,
                            view_formats: &[],
                        });
                        gpu.queue.write_texture(
                            texture.as_image_copy(),
                            layer.pixels.as_raw(),
                            wgpu::TexelCopyBufferLayout {
                                offset: 0,
                                bytes_per_row: Some(layer.pixels.width() * 4),
                                rows_per_image: None,
                            },
                            texture.size(),
                        );
                        let view = texture.create_view(&Default::default());
                        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: None,
                            layout: &layout,
                            entries: &[
                                wgpu::BindGroupEntry {
                                    binding: 0,
                                    resource: wgpu::BindingResource::TextureView(&view),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 1,
                                    resource: wgpu::BindingResource::Sampler(&sampler),
                                },
                            ],
                        })
                    })
                    .clone();
                let vertices = vertices(layer, scene, width, height);
                let vertices = gpu
                    .device
                    .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: None,
                        contents: bytemuck::cast_slice(&vertices),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
                DrawLayer { vertices, bindings }
            })
            .collect();
        Self {
            format,
            pipeline,
            layers,
            background: wgpu::Color {
                r: scene.background[0],
                g: scene.background[1],
                b: scene.background[2],
                a: scene.background[3],
            },
        }
    }

    pub fn resize(&self, gpu: &Gpu, scene: &Scene, width: u32, height: u32) {
        for (draw, layer) in self.layers.iter().zip(&scene.layers) {
            gpu.queue.write_buffer(
                &draw.vertices,
                0,
                bytemuck::cast_slice(&vertices(layer, scene, width, height)),
            );
        }
    }

    pub fn encode(&self, encoder: &mut wgpu::CommandEncoder, view: &wgpu::TextureView) {
        let attachment = Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(self.background),
                store: wgpu::StoreOp::Store,
            },
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("OpenWall draw"),
            color_attachments: &[attachment],
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        for layer in &self.layers {
            pass.set_bind_group(0, &layer.bindings, &[]);
            pass.set_vertex_buffer(0, layer.vertices.slice(..));
            pass.draw(0..6, 0..1);
        }
    }

    pub fn present(&self, gpu: &Gpu, surface: &wgpu::Surface<'_>) -> Result<bool> {
        let frame = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Timeout
            | wgpu::CurrentSurfaceTexture::Occluded => return Ok(false),
            other => anyhow::bail!("cannot acquire wallpaper frame: {other:?}"),
        };
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        self.encode(&mut encoder, &view);
        gpu.queue.submit([encoder.finish()]);
        gpu.queue.present(frame);
        Ok(true)
    }
}

fn vertices(layer: &openwall_core::Layer, scene: &Scene, width: u32, height: u32) -> [Vertex; 6] {
    let scale =
        (width as f32 / scene.report.width as f32).max(height as f32 / scene.report.height as f32);
    let (sin, cos) = layer.rotation.sin_cos();
    [
        (-0.5, -0.5, 0.0, 1.0),
        (0.5, -0.5, 1.0, 1.0),
        (0.5, 0.5, 1.0, 0.0),
        (-0.5, -0.5, 0.0, 1.0),
        (0.5, 0.5, 1.0, 0.0),
        (-0.5, 0.5, 0.0, 0.0),
    ]
    .map(|(x, y, u, v)| {
        let (x, y) = (x * layer.size[0], y * layer.size[1]);
        let px = (layer.center[0] - scene.report.width as f32 / 2.0 + cos * x - sin * y) * scale;
        let py = (layer.center[1] - scene.report.height as f32 / 2.0 + sin * x + cos * y) * scale;
        Vertex {
            position: [px * 2.0 / width as f32, py * 2.0 / height as f32],
            uv: [u, v],
            opacity: layer.opacity,
        }
    })
}

pub fn render_png(scene: &Scene, path: &Path, width: u32, height: u32) -> Result<()> {
    ensure!(
        width > 0 && height > 0 && width <= 8192 && height <= 8192,
        "render dimensions must be 1..8192"
    );
    let gpu = pollster::block_on(Gpu::new(Gpu::instance(), None))?;
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("OpenWall offline render"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let painter = Painter::new(&gpu, scene, texture.format(), width, height);
    let stride = (width * 4).div_ceil(256) * 256;
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: stride as u64 * height as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    painter.encode(&mut encoder, &texture.create_view(&Default::default()));
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: Some(height),
            },
        },
        texture.size(),
    );
    gpu.queue.submit([encoder.finish()]);
    let (tx, rx) = mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
    gpu.device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: Some(Duration::from_secs(10)),
    })?;
    rx.recv_timeout(Duration::from_secs(10))??;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    {
        let mapped = buffer.slice(..).get_mapped_range()?;
        for row in mapped.chunks_exact(stride as usize) {
            rgba.extend_from_slice(&row[..width as usize * 4]);
        }
    }
    buffer.unmap();
    image::save_buffer(path, &rgba, width, height, image::ColorType::Rgba8)
        .context("could not save rendered PNG")
}
