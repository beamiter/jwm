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
in vec2 v_uv;
out vec4 frag_color;

float rounded_rect_sdf(vec2 p, vec2 half_size, float r) {
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

void main() {
    vec4 texel = texture(u_texture, v_uv);
    float a = u_opacity >= 0.0 ? u_opacity : texel.a;

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

    // Dim inactive windows: darken RGB, keep alpha unchanged for opaque
    // windows (u_opacity >= 0) so they remain fully opaque and don't
    // flicker on multi-monitor setups with unsynchronized vblank.
    // For RGBA windows (u_opacity < 0), dim alpha too for translucency.
    float out_a = u_opacity >= 0.0 ? a : a * u_dim;
    frag_color = vec4(texel.rgb * u_dim, out_a);
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
    frag_color = vec4(u_shadow_color.rgb, u_shadow_color.a * alpha);
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
// Feature 1: Window border / outline shader
// ---------------------------------------------------------------------------

pub const BORDER_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform vec4  u_border_color;  // border RGBA
uniform vec2  u_size;          // window size in pixels
uniform float u_radius;        // corner radius (0 = sharp)
uniform float u_border_width;  // border width in pixels
in vec2 v_uv;
out vec4 frag_color;

float rounded_rect_sdf(vec2 p, vec2 half_size, float r) {
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

void main() {
    vec2 pixel_pos = v_uv * u_size;
    vec2 center = u_size * 0.5;
    float dist = rounded_rect_sdf(pixel_pos - center, center, u_radius);
    // The border is visible between -u_border_width and 0
    float outer = 1.0 - smoothstep(-1.0, 1.0, dist);
    float inner = 1.0 - smoothstep(-1.0, 1.0, dist + u_border_width);
    float border_mask = outer - inner;
    frag_color = vec4(u_border_color.rgb, u_border_color.a * border_mask);
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
    vec4 c = texture(u_texture, v_uv);

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
    frag_color = vec4(u_bg_color.rgb, alpha * mask);
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
    frag_color = vec4(texel.rgb * u_brightness, texel.a);
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
