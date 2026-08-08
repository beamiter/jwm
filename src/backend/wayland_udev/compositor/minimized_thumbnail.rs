//! Durable low-resolution minimized-window snapshots for the udev compositor.
//!
//! Full-resolution `GlesTexture` owners remain the only source for reverse
//! Genie. This module owns the independent bounded CPU tier and a smaller raw
//! GLES residency cache used by static Dock cards and as a hover fallback.
//! Wayland's current minimize path keeps its hidden surface mapped, so every
//! CPU entry remains recapturable; this layer does not authorize true unmap.

use super::*;
use crate::backend::compositor_common::capture::flip_rgba_vertical;
use crate::backend::compositor_common::minimized_thumbnail::{
    AdmissionOutcome, MinimizedSnapshot, MinimizedSnapshotCache, SnapshotGeneration,
    SnapshotRetention, ThumbnailPurpose, ThumbnailSource, preferred_thumbnail_source,
    snapshot_dimensions,
};
use smithay::backend::renderer::Texture;

pub(super) const THUMBNAIL_DOWNSAMPLE_VERTEX_SHADER: &str = r#"#version 300 es
layout(location = 0) in vec2 a_position;
out vec2 v_uv;

void main() {
    v_uv = a_position;
    gl_Position = vec4(a_position.x * 2.0 - 1.0,
                       1.0 - a_position.y * 2.0,
                       0.0,
                       1.0);
}
"#;

const MINIMIZED_GPU_CACHE_MAX_BYTES: usize = 12 * 1024 * 1024;
const MINIMIZED_GPU_CACHE_MAX_ENTRIES: usize = 64;
// The udev path still parks a mapped surface offscreen. Until a future
// true-unmap gate exists, every CPU snapshot must remain an LRU victim.
const WAYLAND_SNAPSHOT_RETENTION: SnapshotRetention = SnapshotRetention::RecapturableMapped;

#[derive(Clone, Copy)]
struct ThumbnailDownsampleUniforms {
    texture: i32,
    uv_rect: i32,
    output_size: i32,
    has_alpha: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SnapshotTextureStorage {
    CapturedFramebuffer,
    CpuTopLeftUpload,
}

const fn snapshot_texture_uv_rect(storage: SnapshotTextureStorage) -> [f32; 4] {
    match storage {
        // FBO row zero is the image bottom. The normal top-down compositor
        // quad therefore samples from v=1 towards v=0.
        SnapshotTextureStorage::CapturedFramebuffer => [0.0, 1.0, 1.0, -1.0],
        // TexImage2D receives the shared top-left CPU rows verbatim. In that
        // storage convention v=0 is intentionally the visual top.
        SnapshotTextureStorage::CpuTopLeftUpload => [0.0, 0.0, 1.0, 1.0],
    }
}

pub(super) struct MinimizedGpuSnapshot {
    texture: u32,
    width: u32,
    height: u32,
    has_alpha: bool,
    generation: SnapshotGeneration,
    storage: SnapshotTextureStorage,
    last_use: u64,
}

impl MinimizedGpuSnapshot {
    const fn estimated_bytes(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    const fn uv_rect(&self) -> [f32; 4] {
        snapshot_texture_uv_rect(self.storage)
    }
}

struct CapturedMinimizedSnapshot {
    cpu: MinimizedSnapshot,
    gpu: MinimizedGpuSnapshot,
    color_transform: Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
}

#[derive(Clone, Copy)]
struct MinimizedCaptureSource {
    texture: u32,
    width: u32,
    height: u32,
    has_alpha: bool,
    uv_rect: [f32; 4],
    color_transform: Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
}

#[derive(Clone, Copy)]
pub(super) struct MinimizedRenderSource {
    pub(super) texture: u32,
    pub(super) width: f32,
    pub(super) height: f32,
    pub(super) has_alpha: bool,
    pub(super) uv_rect: [f32; 4],
    pub(super) color_transform:
        Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct MinimizedThumbnailAvailability {
    live: bool,
    retained: bool,
    gpu: bool,
    cpu: bool,
}

fn select_minimized_thumbnail_source(
    purpose: ThumbnailPurpose,
    available: MinimizedThumbnailAvailability,
) -> Option<ThumbnailSource> {
    preferred_thumbnail_source(
        purpose,
        [
            available.live.then_some(ThumbnailSource::LiveMappedTexture),
            available
                .retained
                .then_some(ThumbnailSource::RetainedVisual),
            available.gpu.then_some(ThumbnailSource::GpuSnapshot),
            available.cpu.then_some(ThumbnailSource::CpuSnapshot),
        ]
        .into_iter()
        .flatten(),
    )
}

fn minimized_thumbnail_source_is_drawable(
    purpose: ThumbnailPurpose,
    available: MinimizedThumbnailAvailability,
) -> bool {
    matches!(
        select_minimized_thumbnail_source(purpose, available),
        Some(
            ThumbnailSource::LiveMappedTexture
                | ThumbnailSource::RetainedVisual
                | ThumbnailSource::GpuSnapshot
        )
    )
}

fn next_snapshot_generation(clock: &mut u64) -> SnapshotGeneration {
    *clock = clock.wrapping_add(1);
    if *clock == 0 {
        *clock = 1;
    }
    SnapshotGeneration::new(*clock).expect("snapshot generation clock skips zero")
}

fn gpu_snapshot_lru_candidate<K: Copy + Eq>(
    entries: impl IntoIterator<Item = (K, u64)>,
    protected: K,
) -> Option<K> {
    entries
        .into_iter()
        .filter(|(window, _)| *window != protected)
        .min_by_key(|(_, last_use)| *last_use)
        .map(|(window, _)| window)
}

fn visual_dimensions(width: f32, height: f32) -> Option<(u32, u32)> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    Some((
        width.round().clamp(1.0, u32::MAX as f32) as u32,
        height.round().clamp(1.0, u32::MAX as f32) as u32,
    ))
}

fn oriented_content_uv(content_uv: [f32; 4], y_inverted: bool) -> [f32; 4] {
    let [u, v, width, height] = content_uv;
    if y_inverted {
        [u, v + height, width, -height]
    } else {
        content_uv
    }
}

pub(super) struct MinimizedThumbnailState {
    downsample_program: u32,
    uniforms: ThumbnailDownsampleUniforms,
    cpu: MinimizedSnapshotCache<u64>,
    gpu: HashMap<u64, MinimizedGpuSnapshot>,
    generations: HashMap<u64, SnapshotGeneration>,
    color_transforms:
        HashMap<u64, Option<crate::backend::wayland_udev::color_pipeline::ColorTransform>>,
    pending_captures: HashSet<u64>,
    /// A CPU snapshot is uploaded at most once per explicit demand event.
    /// Consuming this gate on failure prevents an unrelated render loop from
    /// retrying allocation forever under GPU pressure.
    gpu_upload_armed: HashSet<u64>,
    retired_gpu_textures: Vec<u32>,
    generation_clock: u64,
    gpu_use_clock: u64,
}

impl MinimizedThumbnailState {
    /// Build the CPU/GPU cache state around a program whose raw ownership has
    /// already been registered by the compositor construction guard.
    pub(super) unsafe fn from_program(gl: &ffi::Gles2, downsample_program: u32) -> Self {
        unsafe {
            Self {
                downsample_program,
                uniforms: ThumbnailDownsampleUniforms {
                    texture: super::get_uniform_loc(gl, downsample_program, "u_texture"),
                    uv_rect: super::get_uniform_loc(gl, downsample_program, "u_uv_rect"),
                    output_size: super::get_uniform_loc(gl, downsample_program, "u_output_size"),
                    has_alpha: super::get_uniform_loc(gl, downsample_program, "u_has_alpha"),
                },
                cpu: MinimizedSnapshotCache::new(),
                gpu: HashMap::new(),
                generations: HashMap::new(),
                color_transforms: HashMap::new(),
                pending_captures: HashSet::new(),
                gpu_upload_armed: HashSet::new(),
                retired_gpu_textures: Vec::new(),
                generation_clock: 0,
                gpu_use_clock: 0,
            }
        }
    }

    #[cfg(test)]
    fn for_tests() -> Self {
        Self {
            downsample_program: 0,
            uniforms: ThumbnailDownsampleUniforms {
                texture: -1,
                uv_rect: -1,
                output_size: -1,
                has_alpha: -1,
            },
            cpu: MinimizedSnapshotCache::new(),
            gpu: HashMap::new(),
            generations: HashMap::new(),
            color_transforms: HashMap::new(),
            pending_captures: HashSet::new(),
            gpu_upload_armed: HashSet::new(),
            retired_gpu_textures: Vec::new(),
            generation_clock: 0,
            gpu_use_clock: 0,
        }
    }

    fn ensure_generation(&mut self, window_id: u64) -> SnapshotGeneration {
        if let Some(generation) = self.generations.get(&window_id) {
            return *generation;
        }
        let generation = next_snapshot_generation(&mut self.generation_clock);
        self.generations.insert(window_id, generation);
        generation
    }

    fn arm_capture(&mut self, window_id: u64) -> bool {
        let generation = self.ensure_generation(window_id);
        if self
            .cpu
            .peek(&window_id)
            .is_some_and(|snapshot| snapshot.generation() == generation)
        {
            if !self.current_gpu_available(window_id) {
                self.gpu_upload_armed.insert(window_id);
            }
            return false;
        }
        self.pending_captures.insert(window_id)
    }

    fn current_cpu_available(&self, window_id: u64) -> bool {
        let Some(generation) = self.generations.get(&window_id) else {
            return false;
        };
        self.cpu
            .peek(&window_id)
            .is_some_and(|snapshot| snapshot.generation() == *generation)
    }

    fn current_gpu_available(&self, window_id: u64) -> bool {
        let Some(generation) = self.generations.get(&window_id) else {
            return false;
        };
        self.gpu
            .get(&window_id)
            .is_some_and(|snapshot| snapshot.generation == *generation)
    }

    fn next_gpu_use(&mut self) -> u64 {
        self.gpu_use_clock = self.gpu_use_clock.saturating_add(1);
        self.gpu_use_clock
    }

    fn remove_gpu(&mut self, window_id: u64) -> bool {
        self.gpu_upload_armed.remove(&window_id);
        let Some(snapshot) = self.gpu.remove(&window_id) else {
            return false;
        };
        self.retired_gpu_textures.push(snapshot.texture);
        true
    }

    fn enforce_gpu_budget(&mut self, protected: u64) {
        while self.gpu.len() > MINIMIZED_GPU_CACHE_MAX_ENTRIES
            || self
                .gpu
                .values()
                .map(MinimizedGpuSnapshot::estimated_bytes)
                .fold(0usize, usize::saturating_add)
                > MINIMIZED_GPU_CACHE_MAX_BYTES
        {
            let Some(victim) = gpu_snapshot_lru_candidate(
                self.gpu
                    .iter()
                    .map(|(&window, snapshot)| (window, snapshot.last_use)),
                protected,
            ) else {
                break;
            };
            self.remove_gpu(victim);
        }
    }

    fn insert_gpu(&mut self, window_id: u64, mut snapshot: MinimizedGpuSnapshot) {
        self.gpu_upload_armed.remove(&window_id);
        snapshot.last_use = self.next_gpu_use();
        if let Some(old) = self.gpu.insert(window_id, snapshot) {
            self.retired_gpu_textures.push(old.texture);
        }
        self.enforce_gpu_budget(window_id);
    }

    fn touch(&mut self, window_id: u64) -> bool {
        let cpu_current = self.current_cpu_available(window_id);
        let mut touched = cpu_current && self.cpu.get(&window_id).is_some();
        if cpu_current && !self.current_gpu_available(window_id) {
            self.gpu_upload_armed.insert(window_id);
        }
        if self.gpu.contains_key(&window_id) {
            let last_use = self.next_gpu_use();
            self.gpu
                .get_mut(&window_id)
                .expect("checked minimized GPU snapshot disappeared")
                .last_use = last_use;
            touched = true;
        }
        touched
    }

    fn take_gpu_upload_attempt(&mut self, window_id: u64) -> bool {
        self.gpu_upload_armed.remove(&window_id)
    }

    fn retire(&mut self, window_id: u64) -> bool {
        let pending = self.pending_captures.remove(&window_id);
        let upload = self.gpu_upload_armed.remove(&window_id);
        let cpu = self.cpu.remove(&window_id).is_some();
        let gpu = self.remove_gpu(window_id);
        let generation = self.generations.remove(&window_id).is_some();
        let color = self.color_transforms.remove(&window_id).is_some();
        pending || upload || cpu || gpu || generation || color
    }

    unsafe fn drain_retired_gpu_textures(&mut self, gl: &ffi::Gles2) {
        unsafe {
            for texture in self.retired_gpu_textures.drain(..) {
                gl.DeleteTextures(1, &texture);
            }
        }
    }

    /// Release this tier's independent raw GLES owners.  Strong
    /// `GlesTexture` owners used by live/full-resolution Genie paths are not
    /// part of this state and remain Smithay-managed.
    pub(super) unsafe fn release_gpu_resources(&mut self, gl: &ffi::Gles2) {
        unsafe {
            self.drain_retired_gpu_textures(gl);
            for (_, snapshot) in self.gpu.drain() {
                gl.DeleteTextures(1, &snapshot.texture);
            }
            self.gpu_upload_armed.clear();
            if self.downsample_program != 0 {
                gl.DeleteProgram(self.downsample_program);
                self.downsample_program = 0;
            }
        }
    }

    #[cfg(test)]
    pub(super) const fn downsample_program_for_tests(&self) -> u32 {
        self.downsample_program
    }
}

impl WaylandCompositor {
    pub(super) fn arm_minimized_snapshot_capture(&mut self, window_id: u64) -> bool {
        self.minimized_thumbnails.arm_capture(window_id)
    }

    pub(super) fn touch_minimized_snapshot(&mut self, window_id: u64) -> bool {
        self.minimized_thumbnails.touch(window_id)
    }

    pub(super) fn discard_minimized_snapshot(&mut self, window_id: u64) -> bool {
        self.minimized_thumbnails.retire(window_id)
    }

    pub(super) fn minimized_preview_source_available(&self, window_id: u64) -> bool {
        select_minimized_thumbnail_source(
            ThumbnailPurpose::HoverPreview,
            self.minimized_thumbnail_availability(window_id),
        )
        .is_some()
    }

    pub(super) fn minimized_static_source_available(&self, window_id: u64) -> bool {
        select_minimized_thumbnail_source(
            ThumbnailPurpose::StaticDockCard,
            self.minimized_thumbnail_availability(window_id),
        )
        .is_some()
    }

    /// Whether this frame can actually draw pixels without allocating a new
    /// GL texture. CPU-only snapshots remain render candidates so they can
    /// consume one armed upload attempt, but must not permanently defeat
    /// direct scanout after that allocation fails.
    pub(super) fn minimized_static_drawable_source_available(&self, window_id: u64) -> bool {
        minimized_thumbnail_source_is_drawable(
            ThumbnailPurpose::StaticDockCard,
            self.minimized_thumbnail_availability(window_id),
        )
    }

    pub(super) fn minimized_preview_drawable_source_available(&self, window_id: u64) -> bool {
        minimized_thumbnail_source_is_drawable(
            ThumbnailPurpose::HoverPreview,
            self.minimized_thumbnail_availability(window_id),
        )
    }

    pub(super) fn minimized_full_source_available(&self, window_id: u64) -> bool {
        self.minimized_visuals.contains_key(&window_id)
            || self
                .genie_active
                .iter()
                .any(|animation| animation.window_id == window_id)
            || self
                .windows
                .get(&window_id)
                .is_some_and(|window| window.texture_owner.is_some())
    }

    pub(super) fn minimized_low_resolution_source_available(&self, window_id: u64) -> bool {
        self.minimized_thumbnails.current_gpu_available(window_id)
            || self.minimized_thumbnails.current_cpu_available(window_id)
    }

    fn minimized_thumbnail_availability(&self, window_id: u64) -> MinimizedThumbnailAvailability {
        MinimizedThumbnailAvailability {
            live: self
                .windows
                .get(&window_id)
                .is_some_and(|window| window.texture_owner.is_some()),
            retained: self.minimized_visuals.contains_key(&window_id)
                || self
                    .genie_active
                    .iter()
                    .any(|animation| animation.window_id == window_id),
            gpu: self.minimized_thumbnails.current_gpu_available(window_id),
            cpu: self.minimized_thumbnails.current_cpu_available(window_id),
        }
    }

    fn minimized_snapshot_capture_source(&self, window_id: u64) -> Option<MinimizedCaptureSource> {
        if let Some(animation) = self
            .genie_active
            .iter()
            .find(|animation| animation.window_id == window_id)
        {
            let (width, height) = visual_dimensions(
                animation.texture_owner.width() as f32 * animation.content_uv[2].abs(),
                animation.texture_owner.height() as f32 * animation.content_uv[3].abs(),
            )?;
            return Some(MinimizedCaptureSource {
                texture: animation.texture_owner.tex_id(),
                width,
                height,
                has_alpha: animation.has_alpha,
                uv_rect: oriented_content_uv(animation.content_uv, animation.y_inverted),
                color_transform: animation.color_transform,
            });
        }
        if let Some(visual) = self.minimized_visuals.get(&window_id) {
            let (width, height) = visual_dimensions(
                visual.texture_owner.width() as f32 * visual.content_uv[2].abs(),
                visual.texture_owner.height() as f32 * visual.content_uv[3].abs(),
            )?;
            return Some(MinimizedCaptureSource {
                texture: visual.texture_owner.tex_id(),
                width,
                height,
                has_alpha: visual.has_alpha,
                uv_rect: oriented_content_uv(visual.content_uv, visual.y_inverted),
                color_transform: visual.color_transform,
            });
        }
        let window = self.windows.get(&window_id)?;
        let texture = window.texture_owner.as_ref()?;
        let (width, height) = visual_dimensions(
            window.width as f32 * window.content_uv[2].abs(),
            window.height as f32 * window.content_uv[3].abs(),
        )?;
        Some(MinimizedCaptureSource {
            texture: texture.tex_id(),
            width,
            height,
            has_alpha: window.has_alpha,
            uv_rect: oriented_content_uv(window.content_uv, window.y_inverted),
            color_transform: window.color_transform,
        })
    }

    unsafe fn capture_minimized_snapshot_from_source(
        &self,
        gl: &ffi::Gles2,
        source: MinimizedCaptureSource,
        generation: SnapshotGeneration,
    ) -> Option<CapturedMinimizedSnapshot> {
        let (width, height) = snapshot_dimensions(source.width, source.height)?;
        unsafe {
            let _state = super::ThumbnailGlesState::begin(gl);
            let mut resources = super::ThumbnailGlesResources {
                gl,
                texture: 0,
                framebuffer: 0,
            };
            gl.GenTextures(1, &mut resources.texture);
            if resources.texture == 0 {
                log::warn!("[udev/compositor] minimized snapshot texture allocation failed");
                return None;
            }
            gl.BindTexture(ffi::TEXTURE_2D, resources.texture);
            gl.TexImage2D(
                ffi::TEXTURE_2D,
                0,
                ffi::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                std::ptr::null(),
            );
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_S,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_T,
                ffi::CLAMP_TO_EDGE as i32,
            );

            gl.GenFramebuffers(1, &mut resources.framebuffer);
            if resources.framebuffer == 0 {
                log::warn!("[udev/compositor] minimized snapshot framebuffer allocation failed");
                return None;
            }
            gl.BindFramebuffer(ffi::FRAMEBUFFER, resources.framebuffer);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                resources.texture,
                0,
            );
            if gl.CheckFramebufferStatus(ffi::FRAMEBUFFER) != ffi::FRAMEBUFFER_COMPLETE {
                log::warn!("[udev/compositor] minimized snapshot framebuffer is incomplete");
                return None;
            }

            gl.Viewport(0, 0, width as i32, height as i32);
            gl.ClearColor(0.0, 0.0, 0.0, 0.0);
            gl.Clear(ffi::COLOR_BUFFER_BIT);
            gl.UseProgram(self.minimized_thumbnails.downsample_program);
            gl.Uniform1i(self.minimized_thumbnails.uniforms.texture, 0);
            gl.Uniform4f(
                self.minimized_thumbnails.uniforms.uv_rect,
                source.uv_rect[0],
                source.uv_rect[1],
                source.uv_rect[2],
                source.uv_rect[3],
            );
            gl.Uniform2f(
                self.minimized_thumbnails.uniforms.output_size,
                width as f32,
                height as f32,
            );
            gl.Uniform1i(
                self.minimized_thumbnails.uniforms.has_alpha,
                i32::from(source.has_alpha),
            );
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, source.texture);
            gl.BindVertexArray(self.quad_vao);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);

            let mut rgba = vec![0u8; width as usize * height as usize * 4];
            gl.ReadPixels(
                0,
                0,
                width as i32,
                height as i32,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                rgba.as_mut_ptr().cast(),
            );
            flip_rgba_vertical(&mut rgba, width, height);
            let cpu =
                MinimizedSnapshot::try_new(width, height, generation.get(), source.has_alpha, rgba)
                    .ok()?;
            let texture = resources.texture;
            resources.texture = 0;
            Some(CapturedMinimizedSnapshot {
                cpu,
                gpu: MinimizedGpuSnapshot {
                    texture,
                    width,
                    height,
                    has_alpha: source.has_alpha,
                    generation,
                    storage: SnapshotTextureStorage::CapturedFramebuffer,
                    last_use: 0,
                },
                color_transform: source.color_transform,
            })
        }
    }

    fn admit_captured_minimized_snapshot(
        &mut self,
        gl: &ffi::Gles2,
        window_id: u64,
        captured: CapturedMinimizedSnapshot,
    ) -> bool {
        let expected_generation = self
            .minimized_thumbnails
            .generations
            .get(&window_id)
            .copied();
        if expected_generation != Some(captured.cpu.generation())
            || captured.gpu.generation != captured.cpu.generation()
        {
            unsafe { gl.DeleteTextures(1, &captured.gpu.texture) };
            return false;
        }

        match self.minimized_thumbnails.cpu.admit(
            window_id,
            captured.cpu,
            // Hidden Wayland surfaces remain mapped and can be imported again;
            // pinning here would make the 128-entry cache reject every newer
            // addressable window once full.
            WAYLAND_SNAPSHOT_RETENTION,
        ) {
            AdmissionOutcome::Admitted { evicted } => {
                for victim in evicted {
                    self.minimized_thumbnails.remove_gpu(victim);
                    self.minimized_thumbnails.color_transforms.remove(&victim);
                }
                self.minimized_thumbnails
                    .color_transforms
                    .insert(window_id, captured.color_transform);
                self.minimized_thumbnails
                    .insert_gpu(window_id, captured.gpu);
                self.resume_minimized_preview_after_capture(window_id);
                self.force_full_damage_next = true;
                self.needs_render = true;
                true
            }
            AdmissionOutcome::AlreadyCurrent
            | AdmissionOutcome::RejectedStale
            | AdmissionOutcome::RejectedCapacity => {
                unsafe { gl.DeleteTextures(1, &captured.gpu.texture) };
                false
            }
        }
    }

    pub(super) fn capture_pending_minimized_snapshots(&mut self, gl: &ffi::Gles2) {
        unsafe {
            self.minimized_thumbnails.drain_retired_gpu_textures(gl);
        }
        let pending = self
            .minimized_thumbnails
            .pending_captures
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for window_id in pending {
            let Some(source) = self.minimized_snapshot_capture_source(window_id) else {
                continue;
            };
            let Some(generation) = self
                .minimized_thumbnails
                .generations
                .get(&window_id)
                .copied()
            else {
                self.minimized_thumbnails
                    .pending_captures
                    .remove(&window_id);
                continue;
            };
            // One source-bearing attempt per arm. A later geometry/hover/full
            // recapture event may explicitly rearm a transient GL failure.
            self.minimized_thumbnails
                .pending_captures
                .remove(&window_id);
            let captured =
                unsafe { self.capture_minimized_snapshot_from_source(gl, source, generation) };
            if let Some(captured) = captured {
                self.admit_captured_minimized_snapshot(gl, window_id, captured);
            }
        }
        unsafe {
            self.minimized_thumbnails.drain_retired_gpu_textures(gl);
        }
    }

    unsafe fn upload_minimized_snapshot_texture(
        &self,
        gl: &ffi::Gles2,
        snapshot: &MinimizedSnapshot,
    ) -> Option<MinimizedGpuSnapshot> {
        unsafe {
            let _state = super::ThumbnailGlesState::begin(gl);
            let mut resources = super::ThumbnailGlesResources {
                gl,
                texture: 0,
                framebuffer: 0,
            };
            gl.GenTextures(1, &mut resources.texture);
            if resources.texture == 0 {
                return None;
            }
            gl.BindTexture(ffi::TEXTURE_2D, resources.texture);
            gl.TexImage2D(
                ffi::TEXTURE_2D,
                0,
                ffi::RGBA8 as i32,
                snapshot.width() as i32,
                snapshot.height() as i32,
                0,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                snapshot.rgba().as_ptr().cast(),
            );
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_S,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_T,
                ffi::CLAMP_TO_EDGE as i32,
            );
            // TexImage2D reports OOM through GL state rather than its return
            // value. Attaching the fresh storage gives us a deterministic
            // success criterion without turning a failed allocation into a
            // permanently "resident" blank texture.
            gl.GenFramebuffers(1, &mut resources.framebuffer);
            if resources.framebuffer == 0 {
                return None;
            }
            gl.BindFramebuffer(ffi::FRAMEBUFFER, resources.framebuffer);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                resources.texture,
                0,
            );
            if gl.CheckFramebufferStatus(ffi::FRAMEBUFFER) != ffi::FRAMEBUFFER_COMPLETE {
                return None;
            }
            let texture = resources.texture;
            resources.texture = 0;
            Some(MinimizedGpuSnapshot {
                texture,
                width: snapshot.width(),
                height: snapshot.height(),
                has_alpha: snapshot.has_alpha(),
                generation: snapshot.generation(),
                storage: SnapshotTextureStorage::CpuTopLeftUpload,
                last_use: 0,
            })
        }
    }

    fn ensure_minimized_gpu_snapshot(&mut self, gl: &ffi::Gles2, window_id: u64) -> bool {
        let Some(generation) = self
            .minimized_thumbnails
            .generations
            .get(&window_id)
            .copied()
        else {
            return false;
        };
        if self
            .minimized_thumbnails
            .gpu
            .get(&window_id)
            .is_some_and(|snapshot| snapshot.generation == generation)
        {
            return true;
        }
        if !self.minimized_thumbnails.take_gpu_upload_attempt(window_id) {
            return false;
        }
        self.minimized_thumbnails.remove_gpu(window_id);
        let Some(snapshot) = self
            .minimized_thumbnails
            .cpu
            .get(&window_id)
            .filter(|snapshot| snapshot.generation() == generation)
            .cloned()
        else {
            return false;
        };
        let Some(gpu) = (unsafe { self.upload_minimized_snapshot_texture(gl, &snapshot) }) else {
            return false;
        };
        self.minimized_thumbnails.insert_gpu(window_id, gpu);
        unsafe {
            self.minimized_thumbnails.drain_retired_gpu_textures(gl);
        }
        true
    }

    pub(super) fn minimized_render_source(
        &mut self,
        gl: &ffi::Gles2,
        window_id: u64,
        purpose: ThumbnailPurpose,
    ) -> Option<MinimizedRenderSource> {
        if purpose != ThumbnailPurpose::RestoreAnimation
            && !self.minimized_thumbnails.current_gpu_available(window_id)
            && self.minimized_thumbnails.current_cpu_available(window_id)
        {
            self.ensure_minimized_gpu_snapshot(gl, window_id);
        }
        let source = select_minimized_thumbnail_source(
            purpose,
            self.minimized_thumbnail_availability(window_id),
        )?;
        match source {
            ThumbnailSource::GpuSnapshot => {
                let snapshot = self.minimized_thumbnails.gpu.get(&window_id)?;
                Some(MinimizedRenderSource {
                    texture: snapshot.texture,
                    width: snapshot.width as f32,
                    height: snapshot.height as f32,
                    has_alpha: snapshot.has_alpha,
                    uv_rect: snapshot.uv_rect(),
                    color_transform: self
                        .minimized_thumbnails
                        .color_transforms
                        .get(&window_id)
                        .copied()
                        .flatten(),
                })
            }
            ThumbnailSource::RetainedVisual => self
                .minimized_visuals
                .get(&window_id)
                .map(|visual| MinimizedRenderSource {
                    texture: visual.texture_owner.tex_id(),
                    width: visual.w,
                    height: visual.h,
                    has_alpha: visual.has_alpha,
                    uv_rect: oriented_content_uv(visual.content_uv, visual.y_inverted),
                    color_transform: visual.color_transform,
                })
                .or_else(|| {
                    self.genie_active
                        .iter()
                        .find(|animation| animation.window_id == window_id)
                        .map(|animation| MinimizedRenderSource {
                            texture: animation.texture_owner.tex_id(),
                            width: animation.w,
                            height: animation.h,
                            has_alpha: animation.has_alpha,
                            uv_rect: oriented_content_uv(
                                animation.content_uv,
                                animation.y_inverted,
                            ),
                            color_transform: animation.color_transform,
                        })
                }),
            ThumbnailSource::LiveMappedTexture => {
                let window = self.windows.get(&window_id)?;
                let texture = window.texture_owner.as_ref()?;
                let (width, height) = visual_dimensions(
                    window.width as f32 * window.content_uv[2].abs(),
                    window.height as f32 * window.content_uv[3].abs(),
                )?;
                Some(MinimizedRenderSource {
                    texture: texture.tex_id(),
                    width: width as f32,
                    height: height as f32,
                    has_alpha: window.has_alpha,
                    uv_rect: oriented_content_uv(window.content_uv, window.y_inverted),
                    color_transform: window.color_transform,
                })
            }
            // Raw GLES cannot draw CPU bytes directly. A failed lazy upload
            // leaves them durable for a later retry and falls back to the icon.
            ThumbnailSource::CpuSnapshot | ThumbnailSource::Placeholder => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_and_hover_keep_low_res_after_full_retained_eviction() {
        let low_res_only = MinimizedThumbnailAvailability {
            gpu: true,
            ..Default::default()
        };
        assert_eq!(
            select_minimized_thumbnail_source(ThumbnailPurpose::StaticDockCard, low_res_only),
            Some(ThumbnailSource::GpuSnapshot)
        );
        assert_eq!(
            select_minimized_thumbnail_source(ThumbnailPurpose::HoverPreview, low_res_only),
            Some(ThumbnailSource::GpuSnapshot)
        );
    }

    #[test]
    fn hover_prefers_full_pixels_but_static_cards_prefer_low_res() {
        let all = MinimizedThumbnailAvailability {
            live: true,
            retained: true,
            gpu: true,
            cpu: true,
        };
        assert_eq!(
            select_minimized_thumbnail_source(ThumbnailPurpose::StaticDockCard, all),
            Some(ThumbnailSource::GpuSnapshot)
        );
        assert_eq!(
            select_minimized_thumbnail_source(ThumbnailPurpose::HoverPreview, all),
            Some(ThumbnailSource::RetainedVisual)
        );
    }

    #[test]
    fn restore_rejects_both_low_resolution_tiers() {
        assert_eq!(
            select_minimized_thumbnail_source(
                ThumbnailPurpose::RestoreAnimation,
                MinimizedThumbnailAvailability {
                    gpu: true,
                    cpu: true,
                    ..Default::default()
                },
            ),
            None
        );
    }

    #[test]
    fn direct_scanout_blocker_requires_pixels_drawable_this_frame() {
        assert!(!minimized_thumbnail_source_is_drawable(
            ThumbnailPurpose::StaticDockCard,
            MinimizedThumbnailAvailability {
                cpu: true,
                ..Default::default()
            },
        ));
        assert!(minimized_thumbnail_source_is_drawable(
            ThumbnailPurpose::StaticDockCard,
            MinimizedThumbnailAvailability {
                gpu: true,
                ..Default::default()
            },
        ));
        assert!(!minimized_thumbnail_source_is_drawable(
            ThumbnailPurpose::HoverPreview,
            MinimizedThumbnailAvailability {
                cpu: true,
                ..Default::default()
            },
        ));
        assert!(minimized_thumbnail_source_is_drawable(
            ThumbnailPurpose::StaticDockCard,
            MinimizedThumbnailAvailability {
                retained: true,
                ..Default::default()
            },
        ));
    }

    #[test]
    fn capture_and_cpu_upload_have_explicit_opposite_uv_storage() {
        assert_eq!(
            snapshot_texture_uv_rect(SnapshotTextureStorage::CapturedFramebuffer),
            [0.0, 1.0, 1.0, -1.0]
        );
        assert_eq!(
            snapshot_texture_uv_rect(SnapshotTextureStorage::CpuTopLeftUpload),
            [0.0, 0.0, 1.0, 1.0]
        );
    }

    #[test]
    fn retire_then_rearm_cannot_reuse_stale_generation_or_gpu_texture() {
        let mut state = MinimizedThumbnailState::for_tests();
        assert!(state.arm_capture(42));
        let first_generation = state.generations[&42];
        let snapshot =
            MinimizedSnapshot::try_new(1, 1, first_generation.get(), true, vec![7; 4]).unwrap();
        assert!(matches!(
            state.cpu.admit(42, snapshot, WAYLAND_SNAPSHOT_RETENTION),
            AdmissionOutcome::Admitted { .. }
        ));
        state.gpu.insert(
            42,
            MinimizedGpuSnapshot {
                texture: 99,
                width: 1,
                height: 1,
                has_alpha: true,
                generation: first_generation,
                storage: SnapshotTextureStorage::CapturedFramebuffer,
                last_use: 1,
            },
        );

        assert!(state.retire(42));
        assert!(state.cpu.is_empty());
        assert!(!state.generations.contains_key(&42));
        assert!(!state.gpu.contains_key(&42));
        assert_eq!(state.retired_gpu_textures, vec![99]);

        assert!(state.arm_capture(42));
        assert!(state.generations[&42] > first_generation);
    }

    #[test]
    fn generation_clock_wraps_without_emitting_zero() {
        let mut clock = u64::MAX;
        assert_eq!(next_snapshot_generation(&mut clock).get(), 1);
    }

    #[test]
    fn gpu_lru_never_selects_the_newly_uploaded_texture() {
        assert_eq!(
            gpu_snapshot_lru_candidate([(1_u64, 1), (2, 2), (3, 3)], 1),
            Some(2)
        );
        assert_eq!(gpu_snapshot_lru_candidate([(1_u64, 1)], 1), None);
    }

    #[test]
    fn input_orientation_matches_the_existing_wayland_window_contract() {
        assert_eq!(
            oriented_content_uv([0.1, 0.2, 0.6, 0.5], false),
            [0.1, 0.2, 0.6, 0.5]
        );
        assert_eq!(
            oriented_content_uv([0.1, 0.2, 0.6, 0.5], true),
            [0.1, 0.7, 0.6, -0.5]
        );
    }

    #[test]
    fn mapped_cpu_tier_admits_newer_windows_past_the_entry_limit() {
        assert_eq!(
            WAYLAND_SNAPSHOT_RETENTION,
            SnapshotRetention::RecapturableMapped
        );
        let mut cache = MinimizedSnapshotCache::new();
        let limit =
            crate::backend::compositor_common::minimized_thumbnail::SNAPSHOT_CACHE_MAX_ENTRIES;
        for window_id in 0..=limit as u64 {
            let snapshot = MinimizedSnapshot::try_new(1, 1, 1, false, vec![0; 4]).unwrap();
            assert!(matches!(
                cache.admit(window_id, snapshot, WAYLAND_SNAPSHOT_RETENTION),
                AdmissionOutcome::Admitted { .. }
            ));
        }
        assert_eq!(cache.len(), limit);
        assert!(cache.peek(&0).is_none());
        assert!(cache.peek(&(limit as u64)).is_some());
    }

    #[test]
    fn failed_lazy_upload_is_not_retried_by_unrelated_frames() {
        let mut state = MinimizedThumbnailState::for_tests();
        assert!(state.arm_capture(7));
        let generation = state.generations[&7];
        let snapshot =
            MinimizedSnapshot::try_new(1, 1, generation.get(), true, vec![0; 4]).unwrap();
        assert!(matches!(
            state.cpu.admit(7, snapshot, WAYLAND_SNAPSHOT_RETENTION),
            AdmissionOutcome::Admitted { .. }
        ));
        state.pending_captures.remove(&7);

        // Geometry/new-capture demand arms one attempt. Model a failed GL
        // allocation by consuming it without inserting a GPU snapshot.
        assert!(!state.arm_capture(7));
        assert!(state.take_gpu_upload_attempt(7));
        for _ in 0..100 {
            assert!(!state.take_gpu_upload_attempt(7));
        }

        // A later explicit hover/geometry touch permits exactly one retry.
        assert!(state.touch(7));
        assert!(state.take_gpu_upload_attempt(7));
        assert!(!state.take_gpu_upload_attempt(7));
    }
}
