// Shared, bounded celestial sphere for the astronomy and solar-system renderers.
// Included by panorama::cosmic_shader_source before either GLSL dialect compiles.
// The catalogue is NASA SVS 4851 (Gaia/Hipparcos/Tycho). Alpha is DATA:
// 1 - alpha is the constellation-figure mask, not surface transparency.
// Old opaque sky JPEGs naturally have no figure overlay.
const float COSMIC_PI = 3.141592653589793;
const float COSMIC_TAU = 6.283185307179586;
// Zero preserves legacy diagnostic probes. Production scientific mode sets 1
// to suppress invented sky-sphere landmarks. The local 3D volumes below use
// observed catalogue positions with explicitly illustrative internal geometry.
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
uniform float deep_sky_locator_radius;
uniform mat3 nebula_orbit;
uniform float nebula_distance;
uniform float nebula_blend;

// Continuous model-space gas, dust and ionization. These are illustrative
// three-dimensional structures, not a recovered gas distribution. The source
// images only supply a colour palette; no photograph is extruded along an axis.
vec4 cosmic_nebula_field(vec3 q, int target) {
    vec3 seed=vec3(float(target)*7.13+3.0,19.3,float(target)*3.71);
    vec3 warp=q+0.10*sin(q.yzx*5.0+seed)+0.06*sin(q.zxy*9.0+seed.yzx);
    float coarse=cosmic_noise(warp*3.7+seed);
    float middle=cosmic_noise(warp*9.1+seed.zxy);
    float fine=cosmic_noise(warp*22.3+seed.yzx);
    float cloud=0.56*coarse+0.29*middle+0.15*fine;
    float filaments=pow(1.0-abs(middle*2.0-1.0),5.0);
    vec3 lobe_a=(q-vec3(-0.24,-0.06,0.12))/vec3(0.65,0.80,0.64);
    vec3 lobe_b=(q-vec3(0.35,0.22,-0.18))/vec3(0.65,0.52,0.64);
    vec3 lobe_c=(q-vec3(0.08,-0.42,-0.13))/vec3(0.54,0.57,0.76);
    float envelope=max(exp(-dot(lobe_a,lobe_a)*1.3),
                       max(exp(-dot(lobe_b,lobe_b)*1.7),exp(-dot(lobe_c,lobe_c)*1.4)));
    float gas=envelope*(0.018+4.2*pow(smoothstep(0.36,0.75,cloud+envelope*0.10),2.7));
    float dust=envelope*smoothstep(0.46,0.70,coarse*0.65+fine*0.35)*2.2;
    vec3 source=vec3(-0.20,0.10,0.16);
    float ionization=exp(-dot(q-source,q-source)*4.5);

    if (target == 4 || target == 5) {
        // A stellar bulge plus a warped spiral disc, with its own finite
        // thickness: orbiting sees a thin dusty edge and the opposite arms.
        float radius=length(q.xy);
        float z=q.z-0.055*sin(q.x*4.0+q.y*2.0);
        float disc=exp(-z*z/(0.009+radius*radius*0.012))*exp(-radius*radius*2.4);
        float angle=dot(q.xy,q.xy) > 1e-10 ? atan(q.y,q.x) : 0.0;
        float arms=pow(0.5+0.5*cos(angle*(target==4 ? 2.0 : 3.0)-radius*13.0+coarse*1.4),3.0);
        float bulge=exp(-dot(q/vec3(0.28,0.28,0.20),q/vec3(0.28,0.28,0.20))*1.5);
        gas=disc*(0.16+arms*2.6)*(0.25+middle*1.4)*(0.65+filaments*0.5)+bulge*2.2;
        dust=disc*pow(1.0-arms,3.0)*(0.5+middle)*1.7;
        ionization=clamp(bulge+arms*0.25,0.0,1.0);
    } else if (target == 9 || target == 11) {
        // Closed, corrugated expanding shells, rather than a luminous image
        // ring: the front/back wall and side rim remain separate in space.
        float radius=length(q*vec3(1.0,1.0,target==9 ? 1.12 : 0.96));
        float shell=exp(-pow((radius-0.68+(coarse-0.5)*0.12)/0.075,2.0));
        float equator=target==9 ? 0.32+0.68*exp(-q.z*q.z*5.0) : 1.0;
        gas=shell*equator*(0.35+cloud*1.7)+envelope*0.035+0.22*exp(-radius*radius*7.0);
        dust=shell*smoothstep(0.48,0.72,middle)*0.65;
        ionization=exp(-radius*radius*4.5);
    } else if (target == 7 || target == 8) {
        // Turbulent shock surfaces make filaments with different depths,
        // visible as parallax and changing occlusion through a full orbit.
        float radius=length(q*vec3(1.0,0.91,1.08));
        float shock=exp(-pow((radius-0.70+(coarse-0.5)*0.21)/0.065,2.0));
        float strands=0.2+1.7*pow(1.0-abs(middle*2.0-1.0),7.0);
        gas=shock*strands*(target==7 ? smoothstep(-0.65,0.25,q.x+q.z*0.65) : 1.0)
            +envelope*(target==8 ? 0.15 : 0.025);
        dust=shock*smoothstep(0.56,0.76,cloud)*0.8;
    } else if (target == 3 || target == 10) {
        // Dust pillars are actual tapered 3D columns inside emissive gas.
        // Shape is suggestive, not a claim to reconstruct the observed pillars.
        float pillars=0.0;
        for (int column=0; column<3; ++column) {
            float x=(float(column)-1.0)*0.30+0.07*sin(q.y*5.0+float(column));
            vec2 cross_section=(q.xz-vec2(x,float(column-1)*0.13))/vec2(0.115,0.19);
            pillars+=exp(-dot(cross_section,cross_section)*1.8)
                     *(1.0-smoothstep(0.05+float(column)*0.12,0.25+float(column)*0.12,q.y));
        }
        dust+=pillars*(0.9+cloud)*3.0;
        gas*=0.8+0.45*filaments;
    } else {
        // Offset ionized cavities, torn edges and fine ridges form cloud
        // complexes without a rectangular support or one preferred view.
        float cavity=1.0-0.72*exp(-dot((q-source)*vec3(1.5,1.0,1.4),(q-source)*vec3(1.5,1.0,1.4))*13.0);
        gas*=cavity*(0.75+filaments*0.5);
    }
    float boundary=(1.0-smoothstep(0.69,1.0,dot(q,q)))
                  *smoothstep(0.08,0.46,envelope);
    return vec4(gas,dust,ionization,filaments)*vec4(boundary,boundary,1.0,1.0);
}

vec3 cosmic_nebula_palette(int target, vec2 uv) {
    if (!deep_sky_mapped) return vec3(0.026,0.026,0.032);
    vec4 rect=DEEP_SKY_ATLAS_RECT[target];
    float credit_top=(DEEP_SKY_CREDIT_RECT[target].y-rect.y)/rect.w;
    uv.y*=max(0.1,credit_top-0.04);
    // Broad colour swatches exclude both stellar point sources and all credit
    // lettering. Palette samples never depend on camera or model depth.
    vec3 value=textureLod(deep_sky_map,rect.xy+uv*rect.zw,5.0).rgb;
    vec3 linear=value*(value*(value*0.305306011+0.682171111)+0.012522878);
    float luminance=dot(linear,vec3(0.2126,0.7152,0.0722));
    return linear/(0.025+luminance)*0.16;
}

// Integrate a soft ellipsoidal volume with independent gas emission and dust
// extinction. Stable, ray-aligned steps need no frame-dependent jitter, so the
// wallpaper rests without grain flicker when the pointer stops moving.
vec3 cosmic_nebula_integrate(vec3 origin, vec3 ray, int target, vec3 background) {
    float ratio=clamp(DEEP_SKY_HALF_SIZE_LY[target].x/DEEP_SKY_HALF_SIZE_LY[target].y,0.12,6.0);
    vec3 extent=vec3(ratio,1.0,0.78);
    vec3 scaled_origin=origin/extent, scaled_ray=ray/extent;
    float speed2=dot(scaled_ray,scaled_ray);
    // Closest-approach form avoids subtracting the squares of catalogue
    // distances when rendering a physically tiny nebula in the overview.
    float closest_t=-dot(scaled_origin,scaled_ray)/speed2;
    vec3 closest=scaled_origin+scaled_ray*closest_t;
    float half_interval2=(1.0-dot(closest,closest))/speed2;
    if (half_interval2 <= 0.0) return background*0.15;
    float half_interval=sqrt(half_interval2);
    float near_t=max(0.0,closest_t-half_interval), far_t=closest_t+half_interval;
    if (far_t <= near_t) return background*0.15;
    vec3 warm=cosmic_nebula_palette(target,vec2(0.32,0.38));
    vec3 cool=cosmic_nebula_palette(target,vec2(0.65,0.58));
    vec3 middle=cosmic_nebula_palette(target,vec2(0.51,0.23));
    if (deep_sky_mapped) {
        // Observed broad-band colours mix with bounded emission/reflection
        // palettes to retain distinct gas phases after a complete 3D orbit.
        vec3 warm_phase=vec3(0.22,0.035,0.018), cool_phase=vec3(0.015,0.115,0.20);
        if (target==2 || target==3 || target==10) warm_phase=vec3(0.20,0.105,0.035);
        if (target==9 || target==11) cool_phase=vec3(0.020,0.17,0.18);
        if (target==4 || target==5) warm_phase=vec3(0.20,0.14,0.075);
        warm=mix(warm_phase,warm,0.38);
        cool=mix(cool_phase,cool,0.38);
    }
    float step_size=(far_t-near_t)/32.0;
    float transmission=1.0;
    vec3 emission=vec3(0.0);
    for (int sample_index=0; sample_index<32; ++sample_index) {
        vec3 p=origin+ray*(near_t+(float(sample_index)+0.5)*step_size);
        vec3 q=p/extent;
        vec4 field=cosmic_nebula_field(q,target);
        float extinction=field.x*1.9+field.y*3.2;
        float opacity=1.0-exp(-extinction*step_size);
        vec3 tint=mix(warm,cool,smoothstep(0.15,0.82,field.z));
        tint=mix(tint,middle,field.w*0.23);
        // Blue light from embedded young stars scatters off the gas, while
        // colder foreground dust absorbs it independently of source colour.
        vec3 illumination=vec3(0.40)+field.z*vec3(1.10,1.60,2.40);
        if (target==4 || target==5) {
            tint=mix(cool,warm,smoothstep(0.04,0.55,field.z));
            illumination=mix(vec3(0.85,1.15,1.65),vec3(1.95,1.45,0.85),field.z);
        }
        float albedo=field.x/max(extinction,0.0001);
        vec3 light=tint*illumination*albedo*2.8;
        emission+=transmission*opacity*light;
        if (target!=4 && target!=5 && deep_sky_mapped) {
            // Compact model-space sources are embedded at different depths;
            // front dust absorbs their glow as the camera passes behind it.
            vec3 star_a=q-vec3(-0.20,0.10,0.16);
            vec3 star_b=q-vec3(0.18,-0.14,-0.21);
            float star=exp(-dot(star_a,star_a)*520.0)
                      +0.55*exp(-dot(star_b,star_b)*680.0);
            emission+=transmission*vec3(0.30,0.48,0.75)*star*step_size*9.0;
        }
        transmission*=1.0-opacity;
        if (transmission < 0.012) break;
    }
    return emission+background*0.15*transmission;
}

vec3 cosmic_nebula_volume(vec2 screen, int target, vec3 background) {
    vec3 origin=nebula_orbit*vec3(0.0,0.0,max(nebula_distance,0.7));
    vec3 ray=normalize(nebula_orbit*vec3(screen*0.932615316,-1.0));
    return cosmic_nebula_integrate(origin,ray,target,background);
}

// Preserve the original baked image credit as a readable, camera-independent
// strip; letters never become luminous streaks inside the gas model.
vec3 cosmic_nebula_credit(vec2 pixel, vec2 size, int target, vec3 color) {
    if (!deep_sky_mapped) return color;
    vec4 rect=DEEP_SKY_CREDIT_RECT[target];
    vec2 native_size=rect.zw*vec2(textureSize(deep_sky_map,0));
    float scale=min(1.0,size.x*0.88/native_size.x);
    vec2 credit_size=native_size*scale;
    vec2 local=(pixel-vec2((size.x-credit_size.x)*0.5,12.0))/credit_size;
    if (all(greaterThanEqual(local,vec2(0.0))) && all(lessThanEqual(local,vec2(1.0)))) {
        vec3 encoded=textureLod(deep_sky_map,rect.xy+vec2(local.x,1.0-local.y)*rect.zw,0.0).rgb;
        vec3 linear=encoded*(encoded*(encoded*0.305306011+0.682171111)+0.012522878);
        return mix(color,linear*0.65,nebula_blend);
    }
    return color;
}

// Schematic galactic disc and dust arms in a genuine spatial density field.
// The distant star catalogue remains the skybox material. Normalized geometry
// is deliberately not presented as a measured map of Milky Way gas.
vec3 cosmic_galaxy_volume(vec3 q) {
    vec3 light=vec3(0.0);
    float transmission=1.0;
    for (int i=0; i<12; ++i) {
        vec3 p=vec3(-0.58,0.0,0.0)+q*((float(i)+0.5)*0.16);
        float radius=length(p.xz);
        float disc=exp(-p.y*p.y*700.0-radius*radius*2.5);
        float arms=0.5+0.5*cos(atan(p.z,p.x)*4.0-radius*17.0);
        float opacity=disc*(0.015+0.045*arms);
        light+=transmission*opacity*mix(vec3(0.035,0.048,0.080),vec3(0.11,0.075,0.052),exp(-radius*radius*12.0));
        transmission*=1.0-opacity;
    }
    return light;
}

// Volume centres use heliocentric XYZ and observed projected field sizes.
// WCS fixes north; model depth is independent of source distance uncertainty.
vec3 cosmic_deep_sky(vec3 world, vec3 world_dx, vec3 world_dy, vec3 background) {
    vec3 q=cosmic_galactic(world), dx=cosmic_galactic(world_dx), dy=cosmic_galactic(world_dy);
    float footprint=max(max(length(dx),length(dy)),1e-10);
    vec3 color=background+cosmic_galaxy_volume(q)*(1.0-nebula_blend);
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
            float units=DEEP_SKY_HALF_SIZE_LY[i].y;
            vec3 local_origin=vec3(0.0,0.0,distance_ly/units);
            vec3 local_ray=vec3(tangent.x,-tangent.y,-depth);
            color=cosmic_nebula_integrate(local_origin,local_ray,i,color);
        }
        // Observed fields can be subpixel in a wide view. A quiet screen-sized
        // locator makes the real position discoverable without enlarging gas.
        if (deep_sky_locator_radius >= 0.0 && (cosmic_focus_target == 0 || nebula_blend < 1.0)) {
            float pixels=length(q-direction)/footprint;
            float radius=max(deep_sky_locator_radius,5.5);
            float ring=1.0-smoothstep(0.65,1.65,abs(pixels-radius));
            float centre=1.0-smoothstep(0.6,1.4,pixels);
            color+=vec3(0.025,0.12,0.18)*(ring*0.65+centre*0.4)*(1.0-nebula_blend);
        }
    }
    return color;
}
