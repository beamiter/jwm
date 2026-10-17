// Shared, bounded celestial sphere for the astronomy and solar-system renderers.
// Included by panorama::cosmic_shader_source before either GLSL dialect compiles.
// The catalogue is NASA SVS 4851 (Gaia/Hipparcos/Tycho). Alpha is DATA:
// 1 - alpha is the constellation-figure mask, not surface transparency.
// Old opaque sky JPEGs naturally have no figure overlay.
const float COSMIC_PI = 3.141592653589793;
const float COSMIC_TAU = 6.283185307179586;

// One fixed, right-handed framing in every mode. Camera-forward initially
// faces the Galactic centre with a 22-degree roll. Manual orientation then
// explores the entire sphere, including both poles, without latitude clamping.
vec3 cosmic_galactic(vec3 world) {
    return vec3(world.z,
                -0.3746065934 * world.x + 0.9271838546 * world.y,
                -0.9271838546 * world.x - 0.3746065934 * world.y);
}

vec2 cosmic_uv(vec3 q) {
    // NASA's Galactic-map longitude increases LEFT from the centre at l=0.
    // GLSL leaves atan(0,0) undefined. Longitude is arbitrary exactly at a
    // pole; select the central meridian before the finite polar mip footprint.
    float longitude = dot(q.xz,q.xz) > 1e-20 ? atan(q.z,q.x) : 0.0;
    return vec2(0.5 - longitude / COSMIC_TAU,
                0.5 - asin(clamp(q.y, -1.0, 1.0)) / COSMIC_PI);
}

vec2 cosmic_gradient(vec3 q, vec3 dq) {
    // Differentiate the continuous direction rather than atan's branch cut.
    float horizontal2 = max(dot(q.xz, q.xz), 1e-10);
    return vec2((q.z*dq.x - q.x*dq.z) / (COSMIC_TAU*horizontal2),
                -dq.y / (COSMIC_PI*max(sqrt(horizontal2), 1e-5)));
}

float cosmic_hash(vec3 p) {
    p = fract(p * 0.1031);
    p += dot(p, p.yzx + 33.33);
    return fract((p.x + p.y) * p.z);
}

float cosmic_noise(vec3 p) {
    vec3 cell = floor(p), f = fract(p);
    f = f*f*(3.0-2.0*f);
    return mix(mix(mix(cosmic_hash(cell), cosmic_hash(cell+vec3(1,0,0)), f.x),
                   mix(cosmic_hash(cell+vec3(0,1,0)), cosmic_hash(cell+vec3(1,1,0)), f.x), f.y),
               mix(mix(cosmic_hash(cell+vec3(0,0,1)), cosmic_hash(cell+vec3(1,0,1)), f.x),
                   mix(cosmic_hash(cell+vec3(0,1,1)), cosmic_hash(cell+vec3(1,1,1)), f.x), f.y), f.z);
}

// A seamless, time-independent fallback. Compact star kernels vanish before
// their 3D cell boundaries, so a pole or longitude seam never resets the field.
// Unresolved stars fade to a bounded contribution instead of shimmering.
vec3 cosmic_fallback(vec3 q, float footprint) {
    vec3 grid = q * 320.0, cell = floor(grid);
    float chance = cosmic_hash(cell);
    float distance = length(fract(grid) - 0.5);
    float width = max(0.025, min(0.23, footprint * 220.0));
    float star = exp(-distance*distance/(width*width));
    star *= 1.0 - smoothstep(0.25, 0.38, distance);
    star *= smoothstep(0.988, 1.0, chance) * min(1.0, 0.0025/(width*width));
    vec3 tint = mix(vec3(0.50,0.67,1.0), vec3(1.0,0.81,0.57), cosmic_hash(cell+19.0));
    float band = exp(-q.y*q.y*65.0);
    float clouds = cosmic_noise(q*8.0 + 37.0);
    float dust = exp(-pow((q.y + 0.02*sin(q.z*8.0))*32.0, 2.0));
    return vec3(0.0004,0.0007,0.0015) + tint*star*0.55
         + vec3(0.009,0.011,0.018)*band*(0.3+clouds)*(1.0-0.7*dust);
}

// Four small, fixed-direction nebula landmarks. Positions are converted from
// the cited ICRS coordinates in docs/celestial-explorer.md. Colours, angular
// extent and filament morphology below are ORIGINAL ART, not telescope data
// or measured three-dimensional gas volumes. No artificial relief is added to
// the NASA star map, and no clock input makes clouds boil during navigation.
const vec3 COSMIC_NEBULA_DIRECTION[4] = vec3[4](
    vec3(-0.824943831, -0.331876518, -0.457521204), // Orion / M42
    vec3( 0.994392948, -0.020361000,  0.103769428), // Lagoon / M8
    vec3( 0.299894690, -0.010897463, -0.953910069), // Carina / NGC 3372
    vec3( 0.956448362,  0.013831226,  0.291573709)  // Eagle / M16
);
const float COSMIC_NEBULA_RADIUS[4] = float[4](0.032,0.026,0.039,0.025);

vec3 cosmic_nebulae(vec3 q, float footprint) {
    vec3 emission = vec3(0.0);
    for (int i=0; i<4; i++) {
        vec3 centre = COSMIC_NEBULA_DIRECTION[i];
        float radius = COSMIC_NEBULA_RADIUS[i];
        // Spherical chord distance is continuous even across atan's seam.
        // The cheap bounding cap skips ALL noise outside a visible landmark.
        vec3 delta = (q-centre)/radius;
        float radius2 = dot(delta,delta);
        if (radius2 >= 4.0) continue;
        vec3 tangent = normalize(cross(vec3(0,1,0),centre));
        vec3 north = cross(centre,tangent);
        vec3 p = vec3(dot(delta,tangent),dot(delta,north),float(i)*7.3);
        float density = 0.0, weight = 0.57, frequency = 2.6;
        for (int octave=0; octave<4; octave++) {
            float visible = 1.0-smoothstep(0.3,0.9,footprint*frequency/radius);
            if (visible <= 0.001) break;
            density += weight*visible*cosmic_noise(p*frequency+vec3(8.1,3.7,17.0));
            frequency *= 2.03; weight *= 0.5;
        }
        float envelope = exp(-radius2*1.2)*(1.0-smoothstep(2.4,4.0,radius2));
        float filament = smoothstep(0.22,0.68,density);
        float lane = exp(-pow((p.y + 0.20*sin(p.x*4.0+float(i)))*5.0,2.0));
        vec3 warm = i==2 ? vec3(0.16,0.065,0.020) : vec3(0.15,0.024,0.060);
        vec3 cool = i==0 ? vec3(0.030,0.105,0.14) : vec3(0.032,0.066,0.13);
        emission += mix(warm,cool,smoothstep(0.28,0.76,density))
                  * envelope*(0.20+1.8*filament)*(1.0-0.78*lane);
    }
    return emission;
}

// Linear-light output. Each caller applies its own existing display transfer.
// One catalogue lookup per visible background pixel; no per-star traversal.
vec3 cosmic_sky(vec3 world, vec3 world_dx, vec3 world_dy, bool mapped) {
    vec3 q = cosmic_galactic(world);
    vec3 dx = cosmic_galactic(world_dx), dy = cosmic_galactic(world_dy);
    float footprint = max(length(dx),length(dy));
    vec3 color;
    if (mapped) {
        vec2 uv = cosmic_uv(q);
        vec2 uv_dx = cosmic_gradient(q,dx), uv_dy = cosmic_gradient(q,dy);
        // A finite polar footprint selects a coarse mip instead of magnifying
        // the arbitrary longitude exactly on the pole. Constant polar texels
        // cannot flicker as the camera crosses through it.
        if (dot(q.xz,q.xz) < footprint*footprint*0.25) {
            uv_dx.x = 0.5; uv_dy.x = 0.5;
        }
        vec4 sample_value = textureGrad(sky_map,uv,uv_dx,uv_dy);
        vec3 e = sample_value.rgb;
        vec3 linear = e*(e*(e*0.305306011+0.682171111)+0.012522878);
        color = vec3(0.00035,0.00055,0.0010) + linear*0.50;
        float figures = clamp(1.0-sample_value.a,0.0,1.0);
        // Readable but subordinate to stars; near views reveal the catalogue.
        float guide_visibility = 0.08+0.92*smoothstep(0.00025,0.0015,footprint);
        color += vec3(0.0035,0.010,0.018)*figures*0.22*guide_visibility;
    } else {
        color = cosmic_fallback(q,footprint);
    }
    return color + cosmic_nebulae(q,footprint);
}
