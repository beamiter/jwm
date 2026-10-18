#ifndef JWM_SOLAR_OPTICS_GLSL
#define JWM_SOLAR_OPTICS_GLSL
// Shared, bounded visible-light display optics. All RGB outputs are LINEAR.
// Generic granulation, coronal exposure and ring substructure are analytic
// display models, not current solar observations or Cassini photometry.
// No sunspots, active regions, named storms or imaginary terrain are generated.
// Every detail uses caller-supplied analytic footprints: no derivatives inside
// divergent ray-hit branches, texture fetches, time-dependent noise or raymarch.

float solar_optics_hash(vec3 p) {
    p=fract(p*0.1031);
    p+=dot(p,p.yzx+33.33);
    return fract((p.x+p.y)*p.z);
}

float solar_optics_noise(vec3 p) {
    vec3 i=floor(p),f=fract(p);
    f=f*f*f*(f*(f*6.0-15.0)+10.0);
    return mix(mix(mix(solar_optics_hash(i),solar_optics_hash(i+vec3(1,0,0)),f.x),
                   mix(solar_optics_hash(i+vec3(0,1,0)),solar_optics_hash(i+vec3(1,1,0)),f.x),f.y),
               mix(mix(solar_optics_hash(i+vec3(0,0,1)),solar_optics_hash(i+vec3(1,0,1)),f.x),
                   mix(solar_optics_hash(i+vec3(0,1,1)),solar_optics_hash(i+vec3(1,1,1)),f.x),f.y),f.z);
}

float solar_optics_detail(float footprint,float frequency) {
    return 1.0-smoothstep(0.45,2.4,max(footprint,0.0)*frequency);
}

float solar_granulation(vec3 body_normal,float footprint,bool scientific) {
    // Small, low-contrast convection cells, fixed to the solar surface. A
    // distant disc averages to its mean; it never becomes an orange lava field.
    float primary=solar_optics_detail(footprint,580.0);
    float secondary=solar_optics_detail(footprint,1130.0);
    float grain=0.0;
    if(primary>0.001)
        grain+=(solar_optics_noise(body_normal*580.0)-0.5)*primary;
    if(secondary>0.001)
        grain+=(solar_optics_noise(body_normal*1130.0+vec3(13.7,8.1,21.3))-0.5)*secondary*0.42;
    // Two weaker, larger-scale convective brightness components remain
    // resolvable before individual granules do. They describe generic spatial
    // contrast, not observed cells, sunspots or current solar magnetic activity.
    float convection=0.0;
    float large=solar_optics_detail(footprint,24.0);
    float small=solar_optics_detail(footprint,74.0);
    if(large>0.001)
        convection+=(solar_optics_noise(body_normal*24.0+vec3(8.1,13.2,4.7))-0.5)*large*0.18;
    if(small>0.001)
        convection+=(solar_optics_noise(body_normal*74.0+vec3(2.7,31.4,7.1))-0.5)*small*0.12;
    return clamp(1.0+convection*(scientific?1.0:1.15)+(scientific?0.54:0.64)*grain,0.70,1.30);
}

vec3 solar_photosphere(vec3 body_normal,float view_cos,float footprint,bool scientific) {
    float mu=clamp(view_cos,0.0,1.0);
    float edge=1.0-mu;
    // Bounded quadratic limb darkening; the subtly warmer limb is visible
    // light, not a narrowband H-alpha/EUV false-colour image. Exposure is a
    // display choice, independent of the compressed scene's geometric scales.
    vec3 limb=vec3(1.0)-vec3(0.43,0.48,0.53)*edge
                              -vec3(0.18,0.20,0.21)*edge*edge;
    return vec3(3.15,3.06,2.92)*limb*solar_granulation(body_normal,footprint,scientific);
}

float solar_corona_column(float z,float impact) {
    // Analytic integral of 1/(impact^2+z^2)^2, with impact >= 1.
    float square=impact*impact;
    return (atan(z/impact)+z*impact/(square+z*z))/(2.0*square*impact);
}

vec3 solar_corona(vec3 origin,vec3 ray,vec3 sun_center,float sun_radius,
                 float visible_depth,float angular_pixel,bool scientific) {
    vec3 offset=sun_center-origin;
    float along=dot(offset,ray);
    // A foreground opaque object masks the envelope entirely. The caller
    // composites a nearer translucent ring after this pass using its alpha.
    if(along<=0.0 || visible_depth<=along || sun_radius<=0.0)return vec3(0.0);
    float impact2=max(0.0,dot(offset,offset)-along*along)/(sun_radius*sun_radius);
    const float extent=3.8;
    if(impact2>=extent*extent || impact2<1.0)return vec3(0.0);
    float impact=sqrt(impact2);
    float half_chord=sqrt(extent*extent-impact2)*sun_radius;
    float near_depth=max(0.0,along-half_chord);
    float far_depth=min(visible_depth,along+half_chord);
    if(far_depth<=near_depth)return vec3(0.0);
    float column=solar_corona_column((far_depth-along)/sun_radius,impact)
                -solar_corona_column((near_depth-along)/sun_radius,impact);
    float altitude=impact-1.0;
    float edge_width=clamp(angular_pixel*along/sun_radius,0.0001,0.08);
    float coverage=smoothstep(0.0,edge_width,altitude);
    // The real white-light corona is vastly fainter than the photosphere.
    // This restrained, separately enhanced display exposure makes its envelope
    // legible; it is not quantitative coronal flux or a screen-space bloom PSF.
    float intensity=column*(0.012*exp(-altitude*5.5)+0.0012)
                    *coverage*(scientific?1.0:1.65);
    return vec3(1.0,0.98,0.94)*intensity;
}

vec3 solar_imaging_glow(vec3 origin,vec3 ray,vec3 sun_center,float sun_radius,
                       float visible_depth,float angular_pixel,bool scientific) {
    // Separately labelled local imaging glow. It gives a bright source a
    // readable warm-white shoulder under the scene's compressed display
    // exposure; it is NOT coronal brightness, magnetic plasma or a PSF solver.
    // Source-depth masking intentionally preserves foreground silhouettes.
    vec3 offset=sun_center-origin;
    float along=dot(offset,ray),distance2=dot(offset,offset);
    if(along<=0.0 || visible_depth<=along || sun_radius<=0.0)return vec3(0.0);
    float impact2=max(0.0,distance2-along*along);
    float distance=sqrt(distance2);
    float bound=sun_radius+min(sun_radius*1.8,distance*0.10);
    if(impact2<sun_radius*sun_radius || impact2>bound*bound)return vec3(0.0);
    float source_angle=asin(clamp(sun_radius/max(distance,sun_radius),0.0,0.999));
    float edge_angle=max(0.0,atan(sqrt(impact2),along)-source_angle);
    float extent=min(0.10,source_angle*1.8);
    if(edge_angle>=extent)return vec3(0.0);
    float narrow=max(min(source_angle*0.09,0.012),max(angular_pixel*0.5,0.0002));
    float broad=max(min(source_angle*0.32,0.034),max(angular_pixel*0.8,0.0004));
    float lobe=0.32*exp(-edge_angle/narrow)+0.050*exp(-edge_angle/broad);
    float support=1.0-smoothstep(extent*0.70,extent,edge_angle);
    return vec3(1.0,0.93,0.82)*lobe*support*(scientific?1.0:1.30);
}

vec3 solar_ring_zones(float r,float radial_footprint) {
    // Main C/B/A boundaries in Saturn equatorial radii. The optional tenuous
    // ringlets within the Cassini division and beyond A are deliberately absent.
    float aa=clamp(abs(radial_footprint),0.00005,0.024);
    float inner=smoothstep(1.24,1.24+aa,r)*(1.0-smoothstep(1.951-aa,1.951,r));
    float cb=smoothstep(1.526-aa,1.526+aa,r);
    float a=smoothstep(2.027,2.027+aa,r)*(1.0-smoothstep(2.269-aa,2.269,r));
    return vec3(inner*(1.0-cb),inner*cb,a);
}

float solar_ring_wave(float r,float frequency,float phase,float footprint) {
    // A low-pass cutoff removes subpixel ringlets before they can form moire.
    return sin(r*frequency+phase)*solar_optics_detail(abs(footprint),frequency);
}

float solar_ring_tau(float radius_in_saturn_radii,float radial_footprint) {
    float r=radius_in_saturn_radii;
    if(r<=1.24 || r>=2.269 || (r>=1.951 && r<=2.027))return 0.0;
    vec3 zones=solar_ring_zones(r,radial_footprint);
    // Broad density terraces and progressively filtered ringlets provide
    // structure in BOTH presentations. Values are plausible optical depths,
    // not an attempt to reconstruct the observed radial occultation profile.
    float broad=solar_ring_wave(r,31.0,0.7,radial_footprint);
    float medium=solar_ring_wave(r,109.0,2.1,radial_footprint);
    float fine=solar_ring_wave(r,353.0,1.3,radial_footprint);
    float micro=solar_ring_wave(r,1037.0,0.4,radial_footprint);
    float c=0.13+0.037*broad+0.020*medium+0.010*fine;
    float b=1.50+0.34*broad+0.19*medium+0.095*fine+0.035*micro;
    float a=0.54+0.11*broad+0.075*medium+0.035*fine+0.014*micro;
    return dot(zones,max(vec3(c,b,a),vec3(0.025)));
}

float solar_ring_body_shadow(vec3 point,vec3 axis,vec3 sun,
                             float footprint,float sun_angle,float polar_ratio) {
    // Ray against the same oblate ellipsoid used for the rendered body. For
    // k=1 this is the spherical legacy viewer; no compressed Sun size enters.
    float k=clamp(polar_ratio,0.5,1.0);
    float flatten=1.0/(k*k)-1.0;
    vec3 metric_sun=sun+axis*(flatten*dot(axis,sun));
    float quadratic=dot(sun,metric_sun);
    float projection=dot(point,metric_sun);
    if(projection>=0.0)return 1.0;
    float metric2=dot(point,point)+flatten*pow(dot(point,axis),2.0);
    float miss=sqrt(max(0.0,metric2-projection*projection/quadratic));
    float distance=-projection/quadratic;
    float softness=clamp(max(abs(footprint)*1.15,max(sun_angle,0.00001)*distance)/k,
                         0.00008,0.10);
    return smoothstep(1.0-softness,1.0+softness,miss);
}

vec4 solar_ring_scatter(vec3 relative_point_in_saturn_radii,vec3 ring_normal,
                       vec3 direction_to_viewer,vec3 direction_to_sun,
                       float radial_footprint,float sun_angular_radius,
                       bool scientific,float polar_ratio) {
    float r=length(relative_point_in_saturn_radii);
    float tau=solar_ring_tau(r,radial_footprint);
    if(tau<0.00001)return vec4(0.0);
    vec3 axis=normalize(ring_normal);
    vec3 view=normalize(direction_to_viewer),sun=normalize(direction_to_sun);
    float signed_view=dot(axis,view),signed_sun=dot(axis,sun);
    float mu=max(abs(signed_view),0.008),mu0=max(abs(signed_sun),0.0005);
    float alpha=1.0-exp(-tau/mu);
    vec3 zones=solar_ring_zones(r,radial_footprint);
    vec3 srgb=(vec3(0.57,0.54,0.48)*zones.x+vec3(0.92,0.88,0.79)*zones.y
               +vec3(0.85,0.82,0.75)*zones.z)/max(dot(zones,vec3(1.0)),0.00001);
    // Optically thick lit rings saturate in tau: their remaining subtle radial
    // variation comes from an explicitly analytic icy/dusty albedo profile.
    // This preserves readable strata on the bright face without whitening it.
    float patina=0.93+0.040*solar_ring_wave(r,31.0,1.9,radial_footprint)
                     +0.025*solar_ring_wave(r,109.0,0.8,radial_footprint)
                     +0.012*solar_ring_wave(r,353.0,2.2,radial_footprint);
    srgb*=patina;
    vec3 albedo=pow(srgb,vec3(2.2));
    float phase_cos=clamp(dot(view,sun),-1.0,1.0);
    float scatter;
    if(signed_view*signed_sun>=0.0) {
        // Same-side single scattering: finite grazing behaviour and stronger
        // backscatter at small phase angles, without a metallic/specular disc.
        scatter=mu0/(mu+mu0)*(1.0-exp(-tau*(1.0/mu+1.0/mu0)));
        scatter*=1.54*(0.72+0.28*pow(max(phase_cos,0.0),8.0));
    } else {
        // Opposite-side transmission. Dense B rings darken strongly, while
        // thinner C/A material can still transmit sunlight. The equal-cosine
        // limit avoids subtractive cancellation and division by zero.
        if(abs(mu-mu0)<0.01*max(mu,mu0)) {
            float optical=tau/max(0.0005,0.5*(mu+mu0));
            scatter=optical*exp(-optical);
        } else {
            scatter=mu0/(mu0-mu)*(exp(-tau/mu0)-exp(-tau/mu));
        }
        scatter*=1.085*(0.60+0.40*pow(max(-phase_cos,0.0),5.0));
    }
    float shadow=solar_ring_body_shadow(relative_point_in_saturn_radii,axis,sun,
                                        radial_footprint,sun_angular_radius,polar_ratio);
    float planetshine=0.012*max(dot(relative_point_in_saturn_radii/r,sun),0.0)/(r*r);
    float fill=(scientific?0.001:0.004)+planetshine;
    // Return straight linear colour and Beer-Lambert alpha. Dividing the
    // integrated radiance by alpha makes ordinary mix(background,rgb,alpha)
    // correct for both transparent C and optically thick B rings.
    vec3 color=albedo*(max(scatter,0.0)*shadow/max(alpha,0.00001)+fill);
    return vec4(max(color,vec3(0.0)),clamp(alpha,0.0,1.0));
}

vec3 solar_optics_tone(vec3 linear) {
    vec3 x=max(linear,vec3(0.0));
    return clamp((x*(2.51*x+0.03))/(x*(2.43*x+0.59)+0.14),0.0,1.0);
}

#endif
