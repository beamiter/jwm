#version 330 core
// Astronomy wallpaper: original renderer, NASA equirectangular daytime map.
uniform vec2 resolution;
uniform vec3 sun_direction;
uniform float view_longitude;
uniform sampler2D earth_map;
out vec4 frag_color;
const float PI = 3.141592653589793;
// Original, stationary decorative sky, not an observed star catalogue.
// No time input, twinkle, extra textures or sample loops: stars remain calm.
float sky_hash(vec2 p) {
    vec3 q = fract(vec3(p.xyx) * 0.1031);
    q += dot(q, q.yzx + 33.33);
    return fract((q.x + q.y) * q.z);
}
float sky_noise(vec2 p) {
    vec2 cell = floor(p), f = fract(p);
    f = f*f*(3.0-2.0*f);
    return mix(mix(sky_hash(cell), sky_hash(cell+vec2(1,0)), f.x),
               mix(sky_hash(cell+vec2(0,1)), sky_hash(cell+vec2(1,1)), f.x), f.y);
}
vec3 star_layer(vec2 pixel, float spacing, float seed, float brightness) {
    vec2 cell = floor(pixel / spacing) + seed;
    float h = sky_hash(cell);
    // Leave a margin so the complete star fits within its own cell.
    vec2 center = (0.15 + 0.7*vec2(h, sky_hash(cell+17.0))) * spacing;
    vec2 d = mod(pixel, spacing) - center;
    float radius = mix(0.55, 1.35, h);
    float core = 1.0-smoothstep(radius*0.25, radius+0.8, length(d));
    float glow = max(0.0, 1.0-dot(d,d)/16.0);
    glow = glow*glow*0.12;
    vec3 tint = mix(vec3(0.63,0.76,1.0), vec3(1.0,0.89,0.70), h);
    return tint * (core+glow) * brightness * step(0.35,h);
}
vec3 galaxy_background(vec2 pixel) {
    vec2 uv = (pixel-resolution*0.5)/max(resolution.y,1.0);
    // Diagonal milky band with a soft dust lane, scaled by screen height.
    float along = dot(uv,vec2(0.94,0.342));
    float across = dot(uv,vec2(-0.342,0.94)) - 0.08;
    float cloud = 0.55*sky_noise(vec2(along*3.0,across*9.0)+11.0)
                + 0.30*sky_noise(uv*19.0+37.0)
                + 0.15*sky_noise(uv*47.0+61.0);
    float band = exp(-across*across*20.0);
    float dust_distance = across+0.025*sin(along*4.0);
    float dust = exp(-(dust_distance*dust_distance)*320.0);
    vec3 sky = vec3(0.003,0.005,0.013);
    sky += mix(vec3(0.035,0.045,0.095),vec3(0.12,0.09,0.14),cloud)
           * band * (0.3+cloud) * (1.0-0.65*dust);
    sky += star_layer(pixel,23.0,5.0,0.30);
    sky += star_layer(pixel,59.0,43.0,0.66);
    sky += star_layer(pixel,137.0,91.0,0.90);
    return sky;
}
void main() {
    vec2 p = (gl_FragCoord.xy - resolution * 0.5) / max(resolution.y * 0.42, 1.0);
    float radius2 = dot(p, p);
    if (radius2 > 1.0) {
        float halo = exp(-24.0 * (sqrt(radius2) - 1.0));
        frag_color = vec4(galaxy_background(gl_FragCoord.xy) + vec3(0.025, 0.075, 0.15) * halo, 1.0);
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
