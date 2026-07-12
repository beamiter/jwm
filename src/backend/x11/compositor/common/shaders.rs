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
        vec2 center = u_uv_rect.xy + u_uv_rect.zw * 0.5;
        vec2 delta = uv - center;
        float dist = length(delta);
        float t = u_ripple_progress;
        float wave_front = t * 0.8;
        float ring = sin((dist - wave_front) * 30.0)
                   * u_ripple_amplitude
                   * (1.0 - t)                                     // fade over time
                   * smoothstep(0.0, 0.05, dist)                   // calm at center
                   * (1.0 - smoothstep(wave_front, wave_front + 0.15, dist)); // only near wave front
        uv += normalize(delta + vec2(0.001)) * ring;
    }

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
    frag_color = vec4(u_bg_color.rgb, alpha * mask);
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
    frag_color = u_color;
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
// Magnifier post-process shader (extends postprocess with magnifier)
// ---------------------------------------------------------------------------

pub const SLIME_WAVE_SIM_FRAGMENT_SHADER: &str = r#"#version 330 core

// Non-linear shallow-water state:
// R=surface height, G=horizontal velocity X, B=horizontal velocity Y, A=foam.
uniform sampler2D u_state;
uniform vec2 u_texel;
uniform float u_aspect;
uniform float u_time_step;
uniform float u_turbulence;
uniform float u_foam;
uniform int u_injection_count;
uniform vec4 u_injections[10]; // previous.xy, current.xy in top-left UV space
uniform vec2 u_injection_params[10]; // radius, normalized gesture impulse
in vec2 v_uv;
out vec4 frag_color;

vec4 fluid_sample(vec2 uv) {
    return texture(
        u_state,
        clamp(uv, u_texel * 1.5, vec2(1.0) - u_texel * 1.5)
    );
}

vec2 closest_on_segment(vec2 p, vec2 a, vec2 b) {
    vec2 ba = b - a;
    float h = clamp(dot(p - a, ba) / max(dot(ba, ba), 0.000001), 0.0, 1.0);
    return a + ba * h;
}

void main() {
    vec2 uv = v_uv;
    vec2 axial = vec2(u_texel.x, 0.0);
    vec2 vertical = vec2(0.0, u_texel.y);

    // Velocity is stored in grid cells/second, so multiplying by texel size turns
    // it into UV displacement for the semi-Lagrangian backtrace.
    vec4 current = fluid_sample(uv);
    vec2 backtrace = uv - current.gb * u_texel * u_time_step;
    vec4 state = fluid_sample(backtrace);
    vec4 left = fluid_sample(backtrace - axial);
    vec4 right = fluid_sample(backtrace + axial);
    vec4 top = fluid_sample(backtrace - vertical);
    vec4 bottom = fluid_sample(backtrace + vertical);

    float height = state.r;
    vec2 velocity = state.gb;
    float foam = state.a;

    vec2 gradient_height = 0.5 * vec2(
        right.r - left.r,
        bottom.r - top.r
    );
    float divergence = 0.5 * (
        right.g - left.g + bottom.b - top.b
    );
    float laplacian_height =
        left.r + right.r + top.r + bottom.r - 4.0 * height;
    vec2 laplacian_velocity =
        left.gb + right.gb + top.gb + bottom.gb - 4.0 * velocity;

    const float wave_speed = 27.0; // grid cells / second; CFL ~= 0.23 at 120 Hz
    const float drag = 1.65;
    const float viscosity = 0.72;
    velocity += u_time_step * (
        -wave_speed * wave_speed * gradient_height
        + viscosity * laplacian_velocity
    );
    velocity /= 1.0 + drag * u_time_step;
    height += u_time_step * (-divergence + 0.85 * laplacian_height);
    height /= 1.0 + 0.12 * u_time_step;

    // Vorticity confinement restores small rotating structures that ordinary
    // semi-Lagrangian advection would otherwise dissipate.
    float curl = 0.5 * (
        right.b - left.b - bottom.g + top.g
    );
    vec2 curl_gradient = vec2(
        abs(right.b) - abs(left.b),
        abs(bottom.g) - abs(top.g)
    );
    float curl_gradient_length = length(curl_gradient);
    if (curl_gradient_length > 0.00001) {
        vec2 normal = curl_gradient / curl_gradient_length;
        velocity += vec2(normal.y, -normal.x)
            * curl * (5.5 * u_turbulence) * u_time_step;
    }

    vec2 p = vec2(uv.x * u_aspect, uv.y);
    for (int index = 0; index < 10; ++index) {
        if (index >= u_injection_count) {
            break;
        }

        vec4 segment = u_injections[index];
        vec2 a = vec2(segment.x * u_aspect, segment.y);
        vec2 b = vec2(segment.z * u_aspect, segment.w);
        float radius = u_injection_params[index].x;
        float force = u_injection_params[index].y;
        vec2 closest = closest_on_segment(p, a, b);
        vec2 radial = p - closest;
        float normalized_distance = length(radial) / max(radius, 0.00001);
        float r2 = normalized_distance * normalized_distance;
        float cutoff = 1.0 - smoothstep(1.10, 1.48, normalized_distance);
        float wake = exp(-2.2 * r2) * cutoff;
        float displacement = (1.0 - 1.5 * r2) * exp(-1.5 * r2) * cutoff;

        vec2 gesture_cells = (segment.zw - segment.xy) / max(u_texel, vec2(0.000001));
        float gesture_length = length(gesture_cells);
        vec2 gesture_direction = gesture_length > 0.00001
            ? gesture_cells / gesture_length
            : vec2(1.0, 0.0);
        vec2 swirl_direction = vec2(-gesture_direction.y, gesture_direction.x);
        vec2 aspect_direction = normalize(b - a + vec2(0.000001, 0.0));
        float side = sign(
            aspect_direction.x * radial.y - aspect_direction.y * radial.x
        );
        side = side == 0.0 ? 1.0 : side;

        velocity += gesture_direction * wake * force * 52.0;
        velocity += swirl_direction * side * wake * force
            * (14.0 * u_turbulence);
        height += displacement * force * 0.035;
        foam = max(foam, wake * force * 0.35 * u_foam);
    }

    float slope = length(gradient_height);
    float compression = max(-divergence, 0.0);
    float breaking = smoothstep(0.018, 0.11, slope) * 0.52
        + smoothstep(1.5, 8.0, abs(curl)) * 0.34
        + smoothstep(1.0, 6.0, compression) * 0.38;
    foam = max(
        foam * exp(-1.05 * u_time_step),
        clamp(breaking * u_foam, 0.0, 1.0)
    );

    float edge_distance = min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y));
    // Open, lossy boundaries prevent waves and vortices reflecting from the
    // edge of the simulation texture.
    float edge = smoothstep(0.0, 0.055, edge_distance);
    velocity *= mix(0.62, 1.0, edge);
    height *= mix(0.78, 1.0, edge);
    foam *= mix(0.86, 1.0, edge);

    frag_color = vec4(
        clamp(height, -0.45, 0.45),
        clamp(velocity, vec2(-72.0), vec2(72.0)),
        clamp(foam, 0.0, 1.0)
    );
}
"#;

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
// Slime hand-refraction uniforms. Points and bbox use top-left screen pixels.
uniform int   u_slime_enabled;
uniform vec2  u_slime_points[21];
uniform float u_slime_depths[21];  // MediaPipe relative Z; negative is closer
uniform vec4  u_slime_bbox;        // min_x, min_y, max_x, max_y
uniform vec4  u_slime_surface_rect; // target window in top-left screen pixels
uniform vec2  u_slime_screen_size;
uniform float u_slime_scale;       // palm scale in pixels
uniform float u_slime_strength;    // refraction displacement in pixels
uniform float u_slime_ocean_strength;
uniform float u_slime_turbulence_strength;
uniform float u_slime_foam_strength;
uniform float u_slime_opacity;
uniform float u_slime_time;
uniform sampler2D u_slime_wave;
uniform vec2  u_slime_wave_texel;
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

float slime_tapered_capsule_sdf(
    vec2 p, vec2 a, vec2 b, float radius_a, float radius_b
) {
    vec2 pa = p - a;
    vec2 ba = b - a;
    float h = clamp(dot(pa, ba) / max(dot(ba, ba), 0.0001), 0.0, 1.0);
    return length(pa - ba * h) - mix(radius_a, radius_b, h);
}

float slime_smooth_union(float a, float b, float radius) {
    float h = clamp(0.5 + 0.5 * (b - a) / max(radius, 0.0001), 0.0, 1.0);
    return mix(b, a, h) - radius * h * (1.0 - h);
}

float slime_surface_mask_at(vec2 p) {
    float feather = 7.0;
    // A target touching a screen edge needs no transition there. This makes a
    // desktop-wide surface cover every pixel while preserving soft window edges.
    float left = u_slime_surface_rect.x <= 0.5
        ? 1.0
        : smoothstep(u_slime_surface_rect.x, u_slime_surface_rect.x + feather, p.x);
    float top = u_slime_surface_rect.y <= 0.5
        ? 1.0
        : smoothstep(u_slime_surface_rect.y, u_slime_surface_rect.y + feather, p.y);
    float right = u_slime_surface_rect.z >= u_slime_screen_size.x - 0.5
        ? 1.0
        : 1.0 - smoothstep(
            u_slime_surface_rect.z - feather, u_slime_surface_rect.z, p.x
        );
    float bottom = u_slime_surface_rect.w >= u_slime_screen_size.y - 0.5
        ? 1.0
        : 1.0 - smoothstep(
            u_slime_surface_rect.w - feather, u_slime_surface_rect.w, p.y
        );
    return left * top * right * bottom;
}

float slime_depth_radius(int index) {
    // MediaPipe Z is relative to the wrist. Closer (negative) points become
    // thicker, while farther points shrink, giving the analytic hand real depth.
    return clamp(1.0 - u_slime_depths[index] * 0.85, 0.72, 1.45);
}

float slime_palm_depth_radius() {
    return (
        slime_depth_radius(0)
        + slime_depth_radius(5)
        + slime_depth_radius(9)
        + slime_depth_radius(13)
        + slime_depth_radius(17)
    ) / 5.0;
}

float slime_hand_sdf(vec2 p) {
    float radius = max(u_slime_scale * 0.115, 3.0);
    float d = 100000.0;

    // Five finger chains. MediaPipe landmarks are laid out as four joints per
    // finger after the wrist: thumb 1..4, index 5..8, ... pinky 17..20.
    for (int finger = 0; finger < 5; ++finger) {
        int base = 1 + finger * 4;
        float width = finger == 0 ? 1.14
            : (finger == 1 ? 1.0
            : (finger == 2 ? 1.04 : (finger == 3 ? 0.96 : 0.84)));
        float finger_radius = radius * width;
        for (int joint = 0; joint < 3; ++joint) {
            int index_a = base + joint;
            int index_b = index_a + 1;
            float taper_a = 1.0 - float(joint) * 0.12;
            float taper_b = 1.0 - float(joint + 1) * 0.12;
            float segment = slime_tapered_capsule_sdf(
                p,
                u_slime_points[index_a],
                u_slime_points[index_b],
                finger_radius * taper_a * slime_depth_radius(index_a),
                finger_radius * taper_b * slime_depth_radius(index_b)
            );
            d = slime_smooth_union(d, segment, radius * 0.48);
        }
    }

    // Fill the palm with soft metaballs. Endpoint radii inherit landmark depth,
    // so a hand rotating toward the camera changes silhouette smoothly.
    float palm_blend = radius * 1.05 * slime_palm_depth_radius();
    d = slime_smooth_union(d, slime_tapered_capsule_sdf(
        p, u_slime_points[5], u_slime_points[9],
        radius * 1.58 * slime_depth_radius(5),
        radius * 1.68 * slime_depth_radius(9)
    ), palm_blend);
    d = slime_smooth_union(d, slime_tapered_capsule_sdf(
        p, u_slime_points[9], u_slime_points[13],
        radius * 1.68 * slime_depth_radius(9),
        radius * 1.62 * slime_depth_radius(13)
    ), palm_blend);
    d = slime_smooth_union(d, slime_tapered_capsule_sdf(
        p, u_slime_points[13], u_slime_points[17],
        radius * 1.62 * slime_depth_radius(13),
        radius * 1.48 * slime_depth_radius(17)
    ), palm_blend);
    d = slime_smooth_union(d, slime_tapered_capsule_sdf(
        p, u_slime_points[0], u_slime_points[9],
        radius * 1.82 * slime_depth_radius(0),
        radius * 2.10 * slime_depth_radius(9)
    ), palm_blend);
    d = slime_smooth_union(d, slime_tapered_capsule_sdf(
        p, u_slime_points[5], u_slime_points[17],
        radius * 1.86 * slime_depth_radius(5),
        radius * 1.72 * slime_depth_radius(17)
    ), palm_blend);
    vec2 palm_center = (
        u_slime_points[0] + u_slime_points[5] + u_slime_points[9]
        + u_slime_points[13] + u_slime_points[17]
    ) / 5.0;
    d = slime_smooth_union(
        d,
        length(p - palm_center)
            - radius * 2.34 * slime_palm_depth_radius(),
        palm_blend
    );
    return d;
}

float slime_surface_sdf(vec2 p) {
    float d = slime_hand_sdf(p);
    float scale = max(u_slime_scale, 1.0);
    float wave = sin(p.x / scale * 4.1 + u_slime_time * 1.55)
        + sin(p.y / scale * 3.6 - u_slime_time * 1.20);
    float near_edge = 1.0 - smoothstep(scale * 0.06, scale * 0.32, abs(d));
    return d + wave * scale * 0.009 * near_edge;
}

// Returns height, Sobel-compatible gradient X/Y, and axial curvature.
vec4 slime_ocean_profile(vec2 pixel) {
    vec2 surface_size = max(
        u_slime_surface_rect.zw - u_slime_surface_rect.xy,
        vec2(1.0)
    );
    vec2 local = (pixel - u_slime_surface_rect.xy) / surface_size - 0.5;
    float surface_aspect = surface_size.x / surface_size.y;
    vec2 p = vec2(local.x * surface_aspect, local.y);

    vec2 d0 = normalize(vec2(1.0, 0.18));
    vec2 d1 = normalize(vec2(0.57, 0.82));
    vec2 d2 = normalize(vec2(-0.31, 0.95));
    float f0 = 8.5;
    float f1 = 14.5;
    float f2 = 25.0;
    float phase0 = dot(p, d0) * f0 - u_slime_time * 1.20;
    float phase1 = dot(p, d1) * f1 - u_slime_time * 1.86 + 1.7;
    float phase2 = dot(p, d2) * f2 - u_slime_time * 2.75 + 4.1;
    float a0 = 0.105;
    float a1 = 0.052;
    float a2 = 0.024;

    float height = a0 * sin(phase0)
        + a1 * sin(phase1)
        + a2 * sin(phase2);
    vec2 derivative = d0 * (a0 * f0 * cos(phase0))
        + d1 * (a1 * f1 * cos(phase1))
        + d2 * (a2 * f2 * cos(phase2));
    float second_derivative = -a0 * f0 * f0 * sin(phase0)
        - a1 * f1 * f1 * sin(phase1)
        - a2 * f2 * f2 * sin(phase2);

    vec2 grid_pixels = u_slime_screen_size * u_slime_wave_texel;
    vec2 pixel_gradient = derivative / surface_size.y;
    vec2 sobel_gradient = pixel_gradient * grid_pixels * 8.0;
    float cell_scale = grid_pixels.y / surface_size.y;
    float curvature = second_derivative * cell_scale * cell_scale * 0.25;
    return vec4(height, sobel_gradient, curvature);
}

void main() {
    // FBO textures have Y=0 at bottom, but scene was rendered with top-left-origin
    // projection, so flip V to correct the vertical orientation.
    vec2 uv = vec2(v_uv.x, 1.0 - v_uv.y);
    vec2 sample_uv = uv;

    // Magnifier effect
    if (u_magnifier_enabled == 1) {
        vec2 diff = uv - u_magnifier_center;
        float dist = length(diff);
        if (dist < u_magnifier_radius) {
            // Zoom into the area around the center
            sample_uv = u_magnifier_center + diff / u_magnifier_zoom;
        }
    }

    vec4 c = texture(u_texture, sample_uv);
    vec2 slime_pixel = vec2(
        uv.x * u_slime_screen_size.x,
        (1.0 - uv.y) * u_slime_screen_size.y
    );
    float slime_surface_mask = slime_surface_mask_at(slime_pixel);

    // A softly blurred window-wide film gives the undisturbed area a water
    // surface baseline, so propagated waves read as relief instead of isolated
    // transparent lines.
    if (u_slime_enabled == 1 && slime_surface_mask > 0.001) {
        vec2 blur_step = vec2(
            2.6 / u_slime_screen_size.x,
            2.6 / u_slime_screen_size.y
        );
        vec3 water_skin = texture(u_texture, sample_uv).rgb * 0.20;
        water_skin += texture(u_texture, sample_uv + vec2( blur_step.x, 0.0)).rgb * 0.12;
        water_skin += texture(u_texture, sample_uv + vec2(-blur_step.x, 0.0)).rgb * 0.12;
        water_skin += texture(u_texture, sample_uv + vec2(0.0,  blur_step.y)).rgb * 0.12;
        water_skin += texture(u_texture, sample_uv + vec2(0.0, -blur_step.y)).rgb * 0.12;
        water_skin += texture(u_texture, sample_uv + vec2( blur_step.x,  blur_step.y)).rgb * 0.08;
        water_skin += texture(u_texture, sample_uv + vec2(-blur_step.x,  blur_step.y)).rgb * 0.08;
        water_skin += texture(u_texture, sample_uv + vec2( blur_step.x, -blur_step.y)).rgb * 0.08;
        water_skin += texture(u_texture, sample_uv + vec2(-blur_step.x, -blur_step.y)).rgb * 0.08;
        water_skin *= vec3(0.965, 1.005, 1.035);
        c.rgb = mix(c.rgb, water_skin, slime_surface_mask * 0.64);
    }

    // Waves cover the complete target surface. The hand guide alone uses its
    // tighter CPU bounding box below to avoid evaluating its SDF everywhere.
    if (u_slime_enabled == 1 && slime_surface_mask > 0.001) {
        vec2 pixel = slime_pixel;
        // Pose coordinates are top-left based while the fluid texture is sampled
        // in GL's bottom-left convention.
        vec2 wave_uv = vec2(uv.x, 1.0 - uv.y);
        vec2 tx = vec2(u_slime_wave_texel.x, 0.0);
        vec2 ty = vec2(0.0, u_slime_wave_texel.y);
        vec4 center_state = texture(u_slime_wave, wave_uv);
        float center = center_state.r;
        vec2 flow_velocity = center_state.gb;
        float foam_density = clamp(
            center_state.a * u_slime_foam_strength,
            0.0,
            1.0
        );
        float left = texture(u_slime_wave, wave_uv - tx).r;
        float right = texture(u_slime_wave, wave_uv + tx).r;
        float top = texture(u_slime_wave, wave_uv - ty).r;
        float bottom = texture(u_slime_wave, wave_uv + ty).r;
        float top_left = texture(u_slime_wave, wave_uv - tx - ty).r;
        float top_right = texture(u_slime_wave, wave_uv + tx - ty).r;
        float bottom_left = texture(u_slime_wave, wave_uv - tx + ty).r;
        float bottom_right = texture(u_slime_wave, wave_uv + tx + ty).r;
        vec2 grad = vec2(
            (top_right + 2.0 * right + bottom_right)
                - (top_left + 2.0 * left + bottom_left),
            (bottom_left + 2.0 * bottom + bottom_right)
                - (top_left + 2.0 * top + top_right)
        );
        float lap = (left + right + top + bottom) * 0.25 - center;

        // Analytic directional waves provide the persistent ocean scale while the
        // RGBA16F field supplies local interaction, transport, vortices and foam.
        vec4 ocean = slime_ocean_profile(pixel) * u_slime_ocean_strength;
        center += ocean.x;
        grad += ocean.yz;
        lap += ocean.w;

        float flow_speed = length(flow_velocity);
        float turbulence_gate = smoothstep(1.5, 20.0, flow_speed)
            * u_slime_turbulence_strength;
        float micro_phase = dot(
            pixel / max(u_slime_scale, 1.0),
            vec2(1.73, -1.11)
        ) + u_slime_time * 3.2 + center * 9.0;
        vec2 micro_normal = vec2(
            sin(micro_phase),
            cos(micro_phase * 1.27 + 0.8)
        ) * (0.0014 + min(flow_speed * 0.000035, 0.0028));
        grad += micro_normal * turbulence_gate;

        float grad_len = length(grad);
        vec2 grad_dir = grad_len > 0.00001 ? grad / grad_len : vec2(0.0);
        vec2 refraction_px = grad * (0.72 * u_slime_strength * 92.0);
        vec2 lens_px = grad_dir * lap * (0.26 * u_slime_strength * 230.0);
        vec2 transport_px = clamp(
            flow_velocity,
            vec2(-24.0),
            vec2(24.0)
        ) * (0.018 * u_slime_strength);
        vec2 total_px = refraction_px + lens_px + transport_px;
        vec2 total_offset = vec2(
            total_px.x / u_slime_screen_size.x,
            -total_px.y / u_slime_screen_size.y
        );
        float dispersion = 0.015 + clamp(grad_len * 0.20, 0.0, 0.14);
        vec2 refracted_uv = clamp(
            sample_uv + total_offset,
            vec2(0.001),
            vec2(0.999)
        );
        vec3 liquid;
        liquid.r = texture(u_texture, clamp(
            refracted_uv + total_offset * dispersion,
            vec2(0.001),
            vec2(0.999)
        )).r;
        liquid.g = texture(u_texture, refracted_uv).g;
        liquid.b = texture(u_texture, clamp(
            refracted_uv - total_offset * dispersion,
            vec2(0.001),
            vec2(0.999)
        )).b;

        float activity = smoothstep(
            0.00035,
            0.008,
            grad_len + abs(center) * 0.42 + min(flow_speed * 0.00045, 0.02)
        ) * slime_surface_mask;
        float foam_variation = 0.80 + 0.20 * sin(
            dot(pixel, vec2(0.13, 0.19)) + u_slime_time * 2.7
        );
        float foam_visible = smoothstep(
            0.08,
            0.72,
            foam_density * foam_variation
        ) * slime_surface_mask;
        activity = max(activity, foam_visible * 0.92);

        float wave_brightness = 1.0 + clamp(center * 4.2, -0.55, 0.68);
        liquid *= wave_brightness;
        liquid = mix(
            liquid,
            liquid * vec3(0.72, 0.88, 1.12),
            activity * 0.28
        );

        vec3 normal = normalize(vec3(
            -grad.x,
            -grad.y,
            0.30 / (u_slime_strength + 0.01)
        ));
        float cos_theta = clamp(normal.z, 0.0, 1.0);
        float fresnel = 0.02 + 0.98 * pow(1.0 - cos_theta, 5.0);
        vec2 reflection_uv = clamp(
            sample_uv - total_offset * 1.6,
            vec2(0.001),
            vec2(0.999)
        );
        vec3 reflection = texture(u_texture, reflection_uv).rgb
            * vec3(0.80, 0.90, 1.08);
        liquid = mix(liquid, reflection, fresnel * activity * 0.62);

        vec3 light_dir = normalize(vec3(0.4, 0.7, 1.0));
        float ndoth = max(dot(normal, light_dir), 0.0);
        float specular = pow(ndoth, 180.0) * 2.2
            + pow(ndoth, 28.0) * 0.35;
        float caustic_mask = smoothstep(0.04, 0.10, abs(center))
            * smoothstep(0.01, 0.06, abs(lap));
        liquid += vec3(0.86, 0.95, 1.0)
            * (specular * activity * 1.35 + caustic_mask * 0.58);
        float signed_curvature = clamp(lap * 26.0, -1.0, 1.0) * activity;
        float interference = smoothstep(0.008, 0.045, abs(lap))
            * smoothstep(0.002, 0.018, grad_len);
        liquid += vec3(0.84, 0.94, 1.0)
            * max(signed_curvature, 0.0) * (0.24 + interference * 0.20);
        liquid *= 1.0 - max(-signed_curvature, 0.0)
            * (0.30 + interference * 0.18);
        vec2 ridge_light_dir = normalize(vec2(0.42, 0.91));
        float ridge = clamp(
            dot(grad_dir, ridge_light_dir) * grad_len * 155.0,
            -1.0,
            1.0
        ) * activity;
        float gradient_rim = smoothstep(0.0012, 0.014, grad_len) * activity;
        liquid += vec3(0.86, 0.95, 1.0) * gradient_rim * 0.16;
        liquid += vec3(0.92, 0.98, 1.0) * max(ridge, 0.0) * 0.48;
        liquid *= 1.0 - max(-ridge, 0.0) * 0.58;

        vec3 foam_color = vec3(0.90, 0.97, 1.0);
        liquid = mix(liquid, foam_color, foam_visible * 0.78);
        liquid += foam_color * foam_visible * (0.08 + specular * 0.12);
        c.rgb = mix(c.rgb, liquid, activity);

            // Keep the tracked hand as a quiet visual guide; fingertip ripples
            // are the primary effect and continue after this mask has faded.
            if (u_slime_opacity > 0.0
                && pixel.x >= u_slime_bbox.x && pixel.y >= u_slime_bbox.y
                && pixel.x <= u_slime_bbox.z && pixel.y <= u_slime_bbox.w) {
                float hand_opacity = u_slime_opacity * 0.34;
                float d = slime_surface_sdf(pixel);
                float aa = 2.0;
                float mask = (1.0 - smoothstep(-aa, aa, d)) * hand_opacity;
                if (mask > 0.001) {
                float epsilon = max(1.25, u_slime_scale * 0.018);
                vec2 gradient = vec2(
                    slime_surface_sdf(pixel + vec2(epsilon, 0.0))
                        - slime_surface_sdf(pixel - vec2(epsilon, 0.0)),
                    slime_surface_sdf(pixel + vec2(0.0, epsilon))
                        - slime_surface_sdf(pixel - vec2(0.0, epsilon))
                );
                float gradient_len = length(gradient);
                vec2 normal = gradient_len > 0.0001
                    ? gradient / gradient_len
                    : vec2(0.0, -1.0);

                float depth = clamp(-d / max(u_slime_scale * 0.62, 1.0), 0.0, 1.0);
                float meniscus = 1.0 - smoothstep(
                    0.0, max(u_slime_scale * 0.22, 2.0), max(-d, 0.0)
                );
                vec2 uv_normal = vec2(
                    normal.x / u_slime_screen_size.x,
                    -normal.y / u_slime_screen_size.y
                );
                vec2 uv_tangent = vec2(
                    -normal.y / u_slime_screen_size.x,
                    -normal.x / u_slime_screen_size.y
                );
                float flow = sin(
                    u_slime_time * 1.35
                    + pixel.x / max(u_slime_scale * 0.48, 1.0)
                    - pixel.y / max(u_slime_scale * 0.62, 1.0)
                );
                vec2 offset = uv_normal * u_slime_strength
                    * (0.06 + meniscus * 0.94) * mask
                    + uv_tangent * u_slime_strength * 0.08 * flow * depth * mask;
                vec2 refracted_uv = clamp(sample_uv + offset, vec2(0.001), vec2(0.999));
                vec2 dispersion = uv_normal * u_slime_strength
                    * (0.02 + meniscus * 0.09);

                vec3 glass;
                glass.r = texture(u_texture, clamp(refracted_uv + dispersion, vec2(0.001), vec2(0.999))).r;
                glass.g = texture(u_texture, refracted_uv).g;
                glass.b = texture(u_texture, clamp(refracted_uv - dispersion, vec2(0.001), vec2(0.999))).b;
                glass *= vec3(0.99, 1.008, 1.002);
                c.rgb = mix(c.rgb, glass, mask * 0.82);

                float rim = (1.0 - smoothstep(
                    0.0, max(u_slime_scale * 0.038, 1.5), max(-d, 0.0)
                ))
                    * hand_opacity;
                vec2 light_dir = normalize(vec2(-0.58, -0.82));
                float specular = pow(max(dot(normal, light_dir), 0.0), 18.0);
                float fresnel = pow(meniscus, 2.2);
                float caustic = pow(0.5 + 0.5 * flow, 8.0) * depth;
                c.rgb += vec3(0.82, 0.93, 1.0)
                    * (specular * 0.22 + fresnel * 0.055) * mask;
                c.rgb += vec3(0.34, 0.78, 0.62) * caustic * 0.035 * mask;
                c.rgb = mix(c.rgb, vec3(0.94, 0.98, 1.0), rim * 0.14);
                }
            }
    }

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
    float scale = d / (d - p.z);
    vec2 projected = center + p.xy * scale;

    // Rotated normal (original face normal is [0,0,1])
    v_normal = vec3(sy * cx, -sx, cx * cy);

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
    float a = u_opacity >= 0.0 ? u_opacity : texel.a;

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

    float out_a = u_opacity >= 0.0 ? a : a * u_dim;
    vec3 color = texel.rgb * u_dim * edge_darken + vec3(spec * a);
    frag_color = vec4(color, out_a);
}
"#;

// ---------------------------------------------------------------------------
// Wobbly windows vertex shader (NxN grid with corner offsets)
// ---------------------------------------------------------------------------

pub const WOBBLY_VERTEX_SHADER: &str = r#"#version 330 core

uniform vec4 u_rect;               // x, y, w, h in pixels
uniform mat4 u_projection;
uniform vec2 u_grid_offsets[289];  // up to 17x17 grid node offsets
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
    v_uv = pos;
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
