#![allow(dead_code)]

use crate::glb;
use crate::render3d::{Camera, FrameU, Instance, LineV, Look};
use anyhow::{bail, Context};
use eframe::egui;
use glam::{Mat4, Vec3};
use metal_rust::metal::{
    BlendFactor, BlendOperation, ClearColor, CompareFunction, DepthStencilDescriptor, Device,
    IndexType, Library, LoadAction, PixelFormat, PrimitiveTopologyClass, PrimitiveType,
    RenderPassDepthAttachmentDescriptor, RenderStages, ResidencyAllocation, ResidencySet,
    ResidencySetDescriptor, ResourceOptions, SamplerAddressMode, SamplerDescriptor,
    SamplerMinMagFilter, SamplerState, Stages, StoreAction, Buffer, Texture, TextureDescriptor,
    TextureUsage,
};
use metal_rust::metal4::{
    ArgumentTable, ArgumentTableDescriptor, BlendState, BufferImageLayout, CommandQueue,
    CommandAllocator, CompilerDescriptor, LibraryDescriptor, LibraryFunctionDescriptor,
    RecordingRenderEncoder,
    RenderPassDescriptor, RenderPipelineBuildDescriptor, RenderPipelineColorAttachmentDescriptor,
    RenderPipelineDescriptor, TextureOriginSelection, TextureRegionSelection, VisibilityOptions,
};
use metal_rust::metal_fx::FrameInterpolatorDescriptor;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
#[cfg(test)]
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn load_metalfx() -> anyhow::Result<()> {
    static RESULT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    let result = RESULT.get_or_init(|| {
        // Keep the framework loaded for the process lifetime: MetalFX objects
        // can outlive any individual renderer or command buffer.
        // SAFETY: the framework path is a NUL-terminated constant and the
        // flags are valid for dlopen.
        let handle = unsafe {
            libc::dlopen(
                c"/System/Library/Frameworks/MetalFX.framework/MetalFX".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            )
        };
        if handle.is_null() {
            // SAFETY: dlerror returns a NUL-terminated message for this thread.
            let error = unsafe { libc::dlerror() };
            let message = if error.is_null() {
                "unknown dynamic loader error".to_owned()
            } else {
                // SAFETY: dlerror returned a non-null message pointer.
                unsafe { std::ffi::CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned()
            };
            Err(message)
        } else {
            Ok(())
        }
    });
    result
        .as_ref()
        .map_err(|error| anyhow::anyhow!("failed to load MetalFX: {error}"))
        .copied()
}

pub const MAP_TEXTURE_ID: u64 = 0x5349_584e_4701;
const GPU_TEXTURE_CACHE_DIR: &str = "metal_io_textures_v1";
const GPU_TEXTURE_CACHE_CAP: u64 = 2 * 1024 * 1024 * 1024;
const GPU_TEXTURE_CACHE_MAGIC: &[u8; 8] = b"SXMTLIO1";
const GPU_TEXTURE_CACHE_FIXED_HEADER: usize = 36;
static TEXTURE_CACHE_WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TEXTURE_CACHE_WRITERS_ACTIVE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
static TEXTURE_CACHE_MISS_LOGS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static LAST_TEXTURE_CACHE_WRITE: std::sync::Mutex<
    Option<std::sync::mpsc::Receiver<anyhow::Result<Option<PathBuf>>>>,
> = std::sync::Mutex::new(None);

struct CacheImage {
    path: PathBuf,
    key: String,
    srgb: bool,
    mips: Vec<(usize, usize, usize, usize)>,
    payload: Vec<u8>,
}

fn cache_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{:016x}.rgba", cache_key(key)))
}

fn texture_identity(
    key: &str,
    source_version: &str,
    srgb: bool,
    mips: &[(usize, usize, usize, usize)],
    payload: &[u8],
) -> String {
    let mut layout_hash = 0xcbf29ce484222325u64;
    for &(offset, width, height, length) in mips {
        for byte in (offset as u64)
            .to_le_bytes()
            .into_iter()
            .chain((width as u64).to_le_bytes())
            .chain((height as u64).to_le_bytes())
            .chain((length as u64).to_le_bytes())
        {
            layout_hash ^= byte as u64;
            layout_hash = layout_hash.wrapping_mul(0x100000001b3);
        }
    }
    format!(
        "{key}|source={source_version}|srgb={}|layout={layout_hash:016x}|rgba={:08x}",
        srgb as u8,
        crc32(payload)
    )
}

fn texture_identity_for(
    image: &glb::Image,
    mips: &[(usize, usize, usize, usize)],
    payload: &[u8],
) -> String {
    texture_identity(
        &image.key,
        env!("CARGO_PKG_VERSION"),
        image.srgb,
        mips,
        payload,
    )
}

fn cache_key(key: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in key.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn write_texture_cache_entry(dir: &Path, image: &CacheImage) -> anyhow::Result<Option<PathBuf>> {
    let _guard = TEXTURE_CACHE_WRITER.lock().unwrap();
    let path = if image.path.is_absolute() {
        image.path.clone()
    } else {
        dir.join(&image.path)
    };
    if path.exists() {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mip_count = u32::try_from(image.mips.len()).context("too many texture mips")?;
    let mut header = Vec::with_capacity(GPU_TEXTURE_CACHE_FIXED_HEADER + image.mips.len() * 24);
    header.extend_from_slice(GPU_TEXTURE_CACHE_MAGIC);
    header.extend_from_slice(&1u32.to_le_bytes());
    header.extend_from_slice(&(image.srgb as u32).to_le_bytes());
    header.extend_from_slice(&mip_count.to_le_bytes());
    header.extend_from_slice(&(image.payload.len() as u64).to_le_bytes());
    header.extend_from_slice(&crc32(&image.payload).to_le_bytes());
    header.extend_from_slice(&crc32(image.key.as_bytes()).to_le_bytes());
    for &(offset, width, height, length) in &image.mips {
        header.extend_from_slice(&(offset as u64).to_le_bytes());
        header.extend_from_slice(&(width as u32).to_le_bytes());
        header.extend_from_slice(&(height as u32).to_le_bytes());
        header.extend_from_slice(&(length as u64).to_le_bytes());
    }

    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(&header)?;
        file.write_all(&image.payload)?;
        file.sync_all()?;
        fs::rename(&temp, &path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map(|()| Some(path))
}

fn trim_texture_cache_entries(
    mut entries: Vec<(Option<std::time::SystemTime>, PathBuf, u64)>,
    cap: u64,
    protected: Option<(&Path, u64)>,
) -> anyhow::Result<()> {
    let mut total = entries
        .iter()
        .fold(0u64, |sum, entry| sum.saturating_add(entry.2));
    entries.sort_by_key(|entry| entry.0);
    for (_, path, size) in entries {
        if total <= cap {
            break;
        }
        if protected.is_some_and(|(protected, _)| path == protected) {
            continue;
        }
        match fs::remove_file(path) {
            Ok(()) => total = total.saturating_sub(size),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                total = total.saturating_sub(size);
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn trim_texture_cache_dir(
    dir: &Path,
    cap: u64,
    protected: Option<(&Path, u64)>,
) -> anyhow::Result<()> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_file() && entry.path().extension().is_some_and(|ext| ext == "rgba") {
            entries.push((metadata.modified().ok(), entry.path(), metadata.len()));
        }
    }
    trim_texture_cache_entries(entries, cap, protected)
}

fn crc32(bytes: &[u8]) -> u32 {
    crc32_update(!0u32, bytes) ^ !0u32
}

fn crc32_update(mut crc: u32, bytes: &[u8]) -> u32 {
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    crc
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TemporalUpscaleMode {
    Off,
    Scale150,
    Scale200,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameGenerationMode {
    Off,
    Interpolate120,
}

fn display_size_aspect(size: &[u32; 2]) -> f32 {
    if size[1] == 0 {
        1.0
    } else {
        size[0] as f32 / size[1] as f32
    }
}

impl TemporalUpscaleMode {
    pub fn scale(self) -> f32 {
        match self {
            Self::Off => 1.0,
            Self::Scale150 => 1.5,
            Self::Scale200 => 2.0,
        }
    }
}

fn halton(index: u32, base: u32) -> f32 {
    let mut result = 0.0;
    let mut f = 1.0 / base as f32;
    let mut i = index;
    while i > 0 {
        result += f * (i % base) as f32;
        i /= base;
        f /= base as f32;
    }
    result
}

const MESH_SHADER: &str = r#"
    #include <metal_stdlib>
    using namespace metal;

    struct Frame {
        float4x4 view_proj;
        float4x4 inv_view_proj;
        float4 cam_pos;
        float4 sun_dir;
        float4 sun_col;
        float4 sky_top;
        float4 sky_hor;
        uint4 mode;
        float4 paint1;
        float4 paint2;
        float4 misc;
        float4 misc2;
        float4 misc3;
        float4x4 prev_view_proj;
        float4 jitter;
    };

    struct Vertex {
        float4 pos;
        float4 nrm;
        float4 tan;
        float2 uv0;
        float2 uv1;
        float4 color;
        uint4 joints;
        float4 weights;
        float4 tint;
    };

    struct Instance {
        float4x4 transform;
        float4 extra;
    };

    struct Material {
        float4 base;
        uint4 flags;
    };

    struct LineVertex {
        packed_float3 pos;
        uchar4 color;
    };

    struct LineVarying {
        float4 position [[position]];
        float4 color;
    };

    struct Varying {
        float4 position [[position]];
        float3 world;
        float3 normal;
        float4 tangent;
        float2 uv;
        float4 color;
        float4 extra;
        float4 tint;
    };

    vertex Varying mesh_vertex(
        uint vertex_id [[vertex_id]],
        uint instance_id [[instance_id]],
        constant Frame& frame [[buffer(0)]],
        device const Vertex* vertices [[buffer(1)]],
        device const Instance* instances [[buffer(2)]],
        device const float4x4* joints [[buffer(4)]]
    ) {
        const Vertex vtx = vertices[vertex_id];
        const Instance instance = instances[instance_id];
        float4x4 skin = float4x4(1.0);
        if (instance.extra.x > 0.5) {
            float4x4 weighted = float4x4(0.0);
            float weight_sum = 0.0;
            for (uint i = 0; i < 4; ++i) {
                const float weight = vtx.weights[i];
                if (weight > 0.0 && vtx.joints[i] < uint(instance.extra.w)) {
                    weighted += joints[vtx.joints[i]] * weight;
                    weight_sum += weight;
                }
            }
            if (weight_sum > 0.001) {
                skin = weighted / weight_sum;
            }
        }
        const float4x4 model = instance.transform * skin;
        const float4 world = model * float4(vtx.pos.xyz, 1.0);
        Varying output;
        const float4 clip = frame.view_proj * world;
        output.position = clip;
        output.position.xy += frame.jitter.xy * clip.w;
        output.world = world.xyz;
        output.normal = (model * float4(vtx.nrm.xyz, 0.0)).xyz;
        output.tangent = float4((model * float4(vtx.tan.xyz, 0.0)).xyz, vtx.tan.w);
        output.uv = vtx.uv0;
        output.color = vtx.color;
        output.extra = instance.extra;
        output.tint = vtx.tint;
        return output;
    }

    float4 mesh_fragment_impl(
        Varying input,
        constant Frame& frame,
        constant Material& material,
        texture2d<float> base_texture,
        texture2d<float> normal_texture,
        texture2d<float> packed_texture,
        texture2d<float> emissive_texture,
        sampler material_sampler
    ) {
        float4 base = material.base;
        if ((material.flags.x & 1u) != 0u) {
            base *= base_texture.sample(material_sampler, input.uv);
        }
        if (material.flags.z == 1u && base.a < 0.5) {
            discard_fragment();
        }

        float3 normal = normalize(input.normal);
        if ((material.flags.x & 2u) != 0u) {
            float3 tangent = normalize(input.tangent.xyz);
            float3 bitangent = normalize(cross(normal, tangent));
            float3 sampled = normal_texture.sample(material_sampler, input.uv).xyz * 2.0 - 1.0;
            sampled.z = sqrt(max(1.0 - sampled.x * sampled.x - sampled.y * sampled.y, 0.0));
            bitangent *= select(1.0, -1.0, input.tangent.w < 0.0);
            normal = normalize(tangent * sampled.x + bitangent * sampled.y + normal * sampled.z);
        }

        float3 albedo = base.rgb;
        if (frame.misc3.x > 0.5) {
            albedo *= input.tint.rgb;
        }
        const uint cls = material.flags.y;
        float alpha = base.a;
        if (cls == 10u) {
            albedo = frame.paint1.rgb;
            if ((material.flags.x & 16u) != 0u) {
                albedo = frame.misc2.yzw;
            }
        } else if (cls == 5u || cls == 6u) {
            alpha = min(alpha, 0.45);
        } else if (cls == 8u) {
            albedo = float3(0.04, 0.12, 0.16);
            alpha = 0.7;
        }
        // vehicle parts coloured per vertex: tint alpha 254 marks the primary
        // body paint, which takes the paint colour
        if ((material.flags.w & 1024u) != 0u) {
            const float part_code = input.tint.a * 255.0;
            if (part_code > 253.5 && part_code < 254.5) {
                albedo = frame.paint1.rgb;
            }
        }

        float roughness = 0.75;
        float metalness = 0.0;
        if ((material.flags.x & 4u) != 0u) {
            const float3 packed = packed_texture.sample(material_sampler, input.uv).rgb;
            if (frame.mode.y == 0u) {
                roughness = packed.r;
                metalness = packed.g;
            } else if (frame.mode.y == 1u) {
                roughness = packed.g;
                metalness = packed.b;
            } else if (frame.mode.y == 2u) {
                roughness = packed.g;
                metalness = packed.r;
            }
            roughness = clamp(roughness, 0.04, 1.0);
        }
        const float3 light_dir = normalize(frame.sun_dir.xyz);
        const float ndl = max(dot(normal, light_dir), 0.0);
        const float ambient = max(frame.misc.w, 0.0);
        float3 color = albedo * (frame.sky_hor.rgb * ambient
            + frame.sun_col.rgb * frame.sun_dir.w * ndl * 2.0);
        if (cls == 7u) {
            float3 emissive = albedo;
            if ((material.flags.x & 8u) != 0u) {
                emissive = emissive_texture.sample(material_sampler, input.uv).rgb;
            }
            color += emissive * frame.misc.y;
        }
        if (frame.mode.x == 1u) {
            color = albedo;
        } else if (frame.mode.x == 2u) {
            color = normal * 0.5 + 0.5;
        } else if (frame.mode.x == 6u) {
            color = input.color.rgb;
        }
        if (input.extra.y > 0.5) {
            color = mix(color, float3(1.0, 0.55, 0.1), 0.45);
        }
        const float distance_to_camera = length(frame.cam_pos.xyz - input.world);
        const float fog = 1.0 - exp(-distance_to_camera * frame.misc2.x);
        color = mix(color, frame.sky_hor.rgb, fog);
        if (material.flags.z != 2u && cls != 5u && cls != 6u && cls != 8u) {
            alpha = 1.0;
        }
        return float4(color, alpha);
    }

    struct MeshFragmentOutput {
        float4 color [[color(0)]];
        float2 motion [[color(1)]];
    };

    fragment float4 mesh_fragment(
        Varying input [[stage_in]],
        constant Frame& frame [[buffer(0)]],
        constant Material& material [[buffer(3)]],
        texture2d<float> base_texture [[texture(0)]],
        texture2d<float> normal_texture [[texture(1)]],
        texture2d<float> packed_texture [[texture(2)]],
        texture2d<float> emissive_texture [[texture(3)]],
        sampler material_sampler [[sampler(0)]]
    ) {
        return mesh_fragment_impl(input, frame, material, base_texture, normal_texture,
                                  packed_texture, emissive_texture, material_sampler);
    }

    fragment MeshFragmentOutput temporal_mesh_fragment(
        Varying input [[stage_in]],
        constant Frame& frame [[buffer(0)]],
        constant Material& material [[buffer(3)]],
        texture2d<float> base_texture [[texture(0)]],
        texture2d<float> normal_texture [[texture(1)]],
        texture2d<float> packed_texture [[texture(2)]],
        texture2d<float> emissive_texture [[texture(3)]],
        sampler material_sampler [[sampler(0)]]
    ) {
        MeshFragmentOutput output;
        output.color = mesh_fragment_impl(input, frame, material, base_texture, normal_texture,
                                          packed_texture, emissive_texture, material_sampler);
        float4 current_clip = frame.view_proj * float4(input.world, 1.0);
        float4 previous_clip = frame.prev_view_proj * float4(input.world, 1.0);
        float2 current_ndc = current_clip.xy / current_clip.w;
        float2 previous_ndc = previous_clip.xy / previous_clip.w;
        output.motion = previous_ndc - current_ndc;
        return output;
    }

    vertex LineVarying line_vertex(
        uint vertex_id [[vertex_id]],
        constant Frame& frame [[buffer(0)]],
        device const LineVertex* lines [[buffer(1)]]
    ) {
        const LineVertex line = lines[vertex_id];
        LineVarying output;
        output.position = frame.view_proj * float4(float3(line.pos), 1.0);
        output.color = float4(line.color) / 255.0;
        return output;
    }

    fragment float4 line_fragment(LineVarying input [[stage_in]]) {
        return input.color;
    }
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex4 {
    pos: [f32; 4],
    nrm: [f32; 4],
    tan: [f32; 4],
    uv0: [f32; 2],
    uv1: [f32; 2],
    color: [f32; 4],
    joints: [u32; 4],
    weights: [f32; 4],
    tint: [f32; 4],
}

#[cfg(test)]
mod texture_cache_tests {
    use super::*;

    fn test_dir() -> PathBuf {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("sixng-metal-io-cache-{nonce}-{id}"))
    }

    fn sample_image(key: &str, payload: Vec<u8>) -> CacheImage {
        CacheImage {
            path: PathBuf::from(format!("{:016x}.rgba", cache_key(key))),
            key: key.into(),
            srgb: true,
            mips: vec![(0, 2, 2, 16), (16, 1, 1, 4)],
            payload,
        }
    }

    #[test]
    fn gpu_cache_header_round_trips_and_rejects_corruption() {
        let dir = test_dir();
        fs::create_dir_all(&dir).unwrap();
        let image = sample_image("texture-key", (0u8..20).collect());
        let path = write_texture_cache_entry(&dir, &image).unwrap().unwrap();
        let (payload_offset, payload_len) =
            Renderer3D4::cache_matches(&path, &image.key, image.srgb, &image.mips).unwrap();
        assert_eq!(
            payload_offset as usize,
            GPU_TEXTURE_CACHE_FIXED_HEADER + 24 * 2
        );
        assert_eq!(payload_len, image.payload.len());
        assert!(write_texture_cache_entry(&dir, &image).unwrap().is_none());
        let mut bytes = fs::read(&path).unwrap();
        bytes.last_mut().map(|byte| *byte ^= 0xff);
        fs::write(&path, bytes).unwrap();
        assert!(Renderer3D4::cache_matches(&path, &image.key, image.srgb, &image.mips).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn gpu_cache_rejects_layout_and_color_space_changes() {
        let dir = test_dir();
        fs::create_dir_all(&dir).unwrap();
        let image = sample_image("texture-layout", (0u8..20).collect());
        let path = write_texture_cache_entry(&dir, &image).unwrap().unwrap();
        assert!(Renderer3D4::cache_matches(&path, &image.key, false, &image.mips).is_err());
        assert!(
            Renderer3D4::cache_matches(&path, &image.key, image.srgb, &[(0, 1, 1, 4)]).is_err()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn gpu_cache_trimming_preserves_the_current_entry() {
        let now = std::time::SystemTime::now();
        let old = PathBuf::from("/tmp/sixng-old-cache-entry");
        let protected = PathBuf::from("/tmp/sixng-new-cache-entry");
        assert!(trim_texture_cache_entries(
            vec![(Some(now), old, 100), (Some(now), protected.clone(), 100)],
            100,
            Some((&protected, 100)),
        )
        .is_ok());
    }

    #[test]
    fn gpu_cache_trim_ignores_writer_temporary_files() {
        let dir = test_dir();
        fs::create_dir_all(&dir).unwrap();
        let completed = dir.join("completed.rgba");
        let temporary = dir.join("completed.rgba.tmp-123");
        fs::write(&completed, [1u8; 16]).unwrap();
        fs::write(&temporary, [2u8; 16]).unwrap();
        trim_texture_cache_dir(&dir, 0, None).unwrap();
        assert!(!completed.exists());
        assert!(temporary.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn gpu_cache_trim_counts_protected_entry_in_total() {
        let dir = test_dir();
        fs::create_dir_all(&dir).unwrap();
        let oldest = dir.join("oldest.rgba");
        let retained = dir.join("retained.rgba");
        let protected = dir.join("protected.rgba");
        fs::write(&oldest, [1u8; 100]).unwrap();
        fs::write(&retained, [2u8; 100]).unwrap();
        fs::write(&protected, [3u8; 100]).unwrap();
        let now = std::time::SystemTime::now();
        trim_texture_cache_entries(
            vec![
                (
                    Some(now - std::time::Duration::from_secs(2)),
                    oldest.clone(),
                    100,
                ),
                (
                    Some(now - std::time::Duration::from_secs(1)),
                    retained.clone(),
                    100,
                ),
                (Some(now), protected.clone(), 100),
            ],
            200,
            Some((&protected, 100)),
        )
        .unwrap();
        assert!(!oldest.exists());
        assert!(retained.exists());
        assert!(protected.exists());
        let _ = fs::remove_dir_all(dir);
    }
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Material4 {
    base: [f32; 4],
    flags: [u32; 4],
}

pub struct GpuMesh4 {
    pub index_range: std::ops::Range<usize>,
    pub vertex_offset: usize,
    pub n_idx: u32,
    pub material: usize,
    pub skinned: bool,
    pub centre: Vec3,
    pub visible: bool,
}

pub struct GpuModel4 {
    pub meshes: Vec<GpuMesh4>,
    pub mat_alpha: Vec<u32>,
    pub mat_class: Vec<u32>,
    pub mat_decal: Vec<Option<u8>>,
    pub bbox: [Vec3; 2],
    pub n_instances: u32,
    instance_cap: u32,
    joint_count: usize,
    vertex_buffer: metal_rust::metal::Buffer,
    index_buffer: metal_rust::metal::Buffer,
    instance_buffer: metal_rust::metal::Buffer,
    material_buffer: metal_rust::metal::Buffer,
    joints_buffer: metal_rust::metal::Buffer,
    textures: Vec<Texture>,
    image_texture_indices: Vec<Option<usize>>,
    material_texture_indices: Vec<[Option<usize>; 4]>,
}

impl GpuModel4 {
    pub fn buffer_bytes(&self) -> u64 {
        self.vertex_buffer.length() as u64
            + self.index_buffer.length() as u64
            + self.instance_buffer.length() as u64
            + self.material_buffer.length() as u64
            + self.joints_buffer.length() as u64
    }
}

const MATERIAL_STRIDE: usize = 256;

struct PendingTextureUpload {
    texture: Texture,
    staging: Option<metal_rust::metal::Buffer>,
    mips: Vec<(usize, usize, usize, usize)>,
}

fn function_descriptor(
    library: &Library,
    name: &str,
) -> anyhow::Result<metal_rust::metal4::FunctionDescriptor> {
    let descriptor = LibraryFunctionDescriptor::new()?;
    descriptor.set_library(Some(library))?;
    descriptor.set_name(name)?;
    Ok(descriptor.as_function_descriptor())
}

fn create_mesh_pipeline(
    compiler: &metal_rust::metal4::Compiler,
    library: &Library,
) -> anyhow::Result<metal_rust::metal::RenderPipelineState> {
    let vertex = function_descriptor(library, "mesh_vertex")?;
    let fragment = function_descriptor(library, "mesh_fragment")?;
    let descriptor = RenderPipelineDescriptor::new()?;
    descriptor.set_vertex_function_descriptor(Some(&vertex))?;
    descriptor.set_fragment_function_descriptor(Some(&fragment))?;

    let color = RenderPipelineColorAttachmentDescriptor::new()?;
    color.set_pixel_format(PixelFormat::RGBA8_UNORM_SRGB)?;
    color.set_blending_state(BlendState::BlendStateEnabled)?;
    color.set_source_rgb_blend_factor(BlendFactor::BlendFactorOne)?;
    color.set_destination_rgb_blend_factor(BlendFactor::BlendFactorOneMinusSourceAlpha)?;
    color.set_rgb_blend_operation(BlendOperation::BlendOperationAdd)?;
    color.set_source_alpha_blend_factor(BlendFactor::BlendFactorOne)?;
    color.set_destination_alpha_blend_factor(BlendFactor::BlendFactorOneMinusSourceAlpha)?;
    color.set_alpha_blend_operation(BlendOperation::BlendOperationAdd)?;
    descriptor
        .color_attachments()?
        .context("MTL4 mesh pipeline has no color attachment array")?
        .set_attachment(0, &color)?;
    Ok(compiler.new_render_pipeline(
        RenderPipelineBuildDescriptor::Render(&descriptor),
        None,
        None,
    )?)
}

fn create_temporal_mesh_pipeline(
    compiler: &metal_rust::metal4::Compiler,
    library: &Library,
) -> anyhow::Result<metal_rust::metal::RenderPipelineState> {
    let vertex = function_descriptor(library, "mesh_vertex")?;
    let fragment = function_descriptor(library, "temporal_mesh_fragment")?;
    let descriptor = RenderPipelineDescriptor::new()?;
    descriptor.set_vertex_function_descriptor(Some(&vertex))?;
    descriptor.set_fragment_function_descriptor(Some(&fragment))?;
    let color = RenderPipelineColorAttachmentDescriptor::new()?;
    color.set_pixel_format(PixelFormat::RGBA16_FLOAT)?;
    descriptor
        .color_attachments()?
        .context("MTL4 temporal mesh pipeline has no color attachment array")?
        .set_attachment(0, &color)?;
    let motion = RenderPipelineColorAttachmentDescriptor::new()?;
    motion.set_pixel_format(PixelFormat::RG16_FLOAT)?;
    descriptor
        .color_attachments()?
        .context("MTL4 temporal mesh pipeline has no motion attachment array")?
        .set_attachment(1, &motion)?;
    Ok(compiler.new_render_pipeline(
        RenderPipelineBuildDescriptor::Render(&descriptor),
        None,
        None,
    )?)
}

fn create_line_pipeline(
    compiler: &metal_rust::metal4::Compiler,
    library: &Library,
) -> anyhow::Result<metal_rust::metal::RenderPipelineState> {
    let vertex = function_descriptor(library, "line_vertex")?;
    let fragment = function_descriptor(library, "line_fragment")?;
    let descriptor = RenderPipelineDescriptor::new()?;
    descriptor.set_vertex_function_descriptor(Some(&vertex))?;
    descriptor.set_fragment_function_descriptor(Some(&fragment))?;
    descriptor.set_input_primitive_topology(PrimitiveTopologyClass::PrimitiveTopologyClassLine)?;

    let color = RenderPipelineColorAttachmentDescriptor::new()?;
    color.set_pixel_format(PixelFormat::RGBA8_UNORM_SRGB)?;
    color.set_blending_state(BlendState::BlendStateEnabled)?;
    color.set_source_rgb_blend_factor(BlendFactor::BlendFactorOne)?;
    color.set_destination_rgb_blend_factor(BlendFactor::BlendFactorOneMinusSourceAlpha)?;
    color.set_rgb_blend_operation(BlendOperation::BlendOperationAdd)?;
    color.set_source_alpha_blend_factor(BlendFactor::BlendFactorOne)?;
    color.set_destination_alpha_blend_factor(BlendFactor::BlendFactorOneMinusSourceAlpha)?;
    color.set_alpha_blend_operation(BlendOperation::BlendOperationAdd)?;
    descriptor
        .color_attachments()?
        .context("MTL4 line pipeline has no color attachment array")?
        .set_attachment(0, &color)?;
    Ok(compiler.new_render_pipeline(
        RenderPipelineBuildDescriptor::Render(&descriptor),
        None,
        None,
    )?)
}

fn vertex4(vertex: &glb::Vertex) -> Vertex4 {
    let normalized = |v: [u8; 4]| v.map(|c| c as f32 / 255.0);
    Vertex4 {
        pos: [vertex.pos[0], vertex.pos[1], vertex.pos[2], 1.0],
        nrm: [vertex.nrm[0], vertex.nrm[1], vertex.nrm[2], 0.0],
        tan: vertex.tan,
        uv0: vertex.uv0,
        uv1: vertex.uv1,
        color: normalized(vertex.col),
        joints: vertex.joints.map(u32::from),
        weights: normalized(vertex.weights),
        tint: normalized(vertex.tint),
    }
}

fn material_image_indices(material: &glb::Material) -> [Option<usize>; 4] {
    [
        material.base,
        material.normal,
        material.packed,
        material.emissive,
    ]
}

fn frame_uniform(
    camera: &Camera,
    look: &Look,
    aspect: f32,
    prev_view_projection: Mat4,
    jitter: [f32; 2],
) -> FrameU {
    let view_projection = camera.proj(aspect) * camera.view();
    let rgba = |color: [f32; 3]| [color[0], color[1], color[2], 1.0];
    FrameU {
        view_proj: view_projection.to_cols_array_2d(),
        inv_view_proj: view_projection.inverse().to_cols_array_2d(),
        cam_pos: camera.eye().extend(1.0).to_array(),
        sun_dir: look.sun_dir.extend(look.sun_intensity).to_array(),
        sun_col: rgba(look.sun_col),
        sky_top: rgba(look.sky_top),
        sky_hor: [
            look.sky_hor[0],
            look.sky_hor[1],
            look.sky_hor[2],
            look.exposure,
        ],
        mode: [look.mode, look.rma_order, look.finish, look.slot],
        paint1: rgba(look.paint),
        paint2: [
            look.pearl[0],
            look.pearl[1],
            look.pearl[2],
            look.pearl_amount,
        ],
        misc: [look.wet, look.emissive, 0.0, look.ambient],
        misc2: [
            look.fog,
            look.paint2nd[0],
            look.paint2nd[1],
            look.paint2nd[2],
        ],
        misc3: [look.tint as u32 as f32, 0.0, 0.0, 0.0],
        prev_view_proj: prev_view_projection.to_cols_array_2d(),
        jitter: [jitter[0], jitter[1], 0.0, 0.0],
    }
}

fn bind_mesh(
    encoder: &RecordingRenderEncoder<'_>,
    table: &ArgumentTable,
    fallback_texture: &Texture,
    sampler: &SamplerState,
    model: &GpuModel4,
    mesh: &GpuMesh4,
) -> anyhow::Result<()> {
    table.set_buffer(&model.vertex_buffer, mesh.vertex_offset, 1)?;
    table.set_buffer(&model.instance_buffer, 0, 2)?;
    table.set_buffer(&model.material_buffer, mesh.material * MATERIAL_STRIDE, 3)?;
    table.set_buffer(&model.joints_buffer, 0, 4)?;
    for texture_slot in 0..4 {
        let texture = model
            .material_texture_indices
            .get(mesh.material)
            .and_then(|textures| textures[texture_slot])
            .and_then(|index| model.textures.get(index))
            .unwrap_or(fallback_texture);
        table.set_texture(texture, texture_slot)?;
    }
    table.set_sampler_state(sampler, 0)?;
    encoder.draw_indexed_primitives_instanced(
        PrimitiveType::Triangle,
        mesh.n_idx as usize,
        IndexType::IndexTypeUInt32,
        &model.index_buffer,
        mesh.index_range.clone(),
        model.n_instances as usize,
    )?;
    Ok(())
}

struct Target {
    size: [u32; 2],
    display_size: [u32; 2],
    color: Texture,
    motion: Texture,
    upscale: Option<Texture>,
    prev_upscale: Option<Texture>,
    interpolated: Option<Texture>,
    depth: Texture,
    depth_resident: bool,
}

struct SceneFrameSlot {
    allocator: CommandAllocator,
    frame_buffer: Buffer,
}

pub struct Renderer3D4 {
    device: Device,
    queue: CommandQueue,
    io_queue: Option<metal_rust::metal::IoCommandQueue>,
    residency: ResidencySet,
    texture_cache_dir: Option<PathBuf>,
    validated_texture_caches: HashMap<PathBuf, (u64, std::time::SystemTime, u64, usize)>,
    pipeline: metal_rust::metal::RenderPipelineState,
    temporal_pipeline: metal_rust::metal::RenderPipelineState,
    line_pipeline: metal_rust::metal::RenderPipelineState,
    depth_state: metal_rust::metal::DepthStencilState,
    blend_depth_state: metal_rust::metal::DepthStencilState,
    line_top_depth_state: metal_rust::metal::DepthStencilState,
    argument_table: ArgumentTable,
    sampler: SamplerState,
    fallback_texture: Texture,
    line_buffer: Option<metal_rust::metal::Buffer>,
    target: Option<Target>,
    readback: Option<metal_rust::metal::Buffer>,
    pub draws: u32,
    scene_frame_slots: Vec<SceneFrameSlot>,
    scene_frame_slot: usize,
    #[cfg(test)]
    last_texture_cache_hits: usize,
    temporal_mode: TemporalUpscaleMode,
    temporal_frame: u32,
    prev_view_projection: Mat4,
    temporal_scaler: Option<metal_rust::metal4_fx::TemporalScaler>,
    frame_generation_mode: FrameGenerationMode,
    frame_generation_frame: u32,
    previous_real_frame_time: Option<Instant>,
    frame_generation_real_slot: bool,
    frame_interpolator: Option<metal_rust::metal4_fx::FrameInterpolator>,
    compiler: metal_rust::metal4::Compiler,
}

impl Renderer3D4 {
    fn begin_scene_frame_slot(&mut self) -> anyhow::Result<usize> {
        let slot_index = self.scene_frame_slot;
        self.scene_frame_slot = (slot_index + 1) % self.scene_frame_slots.len().max(1);
        let slot = self
            .scene_frame_slots
            .get_mut(slot_index)
            .context("scene MTL4 frame slot is missing")?;
        slot.allocator.reset_safe()?;
        Ok(slot_index)
    }

    pub fn set_frame_generation_mode(&mut self, mode: FrameGenerationMode) {
        if self.frame_generation_mode != mode {
            self.frame_generation_mode = mode;
            self.frame_generation_frame = 0;
            self.previous_real_frame_time = None;
            if mode != FrameGenerationMode::Off {
                self.set_temporal_upscale_mode(TemporalUpscaleMode::Scale200);
            }
            eprintln!("[frame-generation] requested mode {mode:?}");
        }
    }

    pub fn frame_generation_mode(&self) -> FrameGenerationMode {
        self.frame_generation_mode
    }

    pub fn set_frame_generation_real_slot(&mut self, enabled: bool) {
        self.frame_generation_real_slot = enabled;
    }

    pub fn set_temporal_upscale_mode(&mut self, mode: TemporalUpscaleMode) {
        if self.temporal_mode != mode {
            self.temporal_mode = mode;
            self.temporal_frame = 0;
            self.prev_view_projection = Mat4::IDENTITY;
            if mode == TemporalUpscaleMode::Off {
                self.frame_generation_mode = FrameGenerationMode::Off;
                self.frame_generation_frame = 0;
                self.previous_real_frame_time = None;
            }
        }
    }

    pub fn temporal_upscale_mode(&self) -> TemporalUpscaleMode {
        self.temporal_mode
    }

    pub fn new(device: &Device, queue: &CommandQueue) -> anyhow::Result<Self> {
        let io_queue = (|| -> anyhow::Result<_> {
            let descriptor = metal_rust::metal::IOCommandQueueDescriptor::new()?;
            descriptor.set_type(metal_rust::metal::IOCommandQueueType::IOCommandQueueTypeSerial)?;
            descriptor.set_max_command_buffer_count(8)?;
            descriptor.set_max_commands_in_flight(8)?;
            Ok(device.new_io_command_queue(&descriptor)?)
        })()
        .map(Some)
        .unwrap_or_else(|error| {
            eprintln!("[metal-io] texture cache disabled during renderer startup: {error:#}");
            None
        });
        let compiler = device.new_mtl4_compiler(&CompilerDescriptor::new()?)?;
        let library_descriptor = LibraryDescriptor::new()?;
        library_descriptor.set_source(MESH_SHADER)?;
        let library = compiler.new_library(&library_descriptor)?;
        let pipeline = create_mesh_pipeline(&compiler, &library)?;
        let temporal_pipeline = create_temporal_mesh_pipeline(&compiler, &library)?;
        let line_pipeline = create_line_pipeline(&compiler, &library)?;

        let depth_descriptor = DepthStencilDescriptor::new()?;
        depth_descriptor
            .set_depth_compare_function(CompareFunction::CompareFunctionGreaterEqual)?;
        depth_descriptor.set_depth_write_enabled(true)?;
        let depth_state = device.new_depth_stencil_state(&depth_descriptor)?;
        depth_descriptor.set_depth_write_enabled(false)?;
        let blend_depth_state = device.new_depth_stencil_state(&depth_descriptor)?;
        depth_descriptor.set_depth_compare_function(CompareFunction::CompareFunctionAlways)?;
        let line_top_depth_state = device.new_depth_stencil_state(&depth_descriptor)?;

        let descriptor = ResidencySetDescriptor::new()?;
        descriptor.set_label("SixNG MTL4 3D renderer")?;
        descriptor.set_initial_capacity(128)?;
        let residency = device.new_residency_set(&descriptor)?;
        queue.add_residency_set(&residency)?;

        let mut table_descriptor = ArgumentTableDescriptor::new();
        table_descriptor.set_max_buffer_bind_count(5)?;
        table_descriptor.set_max_texture_bind_count(4)?;
        table_descriptor.set_max_sampler_state_bind_count(1)?;
        table_descriptor.set_initialize_bindings(true);
        let argument_table = device.new_mtl4_argument_table(&table_descriptor)?;

        let sampler_descriptor = SamplerDescriptor::new()?;
        sampler_descriptor.set_min_filter(SamplerMinMagFilter::SamplerMinMagFilterLinear)?;
        sampler_descriptor.set_mag_filter(SamplerMinMagFilter::SamplerMinMagFilterLinear)?;
        sampler_descriptor.set_s_address_mode(SamplerAddressMode::SamplerAddressModeRepeat)?;
        sampler_descriptor.set_t_address_mode(SamplerAddressMode::SamplerAddressModeRepeat)?;
        let sampler = device.new_sampler_state(&sampler_descriptor)?;

        let fallback_descriptor = TextureDescriptor::new_2d(
            PixelFormat::RGBA8_UNORM,
            1,
            1,
            ResourceOptions::SHARED,
            TextureUsage::SHADER_READ,
        )?;
        let fallback_texture = device.new_texture(&fallback_descriptor)?;
        fallback_texture.replace_region_2d(
            metal_rust::metal::Region::new_2d(0, 0, 1, 1),
            0,
            &[255, 255, 255, 255],
            4,
        )?;
        let mut scene_frame_slots = Vec::with_capacity(3);
        for _ in 0..3 {
            let allocator = device.new_mtl4_command_allocator()?;
            let frame_buffer =
                device.new_buffer(std::mem::size_of::<FrameU>(), ResourceOptions::SHARED)?;
            residency.add_allocations(&[ResidencyAllocation::Buffer(&frame_buffer)])?;
            scene_frame_slots.push(SceneFrameSlot {
                allocator,
                frame_buffer,
            });
        }
        residency.add_allocations(&[ResidencyAllocation::Texture(&fallback_texture)])?;
        residency.commit()?;

        Ok(Self {
            device: device.clone(),
            queue: queue.clone(),
            io_queue,
            residency,
            texture_cache_dir: None,
            validated_texture_caches: HashMap::new(),
            pipeline,
            temporal_pipeline,
            line_pipeline,
            depth_state,
            blend_depth_state,
            line_top_depth_state,
            argument_table,
            sampler,
            fallback_texture,
            line_buffer: None,
            target: None,
            readback: None,
            draws: 0,
            scene_frame_slots,
            scene_frame_slot: 0,
            #[cfg(test)]
            last_texture_cache_hits: 0,
            temporal_mode: TemporalUpscaleMode::Off,
            temporal_frame: 0,
            prev_view_projection: Mat4::IDENTITY,
            temporal_scaler: None,
            frame_generation_mode: FrameGenerationMode::Off,
            frame_generation_frame: 0,
            previous_real_frame_time: None,
            frame_generation_real_slot: true,
            frame_interpolator: None,
            compiler,
        })
    }

    pub fn set_texture_cache_dir(&mut self, cache_dir: impl AsRef<Path>) -> anyhow::Result<()> {
        if self.io_queue.is_none() {
            bail!("Metal I/O command queue is unavailable");
        }
        let dir = cache_dir.as_ref().join(GPU_TEXTURE_CACHE_DIR);
        fs::create_dir_all(&dir)?;
        trim_texture_cache_dir(&dir, GPU_TEXTURE_CACHE_CAP, None)?;
        self.texture_cache_dir = Some(dir);
        Ok(())
    }

    fn cache_matches(
        path: &Path,
        key: &str,
        srgb: bool,
        mips: &[(usize, usize, usize, usize)],
    ) -> anyhow::Result<(u64, usize)> {
        let mut file = File::open(path)?;
        let mut fixed = [0u8; GPU_TEXTURE_CACHE_FIXED_HEADER];
        file.read_exact(&mut fixed)?;
        anyhow::ensure!(
            &fixed[..8] == GPU_TEXTURE_CACHE_MAGIC,
            "cache magic/version mismatch"
        );
        anyhow::ensure!(
            u32::from_le_bytes(fixed[8..12].try_into()?) == 1,
            "cache format version mismatch"
        );
        anyhow::ensure!(
            u32::from_le_bytes(fixed[12..16].try_into()?) == srgb as u32,
            "cache color space mismatch"
        );
        anyhow::ensure!(
            u32::from_le_bytes(fixed[16..20].try_into()?) as usize == mips.len(),
            "cache mip count mismatch"
        );
        let payload_len = u64::from_le_bytes(fixed[20..28].try_into()?);
        anyhow::ensure!(
            u32::from_le_bytes(fixed[32..36].try_into()?) == crc32(key.as_bytes()),
            "cache image key mismatch"
        );
        let expected_crc = u32::from_le_bytes(fixed[28..32].try_into()?);
        let metadata_len = GPU_TEXTURE_CACHE_FIXED_HEADER as u64 + (mips.len() as u64) * 24;
        let expected_file_len = metadata_len
            .checked_add(payload_len)
            .context("cache size overflow")?;
        anyhow::ensure!(
            file.metadata()?.len() == expected_file_len,
            "cache file length mismatch"
        );

        for &(offset, width, height, length) in mips {
            let mut entry = [0u8; 24];
            file.read_exact(&mut entry)?;
            anyhow::ensure!(
                u64::from_le_bytes(entry[0..8].try_into()?) == offset as u64
                    && u32::from_le_bytes(entry[8..12].try_into()?) as usize == width
                    && u32::from_le_bytes(entry[12..16].try_into()?) as usize == height
                    && u64::from_le_bytes(entry[16..24].try_into()?) == length as u64,
                "cache mip layout mismatch"
            );
        }

        let mut remaining = payload_len;
        let mut chunk = [0u8; 1024 * 1024];
        let mut checksum = !0u32;
        while remaining > 0 {
            let count = remaining.min(chunk.len() as u64) as usize;
            file.read_exact(&mut chunk[..count])?;
            checksum = crc32_update(checksum, &chunk[..count]);
            remaining -= count as u64;
        }
        anyhow::ensure!(
            checksum ^ !0u32 == expected_crc,
            "cache payload checksum mismatch"
        );
        Ok((metadata_len, usize::try_from(payload_len)?))
    }

    fn queue_texture_cache_write(
        &self,
        image: &glb::Image,
        payload: &[u8],
        mips: &[(usize, usize, usize, usize)],
    ) {
        let Some(dir) = self.texture_cache_dir.clone() else {
            return;
        };
        if TEXTURE_CACHE_WRITERS_ACTIVE
            .try_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |active| (active < 2).then_some(active + 1),
            )
            .is_err()
        {
            return;
        }
        if payload.len() as u64 + GPU_TEXTURE_CACHE_FIXED_HEADER as u64 + (mips.len() as u64) * 24
            > GPU_TEXTURE_CACHE_CAP
        {
            TEXTURE_CACHE_WRITERS_ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::Release);
            return;
        }
        let identity = texture_identity_for(image, mips, payload);
        let image = CacheImage {
            path: cache_path(Path::new(""), &identity),
            key: identity,
            srgb: image.srgb,
            mips: mips.to_vec(),
            payload: payload.to_vec(),
        };
        let entry_size =
            GPU_TEXTURE_CACHE_FIXED_HEADER as u64 + (mips.len() as u64) * 24 + payload.len() as u64;
        #[cfg(test)]
        let (test_tx, test_rx) = std::sync::mpsc::channel();
        #[cfg(test)]
        {
            *LAST_TEXTURE_CACHE_WRITE.lock().unwrap() = Some(test_rx);
        }
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| -> Result<Option<PathBuf>, String> {
                match write_texture_cache_entry(&dir, &image) {
                    Ok(Some(written_path)) => {
                        trim_texture_cache_dir(
                            &dir,
                            GPU_TEXTURE_CACHE_CAP,
                            Some((&written_path, entry_size)),
                        )
                        .map_err(|error| format!("{error:#}"))?;
                        Ok(Some(written_path))
                    }
                    Ok(None) => Ok(None),
                    Err(error) => Err(format!("{error:#}")),
                }
            });
            if result.is_err() {
                eprintln!("[metal-io] texture cache writer panicked");
            } else if let Ok(Err(error)) = &result {
                eprintln!("[metal-io] texture cache write failed: {error}");
            }
            #[cfg(test)]
            {
                let write_result = match &result {
                    Ok(Ok(Some(written_path))) if written_path.is_file() => {
                        Ok(Some(written_path.clone()))
                    }
                    Ok(Ok(None)) => Ok(None),
                    Ok(Ok(Some(written_path))) => Err(anyhow::anyhow!(
                        "writer returned missing cache file {}",
                        written_path.display()
                    )),
                    Ok(Err(error)) => Err(anyhow::anyhow!(error.clone())),
                    Err(_) => Err(anyhow::anyhow!("texture cache writer panicked")),
                };
                let _ = test_tx.send(write_result);
            }
            TEXTURE_CACHE_WRITERS_ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::Release);
        });
    }

    fn load_texture_from_cache(
        &self,
        path: &Path,
        identity: &str,
        texture: &Texture,
        srgb: bool,
        mips: &[(usize, usize, usize, usize)],
    ) -> anyhow::Result<()> {
        let io_queue = self
            .io_queue
            .as_ref()
            .context("Metal I/O command queue is unavailable")?;
        let (payload_offset, payload_len) = Self::cache_matches(path, identity, srgb, mips)?;
        #[cfg(not(test))]
        let _ = payload_len;
        let handle = self.device.new_io_file_handle(path, None)?;
        #[cfg(test)]
        let io_buffer = {
            let io_buffer = self
                .device
                .new_buffer(payload_len, ResourceOptions::SHARED)?;
            let mut buffer_command = io_queue.command_buffer()?;
            buffer_command.load_buffer(
                &io_buffer,
                0,
                payload_len,
                &handle,
                payload_offset as usize,
            )?;
            let started = std::time::Instant::now();
            buffer_command.commit().wait()?;
            eprintln!(
                "[metal-io test] cache file-to-buffer load+wait {:.3} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
            io_buffer
        };
        let mut command = io_queue.command_buffer()?;
        command.set_label(Some("SixNG GPU texture cache load"));
        for (level, &(offset, width, height, _)) in mips.iter().enumerate() {
            command.load_texture(
                texture,
                0,
                level,
                metal_rust::metal::Size::new(width, height, 1),
                width * 4,
                width * height * 4,
                metal_rust::metal::Origin::new(0, 0, 0),
                &handle,
                usize::try_from(payload_offset + offset as u64)?,
            )?;
        }
        #[cfg(test)]
        let started = std::time::Instant::now();
        command.commit().wait()?;
        #[cfg(test)]
        eprintln!(
            "[metal-io test] texture command load+wait {:.3} ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        #[cfg(test)]
        {
            let mut expected = Vec::with_capacity(payload_len);
            let mut file = File::open(path)?;
            file.seek(SeekFrom::Start(payload_offset))?;
            file.take(payload_len as u64).read_to_end(&mut expected)?;
            anyhow::ensure!(io_buffer.read(0, payload_len)? == expected);
        }
        Ok(())
    }

    fn validate_texture_cache_entry(
        &mut self,
        path: &Path,
        identity: &str,
        srgb: bool,
        mips: &[(usize, usize, usize, usize)],
    ) -> anyhow::Result<(u64, usize)> {
        let metadata = fs::metadata(path)?;
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        if let Some(&(cached_len, cached_modified, payload_offset, payload_len)) =
            self.validated_texture_caches.get(path)
        {
            if cached_len == metadata.len() && cached_modified == modified {
                return Ok((payload_offset, payload_len));
            }
            self.validated_texture_caches.remove(path);
        }
        let (payload_offset, payload_len) = Self::cache_matches(path, identity, srgb, mips)?;
        self.validated_texture_caches.insert(
            path.to_path_buf(),
            (metadata.len(), modified, payload_offset, payload_len),
        );
        Ok((payload_offset, payload_len))
    }

    fn sync_texture_cache_residency(
        &self,
        texture_cache: &HashMap<String, Texture>,
        model: &glb::Model,
    ) -> anyhow::Result<()> {
        let mut allocations = Vec::new();
        let mut seen = HashMap::new();
        for image in &model.images {
            let Some(texture) = texture_cache.get(&image.key) else {
                continue;
            };
            if seen.insert(image.key.as_str(), ()).is_none()
                && !self
                    .residency
                    .contains_allocation(ResidencyAllocation::Texture(texture))?
            {
                allocations.push(ResidencyAllocation::Texture(texture));
            }
        }
        if !allocations.is_empty() {
            self.residency.add_allocations(&allocations)?;
            self.residency.commit()?;
        }
        Ok(())
    }

    pub fn upload_shared(
        &mut self,
        model: &glb::Model,
        texture_cache: &mut HashMap<String, Texture>,
    ) -> anyhow::Result<GpuModel4> {
        #[cfg(test)]
        {
            self.last_texture_cache_hits = 0;
        }
        let mut textures = Vec::new();
        let mut image_texture_indices = vec![None; model.images.len()];
        let mut new_cache_entries = Vec::new();
        let mut pending_textures = Vec::new();
        for (image_index, image) in model.images.iter().enumerate() {
            let existing = texture_cache.get(&image.key).cloned().or_else(|| {
                new_cache_entries
                    .iter()
                    .find(|(key, _): &&(String, Texture)| key == &image.key)
                    .map(|(_, texture)| texture.clone())
            });
            let texture = if let Some(texture) = existing {
                Some(texture)
            } else if image.mips.is_empty() {
                if image.bc.is_some() {
                    bail!(
                        "MTL4 texture {} was block-compressed without an RGBA fallback",
                        image.key
                    );
                }
                None
            } else {
                let pending = self.new_texture_upload(image)?;
                let texture = pending.texture.clone();
                new_cache_entries.push((image.key.clone(), texture.clone()));
                pending_textures.push(pending);
                Some(texture)
            };
            if let Some(texture) = texture {
                let local_index = textures.len();
                textures.push(texture);
                image_texture_indices[image_index] = Some(local_index);
            }
        }
        self.sync_texture_cache_residency(texture_cache, model)?;

        let default_material = glb::Material {
            name: "default".into(),
            class: "opaque".into(),
            alpha_mode: "OPAQUE".into(),
            base_factor: [0.7, 0.7, 0.7, 1.0],
            base: None,
            normal: None,
            packed: None,
            emissive: None,
            mr: None,
            tint_mask: false,
            ped_shade: false,
            detail: None,
            parts: false,
            occlusion_only: false,
            slots: Vec::new(),
            gap: "ok".into(),
            shader_hash: String::new(),
            decal: None,
        };
        let materials: Vec<&glb::Material> = if model.materials.is_empty() {
            vec![&default_material]
        } else {
            model.materials.iter().collect()
        };
        let mut material_data = vec![0u8; MATERIAL_STRIDE * materials.len()];
        let mut mat_alpha = Vec::with_capacity(materials.len());
        let mut mat_class = Vec::with_capacity(materials.len());
        let mut mat_decal = Vec::with_capacity(materials.len());
        let mut material_texture_indices = Vec::with_capacity(materials.len());
        for (material_index, material) in materials.iter().enumerate() {
            let image_indices = material_image_indices(material);
            let local_textures = image_indices.map(|index| {
                index.and_then(|index| image_texture_indices.get(index).copied().flatten())
            });
            let mut has = 0u32;
            for (slot, texture) in local_textures.iter().enumerate() {
                if texture.is_some() {
                    has |= 1 << slot;
                }
            }
            let alpha = match material.alpha_mode.as_str() {
                "MASK" => 1,
                "BLEND" => 2,
                _ => 0,
            };
            let gap = match material.gap.as_str() {
                "flat" => 2,
                "lowmip" => 1,
                _ => 0,
            };
            let class = crate::render3d::class_id(&material.class);
            let value = Material4 {
                base: material.base_factor,
                // w: gap in bits 0-1; bit 10 = vehicle parts coloured per
                // vertex (the tint alpha 254 marks the primary body paint)
                flags: [
                    has,
                    class,
                    alpha,
                    gap | if material.parts { 0x400 } else { 0 },
                ],
            };
            let offset = material_index * MATERIAL_STRIDE;
            material_data[offset..offset + std::mem::size_of::<Material4>()]
                .copy_from_slice(bytemuck::bytes_of(&value));
            mat_alpha.push(alpha);
            mat_class.push(class);
            mat_decal.push(material.decal);
            material_texture_indices.push(local_textures);
        }

        let mut vertices = Vec::new();
        let mut indices = Vec::new();
        let mut meshes = Vec::with_capacity(model.meshes.len());
        for mesh in &model.meshes {
            if mesh.indices.len() % 3 != 0 {
                bail!("MTL4 mesh {} has an incomplete triangle", mesh.name);
            }
            let vertex_offset = vertices.len() * std::mem::size_of::<Vertex4>();
            let index_start = indices.len() * std::mem::size_of::<u32>();
            vertices.extend(mesh.verts.iter().map(vertex4));
            indices.extend_from_slice(&mesh.indices);
            let index_end = indices.len() * std::mem::size_of::<u32>();
            meshes.push(GpuMesh4 {
                index_range: index_start..index_end,
                vertex_offset,
                n_idx: u32::try_from(mesh.indices.len())
                    .context("MTL4 mesh index count exceeds u32")?,
                material: mesh.material.unwrap_or(0).min(materials.len() - 1),
                skinned: mesh.skinned,
                centre: Vec3::from(mesh.centre),
                visible: true,
            });
        }

        let vertex_bytes = bytemuck::cast_slice::<Vertex4, u8>(&vertices);
        let index_bytes = bytemuck::cast_slice::<u32, u8>(&indices);
        let vertex_buffer = self
            .device
            .new_buffer(vertex_bytes.len().max(4), ResourceOptions::PRIVATE)?;
        let index_buffer = self
            .device
            .new_buffer(index_bytes.len().max(4), ResourceOptions::PRIVATE)?;
        let vertex_staging = if vertex_bytes.is_empty() {
            None
        } else {
            Some(
                self.device
                    .new_buffer_with_bytes(vertex_bytes, ResourceOptions::SHARED)?,
            )
        };
        let index_staging = if index_bytes.is_empty() {
            None
        } else {
            Some(
                self.device
                    .new_buffer_with_bytes(index_bytes, ResourceOptions::SHARED)?,
            )
        };

        let joint_count = model.joints.len();
        let joints = vec![Mat4::IDENTITY.to_cols_array_2d(); joint_count.max(1)];
        let joints_buffer = self
            .device
            .new_buffer_with_bytes(bytemuck::cast_slice(&joints), ResourceOptions::SHARED)?;
        let mut initial_instance = Instance::new(Mat4::IDENTITY, joint_count > 0);
        initial_instance.extra[3] = joint_count as f32;
        let instance_buffer = self.device.new_buffer_with_bytes(
            bytemuck::bytes_of(&initial_instance),
            ResourceOptions::SHARED,
        )?;
        let material_buffer = self
            .device
            .new_buffer_with_bytes(&material_data, ResourceOptions::SHARED)?;

        let mut allocations = vec![
            ResidencyAllocation::Buffer(&vertex_buffer),
            ResidencyAllocation::Buffer(&index_buffer),
            ResidencyAllocation::Buffer(&instance_buffer),
            ResidencyAllocation::Buffer(&material_buffer),
            ResidencyAllocation::Buffer(&joints_buffer),
        ];
        if let Some(buffer) = &vertex_staging {
            allocations.push(ResidencyAllocation::Buffer(buffer));
        }
        if let Some(buffer) = &index_staging {
            allocations.push(ResidencyAllocation::Buffer(buffer));
        }
        for pending in &pending_textures {
            if let Some(staging) = &pending.staging {
                allocations.push(ResidencyAllocation::Buffer(staging));
            }
            allocations.push(ResidencyAllocation::Texture(&pending.texture));
        }
        self.residency.add_allocations(&allocations)?;
        self.residency.commit()?;

        if vertex_staging.is_some() || index_staging.is_some() || !pending_textures.is_empty() {
            let allocator = self.device.new_mtl4_command_allocator()?;
            let mut command_buffer = self
                .device
                .new_mtl4_command_buffer()?
                .begin(&allocator, None)?;
            command_buffer.use_residency_set(&self.residency)?;
            {
                let encoder = command_buffer.compute_encoder()?;
                if let Some(staging) = &vertex_staging {
                    encoder.copy_buffer(staging, 0..vertex_bytes.len(), &vertex_buffer, 0)?;
                }
                if let Some(staging) = &index_staging {
                    encoder.copy_buffer(staging, 0..index_bytes.len(), &index_buffer, 0)?;
                }
                for pending in &pending_textures {
                    if let Some(staging) = &pending.staging {
                        for &(offset, width, height, byte_length) in &pending.mips {
                            encoder.copy_buffer_to_texture(
                                staging,
                                BufferImageLayout {
                                    offset,
                                    bytes_per_row: width * 4,
                                    bytes_per_image: byte_length,
                                },
                                metal_rust::metal::Size::new(width, height, 1),
                                &pending.texture,
                                TextureOriginSelection {
                                    slice: 0,
                                    level: pending
                                        .mips
                                        .iter()
                                        .position(|entry| entry.0 == offset)
                                        .context("MTL4 mip upload offset was lost")?,
                                    origin: metal_rust::metal::Origin::new(0, 0, 0),
                                },
                                None,
                            )?;
                        }
                    }
                }
                encoder.end()?;
            }
            self.queue
                .submit(vec![command_buffer.end()?], None)?
                .wait()?;
        }

        let mut staging_allocations = Vec::new();
        if let Some(buffer) = &vertex_staging {
            staging_allocations.push(ResidencyAllocation::Buffer(buffer));
        }
        if let Some(buffer) = &index_staging {
            staging_allocations.push(ResidencyAllocation::Buffer(buffer));
        }
        for pending in &pending_textures {
            if let Some(staging) = &pending.staging {
                staging_allocations.push(ResidencyAllocation::Buffer(staging));
            }
        }
        for (key, texture) in new_cache_entries {
            texture_cache.insert(key, texture);
        }
        if !staging_allocations.is_empty() {
            self.residency.remove_allocations(&staging_allocations)?;
            self.residency.commit()?;
        }

        Ok(GpuModel4 {
            meshes,
            mat_alpha,
            mat_class,
            mat_decal,
            bbox: [Vec3::from(model.bbox[0]), Vec3::from(model.bbox[1])],
            n_instances: 1,
            instance_cap: 1,
            joint_count,
            vertex_buffer,
            index_buffer,
            instance_buffer,
            material_buffer,
            joints_buffer,
            textures,
            image_texture_indices,
            material_texture_indices,
        })
    }

    fn new_texture_upload(&mut self, image: &glb::Image) -> anyhow::Result<PendingTextureUpload> {
        let (base_width, base_height, _) = image
            .mips
            .first()
            .context("MTL4 texture has no mip levels")?;
        if *base_width == 0 || *base_height == 0 {
            bail!("MTL4 texture dimensions must be non-zero");
        }
        let format = if image.srgb {
            PixelFormat::RGBA8_UNORM_SRGB
        } else {
            PixelFormat::RGBA8_UNORM
        };
        let descriptor = TextureDescriptor::new_2d(
            format,
            *base_width as usize,
            *base_height as usize,
            ResourceOptions::PRIVATE,
            TextureUsage::SHADER_READ,
        )?;
        descriptor.set_mipmap_level_count(image.mips.len())?;
        let texture = self.device.new_texture(&descriptor)?;
        let mut bytes = Vec::new();
        let mut mips = Vec::with_capacity(image.mips.len());
        for (width, height, pixels) in &image.mips {
            let expected = (*width as usize)
                .checked_mul(*height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .context("MTL4 texture mip size overflow")?;
            if pixels.len() != expected {
                bail!(
                    "MTL4 texture mip {}x{} has {} bytes; expected {}",
                    width,
                    height,
                    pixels.len(),
                    expected
                );
            }
            let offset = bytes.len();
            bytes.extend_from_slice(pixels);
            mips.push((offset, *width as usize, *height as usize, expected));
        }
        if let Some(dir) = self.texture_cache_dir.clone() {
            let identity = texture_identity_for(image, &mips, &bytes);
            let path = cache_path(&dir, &identity);
            match self.validate_texture_cache_entry(&path, &identity, image.srgb, &mips) {
                Ok(_) => {
                    match self
                        .load_texture_from_cache(&path, &identity, &texture, image.srgb, &mips)
                    {
                        Ok(()) => {
                            let _ = OpenOptions::new()
                                .write(true)
                                .open(&path)
                                .and_then(|file| file.set_modified(std::time::SystemTime::now()));
                            #[cfg(test)]
                            {
                                self.last_texture_cache_hits += 1;
                            }
                            return Ok(PendingTextureUpload {
                                texture,
                                staging: None,
                                mips,
                            });
                        }
                        Err(error) => {
                            eprintln!(
                                "[metal-io] cache load failed for {}: {error:#}; using staging upload",
                                image.key
                            );
                            self.validated_texture_caches.remove(&path);
                            let _ = fs::remove_file(&path);
                        }
                    }
                }
                Err(_) => {
                    let logged =
                        TEXTURE_CACHE_MISS_LOGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if logged < 8 {
                        eprintln!(
                            "[metal-io] GPU texture cache miss or invalid entry for {}; using staging upload",
                            image.key
                        );
                    }
                    self.validated_texture_caches.remove(&path);
                    let _ = fs::remove_file(&path);
                }
            }
        }
        self.queue_texture_cache_write(image, &bytes, &mips);
        let staging = self
            .device
            .new_buffer_with_bytes(&bytes, ResourceOptions::SHARED)?;
        Ok(PendingTextureUpload {
            texture,
            staging: Some(staging),
            mips,
        })
    }

    pub fn set_instances(
        &mut self,
        model: &mut GpuModel4,
        instances: &[Instance],
    ) -> anyhow::Result<()> {
        if instances.len() > model.instance_cap as usize {
            let capacity = instances.len().next_power_of_two().max(1);
            let buffer = self.device.new_buffer(
                capacity
                    .checked_mul(std::mem::size_of::<Instance>())
                    .context("MTL4 instance buffer size overflow")?,
                ResourceOptions::SHARED,
            )?;
            self.residency
                .add_allocations(&[ResidencyAllocation::Buffer(&buffer)])?;
            self.residency.commit()?;
            self.residency
                .remove_allocations(&[ResidencyAllocation::Buffer(&model.instance_buffer)])?;
            self.residency.commit()?;
            model.instance_buffer = buffer;
            model.instance_cap = capacity as u32;
        }
        let mut values = instances.to_vec();
        for instance in &mut values {
            instance.extra[3] = model.joint_count as f32;
        }
        if !values.is_empty() {
            model
                .instance_buffer
                .write(0, bytemuck::cast_slice(&values))?;
        }
        model.n_instances =
            u32::try_from(values.len()).context("MTL4 instance count exceeds u32")?;
        Ok(())
    }

    pub fn release_model(&mut self, model: &GpuModel4) -> anyhow::Result<()> {
        self.residency.remove_allocations(&[
            ResidencyAllocation::Buffer(&model.vertex_buffer),
            ResidencyAllocation::Buffer(&model.index_buffer),
            ResidencyAllocation::Buffer(&model.instance_buffer),
            ResidencyAllocation::Buffer(&model.material_buffer),
            ResidencyAllocation::Buffer(&model.joints_buffer),
        ])?;
        self.residency.commit()?;
        Ok(())
    }

    pub fn release_texture(&mut self, texture: &Texture) -> anyhow::Result<()> {
        self.residency
            .remove_allocations(&[ResidencyAllocation::Texture(texture)])?;
        self.residency.commit()?;
        Ok(())
    }

    pub fn output_texture(&self) -> Option<Texture> {
        self.target.as_ref().and_then(|target| {
            target
                .upscale
                .clone()
                .or_else(|| Some(target.color.clone()))
        })
    }

    pub fn real_output_texture(&self) -> Option<Texture> {
        self.target.as_ref().and_then(|target| {
            target
                .upscale
                .clone()
                .or_else(|| Some(target.color.clone()))
        })
    }

    pub fn interpolated_output_texture(&self) -> Option<Texture> {
        if self.frame_generation_frame < 2 {
            return None;
        }
        self.target
            .as_ref()
            .and_then(|target| target.interpolated.clone())
    }

    pub fn render(
        &mut self,
        size: [u32; 2],
        camera: &Camera,
        look: &Look,
        models: &[&GpuModel4],
        lines: &[LineV],
        lines_on_top: &[LineV],
    ) -> anyhow::Result<egui::TextureId> {
        if self.frame_generation_mode != FrameGenerationMode::Off
            && !self.frame_generation_real_slot
        {
            return Ok(egui::TextureId::User(MAP_TEXTURE_ID));
        }
        let scene_slot_index = self.begin_scene_frame_slot()?;
        let scale = self.temporal_mode.scale();
        let render_size = if self.temporal_mode == TemporalUpscaleMode::Off {
            size
        } else {
            [
                ((size[0] as f32 / scale).ceil() as u32).max(1),
                ((size[1] as f32 / scale).ceil() as u32).max(1),
            ]
        };
        self.ensure_target(render_size)?;
        let aspect = render_size[0] as f32 / render_size[1] as f32;
        let view_projection = camera.proj(aspect) * camera.view();
        let prev = if self.temporal_frame == 0 {
            view_projection
        } else {
            self.prev_view_projection
        };
        let phase_count = (8.0 * scale * scale).ceil() as u32;
        let index = self.temporal_frame % phase_count.max(1) + 1;
        let jitter = [halton(index, 2) - 0.5, halton(index, 3) - 0.5];
        let clip_jitter = [
            2.0 * jitter[0] / render_size[0] as f32,
            -2.0 * jitter[1] / render_size[1] as f32,
        ];
        let frame = frame_uniform(camera, look, aspect, prev, clip_jitter);
        let frame_buffer = self
            .scene_frame_slots
            .get(scene_slot_index)
            .context("scene MTL4 frame buffer slot is missing")?
            .frame_buffer
            .clone();
        frame_buffer.write(0, bytemuck::bytes_of(&frame))?;
        let mut line_vertices = Vec::with_capacity(lines.len() + lines_on_top.len());
        line_vertices.extend_from_slice(lines);
        let under_count = line_vertices.len();
        line_vertices.extend_from_slice(lines_on_top);
        let top_count = line_vertices.len() - under_count;
        if !line_vertices.is_empty() {
            self.ensure_line_buffer(bytemuck::cast_slice(&line_vertices))?;
        }
        let target = self
            .target
            .as_ref()
            .context("MTL4 target was not created")?;

        let pass = RenderPassDescriptor::new()?;
        pass.set_color_attachment(
            0,
            &target.color,
            ClearColor::new(
                look.clear[0] as f64,
                look.clear[1] as f64,
                look.clear[2] as f64,
                1.0,
            ),
        )?;
        if self.temporal_mode != TemporalUpscaleMode::Off {
            pass.set_color_attachment(1, &target.motion, ClearColor::new(0.0, 0.0, 0.0, 1.0))?;
        }
        let depth_attachment = RenderPassDepthAttachmentDescriptor::new()?;
        depth_attachment.set_texture(Some(&target.depth))?;
        depth_attachment.set_load_action(LoadAction::LoadActionClear)?;
        depth_attachment.set_store_action(if self.temporal_mode == TemporalUpscaleMode::Off {
            StoreAction::StoreActionDontCare
        } else {
            StoreAction::StoreActionStore
        })?;
        depth_attachment.set_clear_depth(0.0)?;
        pass.set_depth_attachment(Some(&depth_attachment))?;

        let allocator = self
            .scene_frame_slots
            .get(scene_slot_index)
            .context("scene MTL4 allocator slot is missing")?
            .allocator
            .clone();
        let mut command_buffer = self
            .device
            .new_mtl4_command_buffer()?
            .begin(&allocator, None)?;
        command_buffer.use_residency_set(&self.residency)?;
        let mut draw_count = 0u32;
        {
            let encoder = command_buffer.render_encoder(&pass, None)?;
            encoder.set_render_pipeline_state(
                if self.temporal_mode == TemporalUpscaleMode::Off {
                    &self.pipeline
                } else {
                    &self.temporal_pipeline
                },
            )?;
            encoder.set_depth_stencil_state(Some(&self.depth_state))?;
            encoder.set_argument_table(
                Some(&self.argument_table),
                RenderStages::RenderStageVertex | RenderStages::RenderStageFragment,
            )?;
            self.argument_table.set_buffer(&frame_buffer, 0, 0)?;

            for model in models {
                if model.n_instances == 0 {
                    continue;
                }
                for mesh in model
                    .meshes
                    .iter()
                    .filter(|mesh| mesh.visible && mesh.n_idx > 0)
                {
                    let alpha = model.mat_alpha[mesh.material] == 2
                        || matches!(model.mat_class[mesh.material], 5 | 6 | 8);
                    if alpha && look.mode == 0 {
                        continue;
                    }
                    bind_mesh(
                        &encoder,
                        &self.argument_table,
                        &self.fallback_texture,
                        &self.sampler,
                        model,
                        mesh,
                    )?;
                    draw_count += 1;
                }
            }

            if look.mode == 0 {
                let eye = camera.eye();
                let mut blended = Vec::new();
                for model in models {
                    if model.n_instances == 0 {
                        continue;
                    }
                    for mesh in model
                        .meshes
                        .iter()
                        .filter(|mesh| mesh.visible && mesh.n_idx > 0)
                    {
                        if model.mat_alpha[mesh.material] == 2
                            || matches!(model.mat_class[mesh.material], 5 | 6 | 8)
                        {
                            blended.push(((mesh.centre - eye).length(), *model, mesh));
                        }
                    }
                }
                blended.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
                encoder.set_depth_stencil_state(Some(&self.blend_depth_state))?;
                for (_, model, mesh) in blended {
                    bind_mesh(
                        &encoder,
                        &self.argument_table,
                        &self.fallback_texture,
                        &self.sampler,
                        model,
                        mesh,
                    )?;
                    draw_count += 1;
                }
            }
            if under_count > 0 {
                let buffer = self
                    .line_buffer
                    .as_ref()
                    .context("MTL4 line buffer was not created")?;
                encoder.set_render_pipeline_state(&self.line_pipeline)?;
                encoder.set_depth_stencil_state(Some(&self.blend_depth_state))?;
                self.argument_table.set_buffer(buffer, 0, 1)?;
                encoder.draw_primitives(PrimitiveType::Line, 0, under_count)?;
                draw_count += 1;
            }
            if top_count > 0 {
                let buffer = self
                    .line_buffer
                    .as_ref()
                    .context("MTL4 line buffer was not created")?;
                encoder.set_render_pipeline_state(&self.line_pipeline)?;
                encoder.set_depth_stencil_state(Some(&self.line_top_depth_state))?;
                self.argument_table.set_buffer(
                    buffer,
                    under_count * std::mem::size_of::<LineV>(),
                    1,
                )?;
                encoder.draw_primitives(PrimitiveType::Line, 0, top_count)?;
                draw_count += 1;
            }
            encoder.end()?;
        }
        if let (Some(scaler), Some(upscale)) =
            (self.temporal_scaler.as_ref(), target.upscale.as_ref())
        {
            scaler.set_color_texture(Some(&target.color))?;
            scaler.set_depth_texture(Some(&target.depth))?;
            scaler.set_motion_texture(Some(&target.motion))?;
            scaler.set_output_texture(Some(upscale))?;
            scaler.set_depth_reversed(true)?;
            scaler.set_reset(self.temporal_frame == 0)?;
            scaler.set_jitter_offset_x(jitter[0])?;
            scaler.set_jitter_offset_y(jitter[1])?;
            scaler.set_motion_vector_scale_x(render_size[0] as f32 * 0.5)?;
            scaler.set_motion_vector_scale_y(render_size[1] as f32 * 0.5)?;
            scaler.set_input_content_width(render_size[0] as usize)?;
            scaler.set_input_content_height(render_size[1] as usize)?;
            scaler.encode_to_command_buffer(&mut command_buffer)?;
        }
        if let (Some(interpolator), Some(upscale), Some(prev_upscale), Some(interpolated)) = (
            self.frame_interpolator.as_ref(),
            target.upscale.as_ref(),
            target.prev_upscale.as_ref(),
            target.interpolated.as_ref(),
        ) {
            let now = Instant::now();
            if let Some(previous) = self.previous_real_frame_time {
                interpolator.set_aspect_ratio(display_size_aspect(&target.display_size))?;
                interpolator.set_field_of_view(camera.fov.to_degrees())?;
                interpolator.set_near_plane(camera.near)?;
                interpolator.set_far_plane(camera.far)?;
                interpolator.set_delta_time(now.duration_since(previous).as_secs_f32())?;
                interpolator.set_jitter_offset_x(jitter[0])?;
                interpolator.set_jitter_offset_y(jitter[1])?;
                interpolator.set_motion_vector_scale_x(render_size[0] as f32 * 0.5)?;
                interpolator.set_motion_vector_scale_y(render_size[1] as f32 * 0.5)?;
                interpolator.set_color_texture(Some(upscale))?;
                interpolator.set_prev_color_texture(Some(prev_upscale))?;
                interpolator.set_depth_texture(Some(&target.depth))?;
                interpolator.set_motion_texture(Some(&target.motion))?;
                interpolator.set_output_texture(Some(interpolated))?;
                interpolator.set_ui_texture(None)?;
                interpolator.set_depth_reversed(true)?;
                interpolator.set_is_ui_texture_composited(false)?;
                interpolator.set_should_reset_history(self.frame_generation_frame < 2)?;
                interpolator.encode_to_command_buffer(&mut command_buffer)?;
                if self.frame_generation_frame < 3
                    || self.frame_generation_frame.is_multiple_of(120)
                {
                    eprintln!(
                        "[frame-generation] encoded real frame {} (delta {:.3} ms)",
                        self.frame_generation_frame,
                        now.duration_since(previous).as_secs_f64() * 1000.0
                    );
                }
            }

            let copy_encoder = command_buffer.compute_encoder()?;
            copy_encoder.barrier_after_queue_stages(
                Stages::StageDispatch,
                Stages::StageBlit,
                VisibilityOptions::VisibilityOptionDevice,
            )?;
            copy_encoder.copy_texture(upscale, prev_upscale)?;
            copy_encoder.end()?;
            self.previous_real_frame_time = Some(now);
            self.frame_generation_frame = self.frame_generation_frame.wrapping_add(1);
        }
        self.queue
            .submit(vec![command_buffer.end()?], None)?
            .wait()?;
        self.draws = draw_count;
        self.prev_view_projection = view_projection;
        self.temporal_frame = self.temporal_frame.wrapping_add(1);
        Ok(egui::TextureId::User(MAP_TEXTURE_ID))
    }

    fn ensure_line_buffer(&mut self, data: &[u8]) -> anyhow::Result<()> {
        if self
            .line_buffer
            .as_ref()
            .is_some_and(|buffer| buffer.length() >= data.len())
        {
            self.line_buffer
                .as_ref()
                .context("MTL4 line buffer disappeared")?
                .write(0, data)?;
            return Ok(());
        }
        let buffer = self
            .device
            .new_buffer(data.len(), ResourceOptions::SHARED)?;
        buffer.write(0, data)?;
        self.residency
            .add_allocations(&[ResidencyAllocation::Buffer(&buffer)])?;
        self.residency.commit()?;
        if let Some(old) = self.line_buffer.replace(buffer) {
            self.residency
                .remove_allocations(&[ResidencyAllocation::Buffer(&old)])?;
            self.residency.commit()?;
        }
        Ok(())
    }

    pub fn read_back(&mut self) -> anyhow::Result<Option<(u32, u32, Vec<u8>)>> {
        let Some(target) = self.target.as_ref() else {
            return Ok(None);
        };
        let size = target.size;
        let byte_length = (size[0] as usize)
            .checked_mul(size[1] as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .context("MTL4 readback buffer size overflow")?;
        self.ensure_readback(byte_length)?;
        let target = self
            .target
            .as_ref()
            .context("MTL4 target was not created")?;
        let buffer = self
            .readback
            .as_ref()
            .context("MTL4 readback buffer was not created")?;
        let region = TextureRegionSelection {
            slice: 0,
            level: 0,
            region: metal_rust::metal::Region::new_2d(0, 0, size[0] as usize, size[1] as usize),
        };
        let allocator = self.device.new_mtl4_command_allocator()?;
        let mut command_buffer = self
            .device
            .new_mtl4_command_buffer()?
            .begin(&allocator, None)?;
        command_buffer.use_residency_set(&self.residency)?;
        {
            let encoder = command_buffer.compute_encoder()?;
            encoder.copy_texture_to_buffer(
                &target.color,
                region,
                buffer,
                BufferImageLayout {
                    offset: 0,
                    bytes_per_row: size[0] as usize * 4,
                    bytes_per_image: byte_length,
                },
                None,
            )?;
            encoder.end()?;
        }
        self.queue
            .submit(vec![command_buffer.end()?], None)?
            .wait()?;
        Ok(Some((size[0], size[1], buffer.read(0, byte_length)?)))
    }

    fn ensure_target(&mut self, size: [u32; 2]) -> anyhow::Result<()> {
        if size.contains(&0) {
            bail!("MTL4 render target dimensions must be non-zero");
        }
        if self.temporal_mode != TemporalUpscaleMode::Off {
            if let Err(error) = load_metalfx() {
                self.set_temporal_upscale_mode(TemporalUpscaleMode::Off);
                return Err(error);
            }
        }
        let display_size = if self.temporal_mode == TemporalUpscaleMode::Off {
            size
        } else {
            [
                (size[0] as f32 * self.temporal_mode.scale()).ceil() as u32,
                (size[1] as f32 * self.temporal_mode.scale()).ceil() as u32,
            ]
        };
        if self.target.as_ref().is_some_and(|target| {
            target.size == size
                && target.display_size == display_size
                && target.prev_upscale.is_some()
                    == (self.frame_generation_mode != FrameGenerationMode::Off)
                && target.interpolated.is_some()
                    == (self.frame_generation_mode != FrameGenerationMode::Off)
        }) {
            return Ok(());
        }

        self.temporal_frame = 0;
        self.frame_generation_frame = 0;
        self.previous_real_frame_time = None;
        self.prev_view_projection = Mat4::IDENTITY;

        let descriptor = TextureDescriptor::new_2d(
            if self.temporal_mode == TemporalUpscaleMode::Off {
                PixelFormat::RGBA8_UNORM_SRGB
            } else {
                PixelFormat::RGBA16_FLOAT
            },
            size[0] as usize,
            size[1] as usize,
            ResourceOptions::PRIVATE,
            TextureUsage::RENDER_TARGET.union(TextureUsage::SHADER_READ),
        )?;
        let color = self.device.new_texture(&descriptor)?;
        let motion_descriptor = TextureDescriptor::new_2d(
            PixelFormat::RG16_FLOAT,
            size[0] as usize,
            size[1] as usize,
            ResourceOptions::PRIVATE,
            TextureUsage::RENDER_TARGET.union(TextureUsage::SHADER_READ),
        )?;
        let motion = self.device.new_texture(&motion_descriptor)?;
        let upscale = if self.temporal_mode == TemporalUpscaleMode::Off {
            None
        } else {
            let output_descriptor = TextureDescriptor::new_2d(
                PixelFormat::RGBA16_FLOAT,
                display_size[0] as usize,
                display_size[1] as usize,
                ResourceOptions::PRIVATE,
                TextureUsage::RENDER_TARGET
                    .union(TextureUsage::SHADER_READ)
                    .union(TextureUsage::SHADER_WRITE),
            )?;
            Some(self.device.new_texture(&output_descriptor)?)
        };
        let prev_upscale = if self.frame_generation_mode != FrameGenerationMode::Off {
            let output_descriptor = TextureDescriptor::new_2d(
                PixelFormat::RGBA16_FLOAT,
                display_size[0] as usize,
                display_size[1] as usize,
                ResourceOptions::PRIVATE,
                TextureUsage::RENDER_TARGET
                    .union(TextureUsage::SHADER_READ)
                    .union(TextureUsage::SHADER_WRITE),
            )?;
            Some(self.device.new_texture(&output_descriptor)?)
        } else {
            None
        };
        let interpolated = if self.frame_generation_mode != FrameGenerationMode::Off {
            let output_descriptor = TextureDescriptor::new_2d(
                PixelFormat::RGBA16_FLOAT,
                display_size[0] as usize,
                display_size[1] as usize,
                ResourceOptions::PRIVATE,
                TextureUsage::RENDER_TARGET
                    .union(TextureUsage::SHADER_READ)
                    .union(TextureUsage::SHADER_WRITE),
            )?;
            Some(self.device.new_texture(&output_descriptor)?)
        } else {
            None
        };
        let depth_descriptor = TextureDescriptor::new_2d(
            PixelFormat::DEPTH32_FLOAT,
            size[0] as usize,
            size[1] as usize,
            if self.temporal_mode == TemporalUpscaleMode::Off {
                ResourceOptions::MEMORYLESS
            } else {
                ResourceOptions::PRIVATE
            },
            if self.temporal_mode == TemporalUpscaleMode::Off {
                TextureUsage::RENDER_TARGET
            } else {
                TextureUsage::RENDER_TARGET.union(TextureUsage::SHADER_READ)
            },
        )?;
        let depth = self.device.new_texture(&depth_descriptor)?;
        let mut allocations = vec![
            ResidencyAllocation::Texture(&color),
            ResidencyAllocation::Texture(&motion),
        ];
        if self.temporal_mode != TemporalUpscaleMode::Off {
            allocations.push(ResidencyAllocation::Texture(&depth));
        }
        if let Some(upscale) = &upscale {
            allocations.push(ResidencyAllocation::Texture(upscale));
        }
        if let Some(prev_upscale) = &prev_upscale {
            allocations.push(ResidencyAllocation::Texture(prev_upscale));
        }
        if let Some(interpolated) = &interpolated {
            allocations.push(ResidencyAllocation::Texture(interpolated));
        }
        self.residency.add_allocations(&allocations)?;
        self.residency.commit()?;

        if let Some(old) = self.target.take() {
            let mut removals = vec![
                ResidencyAllocation::Texture(&old.color),
                ResidencyAllocation::Texture(&old.motion),
            ];
            if old.depth_resident {
                removals.push(ResidencyAllocation::Texture(&old.depth));
            }
            if let Some(upscale) = &old.upscale {
                removals.push(ResidencyAllocation::Texture(upscale));
            }
            if let Some(prev_upscale) = &old.prev_upscale {
                removals.push(ResidencyAllocation::Texture(prev_upscale));
            }
            if let Some(interpolated) = &old.interpolated {
                removals.push(ResidencyAllocation::Texture(interpolated));
            }
            self.residency.remove_allocations(&removals)?;
            self.residency.commit()?;
        }
        let depth_resident = self.temporal_mode != TemporalUpscaleMode::Off;
        self.target = Some(Target {
            size,
            display_size,
            color,
            motion,
            upscale: upscale.clone(),
            prev_upscale: prev_upscale.clone(),
            interpolated: interpolated.clone(),
            depth,
            depth_resident,
        });
        if self.temporal_mode != TemporalUpscaleMode::Off {
            let descriptor = match metal_rust::metal_fx::TemporalScalerDescriptor::new() {
                Ok(descriptor) => descriptor,
                Err(error) => {
                    self.set_temporal_upscale_mode(TemporalUpscaleMode::Off);
                    self.temporal_scaler = None;
                    self.frame_interpolator = None;
                    return Err(error.into());
                }
            };
            descriptor.configure(
                PixelFormat::RGBA16_FLOAT,
                PixelFormat::DEPTH32_FLOAT,
                PixelFormat::RG16_FLOAT,
                PixelFormat::RGBA16_FLOAT,
                size[0] as usize,
                size[1] as usize,
                display_size[0] as usize,
                display_size[1] as usize,
            )?;
            match descriptor.new_mtl4_scaler(&self.device, &self.compiler) {
                Ok(scaler) => self.temporal_scaler = Some(scaler),
                Err(error) => {
                    self.set_temporal_upscale_mode(TemporalUpscaleMode::Off);
                    self.temporal_scaler = None;
                    self.frame_interpolator = None;
                    return Err(error.into());
                }
            }
        } else {
            self.temporal_scaler = None;
        }
        if self.frame_generation_mode != FrameGenerationMode::Off {
            if let (Some(_upscale), Some(_interpolated), Some(_prev_upscale)) =
                (&upscale, &interpolated, &prev_upscale)
            {
                let descriptor = match FrameInterpolatorDescriptor::new() {
                    Ok(descriptor) => descriptor,
                    Err(error) => {
                        eprintln!("MetalFX frame interpolator descriptor unavailable: {error}");
                        self.frame_generation_mode = FrameGenerationMode::Off;
                        self.frame_interpolator = None;
                        return Ok(());
                    }
                };
                match FrameInterpolatorDescriptor::supports_metal4_fx(&self.device) {
                    Ok(true) => {}
                    Ok(false) => {
                        eprintln!("MetalFX frame interpolation is not supported on this device");
                        self.frame_generation_mode = FrameGenerationMode::Off;
                        self.frame_interpolator = None;
                        return Ok(());
                    }
                    Err(error) => {
                        eprintln!("MetalFX frame interpolation support check failed: {error}");
                        self.frame_generation_mode = FrameGenerationMode::Off;
                        self.frame_interpolator = None;
                        return Ok(());
                    }
                }
                descriptor.set_input_width(size[0] as usize)?;
                descriptor.set_input_height(size[1] as usize)?;
                descriptor.set_output_width(display_size[0] as usize)?;
                descriptor.set_output_height(display_size[1] as usize)?;
                descriptor.set_color_texture_format(PixelFormat::RGBA16_FLOAT)?;
                descriptor.set_depth_texture_format(PixelFormat::DEPTH32_FLOAT)?;
                descriptor.set_motion_texture_format(PixelFormat::RG16_FLOAT)?;
                descriptor.set_output_texture_format(PixelFormat::RGBA16_FLOAT)?;
                match descriptor.new_frame_interpolator(&self.device, &self.compiler) {
                    Ok(interpolator) => {
                        eprintln!("[frame-generation] MTL4 interpolator ready");
                        self.frame_interpolator = Some(interpolator);
                    }
                    Err(error) => {
                        eprintln!("MetalFX frame interpolator creation failed: {error}");
                        self.frame_generation_mode = FrameGenerationMode::Off;
                        self.frame_interpolator = None;
                    }
                }
            }
        } else {
            self.frame_interpolator = None;
        }
        Ok(())
    }

    fn ensure_readback(&mut self, byte_length: usize) -> anyhow::Result<()> {
        if self
            .readback
            .as_ref()
            .is_some_and(|buffer| buffer.length() >= byte_length)
        {
            return Ok(());
        }
        let buffer = self
            .device
            .new_buffer(byte_length, ResourceOptions::SHARED)?;
        self.residency
            .add_allocations(&[ResidencyAllocation::Buffer(&buffer)])?;
        self.residency.commit()?;
        if let Some(old) = self.readback.replace(buffer) {
            self.residency
                .remove_allocations(&[ResidencyAllocation::Buffer(&old)])?;
            self.residency.commit()?;
        }
        Ok(())
    }

    pub fn clear_and_readback(
        &mut self,
        size: [u32; 2],
        clear: [f64; 4],
    ) -> anyhow::Result<Vec<u8>> {
        self.ensure_target(size)?;
        let byte_length = (size[0] as usize)
            .checked_mul(size[1] as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .context("MTL4 readback buffer size overflow")?;
        self.ensure_readback(byte_length)?;
        let target = self
            .target
            .as_ref()
            .context("MTL4 target was not created")?;

        let pass = RenderPassDescriptor::new()?;
        pass.set_color_attachment(
            0,
            &target.color,
            ClearColor::new(clear[0], clear[1], clear[2], clear[3]),
        )?;
        let depth_attachment = RenderPassDepthAttachmentDescriptor::new()?;
        depth_attachment.set_texture(Some(&target.depth))?;
        depth_attachment.set_load_action(LoadAction::LoadActionClear)?;
        depth_attachment.set_store_action(StoreAction::StoreActionDontCare)?;
        depth_attachment.set_clear_depth(0.0)?;
        pass.set_depth_attachment(Some(&depth_attachment))?;
        let render_allocator = self.device.new_mtl4_command_allocator()?;
        let mut command_buffer = self
            .device
            .new_mtl4_command_buffer()?
            .begin(&render_allocator, None)?;
        command_buffer.use_residency_set(&self.residency)?;
        {
            let encoder = command_buffer.render_encoder(&pass, None)?;
            encoder.set_depth_stencil_state(Some(&self.depth_state))?;
            encoder.end()?;
        }
        self.queue
            .submit(vec![command_buffer.end()?], None)?
            .wait()?;

        let region = TextureRegionSelection {
            slice: 0,
            level: 0,
            region: metal_rust::metal::Region::new_2d(0, 0, size[0] as usize, size[1] as usize),
        };
        let buffer = self
            .readback
            .as_ref()
            .context("MTL4 readback buffer was not created")?;
        let readback_allocator = self.device.new_mtl4_command_allocator()?;
        let mut command_buffer = self
            .device
            .new_mtl4_command_buffer()?
            .begin(&readback_allocator, None)?;
        command_buffer.use_residency_set(&self.residency)?;
        {
            let encoder = command_buffer.compute_encoder()?;
            encoder.copy_texture_to_buffer(
                &target.color,
                region,
                buffer,
                BufferImageLayout {
                    offset: 0,
                    bytes_per_row: size[0] as usize * 4,
                    bytes_per_image: byte_length,
                },
                None,
            )?;
            encoder.end()?;
        }
        self.queue
            .submit(vec![command_buffer.end()?], None)?
            .wait()?;
        Ok(buffer.read(0, byte_length)?)
    }

    #[cfg(test)]
    fn read_texture_for_test(
        &mut self,
        texture: &Texture,
        width: usize,
        height: usize,
    ) -> anyhow::Result<Vec<u8>> {
        let byte_length = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .context("texture test readback size overflow")?;
        let readback = self
            .device
            .new_buffer(byte_length, ResourceOptions::SHARED)?;
        let texture_was_resident = self
            .residency
            .contains_allocation(ResidencyAllocation::Texture(texture))?;
        let mut allocations = vec![ResidencyAllocation::Buffer(&readback)];
        if !texture_was_resident {
            allocations.push(ResidencyAllocation::Texture(texture));
        }
        self.residency.add_allocations(&allocations)?;
        self.residency.commit()?;

        let allocator = self.device.new_mtl4_command_allocator()?;
        let mut command = self
            .device
            .new_mtl4_command_buffer()?
            .begin(&allocator, None)?;
        command.use_residency_set(&self.residency)?;
        {
            let encoder = command.compute_encoder()?;
            encoder.copy_texture_to_buffer(
                texture,
                TextureRegionSelection {
                    slice: 0,
                    level: 0,
                    region: metal_rust::metal::Region::new_2d(0, 0, width, height),
                },
                &readback,
                BufferImageLayout {
                    offset: 0,
                    bytes_per_row: width * 4,
                    bytes_per_image: byte_length,
                },
                None,
            )?;
            encoder.end()?;
        }
        self.queue.submit(vec![command.end()?], None)?.wait()?;
        let bytes = readback.read(0, byte_length)?;
        self.residency.remove_allocations(&allocations)?;
        self.residency.commit()?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn triangle_model() -> glb::Model {
        let vertex = |pos| glb::Vertex {
            pos,
            nrm: [0.0, 0.0, 1.0],
            col: [255; 4],
            tint: [255; 4],
            ..glb::Vertex::default()
        };
        glb::Model {
            path: PathBuf::new(),
            meshes: vec![glb::Mesh {
                name: "test triangle".into(),
                verts: vec![
                    vertex([-1.0, -1.0, 0.0]),
                    vertex([1.0, -1.0, 0.0]),
                    vertex([0.0, 1.0, 0.0]),
                ],
                indices: vec![0, 1, 2],
                material: Some(0),
                skinned: false,
                has_tangents: false,
                uv_sets: 1,
                centre: [0.0; 3],
            }],
            materials: vec![glb::Material {
                name: "red".into(),
                class: "opaque".into(),
                alpha_mode: "OPAQUE".into(),
                base_factor: [1.0, 1.0, 1.0, 1.0],
                base: Some(0),
                normal: None,
                packed: None,
                emissive: None,
                mr: None,
                tint_mask: false,
                ped_shade: false,
                detail: None,
                parts: false,
                occlusion_only: false,
                slots: Vec::new(),
                gap: "ok".into(),
                shader_hash: String::new(),
                decal: None,
            }],
            joints: Vec::new(),
            images: vec![glb::Image {
                key: "mtl4-test-green".into(),
                mips: vec![(1, 1, vec![0, 255, 0, 255])],
                srgb: true,
                bc: None,
            }],
            bbox: [[-1.0, -1.0, 0.0], [1.0, 1.0, 0.0]],
            triangles: 1,
            vertices: 3,
            extras: serde_json::Value::Null,
        }
    }
    use metal_rust::metal::GPUFamily;

    #[test]
    fn mtl4_texture_cache_hit_round_trips_when_available() -> anyhow::Result<()> {
        let Some(device) = Device::system_default() else {
            return Ok(());
        };
        if !device.supports_family(GPUFamily::GPUFamilyMetal4)? {
            return Ok(());
        }
        let queue = device.new_mtl4_command_queue()?;
        let mut renderer = Renderer3D4::new(&device, &queue)?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let cache_root = std::env::temp_dir().join(format!("sixng-metal-io-hit-{nonce}"));
        renderer.set_texture_cache_dir(&cache_root)?;
        let model = triangle_model();
        let expected = model.images[0].mips[0].2.clone();
        let mut texture_cache = HashMap::new();

        let mut first = renderer.upload_shared(&model, &mut texture_cache)?;
        let writer = LAST_TEXTURE_CACHE_WRITE
            .lock()
            .unwrap()
            .take()
            .context("cache writer completion receiver was not registered")?;
        let _ = writer
            .recv_timeout(std::time::Duration::from_secs(10))?
            .context("cache writer failed")?
            .context("staging upload did not create a cache entry")?;
        renderer.release_model(&first)?;
        for texture in &first.textures {
            renderer.release_texture(texture)?;
        }
        first.textures.clear();
        texture_cache.clear();

        let mut second = renderer.upload_shared(&model, &mut texture_cache)?;
        assert_eq!(
            renderer.last_texture_cache_hits, 1,
            "second upload did not use the Metal I/O texture cache"
        );
        let pixels = renderer.read_texture_for_test(
            second
                .textures
                .first()
                .context("cached texture missing from uploaded model")?,
            1,
            1,
        )?;
        assert_eq!(pixels, expected);
        renderer.release_model(&second)?;
        for texture in &second.textures {
            renderer.release_texture(texture)?;
        }
        second.textures.clear();
        texture_cache.clear();

        let mut third = renderer.upload_shared(&model, &mut texture_cache)?;
        assert_eq!(
            renderer.last_texture_cache_hits, 1,
            "memoized cache validation did not preserve the payload offset"
        );
        let pixels = renderer.read_texture_for_test(
            third
                .textures
                .first()
                .context("cached texture missing from repeated upload")?,
            1,
            1,
        )?;
        assert_eq!(pixels, expected);
        renderer.release_model(&third)?;
        for texture in &third.textures {
            renderer.release_texture(texture)?;
        }
        third.textures.clear();
        std::fs::remove_dir_all(cache_root)?;
        Ok(())
    }

    #[test]
    fn mtl4_clear_and_readback_round_trip_when_available() -> anyhow::Result<()> {
        let Some(device) = Device::system_default() else {
            return Ok(());
        };
        if !device.supports_family(GPUFamily::GPUFamilyMetal4)? {
            return Ok(());
        }
        let queue = device.new_mtl4_command_queue()?;
        let mut renderer = Renderer3D4::new(&device, &queue)?;
        let size = [4, 3];
        let pixels = renderer.clear_and_readback(size, [1.0, 1.0, 1.0, 1.0])?;
        assert_eq!(pixels.len(), size[0] as usize * size[1] as usize * 4);
        assert!(pixels
            .chunks_exact(4)
            .all(|pixel| pixel == [255, 255, 255, 255]));
        Ok(())
    }

    #[test]
    fn mtl4_temporal_target_and_scaler_when_available() -> anyhow::Result<()> {
        let Some(device) = Device::system_default() else {
            return Ok(());
        };
        if !device.supports_family(GPUFamily::GPUFamilyMetal4)? {
            return Ok(());
        }
        let queue = device.new_mtl4_command_queue()?;
        let mut renderer = Renderer3D4::new(&device, &queue)?;
        renderer.set_temporal_upscale_mode(TemporalUpscaleMode::Scale200);
        renderer.ensure_target([16, 12])?;
        let target = renderer
            .target
            .as_ref()
            .context("MTL4 temporal target missing")?;
        if renderer.temporal_scaler.is_some() {
            assert_eq!(target.display_size, [32, 24]);
            assert!(target.upscale.is_some());
        } else {
            assert_eq!(renderer.temporal_upscale_mode(), TemporalUpscaleMode::Off);
            assert_eq!(target.display_size, target.size);
        }
        Ok(())
    }

    #[test]
    fn mtl4_frame_generation_recreates_targets_and_primes_history() -> anyhow::Result<()> {
        let Some(device) = Device::system_default() else {
            return Ok(());
        };
        if !device.supports_family(GPUFamily::GPUFamilyMetal4)? {
            return Ok(());
        }
        load_metalfx()?;
        if !FrameInterpolatorDescriptor::supports_metal4_fx(&device)? {
            eprintln!("Skipping: this device does not support Metal 4 frame interpolation");
            return Ok(());
        }
        let queue = device.new_mtl4_command_queue()?;
        let mut renderer = Renderer3D4::new(&device, &queue)?;
        renderer.set_temporal_upscale_mode(TemporalUpscaleMode::Scale200);
        renderer.ensure_target([32, 32])?;
        if renderer.temporal_scaler.is_none() {
            eprintln!("Skipping: MetalFX temporal scaler initialization failed");
            return Ok(());
        }

        renderer.set_frame_generation_mode(FrameGenerationMode::Interpolate120);
        renderer.ensure_target([32, 32])?;
        assert_eq!(
            renderer.frame_generation_mode(),
            FrameGenerationMode::Interpolate120
        );
        assert!(renderer.target.as_ref().unwrap().prev_upscale.is_some());
        assert!(renderer.target.as_ref().unwrap().interpolated.is_some());

        let camera = Camera::default();
        let look = Look::default();
        renderer.render([64, 64], &camera, &look, &[], &[], &[])?;
        assert!(renderer.interpolated_output_texture().is_none());
        renderer.render([64, 64], &camera, &look, &[], &[], &[])?;
        assert!(renderer.interpolated_output_texture().is_some());

        renderer.ensure_target([40, 40])?;
        assert!(renderer.interpolated_output_texture().is_none());
        renderer.set_frame_generation_mode(FrameGenerationMode::Off);
        renderer.ensure_target([40, 40])?;
        let target = renderer.target.as_ref().unwrap();
        assert!(target.prev_upscale.is_none());
        assert!(target.interpolated.is_none());

        renderer.set_frame_generation_mode(FrameGenerationMode::Interpolate120);
        renderer.ensure_target([40, 40])?;
        renderer.set_temporal_upscale_mode(TemporalUpscaleMode::Off);
        assert_eq!(renderer.frame_generation_mode(), FrameGenerationMode::Off);
        renderer.ensure_target([40, 40])?;
        assert!(renderer.target.as_ref().unwrap().prev_upscale.is_none());
        eprintln!("Frame interpolation target and history checks executed");
        Ok(())
    }

    #[test]
    fn mtl4_mesh_upload_and_draw_changes_pixels_when_available() -> anyhow::Result<()> {
        let Some(device) = Device::system_default() else {
            return Ok(());
        };
        if !device.supports_family(GPUFamily::GPUFamilyMetal4)? {
            return Ok(());
        }
        let queue = device.new_mtl4_command_queue()?;
        let mut renderer = Renderer3D4::new(&device, &queue)?;
        let model = triangle_model();
        let mut model = renderer.upload_shared(&model, &mut HashMap::new())?;
        renderer.set_instances(&mut model, &[Instance::new(Mat4::IDENTITY, false)])?;
        let mut camera = Camera::default();
        camera.fly = true;
        camera.eye = Vec3::new(0.0, 0.0, 3.0);
        camera.yaw = 0.0;
        camera.pitch = 0.0;
        let mut look = Look::default();
        look.mode = 1;
        let line = [
            LineV {
                pos: [-0.9, 0.8, 0.0],
                col: [255, 0, 0, 255],
            },
            LineV {
                pos: [0.9, 0.8, 0.0],
                col: [255, 0, 0, 255],
            },
        ];
        renderer.render([32, 32], &camera, &look, &[&model], &[], &line)?;
        let (_, _, pixels) = renderer
            .read_back()?
            .context("MTL4 renderer did not expose a rendered target")?;
        let center = &pixels[(16 * 32 + 16) * 4..][..4];
        assert!(
            center[1] > center[0] + 30,
            "expected sampled green triangle pixel, got {center:?}"
        );
        assert!(
            center[1] > 100,
            "expected rendered geometry at center, got {center:?}"
        );
        assert!(
            pixels
                .chunks_exact(4)
                .any(|pixel| pixel[0] > 180 && pixel[1] < 80 && pixel[2] < 80),
            "expected the MTL4 top-line overlay in the rendered image"
        );
        Ok(())
    }
}
