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

    // Rounded corners
    if (u_radius > 0.0) {
        vec2 pixel_pos = v_uv * u_size;
        vec2 center = u_size * 0.5;
        float dist = rounded_rect_sdf(pixel_pos - center, center, u_radius);
        float aa = 1.0 - smoothstep(-1.0, 1.0, dist);
        a *= aa;
    }

    // Dim inactive windows
    frag_color = vec4(texel.rgb * u_dim, a * u_dim);
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
