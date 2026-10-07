//! Resolution-independent translucent jelly anatomy and isolated soft bloom.

pub(super) const JELLY_MESH_VERTEX_SHADER: &str = r#"#version 330 core
layout(location = 0) in vec3 a_position;
layout(location = 1) in vec3 a_normal;
layout(location = 2) in vec4 a_color;
uniform vec3 u_camera_position;
uniform vec3 u_camera_right;
uniform vec3 u_camera_up;
uniform vec3 u_camera_forward;
uniform float u_tan_half_fov;
uniform float u_aspect;
out vec3 v_position;
out vec3 v_normal;
out vec4 v_color;
void main() {
    vec3 relative = a_position - u_camera_position;
    float depth = dot(relative, u_camera_forward);
    gl_Position = vec4(
        dot(relative, u_camera_right) / (u_tan_half_fov * u_aspect),
        dot(relative, u_camera_up) / u_tan_half_fov,
        0.0,
        depth
    );
    v_position = a_position;
    v_normal = a_normal;
    v_color = a_color;
}
"#;

pub(super) const JELLY_MESH_FRAGMENT_SHADER: &str = r#"#version 330 core
uniform vec3 u_camera_position;
uniform vec3 u_box_half_extents;
uniform float u_opacity;
uniform int u_emission_pass;
in vec3 v_position;
in vec3 v_normal;
in vec4 v_color;
out vec4 frag_color;
void main() {
    // Match the volume's tank and submerged waterline, including at resizing
    // and hot switches. Geometry can never leak into the surrounding desktop.
    if (any(greaterThan(abs(v_position), u_box_half_extents))
        || v_position.y > 0.88 * u_box_half_extents.y) {
        discard;
    }
    vec3 view = normalize(u_camera_position - v_position);
    vec3 normal = normalize(v_normal);
    if (!gl_FrontFacing) normal = -normal;
    if (dot(normal, view) < 0.0) normal = -normal;
    float facing = clamp(dot(normal, view), 0.0, 1.0);
    float rim = pow(1.0 - facing, 2.4);
    float key = 0.5 + 0.5 * dot(normal, normalize(vec3(-0.46, 0.78, -0.42)));
    // Authoring separates clear bell film from more substantial ribbons and
    // curves through their actual alpha, without changing the transport.
    float strand = smoothstep(0.26, 0.48, v_color.a);
    float alpha = clamp(v_color.a * mix(0.72 + 1.65 * rim, 1.0, strand)
                        * u_opacity, 0.0, 0.86);
    vec3 albedo = clamp(v_color.rgb, 0.0, 1.0);
    vec3 radiance = albedo * (0.86 + 0.20 * key + 0.20 * rim);
    radiance = mix(radiance, vec3(0.94, 0.93, 1.0), 0.30 * rim * (1.0 - strand));
    // A small water-depth tint ties separate geometry to the actual volume.
    float depth_tint = clamp((v_position.z + u_box_half_extents.z)
                            / max(2.0 * u_box_half_extents.z, 1e-5), 0.0, 1.0);
    radiance *= mix(vec3(1.0), vec3(0.88, 0.95, 1.0), depth_tint * 0.30);
    radiance = clamp(radiance, 0.0, 1.0);
    if (u_emission_pass == 1) {
        // Only selected tissue supplies bloom. The transparent central film
        // stays clear; fine curves and the grazing bell rim light the halo.
        float selected = clamp(0.82 * rim + 0.64 * strand, 0.0, 1.0);
        alpha *= selected;
        radiance = clamp(albedo * 1.12, 0.0, 1.0);
    }
    frag_color = vec4(radiance * alpha, alpha);
}
"#;

pub(super) const JELLY_SCREEN_VERTEX_SHADER: &str = r#"#version 330 core
out vec2 v_uv;
void main() {
    vec2 position = vec2(
        (gl_VertexID == 1) ? 3.0 : -1.0,
        (gl_VertexID == 2) ? 3.0 : -1.0
    );
    v_uv = position * 0.5 + 0.5;
    gl_Position = vec4(position, 0.0, 1.0);
}
"#;

pub(super) const JELLY_BLUR_FRAGMENT_SHADER: &str = r#"#version 330 core
uniform sampler2D u_input;
uniform vec2 u_direction;
in vec2 v_uv;
out vec4 frag_color;
void main() {
    vec4 blurred = texture(u_input, v_uv) * 0.2270270270;
    blurred += texture(u_input, v_uv + u_direction * 1.3846153846) * 0.3162162162;
    blurred += texture(u_input, v_uv - u_direction * 1.3846153846) * 0.3162162162;
    blurred += texture(u_input, v_uv + u_direction * 3.2307692308) * 0.0702702703;
    blurred += texture(u_input, v_uv - u_direction * 3.2307692308) * 0.0702702703;
    frag_color = blurred;
}
"#;

pub(super) const JELLY_COMPOSITE_FRAGMENT_SHADER: &str = r#"#version 330 core
uniform sampler2D u_anatomy;
uniform sampler2D u_glow;
in vec2 v_uv;
out vec4 frag_color;
void main() {
    vec4 anatomy = texture(u_anatomy, v_uv);
    vec4 glow = texture(u_glow, v_uv);
    float glow_alpha = clamp(3.0 * glow.a, 0.0, 0.32);
    vec3 glow_tint = glow.a > 1e-5 ? clamp(glow.rgb / glow.a, 0.0, 1.0) : vec3(0.0);
    // Place soft light behind the sharp tissue. This keeps a true, bounded
    // premultiplied result rather than whitening the scene with an additive
    // full-screen bloom or allowing RGB to exceed output alpha.
    frag_color = vec4(
        anatomy.rgb + (1.0 - anatomy.a) * glow_tint * glow_alpha,
        anatomy.a + (1.0 - anatomy.a) * glow_alpha
    );
}
"#;
