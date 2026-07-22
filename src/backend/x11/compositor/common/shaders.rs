pub const VERTEX_SHADER: &str = r#"#version 330 core

uniform vec4 u_rect;       // x, y, w, h in pixels
uniform mat4 u_projection; // orthographic projection

out vec2 v_uv;

void main() {
    // Generate a fullscreen quad from gl_VertexID (0..3)
    vec2 pos = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    v_uv = pos; // GLX textures are Y-inverted (top-left origin matches screen coords)
    vec2 pixel = u_rect.xy + pos * u_rect.zw;
    gl_Position = u_projection * vec4(pixel, 0.0, 1.0);
}
"#;

pub const FRAGMENT_SHADER: &str = r#"#version 330 core

uniform sampler2D u_texture;
uniform float u_opacity; // 1.0 for RGB windows (force opaque), negative to use texture alpha
uniform float u_radius;  // corner radius in pixels (0 = sharp)
uniform vec2  u_size;    // window size in pixels (w, h)
uniform float u_dim;     // dim multiplier (1.0 = no dim, <1.0 = darken)
uniform vec4  u_uv_rect; // x, y, w, h in UV space
uniform float u_ripple_progress;  // 0.0 = start, 1.0 = done, <0 = inactive
uniform float u_ripple_amplitude; // UV distortion strength (0 = no ripple)
in vec2 v_uv;
out vec4 frag_color;

float rounded_rect_sdf(vec2 p, vec2 half_size, float r) {
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
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
    float layer_opacity = clamp(abs(u_opacity), 0.0, 1.0);
    float a = (u_opacity >= 0.0 ? 1.0 : texel.a) * layer_opacity;
    texel.rgb *= layer_opacity;

    // Rounded corners – must mask both alpha AND rgb for premultiplied-alpha
    // blending (GL_ONE, GL_ONE_MINUS_SRC_ALPHA), otherwise rgb bleeds through
    // at corners where alpha is zero.
    if (u_radius > 0.0) {
        vec2 pixel_pos = v_uv * u_size;
        vec2 center = u_size * 0.5;
        float dist = rounded_rect_sdf(pixel_pos - center, center, u_radius);
        float aa = 1.0 - smoothstep(-1.0, 1.0, dist);
        a *= aa;
        texel.rgb *= aa;
    }

    // The compositor uses premultiplied-alpha blending. Layer opacity must
    // therefore scale RGB and alpha together for both opaque and RGBA clients.
    frag_color = vec4(texel.rgb * u_dim, a);
}
"#;

/// Shadow quad: draws a soft rectangular shadow using SDF + gaussian-ish falloff.
pub const SHADOW_FRAGMENT_SHADER: &str = r#"#version 330 core

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
    // Smooth falloff: fully opaque at dist<=0, fades out over u_spread
    float alpha = 1.0 - smoothstep(0.0, u_spread, dist);
    alpha = alpha * alpha; // softer falloff
    float final_alpha = u_shadow_color.a * alpha;
    frag_color = vec4(u_shadow_color.rgb * final_alpha, final_alpha);
}
"#;

// ---------------------------------------------------------------------------
// Dual Kawase blur shaders
// ---------------------------------------------------------------------------

/// Kawase downsample shader: samples 4 diagonal neighbours + center with offsets.
pub const BLUR_DOWN_VERTEX: &str = r#"#version 330 core

uniform vec4 u_rect; // x, y, w, h in pixels (fullscreen quad for blur pass)
uniform mat4 u_projection;
out vec2 v_uv;

void main() {
    vec2 pos = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    v_uv = pos;
    vec2 pixel = u_rect.xy + pos * u_rect.zw;
    gl_Position = u_projection * vec4(pixel, 0.0, 1.0);
}
"#;

pub const BLUR_DOWN_FRAGMENT: &str = r#"#version 330 core

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
pub const BLUR_UP_FRAGMENT: &str = r#"#version 330 core

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
pub const BOX_BLUR_FRAGMENT: &str = r#"#version 330 core

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

pub const BORDER_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform vec4  u_border_color;  // border/glow RGBA
uniform vec2  u_size;          // outline quad size, or inner window size in glow mode
uniform float u_radius;        // corner radius (0 = sharp)
uniform float u_border_width;  // >=0: border width, <0: directional glow radius
in vec2 v_uv;
out vec4 frag_color;

float rounded_rect_sdf(vec2 p, vec2 half_size, float r) {
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

void main() {
    if (u_border_width < 0.0) {
        float spread = max(-u_border_width, 0.001);
        vec2 expanded = u_size + vec2(2.0 * spread);
        vec2 pixel_pos = v_uv * expanded;
        vec2 center = expanded * 0.5;
        float dist = rounded_rect_sdf(pixel_pos - center, u_size * 0.5, u_radius);
        float aa = max(fwidth(dist), 0.75);
        float outside = max(dist, 0.0);
        float normalized = outside / spread;
        float outside_mask = smoothstep(-aa, aa, dist);

        // Soft outer halo, explicitly faded to zero at the expanded quad edge.
        float halo = exp2(-4.0 * normalized * normalized) * outside_mask;
        halo *= 1.0 - smoothstep(0.72, 1.0, normalized);

        // A narrow luminous core leaves a crisp edge even when ordinary borders
        // are disabled. The later client draw covers its inner half.
        float core = 1.0 - smoothstep(0.0, aa * 1.75, abs(dist));
        core *= outside_mask;

        // Directional energy and two asymmetric hotspots reproduce the brighter
        // top/right and subtler lower-left neon treatment without a blur FBO.
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
        frag_color = vec4(u_border_color.rgb * a, a);
        return;
    }

    vec2 pixel_pos = v_uv * u_size;
    vec2 center = u_size * 0.5;
    float dist = rounded_rect_sdf(pixel_pos - center, center, u_radius);
    // The border is visible between -u_border_width and 0
    float outer = 1.0 - smoothstep(-1.0, 1.0, dist);
    float inner = 1.0 - smoothstep(-1.0, 1.0, dist + u_border_width);
    float border_mask = outer - inner;
    float a = u_border_color.a * border_mask;
    frag_color = vec4(u_border_color.rgb * a, a);  // premultiplied alpha
}
"#;

// ---------------------------------------------------------------------------
// Feature 9 & 10: Post-processing shader (color temperature, invert, filters)
// ---------------------------------------------------------------------------

pub const POSTPROCESS_FRAGMENT_SHADER: &str = r#"#version 330 core

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

pub const HUD_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform vec4  u_bg_color; // background color for HUD panel
uniform vec4  u_fg_color; // foreground (text) color
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
// Screenshot annotation lines
// ---------------------------------------------------------------------------

/// A line vertex shader intentionally consumes the submitted positions.
///
/// The compositor's regular/HUD vertex shader generates a quad from
/// `gl_VertexID`, which is correct for panels but makes `GL_LINES` annotation
/// geometry collapse into HUD-quad coordinates.
pub const LINE_VERTEX_SHADER: &str = r#"#version 330 core

uniform mat4 u_projection;

layout(location = 0) in vec2 a_position;

void main() {
    gl_Position = u_projection * vec4(a_position, 0.0, 1.0);
}
"#;

pub const LINE_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform vec4 u_color;

out vec4 frag_color;

void main() {
    frag_color = vec4(u_color.rgb * u_color.a, u_color.a);
}
"#;

// ---------------------------------------------------------------------------
// Feature 11b: HUD text overlay (pre-rasterized bitmap font texture)
// ---------------------------------------------------------------------------

pub const HUD_TEXT_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform sampler2D u_texture;
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec4 texel = texture(u_texture, v_uv);
    // Output premultiplied alpha for GL_ONE, GL_ONE_MINUS_SRC_ALPHA blending
    frag_color = vec4(texel.rgb * texel.a, texel.a);
}
"#;

// ---------------------------------------------------------------------------
// Tag-switch transition shader
// ---------------------------------------------------------------------------

/// Draws a snapshot texture for workspace transitions. The sampled source area
/// can be cropped so persistent UI such as the status bar is excluded.
pub const CUBE_VERTEX_SHADER: &str = r#"#version 330 core

uniform mat4 u_mvp;
uniform float u_aspect; // screen_w / workspace_h
out vec2 v_uv;

void main() {
    vec2 pos = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    v_uv = pos;
    // Face quad spans [-aspect, -1] to [+aspect, +1] in model space
    vec3 vert = vec3((pos.x * 2.0 - 1.0) * u_aspect, pos.y * 2.0 - 1.0, 0.0);
    gl_Position = u_mvp * vec4(vert, 1.0);
}
"#;

pub const CUBE_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform sampler2D u_texture;
uniform float u_brightness; // face lighting (1.0 = fully lit)
uniform vec4 u_uv_rect;     // x, y, w, h in texture UV space
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec2 uv = u_uv_rect.xy + v_uv * u_uv_rect.zw;
    vec4 texel = texture(u_texture, uv);
    frag_color = vec4(texel.rgb * u_brightness, texel.a * u_brightness);
}
"#;

// ---------------------------------------------------------------------------
// Portal (iris wipe) transition shader
// ---------------------------------------------------------------------------

pub const PORTAL_FRAGMENT_SHADER: &str = r#"#version 330 core
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
    frag_color = vec4(texel.rgb + glow_color, texel.a * mask);
}
"#;

pub const TRANSITION_FRAGMENT_SHADER: &str = r#"#version 330 core

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
    frag_color = vec4(texel.rgb, texel.a * u_opacity);
}
"#;

// ---------------------------------------------------------------------------
// Screen edge glow shader
// ---------------------------------------------------------------------------

pub const EDGE_GLOW_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform vec4  u_glow_color;     // glow RGBA
uniform float u_glow_width;     // glow width in pixels
uniform vec2  u_mouse;          // mouse position in pixels
uniform vec2  u_screen_size;    // screen dimensions
uniform float u_time;           // reserved
in vec2 v_uv;
out vec4 frag_color;

void main() {
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
    float edge_dist = u_glow_width;
    if (mouse_min < u_glow_width) {
        if (mouse_min == mouse_dist_left)        edge_dist = dist_left;
        else if (mouse_min == mouse_dist_right)   edge_dist = dist_right;
        else if (mouse_min == mouse_dist_top)     edge_dist = dist_top;
        else                                      edge_dist = dist_bottom;
    }

    float alpha = 1.0 - smoothstep(0.0, u_glow_width, edge_dist);
    alpha *= alpha;

    float mouse_factor = 1.0 - smoothstep(0.0, u_glow_width, mouse_min);
    alpha *= mouse_factor;

    float final_a = u_glow_color.a * alpha;
    frag_color = vec4(u_glow_color.rgb * final_a, final_a);
}
"#;

// ---------------------------------------------------------------------------
// Advanced post-process shader (magnifier/accessibility/HDR)
// ---------------------------------------------------------------------------

pub const ADVANCED_POSTPROCESS_FRAGMENT_SHADER: &str = r#"#version 330 core

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
uniform int   u_eotf_mode;            // 0=sRGB gamma, 1=PQ (ST2084), 2=HLG
uniform int   u_output_colorspace;    // 0=BT.709/sRGB, 1=BT.2020
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

    // HDR pipeline: linearize → nits → tone map → gamut → encode EOTF
    if (u_hdr_enabled == 1) {
        // Step 1: Linearize from sRGB gamma (SDR source content)
        c.rgb = pow(c.rgb, vec3(2.2));

        // Step 2: Scale to absolute nits (SDR white = 80 nits)
        c.rgb *= 80.0;

        // Step 3: Tone mapping to display peak luminance
        if (u_tone_mapping_method == 1) {
            // Reinhard per-channel, normalized to peak nits
            c.rgb = c.rgb * (1.0 + c.rgb / (u_hdr_peak_nits * u_hdr_peak_nits))
                  / (1.0 + c.rgb / u_hdr_peak_nits);
        } else if (u_tone_mapping_method == 2) {
            // ACES filmic (Narkowicz 2015), input/output in [0,1] range
            vec3 x = c.rgb / u_hdr_peak_nits;
            const float a = 2.51;
            const float b = 0.03;
            const float cc = 2.43;
            const float d = 0.59;
            const float e = 0.14;
            c.rgb = clamp((x * (a * x + b)) / (x * (cc * x + d) + e), 0.0, 1.0) * u_hdr_peak_nits;
        }
        // else: method 0 = no tone mapping, pass through

        // Step 4: BT.2020 gamut conversion (from BT.709 linear)
        if (u_output_colorspace == 1) {
            const mat3 bt709_to_bt2020 = mat3(
                0.6274, 0.0691, 0.0164,
                0.3293, 0.9195, 0.0880,
                0.0433, 0.0113, 0.8956
            );
            c.rgb = bt709_to_bt2020 * c.rgb;
        }

        // Step 5: Encode with output EOTF
        if (u_eotf_mode == 1) {
            // PQ (ST2084) OETF: linear nits → PQ [0,1]
            const float m1 = 0.1593017578125;
            const float m2 = 78.84375;
            const float c1 = 0.8359375;
            const float c2 = 18.8515625;
            const float c3 = 18.6875;
            vec3 Y = clamp(c.rgb / 10000.0, 0.0, 1.0);
            vec3 Ym1 = pow(Y, vec3(m1));
            c.rgb = pow((c1 + c2 * Ym1) / (1.0 + c3 * Ym1), vec3(m2));
        } else if (u_eotf_mode == 2) {
            // HLG OETF (BT.2100)
            vec3 nrm = c.rgb / 1000.0; // normalize to [0,1] for 1000-nit reference
            vec3 lo = sqrt(3.0 * nrm);
            const float a = 0.17883277;
            const float b_hlg = 0.28466892;
            const float c_hlg = 0.55991073;
            vec3 hi = a * log(12.0 * nrm - b_hlg) + c_hlg;
            c.rgb = mix(lo, hi, step(vec3(1.0/12.0), nrm));
        } else {
            // sRGB gamma encode (SDR display, reduced banding from 10-bit internal)
            c.rgb = clamp(c.rgb / u_hdr_peak_nits, 0.0, 1.0);
            c.rgb = pow(c.rgb, vec3(1.0 / 2.2));
        }
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
// WaterLily compositor layer
// ---------------------------------------------------------------------------

pub const WATERLILY_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform sampler2D u_texture;
uniform sampler2D u_scene_texture;
uniform int u_scene_available;
uniform vec2 u_screen_size;
uniform float u_opacity;
in vec2 v_uv;
out vec4 frag_color;

vec3 blurred_scene(vec2 uv) {
    vec2 step_uv = vec2(4.0) / u_screen_size;
    // Separable 5x5 Gaussian weights, evaluated in one pass over the small
    // WaterLily rectangle. The dedicated scene snapshot prevents feedback.
    float weights[5] = float[](1.0, 4.0, 6.0, 4.0, 1.0);
    vec3 sum = vec3(0.0);
    for (int y = -2; y <= 2; ++y) {
        for (int x = -2; x <= 2; ++x) {
            float weight = weights[x + 2] * weights[y + 2];
            sum += texture(u_scene_texture, uv + vec2(x, y) * step_uv).rgb * weight;
        }
    }
    return sum / 256.0;
}

// Catmull-Rom bicubic upsampling as nine bilinear fetches. The simulation
// frame is stretched several times to cover the display; plain bilinear
// magnification leaves visible texel stars on smooth vorticity gradients.
vec4 sample_simulation(vec2 uv) {
    vec2 tex_size = vec2(textureSize(u_texture, 0));
    vec2 sample_pos = uv * tex_size;
    vec2 nearest_texel = floor(sample_pos - 0.5) + 0.5;
    vec2 f = sample_pos - nearest_texel;

    vec2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    vec2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    vec2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    vec2 w3 = f * f * (-0.5 + 0.5 * f);
    vec2 w12 = w1 + w2;

    vec2 pos0 = (nearest_texel - 1.0) / tex_size;
    vec2 pos12 = (nearest_texel + w2 / w12) / tex_size;
    vec2 pos3 = (nearest_texel + 2.0) / tex_size;

    vec4 result =
          texture(u_texture, vec2(pos0.x, pos0.y)) * w0.x * w0.y
        + texture(u_texture, vec2(pos12.x, pos0.y)) * w12.x * w0.y
        + texture(u_texture, vec2(pos3.x, pos0.y)) * w3.x * w0.y
        + texture(u_texture, vec2(pos0.x, pos12.y)) * w0.x * w12.y
        + texture(u_texture, vec2(pos12.x, pos12.y)) * w12.x * w12.y
        + texture(u_texture, vec2(pos3.x, pos12.y)) * w3.x * w12.y
        + texture(u_texture, vec2(pos0.x, pos3.y)) * w0.x * w3.y
        + texture(u_texture, vec2(pos12.x, pos3.y)) * w12.x * w3.y
        + texture(u_texture, vec2(pos3.x, pos3.y)) * w3.x * w3.y;
    // The negative Catmull-Rom lobes can overshoot on sharp edges.
    return clamp(result, 0.0, 1.0);
}

void main() {
    vec4 simulation = sample_simulation(v_uv);

    // The v1 producer emits an opaque, nearly-white simulation background.
    // Key only bright, low-chroma pixels so pale blue/red flow details remain.
    float high = max(simulation.r, max(simulation.g, simulation.b));
    float low = min(simulation.r, min(simulation.g, simulation.b));
    float chroma = high - low;
    float bright_white = smoothstep(0.82, 0.985, low);
    float neutral = 1.0 - smoothstep(0.025, 0.12, chroma);
    float backdrop_mix = clamp(bright_white * neutral, 0.0, 1.0);

    vec3 backdrop = vec3(0.0);
    float backdrop_alpha = 0.0;
    if (u_scene_available == 1) {
        // gl_FragCoord and the blitted scene texture both use bottom-left
        // framebuffer coordinates, avoiding an extra orientation conversion.
        vec2 screen_uv = gl_FragCoord.xy / u_screen_size;
        backdrop = blurred_scene(screen_uv);
        backdrop_alpha = backdrop_mix * 0.58;
    }

    float simulation_alpha = simulation.a * (1.0 - backdrop_mix);
    vec3 premultiplied = simulation.rgb * simulation_alpha
                       + backdrop * backdrop_alpha * (1.0 - simulation_alpha);
    float alpha = simulation_alpha + backdrop_alpha * (1.0 - simulation_alpha);

    float layer_opacity = clamp(u_opacity, 0.0, 1.0);
    frag_color = vec4(premultiplied * layer_opacity, alpha * layer_opacity);
}
"#;

// ---------------------------------------------------------------------------
// Window 3D tilt vertex shader
// ---------------------------------------------------------------------------

pub const TILT_VERTEX_SHADER: &str = r#"#version 330 core

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

pub const TILT_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform sampler2D u_texture;
uniform float u_opacity;
uniform float u_radius;
uniform vec2  u_size;
uniform float u_dim;
uniform vec4  u_uv_rect;
uniform vec2  u_light_dir; // light direction in screen space (normalized 2D)

in vec2 v_uv;
in vec3 v_normal;
out vec4 frag_color;

float rounded_rect_sdf(vec2 p, vec2 half_size, float r) {
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

void main() {
    vec2 uv = u_uv_rect.xy + v_uv * u_uv_rect.zw;
    vec4 texel = texture(u_texture, uv);
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

pub const WOBBLY_VERTEX_SHADER: &str = r#"#version 330 core

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

pub const PARTICLE_VERTEX_SHADER: &str = r#"#version 330 core

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

pub const PARTICLE_FRAGMENT_SHADER: &str = r#"#version 330 core

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

pub const OVERVIEW_BG_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform float u_opacity;
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec2 centered = v_uv - vec2(0.5);
    float dist = length(centered * vec2(1.0, 0.85));
    float vignette = smoothstep(0.1, 0.85, dist);
    vec3 top_tint = vec3(0.10, 0.12, 0.16);
    vec3 bottom_tint = vec3(0.03, 0.04, 0.06);
    vec3 color = mix(top_tint, bottom_tint, clamp(v_uv.y * 1.15, 0.0, 1.0));
    // Semi-transparent dark tint so the wallpaper is visible underneath.
    // Windows on this monitor are already skipped during overview, so we
    // only need enough opacity to give the 3D prism a clean dark backdrop.
    float alpha = (0.78 + vignette * 0.12) * u_opacity;
    frag_color = vec4(color * alpha, alpha);
}
"#;

// ---------------------------------------------------------------------------
// Phase 3.2: Genie/Magic Lamp minimize vertex shader
// ---------------------------------------------------------------------------

pub const GENIE_VERTEX_SHADER: &str = r#"#version 330 core
uniform vec4 u_rect;       // x, y, w, h in pixels
uniform mat4 u_projection;
uniform float u_progress;  // 0.0 = normal, 1.0 = fully minimized
uniform vec2 u_dock_pos;   // dock target position in pixels
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
    float delay = (1.0 - fy) * 0.22;
    float collapse = smoothstep(0.0, 1.0, clamp((t - delay) / (1.0 - delay), 0.0, 1.0));
    float center_x = u_rect.x + u_rect.z * 0.5;
    float target_x = mix(center_x, u_dock_pos.x, collapse);
    float half_w = u_rect.z * 0.5 * mix(1.0, 0.015, collapse);
    float px = target_x + (fx - 0.5) * half_w * 2.0;
    float original_y = u_rect.y + fy * u_rect.w;
    float py = mix(original_y, u_dock_pos.y, collapse);

    gl_Position = u_projection * vec4(px, py, 0.0, 1.0);
}
"#;

// Genie uses the same fragment shader as windows (FRAGMENT_SHADER)

// ---------------------------------------------------------------------------
// P4: Temporal Blur Mix shader (for temporal blur reuse)
// ---------------------------------------------------------------------------

/// Mix current blur frame with previous blur frame for temporal stability.
/// Formula: output = (1 - u_temporal_mix) * current + u_temporal_mix * previous
pub const TEMPORAL_BLUR_MIX_VERTEX: &str = r#"#version 330 core

uniform vec4 u_rect;
uniform mat4 u_projection;
out vec2 v_uv;

void main() {
    vec2 pos = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    // Both inputs use the final blur texture orientation: visual top at v=0.
    // Rendering into a framebuffer would invert that orientation, so sample
    // with a flipped v coordinate to keep the temporal result compatible with
    // the regular window shader.
    v_uv = vec2(pos.x, 1.0 - pos.y);
    vec2 pixel = u_rect.xy + pos * u_rect.zw;
    gl_Position = u_projection * vec4(pixel, 0.0, 1.0);
}
"#;

pub const TEMPORAL_BLUR_MIX_FRAGMENT: &str = r#"#version 330 core

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
