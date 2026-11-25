use anyhow::{bail, Context};
use eframe::egui::{self, epaint};
use metal_rust::metal::{
    BlendFactor, BlendOperation, Device, PixelFormat, PrimitiveType, Region, RenderStages,
    ResidencyAllocation, ResidencySet, ResidencySetDescriptor, ResourceOptions, SamplerAddressMode,
    SamplerDescriptor, SamplerMinMagFilter, SamplerState, ScissorRect, Texture, TextureDescriptor,
    TextureUsage,
};
use metal_rust::metal4::{
    ArgumentTable, ArgumentTableDescriptor, BlendState, CompilerDescriptor, LibraryDescriptor,
    LibraryFunctionDescriptor, RenderPassDescriptor, RenderPipelineBuildDescriptor,
    RenderPipelineColorAttachmentDescriptor, RenderPipelineDescriptor,
};
use metal_rust::quartz_core::Layer;
use std::collections::HashMap;

const SHADER_SOURCE: &str = r#"
    #include <metal_stdlib>
    using namespace metal;

    struct UiUniforms {
        float2 screen_size;
    };

    struct UiVertex {
        float2 position;
        float2 uv;
        uchar4 color;
        uint linear_texture;
    };

    struct VertexOutput {
        float4 position [[position]];
        float2 uv;
        float4 color;
        float linear_texture;
    };

    float3 linear_from_gamma(float3 srgb) {
        float3 lower = srgb / 12.92;
        float3 higher = pow((srgb + 0.055) / 1.055, float3(2.4));
        return select(higher, lower, srgb < 0.04045);
    }

    vertex VertexOutput ui_vertex(
        uint vertex_id [[vertex_id]],
        constant UiUniforms& uniforms [[buffer(0)]],
        device const UiVertex* vertices [[buffer(1)]]
    ) {
        UiVertex ui_vertex_data = vertices[vertex_id];
        VertexOutput output;
        output.position = float4(
            2.0 * ui_vertex_data.position.x / uniforms.screen_size.x - 1.0,
            1.0 - 2.0 * ui_vertex_data.position.y / uniforms.screen_size.y,
            0.0,
            1.0
        );
        output.uv = ui_vertex_data.uv;
        output.color = float4(ui_vertex_data.color) / 255.0;
        output.linear_texture = float(ui_vertex_data.linear_texture);
        return output;
    }

    fragment float4 ui_fragment(
        VertexOutput input [[stage_in]],
        texture2d<float> color_texture [[texture(0)]],
        sampler color_sampler [[sampler(0)]]
    ) {
        float4 sampled = color_texture.sample(color_sampler, input.uv);
        float4 gamma_color = input.color * sampled;
        float3 linear = input.linear_texture > 0.5
            ? linear_from_gamma(input.color.rgb) * sampled.rgb
            : linear_from_gamma(gamma_color.rgb);
        return float4(linear, gamma_color.a);
    }
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct UiUniforms {
    screen_size: [f32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct UiVertex {
    position: [f32; 2],
    uv: [f32; 2],
    color: [u8; 4],
    linear_texture: u32,
}

struct UiTexture {
    texture: Texture,
    sampler: SamplerState,
    external: bool,
}

struct Draw {
    texture_id: egui::TextureId,
    clip_rect: egui::Rect,
    first_vertex: usize,
    vertex_count: usize,
}

pub struct Painter {
    device: Device,
    pipeline: metal_rust::metal::RenderPipelineState,
    residency: ResidencySet,
    argument_table: ArgumentTable,
    textures: HashMap<egui::TextureId, UiTexture>,
    frame_slots: Vec<EguiFrameSlot>,
    frame_slot: usize,
}

struct EguiFrameSlot {
    allocator: metal_rust::metal4::CommandAllocator,
    uniform_buffer: metal_rust::metal::Buffer,
    vertex_buffer: metal_rust::metal::Buffer,
    vertex_capacity: usize,
    pending: Option<metal_rust::metal4::SubmittedCommandBuffers>,
}

impl Painter {
    fn begin_frame_slot(&mut self) -> anyhow::Result<usize> {
        let slot_index = self.frame_slot;
        self.frame_slot = (slot_index + 1) % self.frame_slots.len().max(1);
        let slot = self
            .frame_slots
            .get_mut(slot_index)
            .context("egui MTL4 frame slot is missing")?;
        if let Some(pending) = slot.pending.take() {
            pending.wait()?;
        }
        slot.allocator.reset_safe()?;
        Ok(slot_index)
    }

    pub fn new(device: &Device) -> anyhow::Result<Self> {
        let compiler = device.new_mtl4_compiler(&CompilerDescriptor::new()?)?;
        let library_descriptor = LibraryDescriptor::new()?;
        library_descriptor.set_source(SHADER_SOURCE)?;
        let library = compiler.new_library(&library_descriptor)?;

        let vertex = function_descriptor(&library, "ui_vertex")?;
        let fragment = function_descriptor(&library, "ui_fragment")?;
        let descriptor = RenderPipelineDescriptor::new()?;
        descriptor.set_vertex_function_descriptor(Some(&vertex))?;
        descriptor.set_fragment_function_descriptor(Some(&fragment))?;
        let color = RenderPipelineColorAttachmentDescriptor::new()?;
        color.set_pixel_format(PixelFormat::BGRA8_UNORM_SRGB)?;
        color.set_blending_state(BlendState::BlendStateEnabled)?;
        color.set_source_rgb_blend_factor(BlendFactor::BlendFactorOne)?;
        color.set_destination_rgb_blend_factor(BlendFactor::BlendFactorOneMinusSourceAlpha)?;
        color.set_rgb_blend_operation(BlendOperation::BlendOperationAdd)?;
        color.set_source_alpha_blend_factor(BlendFactor::BlendFactorOne)?;
        color.set_destination_alpha_blend_factor(BlendFactor::BlendFactorOneMinusSourceAlpha)?;
        color.set_alpha_blend_operation(BlendOperation::BlendOperationAdd)?;
        descriptor
            .color_attachments()?
            .context("MTL4 pipeline has no color attachment array")?
            .set_attachment(0, &color)?;
        let pipeline = compiler.new_render_pipeline(
            RenderPipelineBuildDescriptor::Render(&descriptor),
            None,
            None,
        )?;

        let mut table_descriptor = ArgumentTableDescriptor::new();
        table_descriptor.set_max_buffer_bind_count(2)?;
        table_descriptor.set_max_texture_bind_count(1)?;
        table_descriptor.set_max_sampler_state_bind_count(1)?;
        table_descriptor.set_initialize_bindings(true);
        let argument_table = device.new_mtl4_argument_table(&table_descriptor)?;

        let residency_descriptor = ResidencySetDescriptor::new()?;
        residency_descriptor.set_label("SixNG egui resources")?;
        residency_descriptor.set_initial_capacity(128)?;
        let residency = device.new_residency_set(&residency_descriptor)?;

        let mut frame_slots = Vec::with_capacity(3);
        for _ in 0..3 {
            let allocator = device.new_mtl4_command_allocator()?;
            let uniform_buffer = device.new_buffer(
                std::mem::size_of::<UiUniforms>(),
                ResourceOptions::SHARED,
            )?;
            let vertex_capacity = 4 * 1024 * 1024;
            let vertex_buffer = device.new_buffer(vertex_capacity, ResourceOptions::SHARED)?;
            residency.add_allocations(&[
                ResidencyAllocation::Buffer(&uniform_buffer),
                ResidencyAllocation::Buffer(&vertex_buffer),
            ])?;
            residency.commit()?;
            frame_slots.push(EguiFrameSlot {
                allocator,
                uniform_buffer,
                vertex_buffer,
                vertex_capacity,
                pending: None,
            });
        }
        Ok(Self {
            device: device.clone(),
            pipeline,
            residency,
            argument_table,
            textures: HashMap::new(),
            frame_slots,
            frame_slot: 0,
        })
    }

    pub fn render(
        &mut self,
        layer: &Layer,
        queue: &metal_rust::metal4::CommandQueue,
        layer_residency: &ResidencySet,
        size: [u32; 2],
        pixels_per_point: f32,
        primitives: &[epaint::ClippedPrimitive],
        texture_delta: &mut egui::TexturesDelta,
    ) -> anyhow::Result<()> {
        let result = self.render_frame(
            layer,
            queue,
            layer_residency,
            size,
            pixels_per_point,
            primitives,
            texture_delta,
        );
        if result.is_err() {
            texture_delta.clear();
        }
        result
    }

    pub fn register_external_texture(
        &mut self,
        id: egui::TextureId,
        texture: &Texture,
    ) -> anyhow::Result<()> {
        if self.textures.get(&id).is_some_and(|current| {
            current.external && current.texture.gpu_resource_id() == texture.gpu_resource_id()
        }) {
            return Ok(());
        }

        if let Some(previous) = self.textures.remove(&id) {
            if !previous.external {
                self.residency
                    .remove_allocation(ResidencyAllocation::Texture(&previous.texture))?;
            }
        }

        let sampler = self.new_sampler(egui::TextureOptions::LINEAR)?;
        self.textures.insert(
            id,
            UiTexture {
                texture: texture.clone(),
                sampler,
                external: true,
            },
        );
        Ok(())
    }

    fn render_frame(
        &mut self,
        layer: &Layer,
        queue: &metal_rust::metal4::CommandQueue,
        layer_residency: &ResidencySet,
        size: [u32; 2],
        pixels_per_point: f32,
        primitives: &[epaint::ClippedPrimitive],
        texture_delta: &mut egui::TexturesDelta,
    ) -> anyhow::Result<()> {
        self.apply_texture_updates(texture_delta)?;
        if size[0] == 0 || size[1] == 0 {
            self.free_textures(texture_delta)?;
            texture_delta.clear();
            return Ok(());
        }
        let slot_index = self.begin_frame_slot()?;
        let mut draws = flatten_primitives(primitives)?;
        let mut vertices = Vec::new();
        for (primitive, first_vertex) in primitives.iter().zip(draws.iter_mut()) {
            let epaint::Primitive::Mesh(mesh) = &primitive.primitive else {
                bail!("Metal 4 painter does not support custom egui paint callbacks");
            };
            first_vertex.first_vertex = vertices.len();
            let linear_texture = u32::from(
                first_vertex.texture_id
                    == egui::TextureId::User(super::render3d_metal4::MAP_TEXTURE_ID),
            );
            for &index in &mesh.indices {
                let vertex = mesh
                    .vertices
                    .get(index as usize)
                    .context("egui mesh contains an out-of-range vertex index")?;
                vertices.push(UiVertex {
                    position: [vertex.pos.x, vertex.pos.y],
                    uv: [vertex.uv.x, vertex.uv.y],
                    color: vertex.color.to_array(),
                    linear_texture,
                });
            }
        }

        let drawable = layer.next_drawable()?;
        let texture = drawable.texture();
        let render_pass = RenderPassDescriptor::new()?;
        render_pass.set_color_attachment(
            0,
            &texture,
            metal_rust::metal::ClearColor::new(0.055, 0.063, 0.078, 1.0),
        )?;

        let uniform = UiUniforms {
            screen_size: [
                size[0] as f32 / pixels_per_point,
                size[1] as f32 / pixels_per_point,
            ],
        };
        let uniform_buffer = {
            let slot = self
                .frame_slots
                .get(slot_index)
                .context("egui MTL4 uniform slot is missing")?;
            slot.uniform_buffer.write(0, bytemuck::bytes_of(&uniform))?;
            slot.uniform_buffer.clone()
        };
        let vertex_buffer = if vertices.is_empty() {
            None
        } else {
            let needed = bytemuck::cast_slice::<UiVertex, u8>(&vertices).len();
            let slot = self
                .frame_slots
                .get_mut(slot_index)
                .context("egui MTL4 vertex slot is missing")?;
            if needed > slot.vertex_capacity {
                bail!(
                    "egui vertex buffer needs {} bytes, ring slot capacity is {}",
                    needed,
                    slot.vertex_capacity,
                );
            }
            slot
                .vertex_buffer
                .write(0, bytemuck::cast_slice::<UiVertex, u8>(&vertices))?;
            Some(slot.vertex_buffer.clone())
        };

        self.argument_table.set_buffer(&uniform_buffer, 0, 0)?;
        if let Some(vertex_buffer) = &vertex_buffer {
            self.argument_table.set_buffer(vertex_buffer, 0, 1)?;
        }

        let allocator = self
            .frame_slots
            .get(slot_index)
            .context("egui MTL4 allocator slot is missing")?
            .allocator
            .clone();
        let mut command_buffer = self
            .device
            .new_mtl4_command_buffer()?
            .begin(&allocator, None)?;
        command_buffer.use_residency_set(&self.residency)?;
        command_buffer.use_residency_set(layer_residency)?;
        {
            let encoder = command_buffer.render_encoder(&render_pass, None)?;
            encoder.set_render_pipeline_state(&self.pipeline)?;
            if vertex_buffer.is_some() {
                encoder.set_argument_table(
                    Some(&self.argument_table),
                    RenderStages::RenderStageVertex | RenderStages::RenderStageFragment,
                )?;
                for draw in &draws {
                    if draw.vertex_count == 0 {
                        continue;
                    }
                    let Some(scissor) = clip_scissor(draw.clip_rect, pixels_per_point, size) else {
                        continue;
                    };
                    let ui_texture = self
                        .textures
                        .get(&draw.texture_id)
                        .context("egui mesh references a texture that was not uploaded")?;
                    self.argument_table.set_texture(&ui_texture.texture, 0)?;
                    self.argument_table
                        .set_sampler_state(&ui_texture.sampler, 0)?;
                    encoder.set_scissor_rect(scissor)?;
                    encoder.draw_primitives(
                        PrimitiveType::Triangle,
                        draw.first_vertex,
                        draw.vertex_count,
                    )?;
                }
            }
            encoder.end()?;
        }

        queue.wait_for_drawable(&drawable)?;
        let submission = queue.submit(vec![command_buffer.end()?], None)?;
        queue.signal_drawable(&drawable)?;
        drawable.present()?;
        self.frame_slots
            .get_mut(slot_index)
            .context("egui MTL4 submission slot is missing")?
            .pending = Some(submission);
        self.free_textures(texture_delta)?;
        texture_delta.clear();
        Ok(())
    }

    fn apply_texture_updates(&mut self, updates: &egui::TexturesDelta) -> anyhow::Result<()> {
        for (id, deltas) in &updates.set {
            for delta in deltas {
                let [width, height] = delta.image.size();
                if width == 0 || height == 0 {
                    bail!("egui supplied an empty texture update");
                }
                let epaint::ImageData::Color(image) = &delta.image;
                let bytes: Vec<u8> = image
                    .pixels
                    .iter()
                    .flat_map(|pixel| pixel.to_array())
                    .collect();
                let sampler = self.new_sampler(delta.options)?;
                if let Some([x, y]) = delta.pos {
                    let ui_texture = self
                        .textures
                        .get_mut(id)
                        .context("egui supplied a partial update for an unknown texture")?;
                    if ui_texture.external {
                        bail!("egui attempted to update a registered external texture");
                    }
                    ui_texture.texture.replace_region_2d(
                        Region::new_2d(x, y, width, height),
                        0,
                        &bytes,
                        width * 4,
                    )?;
                    ui_texture.sampler = sampler;
                } else {
                    if let Some(previous) = self.textures.remove(id) {
                        if !previous.external {
                            self.residency
                                .remove_allocation(ResidencyAllocation::Texture(
                                    &previous.texture,
                                ))?;
                        }
                    }
                    let descriptor = TextureDescriptor::new_2d(
                        PixelFormat::RGBA8_UNORM,
                        width,
                        height,
                        ResourceOptions::SHARED,
                        TextureUsage::TextureUsageShaderRead,
                    )?;
                    let texture = self.device.new_texture(&descriptor)?;
                    texture.replace_region_2d(
                        Region::new_2d(0, 0, width, height),
                        0,
                        &bytes,
                        width * 4,
                    )?;
                    self.residency
                        .add_allocation(ResidencyAllocation::Texture(&texture))?;
                    self.textures.insert(
                        *id,
                        UiTexture {
                            texture,
                            sampler,
                            external: false,
                        },
                    );
                }
            }
        }
        self.residency.commit()?;
        Ok(())
    }

    fn free_textures(&mut self, delta: &egui::TexturesDelta) -> anyhow::Result<()> {
        for id in &delta.free {
            if let Some(texture) = self.textures.remove(id) {
                if !texture.external {
                    self.residency
                        .remove_allocation(ResidencyAllocation::Texture(&texture.texture))?;
                }
            }
        }
        self.residency.commit()?;
        Ok(())
    }

    fn new_sampler(&self, options: egui::TextureOptions) -> anyhow::Result<SamplerState> {
        let descriptor = SamplerDescriptor::new()?;
        descriptor.set_min_filter(match options.minification {
            egui::TextureFilter::Nearest => SamplerMinMagFilter::SamplerMinMagFilterNearest,
            egui::TextureFilter::Linear => SamplerMinMagFilter::SamplerMinMagFilterLinear,
        })?;
        descriptor.set_mag_filter(match options.magnification {
            egui::TextureFilter::Nearest => SamplerMinMagFilter::SamplerMinMagFilterNearest,
            egui::TextureFilter::Linear => SamplerMinMagFilter::SamplerMinMagFilterLinear,
        })?;
        let address_mode = match options.wrap_mode {
            egui::TextureWrapMode::ClampToEdge => SamplerAddressMode::SamplerAddressModeClampToEdge,
            egui::TextureWrapMode::Repeat => SamplerAddressMode::SamplerAddressModeRepeat,
            egui::TextureWrapMode::MirroredRepeat => {
                SamplerAddressMode::SamplerAddressModeMirrorRepeat
            }
        };
        descriptor.set_s_address_mode(address_mode)?;
        descriptor.set_t_address_mode(address_mode)?;
        Ok(self.device.new_sampler_state(&descriptor)?)
    }
}

fn function_descriptor(
    library: &metal_rust::metal::Library,
    name: &str,
) -> anyhow::Result<metal_rust::metal4::FunctionDescriptor> {
    let descriptor = LibraryFunctionDescriptor::new()?;
    descriptor.set_library(Some(library))?;
    descriptor.set_name(name)?;
    Ok(descriptor.as_function_descriptor())
}

fn flatten_primitives(primitives: &[epaint::ClippedPrimitive]) -> anyhow::Result<Vec<Draw>> {
    let mut draws = Vec::with_capacity(primitives.len());
    for primitive in primitives {
        let epaint::Primitive::Mesh(mesh) = &primitive.primitive else {
            bail!("Metal 4 painter does not support custom egui paint callbacks");
        };
        if mesh.indices.len() % 3 != 0 {
            bail!("egui mesh has an incomplete triangle");
        }
        if mesh
            .indices
            .iter()
            .any(|&index| index as usize >= mesh.vertices.len())
        {
            bail!("egui mesh contains an out-of-range vertex index");
        }
        draws.push(Draw {
            texture_id: mesh.texture_id,
            clip_rect: primitive.clip_rect,
            first_vertex: 0,
            vertex_count: mesh.indices.len(),
        });
    }
    Ok(draws)
}

fn clip_scissor(clip: egui::Rect, pixels_per_point: f32, size: [u32; 2]) -> Option<ScissorRect> {
    if !pixels_per_point.is_finite() || pixels_per_point <= 0.0 {
        return None;
    }
    let max_x = size[0] as f32;
    let max_y = size[1] as f32;
    let x0 = (clip.min.x * pixels_per_point).floor().clamp(0.0, max_x) as usize;
    let y0 = (clip.min.y * pixels_per_point).floor().clamp(0.0, max_y) as usize;
    let x1 = (clip.max.x * pixels_per_point).ceil().clamp(0.0, max_x) as usize;
    let y1 = (clip.max.y * pixels_per_point).ceil().clamp(0.0, max_y) as usize;
    (x0 < x1 && y0 < y1).then_some(ScissorRect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use metal_rust::metal::{Device, GPUFamily};

    #[test]
    fn painter_compiles_its_shader_on_metal4_when_available() -> anyhow::Result<()> {
        let Some(device) = Device::system_default() else {
            return Ok(());
        };
        if !device.supports_family(GPUFamily::GPUFamilyMetal4)? {
            return Ok(());
        }
        let _painter = Painter::new(&device)?;
        Ok(())
    }

    #[test]
    fn clip_scissor_clamps_to_drawable_and_rounds_outward() {
        let clip = egui::Rect::from_min_max(egui::pos2(-0.2, 1.1), egui::pos2(5.1, 4.2));
        let scissor = clip_scissor(clip, 2.0, [10, 8]).unwrap();
        assert_eq!(scissor.x, 0);
        assert_eq!(scissor.y, 2);
        assert_eq!(scissor.width, 10);
        assert_eq!(scissor.height, 6);
    }

    #[test]
    fn clip_scissor_discards_empty_rectangles() {
        let clip = egui::Rect::from_min_max(egui::pos2(-5.0, 0.0), egui::pos2(-1.0, 4.0));
        assert!(clip_scissor(clip, 1.0, [10, 10]).is_none());
    }
}
