// Shared, bounded celestial sphere for the astronomy and solar-system renderers.
// Included by panorama::cosmic_shader_source before either GLSL dialect compiles.
// The catalogue is NASA SVS 4851 (Gaia/Hipparcos/Tycho). Alpha is DATA:
// 1 - alpha is the constellation-figure mask, not surface transparency.
// Old opaque sky JPEGs naturally have no figure overlay.
const float COSMIC_PI = 3.141592653589793;
const float COSMIC_TAU = 6.283185307179586;
// Zero preserves legacy diagnostic probes. Production scientific mode sets 1:
// an observed catalogue may be shown; invented stars/nebula morphology may not.
uniform int cosmic_scientific;

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

// Deep-sky landmarks in galactic l/b frame. The first four preserve the
// preceding renderer's coordinates. Additional positions are IAU galactic
// catalogue approximations; the artistic morphologies are NOT telescope data.
// The CPU picking catalogue uses the same centres and radii.
uniform int cosmic_focus_target; // 0=overview; 1..12=close-up
const vec3 COSMIC_NEBULA_DIRECTION[12] = vec3[12](
    vec3(-0.824943831, -0.331876518, -0.457521204), // Orion / M42
    vec3( 0.994392948, -0.020361000,  0.103769428), // Lagoon / M8
    vec3( 0.299894690, -0.010897463, -0.953910069), // Carina / NGC 3372
    vec3( 0.956448362,  0.013831226,  0.291573709), // Eagle / M16
    vec3(-0.481332547, -0.367637672,  0.795714472), // Andromeda / M31
    vec3(-0.589171791, -0.519966434,  0.618475147), // Triangulum / M33
    vec3(-0.895824129, -0.036120457, -0.442938419), // Rosette / NGC 2237
    vec3( 0.272581208, -0.148499838,  0.950603641), // Veil / NGC 6960
    vec3(-0.991766641, -0.100709012, -0.079098822), // Crab / M1
    vec3( 0.437945624, -0.840188215,  0.319824004), // Helix / NGC 7293
    vec3(-0.891847080, -0.399069159,  0.212961479), // Pleiades / M45
    vec3( 0.094967342,  0.014311211,  0.995377513) // North America / NGC 7000
);
const float COSMIC_NEBULA_RADIUS[12] = float[12](0.032, 0.026, 0.039, 0.025, 0.062, 0.048, 0.034, 0.036, 0.023, 0.033, 0.038, 0.037);

vec3 cosmic_nebulae(vec3 q, float footprint) {
    vec3 emission = vec3(0.0);
    for (int i=0; i<12; i++) {
        // A selected close-up keeps the genuine starfield but subdues unrelated
        // illustrative clouds, so neighbouring landmarks cannot occlude it.
        if (cosmic_focus_target != 0 && cosmic_focus_target != i+1) continue;
        vec3 centre = COSMIC_NEBULA_DIRECTION[i];
        float radius = COSMIC_NEBULA_RADIUS[i];
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
        float lum = envelope*(0.20+1.8*filament)*(1.0-0.78*lane);
        if (i==4 || i==5) {
            // Andromeda/Triangulum: tilted spiral-disk sketches with dust arms.
            vec2 disk=vec2(p.x,p.y*1.6);
            float r=length(disk), theta=atan(disk.y,disk.x);
            float arms=0.50+0.50*cos(2.0*theta-r*9.0+1.5*density);
            float bulge=exp(-r*r*8.0);
            lum=exp(-r*r*1.1)*(0.3+1.25*arms*filament)+bulge*0.55;
            warm=vec3(0.16,0.11,0.065);
            cool=vec3(0.028,0.070,0.14);
        } else if (i==9) {
            // Helix: luminous ring with a dim central cavity.
            lum *= 0.22+1.9*exp(-pow((length(p.xy)-0.78)*2.1,2.0));
            cool=vec3(0.025,0.15,0.14);
        } else if (i==10) {
            // Pleiades: compact cluster stars and fine blue reflection haze.
            float stars=pow(cos(p.x*17.0)*cos(p.y*13.0)*0.5+0.5,18.0);
            lum=lum*0.55+envelope*stars*0.9;
            cool=vec3(0.048,0.11,0.19);
        } else if (i==7 || i==8) {
            // Veil and Crab: sparse supernova filaments rather than full discs.
            lum *= 0.30+1.6*abs(sin(p.x*8.0+p.y*3.0+density*7.0));
        }
        emission += mix(warm,cool,smoothstep(0.28,0.76,density))*lum;
    }
    return emission;
}

// Linear-light output. Each caller applies its own existing display transfer.
// One catalogue lookup per visible background pixel; no per-star traversal.
vec3 cosmic_sky_background(vec3 world, vec3 world_dx, vec3 world_dy, bool mapped, bool observed) {
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
        color = cosmic_scientific != 0 ? vec3(0.00035,0.00055,0.0010) : cosmic_fallback(q,footprint);
    }
    return cosmic_scientific != 0 || observed ? color : color + cosmic_nebulae(q,footprint);
}

vec3 cosmic_sky(vec3 world, vec3 world_dx, vec3 world_dy, bool mapped) {
    return cosmic_sky_background(world,world_dx,world_dy,mapped,false);
}

// JWM_DEEP_SKY_CATALOG_INCLUDE
uniform sampler2D deep_sky_map;
uniform bool deep_sky_mapped;

// Telescope image planes use real heliocentric XYZ and physical field sizes.
// The image's WCS fixes its north angle; selecting it changes only the camera.
vec3 cosmic_deep_sky(vec3 world, vec3 world_dx, vec3 world_dy, vec3 background) {
    vec3 q=cosmic_galactic(world), dx=cosmic_galactic(world_dx), dy=cosmic_galactic(world_dy);
    float footprint=max(max(length(dx),length(dy)),1e-10);
    vec3 color=cosmic_focus_target == 0 ? background : background*0.025;
    for (int i=0; i<DEEP_SKY_COUNT; ++i) {
        if (cosmic_focus_target != 0 && cosmic_focus_target != i+1) continue;
        vec3 position=DEEP_SKY_POSITION_LY[i];
        float distance_ly=length(position);
        vec3 direction=position/distance_ly;
        float depth=dot(q,direction);
        if (depth <= 0.0) continue;
        vec2 tangent=vec2(dot(q,DEEP_SKY_RIGHT[i]),-dot(q,DEEP_SKY_UP[i]));
        vec2 extent=2.0*DEEP_SKY_HALF_SIZE_LY[i]/distance_ly;
        vec2 uv=0.5+tangent/(extent*depth);
        if (deep_sky_mapped && all(greaterThanEqual(uv,vec2(0.0))) && all(lessThanEqual(uv,vec2(1.0)))) {
            vec2 tx=vec2(dot(dx,DEEP_SKY_RIGHT[i]),-dot(dx,DEEP_SKY_UP[i]));
            vec2 ty=vec2(dot(dy,DEEP_SKY_RIGHT[i]),-dot(dy,DEEP_SKY_UP[i]));
            vec2 uv_dx=(tx*depth-tangent*dot(dx,direction))/(extent*depth*depth);
            vec2 uv_dy=(ty*depth-tangent*dot(dy,direction))/(extent*depth*depth);
            vec4 rect=DEEP_SKY_ATLAS_RECT[i];
            vec3 e=textureGrad(deep_sky_map,rect.xy+uv*rect.zw,uv_dx*rect.zw,uv_dy*rect.zw).rgb;
            vec3 linear=e*(e*(e*0.305306011+0.682171111)+0.012522878);
            vec2 edge=min(uv,1.0-uv);
            float coverage=smoothstep(0.0,0.025,min(edge.x,edge.y));
            color=mix(color,linear,coverage);
        }
        // Observed fields can be subpixel in a wide view. A quiet screen-sized
        // locator makes the real position discoverable without enlarging gas.
        if (cosmic_focus_target == 0) {
            float pixels=length(q-direction)/footprint;
            float ring=1.0-smoothstep(0.65,1.65,abs(pixels-5.5));
            float centre=1.0-smoothstep(0.6,1.4,pixels);
            color+=vec3(0.025,0.12,0.18)*(ring*0.65+centre*0.4);
        }
    }
    return color;
}
