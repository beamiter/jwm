#!/usr/bin/env python3
"""Integrate the slime hand effect into the shared X11 compositor.

The branch was bootstrapped with the standalone IPC/state module first.  This
script performs the mechanical cross-file wiring with guarded, one-shot text
replacements.  It is intentionally kept in-tree so the integration remains
reproducible and fails loudly when upstream source layout changes.
"""

from __future__ import annotations

from pathlib import Path
import sys

ROOT = Path(__file__).resolve().parents[1]


def replace_once(path: str, old: str, new: str, *, marker: str) -> bool:
    target = ROOT / path
    text = target.read_text(encoding="utf-8")
    if marker in text:
        print(f"[skip] {path}: {marker}")
        return False
    count = text.count(old)
    if count != 1:
        raise RuntimeError(
            f"{path}: expected exactly one integration anchor, found {count}: {old[:90]!r}"
        )
    target.write_text(text.replace(old, new, 1), encoding="utf-8")
    print(f"[edit] {path}: {marker}")
    return True


def patch_shader() -> bool:
    path = ROOT / "src/backend/x11/compositor/common/shaders.rs"
    text = path.read_text(encoding="utf-8")
    if "uniform int   u_slime_enabled;" in text:
        print(f"[skip] {path.relative_to(ROOT)}: slime shader")
        return False

    start_token = 'pub const MAGNIFIER_POSTPROCESS_FRAGMENT_SHADER: &str = r#"#version 330 core'
    start = text.find(start_token)
    if start < 0:
        raise RuntimeError("magnifier postprocess shader start not found")
    end = text.find('\n"#;', start)
    if end < 0:
        raise RuntimeError("magnifier postprocess shader end not found")
    end += len('\n"#;')
    shader = text[start:end]

    decl_anchor = """uniform float u_magnifier_zoom;    // zoom factor (e.g. 2.0)
// Colorblind correction uniform"""
    declarations = """uniform float u_magnifier_zoom;    // zoom factor (e.g. 2.0)
// Slime hand-refraction uniforms. Points and bbox use top-left screen pixels.
uniform int   u_slime_enabled;
uniform vec2  u_slime_points[21];
uniform vec4  u_slime_bbox;        // min_x, min_y, max_x, max_y
uniform vec2  u_slime_screen_size;
uniform float u_slime_scale;       // palm scale in pixels
uniform float u_slime_strength;    // refraction displacement in pixels
uniform float u_slime_opacity;
// Colorblind correction uniform"""
    if shader.count(decl_anchor) != 1:
        raise RuntimeError("slime shader declaration anchor changed")
    shader = shader.replace(decl_anchor, declarations, 1)

    helper_anchor = """in vec2 v_uv;
out vec4 frag_color;

void main() {"""
    helpers = """in vec2 v_uv;
out vec4 frag_color;

float slime_capsule_sdf(vec2 p, vec2 a, vec2 b, float radius) {
    vec2 pa = p - a;
    vec2 ba = b - a;
    float h = clamp(dot(pa, ba) / max(dot(ba, ba), 0.0001), 0.0, 1.0);
    return length(pa - ba * h) - radius;
}

float slime_hand_sdf(vec2 p) {
    float radius = max(u_slime_scale * 0.115, 3.0);
    float d = 100000.0;

    // Five finger chains. MediaPipe landmarks are laid out as four joints per
    // finger after the wrist: thumb 1..4, index 5..8, ... pinky 17..20.
    for (int finger = 0; finger < 5; ++finger) {
        int base = 1 + finger * 4;
        float finger_radius = radius * (finger == 0 ? 1.12 : 1.0);
        d = min(d, slime_capsule_sdf(
            p, u_slime_points[0], u_slime_points[base], finger_radius * 1.18
        ));
        for (int joint = 0; joint < 3; ++joint) {
            float taper = 1.0 - float(joint) * 0.13;
            d = min(d, slime_capsule_sdf(
                p,
                u_slime_points[base + joint],
                u_slime_points[base + joint + 1],
                finger_radius * taper
            ));
        }
    }

    // Fill the palm with overlapping capsules and a central metaball.
    d = min(d, slime_capsule_sdf(p, u_slime_points[5], u_slime_points[9], radius * 1.65));
    d = min(d, slime_capsule_sdf(p, u_slime_points[9], u_slime_points[13], radius * 1.70));
    d = min(d, slime_capsule_sdf(p, u_slime_points[13], u_slime_points[17], radius * 1.65));
    d = min(d, slime_capsule_sdf(p, u_slime_points[0], u_slime_points[9], radius * 2.20));
    d = min(d, slime_capsule_sdf(p, u_slime_points[5], u_slime_points[17], radius * 2.10));
    vec2 palm_center = (
        u_slime_points[0] + u_slime_points[5] + u_slime_points[9]
        + u_slime_points[13] + u_slime_points[17]
    ) / 5.0;
    d = min(d, length(p - palm_center) - radius * 2.55);
    return d;
}

void main() {"""
    if shader.count(helper_anchor) != 1:
        raise RuntimeError("slime shader helper anchor changed")
    shader = shader.replace(helper_anchor, helpers, 1)

    effect_anchor = """    vec4 c = texture(u_texture, sample_uv);

    // Colorblind correction"""
    effect = """    vec4 c = texture(u_texture, sample_uv);

    // Slime-glass hand. The expensive SDF is evaluated only inside the CPU
    // supplied hand bounding box. Refraction is applied before color/HDR passes.
    if (u_slime_enabled == 1 && u_slime_opacity > 0.0) {
        vec2 pixel = vec2(
            uv.x * u_slime_screen_size.x,
            (1.0 - uv.y) * u_slime_screen_size.y
        );
        if (pixel.x >= u_slime_bbox.x && pixel.y >= u_slime_bbox.y
            && pixel.x <= u_slime_bbox.z && pixel.y <= u_slime_bbox.w) {
            float d = slime_hand_sdf(pixel);
            float aa = 2.0;
            float mask = (1.0 - smoothstep(-aa, aa, d)) * u_slime_opacity;
            if (mask > 0.001) {
                float epsilon = max(1.25, u_slime_scale * 0.018);
                vec2 gradient = vec2(
                    slime_hand_sdf(pixel + vec2(epsilon, 0.0))
                        - slime_hand_sdf(pixel - vec2(epsilon, 0.0)),
                    slime_hand_sdf(pixel + vec2(0.0, epsilon))
                        - slime_hand_sdf(pixel - vec2(0.0, epsilon))
                );
                float gradient_len = length(gradient);
                vec2 normal = gradient_len > 0.0001
                    ? gradient / gradient_len
                    : vec2(0.0, -1.0);

                float depth = clamp(-d / max(u_slime_scale * 0.62, 1.0), 0.0, 1.0);
                float edge_weight = 1.0 - depth;
                vec2 uv_normal = vec2(
                    normal.x / u_slime_screen_size.x,
                    -normal.y / u_slime_screen_size.y
                );
                vec2 offset = uv_normal * u_slime_strength
                    * (0.28 + edge_weight * 0.72) * mask;
                vec2 refracted_uv = clamp(sample_uv + offset, vec2(0.001), vec2(0.999));
                vec2 dispersion = uv_normal * u_slime_strength * 0.16;

                vec3 glass;
                glass.r = texture(u_texture, clamp(refracted_uv + dispersion, vec2(0.001), vec2(0.999))).r;
                glass.g = texture(u_texture, refracted_uv).g;
                glass.b = texture(u_texture, clamp(refracted_uv - dispersion, vec2(0.001), vec2(0.999))).b;
                c.rgb = mix(c.rgb, glass, mask * 0.94);

                float rim = (1.0 - smoothstep(0.0, max(u_slime_scale * 0.105, 2.0), abs(d)))
                    * u_slime_opacity;
                vec2 light_dir = normalize(vec2(-0.58, -0.82));
                float specular = pow(max(dot(normal, light_dir), 0.0), 18.0);
                float fresnel = pow(edge_weight, 1.55);
                c.rgb += vec3(0.82, 0.93, 1.0)
                    * (specular * 0.34 + fresnel * 0.10) * mask;
                c.rgb = mix(c.rgb, vec3(0.94, 0.98, 1.0), rim * 0.24);
            }
        }
    }

    // Colorblind correction"""
    if shader.count(effect_anchor) != 1:
        raise RuntimeError("slime shader effect anchor changed")
    shader = shader.replace(effect_anchor, effect, 1)

    path.write_text(text[:start] + shader + text[end:], encoding="utf-8")
    print(f"[edit] {path.relative_to(ROOT)}: slime shader")
    return True


def main() -> int:
    changed = False

    changed |= replace_once(
        "src/backend/x11/compositor/mod.rs",
        "mod postprocess;\nmod tfp;",
        "mod postprocess;\nmod slime;\nmod tfp;",
        marker="mod slime;",
    )
    changed |= replace_once(
        "src/backend/x11/compositor/mod.rs",
        "use types::*;\npub(crate) use vsync::VsyncMethod;",
        "use slime::{SlimeIpc, SlimeState};\nuse types::*;\npub(crate) use vsync::VsyncMethod;",
        marker="use slime::{SlimeIpc, SlimeState};",
    )
    changed |= replace_once(
        "src/backend/x11/compositor/mod.rs",
        """    magnifier_zoom: f32,
    magnifier_uniforms: MagnifierUniforms,

    // --- Window 3D tilt ---""",
        """    magnifier_zoom: f32,
    magnifier_uniforms: MagnifierUniforms,

    // --- Realtime slime hand refraction ---
    slime_ipc: Option<SlimeIpc>,
    slime_state: SlimeState,

    // --- Window 3D tilt ---""",
        marker="// --- Realtime slime hand refraction ---",
    )

    changed |= replace_once(
        "src/backend/x11/compositor/types.rs",
        """pub(super) struct MagnifierUniforms {
    pub(super) magnifier_enabled: Option<glow::UniformLocation>,
    pub(super) magnifier_center: Option<glow::UniformLocation>,
    pub(super) magnifier_radius: Option<glow::UniformLocation>,
    pub(super) magnifier_zoom: Option<glow::UniformLocation>,
    pub(super) colorblind_mode: Option<glow::UniformLocation>,
}""",
        """pub(super) struct MagnifierUniforms {
    pub(super) magnifier_enabled: Option<glow::UniformLocation>,
    pub(super) magnifier_center: Option<glow::UniformLocation>,
    pub(super) magnifier_radius: Option<glow::UniformLocation>,
    pub(super) magnifier_zoom: Option<glow::UniformLocation>,
    pub(super) slime_enabled: Option<glow::UniformLocation>,
    pub(super) slime_points: Option<glow::UniformLocation>,
    pub(super) slime_bbox: Option<glow::UniformLocation>,
    pub(super) slime_screen_size: Option<glow::UniformLocation>,
    pub(super) slime_scale: Option<glow::UniformLocation>,
    pub(super) slime_strength: Option<glow::UniformLocation>,
    pub(super) slime_opacity: Option<glow::UniformLocation>,
    pub(super) colorblind_mode: Option<glow::UniformLocation>,
}""",
        marker="pub(super) slime_enabled:",
    )

    changed |= replace_once(
        "src/backend/x11/compositor/init.rs",
        """                magnifier_zoom: gl.get_uniform_location(postprocess_program, "u_magnifier_zoom"),
                colorblind_mode: gl.get_uniform_location(postprocess_program, "u_colorblind_mode"),""",
        """                magnifier_zoom: gl.get_uniform_location(postprocess_program, "u_magnifier_zoom"),
                slime_enabled: gl.get_uniform_location(postprocess_program, "u_slime_enabled"),
                slime_points: gl.get_uniform_location(postprocess_program, "u_slime_points[0]"),
                slime_bbox: gl.get_uniform_location(postprocess_program, "u_slime_bbox"),
                slime_screen_size: gl.get_uniform_location(postprocess_program, "u_slime_screen_size"),
                slime_scale: gl.get_uniform_location(postprocess_program, "u_slime_scale"),
                slime_strength: gl.get_uniform_location(postprocess_program, "u_slime_strength"),
                slime_opacity: gl.get_uniform_location(postprocess_program, "u_slime_opacity"),
                colorblind_mode: gl.get_uniform_location(postprocess_program, "u_colorblind_mode"),""",
        marker="u_slime_points[0]",
    )
    changed |= replace_once(
        "src/backend/x11/compositor/init.rs",
        """        let postprocess_fbo = if needs_postprocess {
            unsafe { Self::create_scene_fbo(&gl, screen_w, screen_h).ok() }
        } else {
            None
        };

        // Parse P4: blur_strength_by_hz configuration""",
        """        let postprocess_fbo = if needs_postprocess {
            unsafe { Self::create_scene_fbo(&gl, screen_w, screen_h).ok() }
        } else {
            None
        };

        // The pose data plane is optional: compositor startup must not fail if
        // the runtime directory is read-only or another experimental instance
        // already owns the socket.
        let slime_ipc = match SlimeIpc::bind_default() {
            Ok(ipc) => Some(ipc),
            Err(err) => {
                log::warn!("compositor: slime pose IPC disabled: {err}");
                None
            }
        };

        // Parse P4: blur_strength_by_hz configuration""",
        marker="let slime_ipc = match SlimeIpc::bind_default()",
    )
    changed |= replace_once(
        "src/backend/x11/compositor/init.rs",
        """            magnifier_zoom: behavior.magnifier_zoom,
            magnifier_uniforms,
            // Window tilt""",
        """            magnifier_zoom: behavior.magnifier_zoom,
            magnifier_uniforms,
            // Realtime slime hand refraction
            slime_ipc,
            slime_state: SlimeState::default(),
            // Window tilt""",
        marker="slime_state: SlimeState::default()",
    )

    changed |= replace_once(
        "src/backend/x11/compositor/postprocess.rs",
        """            || self.magnifier_enabled
            || self.colorblind_mode != 0""",
        """            || self.magnifier_enabled
            || self.slime_state.is_visible()
            || self.colorblind_mode != 0""",
        marker="|| self.slime_state.is_visible()",
    )

    changed |= replace_once(
        "src/backend/x11/compositor/config.rs",
        """        // Need render if magnifier is active (tracking mouse)
        if self.magnifier_enabled {""",
        """        // A receiver thread raises `has_pending` even while fullscreen
        // unredirect/direct-scanout has stopped regular XDamage rendering. Keep
        // rendering through the fade and one final cleanup frame.
        if self
            .slime_ipc
            .as_ref()
            .is_some_and(SlimeIpc::has_pending)
            || self.slime_state.render_active()
        {
            return true;
        }
        // Need render if magnifier is active (tracking mouse)
        if self.magnifier_enabled {""",
        marker=".is_some_and(SlimeIpc::has_pending)",
    )

    changed |= replace_once(
        "src/backend/x11/compositor/render.rs",
        """    ) -> bool {
        if !self.fullscreen_unredirect {
            return false;
        }
        // Only unredirect if the top (focused) window is fullscreen and opaque""",
        """    ) -> bool {
        // Realtime post-processing cannot run while the X server presents a
        // fullscreen client directly. Restore redirection as soon as a pose is
        // visible, including the first packet received during unredirect.
        if self.slime_state.is_visible() {
            if let Some(previous) = self.unredirected_window.take() {
                let _ = self.conn.redirect_window_manual(previous);
                let _ = self.conn.flush_x11();
                if let Some(wt) = self.windows.get_mut(&previous) {
                    wt.needs_pixmap_refresh = true;
                }
                self.needs_render = true;
                log::info!(
                    "compositor: re-redirected fullscreen window 0x{:x} for slime effect",
                    previous
                );
            }
            return false;
        }
        if !self.fullscreen_unredirect {
            return false;
        }
        // Only unredirect if the top (focused) window is fullscreen and opaque""",
        marker="for slime effect",
    )
    changed |= replace_once(
        "src/backend/x11/compositor/render.rs",
        """        // Phase 2: Begin frame profiling
        self.frame_profiler.begin_frame();

        // P6A: Process deferred X11 operations""",
        """        // Phase 2: Begin frame profiling
        self.frame_profiler.begin_frame();

        // Drain the lossy pose channel before deciding whether fullscreen can
        // bypass the compositor. Only the newest inference result is retained.
        let slime_updated = self.poll_slime_ipc();
        let slime_active = self.slime_state.is_visible();

        // P6A: Process deferred X11 operations""",
        marker="let slime_updated = self.poll_slime_ipc();",
    )
    changed |= replace_once(
        "src/backend/x11/compositor/render.rs",
        "has_blur: wt.is_frosted,",
        "has_blur: wt.is_frosted || slime_active,",
        marker="has_blur: wt.is_frosted || slime_active,",
    )
    changed |= replace_once(
        "src/backend/x11/compositor/render.rs",
        """            || wallpaper_just_loaded
            || wobbly_active
            || explicit_render;""",
        """            || wallpaper_just_loaded
            || wobbly_active
            || slime_updated
            || slime_active
            || explicit_render;""",
        marker="|| slime_updated",
    )
    changed |= replace_once(
        "src/backend/x11/compositor/render.rs",
        """                }

                // Colorblind correction uniform
                self.gl.uniform_1_i32(""",
        """                }

                // Slime hand refraction uniforms
                let slime_opacity = self.slime_state.opacity();
                let slime_enabled = slime_opacity > 0.0;
                self.gl.uniform_1_i32(
                    self.magnifier_uniforms.slime_enabled.as_ref(),
                    if slime_enabled { 1 } else { 0 },
                );
                self.gl.uniform_1_f32(
                    self.magnifier_uniforms.slime_opacity.as_ref(),
                    slime_opacity,
                );
                if slime_enabled {
                    self.gl.uniform_2_f32_slice(
                        self.magnifier_uniforms.slime_points.as_ref(),
                        self.slime_state.points(),
                    );
                    let [min_x, min_y, max_x, max_y] = self.slime_state.bbox();
                    self.gl.uniform_4_f32(
                        self.magnifier_uniforms.slime_bbox.as_ref(),
                        min_x,
                        min_y,
                        max_x,
                        max_y,
                    );
                    self.gl.uniform_2_f32(
                        self.magnifier_uniforms.slime_screen_size.as_ref(),
                        self.screen_w as f32,
                        self.screen_h as f32,
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.slime_scale.as_ref(),
                        self.slime_state.scale(),
                    );
                    self.gl.uniform_1_f32(
                        self.magnifier_uniforms.slime_strength.as_ref(),
                        self.slime_state.strength(),
                    );
                }

                // Colorblind correction uniform
                self.gl.uniform_1_i32(""",
        marker="// Slime hand refraction uniforms",
    )

    changed |= patch_shader()

    print("slime compositor integration complete" if changed else "slime integration already present")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:  # keep CI error concise and actionable
        print(f"slime integration failed: {exc}", file=sys.stderr)
        raise
