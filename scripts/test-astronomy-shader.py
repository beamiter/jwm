#!/usr/bin/env python3
"""Compile/link/render actual wallpaper on surfaceless Mesa desktop and GLES.
No window system, installed dependencies only. Outputs PNGs and JSON metrics.
"""
import ctypes as C, os, json, hashlib, subprocess, datetime, tempfile, shutil, math
from pathlib import Path
from PIL import Image, ImageDraw, ImageChops, ImageStat
os.environ.setdefault('LIBGL_ALWAYS_SOFTWARE','1')
os.environ.setdefault('MESA_SHADER_CACHE_DISABLE','true')
REPO=Path(__file__).resolve().parents[1]
ROOT=Path(os.environ.get('ASTRONOMY_TEST_OUTPUT', tempfile.mkdtemp(prefix='jwm-astronomy-')))
ROOT.mkdir(parents=True, exist_ok=True)
SOURCE=REPO/'src/backend/x11/compositor/shaders/animated_wallpaper.frag'
W,H=int(os.environ.get("PREVIEW_WIDTH",960)),int(os.environ.get("PREVIEW_HEIGHT",540))
DEFAULT_LATITUDE,DEFAULT_LONGITUDE=23.13,113.28  # Guangzhou, explicit city-level choice.
helper=ROOT/'astronomy_values.rs'
helper.write_text('#[path = '+json.dumps(str(REPO/'src/backend/compositor_common/animated_wallpaper.rs'))+'] mod astronomy;\nuse astronomy as animated_wallpaper;\n#[path = '+json.dumps(str(REPO/'src/backend/compositor_common/panorama.rs'))+'] mod panorama;\n'+r"""
fn main() {
 let a:Vec<String>=std::env::args().skip(1).collect();
 let mode=&a[0]; let t:f64=a[1].parse().unwrap();
 if mode=="panorama" {
  let mut view=panorama::PanoramaView::default();view.zoom(t);
  let w:f64=a[4].parse().unwrap();let h:f64=a[5].parse().unwrap();
  let drag=view.begin_drag([w*0.5,h*0.5],[w,h]).unwrap();
  view.set_orientation(drag.orientation_at([w*0.5+a[2].parse::<f64>().unwrap(),h*0.5+a[3].parse::<f64>().unwrap()]).unwrap());
  print!("{} ",view.fov_degrees());for value in view.matrix(){print!("{} ",value);}println!();return;
 }
 if matches!(mode.as_str(),"earth-frame"|"moon-frame"|"earth-drag-frame"|"moon-drag-frame") {
  let view=astronomy::CelestialView {
   rotating:a[3]=="true", latitude:a[4].parse().unwrap(),
   longitude:a[5].parse().unwrap(), roll:a[6].parse().unwrap(),
   axis:[a[7].parse().unwrap(),a[8].parse().unwrap(),a[9].parse().unwrap()],
  };
  let period=a[2].parse().unwrap();
  let matrix=if mode.contains("-drag-") {
   let pressed:f64=a[10].parse().unwrap();
   let initial=astronomy::CelestialRotation::from_matrix(&view.matrix(pressed,period));
   let arcball=astronomy::CelestialArcball::new(
    [a[11].parse().unwrap(),a[12].parse().unwrap()],initial).unwrap();
   arcball.orientation_at([a[13].parse().unwrap(),a[14].parse().unwrap()]).unwrap().matrix()
  } else {view.matrix(t,period)};
  let sun=if mode.starts_with("moon-") {
   astronomy::transform_direction(&matrix,astronomy::lunar_phase_light(t))
  } else {astronomy::solar_direction(t)};
  for value in sun.iter().chain(matrix.iter()) { print!("{} ",value); }
  println!(); return;
 }
 if mode=="sky" {
  let r=astronomy::sky_rotation(t,a[2].parse().unwrap());
  println!("{} {}",r[0],r[1]); return;
 }
 let (sun,longitude)=match mode.as_str() {
  // Explicit historical controls retain the baseline files and old pixel comparisons.
  "legacy-clock" => (astronomy::solar_direction(t),astronomy::viewing_longitude(t,a[2].parse().unwrap())),
  "moon" => {
   let r=astronomy::celestial_longitude(t,a[3].parse().unwrap(),0);
   (astronomy::lunar_sun_direction(t,r),r)
  },
  "legacy-studio" => {
   let r=astronomy::celestial_longitude(t,a[3].parse().unwrap(),a[2].parse().unwrap());
   let (s,c)=r.sin_cos(); ([c* -0.35+s*0.88,0.32,-s* -0.35+c*0.88],r)
  },
  "earth" => (astronomy::solar_direction(t),astronomy::earth_view_longitude(t,a[2].parse().unwrap(),a[3].parse().unwrap(),a[4]=="true")),
  _ => panic!("unknown view mode"),
 };
 println!("{} {} {} {}",sun[0],sun[1],sun[2],longitude);
}
""")
RUSTC=os.environ.get('RUSTC') or shutil.which('rustc') or str(REPO.parent/'toolchain/bin/rustc')
subprocess.run([RUSTC,'--edition=2024',str(helper),'-Awarnings','-o',str(ROOT/'astronomy_values')],check=True)
def panorama_frame(steps=0,dx=0,dy=0,width=640,height=480):
 values=list(map(float,subprocess.check_output([str(ROOT/'astronomy_values'),'panorama',*map(str,[steps,dx,dy,width,height])]).split()))
 assert len(values)==10 and all(math.isfinite(v) for v in values)
 return values[0],values[1:]
def astronomy_values(instant,mode='earth',minutes=1440,longitude=DEFAULT_LONGITUDE,rotating=False,offset_seconds=0):
 args=[mode,str(instant)]
 args.extend([str(minutes),str(longitude),str(rotating).lower()] if mode=='earth' else [str(offset_seconds),str(minutes)])
 return list(map(float,subprocess.check_output([str(ROOT/'astronomy_values'),*args]).split()))
def celestial_frame(instant,body='earth',minutes=1440,rotating=False,latitude=DEFAULT_LATITUDE,longitude=DEFAULT_LONGITUDE,roll=0,axis=(0,1,0)):
 # Matrix order, axis normalization and lunar-light rotation all come from Rust.
 args=[f'{body}-frame',instant,minutes,str(rotating).lower(),latitude,longitude,roll,*axis]
 values=list(map(float,subprocess.check_output([str(ROOT/'astronomy_values'),*map(str,args)]).split()))
 assert len(values)==12 and all(math.isfinite(v) for v in values)
 return values[:3],values[3:]
def celestial_drag_frame(instant,body='earth',minutes=1440,rotating=False,latitude=DEFAULT_LATITUDE,longitude=DEFAULT_LONGITUDE,roll=0,axis=(0,1,0),start=(0,0),end=(0,0),pressed=None):
 # Use the real normalized-quaternion arcball, including its outside-limb
 # projection. Sun/phase sample the render instant while the pose is held.
 args=[f'{body}-drag-frame',instant,minutes,str(rotating).lower(),latitude,longitude,roll,*axis,instant if pressed is None else pressed,*start,*end]
 values=list(map(float,subprocess.check_output([str(ROOT/'astronomy_values'),*map(str,args)]).split()))
 assert len(values)==12 and all(math.isfinite(v) for v in values)
 return values[:3],values[3:]
# Invoke the production Rust geometry planner; do not copy coordinate policy to Python.
planner=ROOT/'astronomy_plan.rs'
planner.write_text('#[path = '+json.dumps(str(REPO/'src/backend/compositor_common/animated_wallpaper.rs'))+'] mod astronomy;\n'+r"""
fn main() {
 let a:Vec<String>=std::env::args().skip(1).collect();
 let n:Vec<i64>=a.iter().map(|x|x.parse().unwrap()).collect();
 let outputs:Vec<_>=n[7..].chunks_exact(4).map(|v|(v[0] as i32,v[1] as i32,v[2] as u32,v[3] as u32)).collect();
 let repair=if n[3]<0 {None} else {Some((n[3] as i32,n[4] as i32,n[5] as i32,n[6] as i32))};
 let plan=astronomy::astronomy_draw_plan((n[0] as u32,n[1] as u32),&outputs,n[2]!=0,repair);
 print!("[");
 for (i,d) in plan.iter().enumerate() {
  if i>0 {print!(",");}
  print!("{{\"origin\":[{},{}],\"size\":[{},{}],\"radius\":{},\"clip\":[{},{},{},{}]}}",d.origin[0],d.origin[1],d.size[0],d.size[1],d.radius,d.clip.0,d.clip.1,d.clip.2,d.clip.3);
 }
 println!("]");
}
""")
subprocess.run([RUSTC,'--edition=2024',str(planner),'-Awarnings','-o',str(ROOT/'astronomy_plan')],check=True)
def layout_plan(width,height,outputs,per_output=True,repair=None):
 args=[width,height,int(per_output),*(repair if repair is not None else (-1,0,0,0)),*(v for output in outputs for v in output)]
 return json.loads(subprocess.check_output([str(ROOT/'astronomy_plan'),*map(str,args)]))
BASE=datetime.datetime(2026,10,15,tzinfo=datetime.timezone.utc).timestamp()
E=C.CDLL('libEGL.so.1'); P=C.c_void_p; I=C.c_int; U=C.c_uint; F=C.c_float
E.eglGetProcAddress.argtypes=[C.c_char_p]; E.eglGetProcAddress.restype=P

def egl(name, result, args):
 f=getattr(E,name); f.restype=result; f.argtypes=args; return f

def gl(name, result, args):
 addr=E.eglGetProcAddress(name.encode()); assert addr,name
 return C.CFUNCTYPE(result,*args)(addr)

def attrs(*xs): return (I*len(xs))(*xs)
platform=C.CFUNCTYPE(P,U,P,C.POINTER(I))(E.eglGetProcAddress(b'eglGetPlatformDisplayEXT'))
d=platform(0x31DD,None,None); major=I();minor=I()
assert egl('eglInitialize',U,[P,C.POINTER(I),C.POINTER(I)])(d,C.byref(major),C.byref(minor))
choose=egl('eglChooseConfig',U,[P,C.POINTER(I),C.POINTER(P),I,C.POINTER(I)])
bind=egl('eglBindAPI',U,[U]); context=egl('eglCreateContext',P,[P,P,P,C.POINTER(I)])
surface=egl('eglCreatePbufferSurface',P,[P,P,C.POINTER(I)])
current=egl('eglMakeCurrent',U,[P,P,P,P])
results={}
PREVIEW_INSTANT=datetime.datetime.now(datetime.timezone.utc).timestamp()
source=SOURCE.read_text().replace('// JWM_COSMIC_SKY_INCLUDE',(REPO/'src/backend/x11/compositor/shaders/cosmic_sky.glsl').read_text()).replace('// JWM_PLANET_SURFACE_INCLUDE',(REPO/'src/backend/x11/compositor/shaders/planet_surface.glsl').read_text())
vert='#version 330 core\nvoid main(){ vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2); gl_Position=vec4(p*2.0-1.0,0,1); }'
def prepare(s,es):
 return '#version 300 es\nprecision highp float;\nprecision highp int;\nprecision highp sampler2D;\nprecision highp sampler3D;\n'+s.lstrip().split('\n',1)[1] if es else s
for es in (False,True):
 kind='gles300' if es else 'gl330'
 assert bind(0x30A0 if es else 0x30A2)
 cfg=P(); count=I()
 assert choose(d,attrs(0x3033,1,0x3040,0x40 if es else 8,0x3024,8,0x3023,8,0x3022,8,0x3021,8,0x3038),C.byref(cfg),1,C.byref(count)) and count.value
 ctx=context(d,cfg,None,attrs(0x3098,3,0x3038) if es else attrs(0x3098,3,0x30FB,3,0x30FD,1,0x3038)); assert ctx
 surf=surface(d,cfg,attrs(0x3057,W,0x3056,H,0x3038)); assert surf
 assert current(d,surf,surf,ctx)
 version=gl('glGetString',C.c_char_p,[U])(0x1F02).decode()
 renderer=gl('glGetString',C.c_char_p,[U])(0x1F01).decode()
 def compile(src,typ):
  sh=gl('glCreateShader',U,[U])(typ); b=prepare(src,es).encode(); ptr=C.c_char_p(b)
  gl('glShaderSource',None,[U,I,C.POINTER(C.c_char_p),C.POINTER(I)])(sh,1,C.byref(ptr),None)
  gl('glCompileShader',None,[U])(sh); ok=I();gl('glGetShaderiv',None,[U,U,C.POINTER(I)])(sh,0x8B81,C.byref(ok))
  log=C.create_string_buffer(8192);gl('glGetShaderInfoLog',None,[U,I,C.POINTER(I),P])(sh,8192,None,log)
  assert ok.value, log.value.decode()
  return sh
 vs=compile(vert,0x8B31); fs=compile(source,0x8B30)
 prog=gl('glCreateProgram',U,[])(); attach=gl('glAttachShader',None,[U,U]);attach(prog,vs);attach(prog,fs)
 gl('glLinkProgram',None,[U])(prog);ok=I();gl('glGetProgramiv',None,[U,U,C.POINTER(I)])(prog,0x8B82,C.byref(ok));assert ok.value,'link failed'
 gl('glUseProgram',None,[U])(prog)
 vao=U();gl('glGenVertexArrays',None,[I,C.POINTER(U)])(1,C.byref(vao));gl('glBindVertexArray',None,[U])(vao)
 gl('glViewport',None,[I,I,I,I])(0,0,W,H)
 loc=gl('glGetUniformLocation',I,[U,C.c_char_p]); unif1i=gl('glUniform1i',None,[I,I]);unif1f=gl('glUniform1f',None,[I,F])
 assert loc(prog,b'view_to_body')>=0 and loc(prog,b'sun_direction')>=0, 'physical camera/lighting uniforms are inactive'
 def upload_view(program,matrix):
  gl('glUniformMatrix3fv',None,[I,I,C.c_ubyte,C.POINTER(F)])(loc(program,b'view_to_body'),1,0,(F*9)(*matrix))
 def fixed_view(program,latitude,longitude,roll=0):
  upload_view(program,celestial_frame(0,latitude=latitude,longitude=longitude,roll=roll)[1])
 gl('glUniform2f',None,[I,F,F])(loc(prog,b'resolution'),W,H)
 gl('glUniform2f',None,[I,F,F])(loc(prog,b'canvas_origin'),0,0)
 unif1f(loc(prog,b'sphere_radius'),layout_plan(W,H,[],False)[0]['radius'])
 # Historical controls use the exact previous 0.18-radian camera latitude.
 fixed_view(prog,math.degrees(0.18),0)
 textures={}
 param=gl('glTexParameteri',None,[U,U,I])
 for body,asset in [('earth',os.environ.get('ASTRONOMY_TEST_TEXTURE',str(REPO/'assets/astronomy/earth-nasa-october.jpg'))),('moon',str(REPO/'assets/astronomy/moon-nasa-lroc.jpg'))]:
  tex=U();gl('glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(tex));gl('glBindTexture',None,[U,U])(0x0DE1,tex)
  for key,val in ((0x2801,0x2601),(0x2800,0x2601),(0x2802,0x2901),(0x2803,0x812F)):param(0x0DE1,key,val)
  texture=Image.open(asset).convert('RGBA');data=C.create_string_buffer(texture.tobytes())
  gl('glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0x0DE1,0,0x1908,texture.width,texture.height,0,0x1908,0x1401,data)
  gl('glGenerateMipmap',None,[U])(0x0DE1);param(0x0DE1,0x2801,0x2703)
  textures[body]=tex.value
 # Match production texture units: surface=0, NASA clouds=1, optional sky=2.
 gl('glActiveTexture',None,[U])(0x84C1)
 cloud_tex=U();gl('glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(cloud_tex));gl('glBindTexture',None,[U,U])(0x0DE1,cloud_tex)
 for key,val in ((0x2801,0x2601),(0x2800,0x2601),(0x2802,0x2901),(0x2803,0x812F)):param(0x0DE1,key,val)
 clouds=Image.open(Path(os.environ.get('ASTRONOMY_TEST_CLOUD_TEXTURE',str(REPO/'assets/astronomy/earth-nasa-clouds.jpg')))).convert('RGBA');cloud_data=C.create_string_buffer(clouds.tobytes())
 gl('glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0x0DE1,0,0x1908,clouds.width,clouds.height,0,0x1908,0x1401,cloud_data)
 gl('glGenerateMipmap',None,[U])(0x0DE1);param(0x0DE1,0x2801,0x2703)
 unif1i(loc(prog,b'cloud_map'),1);unif1i(loc(prog,b'clouds_enabled'),1)
 sky_path=Path(os.environ.get('ASTRONOMY_TEST_SKY_TEXTURE',str(REPO/'assets/astronomy/milkyway-nasa-gaia.jpg')))
 sky_available=sky_path.is_file()
 sky_tex=U()
 gl('glActiveTexture',None,[U])(0x84C2)
 if sky_available:
  sky_texture=Image.open(sky_path).convert('RGBA')
  assert sky_texture.width<=2048 and sky_texture.height<=1024, 'review texture must match production upload cap without a different resizer'
  gl('glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(sky_tex));gl('glBindTexture',None,[U,U])(0x0DE1,sky_tex)
  for key,val in ((0x2801,0x2601),(0x2800,0x2601),(0x2802,0x2901),(0x2803,0x812F)):param(0x0DE1,key,val)
  sky_data=C.create_string_buffer(sky_texture.tobytes())
  gl('glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0x0DE1,0,0x1908,sky_texture.width,sky_texture.height,0,0x1908,0x1401,sky_data)
  gl('glGenerateMipmap',None,[U])(0x0DE1);param(0x0DE1,0x2801,0x2703)
 else:
  gl('glBindTexture',None,[U,U])(0x0DE1,0)
 unif1i(loc(prog,b'sky_map'),2);unif1i(loc(prog,b'sky_enabled'),int(sky_available))
 gl('glActiveTexture',None,[U])(0x84C0)
 gl('glBindTexture',None,[U,U])(0x0DE1,textures['earth'])
 unif1i(loc(prog,b'earth_map'),0)

 # Exercise every production scene before the legacy regression probes. The
 # fast flag runs these GL + GLES checks without repeating the historic suite.
 catalogue_textures={3:textures['earth'],9:textures['moon']}
 for body_id,asset in [(1,'mercury-nasa-messenger.jpg'),(4,'mars-nasa-viking.jpg'),(5,'jupiter-nasa-cassini.jpg')]:
  path=REPO/'assets/astronomy'/asset
  assert path.is_file(),f'missing observed catalogue map: {path}'
  value=Image.open(path).convert('RGBA')
  width=min(value.width,2048)&~1
  if value.size!=(width,width//2):value=value.resize((width,width//2),Image.Resampling.LANCZOS)
  tex=U();gl('glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(tex));gl('glBindTexture',None,[U,U])(0x0DE1,tex)
  for key,val in ((0x2801,0x2703),(0x2800,0x2601),(0x2802,0x2901),(0x2803,0x812F)):param(0x0DE1,key,val)
  data=C.create_string_buffer(value.tobytes())
  gl('glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0x0DE1,0,0x1908,value.width,value.height,0,0x1908,0x1401,data)
  gl('glGenerateMipmap',None,[U])(0x0DE1);catalogue_textures[body_id]=tex.value
 def catalogue_image(body_id,scientific=True,mapped=True,latitude=23.13,longitude=0.0,sky=True):
  matrix=celestial_frame(BASE,latitude=latitude,longitude=longitude)[1]
  light=[sum(matrix[col*3+row]*[0.48,0.25,0.84148675][col] for col in range(3)) for row in range(3)]
  upload_view(prog,matrix)
  gl('glUniform3f',None,[I,F,F,F])(loc(prog,b'sun_direction'),*light)
  unif1i(loc(prog,b'celestial_body'),body_id+1)
  unif1i(loc(prog,b'moon_material'),int(body_id==9))
  unif1i(loc(prog,b'cosmic_scientific'),int(scientific))
  unif1i(loc(prog,b'panorama_enabled'),1)
  unif1f(loc(prog,b'panorama_fov'),60)
  gl('glUniformMatrix3fv',None,[I,I,C.c_ubyte,C.POINTER(F)])(loc(prog,b'panorama_view'),1,0,(F*9)(1,0,0,0,1,0,0,0,1))
  unif1i(loc(prog,b'sky_enabled'),int(sky_available and sky))
  unif1i(loc(prog,b'surface_mapped'),int(mapped and body_id in catalogue_textures))
  unif1f(loc(prog,b'surface_detail'),1)
  unif1f(loc(prog,b'sphere_radius'),H*0.42*(0.48 if body_id==6 else 1.0))
  gl('glActiveTexture',None,[U])(0x84C0)
  gl('glBindTexture',None,[U,U])(0x0DE1,catalogue_textures.get(body_id,0) if mapped else 0)
  gl('glDrawArrays',None,[U,I,I])(4,0,3)
  buf=(C.c_ubyte*(W*H*4))();gl('glReadPixels',None,[I,I,I,I,U,U,P])(0,0,W,H,0x1908,0x1401,buf)
  assert gl('glGetError',U,[])()==0,('catalogue GL error',body_id)
  output=Image.frombytes('RGBA',(W,H),bytes(buf)).transpose(Image.Transpose.FLIP_TOP_BOTTOM)
  assert output.getchannel('A').getextrema()==(255,255),('transparent body',body_id)
  return output.convert('RGB')
 catalogue_frames=[];catalogue_images={}
 for body_id,name in enumerate(('sun','mercury','venus','earth','mars','jupiter','saturn','uranus','neptune','moon','galaxy')):
  output=catalogue_image(body_id)
  filename=f'{kind}-encyclopedia-{name}.png';output.save(ROOT/filename)
  if body_id!=10:assert max(output.getpixel((W//2,H//2)))>8,('black planet',name)
  catalogue_images[body_id]=output
  catalogue_frames.append({'scene':name,'body_id':body_id,'file':filename,'observed_surface':body_id in catalogue_textures,'center_rgb':output.getpixel((W//2,H//2))})
 scientific_blank=catalogue_image(10,sky=False)
 assert all(low==high for low,high in scientific_blank.getextrema()),'scientific mode fabricated a star catalogue'
 illustrative_blank=catalogue_image(10,scientific=False,sky=False)
 assert ImageChops.difference(scientific_blank,illustrative_blank).getbbox() is not None,'illustrative sky fallback disappeared'
 neutral_a=catalogue_image(5,mapped=False,longitude=0)
 neutral_b=catalogue_image(5,mapped=False,longitude=91)
 assert max(high for _,high in ImageChops.difference(neutral_a,neutral_b).getextrema())<=1,'scientific fallback invented cloud geography'
 art_a=catalogue_image(5,scientific=False,mapped=False,longitude=0)
 art_b=catalogue_image(5,scientific=False,mapped=False,longitude=91)
 assert max(high for _,high in ImageChops.difference(art_a,art_b).getextrema())>1,'illustrative cloud bands did not rotate'
 ring_radius=H*0.42*0.48
 ring_pixels=0
 saturn=catalogue_images[6]
 sky_image=catalogue_images[10]
 for y in range(H):
  for x in range(W):
   if math.hypot(x+0.5-W/2,H-y-0.5-H/2)>ring_radius*1.30:
    if max(saturn.getpixel((x,y)))>40 and max(abs(a-b) for a,b in zip(saturn.getpixel((x,y)),sky_image.getpixel((x,y))))>20:ring_pixels+=1
 assert ring_pixels>W*H*0.002,('Saturn rings absent',ring_pixels)
 catalogue_metrics={'frames':catalogue_frames,'all_scenes_opaque':'PASS','scientific_missing_sky_no_invented_stars':'PASS','scientific_missing_surface_no_generated_geography':'PASS','illustrative_material_rotation':'PASS','saturn_ring_pixels':ring_pixels}
 if os.environ.get('ASTRONOMY_TEST_SURFACES_ONLY')=='1':
  results[kind]={'version':version,'renderer':renderer,'compile_link':'PASS','celestial_surfaces':catalogue_metrics}
  current(d,None,None,None)
  egl('eglDestroySurface',U,[P,P])(d,surf);egl('eglDestroyContext',U,[P,P])(d,ctx)
  continue
 # Restore the original fixture inputs for existing physical-light/pixel tests.
 unif1i(loc(prog,b'celestial_body'),0);unif1i(loc(prog,b'moon_material'),0)
 unif1i(loc(prog,b'cosmic_scientific'),0);unif1i(loc(prog,b'panorama_enabled'),0)
 unif1i(loc(prog,b'sky_enabled'),int(sky_available));unif1f(loc(prog,b'sphere_radius'),layout_plan(W,H,[],False)[0]['radius'])
 unif1f(loc(prog,b'surface_detail'),0)
 gl('glBindTexture',None,[U,U])(0x0DE1,textures['earth'])
 images=[];metrics=[]
 cases=[(h,z) for z in (0,8) for h in (0,6,12,18)]+[(12+1/60,0),(12+1,0),(6+1/54000,0),(6+1/3600,0)]
 for hour,zone in cases:
  values=astronomy_values(BASE+hour*3600,mode='legacy-clock',offset_seconds=zone*3600)
  gl('glUniform3f',None,[I,F,F,F])(loc(prog,b'sun_direction'),*values[:3]);fixed_view(prog,math.degrees(0.18),math.degrees(values[3]))
  gl('glDrawArrays',None,[U,I,I])(4,0,3)
  buf=(C.c_ubyte*(W*H*4))();gl('glReadPixels',None,[I,I,I,I,U,U,P])(0,0,W,H,0x1908,0x1401,buf)
  assert gl('glGetError',U,[])()==0
  im=Image.frombytes('RGBA',(W,H),bytes(buf)).transpose(Image.Transpose.FLIP_TOP_BOTTOM)
  assert im.getchannel('A').getextrema()==(255,255)
  name=f'{kind}-earth-utc{hour:.8f}-zone{zone:+d}.png';im.save(ROOT/name)
  metrics.append({'hour':hour,'zone':zone,'sun_direction':values[:3],'view_longitude':values[3],'file':name,'sha256':hashlib.sha256(im.tobytes()).hexdigest(),'rgb_extrema':im.convert('RGB').getextrema()})
  images.append(im)
  if hour == 0 and zone == 0:
   # Actual GPU regression against the same shader with hard silhouette only.
   # Replacing the AA pass with its old branch must make this assertion fail.
   assert source.count('if (coverage <= 0.0) {')==1
   assert source.count('if (coverage < 1.0) {')==1
   hard_source=source.replace('if (coverage <= 0.0) {','if (radius2 > 1.0) {').replace('if (coverage < 1.0) {','if (false) {')
   hard_fragment=compile(hard_source,0x8B30)
   hard_program=gl('glCreateProgram',U,[])();attach(hard_program,vs);attach(hard_program,hard_fragment)
   gl('glLinkProgram',None,[U])(hard_program);linked=I();gl('glGetProgramiv',None,[U,U,C.POINTER(I)])(hard_program,0x8B82,C.byref(linked));assert linked.value
   gl('glUseProgram',None,[U])(hard_program)
   gl('glUniform2f',None,[I,F,F])(loc(hard_program,b'canvas_origin'),0,0)
   gl('glUniform2f',None,[I,F,F])(loc(hard_program,b'resolution'),W,H)
   radius=layout_plan(W,H,[],False)[0]['radius']
   unif1f(loc(hard_program,b'sphere_radius'),radius)
   fixed_view(hard_program,math.degrees(0.18),math.degrees(values[3]))
   gl('glUniform3f',None,[I,F,F,F])(loc(hard_program,b'sun_direction'),*values[:3])
   unif1i(loc(hard_program,b'earth_map'),0)
   unif1i(loc(hard_program,b'cloud_map'),1);unif1i(loc(hard_program,b'clouds_enabled'),1)
   unif1i(loc(hard_program,b'sky_map'),2);unif1i(loc(hard_program,b'sky_enabled'),int(sky_available))
   gl('glDrawArrays',None,[U,I,I])(4,0,3)
   hard_pixels=(C.c_ubyte*(W*H*4))();gl('glReadPixels',None,[I,I,I,I,U,U,P])(0,0,W,H,0x1908,0x1401,hard_pixels)
   assert gl('glGetError',U,[])()==0
   hard_image=Image.frombytes('RGBA',(W,H),bytes(hard_pixels)).transpose(Image.Transpose.FLIP_TOP_BOTTOM)
   delta=ImageChops.difference(im.convert('RGB'),hard_image.convert('RGB'))
   red,green,blue=delta.split();mask=ImageChops.lighter(ImageChops.lighter(red,green),blue)
   changed=0
   for index,value in enumerate(mask.getdata()):
    if value:
     changed+=1
     x=index%W+0.5-W/2;y=index//W+0.5-H/2
     assert abs((x*x+y*y)**0.5-radius)<1.5, 'AA changed pixels outside silhouette'
   assert changed>0, 'silhouette AA disappeared'
   hard_image.save(ROOT/f'{kind}-hard-silhouette-control.png')
   gl('glUseProgram',None,[U])(prog)
   gl('glDeleteProgram',None,[U])(hard_program);gl('glDeleteShader',None,[U])(hard_fragment)

 # Legacy timezone-orbit regression controls. New production Earth cases below
 # use a fixed camera and physical solar lighting.
 # Fixed sky must never blink with clock/timezone updates. Corners are outside Earth.
 sky_boxes=[(0,0,W//8,H//8),(W-W//8,0,W,H//8),
            (0,H-H//8,W//8,H),(W-W//8,H-H//8,W,H)]
 sky_references=[images[0].crop(box).convert('RGB') for box in sky_boxes]
 for box,reference in zip(sky_boxes,sky_references):
  assert all(ImageChops.difference(reference,im.crop(box).convert('RGB')).getbbox() is None for im in images), 'sky corner changes with time'
 # A random star field need not put a bright star in one predetermined corner.
 if sky_available:
  # Gaia texture is diffuse catalogue light, not procedural point stars.
  assert max(max(channel[1] for channel in im.getextrema()) for im in sky_references)>10, 'blank sky texture'
  assert max(max(hi-lo for lo,hi in im.getextrema()) for im in sky_references)>5, 'sky texture lacks spatial detail'
 else:
  assert max(max(channel[1] for channel in im.getextrema()) for im in sky_references)>40, 'missing stars across all four sky corners'
 assert len({m['sha256'] for m in metrics})==len(cases)
 sheet=Image.new('RGB',(W*4,(H+28)*2),(16,19,25)); draw=ImageDraw.Draw(sheet)
 for i,im in enumerate(images[:8]):
  x=(i%4)*W;y=(i//4)*(H+28);sheet.paste(im,(x,y+28));draw.text((x+10,y+8),f'2026-10-15 UTC {cases[i][0]:02d}:00 | timezone UTC{cases[i][1]:+d} | legacy orbit | {kind}',fill='white')
 sheet.save(ROOT/f'{kind}-earth-contact-sheet.png')
 diffs=[]
 for baseline,index,seconds in ((2,8,60),(2,9,3600),(1,10,1/15),(1,11,1)):
  delta=ImageChops.difference(images[baseline].convert('RGB'),images[index].convert('RGB'))
  diffs.append({'seconds':seconds,'mean_abs_rgb':sum(ImageStat.Stat(delta).mean)/3,'max_abs_channel':max(x[1] for x in delta.getextrema())})
 results[kind]={'celestial_surfaces':catalogue_metrics,'version':version,'renderer':renderer,'compile_link':'PASS','opaque_all_frames':True,'distinct_frames':len(cases),'motion':diffs,'metrics':metrics,'legacy_controls':'Historical timezone orbit, latitude 0.18 radians; not production default.'}
 # Full root framebuffer test, two 2560x1440 outputs joined at x=2560.
 wide=surface(d,cfg,attrs(0x3057,5120,0x3056,1440,0x3038));assert wide
 assert current(d,wide,wide,ctx)
 gl('glViewport',None,[I,I,I,I])(0,0,5120,1440)
 gl('glUniform2f',None,[I,F,F])(loc(prog,b'resolution'),5120,1440)
 gl('glUniform2f',None,[I,F,F])(loc(prog,b'canvas_origin'),0,0)
 unif1f(loc(prog,b'sphere_radius'),layout_plan(5120,1440,[],False)[0]['radius'])
 values=astronomy_values(PREVIEW_INSTANT)
 fixed_view(prog,DEFAULT_LATITUDE,math.degrees(values[3]))
 gl('glUniform3f',None,[I,F,F,F])(loc(prog,b'sun_direction'),*values[:3])
 gl('glDrawArrays',None,[U,I,I])(4,0,3)
 buf=(C.c_ubyte*(5120*1440*4))();gl('glReadPixels',None,[I,I,I,I,U,U,P])(0,0,5120,1440,0x1908,0x1401,buf)
 assert gl('glGetError',U,[])()==0
 im=Image.frombytes('RGBA',(5120,1440),bytes(buf)).transpose(Image.Transpose.FLIP_TOP_BOTTOM)
 assert im.getchannel('A').getextrema()==(255,255)
 im.save(ROOT/f'{kind}-earth-dualoutput-5120x1440.png')
 # Physical labels can be (-2560,0)+(0,0); root framebuffer positions remain 0..5120.
 results[kind]['dualoutput']={'utc_instant':datetime.datetime.fromtimestamp(PREVIEW_INSTANT,datetime.timezone.utc).isoformat(),'earth_mode':'fixed','location':'Guangzhou','view_latitude_degrees':DEFAULT_LATITUDE,'view_longitude_degrees':DEFAULT_LONGITUDE,'size':[5120,1440],'earth_center':[2560,720],'earth_radius':604.8,'opaque':True,'expected':'One globe spans output boundary; no per-monitor duplication.'}
 # Execute actual production geometry plans with the actual shader in both APIs.
 # This validates pixel mapping; Rust compositor integration/state restoration is
 # separately covered by its production tests and source review.
 enable=gl('glEnable',None,[U]); disable=gl('glDisable',None,[U])
 scissor=gl('glScissor',None,[I,I,I,I]); clear=gl('glClear',None,[U])
 clear_color=gl('glClearColor',None,[F,F,F,F]); read=gl('glReadPixels',None,[I,I,I,I,U,U,P])
 def layout_image(width,height,outputs,repair=None,materials=None,instant=BASE+120,clouds_enabled=True,earth_minutes=1440,moon_minutes=39343,offset_seconds=0,sky_enabled=None,earth_rotating=False,earth_latitude=DEFAULT_LATITUDE,earth_longitude=DEFAULT_LONGITUDE,frozen_sun=None,legacy_earth=False,sky_minutes=0,sky_seconds=None,earth_roll=0,earth_axis=(0,1,0),moon_latitude=math.degrees(0.18),moon_longitude=0,moon_roll=0,moon_axis=(0,1,0),moon_rotating=True,body_frames=None,panorama=None,body_zoom=1.0,detail=None):
  target=surface(d,cfg,attrs(0x3057,width,0x3056,height,0x3038));assert target
  assert current(d,target,target,ctx)
  gl('glViewport',None,[I,I,I,I])(0,0,width,height)
  unif1i(loc(prog,b'clouds_enabled'),int(clouds_enabled))
  unif1i(loc(prog,b'panorama_enabled'),int(panorama is not None))
  if panorama is not None:
   fov,matrix=panorama
   unif1f(loc(prog,b'panorama_fov'),fov)
   gl('glUniformMatrix3fv',None,[I,I,C.c_ubyte,C.POINTER(F)])(loc(prog,b'panorama_view'),1,0,(F*9)(*matrix))
  unif1f(loc(prog,b'surface_detail'),body_zoom if detail is None else detail)
  roll=list(map(float,subprocess.check_output([str(ROOT/'astronomy_values'),'sky',str(instant if sky_seconds is None else sky_seconds),str(sky_minutes)]).split()))
  gl('glUniform2f',None,[I,F,F])(loc(prog,b'sky_rotation'),*roll)
  use_sky=sky_available if sky_enabled is None else sky_enabled and sky_available
  gl('glActiveTexture',None,[U])(0x84C2);gl('glBindTexture',None,[U,U])(0x0DE1,sky_tex.value if use_sky else 0)
  gl('glActiveTexture',None,[U])(0x84C0)
  unif1i(loc(prog,b'sky_map'),2);unif1i(loc(prog,b'sky_enabled'),int(use_sky))
  disable(0x0C11);clear_color(7/255,11/255,19/255,1);clear(0x4000)
  plan=layout_plan(width,height,outputs,repair=repair)
  for draw_plan in plan:
   # Use the production plan's output coordinate, not enumeration order.
   body='earth' if materials is None else materials[tuple(draw_plan['origin'])]
   unif1i(loc(prog,b'moon_material'),int(body=='moon'))
   gl('glBindTexture',None,[U,U])(0x0DE1,textures[body])
   if body_frames is not None and body in body_frames:
    # Interactive frames are generated by the production Rust drag controller.
    lighting,matrix=body_frames[body]
   elif body=='moon':
    lighting,matrix=celestial_frame(instant,body='moon',minutes=moon_minutes,rotating=moon_rotating,latitude=moon_latitude,longitude=moon_longitude,roll=moon_roll,axis=moon_axis)
   elif legacy_earth:
    values=astronomy_values(instant,mode='legacy-studio',minutes=earth_minutes,offset_seconds=offset_seconds)
    lighting=values[:3]
    matrix=celestial_frame(instant,latitude=math.degrees(0.18),longitude=math.degrees(values[3]))[1]
   else:
    lighting,matrix=celestial_frame(instant,minutes=earth_minutes,longitude=earth_longitude,latitude=earth_latitude,roll=earth_roll,axis=earth_axis,rotating=earth_rotating)
   gl('glUniform3f',None,[I,F,F,F])(loc(prog,b'sun_direction'),*(frozen_sun if body=='earth' and frozen_sun is not None else lighting))
   upload_view(prog,matrix)
   enable(0x0C11);scissor(*draw_plan['clip'])
   gl('glUniform2f',None,[I,F,F])(loc(prog,b'canvas_origin'),*draw_plan['origin'])
   gl('glUniform2f',None,[I,F,F])(loc(prog,b'resolution'),*draw_plan['size'])
   unif1f(loc(prog,b'sphere_radius'),draw_plan['radius']*body_zoom)
   gl('glDrawArrays',None,[U,I,I])(4,0,3)
  if repair is None: disable(0x0C11)
  else: enable(0x0C11);scissor(*repair)
  pixels=(C.c_ubyte*(width*height*4))();read(0,0,width,height,0x1908,0x1401,pixels)
  assert gl('glGetError',U,[])()==0
  output=Image.frombytes('RGBA',(width,height),bytes(pixels)).transpose(Image.Transpose.FLIP_TOP_BOTTOM)
  assert output.getchannel('A').getextrema()==(255,255)
  current(d,None,None,None);egl('eglDestroySurface',U,[P,P])(d,target)
  return output,plan
 # High-frequency maps expose minification shimmer. Compare the actual surface
 # shader against a uniform average, and retain an unfiltered negative control.
 # Include the longitude seam and both geographic poles.
 def upload_filter_fixture(image):
  assert current(d,surf,surf,ctx)
  gl('glActiveTexture',None,[U])(0x84C0)
  tex=U();gl('glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(tex))
  gl('glBindTexture',None,[U,U])(0x0DE1,tex)
  for key,val in ((0x2801,0x2703),(0x2800,0x2601),(0x2802,0x2901),(0x2803,0x812F)):param(0x0DE1,key,val)
  data=C.create_string_buffer(image.tobytes())
  gl('glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0x0DE1,0,0x8058,image.width,image.height,0,0x1908,0x1401,data)
  gl('glGenerateMipmap',None,[U])(0x0DE1)
  assert gl('glGetError',U,[])()==0
  return tex.value
 checker=Image.frombytes('RGBA',(1024,512),b''.join((bytes((0,0,0,255,255,255,255,255))*512 if y%2==0 else bytes((255,255,255,255,0,0,0,255))*512) for y in range(512)))
 checker_texture=upload_filter_fixture(checker)
 average_texture=upload_filter_fixture(Image.new('RGBA',(1,1),(128,128,128,255)))
 earth_texture=textures['earth'];filter_metrics=[]
 for label,longitude,latitude,roll,axis,rotating in [
  ('equator',0,0,0,(0,1,0),False),('longitude-seam',180,0,0,(0,1,0),False),
  ('north-pole',0,90,0,(0,1,0),False),('south-pole',0,-90,0,(0,1,0),False),
  ('rolled-seam',180,0,67,(0,1,0),False),
  ('oblique-spin',113,37,28,(.25,.5,.75),True),
  ('reverse-polar-spin',-125,87,-72,(-.25,-.5,-.75),True),
 ]:
  _,matrix=celestial_frame(BASE+120,longitude=longitude,latitude=latitude,roll=roll,axis=axis,rotating=rotating,minutes=20)
  lighting=matrix[6:9]
  def filter_frame(texture):
   textures['earth']=texture
   return layout_image(256,192,[(0,0,256,192)],clouds_enabled=False,sky_enabled=False,earth_latitude=latitude,earth_longitude=longitude,earth_roll=roll,earth_axis=axis,earth_rotating=rotating,earth_minutes=20,frozen_sun=lighting)[0]
  filtered=filter_frame(checker_texture)
  expected=filter_frame(average_texture)
  assert current(d,surf,surf,ctx)
  gl('glBindTexture',None,[U,U])(0x0DE1,checker_texture);param(0x0DE1,0x2801,0x2601)
  unfiltered=filter_frame(checker_texture)
  assert current(d,surf,surf,ctx)
  gl('glBindTexture',None,[U,U])(0x0DE1,checker_texture);param(0x0DE1,0x2801,0x2703)
  box=(80,48,176,144)
  def average_error(image):
   return sum(ImageStat.Stat(ImageChops.difference(image.crop(box).convert('RGB'),expected.crop(box).convert('RGB'))).mean)/3
  filtered_error=average_error(filtered);unfiltered_error=average_error(unfiltered)
  assert filtered_error<0.5, (label,'minified map is not its area average',filtered_error)
  assert unfiltered_error>5 and filtered_error<unfiltered_error/10, (label,'mipmap negative control did not alias',unfiltered_error)
  name=f'{kind}-texture-minification-{label}.png';filtered.save(ROOT/name)
  unfiltered.save(ROOT/f'{kind}-texture-minification-{label}-unfiltered.png')
  filter_metrics.append({'file':name,'view_longitude':longitude,'view_latitude':latitude,'view_roll':roll,'rotation_axis':axis,'rotating':rotating,'filtered_mean_error':filtered_error,'unfiltered_mean_error':unfiltered_error})
 textures['earth']=earth_texture
 assert current(d,surf,surf,ctx)
 gl('glBindTexture',None,[U,U])(0x0DE1,earth_texture)
 gl('glDeleteTextures',None,[I,C.POINTER(U)])(1,C.byref(U(checker_texture)))
 gl('glDeleteTextures',None,[I,C.POINTER(U)])(1,C.byref(U(average_texture)))
 # A smooth longitude pattern must remain sharp where atan wraps. This probe
 # executes the production analytic function on both sides of the seam and at
 # high north/south latitudes; an implicit derivative selects the wrong mip.
 wrap_row=bytes(v for x in range(1024) for v in (round(128+100*math.cos(2*math.pi*(x+.5)/1024)),0,0,255))
 wrap_texture=upload_filter_fixture(Image.frombytes('RGBA',(1024,512),wrap_row*512))
 gradient_source=source.split('void main() {',1)[0]+r"""
void main() {
 float longitude=PI+(gl_FragCoord.x-3.3)*.001;
 float latitude=gl_FragCoord.x<2.?-1.56:gl_FragCoord.x>6.?1.56:0.;
 float sl=sin(longitude),cl=cos(longitude),sp=sin(latitude),cp=cos(latitude);
 vec3 n=vec3(cp*sl,sp,cp*cl);
 vec3 dx=vec3(cp*cl,0.,-cp*sl)*.001;
 vec3 dy=vec3(-sp*sl,cp,-sp*cl)*.001;
 vec2 gx=equirect_gradient(n,dx),gy=equirect_gradient(n,dy);
 bool ok=length(gx-vec2(.001/(2.*PI),0.))<.000001;
 ok=ok&&length(gy-vec2(0.,-.001/PI))<.000001;
 vec2 uv=vec2(.5+atan(n.x,n.z)/(2.*PI),.5-asin(clamp(n.y,-1.,1.))/PI);
 ok=ok&&textureGrad(earth_map,uv,gx,gy).r>.85;
 frag_color=ok?vec4(0,1,0,1):vec4(1,0,0,1);
}
"""
 gradient_fragment=compile(gradient_source,0x8B30)
 gradient_program=gl('glCreateProgram',U,[])();attach(gradient_program,vs);attach(gradient_program,gradient_fragment)
 gl('glLinkProgram',None,[U])(gradient_program)
 linked=I();gl('glGetProgramiv',None,[U,U,C.POINTER(I)])(gradient_program,0x8B82,C.byref(linked));assert linked.value
 gl('glUseProgram',None,[U])(gradient_program);unif1i(loc(gradient_program,b'earth_map'),0)
 gl('glViewport',None,[I,I,I,I])(0,0,8,1);gl('glDrawArrays',None,[U,I,I])(4,0,3)
 gradient_pixels=(C.c_ubyte*32)();read(0,0,8,1,0x1908,0x1401,gradient_pixels)
 assert list(gradient_pixels)==[0,255,0,255]*8, ('seam/pole explicit texture gradients',list(gradient_pixels))
 gl('glDeleteProgram',None,[U])(gradient_program);gl('glDeleteShader',None,[U])(gradient_fragment)
 gl('glUseProgram',None,[U])(prog);gl('glViewport',None,[I,I,I,I])(0,0,W,H)
 gl('glBindTexture',None,[U,U])(0x0DE1,earth_texture)
 gl('glDeleteTextures',None,[I,C.POINTER(U)])(1,C.byref(U(wrap_texture)))
 results[kind]['texture_filtering']={'frames':filter_metrics,'area_average':'PASS','longitude_seam_poles':'PASS','unfiltered_negative_controls':'PASS','gradient_wrap_probes':8}
 layout_cases={
  'per-output-dual-5120x1440':(5120,1440,[(0,0,2560,1440),(2560,0,2560,1440)]),
  'per-output-mixed-3000x1920':(3000,1920,[(0,0,1920,1080),(1920,0,1080,1920)]),
  'per-output-negative-2560x1440':(2560,1440,[(-1280,0,2560,1440),(1280,0,1280,1440)]),
  'per-output-gap-3000x1800':(3000,1800,[(0,0,1800,1080),(1800,720,1200,1080)]),
 }
 layout_metrics=[]
 for name,(width,height,outputs) in layout_cases.items():
  output,plan=layout_image(width,height,outputs)
  output.save(ROOT/f'{kind}-{name}.png')
  layout_metrics.append({'file':f'{kind}-{name}.png','size':[width,height],'outputs':outputs,'plan':plan})
  if name=='per-output-dual-5120x1440':
   assert ImageChops.difference(output.crop((0,0,2560,1440)).convert('RGB'),output.crop((2560,0,5120,1440)).convert('RGB')).getbbox() is None
  if name=='per-output-mixed-3000x1920':
   assert all(v['radius']<=min(v['size'])*0.42+0.001 for v in plan)
   assert output.getpixel((100,1800))==(7,11,19,255), 'output gap must preserve static background'
  if name=='per-output-negative-2560x1440':
   full,_=layout_image(2560,1440,[(0,0,2560,1440)])
   assert ImageChops.difference(output.crop((0,0,1280,1440)).convert('RGB'),full.crop((1280,0,2560,1440)).convert('RGB')).getbbox() is None, 'negative canvas recentered when clipped'
 # Repairs must preserve every untouched pixel, including between outputs.
 small_outputs=[(0,0,320,240),(320,0,320,240)]
 full,_=layout_image(640,240,small_outputs)
 repair=(300,70,40,50)
 partial,_=layout_image(640,240,small_outputs,repair)
 expected=Image.new('RGBA',(640,240),(7,11,19,255));box=(300,120,340,170)
 expected.paste(full.crop(box),box)
 assert ImageChops.difference(expected.convert('RGB'),partial.convert('RGB')).getbbox() is None
 duplicate,_=layout_image(640,240,[*small_outputs,small_outputs[0]])
 assert ImageChops.difference(full.convert('RGB'),duplicate.convert('RGB')).getbbox() is None
 overlap=[(0,0,400,240),(240,0,400,240)]
 first,_=layout_image(640,240,overlap);second,_=layout_image(640,240,list(reversed(overlap)))
 assert ImageChops.difference(first.convert('RGB'),second.convert('RGB')).getbbox() is None
 invalid,plan=layout_image(320,240,[(400,0,100,100)])
 assert not plan and invalid.getpixel((160,120))==(7,11,19,255)
 # Explicit rotating Earth (physical sunlight) plus UTC-phase Moon.
 dual_metrics=[]
 assignments={(0,0):'earth',(2560,0):'moon'}
 dual_outputs=[(0,0,2560,1440),(2560,0,2560,1440)]
 for seconds in (0,1/15,1,10,60):
  rendered,plan=layout_image(5120,1440,dual_outputs,materials=assignments,instant=BASE+120+seconds,earth_rotating=True,earth_minutes=20,moon_minutes=30)
  filename=f'{kind}-earth-moon-{seconds:g}s.png';rendered.save(ROOT/filename)
  dual_metrics.append({'file':filename,'seconds':seconds,'plan':plan})
  if seconds==0:
   dual_reference=rendered
   assert ImageChops.difference(rendered.crop((0,0,2560,1440)).convert('RGB'),rendered.crop((2560,0,5120,1440)).convert('RGB')).getbbox() is not None
  else:
   motion={}
   for body,x in [('earth',0),('moon',2560)]:
    delta=ImageChops.difference(dual_reference.crop((x,0,x+2560,1440)).convert('RGB'),rendered.crop((x,0,x+2560,1440)).convert('RGB'))
    assert delta.getbbox() is not None, f'{body} failed to rotate'
    motion[body]={'mean_abs_rgb':sum(ImageStat.Stat(delta).mean)/3,'max_abs_channel':max(v[1] for v in delta.getextrema())}
    sky_box=(x,0,x+300,200)
    assert ImageChops.difference(dual_reference.crop(sky_box).convert('RGB'),rendered.crop(sky_box).convert('RGB')).getbbox() is None, f'{body} sky moved'
   dual_metrics[-1]['motion']=motion
 # New production default: the camera is geographically fixed over Guangzhou.
 # Every Sun vector comes from the shared production UTC astronomy helper.
 fixed_frames=[];fixed_images=[];frozen_images=[]
 local_zone=datetime.timezone(datetime.timedelta(hours=8))
 fixed_instants=[datetime.datetime(2026,10,16,h,tzinfo=local_zone) for h in (6,12,18,0)]
 fixed_reference_sun=astronomy_values(fixed_instants[1].timestamp())[:3]
 def center_light(values,latitude):
  lat=math.radians(latitude);longitude=values[3]
  normal=(math.sin(longitude)*math.cos(lat),math.sin(lat),math.cos(longitude)*math.cos(lat))
  return sum(a*b for a,b in zip(normal,values[:3]))/math.sqrt(sum(x*x for x in values[:3]))
 def frame_metric(filename,local_time,values,latitude=DEFAULT_LATITUDE,longitude=DEFAULT_LONGITUDE,mode='fixed'):
  return {'file':filename,'local_datetime':local_time.isoformat(),'utc_datetime':local_time.astimezone(datetime.timezone.utc).isoformat(),'unix_seconds':local_time.timestamp(),'earth_mode':mode,'view_latitude_degrees':latitude,'configured_longitude_degrees':longitude,'view_longitude_radians':values[3],'sun_direction':values[:3],'center_sun_dot':center_light(values,latitude)}
 def contact_sheet(frames,filename,columns=4):
  width,height=frames[0][0].size;label_height=52
  sheet=Image.new('RGB',(width*columns,(height+label_height)*math.ceil(len(frames)/columns)),(16,19,25))
  draw=ImageDraw.Draw(sheet)
  for index,(im,lines) in enumerate(frames):
   x=(index%columns)*width;y=(index//columns)*(height+label_height)
   sheet.paste(im,(x,y+label_height))
   for row,line in enumerate(lines):draw.text((x+10,y+7+row*16),line,fill='white')
  sheet.save(ROOT/filename)
 for local_time in fixed_instants:
  instant=local_time.timestamp();values=astronomy_values(instant)
  assert abs(values[3]-math.radians(DEFAULT_LONGITUDE))<1e-6, 'default Earth camera left Guangzhou'
  for period in (1,20,1440,10080):
   assert astronomy_values(instant,minutes=period)[3]==values[3], 'fixed camera depends on rotation period'
  rendered,_=layout_image(W,H,[(0,0,W,H)],instant=instant)
  frozen,_=layout_image(W,H,[(0,0,W,H)],instant=instant,frozen_sun=fixed_reference_sun)
  filename=f"{kind}-earth-fixed-guangzhou-{local_time.strftime('%Y-%m-%dT%H%M%z')}.png"
  rendered.save(ROOT/filename)
  metric=frame_metric(filename,local_time,values)
  radius=layout_plan(W,H,[(0,0,W,H)])[0]['radius']
  central_box=(int(W/2-radius/2),int(H/2-radius/2),int(W/2+radius/2),int(H/2+radius/2))
  metric['central_mean_rgb']=sum(ImageStat.Stat(rendered.crop(central_box).convert('RGB')).mean)/3
  for side,low,high in (('west',-.7,-.15),('east',.15,.7)):
   side_box=(int(W/2+low*radius),int(H/2-.4*radius),int(W/2+high*radius),int(H/2+.4*radius))
   metric[f'{side}_mean_rgb']=sum(ImageStat.Stat(rendered.crop(side_box).convert('RGB')).mean)/3
  fixed_frames.append(metric);fixed_images.append(rendered);frozen_images.append(frozen)
 assert all(ImageChops.difference(frozen_images[0].convert('RGB'),im.convert('RGB')).getbbox() is None for im in frozen_images[1:]), 'fixed Earth geography moved when only UTC changed'
 frozen_images[0].save(ROOT/f'{kind}-earth-fixed-guangzhou-frozen-sun-control.png')
 assert len({hashlib.sha256(im.tobytes()).hexdigest() for im in fixed_images})==4, 'physical day/night images are not distinct'
 by_hour={local.hour:metric for local,metric in zip(fixed_instants,fixed_frames)}
 assert by_hour[12]['center_sun_dot']>0.75 and by_hour[0]['center_sun_dot']<-0.8, 'local noon/midnight sunlight is reversed'
 assert abs(by_hour[6]['center_sun_dot'])<0.25 and abs(by_hour[18]['center_sun_dot'])<0.25, 'dawn/dusk is not near the terminator'
 assert by_hour[12]['central_mean_rgb']>by_hour[0]['central_mean_rgb']*3, 'default midnight is not substantially darker than noon'
 assert by_hour[6]['east_mean_rgb']>by_hour[6]['west_mean_rgb']*3, 'dawn lights the wrong side of the globe'
 assert by_hour[18]['west_mean_rgb']>by_hour[18]['east_mean_rgb']*3, 'dusk lights the wrong side of the globe'
 for sky_box in sky_boxes:
  reference=fixed_images[0].crop(sky_box).convert('RGB')
  assert all(ImageChops.difference(reference,im.crop(sky_box).convert('RGB')).getbbox() is None for im in fixed_images[1:]), 'fixed Earth sky moved with UTC'
 # Timezone is formatting only; period is ignored in fixed mode. Verify the GPU
 # image, not just the arithmetic, remains unchanged for those configuration changes.
 same_time=fixed_instants[1].timestamp()
 config_control,_=layout_image(W,H,[(0,0,W,H)],instant=same_time,earth_minutes=20,offset_seconds=-43200)
 assert ImageChops.difference(fixed_images[1].convert('RGB'),config_control.convert('RGB')).getbbox() is None, 'fixed Earth responds to display timezone or rotation period'
 contact_sheet([(im,[f"{local.strftime('%Y-%m-%d %H:%M')} UTC+08:00 | Guangzhou 23.13N 113.28E | {kind}",f"UTC {local.astimezone(datetime.timezone.utc).strftime('%Y-%m-%d %H:%M')} | actual UTC sunlight"]) for im,local in zip(fixed_images,fixed_instants)],f'{kind}-earth-fixed-guangzhou-2026-10-16-utc-plus8-contact-sheet.png')
 # Seasonal north/south views and polar day/night, including 18:00 dusk.
 seasonal_frames=[];seasonal_images=[];seasonal_lookup={}
 seasonal_w,seasonal_h=min(W,640),min(H,360)
 for month,day in ((6,21),(12,21)):
  for latitude,hours in ((25,(12,18)),(-25,(12,18)),(80,(12,18,0)),(-80,(12,18,0)),(90,(12,)),(-90,(12,))):
   for hour in hours:
    local_time=datetime.datetime(2026,month,day,hour,tzinfo=local_zone)
    values=astronomy_values(local_time.timestamp(),longitude=120.0)
    rendered,_=layout_image(seasonal_w,seasonal_h,[(0,0,seasonal_w,seasonal_h)],instant=local_time.timestamp(),earth_latitude=latitude,earth_longitude=120.0)
    filename=f"{kind}-earth-seasonal-{local_time.strftime('%Y-%m-%dT%H%M%z')}-lat{latitude:+d}.png"
    rendered.save(ROOT/filename)
    metric=frame_metric(filename,local_time,values,latitude,longitude=120.0)
    center_box=(int(seasonal_w*.48),int(seasonal_h*.48),int(seasonal_w*.52),int(seasonal_h*.52))
    metric['central_mean_rgb']=sum(ImageStat.Stat(rendered.crop(center_box).convert('RGB')).mean)/3
    seasonal_frames.append(metric);seasonal_lookup[(month,latitude,hour)]=metric
    hemisphere=f"{abs(latitude)}{'N' if latitude>=0 else 'S'}"
    seasonal_images.append((rendered,[f"{local_time.strftime('%Y-%m-%d %H:%M')} UTC+08:00 | {hemisphere} 120E",f"UTC {local_time.astimezone(datetime.timezone.utc).strftime('%Y-%m-%d %H:%M')} | center Sun dot {metric['center_sun_dot']:+.3f}"]))
    assert abs(values[3]-math.radians(120))<1e-6, 'season changed geographic camera'
    if abs(latitude)>=80:
     summer=(month==6)==(latitude>0)
     assert metric['center_sun_dot']>0.20 if summer else metric['center_sun_dot']<-0.20, 'polar day/night or hemisphere sign incorrect'
 for latitude in (25,-25):
  summer,winter=(6,12) if latitude>0 else (12,6)
  assert seasonal_lookup[(summer,latitude,12)]['center_sun_dot']>seasonal_lookup[(winter,latitude,12)]['center_sun_dot']+0.2, 'summer/winter hemisphere illumination reversed'
  assert seasonal_lookup[(summer,latitude,18)]['center_sun_dot']>0.10 and seasonal_lookup[(winter,latitude,18)]['center_sun_dot']<-0.10, 'seasonal dusk sign incorrect'
 for latitude,hours in ((80,(12,18,0)),(-80,(12,18,0)),(90,(12,)),(-90,(12,))):
  summer,winter=(6,12) if latitude>0 else (12,6)
  for hour in hours:
   assert seasonal_lookup[(summer,latitude,hour)]['central_mean_rgb']>seasonal_lookup[(winter,latitude,hour)]['central_mean_rgb']*3, 'GPU polar day/night shading is reversed'
 contact_sheet(seasonal_images,f'{kind}-earth-seasonal-north-south-polar-contact-sheet.png')
 # Explicit rotating mode moves geography with its configured period while the
 # Sun continues to use UTC. A full rotation repeats camera orientation only.
 rotation_frames=[];rotation_images=[];rotation_frozen=[]
 for elapsed in (0,300,1200):
  instant=same_time+elapsed;values=astronomy_values(instant,minutes=20,rotating=True)
  assert values[:3]==astronomy_values(instant)[:3], 'rotating Earth uses studio light instead of UTC Sun'
  rendered,_=layout_image(W,H,[(0,0,W,H)],instant=instant,earth_rotating=True,earth_minutes=20)
  frozen,_=layout_image(W,H,[(0,0,W,H)],instant=instant,earth_rotating=True,earth_minutes=20,frozen_sun=fixed_reference_sun)
  local_time=datetime.datetime.fromtimestamp(instant,local_zone)
  filename=f'{kind}-earth-explicit-rotating-{elapsed}s.png';rendered.save(ROOT/filename)
  rotation_frames.append(frame_metric(filename,local_time,values,mode='rotating'));rotation_images.append(rendered);rotation_frozen.append(frozen)
 assert abs(math.remainder(rotation_frames[2]['view_longitude_radians']-rotation_frames[0]['view_longitude_radians'],math.tau))<1e-6, 'rotating camera failed to complete configured period'
 assert abs(abs(math.remainder(rotation_frames[1]['view_longitude_radians']-rotation_frames[0]['view_longitude_radians'],math.tau))-math.pi/2)<1e-6, 'rotating quarter-period geometry is incorrect'
 assert ImageChops.difference(rotation_frozen[0].convert('RGB'),rotation_frozen[2].convert('RGB')).getbbox() is None, 'rotating geography failed to repeat'
 assert ImageChops.difference(rotation_frozen[0].convert('RGB'),rotation_frozen[1].convert('RGB')).getbbox() is not None, 'explicit rotating camera stayed fixed'
 assert ImageChops.difference(rotation_images[0].convert('RGB'),rotation_images[2].convert('RGB')).getbbox() is not None, 'physical sunlight incorrectly repeats with visual rotation period'
 results[kind]['fixed_earth']={'frames':fixed_frames,'fixed_camera_frozen_sun_pixel_identity':'PASS','period_timezone_independent':'PASS','noon_midnight_dawn_dusk':'PASS','dawn_east_dusk_west_pixels':'PASS','stationary_sky':'PASS','local_date':'2026-10-16','label_timezone':'UTC+08:00','location':'Guangzhou','default_latitude_degrees':DEFAULT_LATITUDE,'default_longitude_degrees':DEFAULT_LONGITUDE}
 results[kind]['seasonal_earth']={'frames':seasonal_frames,'north_south_seasons':'PASS','polar_day_night':'PASS','seasonal_dusk':'PASS'}
 results[kind]['rotating_earth']={'frames':rotation_frames,'geographic_period':'PASS','physical_utc_sunlight':'PASS'}
 # Historical studio controls retain old period/timezone behavior for comparison.
 # Production fixed Earth camera and physical day/night behavior are tested below.
 defaults={}
 for elapsed, zone in [(0,28800),(21600,28800),(86400,28800),(0,0)]:
  rendered,_=layout_image(640,240,[(0,0,320,240),(320,0,320,240)],materials={(0,0):'earth',(320,0):'moon'},instant=BASE+elapsed,earth_minutes=1440,moon_minutes=39343,offset_seconds=zone,legacy_earth=True)
  name=f'{kind}-default-period-{elapsed}s-zone{zone}.png';rendered.save(ROOT/name)
  defaults[(elapsed,zone)]=rendered
  dual_metrics.append({'file':name,'seconds':elapsed,'offset_seconds':zone,'earth_minutes':1440,'moon_minutes':39343,'earth_mode':'legacy-studio-control'})
 def crop_body(im,body): return im.crop((0,0,320,240) if body=='earth' else (320,0,640,240)).convert('RGB')
 initial=defaults[(0,28800)]
 assert ImageChops.difference(crop_body(initial,'earth'),crop_body(defaults[(86400,28800)],'earth')).getbbox() is None
 assert ImageChops.difference(crop_body(initial,'earth'),crop_body(defaults[(21600,28800)],'earth')).getbbox() is not None
 assert ImageChops.difference(crop_body(initial,'moon'),crop_body(defaults[(86400,28800)],'moon')).getbbox() is not None
 assert ImageChops.difference(crop_body(initial,'moon'),crop_body(defaults[(0,0)],'moon')).getbbox() is None
 assert ImageChops.difference(crop_body(initial,'earth'),crop_body(defaults[(0,0)],'earth')).getbbox() is not None
 # Moon must equal the exact decorative sky outside its silhouette (no halo).
 sky_source=source.split('void main() {',1)[0]+'void main() { frag_color=vec4(galaxy_background(gl_FragCoord.xy-canvas_origin),1.0); }'
 assert current(d,surf,surf,ctx)
 sky_fragment=compile(sky_source,0x8B30);sky_program=gl('glCreateProgram',U,[])();attach(sky_program,vs);attach(sky_program,sky_fragment)
 gl('glLinkProgram',None,[U])(sky_program);linked=I();gl('glGetProgramiv',None,[U,U,C.POINTER(I)])(sky_program,0x8B82,C.byref(linked));assert linked.value
 production_program=prog;prog=sky_program
 # layout_image releases the EGL context after readback; make current before uniforms.
 assert current(d,surf,surf,ctx);gl('glUseProgram',None,[U])(prog)
 sky,_=layout_image(5120,1440,dual_outputs)
 sky.save(ROOT/f'{kind}-galaxy-only.png')
 prog=production_program
 assert current(d,surf,surf,ctx);gl('glUseProgram',None,[U])(prog)
 moon=dual_reference.crop((2560,0,5120,1440)).convert('RGB');sky=sky.crop((2560,0,5120,1440)).convert('RGB')
 delta=ImageChops.difference(moon,sky)
 # Two complete bands immediately outside the lunar radius, including bright stars.
 radius=layout_plan(5120,1440,dual_outputs)[1]['radius']
 halo_checked=0;surface_checked=0;max_blue_excess=0
 for y in range(1440):
  for x in range(2560):
   distance=math.hypot(x+0.5-1280,y+0.5-720)
   if radius+2 < distance < radius+12:
    assert delta.getpixel((x,y))==(0,0,0), 'Moon has an atmospheric halo'
    halo_checked+=1
   if distance < radius-2:
    rgb=moon.getpixel((x,y));max_blue_excess=max(max_blue_excess,rgb[2]-rgb[0])
    surface_checked+=1
 assert max_blue_excess <= 35, 'Moon blue tint exceeds NASA texture range'
 for name,width,height,outputs,materials in [
  ('mixed-portrait',3000,1920,[(0,0,1920,1080),(1920,0,1080,1920)],{(0,840):'earth',(1920,0):'moon'}),
  ('hotplug-moon-only',1080,1920,[(0,0,1080,1920)],{(0,0):'moon'}),
 ]:
  rendered,plan=layout_image(width,height,outputs,materials=materials)
  filename=f'{kind}-earth-moon-{name}.png';rendered.save(ROOT/filename)
  dual_metrics.append({'file':filename,'plan':plan})
 # A damage rectangle crosses the Earth/Moon output boundary.
 materials={(0,0):'earth',(320,0):'moon'}
 full,_=layout_image(640,240,small_outputs,materials=materials)
 partial,_=layout_image(640,240,small_outputs,repair,materials=materials)
 expected=Image.new('RGBA',(640,240),(7,11,19,255));expected.paste(full.crop(box),box)
 assert ImageChops.difference(expected.convert('RGB'),partial.convert('RGB')).getbbox() is None, 'mixed-material damage differs from full frame'
 cloudless,_=layout_image(640,240,small_outputs,materials=materials,clouds_enabled=False)
 assert ImageChops.difference(full.crop((320,0,640,240)).convert('RGB'),cloudless.crop((320,0,640,240)).convert('RGB')).getbbox() is None, 'Earth clouds affect Moon'
 assert ImageChops.difference(full.crop((0,0,320,240)).convert('RGB'),cloudless.crop((0,0,320,240)).convert('RGB')).getbbox() is not None, 'Cloud texture has no visible Earth effect'
 assert ImageChops.difference(full.crop((0,0,40,30)).convert('RGB'),cloudless.crop((0,0,40,30)).convert('RGB')).getbbox() is None, 'Cloud texture affects galaxy'
 # Missing optional sky texture must safely use the stationary fallback.
 fallback,_=layout_image(640,240,small_outputs,materials=materials,sky_enabled=False)
 filename=f'{kind}-missing-sky-fallback.png';fallback.save(ROOT/filename);dual_metrics.append({'file':filename,'sky_enabled':False})
 for center in (160,480):
  body_box=(center-50,70,center+50,170)
  difference=ImageChops.difference(full.crop(body_box).convert('RGB'),fallback.crop(body_box).convert('RGB'))
  assert max(v[1] for v in difference.getextrema())<=1, 'sky toggle changed body interior'
 if sky_available:
  assert ImageChops.difference(full.crop((0,0,40,30)).convert('RGB'),fallback.crop((0,0,40,30)).convert('RGB')).getbbox() is not None, 'sky texture has no visible effect'
 fallback_later,_=layout_image(640,240,small_outputs,materials=materials,sky_enabled=False,instant=BASE+180)
 assert ImageChops.difference(fallback.crop((0,0,40,30)).convert('RGB'),fallback_later.crop((0,0,40,30)).convert('RGB')).getbbox() is None, 'fallback sky moved'
 fallback_partial,_=layout_image(640,240,small_outputs,repair,materials=materials,sky_enabled=False)
 expected=Image.new('RGBA',(640,240),(7,11,19,255));expected.paste(fallback.crop(box),box)
 assert ImageChops.difference(expected.convert('RGB'),fallback_partial.convert('RGB')).getbbox() is None, 'fallback damage differs'
 reordered,_=layout_image(640,240,list(reversed(small_outputs)),materials=materials)
 assert ImageChops.difference(full.convert('RGB'),reordered.convert('RGB')).getbbox() is None, 'connector enumeration changed materials'
 # Actual dated phase renders use production orbital/light and sphere transforms.
 lunar_frames=[]
 for label,stamp in [('new','2026-10-10T15:50:00+00:00'),('waxing-crescent','2026-10-16T11:00:00+00:00'),('first-quarter','2026-10-18T16:12:00+00:00'),('full','2026-10-26T04:12:00+00:00'),('last-quarter','2026-11-01T20:28:00+00:00')]:
  instant=datetime.datetime.fromisoformat(stamp).timestamp()
  rendered,plan=layout_image(640,480,[(0,0,640,480)],materials={(0,0):'moon'},instant=instant)
  name=f'{kind}-lunar-{label}.png';rendered.save(ROOT/name)
  # Interior hemispheres avoid silhouette/star contamination.
  left=sum(ImageStat.Stat(rendered.crop((190,190,310,290)).convert('RGB')).mean)/3
  right=sum(ImageStat.Stat(rendered.crop((330,190,450,290)).convert('RGB')).mean)/3
  lunar_frames.append({'file':name,'utc':stamp,'left_brightness':left,'right_brightness':right})
  if label in ('waxing-crescent','first-quarter'): assert right > left*2, (label,left,right)
  if label=='last-quarter': assert left > right*2, (label,left,right)
 assert sum(lunar_frames[0][k] for k in ('left_brightness','right_brightness')) < sum(lunar_frames[3][k] for k in ('left_brightness','right_brightness'))*0.2, 'new moon too bright'
 results[kind]['lunar_phases']={'frames':lunar_frames,'waxing_right_waning_left':'PASS','new_full_contrast':'PASS'}
 # Full 3D camera regression: production Rust supplies every matrix and light.
 # These frames cover all signed cardinal axes, oblique axes, both poles and
 # screen roll. A uniform lunar albedo isolates the actual phase contour from
 # the deliberately moving texture, rather than comparing different maria.
 orientation_cases=[
  ('equator',0,0,0,(0,1,0),False),
  ('roll-90',23.13,113.28,90,(0,1,0),False),
  ('roll-180',23.13,113.28,180,(0,1,0),False),
  ('north-pole',90,-90,35,(0,1,0),False),
  ('south-pole',-90,170,-37,(0,1,0),False),
  ('spin-x',23.13,113.28,17,(1,0,0),True),
  ('spin-minus-x',23.13,113.28,17,(-1,0,0),True),
  ('spin-y',23.13,113.28,17,(0,1,0),True),
  ('spin-minus-y',23.13,113.28,17,(0,-1,0),True),
  ('spin-z',23.13,113.28,17,(0,0,1),True),
  ('spin-minus-z',23.13,113.28,17,(0,0,-1),True),
  ('oblique',63,-157,128,(.25,.5,.75),True),
  ('oblique-reverse',63,-157,128,(-.25,-.5,-.75),True),
 ]
 orientation_instant=datetime.datetime.fromisoformat('2026-10-16T11:02:17+00:00').timestamp()
 orientation_frames=[];orientation_images=[];orientation_suns=[]
 for label,latitude,longitude,roll,axis,rotating in orientation_cases:
  options={'earth_latitude':latitude,'earth_longitude':longitude,'earth_roll':roll,'earth_axis':axis,'earth_rotating':rotating,'earth_minutes':20,
           'moon_latitude':latitude,'moon_longitude':longitude,'moon_roll':roll,'moon_axis':axis,'moon_rotating':rotating,'moon_minutes':20}
  output,_=layout_image(640,240,small_outputs,materials=materials,instant=orientation_instant,**options)
  filename=f'{kind}-orientation-{label}.png';output.save(ROOT/filename)
  sun,matrix=celestial_frame(orientation_instant,minutes=20,latitude=latitude,longitude=longitude,roll=roll,axis=axis,rotating=rotating)
  orientation_suns.append(sun)
  orientation_frames.append({'file':filename,'case':label,'latitude':latitude,'longitude':longitude,'roll':roll,'axis':axis,'rotating':rotating,'matrix_column_major':matrix,'earth_sun_body':sun})
  orientation_images.append((output,[f'{label} | Earth + Moon | {kind}',f'latitude {latitude:g}, longitude {longitude:g}, roll {roll:g} | axis {axis}']))
 assert all(sun==orientation_suns[0] for sun in orientation_suns), 'Earth sunlight depends on a presentation axis or roll'
 assert len({hashlib.sha256(im.tobytes()).hexdigest() for im,_ in orientation_images})==len(orientation_cases), 'a signed axis, pole or roll case did not change the visible geography'
 contact_sheet(orientation_images,f'{kind}-all-direction-contact-sheet.png',columns=3)
 # Pointer gestures use the production arcball and preserve the pose seen at
 # press time. Include both spherical and smooth outside-limb projections,
 # rolled/polar starting views, and an already rotating globe held for hours.
 drag_cases=[
  ('click-fixed',0,dict(start=(.2,-.1),end=(.2,-.1))),
  ('click-rotating',37,dict(rotating=True,minutes=20,roll=37,axis=(.25,.5,.75))),
  ('right',0,dict(latitude=0,longitude=0,end=(.65,0))),
  ('left',0,dict(latitude=0,longitude=0,end=(-.65,0))),
  ('up',0,dict(latitude=0,longitude=0,end=(0,.65))),
  ('down',0,dict(latitude=0,longitude=0,end=(0,-.65))),
  ('diagonal',0,dict(latitude=37,longitude=-127,roll=83,end=(.4,.4))),
  ('off-centre',0,dict(latitude=-31,longitude=172,roll=-67,start=(.5,.1),end=(-.25,.6))),
  ('around-limb',0,dict(latitude=19,longitude=-17,roll=128,start=(.8,.4),end=(-.7,.8))),
  ('north-pole',0,dict(latitude=89.9,longitude=113,roll=61,start=(-.1,.2),end=(.1,1.2))),
  ('south-pole-long-hold',21600,dict(latitude=-89.9,longitude=-71,roll=-143,rotating=True,minutes=20,axis=(-.5,.25,.75),end=(-.2,-1.3))),
  ('outside-globe',37,dict(rotating=True,minutes=13,axis=(.25,-.75,.5),start=(.2,.1),end=(2.3,-1.8))),
 ]
 drag_frames=[];drag_images=[];drag_probe_cases=[]
 for label,held_seconds,options in drag_cases:
  pressed=orientation_instant-held_seconds
  earth_frame=celestial_drag_frame(orientation_instant,pressed=pressed,**options)
  moon_frame=celestial_drag_frame(orientation_instant,body='moon',pressed=pressed,**options)
  assert earth_frame[0]==orientation_suns[0], (label,'mouse gesture changed geographic Sun direction')
  # A press without motion must preserve any configured/animated initial pose.
  if options.get('start',(0,0))==options.get('end',(0,0)):
   initial=celestial_frame(pressed,**{key:value for key,value in options.items() if key not in ('start','end')})[1]
   assert max(abs(a-b) for a,b in zip(initial,earth_frame[1]))<.000001, (label,'click changed orientation before any motion')
  output,_=layout_image(640,240,small_outputs,materials=materials,instant=orientation_instant,body_frames={'earth':earth_frame,'moon':moon_frame})
  filename=f'{kind}-mouse-drag-{label}.png';output.save(ROOT/filename)
  drag_frames.append({'file':filename,'case':label,'held_seconds':held_seconds,'pointer_start':options.get('start',(0,0)),'pointer_end':options.get('end',(0,0)),'matrix_column_major':earth_frame[1],'earth_sun_body':earth_frame[0]})
  drag_images.append((output,[f'mouse drag: {label} | Earth + Moon | {kind}',f"pointer {options.get('start',(0,0))} -> {options.get('end',(0,0))} | held {held_seconds}s"]))
  drag_probe_cases.append(('drag',label,earth_frame,moon_frame))
 contact_sheet(drag_images,f'{kind}-mouse-drag-contact-sheet.png',columns=3)
 moon_texture=textures['moon']
 uniform_moon=upload_filter_fixture(Image.new('RGBA',(1,1),(160,160,160,255)))
 textures['moon']=uniform_moon
 phase_contour_metrics=[];drag_phase_contour_metrics=[]
 for phase_stamp in ('2026-10-18T16:12:00+00:00','2026-10-26T04:12:00+00:00','2026-11-01T20:28:00+00:00'):
  instant=datetime.datetime.fromisoformat(phase_stamp).timestamp()
  phase_reference=None;phase_max_error=0
  for label,latitude,longitude,roll,axis,rotating in orientation_cases:
   output,_=layout_image(320,240,[(0,0,320,240)],materials={(0,0):'moon'},instant=instant,moon_minutes=20,moon_latitude=latitude,moon_longitude=longitude,moon_roll=roll,moon_axis=axis,moon_rotating=rotating)
   if phase_reference is None: phase_reference=output
   delta=ImageChops.difference(phase_reference.convert('RGB'),output.convert('RGB'))
   maximum=max(v[1] for v in delta.getextrema());phase_max_error=max(phase_max_error,maximum)
   assert maximum<=1, (phase_stamp,label,'decorative orientation changed the real lunar phase contour',maximum)
  phase_contour_metrics.append({'utc':phase_stamp,'orientations':len(orientation_cases),'max_abs_channel':phase_max_error})
  drag_phase_max_error=0
  for label,held_seconds,options in drag_cases:
   frame=celestial_drag_frame(instant,body='moon',pressed=instant-held_seconds,**options)
   output,_=layout_image(320,240,[(0,0,320,240)],materials={(0,0):'moon'},instant=instant,body_frames={'moon':frame})
   delta=ImageChops.difference(phase_reference.convert('RGB'),output.convert('RGB'))
   maximum=max(v[1] for v in delta.getextrema());drag_phase_max_error=max(drag_phase_max_error,maximum)
   assert maximum<=1, (phase_stamp,label,'mouse drag or hold froze/changed the real lunar phase contour',maximum)
  drag_phase_contour_metrics.append({'utc':phase_stamp,'gestures':len(drag_cases),'max_abs_channel':drag_phase_max_error})
 textures['moon']=moon_texture
 assert current(d,surf,surf,ctx)
 gl('glDeleteTextures',None,[I,C.POINTER(U)])(1,C.byref(U(uniform_moon)))
 # Read geographic normals and light from the real shader transform. Inverse
 # projection below uses the transpose of Rust's orthonormal matrix, never an
 # independently reimplemented yaw/pitch/roll formula.
 def transform(matrix,vector):
  return tuple(sum(matrix[column*3+row]*vector[column] for column in range(3)) for row in range(3))
 def inverse_transform(matrix,vector):
  return tuple(sum(matrix[column*3+row]*vector[row] for row in range(3)) for column in range(3))
 def unit(vector):
  length=math.sqrt(sum(v*v for v in vector));return tuple(v/length for v in vector)
 def uv_of(vector):
  x,y,z=unit(vector);return ((.5+math.atan2(x,z)/math.tau)%1,.5-math.asin(max(-1,min(1,y)))/math.pi)
 probe_source=source.split('void main() {',1)[0]+r"""
uniform vec3 probe_normal;
uniform vec3 probe_dx;
uniform vec3 probe_dy;
uniform vec4 probe_expected_gradients;
uniform bool gradient_probe;
void main() {
 vec3 n=earth_normal(probe_normal);
 if (gradient_probe) {
  vec2 dx=equirect_gradient(n,earth_normal(probe_dx));
  vec2 dy=equirect_gradient(n,earth_normal(probe_dy));
  vec4 error=abs(vec4(dx,dy)-probe_expected_gradients);
  bool ok=max(max(error.x,error.y),max(error.z,error.w))<.00002;
  frag_color=ok?vec4(0,1,0,1):vec4(1,0,0,1);
 } else {
  frag_color=vec4(n*.5+.5,dot(n,normalize(sun_direction))*.5+.5);
 }
}
"""
 probe_fragment=compile(probe_source,0x8B30);probe_program=gl('glCreateProgram',U,[])()
 attach(probe_program,vs);attach(probe_program,probe_fragment);gl('glLinkProgram',None,[U])(probe_program)
 linked=I();gl('glGetProgramiv',None,[U,U,C.POINTER(I)])(probe_program,0x8B82,C.byref(linked));assert linked.value
 gl('glUseProgram',None,[U])(probe_program);gl('glViewport',None,[I,I,I,I])(0,0,1,1);disable(0x0C11)
 def probe_pixel(matrix,sun,normal,derivatives=None,expected_gradients=None):
  upload_view(probe_program,matrix)
  for name,vector in [('sun_direction',sun),('probe_normal',normal)]:
   gl('glUniform3f',None,[I,F,F,F])(loc(probe_program,name.encode()),*vector)
  unif1i(loc(probe_program,b'gradient_probe'),int(derivatives is not None))
  if derivatives is not None:
   for name,vector in zip(('probe_dx','probe_dy'),derivatives):
    gl('glUniform3f',None,[I,F,F,F])(loc(probe_program,name.encode()),*vector)
   gl('glUniform4f',None,[I,F,F,F,F])(loc(probe_program,b'probe_expected_gradients'),*expected_gradients)
  gl('glDrawArrays',None,[U,I,I])(4,0,3)
  pixel=(C.c_ubyte*4)();read(0,0,1,1,0x1908,0x1401,pixel)
  assert gl('glGetError',U,[])()==0
  return list(pixel)
 geographic_probes=[(0,0,1),(1,0,0),(-1,0,0),(0,1,0),(0,-1,0),(0,0,-1),unit((.7,.4,-.6))]
 probe_cases=[('configured',label,
  celestial_frame(orientation_instant,minutes=20,latitude=latitude,longitude=longitude,roll=roll,axis=axis,rotating=rotating),
  celestial_frame(orientation_instant,body='moon',minutes=20,latitude=latitude,longitude=longitude,roll=roll,axis=axis,rotating=rotating))
  for label,latitude,longitude,roll,axis,rotating in orientation_cases]+drag_probe_cases
 probe_counts={group:{'geographic':0,'gradients':0,'moon':0} for group in ('configured','drag')}
 for group,label,(sun,matrix),(moon_sun,moon_matrix) in probe_cases:
  for body_normal in geographic_probes:
   actual=probe_pixel(matrix,sun,inverse_transform(matrix,body_normal))
   lighting=sum(v*s for v,s in zip(body_normal,unit(sun)))
   expected=[round((v*.5+.5)*255) for v in (*body_normal,lighting)]
   assert max(abs(a-b) for a,b in zip(actual,expected))<=1, (label,'UTC sunlight changed for a geographic location',actual,expected)
   probe_counts[group]['geographic']+=1
  phase_sun,_=celestial_frame(orientation_instant,body='moon',latitude=0,longitude=0)
  for view_normal in ((0,0,1),(.6,0,.8),(-.6,0,.8),(0,.6,.8),(0,-.6,.8)):
   actual=probe_pixel(moon_matrix,moon_sun,view_normal)[3]
   expected=round((sum(v*s for v,s in zip(view_normal,unit(phase_sun)))*.5+.5)*255)
   assert abs(actual-expected)<=1, (label,'Moon screen-space phase light changed',actual,expected)
   probe_counts[group]['moon']+=1
  # Independent central differences of spherical UV coordinates catch an
  # unrotated derivative even when the sampled centre normal is correct.
  for x,y in ((.13,.07),(-.31,.22),(.41,-.36)):
   normal=(x,y,math.sqrt(1-x*x-y*y));scale=.007
   derivatives=((scale,0,-x*scale/normal[2]),(0,scale,-y*scale/normal[2]))
   expected=[];epsilon=.001
   for derivative in derivatives:
    before=uv_of(transform(matrix,tuple(n-epsilon*dn for n,dn in zip(normal,derivative))))
    after=uv_of(transform(matrix,tuple(n+epsilon*dn for n,dn in zip(normal,derivative))))
    expected.extend((math.remainder(after[0]-before[0],1)/(2*epsilon),(after[1]-before[1])/(2*epsilon)))
   actual=probe_pixel(matrix,sun,normal,derivatives,expected)
   assert actual==[0,255,0,255], (label,'transformed analytic derivatives disagree with finite differences',x,y,actual,expected)
   probe_counts[group]['gradients']+=1
 # For points on the arcball's spherical region, the geographic point grabbed
 # by the mouse must arrive under the new pointer in all signed directions.
 # These expectations use sphere geometry, not a second arcball implementation.
 drag_anchor_checks=0
 for (label,held_seconds,options),(_,_,(sun,matrix),_) in zip(drag_cases,drag_probe_cases):
  start=options.get('start',(0,0));end=options.get('end',(0,0))
  if any(sum(v*v for v in point)>.5 for point in (start,end)):continue
  initial=celestial_frame(orientation_instant-held_seconds,**{key:value for key,value in options.items() if key not in ('start','end')})[1]
  grabbed=transform(initial,(*start,math.sqrt(1-sum(v*v for v in start))))
  actual=probe_pixel(matrix,sun,(*end,math.sqrt(1-sum(v*v for v in end))))
  lighting=sum(v*s for v,s in zip(grabbed,unit(sun)))
  expected=[round((v*.5+.5)*255) for v in (*grabbed,lighting)]
  assert max(abs(a-b) for a,b in zip(actual,expected))<=1, (label,'grabbed geography did not follow pointer',actual,expected)
  drag_anchor_checks+=1
 gl('glDeleteProgram',None,[U])(probe_program);gl('glDeleteShader',None,[U])(probe_fragment)
 gl('glUseProgram',None,[U])(prog);gl('glViewport',None,[I,I,I,I])(0,0,W,H)
 # The same physical instant represented on either side of the date line is
 # still the same image. A timezone is not a second rotation or Sun clock.
 timezone_frames=[];timezone_reference=None
 for stamp in ('2026-10-16T11:02:17+00:00','2026-10-16T19:02:17+08:00','2026-10-17T01:02:17+14:00','2026-10-15T23:02:17-12:00'):
  local_time=datetime.datetime.fromisoformat(stamp)
  assert local_time.timestamp()==orientation_instant
  output,_=layout_image(640,240,small_outputs,materials=materials,instant=local_time.timestamp(),offset_seconds=int(local_time.utcoffset().total_seconds()),earth_rotating=True,earth_axis=(.25,.5,.75),earth_roll=47,earth_minutes=20,moon_axis=(-.75,.25,.5),moon_roll=-71,moon_minutes=13)
  if timezone_reference is None: timezone_reference=output
  assert ImageChops.difference(timezone_reference.convert('RGB'),output.convert('RGB')).getbbox() is None, ('timezone shifted real Sun/Moon lighting',stamp)
  timezone_frames.append({'local_datetime':stamp,'utc_datetime':local_time.astimezone(datetime.timezone.utc).isoformat(),'pixel_identical':True})
 results[kind]['all_direction_rotation']={'frames':orientation_frames,'geographic_light_gpu_probes':probe_counts['configured']['geographic'],'moon_screen_light_gpu_probes':probe_counts['configured']['moon'],'transformed_gradient_finite_difference_probes':probe_counts['configured']['gradients'],'moon_phase_contours':phase_contour_metrics,'equivalent_timezone_frames':timezone_frames,'all_signed_axes_poles_roll':'PASS','earth_geographic_utc_light':'PASS','moon_real_phase_under_rotation':'PASS'}
 results[kind]['mouse_drag_rotation']={'frames':drag_frames,'geographic_light_gpu_probes':probe_counts['drag']['geographic'],'moon_screen_light_gpu_probes':probe_counts['drag']['moon'],'transformed_gradient_finite_difference_probes':probe_counts['drag']['gradients'],'grabbed_geographic_point_gpu_probes':drag_anchor_checks,'moon_phase_contours':drag_phase_contour_metrics,'all_signed_directions_poles_limb':'PASS','stationary_click_pose':'PASS','geography_follows_pointer':'PASS','held_pose_current_utc_light':'PASS'}
 results[kind]['dual_materials']={'frames':dual_metrics,'halo_pixels_equal_sky':halo_checked,'moon_surface_pixels':surface_checked,'moon_max_blue_minus_red':max_blue_excess,'mixed_material_repair':'PASS','reordered_outputs':'PASS','stationary_sky_both_materials':True,'clouds_earth_only':'PASS'}
 results[kind]['optional_sky']={'texture_loaded':sky_available,'texture_sha256':hashlib.sha256(sky_path.read_bytes()).hexdigest() if sky_available else None,'missing_fallback':'PASS','stationary_fallback':'PASS','fallback_damage':'PASS'}
 # Motion controls keep physical body time fixed while varying only sky UTC.
 motion_frames=[]
 for textured in (False,True):
  moving=[]
  for seconds in (0,1/15,60,21600,86400,86400-0.001,86400+0.001,37152-0.001,37152+0.001,43200-0.001,43200+0.001):
   im,_=layout_image(1920,540,[(0,0,960,540),(960,0,960,540)],materials={(0,0):'earth',(960,0):'moon'},sky_minutes=1440,sky_seconds=seconds,sky_enabled=textured)
   filename=f'{kind}-sky-motion-{textured}-{seconds:g}.png';im.save(ROOT/filename)
   motion_frames.append({'file':filename,'sky_seconds':seconds,'textured':textured})
   moving.append(im.convert('RGB'))
  assert ImageChops.difference(moving[0],moving[4]).getbbox() is None, 'sky period/restart did not repeat'
  assert max(v[1] for v in ImageChops.difference(moving[5],moving[6]).getextrema())<=1, 'sky jumps across phase wrap'
  for before,after in ((7,8),(9,10)):
   assert max(v[1] for v in ImageChops.difference(moving[before],moving[after]).getextrema())<=1, 'sky jumps across longitude seam/atan branch'
  assert ImageChops.difference(moving[0].crop((0,0,300,100)),moving[2].crop((0,0,300,100))).getbbox() is not None, 'sky did not move after one minute'
  for center in (480,1440):
   body_box=(center-100,170,center+100,370)
   assert all(ImageChops.difference(moving[0].crop(body_box),im.crop(body_box)).getbbox() is None for im in moving[1:]), 'sky motion altered Earth/Moon interior'
  disabled,_=layout_image(1920,540,[(0,0,960,540),(960,0,960,540)],materials={(0,0):'earth',(960,0):'moon'},sky_minutes=0,sky_seconds=21600,sky_enabled=textured)
  disabled_later,_=layout_image(1920,540,[(0,0,960,540),(960,0,960,540)],materials={(0,0):'earth',(960,0):'moon'},sky_minutes=0,sky_seconds=86400,sky_enabled=textured)
  assert ImageChops.difference(disabled.convert('RGB'),disabled_later.convert('RGB')).getbbox() is None, 'disabled sky moves'
  for seconds in (0,21600):
   wide_image,_=layout_image(1920,540,[(0,0,1920,540)],sky_minutes=1440,sky_seconds=seconds,sky_enabled=textured)
   filename=f'{kind}-sky-ultrawide-{textured}-{seconds}.png';wide_image.save(ROOT/filename)
   motion_frames.append({'file':filename,'sky_seconds':seconds,'textured':textured,'aspect':'32:9'})
 results[kind]['sky_motion']={'frames':motion_frames,'period_wrap_restart':'PASS','body_interiors':'PASS','static_disable':'PASS','one_minute_motion':'PASS'}
 # The exploration path runs the real panorama projection and shared sky,
 # independently from the retained legacy-framing regression controls above.
 cosmos_frames=[]
 catalog_path=REPO/'assets/astronomy/galaxy-nasa-4k.png'
 catalog=Image.open(catalog_path).convert('RGBA')
 assert catalog.size==(4096,2048) and catalog.getchannel('A').getextrema()==(0,255)
 legacy_sky_tex,legacy_sky_available=sky_tex,sky_available
 sky_tex=U(upload_filter_fixture(catalog));sky_available=True
 def cosmos_image(label,pose,body='earth',zoom=0.35,mapped=True,detail=None,instant=BASE+43200,repair=None):
  im,_=layout_image(640,480,[(0,0,640,480)],materials={(0,0):body},
                    panorama=pose,body_zoom=zoom,sky_enabled=mapped,detail=detail,
                    moon_rotating=False,instant=instant,repair=repair)
  filename=f'{kind}-cosmos-{label}.png';im.save(ROOT/filename)
  cosmos_frames.append({'file':filename,'body':body,'body_zoom':zoom,'fov_degrees':pose[0],'mapped':mapped})
  return im.convert('RGB')
 scale=2*math.tan(math.radians(60)/2)/480
 pose_cases=[('centre',0,0),('left',math.pi/2,0),('back',math.pi,0),
             ('right',-math.pi/2,0),('up',0,-math.pi/2),('down',0,math.pi/2),
             ('past-pole',0,math.pi*.8),('oblique',1.7,-2.2),('full-turn',2*math.pi,0)]
 poses={name:panorama_frame(dx=yaw/scale,dy=pitch/scale) for name,yaw,pitch in pose_cases}
 panorama_images={name:cosmos_image(name,pose) for name,pose in poses.items()}
 assert max(v[1] for v in ImageChops.difference(panorama_images['centre'],panorama_images['full-turn']).getextrema())<=1,'360-degree sky failed to close'
 sky_box=(0,0,640,140)
 assert len({hashlib.sha256(im.crop(sky_box).tobytes()).hexdigest() for name,im in panorama_images.items() if name!='full-turn'})==8,'full-sphere views repeat a flat crop'
 for name,im in panorama_images.items():
  assert ImageChops.difference(im.crop((285,205,355,275)),panorama_images['centre'].crop((285,205,355,275))).getbbox() is None,'sky camera changed body geography or true-time light'
 for mapped in (True,False):
  a=cosmos_image(f'seam-{mapped}-before',panorama_frame(dx=(math.pi-1e-7)/scale),mapped=mapped)
  b=cosmos_image(f'seam-{mapped}-after',panorama_frame(dx=(math.pi+1e-7)/scale),mapped=mapped)
  assert max(v[1] for v in ImageChops.difference(a,b).getextrema())<=1,'celestial longitude seam flickers'
  x=cosmos_image(f'fallback-{mapped}-opposite',poses['back'],mapped=mapped)
  y=cosmos_image(f'fallback-{mapped}-origin',poses['centre'],mapped=mapped)
  assert ImageChops.difference(x.crop(sky_box),y.crop(sky_box)).getbbox() is not None
 for steps in (-64,6,64):
  pose=panorama_frame(steps=steps)
  im=cosmos_image(f'lens-{steps}',pose)
  assert ImageChops.difference(im.crop(sky_box),panorama_images['centre'].crop(sky_box)).getbbox() is not None,'sky wheel did not change actual perspective'
 # Sample the actual production ray function on GPU and compare against an
 # independent scalar perspective reference. This catches transposed matrices,
 # wrong vertical FOV, Y inversion and rays that stop short of the poles.
 assert current(d,surf,surf,ctx)
 diagnostic=source[:source.index('void main() {')]+'''void main(){
 vec3 dx,dy;vec3 ray=panorama_ray(gl_FragCoord.xy-canvas_origin,dx,dy);
 frag_color=vec4(ray*0.5+0.5,1.0);
}'''
 probe_shader=compile(diagnostic,0x8B30);probe=gl('glCreateProgram',U,[])()
 attach(probe,vs);attach(probe,probe_shader);gl('glLinkProgram',None,[U])(probe)
 linked=I();gl('glGetProgramiv',None,[U,U,C.POINTER(I)])(probe,0x8B82,C.byref(linked));assert linked.value
 gl('glUseProgram',None,[U])(probe)
 probe_count=0
 for pose in [*poses.values(),panorama_frame(steps=64),panorama_frame(steps=-64)]:
  fov,matrix=pose
  gl('glViewport',None,[I,I,I,I])(0,0,W,H);disable(0x0C11)
  gl('glUniform2f',None,[I,F,F])(loc(probe,b'resolution'),W,H)
  gl('glUniform2f',None,[I,F,F])(loc(probe,b'canvas_origin'),0,0)
  unif1f(loc(probe,b'panorama_fov'),fov)
  gl('glUniformMatrix3fv',None,[I,I,C.c_ubyte,C.POINTER(F)])(loc(probe,b'panorama_view'),1,0,(F*9)(*matrix))
  gl('glDrawArrays',None,[U,I,I])(4,0,3)
  for px,py in [(0,0),(W-1,0),(0,H-1),(W-1,H-1),(W//2,H//2),(W//3,H*2//3)]:
   pixel=(C.c_ubyte*4)();read(px,py,1,1,0x1908,0x1401,pixel)
   k=2*math.tan(math.radians(fov)/2)/H
   camera=[(px+.5-W*.5)*k,(py+.5-H*.5)*k,1.0]
   length=math.sqrt(sum(v*v for v in camera));camera=[v/length for v in camera]
   ray=[sum(matrix[column*3+row]*camera[column] for column in range(3)) for row in range(3)]
   expected=[round((v*.5+.5)*255) for v in ray]
   assert max(abs(pixel[i]-expected[i]) for i in range(3))<=1,(pose,px,py,list(pixel),expected)
   probe_count+=1
 gl('glUseProgram',None,[U])(prog);gl('glDeleteProgram',None,[U])(probe);gl('glDeleteShader',None,[U])(probe_shader)
 # Point the production camera at each real catalogue direction. Camera axes
 # are built independently; all landmark colours remain in a bounded sky cap.
 def direction_pose(galactic):
  gx,gy,gz=galactic
  def unit(v):
   length=math.sqrt(sum(x*x for x in v));return [x/length for x in v]
  forward=unit([-.3746065934*gy-.9271838546*gz,.9271838546*gy-.3746065934*gz,gx])
  right=unit([forward[2],0.,-forward[0]])
  up=[forward[1]*right[2]-forward[2]*right[1],forward[2]*right[0]-forward[0]*right[2],forward[0]*right[1]-forward[1]*right[0]]
  return 8.,right+up+forward
 landmarks=[('orion',[-.824943831,-.331876518,-.457521204]),
            ('lagoon',[.994392948,-.020361,.103769428]),
            ('carina',[.299894690,-.010897463,-.953910069]),
            ('eagle',[.956448362,.013831226,.291573709])]
 # Exact poles cannot be hidden behind the globe in this diagnostic. Cardinal
 # longitude values are an independent catalogue-orientation oracle, including
 # NASA's left-increasing longitude convention and the atan(0,0) guard.
 assert current(d,surf,surf,ctx)
 coordinate_source=source[:source.index('void main() {')]+'''uniform vec3 catalog_probe;
 uniform int catalog_probe_mode;
 void main(){
  if(catalog_probe_mode==1){
   frag_color=vec4(cosmic_nebulae(catalog_probe,0.0003)*4.0,1.0);return;
  }
  vec2 uv=cosmic_uv(catalog_probe);
  vec2 dx=cosmic_gradient(catalog_probe,vec3(1,0,0));
  vec2 dy=cosmic_gradient(catalog_probe,vec3(0,1,0));
  bool finite=!(any(isnan(uv))||any(isinf(uv))||any(isnan(dx))||any(isinf(dx))||any(isnan(dy))||any(isinf(dy)));
  frag_color=vec4(uv,finite?1.0:0.0,1.0);
 }'''
 coordinate_shader=compile(coordinate_source,0x8B30);coordinate_program=gl('glCreateProgram',U,[])()
 attach(coordinate_program,vs);attach(coordinate_program,coordinate_shader);gl('glLinkProgram',None,[U])(coordinate_program)
 linked=I();gl('glGetProgramiv',None,[U,U,C.POINTER(I)])(coordinate_program,0x8B82,C.byref(linked));assert linked.value
 gl('glUseProgram',None,[U])(coordinate_program);gl('glViewport',None,[I,I,I,I])(0,0,1,1);disable(0x0C11)
 def catalog_probe_pixel(direction,mode=0):
  gl('glUniform3f',None,[I,F,F,F])(loc(coordinate_program,b'catalog_probe'),*direction)
  unif1i(loc(coordinate_program,b'catalog_probe_mode'),mode)
  gl('glDrawArrays',None,[U,I,I])(4,0,3)
  value=(C.c_ubyte*4)();read(0,0,1,1,0x1908,0x1401,value)
  assert gl('glGetError',U,[])()==0
  return list(value)
 coordinate_probes=[([1,0,0],[.5,.5]),([0,0,1],[.25,.5]),
                    ([-1,0,0],[0,.5]),([0,0,-1],[.75,.5]),
                    ([0,1,0],[.5,0]),([0,-1,0],[.5,1])]
 for direction,expected in coordinate_probes:
  value=catalog_probe_pixel(direction)
  longitude_error=abs(value[0]-round(expected[0]*255))
  if expected[0]==0:longitude_error=min(longitude_error,abs(value[0]-255))
  assert longitude_error<=1 and abs(value[1]-round(expected[1]*255))<=1,(direction,value,expected)
  assert value[2:]==[255,255],('nonfinite pole/gradient',direction,value)
 for name,direction in landmarks:
  value=catalog_probe_pixel(direction,1)
  assert max(value[:3])>5,('missing nebula at catalogue direction',name,value)
  assert catalog_probe_pixel([-v for v in direction],1)[:3]==[0,0,0],('nebula leaks outside spherical cap',name)
 gl('glUseProgram',None,[U])(prog);gl('glDeleteProgram',None,[U])(coordinate_program);gl('glDeleteShader',None,[U])(coordinate_shader)
 # Keep the inspecting globe small so each nebula can be seen around it.
 for name,direction in landmarks:
  cosmos_image('nebula-'+name,direction_pose(direction),zoom=0.035)
 for name,direction in [('north',[1e-7,1,0]),('south',[1e-7,-1,0])]:
  cosmos_image('galactic-pole-'+name,direction_pose(direction))
 # Real high-resolution maps must supply additional spatial information at a
 # fixed camera/zoom, not just enlarge a pre-rendered low-resolution frame.
 detail_metrics=[]
 for body,path in [('earth',REPO/'assets/astronomy/earth-nasa-october.jpg'),('moon',REPO/'assets/astronomy/moon-nasa-lroc-8k.jpg')]:
  original=textures[body]
  actual=Image.open(path).convert('RGBA')
  base_size=(4096,2048) if body=='earth' else (2048,1024)
  low=actual.resize(base_size,Image.Resampling.LANCZOS)
  low_texture=upload_filter_fixture(low);high_texture=upload_filter_fixture(actual)
  textures[body]=low_texture
  lit_instant=BASE+4*3600 if body=='earth' else BASE+11*86400+16*3600
  low_image=cosmos_image(body+'-zoom-base',poses['centre'],body=body,zoom=8,detail=0,instant=lit_instant)
  textures[body]=high_texture
  high_image=cosmos_image(body+'-zoom-detail',poses['centre'],body=body,zoom=8,detail=0,instant=lit_instant)
  grain_image=cosmos_image(body+'-zoom-material',poses['centre'],body=body,zoom=8,instant=lit_instant)
  delta=ImageChops.difference(low_image,high_image)
  assert delta.getbbox() is not None,'high-resolution map was not actually sampled'
  assert max(v[1] for v in delta.getextrema())>=2,'near LOD contains no additional texture information'
  assert ImageChops.difference(high_image,grain_image).getbbox() is not None,'filtered material LOD was not reached'
  detail_metrics.append({'body':body,'native_size':actual.size,'base_size':low.size,'mean_difference':sum(ImageStat.Stat(delta).mean)/3,'max_difference':max(v[1] for v in delta.getextrema()),'file_sha256':hashlib.sha256(path.read_bytes()).hexdigest()})
  textures[body]=original
  assert current(d,surf,surf,ctx)
  for texture in [low_texture,high_texture]:gl('glDeleteTextures',None,[I,C.POINTER(U)])(1,C.byref(U(texture)))
 results[kind]['cosmic_exploration']={'frames':cosmos_frames,'production_ray_gpu_probes':probe_count,'catalog_coordinate_gpu_probes':len(coordinate_probes),'nebula_cap_gpu_probes':len(landmarks)*2,'panorama_full_sphere':'PASS','360_wrap':'PASS','longitude_seam_textured_and_fallback':'PASS','body_light_and_geography_unchanged':'PASS','zoom_detail':detail_metrics,'catalog_sha256':hashlib.sha256(catalog_path.read_bytes()).hexdigest()}
 sky_tex,sky_available=legacy_sky_tex,legacy_sky_available
 results[kind]['layouts']=layout_metrics
 results[kind]['layout_pixel_assertions']='PASS: dual centers, portrait radius, gaps, negative clipping, repairs, mirror dedup, stable overlaps, outside-root'
 current(d,None,None,None)
 egl('eglDestroySurface',U,[P,P])(d,wide)
 current(d,None,None,None)
 egl('eglDestroySurface',U,[P,P])(d,surf);egl('eglDestroyContext',U,[P,P])(d,ctx)
if os.environ.get('ASTRONOMY_TEST_SURFACES_ONLY')=='1':
 max_error=0
 for a,b in zip(results['gl330']['celestial_surfaces']['frames'],results['gles300']['celestial_surfaces']['frames']):
  error=max(high for _,high in ImageChops.difference(Image.open(ROOT/a['file']),Image.open(ROOT/b['file'])).getextrema())
  assert error<=2,('GL/GLES catalogue mismatch',a['scene'],error)
  max_error=max(max_error,error)
 results['gl_gles_max_channel_difference']=max_error
 results['scope']='Surfaceless Mesa GL330/GLES300: all eleven scenes, observed maps, scientific fallback fidelity, Saturn rings; no live compositor/hotplug claims.'
 (ROOT/'celestial-surfaces-metrics.json').write_text(json.dumps(results,indent=2))
 print(json.dumps({'result':'PASS','output':str(ROOT),'apis':list(results)[:2],'gl_gles_max_channel_difference':max_error}))
 raise SystemExit(0)
results['source_sha256']=hashlib.sha256(source.encode()).hexdigest()
results['production_astronomy_source_sha256']=hashlib.sha256((REPO/'src/backend/compositor_common/animated_wallpaper.rs').read_bytes()).hexdigest()
results['production_panorama_source_sha256']=hashlib.sha256((REPO/'src/backend/compositor_common/panorama.rs').read_bytes()).hexdigest()
results['shared_sky_sha256']=hashlib.sha256((REPO/'src/backend/x11/compositor/shaders/cosmic_sky.glsl').read_bytes()).hexdigest()

# Both real APIs must agree within one byte of raster/float rounding.
api_max_error=0
for a,b in zip(results['gl330']['metrics'],results['gles300']['metrics']):
 delta=ImageChops.difference(Image.open(ROOT/a['file']).convert('RGB'),Image.open(ROOT/b['file']).convert('RGB'))
 api_max_error=max(api_max_error,max(v[1] for v in delta.getextrema()))
 assert api_max_error <= 1, (a['file'],delta.getextrema())
for a,b in zip(results['gl330']['layouts'],results['gles300']['layouts']):
 delta=ImageChops.difference(Image.open(ROOT/a['file']).convert('RGB'),Image.open(ROOT/b['file']).convert('RGB'))
 api_max_error=max(api_max_error,max(v[1] for v in delta.getextrema()))
 assert api_max_error <= 1, (a['file'],delta.getextrema())
for a,b in zip(results['gl330']['dual_materials']['frames'],results['gles300']['dual_materials']['frames']):
 delta=ImageChops.difference(Image.open(ROOT/a['file']).convert('RGB'),Image.open(ROOT/b['file']).convert('RGB'))
 api_max_error=max(api_max_error,max(v[1] for v in delta.getextrema()))
 assert api_max_error <= 1, (a['file'],delta.getextrema())
for group in ('fixed_earth','seasonal_earth','rotating_earth','lunar_phases','sky_motion','texture_filtering','all_direction_rotation','mouse_drag_rotation','cosmic_exploration'):
 for a,b in zip(results['gl330'][group]['frames'],results['gles300'][group]['frames']):
  delta=ImageChops.difference(Image.open(ROOT/a['file']).convert('RGB'),Image.open(ROOT/b['file']).convert('RGB'))
  api_max_error=max(api_max_error,max(v[1] for v in delta.getextrema()))
  assert api_max_error <= 1, (a['file'],delta.getextrema())
# Optional retained baseline: report intentional appearance changes, while
# geometry/timing compatibility remains checked above. Never claim old pixels
# are unchanged merely because both current graphics APIs agree.
if os.environ.get('ASTRONOMY_TEST_BASELINE_OUTPUT'):
 baseline=Path(os.environ['ASTRONOMY_TEST_BASELINE_OUTPUT'])
 compatibility=[]
 for kind in ('gl330','gles300'):
  for frame in results[kind]['metrics']:
   old=baseline/frame['file']
   if not old.is_file(): raise AssertionError(f'Missing baseline frame: {old}')
   delta=ImageChops.difference(Image.open(old).convert('RGB'),Image.open(ROOT/frame['file']).convert('RGB'))
   compatibility.append({'file':frame['file'],'pixel_identical':delta.getbbox() is None,'mean_abs_rgb':sum(ImageStat.Stat(delta).mean)/3,'max_abs_channel':max(v[1] for v in delta.getextrema())})
 results['legacy_visual_comparison']=compatibility
 # Same dated instants and unchanged Earth behavior. Do not compare new
 # fixed Earth imagery with the old studio-lit Earth and call it a regression.
 unchanged=[]
 for kind in ('gl330','gles300'):
  controls=[(f'{kind}-earth-moon-{seconds:g}s.png',(0,0,2560,1440)) for seconds in (0,1/15,1,10,60)]
  controls += [(f'{kind}-default-period-{elapsed}s-zone{zone}.png',(0,0,320,240)) for elapsed,zone in ((0,28800),(21600,28800),(86400,28800),(0,0))]
  controls.append((f'{kind}-galaxy-only.png',None))
  for group in ('fixed_earth','seasonal_earth','rotating_earth'):
   controls.extend((frame['file'],None) for frame in results[kind][group]['frames'])
  for filename,crop in controls:
   old=Image.open(baseline/filename).convert('RGB');new=Image.open(ROOT/filename).convert('RGB')
   if crop is not None: old=old.crop(crop);new=new.crop(crop)
   delta=ImageChops.difference(old,new);maximum=max(v[1] for v in delta.getextrema())
   assert maximum<=1, (filename,'Earth/galaxy baseline changed',delta.getextrema())
   unchanged.append({'file':filename,'scope':'earth-and-sky' if crop or 'galaxy-only' not in filename else 'galaxy-only','crop':crop,'pixel_identical':delta.getbbox() is None,'max_abs_channel':maximum})
 results['retained_earth_galaxy_baseline']=unchanged
results['gl_gles_max_channel_difference']=api_max_error
results['scope']='Actual surfaceless Mesa shader execution and production Rust UTC-solar/3D-orientation/arcball/geometry helpers; signed axes and mouse gestures, poles and roll, grabbed geographic points, held-pose current lighting and real lunar phase invariants, transformed finite-difference gradient probes, equivalent civil timestamps, exact dated seasonal renders and simulated output layouts. WM event routing and compositor drag lifecycle are covered by separate Rust regressions. No hardware compositor, real hotplug, or device deployment tested.'
print('PASS: GLSL/GLES, all-direction rotation and mouse arcball, geographic UTC sunlight, rotation-independent lunar phase, transformed gradients, civil-time equivalence, opaque frames and production multi-output plans; output:',ROOT)

(ROOT/'earth-validation.json').write_text(json.dumps(results,indent=2)+'\n')
print(json.dumps(results,indent=2))
