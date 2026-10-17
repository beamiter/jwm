#version 330 core
// Original, fixed-cost 3D illustration. Readable scales/speeds, not ephemerides.
out vec4 frag_color;
uniform vec2 resolution;
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
// Full right-hand-rule obliquities: >90 degrees naturally rotates retrograde.
const float obliquity[8]=float[8](.034,177.4,23.44,25.19,3.1,26.7,97.8,28.3);
vec3 body_axis(int body){
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
vec3 center(int i){if(i==0)return vec3(0);float a=orbit_phase[i-1];return vec3(cos(a)*orbit[i-1],sin(a)*.025*float(i),sin(a)*orbit[i-1]);}
float sphere(vec3 ro,vec3 rd,vec3 c,float r){vec3 q=ro-c;float b=dot(q,rd),d=b*b-dot(q,q)+r*r;if(d<0.)return 1e5;float t=-b-sqrt(d);if(t>0.)return t;t=-b+sqrt(d);return t>0.?t:1e5;}
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
 // Outside the feathered annulus opacity is exactly zero.
 if(r<=.96||r>=1.68)return 0.;
 float edge=smoothstep(.96,1.01,r)*(1.-smoothstep(1.61,1.68,r));
 float cassini=1.-.8*exp(-pow((r-1.43)/.022,2.));
 float structure=.59+.10*sin(r*129.)+.045*sin(r*337.);
 return clamp(edge*cassini*structure,0.,.85);
}
vec3 material(int body,vec3 n,vec3 normal_dx,vec3 normal_dy){
 vec3 q=surface_point(body,n); // Body-fixed clouds and surface, no boiling noise.
 float footprint=max(length(normal_dx),length(normal_dy));
 float detail=surface_detail(q,footprint);
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
  vec3 base=earth_enabled!=0?textureGrad(earth_map,uv,uv_dx,uv_dy).rgb:mix(vec3(.025,.13,.32),vec3(.13,.27,.10),smoothstep(.48,.57,fbm(q*5.)));
  float clouds=cloud_enabled!=0?smoothstep(.12,.88,textureGrad(cloud_map,uv,uv_dx,uv_dy).r):smoothstep(.58,.74,fbm(q*vec3(8.,12.,8.)));
  return mix(base*(1.+.06*detail),vec3(.94,.96,.98),clouds*.88);
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
// JWM_COSMIC_SKY_INCLUDE
vec3 stars(vec3 ray,vec3 ray_dx,vec3 ray_dy){
 return cosmic_sky(ray,ray_dx,ray_dy,sky_enabled!=0);
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

 float nearest=1e5;int body=-1;
 for(int i=0;i<9;i++){
  vec3 c=center(i);float t=sphere(camera_eye,ray,c,radius[i]);if(t<nearest){nearest=t;body=i;}
  vec3 offset=c-camera_eye;float along=dot(offset,ray);float impact2=dot(offset,offset)-along*along;float footprint=along/pixels*1.5;
  if(along>0.&&abs(impact2-radius[i]*radius[i])<2.*radius[i]*footprint+footprint*footprint)needs_aa=true;
 }
 // The center sample is discarded for antialiased pixels; skip its material work.
 if(classify&&needs_aa)return vec3(0);
 vec3 ray_dx=(right-ray*dot(ray,right))*(inverse_length/pixels);
 vec3 ray_dy=(up-ray*dot(ray,up))*(inverse_length/pixels);
 vec3 color=body<0?stars(ray,ray_dx,ray_dy):vec3(0);
 // Feathered guides on the real orbital plane: deliberately subordinate to planets.
 if(abs(ray.y)>.0001){float t=-camera_eye.y/ray.y;if(t>0.&&t<nearest){vec3 p=camera_eye+ray*t;float r=length(p.xz);float width=max(.004,t/pixels*.38);float guide=0.;for(int i=0;i<8;i++)guide=max(guide,exp(-pow((r-orbit[i])/width,2.)));color+=vec3(.007,.012,.019)*guide;}}
 if(body>=0){
  vec3 c=center(body),p=camera_eye+ray*nearest,n=normalize(p-c);
  if(body==0){
   vec3 q=rotate_y(n,surface_phase);float cells=fbm(q*18.);float granules=noise(q*100.);
   vec3 hot=mix(vec3(3.8,1.24,.15),vec3(6.,3.4,.9),cells);
   color=hot*(.93+.07*granules)*(.68+.32*pow(max(0.,dot(n,-ray)),.4));
  } else {
   vec3 light=normalize(-p);float cosine=dot(n,light);float diffuse=max(0.,cosine);
   if(body==6){float denom=dot(light,ring_normal);if(abs(denom)>.001){float t=dot(c-p,ring_normal)/denom;if(t>.001){float r=length(p+light*t-c);diffuse*=1.-ring_opacity(r)*.8;}}}
   vec3 normal_dx=vec3(0),normal_dy=vec3(0);
   {
    // Differentiate the sphere intersection analytically. Neighbor fragments
    // may hit another body or only sky, so implicit material derivatives are unsafe.
    // All surfaces use this footprint to reveal filtered detail on approach.
    float incidence=dot(n,ray);
    float limit=sqrt(min(1.,nearest*inverse_length/(pixels*radius[body])));
    incidence=incidence<0.?min(incidence,-limit):max(incidence,limit);
    normal_dx=(ray_dx-ray*(dot(n,ray_dx)/incidence))*(nearest/radius[body]);
    normal_dy=(ray_dy-ray*(dot(n,ray_dy)/incidence))*(nearest/radius[body]);
   }
   vec3 albedo=material(body,n,normal_dx,normal_dy);float rim=pow(1.-max(0.,dot(n,-ray)),3.);
   // A restrained cool fill keeps the night hemisphere readable in an illustration.
   color=pow(albedo,vec3(2.2))*(.13+2.3*diffuse)+pow(albedo,vec3(2.2))*vec3(.025,.04,.07)*max(0.,dot(n,-ray));
   if(body==3){float spec=pow(max(0.,dot(reflect(-light,n),-ray)),32.);color+=vec3(.32,.48,.65)*spec*diffuse;color+=vec3(.025,.12,.32)*rim*(.2+diffuse);}
   if(body==2)color+=vec3(.24,.13,.035)*rim*diffuse;
   if(body==7||body==8)color+=vec3(.015,.08,.18)*rim*(.3+diffuse);
  }
 }
 // Corona is in scene depth, so foreground planets/rings mask the bloom.
 float along=dot(-camera_eye,ray),miss=length(camera_eye+ray*along);
 if(along>0.&&(body==0||along<nearest)){
  float edge=max(0.,miss-radius[0]);
  color+=vec3(1.0,.36,.065)*(.45*exp(-edge*5.)+.10*exp(-edge*1.35)+.018*exp(-edge*.35));
 }
 // Saturn's front ring is translucent; its rear portion is hidden by the sphere.
 vec3 sat=center(6);float denom=dot(ray,ring_normal);
 if(abs(denom)>.0001){float t=dot(sat-camera_eye,ring_normal)/denom;
  if(t>0.&&t<nearest){vec3 p=camera_eye+ray*t;float r=length(p-sat);float opacity=ring_opacity(r);
   if(opacity>.001){needs_aa=true;vec3 light=normalize(-p);float shadow=sphere(p+light*.002,light,sat,radius[6])<1e4?.08:1.;
    float facing=.30+.7*abs(dot(ring_normal,light));vec3 dust=mix(vec3(.43,.34,.23),vec3(.83,.75,.58),smoothstep(.98,1.4,r));
    vec3 ring=pow(dust,vec3(2.2))*(.15+1.8*facing*shadow);color=mix(color,ring,opacity);nearest=t;
   }
  }
 }
 return color;
}
vec3 tone(vec3 x){return clamp((x*(2.51*x+.03))/(x*(2.43*x+.59)+.14),0.,1.);}
void main(){
 // Filtered interior materials need one ray; silhouettes and thin rings pay for
 // four fixed subpixel rays. Enlarged planets do not multiply all material work.
 bool needs_aa;vec3 color=shade(gl_FragCoord.xy,true,needs_aa);
 if(needs_aa){color=vec3(0);for(int i=0;i<4;i++){
  vec2 offset=vec2(float(i&1),float(i>>1))*.5-.25;bool unused;
  color+=shade(gl_FragCoord.xy+offset,false,unused)*.25;
 }}
 frag_color=vec4(pow(tone(color),vec3(1./2.2)),1.);
}
