// Bounded surface/lighting approximations shared by celestial scene shaders.
// IDs: Sun=0, Mercury=1, Venus=2, Earth=3, Mars=4, Jupiter=5,
// Saturn=6, Uranus=7, Neptune=8, Moon=9. Shapes are unit spheres.
// No generated geographic relief is used in scientific presentation. The
// neutral colors and analytic ring opacity are labelled display models.
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
    if (body == 0) {
        float filtered = 1.0-smoothstep(0.20,0.8,footprint*500.0);
        float grain = sin(n.x*431.0+n.y*337.0)*sin(n.z*389.0-n.y*457.0);
        return neutral*(1.0+0.055*grain*filtered);
    }
    return neutral;
}

float planet_ring_optical_depth(float radius) {
    if (radius >= 1.24 && radius < 1.526) return 0.12; // C ring
    if (radius >= 1.526 && radius < 1.951) return 0.78; // B ring
    if (radius >= 2.027 && radius < 2.269) return 0.44; // A ring
    return 0.0; // Cassini division and space outside measured main rings
}

vec3 planet_shade(int body, vec3 normal, vec3 light, float view_cos, vec3 albedo) {
    if (body == 0) {
        // Visible photosphere: a near-white emitter with limb darkening.
        return pow(max(albedo*(0.40+0.60*view_cos),vec3(0.0)),vec3(1.0/2.2));
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
    if (body==0) return vec3(0.16,0.14,0.11)*exp(-altitude*18.0);
    if (body!=2 && body<5) return vec3(0.0);
    float sunlight = smoothstep(-0.04,0.22,dot(limb,normalize(light)));
    vec3 tint = body>=7 ? vec3(0.012,0.038,0.055) : vec3(0.032,0.025,0.013);
    return tint*exp(-altitude*48.0)*sunlight;
}

// Orthographic ray/plane intersection in units of Saturn's equatorial radius.
// Ring edges use NASA Planetary Rings Fact Sheet main-ring radii / 60,268 km.
// Optical depth/color are analytic display models, not Cassini ring photometry.
vec4 planet_saturn_ring(vec2 p, float surface_z, mat3 camera_to_body,
                        vec3 light, float pixel_width, bool scientific) {
    vec3 origin = camera_to_body*vec3(p,3.0);
    vec3 direction = camera_to_body*vec3(0,0,-1);
    if (abs(direction.y)<0.00001) return vec4(0.0);
    float distance = -origin.y/direction.y;
    if (distance<0.0 || 3.0-distance<surface_z) return vec4(0.0);
    vec3 point = origin+distance*direction;
    float radius = length(point.xz);
    float aa = min(0.04,max(0.0001,pixel_width/max(abs(direction.y),0.04)));
    float c = smoothstep(1.24-aa,1.24+aa,radius)*(1.0-smoothstep(1.526-aa,1.526+aa,radius));
    float b = smoothstep(1.526-aa,1.526+aa,radius)*(1.0-smoothstep(1.951-aa,1.951+aa,radius));
    float a = smoothstep(2.027-aa,2.027+aa,radius)*(1.0-smoothstep(2.269-aa,2.269+aa,radius));
    float tau = c*0.12+b*0.78+a*0.44;
    if (tau<0.00001) return vec4(0.0);
    vec3 sun = normalize(light);
    float along = dot(point,sun);
    float perpendicular = sqrt(max(0.0,dot(point,point)-along*along));
    float shadow = along<0.0 ? smoothstep(0.98,1.02,perpendicular) : 1.0;
    float micro = 1.0;
    if (!scientific) micro += 0.06*sin(radius*353.0)*(1.0-smoothstep(0.2,1.0,aa*353.0));
    float opacity = 1.0-exp(-tau/max(abs(direction.y),0.025));
    vec3 color = mix(vec3(0.35,0.31,0.25),vec3(0.74,0.68,0.55),clamp(b+a,0.0,1.0));
    color *= micro*(0.055+0.945*shadow)*(0.28+0.72*sqrt(abs(sun.y)));
    return vec4(color,opacity);
}
