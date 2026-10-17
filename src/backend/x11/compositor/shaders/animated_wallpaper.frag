#version 330 core
// Astronomy wallpaper: original renderer, NASA equirectangular daytime map.
uniform vec2 resolution;
uniform vec2 canvas_origin;
uniform float sphere_radius;
uniform vec3 sun_direction;
uniform float view_longitude;
uniform float view_latitude;
uniform sampler2D earth_map;
uniform bool moon_material;
uniform sampler2D cloud_map;
uniform bool clouds_enabled;
uniform sampler2D sky_map;
uniform bool sky_enabled;
// (cos, sin), or (0, 0) for the exact original stationary framing.
uniform vec2 sky_rotation;
out vec4 frag_color;
const float PI = 3.141592653589793;
// Analytic gradients of an equirectangular lookup. Taking dFdx(atan(...))
// mistakes the longitude wrap for a full-texture jump and blurs a visible seam.
// Analytic ray/sphere differentials also remain defined in silhouette branches.
vec2 equirect_gradient(vec3 n, vec3 dn) {
    float longitude_scale = max(dot(n.xz,n.xz), 0.00000001);
    float latitude_scale = max(length(n.xz), 0.0001);
    return vec2((n.z*dn.x-n.x*dn.z)/(2.0*PI*longitude_scale),
                -dn.y/(PI*latitude_scale));
}
// Original decorative fallback, not an observed star catalogue.
// Coherent slow drift only: no twinkle, extra textures or sample loops.
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
    bool moving = dot(sky_rotation, sky_rotation) > 0.5;
    // Preserve the original static framing exactly when motion is disabled.
    if (!moving && !sky_enabled) return procedural_background(pixel);
    vec2 uv = (pixel-resolution*0.5)/max(resolution.y,1.0);
    vec2 g = vec2(dot(uv,vec2(0.94,0.342)),dot(uv,vec2(-0.342,0.94))-0.08);
    vec2 sky_uv = vec2(0.57+g.x*0.22,0.5-g.y*0.44);
    vec2 sky_dx = vec2(0.94*0.22,0.342*0.44)/max(resolution.y,1.0);
    vec2 sky_dy = vec2(0.342*0.22,-0.94*0.44)/max(resolution.y,1.0);
    if (moving) {
        // Travel through the panorama, rather than spinning the canvas. The
        // aspect-correct ray stays within the texture poles on ultrawide views.
        vec3 view = vec3(g*(0.44*PI), 1.0);
        float inverse_length = inversesqrt(dot(view,view));
        vec3 ray = view*inverse_length;
        vec3 view_dx = vec3(0.94,-0.342,0.0)*(0.44*PI/max(resolution.y,1.0));
        vec3 view_dy = vec3(0.342,0.94,0.0)*(0.44*PI/max(resolution.y,1.0));
        vec3 ray_dx = (view_dx-ray*dot(ray,view_dx))*inverse_length;
        vec3 ray_dy = (view_dy-ray*dot(ray,view_dy))*inverse_length;
        mat2 roll = mat2(sky_rotation.x, -sky_rotation.y,
                         sky_rotation.y, sky_rotation.x);
        ray.xz = roll*ray.xz;
        ray_dx.xz = roll*ray_dx.xz;
        ray_dy.xz = roll*ray_dy.xz;
        sky_uv = vec2(0.57 + atan(ray.x,ray.z)/(2.0*PI),
                      0.5 - asin(clamp(ray.y,-1.0,1.0))/PI);
        sky_dx = equirect_gradient(ray,ray_dx);
        sky_dy = equirect_gradient(ray,ray_dy);
        if (!sky_enabled) {
            // A periodic spatial fallback: both sides meet at the same sample
            // and first derivative. Blend over the whole circumference so no
            // seam, reset or hard cut enters the view as new sky drifts in.
            float circumference = max(resolution.y,1.0)/0.22;
            float x = fract(sky_uv.x)*circumference;
            vec2 p = vec2(x, (0.5-sky_uv.y)*max(resolution.y,1.0)/0.44
                            + resolution.y*0.5);
            return mix(procedural_background(p),
                       procedural_background(p-vec2(circumference,0.0)),
                       smoothstep(0.0,circumference,x));
        }
    }
    // NASA SVS/Gaia catalogue panorama, decorative rather than a live sky.
    // Horizontal REPEAT joins catalogue longitude; latitude stays in [0,1].
    // The bundled JPEG is sampled as UNORM and dimmed in display space.
    return textureGrad(sky_map,sky_uv,sky_dx,sky_dy).rgb*0.34;
}

// One shared Earth-fixed transform keeps the camera, map and atmosphere aligned.
vec3 earth_normal(vec3 n) {
    float latitude = view_latitude;
    n.yz = mat2(cos(latitude), -sin(latitude), sin(latitude), cos(latitude)) * n.yz;
    float c = cos(view_longitude), s = sin(view_longitude);
    return vec3(c*n.x + s*n.z, n.y, -s*n.x + c*n.z);
}
vec3 atmosphere(vec2 p, float radius2) {
    if (moon_material) return vec3(0.0);
    vec3 limb = earth_normal(vec3(normalize(p), 0.0));
    float sunlight = smoothstep(-0.12, 0.22, dot(limb, normalize(sun_direction)));
    float halo = exp(-35.0 * max(0.0, sqrt(radius2)-1.0));
    return vec3(0.012, 0.055, 0.105) * halo * (0.025 + 0.975*sunlight);
}

void main() {
    vec2 pixel = gl_FragCoord.xy - canvas_origin;
    vec2 p = (pixel - resolution * 0.5) / max(sphere_radius, 1.0);
    float radius2 = dot(p, p);
    // One framebuffer pixel of analytic silhouette coverage, stable over motion.
    float edge = max(fwidth(radius2), 0.000001);
    float coverage = 1.0 - smoothstep(1.0-edge*0.5, 1.0+edge*0.5, radius2);
    if (coverage <= 0.0) {
        frag_color = vec4(galaxy_background(pixel) + atmosphere(p, radius2), 1.0);
        return;
    }
    vec3 n = vec3(p, sqrt(max(0.0, 1.0 - radius2)));
    if (radius2 > 1.0) n.xy *= inversesqrt(radius2);
    // Configured fixed geographic camera; never infer location from timezone.
    float latitude = view_latitude;
    float c = cos(view_longitude), s = sin(view_longitude);
    vec3 earth_n = earth_normal(n);
    vec2 uv = vec2(0.5 + atan(earth_n.x, earth_n.z)/(2.0*PI),
                   0.5 - asin(clamp(earth_n.y, -1.0, 1.0))/PI);
    float inverse_radius = 1.0/max(sphere_radius,1.0);
    // Bound the tangent differential by one pixel's curved-surface footprint.
    float normal_z = max(n.z,sqrt(inverse_radius));
    vec3 normal_dx = earth_normal(vec3(inverse_radius,0.0,-p.x*inverse_radius/normal_z));
    vec3 normal_dy = earth_normal(vec3(0.0,inverse_radius,-p.y*inverse_radius/normal_z));
    vec2 uv_dx = equirect_gradient(earth_n,normal_dx);
    vec2 uv_dy = equirect_gradient(earth_n,normal_dy);
    vec3 surface = textureGrad(earth_map,uv,uv_dx,uv_dy).rgb;
    vec3 albedo = pow(surface, vec3(2.2));
    float light = dot(earth_n, normalize(sun_direction));
    float view_cos = sqrt(max(0.0, 1.0-radius2));
    float mu = max(light, 0.0);
    // Airless regolith has strong backscatter and a softly modelled limb.
    // The LROC mosaic supplies real maria/craters; no invented bump geography.
    if (moon_material) {
        float lommel = mu / max(mu + view_cos, 0.08);
        float diffuse = 0.48 * mu + 0.52 * lommel;
        vec3 color = pow(albedo * (0.014 + 1.20*diffuse), vec3(1.0/2.2));
        // Opaque lunar pixels must not evaluate a panorama or procedural stars.
        if (coverage < 1.0) color = mix(galaxy_background(pixel),color,coverage);
        frag_color = vec4(color, 1.0);
        return;
    }
    // NASA's static Blue Marble cloud composite, not live weather.
    float cloud = clouds_enabled ? textureGrad(cloud_map,uv,uv_dx,uv_dy).r : 0.0;
    // Retain the cloud map's soft optical-density gradations instead of
    // thresholding it into opaque white patches. Static composite, not weather.
    cloud = pow(clamp(cloud, 0.0, 1.0), 1.15) * 0.70;
    float water = smoothstep(0.015, 0.09, surface.b - max(surface.r, surface.g));
    // Restrained blue water reflectance keeps the supplied near-black ocean
    // map readable under daylight without lifting the whole night hemisphere.
    albedo = mix(albedo, max(albedo, vec3(0.018,0.055,0.095)), water*0.85);
    albedo = mix(albedo, vec3(0.68,0.73,0.78), cloud);
    // Separate direct sunlight from diffuse skylight: never gate the whole
    // surface with a narrow day/night mask. light == 0 stays the geometric
    // terminator; direct irradiance is zero throughout the night hemisphere.
    // A compact, artistic scattering approximation, not radiative transfer.
    // sin(-6 degrees) anchors the faint civil-twilight end of diffuse light.
    float daylight = smoothstep(-0.104528, 0.35, light);
    float grazing = 1.0 - smoothstep(0.0, 0.25, mu);
    vec3 direct_tint = mix(vec3(1.0), vec3(1.0,0.78,0.58), grazing);
    vec3 sky_tint = mix(vec3(0.52,0.68,1.0), vec3(1.0), daylight);
    vec3 irradiance = vec3(0.0020) + 0.84*mu*direct_tint
                    + 0.15872*daylight*sky_tint;
    vec3 color_linear = albedo * irradiance;
    // Ocean classification from the bundled colour map, no added imagery.
    float ocean = water * (1.0-cloud);
    vec3 view_world = vec3(s*cos(latitude), sin(latitude), c*cos(latitude));
    vec3 half_vector = normalize(sun_direction) + view_world;
    half_vector *= inversesqrt(max(dot(half_vector, half_vector), 0.000001));
    float specular = pow(max(dot(earth_n, half_vector), 0.0), 80.0);
    color_linear += vec3(0.16, 0.19, 0.22) * specular * ocean * smoothstep(0.0, 0.06, light);
    // Thin Rayleigh-like blue limb, warm grazing sunlight at the terminator.
    float rim = pow(1.0-view_cos, 5.0);
    // A shallow blue aerial-perspective layer increases towards the limb.
    // It follows solar illumination and preserves geographic texture detail.
    float haze = (0.06 + 0.22*pow(1.0-view_cos, 2.0))*daylight;
    color_linear = mix(color_linear, vec3(0.035,0.11,0.19)*irradiance, haze);
    vec3 scatter = vec3(0.018,0.10,0.24) * daylight;
    float sunset = exp(-pow((light-0.015)/0.075, 2.0));
    scatter += vec3(0.065,0.018,0.004) * sunset;
    color_linear += scatter * rim * 0.48;
    vec3 color = pow(max(color_linear, vec3(0.0)), vec3(1.0/2.2));
    if (coverage < 1.0) {
        vec3 sky = galaxy_background(pixel) + atmosphere(p, radius2);
        color = mix(sky, color, coverage);
    }
    frag_color = vec4(color, 1.0);
}
