#version 330 core
// Date-derived JPL approximation or legacy illustration. Display scales are
// deliberately compressed; body positions and mouse picking share one CPU frame.
out vec4 frag_color;
uniform vec2 resolution;
uniform vec2 canvas_origin;
uniform int ephemeris_enabled;
uniform vec3 body_centers[9];
uniform vec3 body_axes[9];
uniform vec3 ellipse_centers[8];
uniform vec3 ellipse_u[8];
uniform vec3 ellipse_v[8];
uniform int scientific_mode;
uniform int focus_enabled;
uniform int focused_body;
uniform vec3 camera_eye;
uniform mat3 camera_basis;
uniform int camera_basis_enabled;
uniform float camera_focal;
uniform float orbit_phase[8];
uniform float surface_phase;
uniform float spin_phase[8];
uniform sampler2D earth_map;
uniform int earth_enabled;
uniform sampler2D cloud_map;
uniform int cloud_enabled;
uniform sampler2D sky_map;
uniform int sky_enabled;
uniform sampler2D mercury_map;
uniform int mercury_enabled;
uniform sampler2D venus_map;
uniform int venus_enabled;
uniform sampler2D mars_map;
uniform int mars_enabled;
uniform sampler2D jupiter_map;
uniform int jupiter_enabled;
uniform sampler2D saturn_map;
uniform int saturn_enabled;
uniform sampler2D uranus_map;
uniform int uranus_enabled;
uniform sampler2D neptune_map;
uniform int neptune_enabled;
const float PI=3.14159265359;
const float TAU=6.28318530718;
// Explicit spherical lookup derivatives preserve longitude wrap continuity and
// remain defined inside ray-hit branches and adaptive antialiasing samples.
vec2 equirect_gradient(vec3 n,vec3 dn){
 float longitude_scale=max(dot(n.xz,n.xz),.00000001);
 float latitude_scale=max(length(n.xz),.0001);
 return vec2((n.x*dn.z-n.z*dn.x)/(TAU*longitude_scale),-dn.y/(PI*latitude_scale));
}
const float orbit[8]=float[8](2.5,3.6,4.8,6.0,7.5,9.3,11.0,12.7);
const float radius[9]=float[9](1.12,.23,.40,.46,.33,.90,.76,.58,.55);
// NASA GSFC 1-bar polar/equatorial radii, shared with CPU picking. Display
// distances/radii remain compressed; flattening itself is not exaggerated.
const float polar_ratio[9]=float[9](1.,1.,1.,1.,1.,.9351256,.90203756,.97707266,.98291874);
const float semimajor_au[8]=float[8](.3871,.7233,1.,1.5237,5.2029,9.537,19.189,30.069);
// Full right-hand-rule obliquities: >90 degrees naturally rotates retrograde.
const float obliquity[8]=float[8](.034,177.4,23.44,25.19,3.1,26.7,97.8,28.3);
vec3 body_axis(int body){
 if(ephemeris_enabled!=0)return normalize(body_axes[body]);
 if(body==0)return vec3(0.,1.,0.);
 float tilt=obliquity[body-1]*PI/180.;
 // Each illustrative orbit is inclined in the yz plane; tilt relative to it.
 vec3 orbital_normal=normalize(vec3(0.,orbit[body-1],-.025*float(body)));
 return vec3(sin(tilt),0.,0.)+cos(tilt)*orbital_normal;
}
float hash(vec3 p){p=fract(p*.1031);p+=dot(p,p.yzx+33.33);return fract((p.x+p.y)*p.z);}
float noise(vec3 p){
 vec3 i=floor(p),f=fract(p);f=f*f*(3.-2.*f);
 return mix(mix(mix(hash(i),hash(i+vec3(1,0,0)),f.x),mix(hash(i+vec3(0,1,0)),hash(i+vec3(1,1,0)),f.x),f.y),mix(mix(hash(i+vec3(0,0,1)),hash(i+vec3(1,0,1)),f.x),mix(hash(i+vec3(0,1,1)),hash(i+vec3(1,1,1)),f.x),f.y),f.z);
}
float fbm(vec3 p){float n=0.,weight=.55;for(int j=0;j<4;j++){n+=weight*noise(p);p=p*2.03+vec3(4.7,9.2,1.8);weight*=.48;}return n;}
// Spatial detail fades in only when its wavelength spans enough screen pixels.
// It is body-fixed and has no time input: approaching a planet reveals existing
// structure instead of adding boiling noise or a distant shimmer.
float detail_weight(float footprint,float frequency){return 1.-smoothstep(.10,.65,footprint*frequency);}
float surface_detail(vec3 q,float footprint){
 float detail=0.,weight=.55,frequency=32.;
 for(int octave=0;octave<5;octave++){
  float visible=detail_weight(footprint,frequency);
  if(visible<.001)break;
  detail+=weight*visible*(noise(q*frequency+vec3(4.7,9.2,1.8)*float(octave))-.5);
  weight*=.5;frequency*=2.03;
 }
 return detail;
}
float crater_detail(vec3 q,float footprint){
 float height=0.,frequency=9.,weight=.65;
 for(int octave=0;octave<3;octave++){
  float visible=detail_weight(footprint,frequency*5.);
  if(visible<.001)break;
  vec3 p=q*frequency,cell=floor(p);
  vec3 offset=vec3(hash(cell+3.),hash(cell+17.),hash(cell+41.))*.24-.12;
  float radius=.13+.11*hash(cell+71.);
  float distance=length(fract(p)-.5-offset)/radius;
  // Compact support inside each cell prevents a cell edge discontinuity.
  float bowl=-.55*(1.-smoothstep(.12,.94,distance));
  float rim=.28*exp(-pow((distance-.98)/.13,2.));
  float support=1.-smoothstep(1.05,1.25,distance);
  height+=weight*visible*(bowl+rim)*support;
  frequency*=3.;weight*=.48;
 }
 return height;
}
vec3 center(int i){if(ephemeris_enabled!=0)return body_centers[i];if(i==0)return vec3(0);float a=orbit_phase[i-1];return vec3(cos(a)*orbit[i-1],sin(a)*.025*float(i),sin(a)*orbit[i-1]);}
float sphere(vec3 ro,vec3 rd,vec3 c,float r){vec3 q=ro-c;float b=dot(q,rd),d=b*b-dot(q,q)+r*r;if(d<0.)return 1e5;float t=-b-sqrt(d);if(t>0.)return t;t=-b+sqrt(d);return t>0.?t:1e5;}
vec3 spheroid_metric(vec3 v,vec3 axis,float ratio){
 return v+axis*((1./(ratio*ratio)-1.)*dot(v,axis));
}
vec2 spheroid_interval(vec3 ro,vec3 rd,vec3 c,float r,vec3 axis,float ratio){
 vec3 q=ro-c,md=spheroid_metric(rd,axis,ratio);
 float a=dot(rd,md),b=dot(q,md),cc=dot(q,spheroid_metric(q,axis,ratio))-r*r;
 float d=b*b-a*cc;
 if(d<0.)return vec2(1e5,-1e5);
 float root=sqrt(max(d,0.));
 return vec2(-b-root,-b+root)/a;
}
float body_intersection(int body,vec3 ro,vec3 rd){
 vec2 interval=spheroid_interval(ro,rd,center(body),radius[body],body_axis(body),polar_ratio[body]);
 return interval.x>0.?interval.x:(interval.y>0.?interval.y:1e5);
}
// Decode image albedo once. Lighting, atmosphere and ring compositing remain
// linear until the single final filmic display transform.
vec3 solar_linear(vec3 encoded){
 vec3 e=clamp(encoded,0.,1.);
 return e*(e*(e*.305306011+.682171111)+.012522878);
}
// JWM_SOLAR_OPTICS_INCLUDE
vec3 rotate_y(vec3 n,float a){float s=sin(a),c=cos(a);return vec3(c*n.x-s*n.z,n.y,s*n.x+c*n.z);}
// World-to-body rigid transform. Only albedo enters this frame; normals,
// Sun direction and terminator remain in world coordinates.
vec3 surface_point(int body,vec3 n){
 vec3 axis=body_axis(body);
 vec3 x=normalize(cross(axis,vec3(0.,0.,1.)));
 vec3 z=cross(x,axis);
 return rotate_y(vec3(dot(n,x),dot(n,axis),dot(n,z)),spin_phase[body-1]);
}
float ring_opacity(float r){
 return 1.-exp(-solar_ring_tau(r/radius[6],.0001));
}
vec3 material(int body,vec3 n,vec3 normal_dx,vec3 normal_dy){
 vec3 q=surface_point(body,n); // Body-fixed clouds and surface, no boiling noise.
 float footprint=max(length(normal_dx),length(normal_dy));
 vec2 mapped_uv=vec2(atan(q.z,q.x)/TAU+.5,.5-asin(clamp(q.y,-1.,1.))/PI);
 vec2 mapped_dx=equirect_gradient(q,surface_point(body,normal_dx));
 vec2 mapped_dy=equirect_gradient(q,surface_point(body,normal_dy));
 // Observed maps take precedence over either optional display material.
 if(body==1&&mercury_enabled!=0)return textureGrad(mercury_map,mapped_uv,mapped_dx,mapped_dy).rgb;
 if(body==2&&venus_enabled!=0)return textureGrad(venus_map,mapped_uv,mapped_dx,mapped_dy).rgb;
 if(body==4&&mars_enabled!=0)return textureGrad(mars_map,mapped_uv,mapped_dx,mapped_dy).rgb;
 if(body==5&&jupiter_enabled!=0)return textureGrad(jupiter_map,mapped_uv,mapped_dx,mapped_dy).rgb;
 if(body==6&&saturn_enabled!=0)return textureGrad(saturn_map,mapped_uv,mapped_dx,mapped_dy).rgb;
 if(body==7&&uranus_enabled!=0)return textureGrad(uranus_map,mapped_uv,mapped_dx,mapped_dy).rgb;
 if(body==8&&neptune_enabled!=0)return textureGrad(neptune_map,mapped_uv,mapped_dx,mapped_dy).rgb;
 float detail=scientific_mode!=0?0.:surface_detail(q,footprint);
 if(scientific_mode!=0 && body!=3){
  // Until an observed map is available, expose an approximate mean appearance
  // instead of inventing named terrain, storms or crater geometry.
  if(body==1)return vec3(.44,.42,.40);
  if(body==2)return vec3(.91,.87,.75);
  if(body==4)return vec3(.57,.30,.16);
  if(body==5)return vec3(.78,.68,.54);
  // Low-contrast zonal cloud reflectance is an analytic display material, not
  // a synthetic storm map. Missing observations never acquire named terrain.
  float latitude=asin(clamp(q.y,-1.,1.));
  float zonal=cos(latitude*14.)*(1.-smoothstep(.15,.7,footprint*14.));
  if(body==6)return vec3(.85,.80,.68)*(1.+.035*zonal);
  if(body==7)return vec3(.61,.78,.80)*(1.+.010*zonal);
  return vec3(.46,.67,.77)*(1.+.020*zonal);
 }
 float fine=body==1?fbm(q*12.):0.;
 if(body==1){float rocks=fbm(q*5.);float craters=crater_detail(q,footprint);return mix(vec3(.20,.18,.17),vec3(.55,.50,.43),rocks)*(.88+.18*fine+.27*detail+.45*craters);}
 if(body==2){
  // Venus is an opaque, pale cloud deck in visible light, not exposed orange rock.
  float flow=fbm(q*vec3(3.,7.,3.));
  float cloud=fbm(q*vec3(7.,18.,7.)+vec3(flow*2.,0.,flow));
  return mix(vec3(.72,.66,.51),vec3(.94,.90,.76),smoothstep(.12,.86,cloud))*(.97+.03*flow+.07*detail);
 }
 if(body==3){
  vec2 uv=vec2(atan(q.z,q.x)/TAU+.5,.5-asin(clamp(q.y,-1.,1.))/PI);
  vec2 uv_dx=equirect_gradient(q,surface_point(body,normal_dx));
  vec2 uv_dy=equirect_gradient(q,surface_point(body,normal_dy));
  vec3 base=earth_enabled!=0?textureGrad(earth_map,uv,uv_dx,uv_dy).rgb:scientific_mode!=0?vec3(.04,.16,.28):mix(vec3(.025,.13,.32),vec3(.13,.27,.10),smoothstep(.48,.57,fbm(q*5.)));
  // Clouds are intersected and lit above this surface, with their own shadow.
  return base*(1.+.06*detail);
 }
 if(body==4){float rock=fbm(q*4.);float dust=fbm(q*13.);float craters=crater_detail(q,footprint);return mix(vec3(.27,.10,.05),vec3(.71,.38,.19),rock)*(.82+.25*dust+.22*detail+.34*craters)+vec3(.2)*smoothstep(.88,.96,abs(q.y));}
 if(body==5||body==6){
  // Unequal latitude zones with body-fixed shear and curled storm edges.
  // All noise is spatial; no time coordinate can make the clouds boil.
  float eddy=fbm(q*vec3(9.,16.,9.));
  float shear=noise(q*vec3(4.,28.,4.));
  float lat=q.y+.075*(eddy-.5)+.020*sin(q.x*15.+q.z*9.+eddy*9.);
  float zones=noise(vec3(lat*11.,2.3,7.1));
  float wisps=mix(.5,noise(q*vec3(25.,95.,25.)+vec3(eddy*4.,shear,eddy*3.)),detail_weight(footprint,95.));
  float belts=smoothstep(.08,.92,zones+.24*(eddy-.5));
  vec3 pale=body==5?vec3(.91,.85,.73):vec3(.88,.81,.65);
  vec3 dark=body==5?vec3(.60,.41,.27):vec3(.79,.71,.55);
  vec3 base=mix(dark,pale,belts);
  base*=.91+.13*wisps+.06*eddy+.12*detail;
  if(body==5){
   float longitude=atan(q.z,q.x);
   float dx=atan(sin(longitude-.6),cos(longitude-.6))/.25;
   float dy=(q.y+.23)/.095;
   float rr=length(vec2(dx,dy));
   float curl=sin(atan(dy,dx)*2.+rr*9.+eddy*4.);
   float spot=1.-smoothstep(.65,1.15,rr+.06*curl);
   vec3 storm=mix(vec3(.64,.31,.19),vec3(.79,.50,.31),.5+.22*curl);
   base=mix(base,storm,spot*.88);
  }
  return base;
 }
 float bands=.5+.2*sin(q.y*16.+fbm(q*vec3(4.,14.,4.))*1.3);
 return (body==7?mix(vec3(.32,.63,.66),vec3(.42,.70,.73),bands):mix(vec3(.035,.18,.52),vec3(.10,.32,.68),bands))*(1.+.08*detail);
}
float earth_clouds(vec3 direction,vec3 direction_dx,vec3 direction_dy){
 vec3 q=surface_point(3,direction);
 if(cloud_enabled==0)return scientific_mode!=0?0.:smoothstep(.58,.74,fbm(q*vec3(8.,12.,8.)));
 vec2 uv=vec2(atan(q.z,q.x)/TAU+.5,.5-asin(clamp(q.y,-1.,1.))/PI);
 vec2 dx=equirect_gradient(q,surface_point(3,direction_dx));
 vec2 dy=equirect_gradient(q,surface_point(3,direction_dy));
 return smoothstep(.12,.88,textureGrad(cloud_map,uv,dx,dy).r);
}

float ocean_reflection(vec3 normal,vec3 light,vec3 view,float footprint){
 float nl=max(dot(normal,light),0.),nv=max(dot(normal,view),0.);
 if(nl<=0. || nv<=0.)return 0.;
 vec3 half_sum=light+view;
 if(dot(half_sum,half_sum)<.000001)return 0.;
 vec3 h=normalize(half_sum);
 float nh=max(dot(normal,h),0.),vh=max(dot(view,h),0.);
 // Unresolved glitter broadens instead of flashing on/off as the body shrinks.
 float roughness=.17+min(.28,footprint*.8),alpha=roughness*roughness;
 float alpha2=alpha*alpha,denom=nh*nh*(alpha2-1.)+1.;
 float distribution=alpha2/(PI*denom*denom);
 float geometry_l=2.*nl/(nl+sqrt(alpha2+(1.-alpha2)*nl*nl));
 float geometry_v=2.*nv/(nv+sqrt(alpha2+(1.-alpha2)*nv*nv));
 float fresnel=.0204+.9796*pow(1.-vh,5.);
 return min(8.,distribution*geometry_l*geometry_v*fresnel/(4.*max(nv,.001)));
}

vec3 earth_layers(vec3 color,vec3 encoded,vec3 point,vec3 normal,vec3 light,
                  vec3 ray,vec3 ray_dx,vec3 ray_dy,vec3 direction_dx,vec3 direction_dy){
 vec3 c=center(3),direction=normalize(point-c);
 float footprint=max(length(direction_dx),length(direction_dy));
 float nl=max(dot(normal,light),0.);
 float cloud_radius=radius[3]*1.0022;
 if(nl>0.){
  vec3 relative=point-c;
  float b=dot(relative,light);
  float to_cloud=-b+sqrt(max(0.,b*b+cloud_radius*cloud_radius-dot(relative,relative)));
  vec3 shadow_direction=normalize(relative+light*to_cloud);
  float cloud_shadow=earth_clouds(shadow_direction,direction_dx,direction_dy);
  color*=1.-.56*cloud_shadow;
  // The bundled Blue Marble map supplies an approximate ocean mask through
  // blue chroma; it is not a separate measured land/water classification map.
  float water=smoothstep(.008,.11,encoded.b-max(encoded.r,encoded.g)*.88);
  water*=1.-smoothstep(.5,1.35,encoded.r+encoded.g+encoded.b);
  color+=vec3(1.0,.985,.95)*ocean_reflection(normal,light,-ray,footprint)
           *water*(1.-.94*cloud_shadow);
 }
 // A separate shell produces parallax and day/night shading at cloud altitude.
 float t=sphere(camera_eye,ray,c,cloud_radius);
 if(t<1e4){
  vec3 cp=camera_eye+ray*t,cn=normalize(cp-c);
  float incidence=min(-.035,dot(cn,ray));
  vec3 dx=(ray_dx-ray*(dot(cn,ray_dx)/incidence))*(t/cloud_radius);
  vec3 dy=(ray_dy-ray*(dot(cn,ray_dy)/incidence))*(t/cloud_radius);
  float coverage=earth_clouds(cn,dx,dy);
  float sunlight=max(dot(cn,light),0.);
  float forward=pow(max(dot(ray,light),0.),12.);
  vec3 cloud=vec3(.92,.955,1.)*(.0018+1.25*sunlight);
  cloud+=vec3(.30,.26,.18)*forward*smoothstep(-.02,.08,dot(cn,light))*(1.-coverage)*.2;
  color=mix(color,cloud,coverage*.94);
 }
 return color;
}

void atmosphere_parameters(int body,out float extent,out float scale_height,
                            out vec3 extinction,out vec3 tint){
 extent=.016;scale_height=.003;extinction=vec3(7.,15.,30.);tint=vec3(.28,.56,.9);
 if(body==2){extent=.020;scale_height=.004;extinction=vec3(16.,19.,24.);tint=vec3(.88,.77,.54);}
 if(body==3){extent=.018;scale_height=.003;extinction=vec3(5.8,13.5,33.1);tint=vec3(.42,.66,1.);}
 if(body==4){extent=.018;scale_height=.003;extinction=vec3(3.,4.,6.);tint=vec3(.85,.40,.18);}
 if(body==5){extent=.010;scale_height=.0018;extinction=vec3(9.,14.,24.);tint=vec3(.74,.69,.59);}
 if(body==6){extent=.013;scale_height=.0022;extinction=vec3(9.,15.,25.);tint=vec3(.84,.76,.59);}
 if(body==7){tint=vec3(.48,.76,.82);}
}

vec3 aerial_perspective(vec3 color,vec3 ray,float visible_depth,float angular_pixel){
 // Pick the nearest visible atmospheric envelope before doing any integration.
 // Each pixel pays at most SIX steps, including multi-body overview pixels.
 int atmosphere_body=-1;float first=1e5,last=0.;
 for(int i=2;i<9;i++){
  if(focus_enabled!=0&&focused_body!=i)continue;
  float extent,h;vec3 beta,tint;
  atmosphere_parameters(i,extent,h,beta,tint);
  float outer_radius=radius[i]*(1.+extent);
  float outer_ratio=(polar_ratio[i]+extent)/(1.+extent);
  vec2 interval=spheroid_interval(camera_eye,ray,center(i),outer_radius,body_axis(i),outer_ratio);
  float start=max(0.,interval.x),finish=min(visible_depth,interval.y);
  if(finish>start&&start<first){atmosphere_body=i;first=start;last=finish;}
 }
 if(atmosphere_body<0)return color;
 int body=atmosphere_body;
 float extent,height;vec3 beta,tint;
 atmosphere_parameters(body,extent,height,beta,tint);
 // Keep the nadir view transparent. Density and exposure are conservative
 // display parameters; long grazing paths alone build the visible blue limb.
 beta*=.32;
 vec3 c=center(body),axis=body_axis(body);
 float r=radius[body],step_size=(last-first)/6.;
 vec3 transmission=vec3(1.),scattered=vec3(0.);
 for(int j=0;j<6;j++){
  vec3 point=camera_eye+ray*(first+(float(j)+.5)*step_size),q=(point-c)/r;
  vec3 mq=spheroid_metric(q,axis,polar_ratio[body]);
  float altitude=max(0.,sqrt(max(0.,dot(q,mq)))-1.);
  float density=exp(-altitude/height)*(1.-smoothstep(extent*.72,extent,altitude));
  vec3 light=normalize(-c),ml=spheroid_metric(light,axis,polar_ratio[body]);
  float a=dot(light,ml),b=dot(q,ml);
  float impact=sqrt(max(0.,dot(q,mq)-b*b/a));
  float softness=max(.0008,min(.015,angular_pixel*(first+float(j)*step_size)/r));
  float sunlight=b<0.?smoothstep(1.-softness,1.+softness,impact):1.;
  if(body==6){
   float denominator=dot(light,axis);
   if(abs(denominator)>.0001){
    float to_ring=-dot(q,axis)/denominator;
    if(to_ring>0.){
     float at_ring=length(q+light*to_ring);
     sunlight*=exp(-solar_ring_tau(at_ring,max(softness,.0001))/max(abs(denominator),.005));
    }
   }
  }
  float mu=dot(normalize(mq),light);
  // Bounded Chapman-like slant path plus Rayleigh phase. This is a six-sample
  // display atmosphere, not a spectral multiple-scattering weather simulation.
  vec3 sun_transmission=exp(-beta*(density*height/max(.08,mu+.12)));
  float cosine=dot(ray,light),rayleigh=.75*(1.+cosine*cosine);
  float forward=pow(max(cosine,0.),16.);
  vec3 segment=exp(-beta*(density*step_size/r));
  vec3 illumination=(tint*rayleigh+vec3(1.,.86,.66)*forward*.32)*sunlight;
  scattered+=transmission*(1.-segment)*sun_transmission*illumination;
  transmission*=segment;
 }
 return color*transmission+scattered;
}

// JWM_COSMIC_SKY_INCLUDE
// J2000 ecliptic (renderer x, -z, y) -> ICRS -> Galactic catalogue.
// Preserve the fixed catalogue framing API used by the independent sky viewer.
vec3 solar_catalog_direction(vec3 world){
 vec3 e=vec3(world.x,-world.z,world.y);
 vec3 q=vec3(e.x,.9174820621*e.y-.3977771559*e.z,.3977771559*e.y+.9174820621*e.z);
 vec3 g=vec3(dot(vec3(-.0548755604,-.8734370902,-.4838350155),q),
             dot(vec3(-.8676661490,-.1980763734,.4559837762),q),
             dot(vec3(.4941094279,-.4448296300,.7469822445),q));
 return vec3(-.3746065934*g.y-.9271838546*g.z,.9271838546*g.y-.3746065934*g.z,g.x);
}
vec3 stars(vec3 ray,vec3 ray_dx,vec3 ray_dy){
 vec3 color=ephemeris_enabled!=0
  ?cosmic_sky(solar_catalog_direction(ray),solar_catalog_direction(ray_dx),solar_catalog_direction(ray_dy),sky_enabled!=0)
  :cosmic_sky(ray,ray_dx,ray_dy,sky_enabled!=0);
 // The shared catalogue keeps its independent Galaxy exposure. Sunlit planets
 // here need a darker sky; turning away reveals more of the distant catalogue.
 vec3 forward=camera_basis_enabled!=0?camera_basis[2]:normalize(-camera_eye);
 float sun_in_frame=(focus_enabled==0||focused_body==0)?smoothstep(.50,.90,dot(forward,normalize(-camera_eye))):0.;
 float exposure=mix(.46,.18,sun_in_frame);
 if(focus_enabled!=0&&focused_body==0)exposure=.07;
 return color*exposure;
}
vec3 solar_ray(vec2 pixel,out float inverse_length,out vec3 right,out vec3 up){
 float pixels=max(min(resolution.x,resolution.y),1.);
 vec2 uv=(pixel-.5*resolution)/pixels;
 vec3 forward;
 // A slightly wider lens keeps the full system in square/portrait outputs.
 float focal=resolution.x>=resolution.y*1.25?1.5:1.2*max(1.,resolution.x/max(resolution.y,1.));
 if(camera_basis_enabled!=0){right=camera_basis[0];up=camera_basis[1];forward=camera_basis[2];focal=max(camera_focal,.01);}
 else{forward=normalize(-camera_eye);right=normalize(cross(forward,vec3(0,1,0)));up=cross(right,forward);}
 vec3 view=forward*focal+right*uv.x+up*uv.y;
 inverse_length=inversesqrt(dot(view,view));
 return view*inverse_length;
}
vec3 shade(vec2 pixel, bool classify, out bool needs_aa){
 needs_aa=false;
 vec3 ring_normal=body_axis(6);
 float pixels=max(min(resolution.x,resolution.y),1.);
 float inverse_length;vec3 right,up;
 vec3 ray=solar_ray(pixel,inverse_length,right,up);
 float angular_pixel=inverse_length/pixels;
 vec3 ray_dx=(right-ray*dot(ray,right))*angular_pixel;
 vec3 ray_dy=(up-ray*dot(ray,up))*angular_pixel;
 float nearest=1e5;int body=-1;
 for(int i=0;i<9;i++){
  if(focus_enabled!=0&&focused_body!=i)continue;
  float t=body_intersection(i,camera_eye,ray);if(t<nearest){nearest=t;body=i;}
  vec3 axis=body_axis(i),offset=camera_eye-center(i),md=spheroid_metric(ray,axis,polar_ratio[i]);
  float a=dot(ray,md),b=dot(offset,md),along=-b/a;
  float impact2=dot(offset,spheroid_metric(offset,axis,polar_ratio[i]))-b*b/a;
  float footprint=along*angular_pixel/polar_ratio[i];
  if(along>0.&&abs(impact2-radius[i]*radius[i])<2.*radius[i]*footprint+footprint*footprint)needs_aa=true;
 }
 // Analytic ray/plane derivatives filter radial ringlets independently of the
 // sphere texture footprint. The centre classifier avoids shading a fifth ray.
 float ring_t=1e5,ring_footprint=0.;vec3 ring_point=vec3(0.);
 vec3 sat=center(6);float ring_denom=dot(ray,ring_normal);
 if((focus_enabled==0||focused_body==6)&&abs(ring_denom)>.0001){
  float t=dot(sat-camera_eye,ring_normal)/ring_denom;
  if(t>0.&&t<nearest){
   vec3 point=camera_eye+ray*t-sat;
   float r=length(point)/radius[6];
   if(r>=1.24&&r<=2.269){
    vec3 radial=normalize(point);
    vec3 dx=(ray_dx-ray*(dot(ray_dx,ring_normal)/ring_denom))*t;
    vec3 dy=(ray_dy-ray*(dot(ray_dy,ring_normal)/ring_denom))*t;
    ring_footprint=length(vec2(dot(radial,dx),dot(radial,dy)))/radius[6];
    ring_t=t;ring_point=point/radius[6];needs_aa=true;
   }
  }
 }
 // The center sample is discarded for antialiased pixels; skip its material work.
 if(classify&&needs_aa)return vec3(0);
 vec3 color=body<0?stars(ray,ray_dx,ray_dy):vec3(0);
 // Each ellipse uses the same date-derived plane, apsides and display scale
 // as its planet. This avoids circular guides beneath inclined true positions.
 if(focus_enabled!=0){}else if(ephemeris_enabled!=0){
  float guide=0.;
  for(int i=0;i<8;i++){
   vec3 u=ellipse_u[i],v=ellipse_v[i],normal=normalize(cross(u,v));
   float denominator=dot(ray,normal);
   if(abs(denominator)<.00001)continue;
   float t=dot(ellipse_centers[i]-camera_eye,normal)/denominator;
   if(t<=0.||t>=nearest)continue;
   vec3 q=camera_eye+ray*t-ellipse_centers[i];
   vec2 elliptic=vec2(dot(q,u)/dot(u,u),dot(q,v)/dot(v,v));
   float separation=abs(length(elliptic)-1.)*min(length(u),length(v));
   float width=max(.004,t/pixels*.38);
   guide=max(guide,exp(-pow(separation/width,2.)));
  }
  color+=vec3(.003,.005,.008)*guide;
 }else if(abs(ray.y)>.0001){float t=-camera_eye.y/ray.y;if(t>0.&&t<nearest){vec3 p=camera_eye+ray*t;float r=length(p.xz);float width=max(.004,t/pixels*.38);float guide=0.;for(int i=0;i<8;i++)guide=max(guide,exp(-pow((r-orbit[i])/width,2.)));color+=vec3(.003,.005,.008)*guide;}}
 if(body>=0){
  vec3 c=center(body),p=camera_eye+ray*nearest,offset=p-c;
  vec3 n=normalize(spheroid_metric(offset,body_axis(body),polar_ratio[body]));
  vec3 direction=normalize(offset),direction_dx,direction_dy;
  // Differentiate the implicit ellipsoid, then normalize POSITION for mapped
  // planetocentric latitude. The geometric light normal must not move cloud bands.
  float incidence=dot(n,ray);
  float limit=sqrt(min(1.,nearest*angular_pixel/radius[body]));
  incidence=incidence<0.?min(incidence,-limit):max(incidence,limit);
  vec3 dpdx=(ray_dx-ray*(dot(n,ray_dx)/incidence))*nearest;
  vec3 dpdy=(ray_dy-ray*(dot(n,ray_dy)/incidence))*nearest;
  direction_dx=(dpdx-direction*dot(direction,dpdx))/length(offset);
  direction_dy=(dpdy-direction*dot(direction,dpdy))/length(offset);
  float footprint=max(length(direction_dx),length(direction_dy));
  float view_cos=max(dot(n,-ray),0.);
  if(body==0){
   color=solar_photosphere(rotate_y(direction,surface_phase),view_cos,footprint,scientific_mode!=0)
          *(focus_enabled!=0?.52:1.);
  } else {
   // Sunlight is effectively parallel over each body. Using the enlarged
   // display radius to vary it would create a fictitious nearby point light.
   vec3 light=normalize(-c);float cosine=dot(n,light),diffuse=max(0.,cosine);
   if(body==1||body==4){
    // Rough regolith has a flatter disc response than a plastic Lambert sphere.
    diffuse=.40*diffuse+.60*diffuse/max(diffuse+view_cos,.06);
   }
   float ring_shadow=1.;
   if(body==6){
    float denom=dot(light,ring_normal);
    if(abs(denom)>.0001){
     float t=dot(c-p,ring_normal)/denom;
     if(t>.0001){
      float r=length(p+light*t-c)/radius[6];
      vec3 radial=normalize(p+light*t-c);
      vec3 shadow_dx=dpdx-light*(dot(dpdx,ring_normal)/denom);
      vec3 shadow_dy=dpdy-light*(dot(dpdy,ring_normal)/denom);
      float shadow_footprint=max(.0001,length(vec2(dot(radial,shadow_dx),dot(radial,shadow_dy)))/radius[6]);
      ring_shadow=exp(-solar_ring_tau(r,shadow_footprint)/max(abs(denom),.005));
     }
    }
   }
   vec3 albedo=material(body,direction,direction_dx,direction_dy);
   float ambient=scientific_mode!=0?.0012:.018;
   color=solar_linear(albedo)*(ambient+1.40*diffuse*ring_shadow);
   if(body==3)color=earth_layers(color,albedo,p,n,light,ray,ray_dx,ray_dy,direction_dx,direction_dy);
  }
 }
 if(focus_enabled==0||focused_body==0){
  color+=solar_corona(camera_eye,ray,center(0),radius[0],nearest,angular_pixel,scientific_mode!=0);
  color+=solar_imaging_glow(camera_eye,ray,center(0),radius[0],nearest,angular_pixel,scientific_mode!=0);
 }
 color=aerial_perspective(color,ray,nearest,angular_pixel);
 // The same radial optical-depth model transmits the background and shadows
 // Saturn. Reflection and transmission differ on the lit and unlit ring faces.
 if(ring_t<nearest){
  float sun_angle=.00465047/max(.1,semimajor_au[5]*length(sat)/orbit[5]);
  vec4 ring=solar_ring_scatter(ring_point,ring_normal,-ray,normalize(-sat),
                              ring_footprint,sun_angle,scientific_mode!=0,polar_ratio[6]);
  color=mix(color,ring.rgb,ring.a);
 }
 return color;
}
vec3 tone(vec3 x){return clamp((x*(2.51*x+.03))/(x*(2.43*x+.59)+.14),0.,1.);}
void main(){
 // Filtered interior materials need one ray; silhouettes and thin rings pay for
 // four fixed subpixel rays. Enlarged planets do not multiply all material work.
 vec2 pixel=gl_FragCoord.xy-canvas_origin;
 bool needs_aa;vec3 color=shade(pixel,true,needs_aa);
 if(needs_aa){color=vec3(0);for(int i=0;i<4;i++){
  vec2 offset=vec2(float(i&1),float(i>>1))*.5-.25;bool unused;
  color+=shade(pixel+offset,false,unused)*.25;
 }}
 frag_color=vec4(pow(tone(color),vec3(1./2.2)),1.);
}
