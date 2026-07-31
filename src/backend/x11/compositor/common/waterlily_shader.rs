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
            sum += texture(u_scene_texture, clamp_scene_uv(uv + p * step_uv)).rgb * weight;
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

in vec2 v_uv;
out vec4 frag_color;

const int MAX_STEPS = 128;
const int SHADOW_STEPS = 4;
// Matches the planar shader's quiescent-water keying: empty tank water shows
// the frosted desktop at the same strength as a near-white 2D frame.
const float FROST_ALPHA = 0.58;
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
            sum += texture(u_scene_texture, clamp_scene_uv(uv + p * step_uv)).rgb * weight;
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

// A screen pixel covers a finite cone in the tank, not an infinitesimal ray.
// Filtering a small cross in the camera plane suppresses the point-cloud
// aliasing produced when thin vortex sheets are magnified from a 96x64x32
// simulation onto a multi-megapixel desktop.  The footprint remains below
// one voxel, preserving bell and tentacle silhouettes.
vec4 sample_volume_footprint(vec3 position, float radius) {
    vec3 right = u_camera_right * radius;
    vec3 up = u_camera_up * radius;
    vec4 center = texture(
        u_volume,
        clamp(world_to_texture(position), vec3(0.0), vec3(1.0))
    );
    vec4 cross_sum =
          texture(u_volume, clamp(world_to_texture(position + right), vec3(0.0), vec3(1.0)))
        + texture(u_volume, clamp(world_to_texture(position - right), vec3(0.0), vec3(1.0)))
        + texture(u_volume, clamp(world_to_texture(position + up), vec3(0.0), vec3(1.0)))
        + texture(u_volume, clamp(world_to_texture(position - up), vec3(0.0), vec3(1.0)));
    return center * 0.52 + cross_sum * 0.12;
}

bool finite_vec3(vec3 value) {
    return !any(isnan(value)) && !any(isinf(value));
}

// Central differences reconstruct a real 3D density normal from six
// neighbouring voxels.  All solver cells are cubic in world space; the Y
// sign is inverted because texture rows run down while world Y runs up.
vec3 density_gradient(vec3 tex) {
    vec3 voxel = 1.0 / vec3(textureSize(u_volume, 0));
    return vec3(
        density_at(tex + vec3(voxel.x, 0.0, 0.0))
            - density_at(tex - vec3(voxel.x, 0.0, 0.0)),
        density_at(tex - vec3(0.0, voxel.y, 0.0))
            - density_at(tex + vec3(0.0, voxel.y, 0.0)),
        density_at(tex + vec3(0.0, 0.0, voxel.z))
            - density_at(tex - vec3(0.0, 0.0, voxel.z))
    );
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
        float alpha = density_at(tex);
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

    vec2 span = box_span(u_camera_position, ray);
    float entry = max(span.x, 0.0);
    if (span.y > entry) {
        float diagonal = 2.0 * length(u_box_half_extents);
        float chord = span.y - entry;
        // Sample in voxel units rather than as a fraction of the box
        // diagonal.  A front view crosses the tank's short axis; the old
        // formula gave it barely a dozen randomly offset samples and turned
        // thin membranes into the black salt-and-pepper pattern visible in
        // screenshots.  Midpoint integration is spatially coherent and 1.35
        // samples per voxel resolves both membrane boundaries without noise.
        float reference = 2.0 * u_box_half_extents.x
                        / float(textureSize(u_volume, 0).x);
        int steps = int(clamp(
            ceil(chord / reference * 1.35),
            16.0,
            float(MAX_STEPS)
        ));
        float step_length = chord / float(steps);
        // One voxel of straight-through travel is the producer's opacity
        // reference length; voxels are cubic, so any axis gives it.
        float t = entry + 0.5 * step_length;
        vec3 light_direction = normalize(vec3(-0.46, 0.78, -0.42));
        vec3 view_direction = -ray;

        for (int i = 0; i < MAX_STEPS; ++i) {
            if (i >= steps || coverage > 0.985) {
                break;
            }
            vec3 position = u_camera_position + ray * t;
            vec3 tex = world_to_texture(position);
            vec4 voxel = sample_volume_footprint(position, reference * 0.42);
            float alpha = 1.0 - pow(
                max(1.0 - voxel.a, 0.0),
                step_length / reference
            );

            // Boundaries of the scalar field behave as translucent material
            // surfaces.  Their 3D gradient supplies a view-dependent normal;
            // homogeneous wake remains a softly scattering volume.
            vec3 gradient = vec3(0.0);
            float boundary = 0.0;
            if (voxel.a > 0.015) {
                gradient = density_gradient(tex);
                boundary = smoothstep(0.018, 0.24, length(gradient));
            }
            vec3 normal = length(gradient) > 1e-5
                ? -normalize(gradient)
                : view_direction;
            if (dot(normal, view_direction) < 0.0) {
                normal = -normal;
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
            float raw_visibility = boundary > 0.08
                ? light_visibility(position, light_direction, reference)
                : 1.0;
            // Jelly tissue is translucent and strongly forward scattering;
            // self-shadowing modulates it but must never crush it to black.
            float visibility = mix(1.0, max(raw_visibility, 0.52), 0.38);

            // Sunlight fades with depth below the water surface and view-path
            // haze desaturates distant layers.  Unlike the old emission-only
            // path, the same voxel changes brightness as the camera orbits it.
            float depth_light = mix(
                0.92,
                1.08,
                clamp(position.y / (2.0 * u_box_half_extents.y) + 0.5, 0.0, 1.0)
            );
            float haze = exp(-0.9 * (t - entry) / diagonal);
            // Jelly tissue is a bright multiple-scattering dielectric, not a
            // painted opaque solid.  Lift its albedo before integration so a
            // long path through many translucent cells converges to luminous
            // colour instead of multiplying into dark stipple on white UI.
            vec3 tissue_color = mix(
                vec3(0.88, 0.94, 1.0),
                voxel.rgb,
                0.62
            );
            vec3 volume_color = mix(
                tissue_color * vec3(0.92, 0.95, 1.0),
                tissue_color,
                haze
            );
            // Wrapped, two-sided diffuse approximates subsurface transport
            // through a thin wet membrane.  It removes the hard Lambertian
            // terminator that previously drew concentric black rings.
            float wrapped_light = clamp(
                (dot(normal, light_direction) + 0.42) / 1.42,
                0.0,
                1.0
            );
            float back_light = pow(
                max(dot(-normal, light_direction), 0.0),
                2.0
            );
            float diffuse = 0.86
                          + 0.18 * wrapped_light * visibility
                          + 0.08 * back_light;
            float rim = pow(1.0 - n_dot_v, 3.0) * boundary;
            vec3 surface_color =
                tissue_color * diffuse * depth_light
                + vec3(0.88, 0.94, 1.0) * rim * 0.18
                + vec3(1.0, 0.96, 0.90) * specular * visibility * 0.72;
            vec3 emission = mix(
                volume_color * depth_light * mix(0.64, 1.0, haze),
                surface_color,
                boundary
            );
            // Never let one undefined driver result (for example a degenerate
            // half vector at an exact camera/light alignment) poison the
            // complete ray through NaN propagation.  The brightness floor is
            // multiple-scattered ambient light, and also prevents dense wake
            // crossings from becoming charcoal on a white desktop.
            if (!finite_vec3(emission)) {
                emission = tissue_color;
            }
            emission = max(emission, vec3(0.66, 0.69, 0.74));

            float contribution = (1.0 - coverage) * alpha;
            accumulated += contribution * emission;
            coverage += contribution;
            // Capture the first reliable density boundary for refraction.
            // Averaging the front and back sides of a translucent shell can
            // cancel their opposing normals to an almost-zero vector; trying
            // to normalize that vector produced NaN texture coordinates and
            // the driver's black salt-and-pepper pixels.  First-hit state is
            // also the physically correct interface for scene transmission.
            if (front_surface_weight <= 1e-5
                && boundary > 0.075
                && contribution > 0.0005) {
                front_normal_sum = normal;
                front_depth_sum = (t - entry) / max(chord, 1e-5);
                front_surface_weight = boundary * alpha;
            }
            t += step_length;
        }
    }

    // Refract the captured desktop through the foremost reconstructed 3D
    // surface.  The bend changes with normal, view angle, and actual hit
    // depth; a small Fresnel reflection remains at grazing angles.
    vec3 frost = vec3(0.0);
    float frost_alpha = 0.0;
    if (u_scene_available == 1) {
        vec2 screen_uv = gl_FragCoord.xy / max(u_screen_size, vec2(1.0));
        vec2 refracted_uv = screen_uv;
        float reflection_mix = 0.0;
        vec2 normal_screen = vec2(0.0);
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
            float bend_px = mix(5.0, 30.0, 1.0 - n_dot_v)
                          * mix(1.0, 0.45, clamp(front_depth, 0.0, 1.0));
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
        vec3 reflection = texture(
            u_scene_texture,
            clamp_scene_uv(screen_uv - normal_screen * 0.012)
        ).rgb;
        frost = mix(transmission, reflection, reflection_mix * 0.72) * FROST_ALPHA;
        frost_alpha = FROST_ALPHA;
    }

    coverage = clamp(coverage, 0.0, 1.0);
    if (!finite_vec3(accumulated)) {
        accumulated = coverage * vec3(0.78, 0.84, 0.92);
    }
    if (!finite_vec3(frost)) {
        frost = vec3(0.0);
        frost_alpha = 0.0;
    }
    vec3 premultiplied = accumulated + (1.0 - coverage) * frost;
    float alpha = coverage + (1.0 - coverage) * frost_alpha;
    float layer_opacity = clamp(u_opacity, 0.0, 1.0);
    frag_color = vec4(premultiplied * layer_opacity, alpha * layer_opacity);
}
"#;
