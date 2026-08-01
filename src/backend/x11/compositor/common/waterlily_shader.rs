//! High-quality WaterLily rain-on-glass optics and the volumetric tank.
//!
//! Kept separate from the legacy shader bundle so the physically based rain path
//! can evolve without destabilising unrelated compositor effects.  The producer
//! still sends the version-1 RGBA8 frame contract: inverted alpha is optical
//! height and RGB remains the authored material/contact tint.  Version-2
//! volumetric frames render through the dedicated ray-marching shader below.

pub(super) const WATERLILY_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform sampler2D u_texture;
uniform sampler2D u_scene_texture;
uniform int u_scene_available;
uniform vec2 u_screen_size;
uniform float u_opacity;
in vec2 v_uv;
out vec4 frag_color;

const float PI = 3.14159265358979323846;
const float WATER_IOR = 1.333;
const float WATER_F0 = 0.0203731878; // ((1.0 - 1.333) / (1.0 + 1.333))^2
const int REFRACTION_TAPS = 24;

vec2 clamp_scene_uv(vec2 uv) {
    // Half a pixel keeps linear filtering away from the undefined framebuffer edge.
    vec2 guard_uv = 0.5 / max(u_screen_size, vec2(1.0));
    return clamp(uv, guard_uv, vec2(1.0) - guard_uv);
}

vec3 frosted_scene(vec2 uv) {
    // Stable 9x9 Gaussian-like convolution. Fogged glass needs a broad low-frequency
    // transmission lobe; a single narrow blur reads like ordinary compositor blur.
    vec2 step_uv = vec2(2.75) / max(u_screen_size, vec2(1.0));
    vec3 sum = vec3(0.0);
    float total = 0.0;
    for (int y = -4; y <= 4; ++y) {
        for (int x = -4; x <= 4; ++x) {
            vec2 p = vec2(float(x), float(y));
            float weight = exp(-dot(p, p) * 0.18);
            // Tap a prefiltered mip at the tap spacing: sparse taps of the
            // full-resolution scene aliased wallpaper grain and text into
            // per-pixel speckle wherever water transmitted the desktop.
            sum += textureLod(
                u_scene_texture,
                clamp_scene_uv(uv + p * step_uv),
                2.0
            ).rgb * weight;
            total += weight;
        }
    }
    return sum / max(total, 1e-5);
}

// Catmull-Rom bicubic upsampling as nine bilinear fetches. The producer can run
// below output resolution while the optical thickness field remains smooth enough
// for stable derivatives and refraction.
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
    return clamp(result, 0.0, 1.0);
}

float optical_height(vec2 uv) {
    return max(1.0 - texture(u_texture, clamp(uv, vec2(0.0), vec2(1.0))).a, 0.0);
}

vec3 sample_refraction_cone(vec2 center_uv, vec2 major_axis, float radius_px) {
    // A deterministic Vogel disk is the same stable cone-tracing pattern used by
    // real-time engines for glossy transmission. It avoids temporal glitter while
    // integrating a finite roughness lobe instead of taking one brittle scene tap.
    vec2 axis = length(major_axis) > 1e-5 ? normalize(major_axis) : vec2(1.0, 0.0);
    vec2 tangent = vec2(-axis.y, axis.x);
    vec2 inv_screen = 1.0 / max(u_screen_size, vec2(1.0));

    vec3 sum = texture(u_scene_texture, clamp_scene_uv(center_uv)).rgb * 2.0;
    float total = 2.0;
    for (int i = 0; i < REFRACTION_TAPS; ++i) {
        float fi = float(i) + 0.5;
        float radial = sqrt(fi / float(REFRACTION_TAPS));
        float angle = fi * 2.39996322972865332;
        vec2 disk = vec2(cos(angle), sin(angle)) * radial;
        vec2 ellipse = axis * disk.x * 1.28 + tangent * disk.y * 0.82;
        float weight = 1.0 - 0.55 * radial;
        vec2 sample_uv = center_uv + ellipse * radius_px * inv_screen;
        sum += texture(u_scene_texture, clamp_scene_uv(sample_uv)).rgb * weight;
        total += weight;
    }
    return sum / max(total, 1e-5);
}

float distribution_ggx(float n_dot_h, float roughness) {
    float a = roughness * roughness;
    float a2 = a * a;
    float d = n_dot_h * n_dot_h * (a2 - 1.0) + 1.0;
    return a2 / max(PI * d * d, 1e-5);
}

float geometry_schlick_ggx(float n_dot_v, float roughness) {
    float r = roughness + 1.0;
    float k = (r * r) * 0.125;
    return n_dot_v / max(n_dot_v * (1.0 - k) + k, 1e-5);
}

float geometry_smith(float n_dot_v, float n_dot_l, float roughness) {
    return geometry_schlick_ggx(n_dot_v, roughness)
         * geometry_schlick_ggx(n_dot_l, roughness);
}

float fresnel_schlick(float cos_theta) {
    float one_minus = 1.0 - clamp(cos_theta, 0.0, 1.0);
    return WATER_F0 + (1.0 - WATER_F0) * pow(one_minus, 5.0);
}

void main() {
    vec4 simulation = sample_simulation(v_uv);

    // Translucent producer pixels carry water height in inverted alpha. Rebuild a
    // smooth height field, derive a surface normal, and trace a finite transmission
    // cone through the already-rendered scene. This is an engine-style screen-space
    // optical pass: physically based and scene reactive, without pretending that an
    // OpenGL compositor with no scene geometry owns a hardware ray-tracing AS.
    if (u_scene_available == 1 && simulation.a < 0.97) {
        vec2 texel = 1.0 / vec2(textureSize(u_texture, 0));

        float h_tl = optical_height(v_uv + texel * vec2(-1.0, -1.0));
        float h_tc = optical_height(v_uv + texel * vec2( 0.0, -1.0));
        float h_tr = optical_height(v_uv + texel * vec2( 1.0, -1.0));
        float h_ml = optical_height(v_uv + texel * vec2(-1.0,  0.0));
        float h_mc = max(1.0 - simulation.a, 0.0);
        float h_mr = optical_height(v_uv + texel * vec2( 1.0,  0.0));
        float h_bl = optical_height(v_uv + texel * vec2(-1.0,  1.0));
        float h_bc = optical_height(v_uv + texel * vec2( 0.0,  1.0));
        float h_br = optical_height(v_uv + texel * vec2( 1.0,  1.0));

        // Sobel gradient in producer coordinates. Producer rows are top-to-bottom
        // while framebuffer coordinates are bottom-to-top, hence the Y sign flip.
        float dh_dx = ((h_tr + 2.0 * h_mr + h_br)
                     - (h_tl + 2.0 * h_ml + h_bl)) * 0.125;
        float dh_dy_down = ((h_bl + 2.0 * h_bc + h_br)
                          - (h_tl + 2.0 * h_tc + h_tr)) * 0.125;
        vec2 gradient = vec2(dh_dx, -dh_dy_down);

        // Height Hessian feeds the thin-lens Jacobian. Its determinant estimates
        // ray convergence/divergence, yielding subtle caustic brightening without a
        // fake animated texture.
        float dxx = h_ml - 2.0 * h_mc + h_mr;
        float dyy = h_tc - 2.0 * h_mc + h_bc;
        float dxy = -0.25 * (h_br - h_bl - h_tr + h_tl);
        float curvature = dxx + dyy;
        float slope_energy = length(gradient);
        float surface_activity = clamp(
            smoothstep(0.0025, 0.055, slope_energy)
          + 0.55 * smoothstep(0.004, 0.080, abs(curvature)),
            0.0,
            1.0
        );

        vec2 screen_uv = gl_FragCoord.xy / max(u_screen_size, vec2(1.0));
        vec3 scene_center = texture(u_scene_texture, clamp_scene_uv(screen_uv)).rgb;

        // Preserve the v1 contract exactly on a flat alpha field. Besides keeping
        // old producers compatible, this makes broad wet trails optically quiet while
        // only their meniscus bends light.
        vec3 legacy_lens = mix(scene_center, simulation.rgb, simulation.a);
        if (surface_activity <= 1e-5) {
            float layer_opacity = clamp(u_opacity, 0.0, 1.0);
            frag_color = vec4(legacy_lens * layer_opacity, layer_opacity);
            return;
        }

        vec3 normal = normalize(vec3(-gradient * 6.4, 1.0));
        vec3 view_dir = vec3(0.0, 0.0, 1.0);
        vec3 incident = -view_dir;
        vec3 transmitted_ray = refract(incident, normal, 1.0 / WATER_IOR);
        vec2 bend = transmitted_ray.xy / max(-transmitted_ray.z, 0.12);

        float height_shape = smoothstep(0.02, 0.86, h_mc);
        float optical_path_px = mix(7.0, 46.0, height_shape);
        vec2 refract_px = bend * optical_path_px;
        vec2 refract_uv = refract_px / max(u_screen_size, vec2(1.0));

        // Roughness grows around complex contact lines but remains low on a clean
        // spherical cap. Chromatic dispersion is deliberately subtle: water is not
        // a prism, yet high-contrast desktop edges should split by a fraction of a
        // pixel at the steepest rim.
        float roughness = clamp(
            0.028 + 0.15 * smoothstep(0.010, 0.090, abs(curvature))
                  + 0.045 * smoothstep(0.080, 0.260, slope_energy),
            0.025,
            0.24
        );
        float cone_radius_px = 0.35 + roughness * 13.0 + length(refract_px) * 0.035;
        float dispersion = mix(0.0045, 0.015, smoothstep(0.04, 0.24, slope_energy));

        vec3 red_cone = sample_refraction_cone(
            screen_uv + refract_uv * (1.0 - dispersion), bend, cone_radius_px
        );
        vec3 green_cone = sample_refraction_cone(
            screen_uv + refract_uv, bend, cone_radius_px
        );
        vec3 blue_cone = sample_refraction_cone(
            screen_uv + refract_uv * (1.0 + dispersion), bend, cone_radius_px
        );
        vec3 transmitted = vec3(red_cone.r, green_cone.g, blue_cone.b);

        // Beer-Lambert absorption for a very short water path. The coefficients are
        // intentionally small; the result is neutral glass with only a faint cool
        // bias instead of dyed blue droplets.
        vec3 extinction = vec3(0.030, 0.014, 0.006);
        transmitted *= exp(-extinction * (0.35 + 2.65 * h_mc));

        // Thin-lens Jacobian in screen pixels. Clamp the determinant near folds so
        // caustics brighten rather than exploding into fireflies.
        vec2 screen_per_texel = max(
            u_screen_size / vec2(textureSize(u_texture, 0)),
            vec2(1.0)
        );
        float lens_gain = optical_path_px * (1.0 - 1.0 / WATER_IOR) * 1.8;
        float jxx = 1.0 + lens_gain * dxx / screen_per_texel.x;
        float jyy = 1.0 + lens_gain * dyy / screen_per_texel.y;
        float jxy = lens_gain * dxy / screen_per_texel.y;
        float jyx = lens_gain * dxy / screen_per_texel.x;
        float jacobian_det = jxx * jyy - jxy * jyx;
        float caustic = clamp(
            pow(1.0 / max(abs(jacobian_det), 0.28), 0.32),
            0.72,
            1.42
        );
        transmitted *= mix(1.0, caustic, surface_activity * 0.72);

        // Contact-line extinction and Fresnel reflection create the dark outer rim
        // and bright grazing edge observed on real glass, driven by the same normal
        // rather than hand-painted rings.
        float rim = smoothstep(0.075, 0.285, slope_energy);
        transmitted *= 1.0 - 0.20 * rim * surface_activity;

        float n_dot_v = max(dot(normal, view_dir), 0.0);
        float fresnel = fresnel_schlick(n_dot_v);
        vec2 reflected_offset = reflect(incident, normal).xy * 18.0
                              / max(u_screen_size, vec2(1.0));
        vec3 reflection_proxy = sample_refraction_cone(
            screen_uv + reflected_offset,
            normal.xy,
            1.2 + roughness * 10.0
        );
        transmitted = mix(
            transmitted,
            reflection_proxy,
            fresnel * surface_activity * 0.78
        );

        // GGX softbox highlight. A fixed virtual studio light is preferable to a
        // baked white dot: it moves naturally across every evolving normal and its
        // energy is modulated by the real scene luminance.
        vec3 light_dir = normalize(vec3(-0.42, 0.52, 0.74));
        vec3 half_dir = normalize(light_dir + view_dir);
        float n_dot_l = max(dot(normal, light_dir), 0.0);
        float n_dot_h = max(dot(normal, half_dir), 0.0);
        float v_dot_h = max(dot(view_dir, half_dir), 0.0);
        float d = distribution_ggx(n_dot_h, roughness);
        float g = geometry_smith(n_dot_v, n_dot_l, roughness);
        float f = fresnel_schlick(v_dot_h);
        float specular = d * g * f / max(4.0 * n_dot_v * n_dot_l, 1e-4);
        float scene_luma = dot(scene_center, vec3(0.2126, 0.7152, 0.0722));
        vec3 softbox_color = vec3(1.0, 0.975, 0.91)
                           * (0.20 + 0.80 * max(scene_luma, 0.12));
        transmitted += softbox_color
                     * min(specular, 2.5)
                     * n_dot_l
                     * surface_activity
                     * 0.58;

        // Retain the producer tint as a material/contact mask, but let active lens
        // regions be dominated by scene-derived optics. Opaque rim pixels still
        // preserve the authored color and anti-aliasing coverage.
        float producer_mix = simulation.a * mix(1.0, 0.34, surface_activity);
        vec3 physical_lens = mix(transmitted, simulation.rgb, producer_mix);
        vec3 lens = mix(legacy_lens, clamp(physical_lens, 0.0, 1.0), surface_activity);

        float layer_opacity = clamp(u_opacity, 0.0, 1.0);
        frag_color = vec4(lens * layer_opacity, layer_opacity);
        return;
    }

    // Opaque near-white pixels represent mist. Key only bright low-chroma pixels
    // so ordinary WaterLily flow colors remain untouched.
    float high = max(simulation.r, max(simulation.g, simulation.b));
    float low = min(simulation.r, min(simulation.g, simulation.b));
    float chroma = high - low;
    float bright_white = smoothstep(0.82, 0.985, low);
    float neutral = 1.0 - smoothstep(0.025, 0.12, chroma);
    float backdrop_mix = clamp(bright_white * neutral, 0.0, 1.0);

    vec3 backdrop = vec3(0.0);
    float backdrop_alpha = 0.0;
    if (u_scene_available == 1) {
        vec2 screen_uv = gl_FragCoord.xy / max(u_screen_size, vec2(1.0));
        backdrop = frosted_scene(screen_uv);
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

/// Native 3D display of version-2 volumetric frames: a perspective camera
/// ray-marches the RGBA volume with front-to-back emission/absorption,
/// reconstructs surface normals from the 3D density gradient, and applies
/// lighting, self-shadowing, Fresnel response, and scene refraction.  A true
/// 3D solve (the jellyfish smack) therefore shows parallax, occlusion, and
/// view-dependent material depth instead of a baked planar projection.
///
/// World conventions shared with the producer contract and the CPU-side
/// camera: the volume box is centered at the origin; +X is screen right and
/// maps to the frame width, +Y is up and maps to the per-slice rows (flipped,
/// rows are top-left), and +Z runs front (slice 0, nearest the resting
/// camera) to back. Voxel alpha is authored as the opacity a ray accumulates
/// crossing one voxel, so the marcher renormalizes it by its actual step
/// length and the image stays invariant under sample-count changes.
///
/// Reconstruction and shading: a one-tap trilinear probe classifies each
/// step as clear water or material, so the expensive work only runs inside
/// the smack.  Material steps resample the volume with a C2-continuous
/// tricubic B-spline (eight hardware trilinear taps) and take its analytic
/// derivative for the surface normal, replacing the old five-tap
/// screen-space footprint whose direction-dependent blur smeared the bells
/// into fog.  Voxel opacity splits the producer's authored material bands:
/// low-alpha turbulent wake shades as a forward-scattering medium
/// (Henyey-Greenstein lobe) in its own palette hue, while high-alpha tissue
/// is sharpened into a crisp translucent surface with wrapped diffuse,
/// subsurface backlight, Fresnel rim, and specular response.  The ambient
/// floor is proportional to the voxel's own albedo, never a flat gray, so
/// stacked layers converge to luminous color instead of white haze.
pub(super) const WATERLILY_VOLUME_FRAGMENT_SHADER: &str = r#"#version 330 core

uniform sampler3D u_volume;
uniform sampler2D u_scene_texture;
uniform int u_scene_available;
uniform vec2 u_screen_size;
uniform float u_opacity;
uniform vec3 u_camera_position;
uniform vec3 u_camera_right;
uniform vec3 u_camera_up;
uniform vec3 u_camera_forward;
uniform float u_tan_half_fov;
uniform vec3 u_box_half_extents;
// Phase in [0, 1) of the shared surface-wave loop.  Every wave frequency
// below is an integer number of cycles per loop, so the open-water swell
// animates continuously and wraps seamlessly; like the camera pose it is
// derived from the frame timestamp, keeping re-renders of an unchanged
// frame bit-stable for damage tracking.
uniform float u_time;

in vec2 v_uv;
out vec4 frag_color;

const int MAX_STEPS = 176;
const int SHADOW_STEPS = 4;
const float TAU = 6.28318530717958647692;
// Producer material bands, matching the jelly worker's authored voxel
// opacities: turbulent wake publishes below ~0.10 while bell tissue, oral
// arms, and gonads publish about 0.28-0.35.  The gap lets the transfer
// function sharpen tissue into a translucent surface without touching the
// wispy wake, and a fully opaque voxel still maps to full opacity.
const float WAKE_ALPHA_CEILING = 0.10;
const float TISSUE_ALPHA_KNEE = 0.30;
// The aquarium is deliberately a little clearer than the full-screen planar
// frost.  It now has a real projected silhouette, so water need not obscure
// the desktop just to make the effect's extent legible.
const float WATER_BACKDROP_ALPHA = 0.42;
// World-space height of the open water surface.  Because the box is centred,
// 0.88 leaves a narrow air gap (six percent of the full tank height) below the
// top glass rim.
const float WATER_LEVEL_RATIO = 0.88;
const float WATER_F0 = 0.0203731878;

vec2 clamp_scene_uv(vec2 uv) {
    vec2 guard_uv = 0.5 / max(u_screen_size, vec2(1.0));
    return clamp(uv, guard_uv, vec2(1.0) - guard_uv);
}

vec3 frosted_scene(vec2 uv) {
    // Same broad transmission lobe as the planar WaterLily backdrop so
    // switching between planar and volumetric cases keeps one glass look.
    vec2 step_uv = vec2(2.75) / max(u_screen_size, vec2(1.0));
    vec3 sum = vec3(0.0);
    float total = 0.0;
    for (int y = -4; y <= 4; ++y) {
        for (int x = -4; x <= 4; ++x) {
            vec2 p = vec2(float(x), float(y));
            float weight = exp(-dot(p, p) * 0.18);
            // Tap a prefiltered mip at the tap spacing: sparse taps of the
            // full-resolution scene aliased wallpaper grain and text into
            // per-pixel speckle wherever water transmitted the desktop.
            sum += textureLod(
                u_scene_texture,
                clamp_scene_uv(uv + p * step_uv),
                2.0
            ).rgb * weight;
            total += weight;
        }
    }
    return sum / max(total, 1e-5);
}

// Slab intersection with the volume box. IEEE infinities from axis-parallel
// rays flow through min/max correctly.
vec2 box_span(vec3 origin, vec3 direction) {
    vec3 inverse = 1.0 / direction;
    vec3 near = (-u_box_half_extents - origin) * inverse;
    vec3 far = (u_box_half_extents - origin) * inverse;
    vec3 lower = min(near, far);
    vec3 upper = max(near, far);
    return vec2(
        max(max(lower.x, lower.y), lower.z),
        min(min(upper.x, upper.y), upper.z)
    );
}

// Intersect the already-clipped tank chord with the half-space below the
// water surface.  Keeping this in world space means camera yaw/elevation do
// not turn the top of the water into a screen-aligned special effect.
vec2 water_span(
    vec3 origin,
    vec3 direction,
    float tank_entry,
    float tank_exit
) {
    float surface_y = u_box_half_extents.y * WATER_LEVEL_RATIO;
    if (abs(direction.y) < 1e-6) {
        float y = (origin + direction * tank_entry).y;
        return y <= surface_y
            ? vec2(tank_entry, tank_exit)
            : vec2(tank_exit, tank_entry);
    }
    float surface_t = (surface_y - origin.y) / direction.y;
    if (direction.y < 0.0) {
        return vec2(max(tank_entry, surface_t), tank_exit);
    }
    return vec2(tank_entry, min(tank_exit, surface_t));
}

// Outward normal of the AABB face nearest a point on the box.  This drives
// actual view-dependent glass Fresnel instead of a 2D rectangular border.
vec3 box_face_normal(vec3 position) {
    vec3 q = abs(position) / max(u_box_half_extents, vec3(1e-5));
    if (q.x >= q.y && q.x >= q.z) {
        return vec3(position.x < 0.0 ? -1.0 : 1.0, 0.0, 0.0);
    }
    if (q.y >= q.z) {
        return vec3(0.0, position.y < 0.0 ? -1.0 : 1.0, 0.0);
    }
    return vec3(0.0, 0.0, position.z < 0.0 ? -1.0 : 1.0);
}

// Highlight the two tangent-axis boundaries of a hit face.  Evaluating this
// at both entry and exit points exposes the front and back rim with correct
// perspective, which is what makes the almost-full-screen box read as a 3D
// aquarium rather than another translucent fullscreen filter.
float box_edge_mask(vec3 position, vec3 face_normal) {
    vec3 q = abs(position) / max(u_box_half_extents, vec3(1e-5));
    vec3 tangent_q = q * (vec3(1.0) - abs(face_normal));
    float edge = max(tangent_q.x, max(tangent_q.y, tangent_q.z));
    return smoothstep(0.970, 0.996, edge);
}

vec3 world_to_texture(vec3 position) {
    vec3 tex = position / (2.0 * u_box_half_extents) + 0.5;
    // Producer rows have a top-left origin while world +Y points upward.
    tex.y = 1.0 - tex.y;
    return tex;
}

bool inside_texture(vec3 tex) {
    return all(greaterThanEqual(tex, vec3(0.0)))
        && all(lessThanEqual(tex, vec3(1.0)));
}

float density_at(vec3 tex) {
    return texture(u_volume, clamp(tex, vec3(0.0), vec3(1.0))).a;
}

// Tissue-band density: raw opacity with the whole wake band subtracted
// away.  Normals and self-shadowing must never see the turbulent wake: its
// rough voxel field sits inside the reconstruction support of any tissue
// that swims through its own wake, and letting it perturb the gradient
// speckled the lighting of exactly those bells while bells in clear water
// stayed clean.
float tissue_density_at(vec3 tex) {
    return max(density_at(tex) - 0.14, 0.0);
}

bool finite_vec3(vec3 value) {
    return !any(isnan(value)) && !any(isinf(value));
}

// One axis of the uniform cubic B-spline, folded into two linear lobes so
// the hardware's trilinear filter evaluates four control points per axis
// with two taps (Sigg & Hadwiger).  Each lobe is (combined weight, sample
// offset from the base texel in texel units).
void bspline_value_lobes(float f, out vec2 lobe0, out vec2 lobe1) {
    float g = 1.0 - f;
    float w0 = g * g * g / 6.0;
    float w1 = (4.0 - 6.0 * f * f + 3.0 * f * f * f) / 6.0;
    float w2 = (1.0 + 3.0 * (f + f * f - f * f * f)) / 6.0;
    float w3 = f * f * f / 6.0;
    float s0 = w0 + w1;
    float s1 = w2 + w3;
    lobe0 = vec2(s0, -1.0 + w1 / s0);
    lobe1 = vec2(s1, 1.0 + w3 / s1);
}

// Derivative weights of the same B-spline.  The left pair is negative and
// the right pair positive over the whole cell, so each pair again folds
// into one signed trilinear tap and the derivative stays exact.
void bspline_derivative_lobes(float f, out vec2 lobe0, out vec2 lobe1) {
    float g = 1.0 - f;
    float d0 = -0.5 * g * g;
    float d1 = (3.0 * f * f - 4.0 * f) * 0.5;
    float d2 = (1.0 + 2.0 * f - 3.0 * f * f) * 0.5;
    float d3 = 0.5 * f * f;
    float s0 = d0 + d1;
    float s1 = d2 + d3;
    lobe0 = vec2(s0, -1.0 + d1 / s0);
    lobe1 = vec2(s1, 1.0 + d3 / s1);
}

// C2-continuous tricubic resampling of the RGBA volume as eight hardware
// trilinear taps.  Compared with the old five-tap screen-space footprint it
// has no direction-dependent blur and no trilinear diamond artifacts, and
// the reconstructed field is smooth enough to give the analytic gradient
// below clean, glitter-free surface normals on a 64-cell-tall solve blown
// up to the whole desktop.
vec4 sample_volume_tricubic(vec3 tex) {
    vec3 size = vec3(textureSize(u_volume, 0));
    vec3 inv_size = 1.0 / size;
    vec3 coord = tex * size - 0.5;
    vec3 base = floor(coord);
    vec3 f = coord - base;
    vec2 lx0; vec2 lx1; vec2 ly0; vec2 ly1; vec2 lz0; vec2 lz1;
    bspline_value_lobes(f.x, lx0, lx1);
    bspline_value_lobes(f.y, ly0, ly1);
    bspline_value_lobes(f.z, lz0, lz1);
    vec3 texel = base + 0.5;
    float x0 = (texel.x + lx0.y) * inv_size.x;
    float x1 = (texel.x + lx1.y) * inv_size.x;
    float y0 = (texel.y + ly0.y) * inv_size.y;
    float y1 = (texel.y + ly1.y) * inv_size.y;
    float z0 = (texel.z + lz0.y) * inv_size.z;
    float z1 = (texel.z + lz1.y) * inv_size.z;
    vec4 front =
          ly0.x * (lx0.x * texture(u_volume, vec3(x0, y0, z0))
                 + lx1.x * texture(u_volume, vec3(x1, y0, z0)))
        + ly1.x * (lx0.x * texture(u_volume, vec3(x0, y1, z0))
                 + lx1.x * texture(u_volume, vec3(x1, y1, z0)));
    vec4 back =
          ly0.x * (lx0.x * texture(u_volume, vec3(x0, y0, z1))
                 + lx1.x * texture(u_volume, vec3(x1, y0, z1)))
        + ly1.x * (lx0.x * texture(u_volume, vec3(x0, y1, z1))
                 + lx1.x * texture(u_volume, vec3(x1, y1, z1)));
    return lz0.x * front + lz1.x * back;
}

// Analytic world-space gradient of the tricubic density field: derivative
// lobes along one axis, value lobes along the other two, eight taps per
// axis.  Solver cells are cubic in world space so per-texel derivatives are
// a uniform scale of the world gradient; the Y sign flips because texture
// rows run down while world +Y points up.
vec3 tricubic_alpha_gradient(vec3 tex) {
    vec3 size = vec3(textureSize(u_volume, 0));
    vec3 inv_size = 1.0 / size;
    vec3 coord = tex * size - 0.5;
    vec3 base = floor(coord);
    vec3 f = coord - base;
    vec2 vx0; vec2 vx1; vec2 vy0; vec2 vy1; vec2 vz0; vec2 vz1;
    bspline_value_lobes(f.x, vx0, vx1);
    bspline_value_lobes(f.y, vy0, vy1);
    bspline_value_lobes(f.z, vz0, vz1);
    vec2 dx0; vec2 dx1; vec2 dy0; vec2 dy1; vec2 dz0; vec2 dz1;
    bspline_derivative_lobes(f.x, dx0, dx1);
    bspline_derivative_lobes(f.y, dy0, dy1);
    bspline_derivative_lobes(f.z, dz0, dz1);
    vec3 texel = base + 0.5;
    float xv0 = (texel.x + vx0.y) * inv_size.x;
    float xv1 = (texel.x + vx1.y) * inv_size.x;
    float yv0 = (texel.y + vy0.y) * inv_size.y;
    float yv1 = (texel.y + vy1.y) * inv_size.y;
    float zv0 = (texel.z + vz0.y) * inv_size.z;
    float zv1 = (texel.z + vz1.y) * inv_size.z;
    float xd0 = (texel.x + dx0.y) * inv_size.x;
    float xd1 = (texel.x + dx1.y) * inv_size.x;
    float yd0 = (texel.y + dy0.y) * inv_size.y;
    float yd1 = (texel.y + dy1.y) * inv_size.y;
    float zd0 = (texel.z + dz0.y) * inv_size.z;
    float zd1 = (texel.z + dz1.y) * inv_size.z;

    float gx =
          vz0.x * (vy0.x * (dx0.x * tissue_density_at(vec3(xd0, yv0, zv0))
                          + dx1.x * tissue_density_at(vec3(xd1, yv0, zv0)))
                 + vy1.x * (dx0.x * tissue_density_at(vec3(xd0, yv1, zv0))
                          + dx1.x * tissue_density_at(vec3(xd1, yv1, zv0))))
        + vz1.x * (vy0.x * (dx0.x * tissue_density_at(vec3(xd0, yv0, zv1))
                          + dx1.x * tissue_density_at(vec3(xd1, yv0, zv1)))
                 + vy1.x * (dx0.x * tissue_density_at(vec3(xd0, yv1, zv1))
                          + dx1.x * tissue_density_at(vec3(xd1, yv1, zv1))));
    float gy =
          vz0.x * (dy0.x * (vx0.x * tissue_density_at(vec3(xv0, yd0, zv0))
                          + vx1.x * tissue_density_at(vec3(xv1, yd0, zv0)))
                 + dy1.x * (vx0.x * tissue_density_at(vec3(xv0, yd1, zv0))
                          + vx1.x * tissue_density_at(vec3(xv1, yd1, zv0))))
        + vz1.x * (dy0.x * (vx0.x * tissue_density_at(vec3(xv0, yd0, zv1))
                          + vx1.x * tissue_density_at(vec3(xv1, yd0, zv1)))
                 + dy1.x * (vx0.x * tissue_density_at(vec3(xv0, yd1, zv1))
                          + vx1.x * tissue_density_at(vec3(xv1, yd1, zv1))));
    float gz =
          dz0.x * (vy0.x * (vx0.x * tissue_density_at(vec3(xv0, yv0, zd0))
                          + vx1.x * tissue_density_at(vec3(xv1, yv0, zd0)))
                 + vy1.x * (vx0.x * tissue_density_at(vec3(xv0, yv1, zd0))
                          + vx1.x * tissue_density_at(vec3(xv1, yv1, zd0))))
        + dz1.x * (vy0.x * (vx0.x * tissue_density_at(vec3(xv0, yv0, zd1))
                          + vx1.x * tissue_density_at(vec3(xv1, yv0, zd1)))
                 + vy1.x * (vx0.x * tissue_density_at(vec3(xv0, yv1, zd1))
                          + vx1.x * tissue_density_at(vec3(xv1, yv1, zd1))));
    return vec3(gx, -gy, gz);
}

// The producer separates material bands by voxel opacity.  Pass the wake
// band through untouched, steepen the tissue coverage ramp so bells,
// arms, and gonads read as crisp surfaces after desktop magnification, and
// stay identity above the knee so a fully opaque voxel remains opaque.
float shape_material_alpha(float raw) {
    float sharpened = 0.34 * smoothstep(
        WAKE_ALPHA_CEILING,
        TISSUE_ALPHA_KNEE,
        raw
    );
    float passthrough = raw * step(0.34, raw);
    return max(min(raw, WAKE_ALPHA_CEILING), max(sharpened, passthrough));
}

// Henyey-Greenstein phase lobe for the turbulent wake: forward scattering
// makes shed vortex rings glow when the ray runs with the light instead of
// painting them a flat gray.
float wake_phase(float cos_scatter) {
    float g = 0.55;
    float denom = 1.0 + g * g - 2.0 * g * cos_scatter;
    return (1.0 - g * g) / max(denom * sqrt(denom), 1e-3);
}

// A short secondary march toward the key light supplies contact and
// self-shadowing inside the bell.  It is evaluated only at density
// boundaries, keeping empty-water and low-gradient wake samples cheap.
float light_visibility(vec3 position, vec3 light_direction, float voxel_length) {
    float optical_depth = 0.0;
    vec3 p = position;
    for (int i = 0; i < SHADOW_STEPS; ++i) {
        p += light_direction * voxel_length * (1.35 + 0.35 * float(i));
        vec3 tex = world_to_texture(p);
        if (!inside_texture(tex)) {
            break;
        }
        float alpha = tissue_density_at(tex);
        optical_depth += -log(max(1.0 - alpha, 0.035));
    }
    return exp(-0.48 * optical_depth);
}

void main() {
    vec2 ndc = vec2(v_uv.x * 2.0 - 1.0, 1.0 - v_uv.y * 2.0);
    float aspect = u_screen_size.x / max(u_screen_size.y, 1.0);
    vec3 ray = normalize(
        u_camera_forward
        + ndc.x * u_tan_half_fov * aspect * u_camera_right
        + ndc.y * u_tan_half_fov * u_camera_up
    );

    vec3 accumulated = vec3(0.0);
    float coverage = 0.0;
    vec3 front_normal_sum = vec3(0.0);
    float front_surface_weight = 0.0;
    float front_depth_sum = 0.0;

    vec2 tank_span = box_span(u_camera_position, ray);
    float tank_entry = max(tank_span.x, 0.0);
    float tank_exit = tank_span.y;
    // The old path frosted every pixel even when its ray missed the volume.
    // A transparent miss gives the aquarium a projected outline and leaves
    // the desktop around it sharp.
    if (tank_exit <= tank_entry) {
        frag_color = vec4(0.0);
        return;
    }

    vec3 tank_entry_position = u_camera_position + ray * tank_entry;
    vec3 tank_exit_position = u_camera_position + ray * tank_exit;
    vec3 tank_entry_normal = box_face_normal(tank_entry_position);
    vec3 tank_exit_normal = box_face_normal(tank_exit_position);
    float front_edge = box_edge_mask(tank_entry_position, tank_entry_normal);
    float back_edge = box_edge_mask(tank_exit_position, tank_exit_normal);

    vec2 wet_span = water_span(
        u_camera_position,
        ray,
        tank_entry,
        tank_exit
    );
    float entry = max(wet_span.x, tank_entry);
    float exit = min(wet_span.y, tank_exit);
    float diagonal = 2.0 * length(u_box_half_extents);
    float chord = max(exit - entry, 0.0);
    float reference = 2.0 * u_box_half_extents.x
                    / float(textureSize(u_volume, 0).x);
    if (chord > reference * 0.25) {
        // Sample in voxel units rather than as a fraction of the box
        // diagonal, at 1.5 samples per voxel: the one-tap water probe below
        // made material steps rare enough to afford both the higher rate and
        // the larger step budget, which together resolve membrane boundaries
        // without noise on long diagonal chords.
        int steps = int(clamp(
            ceil(chord / reference * 1.5),
            16.0,
            float(MAX_STEPS)
        ));
        float step_length = chord / float(steps);
        // Spatially coherent midpoint integration, deliberately unjittered:
        // per-pixel sample-depth jitter was tried and reintroduced the
        // salt-and-pepper speckle on the voxel-rough wake field that the
        // coherent marcher had originally eliminated.  The C2 tricubic
        // reconstruction is smooth enough that slab banding is not visible
        // at 1.5 samples per voxel.
        // One voxel of straight-through travel is the producer's opacity
        // reference length; voxels are cubic, so any axis gives it.
        float t = entry + 0.5 * step_length;
        vec3 light_direction = normalize(vec3(-0.46, 0.78, -0.42));
        vec3 view_direction = -ray;
        float forward_scatter = wake_phase(dot(-light_direction, ray));

        for (int i = 0; i < MAX_STEPS; ++i) {
            if (i >= steps || coverage > 0.985) {
                break;
            }
            vec3 position = u_camera_position + ray * t;
            vec3 tex = world_to_texture(position);
            // Most of the tank is clear water and one trilinear tap decides
            // that; the eight-tap reconstruction, the twenty-four-tap
            // gradient, and the lighting only run inside material.  The
            // threshold must stay BELOW half the producer's smallest
            // nonzero byte (1/255): the probe is trilinear while shading is
            // tricubic, so a higher cut skips samples whose tricubic value
            // is far above it, and that skip/shade flip between adjacent
            // rays accumulated into black speckle along the wake's vortex
            // rings.  At 0.002 the probe only skips where every trilinear
            // neighbour is zero, and the tricubic tail it can lose there is
            // bounded by ~0.003 — invisible, and empty water still costs
            // one tap.
            if (texture(u_volume, tex).a < 0.002) {
                t += step_length;
                continue;
            }
            vec4 voxel = sample_volume_tricubic(tex);
            float shaped = shape_material_alpha(voxel.a);
            float alpha = 1.0 - pow(
                max(1.0 - shaped, 0.0),
                step_length / reference
            );

            // Voxel opacity splits the authored material bands: low-alpha
            // wake stays a scattering medium, high-alpha tissue shades as a
            // translucent surface with a real reconstructed normal.  The
            // ramp starts strictly ABOVE the wake band's ceiling (the
            // producer publishes wake at 0.112 or less and the B-spline
            // reconstruction cannot overshoot): letting dense vortex cores
            // lean into the surface-shaded branch lit them with normals
            // from the rough vorticity field, and that per-pixel lighting
            // jitter was the black speckle tracing every wake ring.
            float material = smoothstep(0.16, 0.26, voxel.a);
            vec3 normal = view_direction;
            float boundary = 0.0;
            if (material > 0.02) {
                vec3 gradient = tricubic_alpha_gradient(tex);
                float gradient_length = length(gradient);
                boundary = smoothstep(0.012, 0.14, gradient_length);
                if (gradient_length > 1e-5) {
                    normal = -gradient / gradient_length;
                    if (dot(normal, view_direction) < 0.0) {
                        normal = -normal;
                    }
                    // A near-zero gradient (the flat middle of the shell
                    // profile) has a noisy direction; fade such normals
                    // toward the view vector instead of letting them
                    // speckle the lighting.  Both inputs are unit vectors
                    // in the same hemisphere, so the mix cannot collapse.
                    normal = normalize(mix(view_direction, normal, boundary));
                }
            }

            float n_dot_v = max(dot(normal, view_direction), 0.0);
            vec3 half_vector = light_direction + view_direction;
            float half_length = length(half_vector);
            vec3 half_direction = half_length > 1e-5
                ? half_vector / half_length
                : view_direction;
            float n_dot_h = max(dot(normal, half_direction), 0.0);
            float fresnel = WATER_F0
                + (1.0 - WATER_F0) * pow(1.0 - n_dot_v, 5.0);
            float specular = pow(n_dot_h, 54.0) * (0.22 + 1.8 * fresnel);
            // Gate the shadow march on the alpha-derived material weight,
            // which is uniform across the whole shell thickness.  Gating on
            // the gradient-derived boundary shadowed only the two edge
            // bands of the shell profile and painted concentric onion
            // rings across every bell dome.
            float raw_visibility = material > 0.25
                ? light_visibility(position, light_direction, reference)
                : 1.0;
            // Jelly tissue is translucent and strongly forward scattering;
            // self-shadowing modulates it but must never crush it to black.
            float visibility = mix(1.0, max(raw_visibility, 0.52), 0.42);

            // Sunlight fades with depth below the water surface and
            // view-path haze softens distant layers, so the same voxel
            // changes brightness as the camera drifts around it.
            float depth_light = mix(
                0.88,
                1.10,
                clamp(position.y / (2.0 * u_box_half_extents.y) + 0.5, 0.0, 1.0)
            );
            float haze = exp(-0.75 * (t - entry) / diagonal);
            // Trust the producer's authored color; only a slight lift keeps
            // dense crossings from going muddy.  The previous 38% white wash
            // was a large part of the cotton-wool look on screenshots.
            vec3 albedo = mix(voxel.rgb, vec3(0.93, 0.96, 1.0), 0.12);

            // Wake: forward-scattering medium in its own palette hue, whose
            // shed rings glow when the ray runs with the key light.
            vec3 wake_emission = albedo * depth_light
                * (0.38 + 0.60 * forward_scatter * mix(0.7, 1.0, haze));

            // Tissue: wrapped two-sided diffuse with subsurface backlight,
            // Fresnel rim, and a tight specular from the key light.
            float wrapped_light = clamp(
                (dot(normal, light_direction) + 0.5) / 1.5,
                0.0,
                1.0
            );
            float back_light = pow(
                max(dot(-normal, light_direction), 0.0),
                2.5
            );
            float rim = pow(1.0 - n_dot_v, 3.0) * boundary;
            vec3 tissue_emission =
                albedo * depth_light
                    * (0.58 + 0.65 * wrapped_light * visibility
                       + 0.35 * back_light)
                + vec3(0.88, 0.94, 1.0) * rim * 0.30
                + vec3(1.0, 0.97, 0.92) * specular * visibility * 0.85;

            vec3 emission = mix(wake_emission, tissue_emission, material);
            // Never let one undefined driver result (for example a
            // degenerate half vector at an exact camera/light alignment)
            // poison the complete ray through NaN propagation.
            if (!finite_vec3(emission)) {
                emission = albedo;
            }
            // Multiple-scattered ambient floor proportional to the voxel's
            // own albedo: hue-preserving, unlike the old flat gray floor
            // that turned every wake wisp into white fog over the desktop.
            emission = max(emission, albedo * 0.26);

            float contribution = (1.0 - coverage) * alpha;
            accumulated += contribution * emission;
            coverage += contribution;
            // Capture the first reliable tissue interface for refraction.
            // First-hit state is the physically correct interface for scene
            // transmission, and it avoids averaging the opposing normals of
            // a translucent shell into a degenerate vector whose
            // normalization produced NaN texture coordinates.
            if (front_surface_weight <= 1e-5
                && material > 0.25
                && boundary > 0.05
                && contribution > 0.0005) {
                front_normal_sum = normal;
                front_depth_sum = (t - entry) / max(chord, 1e-5);
                front_surface_weight = boundary * alpha;
            }
            t += step_length;
        }
    }

    // Refract the captured desktop only through the water actually crossed by
    // this ray.  Beer-Lambert attenuation gives the tank a subtle cyan depth
    // cue while preserving enough contrast for normal desktop work beneath it.
    vec3 water_backdrop = vec3(0.0);
    float water_backdrop_alpha = 0.0;
    vec2 screen_uv = gl_FragCoord.xy / max(u_screen_size, vec2(1.0));
    vec2 normal_screen = vec2(0.0);
    float wet_fraction = clamp(
        chord / max(tank_exit - tank_entry, 1e-5),
        0.0,
        1.0
    );
    if (u_scene_available == 1 && wet_fraction > 1e-4) {
        vec2 refracted_uv = screen_uv;
        float reflection_mix = 0.0;
        float front_normal_length = length(front_normal_sum);
        if (front_surface_weight > 1e-4 && front_normal_length > 1e-4) {
            // Explicit division stays finite on every GLSL implementation;
            // the length guard above prevents normalize(vec3(0)).
            vec3 front_normal = front_normal_sum / front_normal_length;
            normal_screen = vec2(
                dot(front_normal, u_camera_right),
                dot(front_normal, u_camera_up)
            );
            float front_depth = front_depth_sum;
            float n_dot_v = max(dot(front_normal, -ray), 0.0);
            float bend_px = mix(4.0, 22.0, 1.0 - n_dot_v)
                          * mix(1.0, 0.48, clamp(front_depth, 0.0, 1.0));
            refracted_uv += normal_screen * bend_px
                          / max(u_screen_size, vec2(1.0));
            float fresnel = WATER_F0
                + (1.0 - WATER_F0) * pow(1.0 - n_dot_v, 5.0);
            reflection_mix = fresnel * smoothstep(
                0.01,
                0.28,
                front_surface_weight
            );
        }
        vec3 transmission = frosted_scene(refracted_uv);
        // A gently prefiltered tap: a full-resolution one re-imported the
        // desktop's pixel-level texture as noise on every tissue surface.
        vec3 reflection = textureLod(
            u_scene_texture,
            clamp_scene_uv(screen_uv - normal_screen * 0.010),
            1.5
        ).rgb;
        transmission = mix(transmission, reflection, reflection_mix * 0.68);

        float optical_depth = clamp(chord / diagonal * 2.4, 0.0, 1.4);
        vec3 transmittance = exp(
            -vec3(0.24, 0.075, 0.026) * (0.28 + optical_depth)
        );
        vec3 deep_water = vec3(0.025, 0.145, 0.175);
        transmission = transmission * transmittance
                     + deep_water * (vec3(1.0) - transmittance);
        // Fade continuously from the waterline.  Starting immediately at a
        // fixed 72% opacity made a one-sample wet chord pop into a hard band.
        water_backdrop_alpha = WATER_BACKDROP_ALPHA
                             * smoothstep(0.0, 0.18, wet_fraction);
        water_backdrop = transmission * water_backdrop_alpha;
    } else if (wet_fraction > 1e-4) {
        // A headless/no-snapshot consumer still gets a recognisable water
        // volume rather than an otherwise invisible empty box.
        water_backdrop_alpha = 0.16 * wet_fraction;
        water_backdrop = vec3(0.018, 0.105, 0.135) * water_backdrop_alpha;
    }

    // The open water plane is real world-space geometry.  At grazing angles
    // it catches a broad reflection; near its intersection with the four tank
    // walls it becomes the clearly visible water line.  Three traveling
    // sine waves animate a gentle open-water swell whose crests catch
    // moving glints, driven by the frame timestamp through u_time.
    float surface_strength = 0.0;
    if (abs(ray.y) > 1e-6) {
        float surface_y = u_box_half_extents.y * WATER_LEVEL_RATIO;
        float surface_t = (surface_y - u_camera_position.y) / ray.y;
        if (surface_t >= tank_entry && surface_t <= tank_exit) {
            vec3 surface_position = u_camera_position + ray * surface_t;
            vec2 surface_q = abs(surface_position.xz)
                           / max(u_box_half_extents.xz, vec2(1e-5));
            if (all(lessThanEqual(surface_q, vec2(1.001)))) {
                vec2 span = surface_position.xz
                          / max(u_box_half_extents.xz, vec2(1e-5));
                float wave_phase = TAU * u_time;
                float swell = sin(dot(span, vec2(9.1, 4.7))
                                  + wave_phase * 13.0)
                            + 0.6 * sin(dot(span, vec2(-5.3, 7.9))
                                        + wave_phase * 21.0)
                            + 0.45 * sin(dot(span, vec2(3.7, -11.3))
                                         + wave_phase * 34.0);
                swell *= 0.487;
                float glint = smoothstep(0.35, 0.95, swell);
                float surface_edge = smoothstep(
                    0.955,
                    0.996,
                    max(surface_q.x, surface_q.y)
                );
                float surface_fresnel = WATER_F0
                    + (1.0 - WATER_F0) * pow(1.0 - abs(ray.y), 5.0);
                surface_strength = (0.014
                                    + 0.11 * surface_fresnel
                                    + 0.26 * surface_edge)
                                 * (0.80 + 0.28 * swell)
                                 + 0.10 * glint
                                     * (0.35 + 0.65 * surface_fresnel);
            }
        }
    }

    coverage = clamp(coverage, 0.0, 1.0);
    if (!finite_vec3(accumulated)) {
        accumulated = coverage * vec3(0.78, 0.84, 0.92);
    }
    if (!finite_vec3(water_backdrop)) {
        water_backdrop = vec3(0.0);
        water_backdrop_alpha = 0.0;
    }

    // Back rim lies behind the volume and is naturally occluded by a jelly.
    // The front pane/rim and water surface are then composited over it.  All
    // terms remain premultiplied to match the compositor blend state.
    float rear_glass_alpha = 0.20 * back_edge;
    vec3 rear_glass_color = mix(
        vec3(0.10, 0.55, 0.62),
        vec3(0.78, 0.98, 1.0),
        back_edge
    );
    vec3 behind = rear_glass_color * rear_glass_alpha
                + (1.0 - rear_glass_alpha) * water_backdrop;
    float behind_alpha = rear_glass_alpha
                       + (1.0 - rear_glass_alpha) * water_backdrop_alpha;

    vec3 premultiplied = accumulated + (1.0 - coverage) * behind;
    float alpha = coverage + (1.0 - coverage) * behind_alpha;

    float glass_n_dot_v = abs(dot(tank_entry_normal, -ray));
    float glass_fresnel = WATER_F0
        + (1.0 - WATER_F0) * pow(1.0 - glass_n_dot_v, 5.0);
    float front_glass_alpha = clamp(
        0.012 + 0.11 * glass_fresnel
      + 0.46 * front_edge
      + surface_strength,
        0.0,
        0.72
    );
    float glass_highlight = clamp(
        0.18 + 0.82 * max(front_edge, surface_strength * 2.0),
        0.0,
        1.0
    );
    vec3 front_glass_color = mix(
        vec3(0.10, 0.48, 0.56),
        vec3(0.88, 1.0, 0.98),
        glass_highlight
    );
    premultiplied = front_glass_color * front_glass_alpha
                  + premultiplied * (1.0 - front_glass_alpha);
    alpha = front_glass_alpha + alpha * (1.0 - front_glass_alpha);

    float layer_opacity = clamp(u_opacity, 0.0, 1.0);
    frag_color = vec4(premultiplied * layer_opacity, alpha * layer_opacity);
}
"#;
