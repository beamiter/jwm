pub const VERTEX_SHADER: &str = r#"#version 300 es

uniform vec4 u_rect;       // x, y, w, h in pixels
uniform mat4 u_projection; // orthographic projection
layout(location = 0) in vec2 a_position;

out vec2 v_uv;

void main() {
    v_uv = a_position; // GLX textures are Y-inverted (top-left origin matches screen coords)
    vec2 pixel = u_rect.xy + a_position * u_rect.zw;
    gl_Position = u_projection * vec4(pixel, 0.0, 1.0);
}
"#;

pub const FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform float u_opacity; // 1.0 for RGB windows (force opaque), negative to use texture alpha
uniform float u_radius;  // corner radius in pixels (0 = sharp)
uniform vec2  u_size;    // window size in pixels (w, h)
uniform float u_dim;     // dim multiplier (1.0 = no dim, <1.0 = darken)
uniform float u_desat;   // desaturation toward luminance (0 = off, 1 = grayscale)
uniform vec4  u_uv_rect; // x, y, w, h in UV space
uniform float u_ripple_progress;  // 0.0 = start, 1.0 = done, <0 = inactive
uniform float u_ripple_amplitude; // UV distortion strength (0 = no ripple)

// wp-color-management transform. Discriminant values must match
// TransferKind::shader_id in src/backend/wayland_udev/color_pipeline.rs.
const int TF_LINEAR = 0;
const int TF_POWER = 1;
const int TF_BT1886 = 2;
const int TF_GAMMA22 = 3;
const int TF_PQ = 4;
const int TF_HLG = 5;
const int TF_SRGB = 6;
uniform int   u_color_managed;     // 0 = bypass (no transform), 1 = apply
uniform mat3  u_color_matrix;      // linear surface→output RGB (uploaded column-major)
uniform int   u_decode_tf;
uniform float u_decode_gamma;      // used only when u_decode_tf == TF_POWER
uniform int   u_encode_tf;
uniform float u_encode_gamma;
// SOTA #2 Phase 2.2: scene-linear output. When 1, the fragment writes
// linear values (skipping the final encode for CM surfaces; applying an
// sRGB→linear decode for legacy non-CM surfaces so they end up linear
// in the FP16 FBO). When 0 (default), behavior is unchanged.
uniform int   u_scene_linear;
in vec2 v_uv;
out vec4 frag_color;

float rounded_rect_sdf(vec2 p, vec2 half_size, float r) {
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

// PQ inverse (SMPTE ST 2084): encoded → linear 0..1 representing 0..10000 cd/m².
vec3 pq_inverse(vec3 e) {
    const float M1 = 0.1593017578125;
    const float M2 = 78.84375;
    const float C1 = 0.8359375;
    const float C2 = 18.8515625;
    const float C3 = 18.6875;
    vec3 ep_m2 = pow(max(e, 0.0), vec3(1.0 / M2));
    vec3 num = max(ep_m2 - C1, 0.0);
    vec3 den = max(C2 - C3 * ep_m2, 1e-12);
    return pow(num / den, vec3(1.0 / M1));
}
vec3 pq_forward(vec3 l) {
    const float M1 = 0.1593017578125;
    const float M2 = 78.84375;
    const float C1 = 0.8359375;
    const float C2 = 18.8515625;
    const float C3 = 18.6875;
    vec3 lm = pow(max(l, 0.0), vec3(M1));
    return pow((C1 + C2 * lm) / (1.0 + C3 * lm), vec3(M2));
}

// HLG inverse (BT.2100): C = 0.5599107, A = 0.17883277, B = 0.28466892.
vec3 hlg_inverse(vec3 e) {
    const float A = 0.17883277;
    const float B = 0.28466892;
    const float C = 0.5599107;
    vec3 lo = (e * e) / 3.0;
    vec3 hi = (exp((e - C) / A) + B) / 12.0;
    return mix(lo, hi, step(0.5, e));
}
vec3 hlg_forward(vec3 l) {
    const float A = 0.17883277;
    const float B = 0.28466892;
    const float C = 0.5599107;
    vec3 lo = sqrt(max(l * 3.0, 0.0));
    vec3 hi = A * log(max(12.0 * l - B, 1e-12)) + C;
    return mix(lo, hi, step(1.0 / 12.0, l));
}

// IEC 61966-2-1 sRGB inverse EOTF (encoded → linear). Used for the
// legacy non-CM path under scene-linear compositing — clients without an
// image description are assumed sRGB.
vec3 srgb_inverse(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(lo, hi, step(0.04045, c));
}

vec3 srgb_forward(vec3 c) {
    c = max(c, 0.0);
    vec3 lo = c * 12.92;
    vec3 hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return mix(lo, hi, step(0.0031308, c));
}

vec3 decode_eotf(vec3 c, int kind, float gamma) {
    c = clamp(c, 0.0, 1.0);
    if (kind == TF_LINEAR)  return c;
    if (kind == TF_POWER)   return pow(c, vec3(max(gamma, 1e-3)));
    if (kind == TF_BT1886)  return pow(c, vec3(2.4));
    if (kind == TF_GAMMA22) return pow(c, vec3(2.2));
    if (kind == TF_PQ)      return pq_inverse(c);
    if (kind == TF_HLG)     return hlg_inverse(c);
    if (kind == TF_SRGB)    return srgb_inverse(c);
    return c;
}

vec3 encode_eotf(vec3 c, int kind, float gamma) {
    c = max(c, 0.0);
    if (kind == TF_LINEAR)  return clamp(c, 0.0, 1.0);
    if (kind == TF_POWER)   return clamp(pow(c, vec3(1.0 / max(gamma, 1e-3))), 0.0, 1.0);
    if (kind == TF_BT1886)  return clamp(pow(c, vec3(1.0 / 2.4)), 0.0, 1.0);
    if (kind == TF_GAMMA22) return clamp(pow(c, vec3(1.0 / 2.2)), 0.0, 1.0);
    if (kind == TF_PQ)      return clamp(pq_forward(c), 0.0, 1.0);
    if (kind == TF_HLG)     return clamp(hlg_forward(c), 0.0, 1.0);
    if (kind == TF_SRGB)    return clamp(srgb_forward(c), 0.0, 1.0);
    return clamp(c, 0.0, 1.0);
}

void main() {
    vec2 uv = u_uv_rect.xy + v_uv * u_uv_rect.zw;

    // Window-open ripple: radial UV distortion expanding from center
    if (u_ripple_amplitude > 0.0) {
        float t = clamp(u_ripple_progress, 0.0, 1.0);
        vec2 local = v_uv - vec2(0.5);
        vec2 pixel_delta = local * max(u_size, vec2(1.0));
        float extent = max(max(u_size.x, u_size.y), 1.0);
        float pixel_dist = length(pixel_delta);
        float dist = pixel_dist / extent;
        float wave_front = t * 0.72;
        float distance_to_wave = abs(dist - wave_front);
        float wave_envelope = 1.0 - smoothstep(0.0, 0.16, distance_to_wave);
        float time_envelope = sin(t * 3.14159265);
        float ring = sin((dist - wave_front) * 55.0)
                   * u_ripple_amplitude
                   * wave_envelope
                   * time_envelope;
        vec2 pixel_dir = pixel_dist > 0.001 ? pixel_delta / pixel_dist : vec2(0.0);
        vec2 uv_dir = pixel_dir * extent / max(u_size, vec2(1.0));
        uv += uv_dir * ring * u_uv_rect.zw;
        vec2 uv0 = u_uv_rect.xy;
        vec2 uv1 = uv0 + u_uv_rect.zw;
        uv = clamp(uv, min(uv0, uv1), max(uv0, uv1));
    }

    vec4 texel = texture(u_texture, uv);
    // A positive opacity marks an RGB/force-opaque region, so its texture
    // alpha is metadata rather than source coverage. A negative opacity marks
    // premultiplied RGBA content and its magnitude remains the layer fade.
    float source_alpha = u_opacity >= 0.0 ? 1.0 : clamp(texel.a, 0.0, 1.0);
    vec3 straight = source_alpha > 1e-6 ? texel.rgb / source_alpha : vec3(0.0);

    // wp-color-management transform: unpremultiply encoded source color,
    // decode to linear, apply the gamut matrix, optionally encode for an
    // encoded target, then premultiply again. Rounded-corner coverage, dim and
    // layer opacity remain later linear scalars over the converted source.
    if (u_color_managed == 1) {
        vec3 lin = decode_eotf(straight, u_decode_tf, u_decode_gamma);
        lin = u_color_matrix * lin;
        // Skip the final encode when writing to a linear-storage FBO so
        // GL blending mixes in linear space; the encode pass at the end
        // of the frame applies the output EOTF once over the composited
        // result. Phase 2.2.
        if (u_scene_linear == 1) {
            straight = lin;
        } else {
            straight = encode_eotf(lin, u_encode_tf, u_encode_gamma);
        }
    } else if (u_scene_linear == 1) {
        // Non-CM client under scene-linear: assume sRGB and linearize.
        straight = srgb_inverse(straight);
    }
    texel.rgb = straight * source_alpha;

    float layer_opacity = clamp(abs(u_opacity), 0.0, 1.0);
    float a = source_alpha * layer_opacity;
    texel.rgb *= layer_opacity;

    // Rounded corners – must mask both alpha AND rgb for premultiplied-alpha
    // blending (GL_ONE, GL_ONE_MINUS_SRC_ALPHA), otherwise rgb bleeds through
    // at corners where alpha is zero.
    if (u_radius > 0.0) {
        vec2 pixel_pos = v_uv * u_size;
        vec2 center = u_size * 0.5;
        float dist = rounded_rect_sdf(pixel_pos - center, center, u_radius);
        // Screen-space AA width: u_size is the logical window size, so when
        // the quad is drawn scaled (expose/overview thumbnails, PiP,
        // open/close animations) a fixed ±1 local-pixel band lands narrower
        // or wider than one screen pixel. fwidth keeps the band exactly one
        // screen pixel at any scale.
        float aa_w = max(fwidth(dist), 0.5);
        float aa = 1.0 - smoothstep(-aa_w, aa_w, dist);
        a *= aa;
        texel.rgb *= aa;
    }

    // Inactive desaturation toward luminance. A linear per-channel mix, so
    // it commutes with the premultiplied alpha already folded into rgb.
    if (u_desat > 0.0) {
        float luma = dot(texel.rgb, vec3(0.2126, 0.7152, 0.0722));
        texel.rgb = mix(texel.rgb, vec3(luma), clamp(u_desat, 0.0, 1.0));
    }

    // The compositor uses premultiplied-alpha blending. Layer opacity must
    // therefore scale RGB and alpha together for both opaque and RGBA clients.
    frag_color = vec4(texel.rgb * u_dim, a);
}
"#;

/// Shadow quad: draws a soft rectangular shadow using SDF + gaussian falloff.
pub const SHADOW_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform vec4  u_shadow_color;  // shadow RGBA
uniform vec2  u_size;          // window size in pixels
uniform float u_radius;        // corner radius (matches window)
uniform float u_spread;        // shadow blur spread in pixels
in vec2 v_uv;
out vec4 frag_color;

float rounded_rect_sdf(vec2 p, vec2 half_size, float r) {
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

void main() {
    // The shadow quad is expanded by u_spread on each side, so the UV
    // range [0,1] maps to the expanded rect. Convert to pixel coords.
    vec2 expanded = u_size + 2.0 * u_spread;
    vec2 pixel_pos = v_uv * expanded;
    vec2 center = expanded * 0.5;
    // SDF relative to the inner (window) rect
    float dist = rounded_rect_sdf(pixel_pos - center, u_size * 0.5, u_radius);
    // Gaussian penumbra: model the rect edge blurred with sigma = spread / 3,
    // approximating the normal CDF with a logistic curve. Coverage is ~1 well
    // inside the rect, 0.5 at the rect edge, and decays smoothly outward —
    // a diffuse pool instead of a dark outline hugging the window.
    float sigma = max(u_spread, 1.0) / 3.0;
    float alpha = 1.0 / (1.0 + exp(1.702 * dist / sigma));
    // The quad clips at dist == u_spread; force an exact zero before the edge
    // so the cutoff can never show as a seam.
    alpha *= 1.0 - smoothstep(0.85, 1.0, dist / max(u_spread, 1.0));
    float final_alpha = u_shadow_color.a * alpha;
    frag_color = vec4(u_shadow_color.rgb * final_alpha, final_alpha);
}
"#;

// ---------------------------------------------------------------------------
// Dual Kawase blur shaders
// ---------------------------------------------------------------------------

/// Kawase downsample shader: samples 4 diagonal neighbours + center with offsets.
pub const BLUR_DOWN_VERTEX: &str = r#"#version 300 es

uniform vec4 u_rect; // x, y, w, h in pixels (fullscreen quad for blur pass)
uniform mat4 u_projection;
layout(location = 0) in vec2 a_position;
out vec2 v_uv;

void main() {
    v_uv = a_position;
    vec2 pixel = u_rect.xy + a_position * u_rect.zw;
    gl_Position = u_projection * vec4(pixel, 0.0, 1.0);
}
"#;

pub const BLUR_DOWN_FRAGMENT: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform vec2 u_halfpixel; // 0.5 / texture_size
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec4 sum = texture(u_texture, v_uv) * 4.0;
    sum += texture(u_texture, v_uv - u_halfpixel);
    sum += texture(u_texture, v_uv + u_halfpixel);
    sum += texture(u_texture, v_uv + vec2(u_halfpixel.x, -u_halfpixel.y));
    sum += texture(u_texture, v_uv - vec2(u_halfpixel.x, -u_halfpixel.y));
    frag_color = sum / 8.0;
}
"#;

/// Kawase upsample shader: blends 8 neighbours to reconstruct blurred image.
pub const BLUR_UP_FRAGMENT: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform vec2 u_halfpixel;
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec4 sum = texture(u_texture, v_uv + vec2(-u_halfpixel.x * 2.0, 0.0));
    sum += texture(u_texture, v_uv + vec2(-u_halfpixel.x, u_halfpixel.y)) * 2.0;
    sum += texture(u_texture, v_uv + vec2(0.0, u_halfpixel.y * 2.0));
    sum += texture(u_texture, v_uv + vec2(u_halfpixel.x, u_halfpixel.y)) * 2.0;
    sum += texture(u_texture, v_uv + vec2(u_halfpixel.x * 2.0, 0.0));
    sum += texture(u_texture, v_uv + vec2(u_halfpixel.x, -u_halfpixel.y)) * 2.0;
    sum += texture(u_texture, v_uv + vec2(0.0, -u_halfpixel.y * 2.0));
    sum += texture(u_texture, v_uv + vec2(-u_halfpixel.x, -u_halfpixel.y)) * 2.0;
    frag_color = sum / 12.0;
}
"#;

// ---------------------------------------------------------------------------
// Box Blur shader (fast fallback for low-end hardware)
// ---------------------------------------------------------------------------

/// Box blur fragment shader: 3x3 uniform kernel, single pass
/// Much faster than Kawase but lower quality. Used when BlurQuality::Minimal.
pub const BOX_BLUR_FRAGMENT: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform vec2 u_halfpixel; // 0.5 / texture_size (reuse same uniform as Kawase)
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec4 sum = vec4(0.0);
    // 3x3 box kernel with equal weights
    for (int y = -1; y <= 1; y++) {
        for (int x = -1; x <= 1; x++) {
            sum += texture(u_texture, v_uv + vec2(x, y) * u_halfpixel);
        }
    }
    frag_color = sum / 9.0;
}
"#;

// ---------------------------------------------------------------------------
// Feature 1: Window border / outline shader
// ---------------------------------------------------------------------------

pub const BORDER_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform vec4  u_border_color;  // border/glow RGBA
uniform vec2  u_size;          // outline quad size, or inner window size in glow mode
uniform float u_radius;        // corner radius (0 = sharp)
uniform float u_radius_top;    // radius of the top two corners
uniform float u_border_width;  // >=0: border width, <0: directional glow radius
uniform int   u_scene_linear;
in vec2 v_uv;
out vec4 frag_color;

// The top two corners carry their own radius so a panel can sit flush against
// the bar it drops out of: square where the two shapes meet, rounded below.
// Inside the rect the chosen radius cancels out of the expression, so the two
// halves meet with no seam however far apart the radii are.
float rounded_rect_sdf(vec2 p, vec2 half_size, float r_bottom, float r_top) {
    float r = p.y < 0.0 ? r_top : r_bottom;
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

vec3 srgb_inverse(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(lo, hi, step(0.04045, c));
}

void main() {
    vec3 rgb = u_scene_linear == 1
        ? srgb_inverse(u_border_color.rgb)
        : u_border_color.rgb;

    if (u_border_width < 0.0) {
        float spread = max(-u_border_width, 0.001);
        vec2 expanded = u_size + vec2(2.0 * spread);
        vec2 pixel_pos = v_uv * expanded;
        vec2 center = expanded * 0.5;
        float dist = rounded_rect_sdf(pixel_pos - center, u_size * 0.5, u_radius, u_radius_top);
        float aa = max(fwidth(dist), 0.75);
        float outside = max(dist, 0.0);
        float normalized = outside / spread;
        float outside_mask = smoothstep(-aa, aa, dist);

        float halo = exp2(-4.0 * normalized * normalized) * outside_mask;
        halo *= 1.0 - smoothstep(0.72, 1.0, normalized);
        float core = 1.0 - smoothstep(0.0, aa * 1.75, abs(dist));
        core *= outside_mask;

        float directional = 0.42 + 0.36 * v_uv.x + 0.22 * (1.0 - v_uv.y);
        vec2 top_right_delta =
            (v_uv - vec2(0.82, 0.08)) / vec2(0.24, 0.18);
        vec2 lower_left_delta =
            (v_uv - vec2(0.08, 0.82)) / vec2(0.30, 0.26);
        float top_right = exp(-dot(top_right_delta, top_right_delta));
        float lower_left = exp(-dot(lower_left_delta, lower_left_delta));
        float energy = clamp(
            directional + 0.68 * top_right + 0.24 * lower_left,
            0.25,
            1.65
        );

        float glow_mask = max(halo, core * 0.85);
        float a = clamp(u_border_color.a * glow_mask * energy, 0.0, 1.0);
        frag_color = vec4(rgb * a, a);
        return;
    }

    vec2 pixel_pos = v_uv * u_size;
    vec2 center = u_size * 0.5;
    float dist = rounded_rect_sdf(pixel_pos - center, center, u_radius, u_radius_top);
    // The border is visible between -u_border_width and 0
    float outer = 1.0 - smoothstep(-1.0, 1.0, dist);
    float inner = 1.0 - smoothstep(-1.0, 1.0, dist + u_border_width);
    float border_mask = outer - inner;
    float a = u_border_color.a * border_mask;
    frag_color = vec4(rgb * a, a);  // premultiplied alpha
}
"#;

/// Two-color linear-gradient border ring for the focused window.
///
/// Same SDF ring mask as `BORDER_FRAGMENT_SHADER`'s positive-width branch,
/// but the color interpolates between `u_color_a` and `u_color_b` along
/// `u_gradient_angle` (radians; 0 = left→right, π/2 = top→bottom). Kept as a
/// dedicated program so the many other users of the plain border shader never
/// inherit stale gradient state.
pub const GRADIENT_BORDER_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform vec4  u_color_a;        // gradient start RGBA
uniform vec4  u_color_b;        // gradient end RGBA
uniform float u_gradient_angle; // radians
uniform vec2  u_size;           // outline quad size
uniform float u_radius;         // outer corner radius (0 = sharp)
uniform float u_radius_top;     // radius of the top two corners
uniform float u_border_width;   // ring thickness in pixels
uniform int   u_scene_linear;
in vec2 v_uv;
out vec4 frag_color;

// Split like the plain border program's, so a ring around a docked panel
// follows the card's square top corners instead of curving away from them.
float rounded_rect_sdf(vec2 p, vec2 half_size, float r_bottom, float r_top) {
    float r = p.y < 0.0 ? r_top : r_bottom;
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

vec3 srgb_inverse(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(lo, hi, step(0.04045, c));
}

void main() {
    vec2 pixel_pos = v_uv * u_size;
    vec2 center = u_size * 0.5;
    float dist = rounded_rect_sdf(pixel_pos - center, center, u_radius, u_radius_top);
    float outer = 1.0 - smoothstep(-1.0, 1.0, dist);
    float inner = 1.0 - smoothstep(-1.0, 1.0, dist + u_border_width);
    float border_mask = outer - inner;

    // Project onto the gradient direction; the |dx|+|dy| norm maps the quad's
    // extreme corners to exactly t = 0 and t = 1 for every angle.
    vec2 dir = vec2(cos(u_gradient_angle), sin(u_gradient_angle));
    float t = 0.5 + dot(v_uv - vec2(0.5), dir) / (abs(dir.x) + abs(dir.y));
    vec4 col = mix(u_color_a, u_color_b, clamp(t, 0.0, 1.0));
    vec3 rgb = u_scene_linear == 1 ? srgb_inverse(col.rgb) : col.rgb;

    float a = col.a * border_mask;
    frag_color = vec4(rgb * a, a); // premultiplied alpha
}
"#;

/// Frosted-glass surface (mirrors the X11 backend's copy) for JWM's own panels
/// under the glass themes (`appearance.ui_theme = "glass"` / `"glass-dark"` /
/// `"aurora"`).
///
/// The quad samples `u_backdrop` — a Kawase-blurred copy of the frame captured
/// just before the overlays are drawn — in screen space, so the sheet shows the
/// desktop behind it rather than an opaque fill. Everything else here exists to
/// make that read as a *thick pane of glass* rather than a translucent
/// rectangle, which is the whole difference between Apple's material and a
/// plain backdrop blur:
///
/// * **Continuous corners.** The mask is a superellipse, not a circular
///   rounded rect, so curvature eases into the straight edges.
/// * **Edge refraction.** A beveled band drags the backdrop outward along the
///   surface normal, squeezing what lies beyond the panel into its rim.
/// * **Rim hairline + inner glow.** The bevel glows softly and terminates in a
///   specular line that runs the whole perimeter, brightest on the two edges
///   aligned with the light.
/// * **Chroma lift and sheen.** A blur averages color toward gray, so
///   saturation is pushed back up, and a broad diagonal sheen lights the face.
///
/// A little hash grain keeps the wide, smooth gradients from banding on 8-bit
/// outputs. Output is premultiplied, matching every other overlay program.
pub const GLASS_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_backdrop;      // blurred scene, full screen, GL-oriented
uniform vec2  u_screen_size;       // framebuffer size in pixels
uniform vec4  u_tint;              // veil over the backdrop: rgb + coverage
uniform vec2  u_size;              // sheet size in pixels
uniform float u_radius;            // corner radius in pixels
uniform float u_radius_top;        // radius of the top two corners
uniform float u_corner_exp;        // 2 = circular, ~4 = continuous (squircle)
uniform float u_saturation;        // chroma multiplier on the backdrop
uniform float u_luminance;         // brightness multiplier on the backdrop
uniform float u_bevel_width;       // beveled band inside the edge, pixels
uniform float u_refraction;        // how far the bevel drags the backdrop, px
uniform float u_rim_width;         // specular hairline width, pixels
uniform float u_rim_intensity;     // hairline strength
uniform vec3  u_rim_tint;          // hairline color
uniform float u_sheen;             // broad diagonal sheen across the face
uniform float u_edge_shade;        // bottom contact-shade strength
uniform float u_grain;             // dither amplitude
uniform float u_alpha;             // fade envelope (toasts/OSD)
uniform int   u_scene_linear;
in vec2 v_uv;
out vec4 frag_color;

vec3 srgb_inverse(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(lo, hi, step(0.04045, c));
}

// Superellipse |x|^n + |y|^n = r^n in the corner quadrant. n == 2 reduces to
// the circular rounded rect every other program uses; higher n bulges the
// corner outward into Apple's continuous curvature, where the arc eases into
// the straight edge instead of meeting it at a visible tangent point.
float squircle_sdf(vec2 p, vec2 half_size, float r_bottom, float r_top, float n) {
    float r = p.y < 0.0 ? r_top : r_bottom;
    vec2 d = abs(p) - half_size + vec2(r);
    vec2 m = max(d, 0.0);
    float corner = pow(pow(m.x, n) + pow(m.y, n), 1.0 / n);
    return corner + min(max(d.x, d.y), 0.0) - r;
}

void main() {
    vec2 half_size = u_size * 0.5;
    float dist = squircle_sdf(v_uv * u_size - half_size, half_size, u_radius, u_radius_top,
                              max(u_corner_exp, 2.0));

    // Derivatives must be taken before any discard: once part of a quad is
    // killed, the neighbours' values are undefined.
    vec2 gradient = vec2(dFdx(dist), dFdy(dist));
    float gradient_len = length(gradient);
    // Outward surface normal in gl_FragCoord space (y up).
    vec2 normal = gradient_len > 1e-4 ? gradient / gradient_len : vec2(0.0);
    // fwidth keeps the antialiased band one screen pixel wide whatever the
    // sheet's size, exactly like the window and border programs.
    float aa_w = max(fwidth(dist), 0.5);

    float mask = 1.0 - smoothstep(-aa_w, aa_w, dist);
    if (mask <= 0.0) {
        discard;
    }

    // Bevel: 0 deep inside the sheet, 1 at the very edge. Squaring it keeps
    // the lensing concentrated in the last few pixels, the way the curvature
    // of a real chamfer does.
    float bevel = smoothstep(-max(u_bevel_width, 1.0), 0.0, dist);
    float lens = bevel * bevel;

    // Refraction: walk the sample point outward along the normal so content
    // from beyond the edge is squeezed into the bevel. This is what stops the
    // panel reading as a decal pasted onto the desktop.
    vec2 sample_px = gl_FragCoord.xy + normal * lens * u_refraction;
    // The Kawase chain hands its result back vertically mirrored: every
    // down/up pass renders through a Y-flipping ortho, and the chain always
    // runs an odd number of them (N down + N-1 up). Flip Y back here so the
    // sheet samples the pixels it actually covers.
    vec2 backdrop_uv = vec2(sample_px.x, u_screen_size.y - sample_px.y)
                     / max(u_screen_size, vec2(1.0));
    vec3 backdrop = texture(u_backdrop, clamp(backdrop_uv, 0.0, 1.0)).rgb;
    float luma = dot(backdrop, vec3(0.2126, 0.7152, 0.0722));
    backdrop = clamp(mix(vec3(luma), backdrop, u_saturation) * u_luminance, 0.0, 1.0);

    vec3 color = mix(backdrop, (u_scene_linear == 1 ? srgb_inverse(u_tint.rgb) : u_tint.rgb), clamp(u_tint.a, 0.0, 1.0));

    // Broad sheen: the face is brightest toward the top-left, as if lit from
    // over the user's shoulder.
    color += vec3(u_sheen * (1.0 - clamp((v_uv.x + v_uv.y) * 0.5, 0.0, 1.0)));

    // The bevel is thicker glass, so it carries a soft inner glow.
    color += u_rim_tint * (lens * u_rim_intensity * 0.30);

    // Rim hairline around the whole perimeter. Taking |dot| with the light
    // direction lights both the edge facing the light and the one facing away
    // from it — the two opposed highlights that read unmistakably as a pane.
    float rim = smoothstep(-max(u_rim_width, 0.5), 0.0, dist);
    rim *= rim;
    float facing = abs(dot(normal, normalize(vec2(-0.55, 0.83))));
    color += u_rim_tint * (rim * u_rim_intensity * (0.45 + 0.55 * facing));

    // Contact shade along the bottom edge keeps the sheet seated.
    color -= vec3(u_edge_shade * bevel * v_uv.y);

    // Cheap hash dither; ±half a step is enough to break up banding.
    float noise = fract(sin(dot(gl_FragCoord.xy, vec2(12.9898, 78.233))) * 43758.5453);
    color += vec3((noise - 0.5) * u_grain);

    float a = clamp(u_alpha, 0.0, 1.0) * mask;
    frag_color = vec4(clamp(color, 0.0, 1.0) * a, a);
}
"#;

// ---------------------------------------------------------------------------
// SOTA #2 Phase 2.2: scene-linear encode pass
// ---------------------------------------------------------------------------
//
// Fullscreen quad that reads the FP16 linear-scene FBO and applies the
// output's forward EOTF, writing display-encoded values to output_fbo.
// Phase 2.3 binds and dispatches this pass at the end of the window draw.
//
// Uses BLUR_DOWN_VERTEX for the vertex stage (same gl_VertexID-based
// fullscreen quad). The TF discriminant values match TransferKind in
// color_pipeline.rs and the constants at the top of FRAGMENT_SHADER.

pub const SCENE_LINEAR_ENCODE_FRAGMENT: &str = r#"#version 300 es
precision highp float;

const int TF_LINEAR = 0;
const int TF_POWER = 1;
const int TF_BT1886 = 2;
const int TF_GAMMA22 = 3;
const int TF_PQ = 4;
const int TF_HLG = 5;
const int TF_SRGB = 6;

uniform sampler2D u_texture;
uniform int   u_encode_tf;
uniform float u_encode_gamma;
in vec2 v_uv;
out vec4 frag_color;

// IEC 61966-2-1 sRGB forward EOTF (linear → encoded).
vec3 srgb_forward(vec3 c) {
    c = max(c, 0.0);
    vec3 lo = c * 12.92;
    vec3 hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return mix(lo, hi, step(0.0031308, c));
}

vec3 pq_forward(vec3 l) {
    const float M1 = 0.1593017578125;
    const float M2 = 78.84375;
    const float C1 = 0.8359375;
    const float C2 = 18.8515625;
    const float C3 = 18.6875;
    vec3 lm = pow(max(l, 0.0), vec3(M1));
    return pow((C1 + C2 * lm) / (1.0 + C3 * lm), vec3(M2));
}

vec3 hlg_forward(vec3 l) {
    const float A = 0.17883277;
    const float B = 0.28466892;
    const float C = 0.5599107;
    vec3 lo = sqrt(max(l * 3.0, 0.0));
    vec3 hi = A * log(max(12.0 * l - B, 1e-12)) + C;
    return mix(lo, hi, step(1.0 / 12.0, l));
}

void main() {
    vec4 texel = texture(u_texture, v_uv);
    vec3 c = max(texel.rgb, 0.0);
    if (u_encode_tf == TF_LINEAR)       c = clamp(c, 0.0, 1.0);
    else if (u_encode_tf == TF_POWER)   c = clamp(pow(c, vec3(1.0 / max(u_encode_gamma, 1e-3))), 0.0, 1.0);
    else if (u_encode_tf == TF_BT1886)  c = clamp(pow(c, vec3(1.0 / 2.4)), 0.0, 1.0);
    else if (u_encode_tf == TF_GAMMA22) c = clamp(pow(c, vec3(1.0 / 2.2)), 0.0, 1.0);
    else if (u_encode_tf == TF_PQ)      c = clamp(pq_forward(c), 0.0, 1.0);
    else if (u_encode_tf == TF_HLG)     c = clamp(hlg_forward(c), 0.0, 1.0);
    // TF_SRGB and the -1 default both encode sRGB; fall through to the else.
    else                                 c = clamp(srgb_forward(c), 0.0, 1.0);
    frag_color = vec4(c, texel.a);
}
"#;

// Companion fullscreen-quad decode pass: reads the encoded output_fbo
// (containing wallpaper + shadows + anything else not yet linear-aware)
// and writes its linearization into the FP16 linear_fbo, so the window
// draws blend correctly over it. Defaults to sRGB since legacy passes
// produce sRGB-encoded values.

pub const SCENE_LINEAR_DECODE_FRAGMENT: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
in vec2 v_uv;
out vec4 frag_color;

vec3 srgb_inverse(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(lo, hi, step(0.04045, c));
}

void main() {
    vec4 texel = texture(u_texture, v_uv);
    frag_color = vec4(srgb_inverse(clamp(texel.rgb, 0.0, 1.0)), texel.a);
}
"#;

// ---------------------------------------------------------------------------
// Feature 9 & 10: Post-processing shader (color temperature, invert, filters)
// ---------------------------------------------------------------------------

pub const POSTPROCESS_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform float u_color_temp;    // color temperature shift: 0.0=neutral, <0=cool, >0=warm (range ~ -1..1)
uniform float u_saturation;    // saturation multiplier: 1.0=normal, 0.0=grayscale, >1.0=vivid
uniform float u_brightness;    // brightness multiplier: 1.0=normal
uniform float u_contrast;      // contrast multiplier: 1.0=normal
uniform int   u_invert;        // 1 = invert colors, 0 = normal
uniform int   u_grayscale;     // 1 = force grayscale (accessibility), 0 = normal
in vec2 v_uv;
out vec4 frag_color;

void main() {
    // FBO textures have Y=0 at bottom, flip to match top-left-origin scene
    vec2 uv = vec2(v_uv.x, 1.0 - v_uv.y);
    vec4 c = texture(u_texture, uv);

    // Invert
    if (u_invert == 1) {
        c.rgb = 1.0 - c.rgb;
    }

    // Grayscale
    if (u_grayscale == 1) {
        float lum = dot(c.rgb, vec3(0.2126, 0.7152, 0.0722));
        c.rgb = vec3(lum);
    }

    // Saturation
    float lum = dot(c.rgb, vec3(0.2126, 0.7152, 0.0722));
    c.rgb = mix(vec3(lum), c.rgb, u_saturation);

    // Brightness
    c.rgb *= u_brightness;

    // Contrast
    c.rgb = (c.rgb - 0.5) * u_contrast + 0.5;

    // Color temperature (shift red/blue)
    if (u_color_temp != 0.0) {
        float t = u_color_temp;
        c.r += t * 0.1;
        c.b -= t * 0.1;
        c.rgb = clamp(c.rgb, 0.0, 1.0);
    }

    frag_color = c;
}
"#;

// ---------------------------------------------------------------------------
// Feature 11: Debug HUD shader (text rendering via simple bitmap digits)
// ---------------------------------------------------------------------------

pub const HUD_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform vec4  u_bg_color; // background color for HUD panel
uniform vec2  u_size;     // panel size in pixels
in vec2 v_uv;
out vec4 frag_color;

void main() {
    // Simple semi-transparent background panel
    float alpha = u_bg_color.a;
    // Slight rounded corners for the panel
    vec2 pixel_pos = v_uv * u_size;
    vec2 center = u_size * 0.5;
    vec2 d = abs(pixel_pos - center) - center + vec2(4.0);
    float dist = length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - 4.0;
    float mask = 1.0 - smoothstep(-1.0, 1.0, dist);
    float final_alpha = alpha * mask;
    frag_color = vec4(u_bg_color.rgb * final_alpha, final_alpha);
}
"#;

// ---------------------------------------------------------------------------
// Feature 11b: HUD text overlay (pre-rasterized bitmap font texture)
// ---------------------------------------------------------------------------

pub const HUD_TEXT_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform float u_opacity; // layer opacity for fading text (set 1.0 for static)
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec4 texel = texture(u_texture, v_uv);
    float a = texel.a * clamp(u_opacity, 0.0, 1.0);
    // Output premultiplied alpha for GL_ONE, GL_ONE_MINUS_SRC_ALPHA blending
    frag_color = vec4(texel.rgb * a, a);
}
"#;

// ---------------------------------------------------------------------------
// Tag-switch transition shader
// ---------------------------------------------------------------------------

/// Draws a snapshot texture for workspace transitions. The sampled source area
/// can be cropped so persistent UI such as the status bar is excluded.
pub const CUBE_VERTEX_SHADER: &str = r#"#version 300 es

uniform mat4 u_mvp;
uniform mat4 u_model;
uniform float u_aspect; // screen_w / workspace_h
layout(location = 0) in vec2 a_position;
out vec2 v_uv;
out vec3 v_world;
out vec3 v_normal;

void main() {
    v_uv = a_position;
    // Face quad spans [-aspect, -1] to [+aspect, +1] in model space
    vec3 vert = vec3((a_position.x * 2.0 - 1.0) * u_aspect, a_position.y * 2.0 - 1.0, 0.0);
    v_world = (u_model * vec4(vert, 1.0)).xyz;
    v_normal = normalize(mat3(u_model) * vec3(0.0, 0.0, 1.0));
    gl_Position = u_mvp * vec4(vert, 1.0);
}
"#;

pub const CUBE_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform float u_brightness; // face lighting (1.0 = fully lit)
uniform vec4 u_uv_rect;     // x, y, w, h in texture UV space
uniform float u_aspect;     // face half-width; half-height is 1.0
uniform vec3 u_camera;      // camera position in world space
uniform vec4 u_accent;      // rgb accent; a = selection strength
uniform float u_alpha;      // lit-path opacity
uniform float u_desat;      // lit-path desaturation, 0..1
uniform float u_edge;       // lit-path rounded bevel strength, 0..1
uniform float u_lit;        // 0 = byte-for-byte legacy shading, 1 = lit face
uniform int u_scene_linear; // lit path writes into a linear output FBO
uniform int u_has_alpha;    // source alpha is meaningful (not an opaque region)
uniform int u_filler;       // empty/missing slot uses the built-in tinted material
uniform int u_reflection;   // mirrored overview pass below the floor plane
uniform float u_floor_y;    // world-space contact plane for reflection falloff
// Per-window wp-color-management contract. Discriminants match
// TransferKind::shader_id and the ordinary window program.
const int TF_LINEAR = 0;
const int TF_POWER = 1;
const int TF_BT1886 = 2;
const int TF_GAMMA22 = 3;
const int TF_PQ = 4;
const int TF_HLG = 5;
const int TF_SRGB = 6;
uniform int u_color_managed;
uniform mat3 u_color_matrix;
uniform int u_decode_tf;
uniform float u_decode_gamma;
uniform int u_encode_tf;
uniform float u_encode_gamma;
in vec2 v_uv;
in vec3 v_world;
in vec3 v_normal;
out vec4 frag_color;

const float CORNER_RADIUS = 0.06;

float rounded_box(vec2 p, vec2 half_size, float radius) {
    vec2 d = abs(p) - half_size + vec2(radius);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - radius;
}

vec3 srgb_inverse(vec3 c) {
    c = clamp(c, 0.0, 1.0);
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(lo, hi, step(0.04045, c));
}

vec3 srgb_forward(vec3 c) {
    c = max(c, 0.0);
    vec3 lo = c * 12.92;
    vec3 hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
    return mix(lo, hi, step(0.0031308, c));
}

vec3 pq_inverse(vec3 e) {
    const float M1 = 0.1593017578125;
    const float M2 = 78.84375;
    const float C1 = 0.8359375;
    const float C2 = 18.8515625;
    const float C3 = 18.6875;
    vec3 ep_m2 = pow(max(e, 0.0), vec3(1.0 / M2));
    vec3 num = max(ep_m2 - C1, 0.0);
    vec3 den = max(C2 - C3 * ep_m2, 1e-12);
    return pow(num / den, vec3(1.0 / M1));
}

vec3 pq_forward(vec3 l) {
    const float M1 = 0.1593017578125;
    const float M2 = 78.84375;
    const float C1 = 0.8359375;
    const float C2 = 18.8515625;
    const float C3 = 18.6875;
    vec3 lm = pow(max(l, 0.0), vec3(M1));
    return pow((C1 + C2 * lm) / (1.0 + C3 * lm), vec3(M2));
}

vec3 hlg_inverse(vec3 e) {
    const float A = 0.17883277;
    const float B = 0.28466892;
    const float C = 0.5599107;
    vec3 lo = (e * e) / 3.0;
    vec3 hi = (exp((e - C) / A) + B) / 12.0;
    return mix(lo, hi, step(0.5, e));
}

vec3 hlg_forward(vec3 l) {
    const float A = 0.17883277;
    const float B = 0.28466892;
    const float C = 0.5599107;
    vec3 lo = sqrt(max(l * 3.0, 0.0));
    vec3 hi = A * log(max(12.0 * l - B, 1e-12)) + C;
    return mix(lo, hi, step(1.0 / 12.0, l));
}

vec3 decode_eotf(vec3 c, int kind, float gamma) {
    c = clamp(c, 0.0, 1.0);
    if (kind == TF_LINEAR)  return c;
    if (kind == TF_POWER)   return pow(c, vec3(max(gamma, 1e-3)));
    if (kind == TF_BT1886)  return pow(c, vec3(2.4));
    if (kind == TF_GAMMA22) return pow(c, vec3(2.2));
    if (kind == TF_PQ)      return pq_inverse(c);
    if (kind == TF_HLG)     return hlg_inverse(c);
    if (kind == TF_SRGB)    return srgb_inverse(c);
    return c;
}

vec3 encode_eotf(vec3 c, int kind, float gamma) {
    c = max(c, 0.0);
    if (kind == TF_LINEAR)  return clamp(c, 0.0, 1.0);
    if (kind == TF_POWER)   return clamp(pow(c, vec3(1.0 / max(gamma, 1e-3))), 0.0, 1.0);
    if (kind == TF_BT1886)  return clamp(pow(c, vec3(1.0 / 2.4)), 0.0, 1.0);
    if (kind == TF_GAMMA22) return clamp(pow(c, vec3(1.0 / 2.2)), 0.0, 1.0);
    if (kind == TF_PQ)      return clamp(pq_forward(c), 0.0, 1.0);
    if (kind == TF_HLG)     return clamp(hlg_forward(c), 0.0, 1.0);
    if (kind == TF_SRGB)    return clamp(srgb_forward(c), 0.0, 1.0);
    return clamp(c, 0.0, 1.0);
}

vec3 output_domain_color(vec3 encoded) {
    return u_scene_linear != 0 ? srgb_inverse(encoded) : encoded;
}

void main() {
    vec2 uv = u_uv_rect.xy + v_uv * u_uv_rect.zw;
    // A filler owns no texture. Keep the legacy transition on the real sample
    // path while making lit empty slots independent of inherited GL bindings.
    vec4 texel = (u_lit >= 0.5 && u_filler != 0)
        ? vec4(0.0)
        : texture(u_texture, uv);

    // Workspace transitions predate the lit prism face and intentionally keep
    // their exact brightness-only output. Keep this branch first so none of the
    // new material controls can alter a legacy draw.
    if (u_lit < 0.5) {
        frag_color = vec4(texel.rgb * u_brightness, texel.a * u_brightness);
        return;
    }

    // Window textures are premultiplied. Convert through straight color, then
    // premultiply again before compositing the dark backing. This preserves
    // the overview material's alpha contract while consuming the same EOTF
    // and gamut transform as the ordinary window draw.
    float raw_alpha = clamp(texel.a, 0.0, 1.0);
    float source_alpha = u_has_alpha != 0 ? raw_alpha : 1.0;
    vec3 straight = u_has_alpha != 0
        ? (raw_alpha > 1.0e-6 ? texel.rgb / raw_alpha : vec3(0.0))
        : texel.rgb;
    if (u_filler == 0 && u_color_managed != 0) {
        vec3 linear = decode_eotf(straight, u_decode_tf, u_decode_gamma);
        linear = u_color_matrix * linear;
        straight = u_scene_linear != 0
            ? linear
            : encode_eotf(linear, u_encode_tf, u_encode_gamma);
    } else if (u_scene_linear != 0) {
        // A surface without an image description follows the compositor's
        // ordinary legacy-sRGB assumption in scene-linear mode.
        straight = srgb_inverse(straight);
    }
    vec3 source_rgb = straight * source_alpha;
    vec3 accent = output_domain_color(u_accent.rgb);
    vec3 backing = output_domain_color(vec3(0.055, 0.065, 0.085));
    vec3 filler_encoded = mix(vec3(0.10, 0.13, 0.19),
                              vec3(0.04, 0.05, 0.08), v_uv.y)
                        + u_accent.rgb * 0.08;
    vec3 base = u_filler != 0
        ? output_domain_color(clamp(filler_encoded, 0.0, 1.0))
        : source_rgb + backing * (1.0 - source_alpha);

    vec3 normal = normalize(v_normal);
    vec3 view = normalize(u_camera - v_world);
    normal *= sign(dot(normal, view) + 1e-4);
    vec3 light = normalize(vec3(-0.35, 0.85, 0.55));
    float diffuse = 0.78 + 0.22 * clamp(dot(normal, light), 0.0, 1.0);
    vec3 half_vec = normalize(light + view);
    float specular = pow(clamp(dot(normal, half_vec), 0.0, 1.0), 40.0) * 0.55;
    float fresnel = pow(1.0 - clamp(dot(normal, view), 0.0, 1.0), 3.5);

    vec3 color = base * u_brightness * diffuse;
    float desat = clamp(u_desat, 0.0, 1.0);
    float luma = dot(color, vec3(0.2126, 0.7152, 0.0722));
    color = mix(color, vec3(luma), desat);
    color += vec3(1.0) * specular * (1.0 - desat * 0.5);
    color += mix(accent, vec3(1.0), 0.4) * fresnel * 0.17;

    float edge = clamp(u_edge, 0.0, 1.0);
    vec2 half_size = vec2(u_aspect, 1.0);
    vec2 local = (v_uv * 2.0 - 1.0) * half_size;
    float dist = rounded_box(local, half_size, CORNER_RADIUS * edge);
    float aa = max(fwidth(dist), 1.0e-4);
    float mask = 1.0 - smoothstep(-aa, aa, dist);
    float bevel = 1.0 - smoothstep(0.0, 0.014, -dist);
    float halo = 1.0 - smoothstep(0.0, 0.10, -dist);
    color += (accent * 0.45 + output_domain_color(vec3(0.16)))
           * bevel * (0.35 + 0.65 * u_accent.a) * edge;
    color += accent * halo * u_accent.a * 0.20 * edge;

    float alpha = clamp(u_alpha, 0.0, 1.0) * mask;
    if (u_reflection != 0) {
        // The mirrored face touches the floor at v_uv.y == 0. Fade both in
        // normalized face space and in world space so scaling the entry/exit
        // animation cannot leave a hard reflected rectangle at the monitor
        // edge. This is deliberately static: a settled overview needs no
        // animation-only redraws.
        float distance_below_floor = max(u_floor_y - v_world.y, 0.0);
        float contact = pow(clamp(1.0 - v_uv.y, 0.0, 1.0), 1.45)
                      * exp(-distance_below_floor * 0.85);
        alpha *= contact * 0.48;
        color = mix(color, output_domain_color(vec3(0.05, 0.06, 0.09)), 0.24) * 0.90;
    }
    // This UI material is reflective, not emissive. Bound straight color
    // before premultiplication so highlights cannot leak through a fade.
    frag_color = vec4(clamp(color, 0.0, 1.0) * alpha, alpha);
}
"#;

// ---------------------------------------------------------------------------
// Overview prism caps
// ---------------------------------------------------------------------------

/// Top/bottom polygon of the Wayland overview prism. The fan is generated from
/// `gl_VertexID`, so it can share the compositor's already-bound VAO without a
/// cap-specific buffer. Rim vertices are offset by half a side step, exactly
/// matching the face seams produced by `compositor_common::prism`.
pub const OVERVIEW_CAP_VERTEX_SHADER: &str = r#"#version 300 es

uniform mat4 u_mvp;
uniform mat4 u_model;
uniform float u_radius;
uniform float u_y;
uniform float u_sides;

out vec2 v_local;
out float v_edge;
out vec3 v_world;
out vec3 v_normal;

void main() {
    float sides = max(u_sides, 3.0);
    vec2 offset = vec2(0.0);
    v_edge = 0.0;
    if (gl_VertexID > 0) {
        float step_angle = 6.28318530718 / sides;
        float angle = (float(gl_VertexID - 1) + 0.5) * step_angle;
        offset = vec2(sin(angle), cos(angle));
        v_edge = 1.0;
    }

    vec3 vert = vec3(offset.x * u_radius, u_y, offset.y * u_radius);
    vec3 local_normal = vec3(0.0, u_y < 0.0 ? -1.0 : 1.0, 0.0);
    v_local = offset;
    v_world = (u_model * vec4(vert, 1.0)).xyz;
    v_normal = normalize(mat3(u_model) * local_normal);
    gl_Position = u_mvp * vec4(vert, 1.0);
}
"#;

pub const OVERVIEW_CAP_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform vec4 u_color;       // encoded base color; a = global prism fade
uniform vec3 u_accent;      // encoded UI accent
uniform vec3 u_camera;      // camera position in world space
uniform int u_scene_linear; // output FBO remains linear for KMS encode
uniform int u_reflection;   // mirrored overview pass below the floor plane
uniform float u_floor_y;    // world-space contact plane for reflection falloff

in vec2 v_local;
in float v_edge;
in vec3 v_world;
in vec3 v_normal;
out vec4 frag_color;

vec3 srgb_inverse(vec3 c) {
    c = clamp(c, 0.0, 1.0);
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(lo, hi, step(0.04045, c));
}

vec3 output_domain_color(vec3 encoded) {
    return u_scene_linear != 0 ? srgb_inverse(encoded) : encoded;
}

void main() {
    vec3 normal = normalize(v_normal);
    vec3 view = normalize(u_camera - v_world);
    normal *= sign(dot(normal, view) + 1.0e-4);
    vec3 light = normalize(vec3(-0.35, 0.85, 0.55));
    float diffuse = 0.74 + 0.26 * clamp(dot(normal, light), 0.0, 1.0);
    vec3 half_vec = normalize(light + view);
    float specular = pow(clamp(dot(normal, half_vec), 0.0, 1.0), 36.0) * 0.34;

    float edge = clamp(v_edge, 0.0, 1.0);
    float radial_shade = mix(1.12, 0.72, smoothstep(0.0, 1.0, edge));
    float rim = smoothstep(0.78, 1.0, edge);
    vec3 base = output_domain_color(u_color.rgb);
    vec3 accent = output_domain_color(u_accent);
    vec3 color = base * diffuse * radial_shade;
    color += accent * (0.035 + rim * 0.24);
    color += vec3(1.0) * specular;

    float alpha = clamp(u_color.a, 0.0, 1.0);
    if (u_reflection != 0) {
        float distance_below_floor = max(u_floor_y - v_world.y, 0.0);
        alpha *= 0.42 * exp(-distance_below_floor * 1.60);
        color = mix(color, output_domain_color(vec3(0.05, 0.06, 0.09)), 0.30) * 0.88;
    }
    frag_color = vec4(clamp(color, 0.0, 1.0) * alpha, alpha);
}
"#;

// ---------------------------------------------------------------------------
// Portal (iris wipe) transition shader
// ---------------------------------------------------------------------------

pub const PORTAL_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform float u_progress;    // 0.0 to 1.0
uniform float u_glow;        // glow intensity at edge
uniform vec2 u_center;       // center of portal in UV space (0.5, 0.5)
uniform vec4 u_uv_rect;
uniform vec4 u_rect;         // target rectangle in pixels
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec2 uv = u_uv_rect.xy + v_uv * u_uv_rect.zw;
    // Flip Y for FBO texture
    uv.y = u_uv_rect.y + (1.0 - v_uv.y) * u_uv_rect.w;
    vec4 texel = texture(u_texture, uv);

    // Measure in pixels, then normalize by the farthest corner. This keeps the
    // iris circular on ultrawide and portrait outputs.
    vec2 diff = v_uv - u_center;
    float max_dist = max(length(u_rect.zw) * 0.5, 1.0);
    float dist = length(diff * u_rect.zw) / max_dist;

    float radius = clamp(u_progress, 0.0, 1.0);

    // Smooth edge
    float edge_width = max(2.0 / max_dist, 0.015 + 0.02 * (1.0 - radius));
    float mask = smoothstep(radius - edge_width, radius, dist);

    // Glow ring at the edge
    float ring = (1.0 - smoothstep(radius, radius + edge_width, dist)) *
                 smoothstep(radius - edge_width * 2.0, radius - edge_width, dist);
    vec3 glow_color = vec3(0.4, 0.6, 1.0) * u_glow * ring * 2.0;

    // Old scene visible where mask > 0
    frag_color = vec4(texel.rgb * mask + glow_color, texel.a * mask);
}
"#;

pub const TRANSITION_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform float u_opacity; // 1.0 = fully visible old scene, 0.0 = gone
uniform vec4 u_uv_rect;  // x, y, w, h in texture UV space
in vec2 v_uv;
out vec4 frag_color;

void main() {
    // Snapshot comes from an FBO texture, whose Y direction is opposite to
    // the GLX window textures used in the main compositor pass.
    vec2 uv = vec2(
        u_uv_rect.x + v_uv.x * u_uv_rect.z,
        u_uv_rect.y + (1.0 - v_uv.y) * u_uv_rect.w
    );
    vec4 texel = texture(u_texture, uv);
    frag_color = texel * clamp(u_opacity, 0.0, 1.0);
}
"#;

// ---------------------------------------------------------------------------
// Screen edge glow shader
// ---------------------------------------------------------------------------

pub const EDGE_GLOW_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform vec4  u_glow_color;     // glow RGBA
uniform float u_glow_width;     // glow width in pixels
uniform vec2  u_mouse;          // mouse position in pixels
uniform vec2  u_screen_size;    // screen dimensions
uniform float u_time;           // reserved
in vec2 v_uv;
out vec4 frag_color;

void main() {
    float glow_width = max(u_glow_width, 0.001);
    vec2 pixel = v_uv * u_screen_size;

    float dist_left   = pixel.x;
    float dist_right  = u_screen_size.x - pixel.x;
    float dist_top    = pixel.y;
    float dist_bottom = u_screen_size.y - pixel.y;

    float mouse_dist_left   = u_mouse.x;
    float mouse_dist_right  = u_screen_size.x - u_mouse.x;
    float mouse_dist_top    = u_mouse.y;
    float mouse_dist_bottom = u_screen_size.y - u_mouse.y;
    float mouse_min = min(min(mouse_dist_left, mouse_dist_right), min(mouse_dist_top, mouse_dist_bottom));

    // Only glow on the edge closest to the mouse
    float edge_dist = glow_width;
    if (mouse_min < glow_width) {
        if (mouse_min == mouse_dist_left)        edge_dist = dist_left;
        else if (mouse_min == mouse_dist_right)   edge_dist = dist_right;
        else if (mouse_min == mouse_dist_top)     edge_dist = dist_top;
        else                                      edge_dist = dist_bottom;
    }

    float alpha = 1.0 - smoothstep(0.0, glow_width, edge_dist);
    alpha *= alpha;

    float mouse_factor = 1.0 - smoothstep(0.0, glow_width, mouse_min);
    alpha *= mouse_factor;

    float final_a = u_glow_color.a * alpha;
    frag_color = vec4(u_glow_color.rgb * final_a, final_a);
}
"#;

// ---------------------------------------------------------------------------
// Magnifier post-process shader (extends postprocess with magnifier)
// ---------------------------------------------------------------------------

pub const MAGNIFIER_POSTPROCESS_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform float u_color_temp;
uniform float u_saturation;
uniform float u_brightness;
uniform float u_contrast;
uniform int   u_invert;
uniform int   u_grayscale;
// Magnifier uniforms
uniform int   u_magnifier_enabled;
uniform vec2  u_magnifier_center;  // normalized [0,1] screen coords
uniform float u_magnifier_radius;  // radius in physical pixels
uniform float u_magnifier_zoom;    // zoom factor (e.g. 2.0)
uniform vec4  u_rect;              // fullscreen target; zw = pixel dimensions
// Colorblind correction uniform
uniform int   u_colorblind_mode;   // 0=none, 1=deuteranopia, 2=protanopia, 3=tritanopia
// HDR tone mapping uniforms
uniform int   u_hdr_enabled;           // 0=off, 1=on
uniform float u_hdr_peak_nits;         // Target display peak luminance (400-1000 nits)
uniform int   u_tone_mapping_method;   // 0=none, 1=Reinhard, 2=ACES
in vec2 v_uv;
out vec4 frag_color;

void main() {
    // FBO textures have Y=0 at bottom, but scene was rendered with top-left-origin
    // projection, so flip V to correct the vertical orientation.
    vec2 uv = vec2(v_uv.x, 1.0 - v_uv.y);
    vec2 sample_uv = uv;

    // Magnifier effect
    if (u_magnifier_enabled == 1) {
        vec2 diff = uv - u_magnifier_center;
        float dist = length(diff * u_rect.zw);
        if (dist < u_magnifier_radius) {
            // Zoom into the area around the center
            sample_uv = u_magnifier_center + diff / u_magnifier_zoom;
        }
    }

    vec4 c = texture(u_texture, sample_uv);

    // Colorblind correction (Daltonization) — applied before other color adjustments
    if (u_colorblind_mode > 0) {
        // Convert to LMS (Hunt-Pointer-Estevez matrix)
        mat3 rgb_to_lms = mat3(
            0.31399022, 0.15537241, 0.01775239,
            0.63951294, 0.75789446, 0.10944209,
            0.04649755, 0.08670142, 0.87256922
        );
        mat3 lms_to_rgb = mat3(
            5.47221206, -1.1252419, 0.02980165,
            -4.6419601, 2.29317094, -0.19318073,
            0.16963708, -0.1678952, 1.16364789
        );

        vec3 lms = rgb_to_lms * c.rgb;
        vec3 sim_lms = lms;

        if (u_colorblind_mode == 1) { // Deuteranopia
            sim_lms.y = 0.494207 * lms.x + 1.24827 * lms.z;
        } else if (u_colorblind_mode == 2) { // Protanopia
            sim_lms.x = 2.02344 * lms.y - 2.52581 * lms.z;
        } else if (u_colorblind_mode == 3) { // Tritanopia
            sim_lms.z = -0.395913 * lms.x + 0.801109 * lms.y;
        }

        vec3 sim_rgb = lms_to_rgb * sim_lms;
        vec3 error = c.rgb - sim_rgb;

        // Redistribute error to remaining channels
        c.r += error.r * 0.0;
        c.g += error.r * 0.7 + error.g * 1.0;
        c.b += error.r * 0.7 + error.b * 1.0;
        c.rgb = clamp(c.rgb, 0.0, 1.0);
    }

    // Invert
    if (u_invert == 1) {
        c.rgb = 1.0 - c.rgb;
    }

    // Grayscale
    if (u_grayscale == 1) {
        float lum = dot(c.rgb, vec3(0.2126, 0.7152, 0.0722));
        c.rgb = vec3(lum);
    }

    // Saturation
    float lum = dot(c.rgb, vec3(0.2126, 0.7152, 0.0722));
    c.rgb = mix(vec3(lum), c.rgb, u_saturation);

    // Brightness
    c.rgb *= u_brightness;

    // Contrast
    c.rgb = (c.rgb - 0.5) * u_contrast + 0.5;

    // Color temperature (shift red/blue)
    if (u_color_temp != 0.0) {
        float t = u_color_temp;
        c.r += t * 0.1;
        c.b -= t * 0.1;
        c.rgb = clamp(c.rgb, 0.0, 1.0);
    }

    // HDR tone mapping (SDR→HDR expansion)
    if (u_hdr_enabled == 1) {
        // Expand SDR content (assumed 0-80 nits) to HDR range (0-peak_nits)
        float sdr_white_nits = 80.0;
        float scale = u_hdr_peak_nits / sdr_white_nits;
        c.rgb *= scale;

        // Apply tone mapping curve to prevent clipping
        if (u_tone_mapping_method == 1) {
            // Reinhard tone mapping: x / (1 + x)
            c.rgb = c.rgb / (vec3(1.0) + c.rgb);
        } else if (u_tone_mapping_method == 2) {
            // ACES filmic tone mapping (simplified Narkowicz 2015)
            const float a = 2.51;
            const float b = 0.03;
            const float c_coef = 2.43;
            const float d = 0.59;
            const float e = 0.14;
            c.rgb = clamp((c.rgb * (a * c.rgb + b)) / (c.rgb * (c_coef * c.rgb + d) + e), 0.0, 1.0);
        }
        // else: u_tone_mapping_method == 0, no tone curve (linear expansion)
    }

    // Magnifier border ring
    if (u_magnifier_enabled == 1) {
        vec2 diff = uv - u_magnifier_center;
        float dist = length(diff * u_rect.zw);
        float ring = abs(dist - u_magnifier_radius);
        float ring_width = 2.0;
        float ring_alpha = 1.0 - smoothstep(0.0, ring_width, ring);
        c.rgb = mix(c.rgb, vec3(0.8, 0.8, 0.8), ring_alpha * 0.8);
    }

    frag_color = c;
}
"#;

// ---------------------------------------------------------------------------
// Window 3D tilt vertex shader
// ---------------------------------------------------------------------------

pub const TILT_VERTEX_SHADER: &str = r#"#version 300 es

uniform vec4  u_rect;        // x, y, w, h in pixels
uniform mat4  u_projection;  // orthographic projection
uniform vec2  u_tilt;        // tilt angles (x, y) in radians
uniform float u_perspective; // viewer distance in pixels
uniform int   u_grid_size;   // grid subdivisions (e.g. 8)

out vec2 v_uv;
out vec3 v_normal;   // surface normal after rotation

void main() {
    int grid = u_grid_size;
    int quad_id = gl_VertexID / 6;
    int vert_in_quad = gl_VertexID % 6;
    int col = quad_id % grid;
    int row = quad_id / grid;

    // Two triangles per quad: (0,1,2) and (2,1,3) = 6 vertices
    int dx, dy;
    if (vert_in_quad == 0)      { dx = 0; dy = 0; }
    else if (vert_in_quad == 1) { dx = 1; dy = 0; }
    else if (vert_in_quad == 2) { dx = 0; dy = 1; }
    else if (vert_in_quad == 3) { dx = 0; dy = 1; }
    else if (vert_in_quad == 4) { dx = 1; dy = 0; }
    else                        { dx = 1; dy = 1; }

    float fx = float(col + dx) / float(grid);
    float fy = float(row + dy) / float(grid);
    v_uv = vec2(fx, fy);

    // Center-relative position in pixels
    vec2 pixel = u_rect.xy + vec2(fx, fy) * u_rect.zw;
    vec2 center = u_rect.xy + u_rect.zw * 0.5;
    vec2 rel = pixel - center;

    // 3D rotation: Rx(tilt.x) * Ry(tilt.y)
    float sx = sin(u_tilt.x), cx = cos(u_tilt.x);
    float sy = sin(u_tilt.y), cy = cos(u_tilt.y);

    vec3 p = vec3(rel, 0.0);
    // Rotate around X axis (tilt from mouse Y)
    p = vec3(p.x,
             p.y * cx - p.z * sx,
             p.y * sx + p.z * cx);
    // Rotate around Y axis (tilt from mouse X)
    p = vec3(p.x * cy - p.z * sy,
             p.y,
             p.x * sy + p.z * cy);

    // Perspective projection
    float d = u_perspective;
    float scale = clamp(
        d / max(d - p.z, max(d * 0.1, 1.0)),
        0.4,
        2.5
    );
    vec2 projected = center + p.xy * scale;

    // Rotated normal (original face normal is [0,0,1])
    v_normal = vec3(-sy * cx, -sx, cx * cy);

    gl_Position = u_projection * vec4(projected, 0.0, 1.0);
}
"#;

pub const TILT_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_texture;
uniform float u_opacity;
uniform float u_radius;
uniform vec2  u_size;
uniform float u_dim;
uniform vec4  u_uv_rect;
uniform vec2  u_light_dir; // light direction in screen space (normalized 2D)
uniform int   u_scene_linear;

in vec2 v_uv;
in vec3 v_normal;
out vec4 frag_color;

float rounded_rect_sdf(vec2 p, vec2 half_size, float r) {
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

vec3 srgb_inverse(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(lo, hi, step(0.04045, c));
}

void main() {
    vec2 uv = u_uv_rect.xy + v_uv * u_uv_rect.zw;
    vec4 texel = texture(u_texture, uv);
    if (u_scene_linear == 1) {
        texel.rgb = srgb_inverse(texel.rgb);
    }
    float layer_opacity = clamp(abs(u_opacity), 0.0, 1.0);
    float a = (u_opacity >= 0.0 ? 1.0 : texel.a) * layer_opacity;
    texel.rgb *= layer_opacity;

    // Rounded corners
    if (u_radius > 0.0) {
        vec2 pixel_pos = v_uv * u_size;
        vec2 center = u_size * 0.5;
        float dist = rounded_rect_sdf(pixel_pos - center, center, u_radius);
        float aa = 1.0 - smoothstep(-1.0, 1.0, dist);
        a *= aa;
        texel.rgb *= aa;
    }

    // Specular highlight (Blinn-Phong)
    vec3 N = normalize(v_normal);
    vec3 L = normalize(vec3(u_light_dir, 0.5));
    vec3 V = vec3(0.0, 0.0, 1.0);
    vec3 H = normalize(L + V);
    float spec = pow(max(dot(N, H), 0.0), 48.0) * 0.15;

    // Edge darkening: fragments angled away from viewer get slightly darker
    float facing = max(dot(N, V), 0.0);
    float edge_darken = mix(0.82, 1.0, facing);

    vec3 color = texel.rgb * u_dim * edge_darken + vec3(spec * a);
    frag_color = vec4(color, a);
}
"#;

// ---------------------------------------------------------------------------
// Wobbly windows vertex shader (NxN grid with corner offsets)
// ---------------------------------------------------------------------------

pub const WOBBLY_VERTEX_SHADER: &str = r#"#version 300 es

uniform vec4 u_rect;               // x, y, w, h in pixels
uniform mat4 u_projection;
uniform vec2 u_grid_offsets[225];  // up to 15x15 grid node offsets
uniform int  u_grid_n;             // nodes per axis (grid_size + 1)

out vec2 v_uv;

void main() {
    int grid = u_grid_n - 1;      // quads per axis
    int quad_id = gl_VertexID / 6;
    int vert_in_quad = gl_VertexID % 6;

    int col = quad_id % grid;
    int row = quad_id / grid;

    // Two triangles per quad: (0,1,2) and (2,1,3)
    int dx, dy;
    if (vert_in_quad == 0)      { dx = 0; dy = 0; }
    else if (vert_in_quad == 1) { dx = 1; dy = 0; }
    else if (vert_in_quad == 2) { dx = 0; dy = 1; }
    else if (vert_in_quad == 3) { dx = 0; dy = 1; }
    else if (vert_in_quad == 4) { dx = 1; dy = 0; }
    else                        { dx = 1; dy = 1; }

    int node_col = col + dx;
    int node_row = row + dy;

    float fx = float(node_col) / float(grid);
    float fy = float(node_row) / float(grid);
    v_uv = vec2(fx, fy);

    // Direct grid node lookup — no bilinear interpolation needed
    vec2 offset = u_grid_offsets[node_row * u_grid_n + node_col];

    vec2 pixel = u_rect.xy + vec2(fx, fy) * u_rect.zw + offset;
    gl_Position = u_projection * vec4(pixel, 0.0, 1.0);
}
"#;

// ---------------------------------------------------------------------------
// Particle effect shaders
// ---------------------------------------------------------------------------

pub const PARTICLE_VERTEX_SHADER: &str = r#"#version 300 es

layout(location = 0) in vec2 a_position;
layout(location = 1) in vec4 a_color;
layout(location = 2) in float a_life; // 0.0 = dead, 1.0 = full life

uniform mat4 u_projection;
uniform float u_point_size;

out vec4 v_color;
out float v_life;

void main() {
    v_color = a_color;
    v_life = a_life;
    gl_Position = u_projection * vec4(a_position, 0.0, 1.0);
    gl_PointSize = u_point_size * a_life;
}
"#;

pub const PARTICLE_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

in vec4 v_color;
in float v_life;
out vec4 frag_color;

void main() {
    // Circular point
    vec2 coord = gl_PointCoord - vec2(0.5);
    float dist = length(coord);
    if (dist > 0.5) discard;

    float alpha = v_color.a * v_life * (1.0 - smoothstep(0.3, 0.5, dist));
    frag_color = vec4(v_color.rgb * alpha, alpha);
}
"#;

// ---------------------------------------------------------------------------
// Overview background shader (semi-transparent dark overlay)
// ---------------------------------------------------------------------------

pub const OVERVIEW_BG_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform float u_opacity;
uniform int u_scene_linear;
in vec2 v_uv;
out vec4 frag_color;

vec3 srgb_inverse(vec3 c) {
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(lo, hi, step(0.04045, c));
}

void main() {
    vec2 centered = v_uv - vec2(0.5);
    float dist = length(centered * vec2(1.0, 0.85));
    float vignette = smoothstep(0.1, 0.85, dist);
    vec3 top_tint = vec3(0.10, 0.12, 0.16);
    vec3 bottom_tint = vec3(0.03, 0.04, 0.06);
    if (u_scene_linear != 0) {
        top_tint = srgb_inverse(top_tint);
        bottom_tint = srgb_inverse(bottom_tint);
    }
    vec3 color = mix(top_tint, bottom_tint, clamp(v_uv.y * 1.15, 0.0, 1.0));
    // Semi-transparent dark tint so the wallpaper is visible underneath.
    // Windows on this monitor are already skipped during overview, so we
    // only need enough opacity to give the 3D prism a clean dark backdrop.
    float alpha = (0.78 + vignette * 0.12) * u_opacity;
    frag_color = vec4(color * alpha, alpha);
}
"#;

// ---------------------------------------------------------------------------
// Overview skydome: a static lighting environment for the 3D prism
// ---------------------------------------------------------------------------

/// Unlike `OVERVIEW_BG_FRAGMENT_SHADER`, which remains shared by Expose and
/// Peek, this program belongs only to the 3D overview. It intentionally has
/// no time uniform: angle/layout/content damage redraw it, but a settled prism
/// does not consume frames merely to twinkle the sky.
pub const OVERVIEW_SKYDOME_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform float u_opacity;
uniform float u_angle;      // prism rotation in radians; static sky parallax
uniform vec2 u_ground;      // x = horizon, y = floor contact in monitor UV
uniform vec3 u_accent;      // encoded UI accent shared with the prism
uniform int u_scene_linear; // output FBO remains linear for KMS encode
uniform vec4 u_rect;        // monitor rect; z/w provide aspect ratio
in vec2 v_uv;
out vec4 frag_color;

vec3 srgb_inverse(vec3 c) {
    c = clamp(c, 0.0, 1.0);
    vec3 lo = c / 12.92;
    vec3 hi = pow(max((c + 0.055) / 1.055, 0.0), vec3(2.4));
    return mix(lo, hi, step(0.04045, c));
}

vec3 output_domain_color(vec3 encoded) {
    return u_scene_linear != 0 ? srgb_inverse(encoded) : encoded;
}

float hash21(vec2 p) {
    p = fract(p * vec2(123.34, 456.21));
    p += dot(p, p + 45.32);
    return fract(p.x * p.y);
}

float star_layer(vec2 uv, float density, float seed) {
    vec2 grid = uv * density;
    vec2 cell = floor(grid);
    vec2 local = fract(grid);
    float pick = hash21(cell + seed);
    if (pick < 0.90) {
        return 0.0;
    }
    vec2 center = vec2(hash21(cell + seed + 3.1),
                       hash21(cell + seed + 7.7));
    return 1.0 - smoothstep(0.0, 0.07, length(local - center));
}

void main() {
    float aspect = max(u_rect.z, 1.0) / max(u_rect.w, 1.0);
    vec2 centered = (v_uv - vec2(0.5)) * vec2(aspect, 1.0);
    float horizon = clamp(u_ground.x, 0.02, 0.98);
    float above = clamp((horizon - v_uv.y) / max(horizon, 0.001), 0.0, 1.0);
    float below = clamp((v_uv.y - horizon) / max(1.0 - horizon, 0.001), 0.0, 1.0);

    vec3 accent = output_domain_color(u_accent);
    vec3 zenith = output_domain_color(vec3(0.026, 0.036, 0.068));
    vec3 haze = output_domain_color(vec3(0.062, 0.086, 0.140));
    vec3 color = mix(haze, zenith, pow(above, 0.70));

    // Two deterministic star layers move only when the prism itself turns.
    float pan = u_angle * 0.16;
    float stars = star_layer(vec2(v_uv.x * aspect + pan, v_uv.y), 26.0, 0.0)
                + star_layer(vec2(v_uv.x * aspect + pan * 1.9, v_uv.y), 46.0, 11.0) * 0.55;
    color += output_domain_color(vec3(0.82, 0.88, 1.0)) * stars * above * 0.85;

    vec3 floor_near = output_domain_color(vec3(0.040, 0.050, 0.074));
    vec3 floor_far = output_domain_color(vec3(0.008, 0.011, 0.018));
    vec3 floor_color = mix(floor_near, floor_far, pow(below, 0.55));
    vec2 pool_delta = vec2(centered.x, (v_uv.y - u_ground.y) * 2.4);
    float pool = exp(-dot(pool_delta, pool_delta) * 2.6);
    floor_color += accent * pool * 0.26;
    color = mix(color, floor_color,
                smoothstep(horizon - 0.06, horizon + 0.10, v_uv.y));

    float band = exp(-pow((v_uv.y - horizon) * 9.0, 2.0));
    color += accent * band * 0.22;
    float vignette = 1.0 - smoothstep(0.24, 0.92, length(centered));
    color *= mix(0.52, 1.0, vignette);

    float alpha = (0.90 + (1.0 - vignette) * 0.08)
                * clamp(u_opacity, 0.0, 1.0);
    frag_color = vec4(clamp(color, 0.0, 1.0) * alpha, alpha);
}
"#;

// ---------------------------------------------------------------------------
// Phase 3.2: Genie/Magic Lamp minimize vertex shader
// ---------------------------------------------------------------------------

pub const GENIE_VERTEX_SHADER: &str = r#"#version 300 es

uniform vec4 u_rect;       // x, y, w, h in pixels
uniform mat4 u_projection;
uniform float u_progress;  // 0.0 = normal, 1.0 = fully minimized
uniform vec2 u_dock_pos;   // dock target position in pixels
uniform vec2 u_dock_size;  // dock target slot size in pixels
uniform int u_grid_size;   // grid subdivisions

out vec2 v_uv;

void main() {
    int grid = u_grid_size;
    int quad_id = gl_VertexID / 6;
    int vert_in_quad = gl_VertexID % 6;
    int col = quad_id % grid;
    int row = quad_id / grid;

    int dx, dy;
    if (vert_in_quad == 0)      { dx = 0; dy = 0; }
    else if (vert_in_quad == 1) { dx = 1; dy = 0; }
    else if (vert_in_quad == 2) { dx = 0; dy = 1; }
    else if (vert_in_quad == 3) { dx = 0; dy = 1; }
    else if (vert_in_quad == 4) { dx = 1; dy = 0; }
    else                        { dx = 1; dy = 1; }

    float fx = float(col + dx) / float(grid);
    float fy = float(row + dy) / float(grid);
    v_uv = vec2(fx, fy);

    // Bottom rows lead the deformation, but every row reaches the dock at
    // progress=1. The previous row-weighted formula left the top edge behind.
    float t = smoothstep(0.0, 1.0, clamp(u_progress, 0.0, 1.0));
    float window_center_y = u_rect.y + u_rect.w * 0.5;
    float leading_row = u_dock_pos.y < window_center_y ? (1.0 - fy) : fy;
    float delay = (1.0 - leading_row) * 0.22;
    float collapse = smoothstep(0.0, 1.0, clamp((t - delay) / (1.0 - delay), 0.0, 1.0));
    float center_x = u_rect.x + u_rect.z * 0.5;
    float target_x = mix(center_x, u_dock_pos.x, collapse);
    float half_w = mix(u_rect.z * 0.5, max(u_dock_size.x, 1.0) * 0.5, collapse);
    float px = target_x + (fx - 0.5) * half_w * 2.0;
    float original_y = u_rect.y + fy * u_rect.w;
    float target_y = u_dock_pos.y + (fy - 0.5) * max(u_dock_size.y, 1.0);
    float py = mix(original_y, target_y, collapse);

    gl_Position = u_projection * vec4(px, py, 0.0, 1.0);
}
"#;

// Genie uses the same fragment shader as windows (FRAGMENT_SHADER)

// ---------------------------------------------------------------------------
// P4: Temporal Blur Mix shader (for temporal blur reuse)
// ---------------------------------------------------------------------------

/// Mix current blur frame with previous blur frame for temporal stability.
/// Formula: output = (1 - u_temporal_mix) * current + u_temporal_mix * previous
pub const TEMPORAL_BLUR_MIX_VERTEX: &str = r#"#version 300 es

uniform vec4 u_rect;
uniform mat4 u_projection;
layout(location = 0) in vec2 a_position;
out vec2 v_uv;

void main() {
    v_uv = a_position;
    vec2 pixel = u_rect.xy + a_position * u_rect.zw;
    gl_Position = u_projection * vec4(pixel, 0.0, 1.0);
}
"#;

pub const TEMPORAL_BLUR_MIX_FRAGMENT: &str = r#"#version 300 es
precision highp float;

uniform sampler2D u_current_blur;    // Current frame blur result
uniform sampler2D u_previous_blur;   // Previous frame blur result
uniform float u_temporal_mix;        // 0.0 = all current, 1.0 = all previous (typical: 0.8)
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec4 current = texture(u_current_blur, v_uv);
    vec4 previous = texture(u_previous_blur, v_uv);

    // Linear blend: (1-ratio)*new + ratio*previous
    // High ratio (e.g., 0.8) = 80% previous, 20% new (more stable)
    frag_color = mix(current, previous, u_temporal_mix);
}
"#;

// ---------------------------------------------------------------------------
// Annotation line shaders
// ---------------------------------------------------------------------------

pub const LINE_VERTEX_SHADER: &str = r#"#version 300 es

uniform mat4 u_projection;

layout(location = 0) in vec2 a_position;

void main() {
    gl_Position = u_projection * vec4(a_position, 0.0, 1.0);
}
"#;

pub const LINE_FRAGMENT_SHADER: &str = r#"#version 300 es
precision highp float;

uniform vec4 u_color;

out vec4 frag_color;

void main() {
    frag_color = vec4(u_color.rgb * u_color.a, u_color.a);
}
"#;

#[cfg(test)]
mod tests {
    use super::{FRAGMENT_SHADER, GENIE_VERTEX_SHADER};

    #[test]
    fn genie_and_dock_texture_shader_can_write_into_a_linear_output() {
        // Genie is linked with FRAGMENT_SHADER, and the static/hover Dock
        // passes use the same program. Keep the hardware OETF safety contract
        // explicit: legacy sRGB pixels must be decoded before that output LUT.
        assert!(FRAGMENT_SHADER.contains("uniform int   u_scene_linear"));
        assert!(FRAGMENT_SHADER.contains("else if (u_scene_linear == 1)"));
        assert!(GENIE_VERTEX_SHADER.contains("u_dock_size"));
    }
}
