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
in vec2 v_uv;
out vec4 frag_color;

float rounded_rect_sdf(vec2 p, vec2 half_size, float r) {
    vec2 d = abs(p) - half_size + vec2(r);
    return length(max(d, 0.0)) + min(max(d.x, d.y), 0.0) - r;
}

void main() {
    vec2 uv = u_uv_rect.xy + v_uv * u_uv_rect.zw;
    vec4 texel = texture(u_texture, uv);
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
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec2 uv = u_uv_rect.xy + v_uv * u_uv_rect.zw;
    // Flip Y for FBO texture
    uv.y = u_uv_rect.y + (1.0 - v_uv.y) * u_uv_rect.w;
    vec4 texel = texture(u_texture, uv);

    // Distance from center in normalized coords
    vec2 diff = v_uv - u_center;
    float dist = length(diff);

    // Portal radius grows from 0 to sqrt(2) (full diagonal)
    float radius = u_progress * 1.42; // sqrt(2)

    // Smooth edge
    float edge_width = 0.02 + 0.03 * (1.0 - u_progress);
    float mask = smoothstep(radius, radius - edge_width, dist);

    // Glow ring at the edge
    float ring = smoothstep(radius + edge_width, radius, dist) *
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
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec2 pixel = v_uv * u_screen_size;
    float dist_left   = pixel.x;
    float dist_right  = u_screen_size.x - pixel.x;
    float dist_top    = pixel.y;
    float dist_bottom = u_screen_size.y - pixel.y;

    float min_dist = min(min(dist_left, dist_right), min(dist_top, dist_bottom));

    // Only glow near the edge closest to the mouse
    float mouse_dist_left   = u_mouse.x;
    float mouse_dist_right  = u_screen_size.x - u_mouse.x;
    float mouse_dist_top    = u_mouse.y;
    float mouse_dist_bottom = u_screen_size.y - u_mouse.y;
    float mouse_min = min(min(mouse_dist_left, mouse_dist_right), min(mouse_dist_top, mouse_dist_bottom));

    // Determine which edge the mouse is closest to, only glow on that edge
    float edge_dist = u_glow_width; // default: no glow
    if (mouse_min < u_glow_width) {
        if (mouse_min == mouse_dist_left)        edge_dist = dist_left;
        else if (mouse_min == mouse_dist_right)   edge_dist = dist_right;
        else if (mouse_min == mouse_dist_top)     edge_dist = dist_top;
        else                                      edge_dist = dist_bottom;
    }

    float alpha = 1.0 - smoothstep(0.0, u_glow_width, edge_dist);
    alpha *= alpha; // softer falloff
    // Also fade based on mouse proximity to edge
    float mouse_factor = 1.0 - smoothstep(0.0, u_glow_width, mouse_min);
    alpha *= mouse_factor;

    frag_color = vec4(u_glow_color.rgb, u_glow_color.a * alpha);
}
"#;

// ---------------------------------------------------------------------------
// Magnifier post-process shader (extends postprocess with magnifier)
// ---------------------------------------------------------------------------

pub const MAGNIFIER_POSTPROCESS_FRAGMENT_SHADER: &str = r#"#version 330 core

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
uniform float u_magnifier_radius;  // in normalized coords
uniform float u_magnifier_zoom;    // zoom factor (e.g. 2.0)
// Colorblind correction uniform
uniform int   u_colorblind_mode;   // 0=none, 1=deuteranopia, 2=protanopia, 3=tritanopia
in vec2 v_uv;
out vec4 frag_color;

void main() {
    vec2 sample_uv = v_uv;

    // Magnifier effect
    if (u_magnifier_enabled == 1) {
        vec2 diff = v_uv - u_magnifier_center;
        float dist = length(diff);
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

    // Magnifier border ring
    if (u_magnifier_enabled == 1) {
        vec2 diff = v_uv - u_magnifier_center;
        float dist = length(diff);
        float ring = abs(dist - u_magnifier_radius);
        float ring_width = 0.002;
        float ring_alpha = 1.0 - smoothstep(0.0, ring_width, ring);
        c.rgb = mix(c.rgb, vec3(0.8, 0.8, 0.8), ring_alpha * 0.8);
    }

    frag_color = c;
}
"#;

// ---------------------------------------------------------------------------
// Window 3D tilt vertex shader
// ---------------------------------------------------------------------------

pub const TILT_VERTEX_SHADER: &str = r#"#version 330 core

uniform vec4 u_rect;       // x, y, w, h in pixels
uniform mat4 u_projection; // orthographic projection
uniform vec2 u_tilt;       // tilt angles (x, y) in radians

out vec2 v_uv;

void main() {
    vec2 pos = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    v_uv = pos;

    // Compute pixel position
    vec2 pixel = u_rect.xy + pos * u_rect.zw;

    // Apply 3D tilt: compute center of the window
    vec2 center = u_rect.xy + u_rect.zw * 0.5;
    vec2 rel = pixel - center;

    // Simple perspective tilt
    float z = rel.x * sin(u_tilt.y) + rel.y * sin(u_tilt.x);
    float perspective = 1.0 / (1.0 - z * 0.0003);

    pixel = center + rel * perspective;

    gl_Position = u_projection * vec4(pixel, 0.0, 1.0);
}
"#;

// ---------------------------------------------------------------------------
// Wobbly windows vertex shader (NxN grid with corner offsets)
// ---------------------------------------------------------------------------

pub const WOBBLY_VERTEX_SHADER: &str = r#"#version 330 core

uniform vec4 u_rect;       // x, y, w, h in pixels
uniform mat4 u_projection;
uniform vec2 u_corner_offsets[4]; // TL, TR, BL, BR corner displacements
uniform int  u_grid_size;  // grid subdivisions (e.g. 8)

out vec2 v_uv;

void main() {
    int grid = u_grid_size;
    int quad_id = gl_VertexID / 6;
    int vert_in_quad = gl_VertexID % 6;

    int col = quad_id % grid;
    int row = quad_id / grid;

    // Triangle strip indices for a quad: 0,1,2, 2,1,3
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

    // Bilinear interpolation of corner offsets
    vec2 tl = u_corner_offsets[0];
    vec2 tr = u_corner_offsets[1];
    vec2 bl = u_corner_offsets[2];
    vec2 br = u_corner_offsets[3];

    vec2 offset = mix(mix(tl, tr, fx), mix(bl, br, fx), fy);

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
    frag_color = vec4(v_color.rgb, alpha);
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
    frag_color = vec4(color, alpha);
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

    // Bezier-like genie deformation
    float t = u_progress;

    // Bottom rows converge to dock position, top rows follow more slowly
    float row_t = fy; // 0 = top, 1 = bottom

    // Horizontal squeeze: width narrows toward dock
    float center_x = u_rect.x + u_rect.z * 0.5;
    float target_x = mix(center_x, u_dock_pos.x, t * row_t);
    float half_w = u_rect.z * 0.5 * mix(1.0, 0.02, t * row_t);
    float px = target_x + (fx - 0.5) * half_w * 2.0;

    // Vertical: bottom converges to dock_y, top stays then follows
    float py = mix(u_rect.y + fy * u_rect.w, u_dock_pos.y, t * row_t * row_t);

    gl_Position = u_projection * vec4(px, py, 0.0, 1.0);
}
"#;

// Genie uses the same fragment shader as windows (FRAGMENT_SHADER)

