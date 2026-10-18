// Bounded surface/lighting approximations shared by celestial scene shaders.
// IDs: Sun=0, Mercury=1, Venus=2, Earth=3, Mars=4, Jupiter=5,
// Saturn=6, Uranus=7, Neptune=8, Moon=9. Shapes are unit spheres.
// No generated geographic relief is used in scientific presentation. The
// neutral colors and analytic ring opacity are labelled display models.
// JWM_SOLAR_OPTICS_INCLUDE

vec3 planet_neutral_albedo(int body) {
    vec3 rgb = vec3(0.50,0.48,0.46);
    if (body == 0) rgb = vec3(1.0,0.98,0.93);
    if (body == 1) rgb = vec3(0.49,0.47,0.45);
    if (body == 2) rgb = vec3(0.86,0.82,0.71);
    if (body == 4) rgb = vec3(0.66,0.36,0.23);
    if (body == 5) rgb = vec3(0.79,0.71,0.60);
    if (body == 6) rgb = vec3(0.85,0.79,0.64);
    if (body == 7) rgb = vec3(0.64,0.82,0.84);
    if (body == 8) rgb = vec3(0.38,0.58,0.77);
    return pow(rgb,vec3(2.2));
}

vec3 planet_display_albedo(int body, vec3 n, float footprint, bool scientific) {
    vec3 neutral = planet_neutral_albedo(body);
    if (body == 0) return neutral*solar_granulation(n,footprint,scientific);
    if (scientific) return neutral;
    // Explicitly illustrative cloud bands. Stable body coordinates and an
    // analytic frequency cutoff keep these clouds still during navigation.
    if (body == 2 || (body >= 5 && body <= 8)) {
        float latitude = asin(clamp(n.y,-1.0,1.0));
        float frequency = body == 5 ? 32.0 : (body == 6 ? 48.0 : 16.0);
        float filtered = 1.0-smoothstep(0.35,1.2,footprint*frequency);
        float warp = 0.40*sin(n.x*8.0+n.z*6.0);
        float bands = sin(latitude*frequency+warp)*filtered;
        float secondary = sin(latitude*frequency*2.37-0.7)*filtered*0.25;
        float strength = body == 5 ? 0.24 : (body == 6 ? 0.13 : 0.055);
        return neutral*(1.0+strength*(bands+secondary));
    }
    return neutral;
}

float planet_ring_optical_depth(float radius) {
    // This legacy shadow API lacks a ray footprint. A conservative fixed
    // cutoff retains the broad density terraces and suppresses fine ringlets.
    return solar_ring_tau(radius,0.006);
}

vec3 planet_shade(int body, vec3 normal, vec3 light, float view_cos, vec3 albedo) {
    if (body == 0) {
        // The albedo caller already filtered the granulation. Preserve the
        // public gamma-output API while sharing the solar scene's HDR emitter.
        vec3 modulation=albedo/max(planet_neutral_albedo(0),vec3(0.01));
        vec3 linear=solar_photosphere(normal,view_cos,1.0,true)*modulation*0.52;
        return pow(solar_optics_tone(linear),vec3(1.0/2.2));
    }
    vec3 sun = normalize(light);
    float mu = max(dot(normal,sun),0.0);
    float diffuse = mu;
    if (body == 1 || body == 4 || body == 9) {
        float lommel = mu/max(mu+view_cos,0.08);
        diffuse = 0.44*mu+0.56*lommel;
    }
    float ring_shadow = 1.0;
    if (body == 6 && abs(sun.y)>0.0001) {
        float distance = -normal.y/sun.y;
        if (distance>0.0) {
            vec3 at_ring = normal+distance*sun;
            float opacity = planet_ring_optical_depth(length(at_ring.xz));
            ring_shadow = exp(-opacity/max(abs(sun.y),0.025));
        }
    }
    vec3 linear = albedo*(0.004+1.06*diffuse*ring_shadow);
    // Smooth aerial scattering models a cloud envelope; never terrain relief.
    if (body == 2 || (body>=5 && body<=8)) {
        vec3 tint = (body>=7) ? vec3(0.035,0.12,0.20) : vec3(0.18,0.14,0.08);
        linear += tint*pow(1.0-view_cos,4.0)*mu*0.16;
    }
    return pow(max(linear,vec3(0.0)),vec3(1.0/2.2));
}

vec3 planet_halo(int body, vec3 limb, vec3 light, float radius2) {
    if (body==1 || body==9) return vec3(0.0);
    float altitude = max(0.0,sqrt(radius2)-1.0);
    if (body==0) {
        // The orthographic legacy API has no perspective depth. Its caller
        // masks the disc; this is a local display glow, not coronal photometry.
        if(altitude>1.8)return vec3(0.0);
        float lobe=(0.32*exp(-altitude*16.0)+0.05*exp(-altitude*5.0))
                    *(1.0-smoothstep(1.3,1.8,altitude));
        return pow(solar_optics_tone(vec3(1.0,0.93,0.82)*lobe),vec3(1.0/2.2));
    }
    if (body!=2 && body<5) return vec3(0.0);
    float sunlight = smoothstep(-0.04,0.22,dot(limb,normalize(light)));
    vec3 tint = body>=7 ? vec3(0.012,0.038,0.055) : vec3(0.032,0.025,0.013);
    return tint*exp(-altitude*48.0)*sunlight;
}

// Orthographic ray/plane intersection in units of Saturn's equatorial radius.
// Ring edges use NASA Planetary Rings Fact Sheet main-ring radii / 60,268 km.
// Optical depth/color are analytic display models, not Cassini ring photometry.
// Legacy output is straight gamma RGB + alpha; shared optics remain linear.
vec4 planet_saturn_ring(vec2 p, float surface_z, mat3 camera_to_body,
                        vec3 light, float pixel_width, bool scientific) {
    vec3 origin = camera_to_body*vec3(p,3.0);
    vec3 direction = camera_to_body*vec3(0,0,-1);
    if (abs(direction.y)<0.00001) return vec4(0.0);
    float distance = -origin.y/direction.y;
    if (distance<0.0 || 3.0-distance<surface_z) return vec4(0.0);
    vec3 point = origin+distance*direction;
    vec3 origin_dx=camera_to_body[0]*pixel_width;
    vec3 origin_dy=camera_to_body[1]*pixel_width;
    vec3 point_dx=origin_dx-direction*(origin_dx.y/direction.y);
    vec3 point_dy=origin_dy-direction*(origin_dy.y/direction.y);
    float footprint=max(length(point_dx),length(point_dy));
    // The isolated viewer still draws a unit sphere, so its occulting shape
    // uses k=1. The solar scene passes Saturn's oblate polar/equatorial ratio.
    vec4 ring=solar_ring_scatter(point,vec3(0,1,0),-direction,light,
                                footprint,0.00465/9.58,scientific,1.0);
    return vec4(pow(solar_optics_tone(ring.rgb),vec3(1.0/2.2)),ring.a);
}
