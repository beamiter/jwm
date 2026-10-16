#version 330 core
// Astronomy wallpaper: original renderer, NASA equirectangular daytime map.
uniform vec2 resolution;
uniform vec2 canvas_origin;
uniform float sphere_radius;
uniform vec3 sun_direction;
uniform float view_longitude;
uniform sampler2D earth_map;
uniform bool moon_material;
uniform sampler2D cloud_map;
uniform bool clouds_enabled;
uniform sampler2D sky_map;
uniform bool sky_enabled;
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
    float radius = mix(0.40, 1.55, h*h);
    float core = 1.0-smoothstep(radius*0.25, radius+0.8, length(d));
    float glow = max(0.0, 1.0-dot(d,d)/16.0);
    glow = glow*glow*0.12;
    vec3 tint = mix(vec3(0.63,0.76,1.0), vec3(1.0,0.89,0.70), h);
    // Large-scale variations break up the regular cell distribution.
    float density = sky_noise(pixel / 380.0 + seed);
    float visible = step(mix(0.82, 0.32, density), h);
    return tint * (core+glow) * brightness * visible * (0.35+0.65*h*h);
}
vec3 procedural_background(vec2 pixel) {
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
    dust *= 0.55 + 0.65*sky_noise(vec2(along*12.0, across*42.0)+83.0);
    float filaments = sky_noise(vec2(along*8.0, across*50.0)+23.0);
    vec3 sky = vec3(0.003,0.005,0.013);
    sky += mix(vec3(0.065,0.075,0.105),vec3(0.18,0.16,0.18),cloud)
           * band * (0.28+cloud) * (1.0-0.78*dust) * (0.65+0.35*filaments);
    sky += star_layer(pixel,23.0,5.0,0.30);
    sky += star_layer(pixel,59.0,43.0,0.66);
    sky += star_layer(pixel,137.0,91.0,0.90);
    return sky;
}

vec3 galaxy_background(vec2 pixel) {
    if (!sky_enabled) return procedural_background(pixel);
    // NASA SVS/Gaia catalogue-derived diffuse sky, fixed decorative framing.
    // This crop is not the observer's location, current sky or a telescope view.
    vec2 uv = (pixel-resolution*0.5)/max(resolution.y,1.0);
    vec2 g = vec2(dot(uv,vec2(0.94,0.342)),dot(uv,vec2(-0.342,0.94))-0.08);
    vec2 sky_uv = vec2(0.57+g.x*0.22,0.5-g.y*0.44);
    // Bundled JPEG already contains the linear-to-sRGB transfer. It is sampled
    // as UNORM, like our body maps, then attenuated in display space deliberately
    // to keep the catalogue-derived detail quiet behind the Earth and Moon.
    return texture(sky_map,sky_uv).rgb*0.34;
}

void main() {
    vec2 pixel = gl_FragCoord.xy - canvas_origin;
    vec2 p = (pixel - resolution * 0.5) / max(sphere_radius, 1.0);
    float radius2 = dot(p, p);
    // One framebuffer pixel of analytic silhouette coverage, stable over motion.
    float edge = max(fwidth(radius2), 0.000001);
    float coverage = 1.0 - smoothstep(1.0-edge*0.5, 1.0+edge*0.5, radius2);
    if (coverage <= 0.0) {
        float halo = exp(-24.0 * (sqrt(radius2) - 1.0));
        frag_color = vec4(galaxy_background(pixel) + vec3(0.009, 0.028, 0.062) * halo * (moon_material ? 0.0 : 1.0), 1.0);
        return;
    }
    vec3 n = vec3(p, sqrt(max(0.0, 1.0 - radius2)));
    if (radius2 > 1.0) n.xy *= inversesqrt(radius2);
    // Gentle fixed northward camera latitude; no inferred user location.
    float latitude = 0.18;
    n.yz = mat2(cos(latitude), -sin(latitude), sin(latitude), cos(latitude)) * n.yz;
    float c = cos(view_longitude), s = sin(view_longitude);
    vec3 earth_n = vec3(c*n.x + s*n.z, n.y, -s*n.x + c*n.z);
    vec2 uv = vec2(0.5 + atan(earth_n.x, earth_n.z)/(2.0*PI),
                   0.5 - asin(clamp(earth_n.y, -1.0, 1.0))/PI);
    vec3 surface = texture(earth_map, uv).rgb;
    vec3 albedo = pow(surface, vec3(2.2));
    float light = dot(earth_n, normalize(sun_direction));
    float daylight = smoothstep(-0.045, 0.075, light);
    float view_cos = sqrt(max(0.0, 1.0-radius2));
    float mu = max(light, 0.0);
    // Airless regolith has strong backscatter and a softly modelled limb.
    // The LROC mosaic supplies real maria/craters; no invented bump geography.
    if (moon_material) {
        float lommel = mu / max(mu + view_cos, 0.08);
        float diffuse = 0.48 * mu + 0.52 * lommel;
        vec3 color = pow(albedo * (0.014 + 1.20*diffuse), vec3(1.0/2.2));
        frag_color = vec4(mix(galaxy_background(pixel), color, coverage), 1.0);
        return;
    }
    // NASA's static Blue Marble cloud composite, not live weather.
    float cloud = clouds_enabled ? texture(cloud_map, uv).r : 0.0;
    cloud = smoothstep(0.06, 0.92, cloud) * 0.88;
    albedo = mix(albedo, vec3(0.83,0.86,0.90), cloud);
    vec3 color_linear = albedo * (0.008 + 0.992*daylight) * (0.16 + 0.84*mu);
    // Ocean classification from the bundled colour map, no added imagery.
    float ocean = smoothstep(0.015, 0.09, surface.b - max(surface.r, surface.g)) * (1.0-cloud);
    vec3 view_world = vec3(s*cos(latitude), sin(latitude), c*cos(latitude));
    vec3 half_vector = normalize(normalize(sun_direction) + view_world);
    float specular = pow(max(dot(earth_n, half_vector), 0.0), 80.0);
    color_linear += vec3(0.42, 0.47, 0.50) * specular * ocean * daylight;
    // Thin Rayleigh-like blue limb, warm grazing sunlight at the terminator.
    float rim = pow(1.0-view_cos, 7.0);
    vec3 scatter = vec3(0.012,0.075,0.20) * daylight;
    float sunset = exp(-pow((light-0.015)/0.075, 2.0));
    scatter += vec3(0.065,0.018,0.004) * sunset;
    color_linear += scatter * rim * 0.48;
    vec3 color = pow(max(color_linear, vec3(0.0)), vec3(1.0/2.2));
    if (coverage < 1.0) {
        float halo = exp(-24.0 * max(0.0, sqrt(radius2) - 1.0));
        vec3 sky = galaxy_background(pixel) + vec3(0.009, 0.028, 0.062) * halo * (moon_material ? 0.0 : 1.0);
        color = mix(sky, color, coverage);
    }
    frag_color = vec4(color, 1.0);
}
