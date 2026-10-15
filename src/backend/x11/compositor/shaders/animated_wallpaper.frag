#version 330 core
// Astronomy wallpaper: original renderer, NASA equirectangular daytime map.
uniform vec2 resolution;
uniform vec3 sun_direction;
uniform float view_longitude;
uniform sampler2D earth_map;
out vec4 frag_color;
const float PI = 3.141592653589793;
void main() {
    vec2 p = (gl_FragCoord.xy - resolution * 0.5) / max(resolution.y * 0.42, 1.0);
    float radius2 = dot(p, p);
    vec3 space = vec3(0.002, 0.004, 0.009);
    if (radius2 > 1.0) {
        float halo = exp(-24.0 * (sqrt(radius2) - 1.0));
        frag_color = vec4(space + vec3(0.025, 0.075, 0.15) * halo, 1.0);
        return;
    }
    vec3 n = vec3(p, sqrt(max(0.0, 1.0 - radius2)));
    // Gentle fixed northward camera latitude; no inferred user location.
    float latitude = 0.18;
    n.yz = mat2(cos(latitude), -sin(latitude), sin(latitude), cos(latitude)) * n.yz;
    float c = cos(view_longitude), s = sin(view_longitude);
    vec3 earth_n = vec3(c*n.x + s*n.z, n.y, -s*n.x + c*n.z);
    vec2 uv = vec2(0.5 + atan(earth_n.x, earth_n.z)/(2.0*PI),
                   0.5 - asin(clamp(earth_n.y, -1.0, 1.0))/PI);
    vec3 surface = texture(earth_map, uv).rgb;
    float light = dot(earth_n, sun_direction);
    float daylight = smoothstep(-0.08, 0.12, light);
    vec3 color = surface * (0.055 + 0.945 * daylight) * (0.4 + 0.6 * max(light, 0.0));
    float rim = pow(1.0 - sqrt(max(0.0, 1.0-radius2)), 3.0);
    color += vec3(0.08, 0.26, 0.48) * rim * (0.15 + 0.85 * daylight);
    frag_color = vec4(color, 1.0);
}
