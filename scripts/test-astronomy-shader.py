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
helper.write_text('#[path = '+json.dumps(str(REPO/'src/backend/compositor_common/animated_wallpaper.rs'))+'] mod astronomy;\n'+r"""
fn main() {
 let a:Vec<String>=std::env::args().skip(1).collect();
 let mode=&a[0]; let t:f64=a[1].parse().unwrap();
 let (sun,longitude)=match mode.as_str() {
  // Explicit historical controls retain the baseline files and old pixel comparisons.
  "legacy-clock" => (astronomy::solar_direction(t),astronomy::viewing_longitude(t,a[2].parse().unwrap())),
  "legacy-studio" | "moon" => {
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
subprocess.run([RUSTC,str(helper),'-Awarnings','-o',str(ROOT/'astronomy_values')],check=True)
def astronomy_values(instant,mode='earth',minutes=1440,longitude=DEFAULT_LONGITUDE,rotating=False,offset_seconds=0):
 args=[mode,str(instant)]
 args.extend([str(minutes),str(longitude),str(rotating).lower()] if mode=='earth' else [str(offset_seconds),str(minutes)])
 return list(map(float,subprocess.check_output([str(ROOT/'astronomy_values'),*args]).split()))
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
subprocess.run([RUSTC,str(planner),'-Awarnings','-o',str(ROOT/'astronomy_plan')],check=True)
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
source=SOURCE.read_text()
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
 assert loc(prog,b'view_latitude')>=0 and loc(prog,b'view_longitude')>=0 and loc(prog,b'sun_direction')>=0, 'physical camera/lighting uniforms are inactive'
 gl('glUniform2f',None,[I,F,F])(loc(prog,b'resolution'),W,H)
 gl('glUniform2f',None,[I,F,F])(loc(prog,b'canvas_origin'),0,0)
 unif1f(loc(prog,b'sphere_radius'),layout_plan(W,H,[],False)[0]['radius'])
 # Historical controls use the exact previous 0.18-radian camera latitude.
 unif1f(loc(prog,b'view_latitude'),0.18)
 textures={}
 param=gl('glTexParameteri',None,[U,U,I])
 for body,asset in [('earth',os.environ.get('ASTRONOMY_TEST_TEXTURE',str(REPO/'assets/astronomy/earth-nasa-october.jpg'))),('moon',str(REPO/'assets/astronomy/moon-nasa-lroc.jpg'))]:
  tex=U();gl('glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(tex));gl('glBindTexture',None,[U,U])(0x0DE1,tex)
  for key,val in ((0x2801,0x2601),(0x2800,0x2601),(0x2802,0x2901),(0x2803,0x812F)):param(0x0DE1,key,val)
  texture=Image.open(asset).convert('RGBA');data=C.create_string_buffer(texture.tobytes())
  gl('glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0x0DE1,0,0x1908,texture.width,texture.height,0,0x1908,0x1401,data)
  textures[body]=tex.value
 # Match production texture units: surface=0, NASA clouds=1, optional sky=2.
 gl('glActiveTexture',None,[U])(0x84C1)
 cloud_tex=U();gl('glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(cloud_tex));gl('glBindTexture',None,[U,U])(0x0DE1,cloud_tex)
 for key,val in ((0x2801,0x2601),(0x2800,0x2601),(0x2802,0x2901),(0x2803,0x812F)):param(0x0DE1,key,val)
 clouds=Image.open(Path(os.environ.get('ASTRONOMY_TEST_CLOUD_TEXTURE',str(REPO/'assets/astronomy/earth-nasa-clouds.jpg')))).convert('RGBA');cloud_data=C.create_string_buffer(clouds.tobytes())
 gl('glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0x0DE1,0,0x1908,clouds.width,clouds.height,0,0x1908,0x1401,cloud_data)
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
 else:
  gl('glBindTexture',None,[U,U])(0x0DE1,0)
 unif1i(loc(prog,b'sky_map'),2);unif1i(loc(prog,b'sky_enabled'),int(sky_available))
 gl('glActiveTexture',None,[U])(0x84C0)
 gl('glBindTexture',None,[U,U])(0x0DE1,textures['earth'])
 unif1i(loc(prog,b'earth_map'),0)
 images=[];metrics=[]
 cases=[(h,z) for z in (0,8) for h in (0,6,12,18)]+[(12+1/60,0),(12+1,0),(6+1/54000,0),(6+1/3600,0)]
 for hour,zone in cases:
  values=astronomy_values(BASE+hour*3600,mode='legacy-clock',offset_seconds=zone*3600)
  gl('glUniform3f',None,[I,F,F,F])(loc(prog,b'sun_direction'),*values[:3]);unif1f(loc(prog,b'view_longitude'),values[3])
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
   unif1f(loc(hard_program,b'view_latitude'),0.18)
   gl('glUniform3f',None,[I,F,F,F])(loc(hard_program,b'sun_direction'),*values[:3]);unif1f(loc(hard_program,b'view_longitude'),values[3])
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
 results[kind]={'version':version,'renderer':renderer,'compile_link':'PASS','opaque_all_frames':True,'distinct_frames':len(cases),'motion':diffs,'metrics':metrics,'legacy_controls':'Historical timezone orbit, latitude 0.18 radians; not production default.'}
 # Full root framebuffer test, two 2560x1440 outputs joined at x=2560.
 wide=surface(d,cfg,attrs(0x3057,5120,0x3056,1440,0x3038));assert wide
 assert current(d,wide,wide,ctx)
 gl('glViewport',None,[I,I,I,I])(0,0,5120,1440)
 gl('glUniform2f',None,[I,F,F])(loc(prog,b'resolution'),5120,1440)
 gl('glUniform2f',None,[I,F,F])(loc(prog,b'canvas_origin'),0,0)
 unif1f(loc(prog,b'sphere_radius'),layout_plan(5120,1440,[],False)[0]['radius'])
 values=astronomy_values(PREVIEW_INSTANT)
 unif1f(loc(prog,b'view_latitude'),math.radians(DEFAULT_LATITUDE))
 gl('glUniform3f',None,[I,F,F,F])(loc(prog,b'sun_direction'),*values[:3]);unif1f(loc(prog,b'view_longitude'),values[3])
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
 def layout_image(width,height,outputs,repair=None,materials=None,instant=BASE+120,clouds_enabled=True,earth_minutes=1440,moon_minutes=39343,offset_seconds=0,sky_enabled=None,earth_rotating=False,earth_latitude=DEFAULT_LATITUDE,earth_longitude=DEFAULT_LONGITUDE,frozen_sun=None,legacy_earth=False):
  target=surface(d,cfg,attrs(0x3057,width,0x3056,height,0x3038));assert target
  assert current(d,target,target,ctx)
  gl('glViewport',None,[I,I,I,I])(0,0,width,height)
  unif1i(loc(prog,b'clouds_enabled'),int(clouds_enabled))
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
   if body=='moon' or legacy_earth:
    values=astronomy_values(instant,mode='moon' if body=='moon' else 'legacy-studio',minutes=moon_minutes if body=='moon' else earth_minutes,offset_seconds=0 if body=='moon' else offset_seconds)
    latitude=0.18
   else:
    values=astronomy_values(instant,minutes=earth_minutes,longitude=earth_longitude,rotating=earth_rotating)
    latitude=math.radians(earth_latitude)
   gl('glUniform3f',None,[I,F,F,F])(loc(prog,b'sun_direction'),*(frozen_sun if body=='earth' and frozen_sun is not None else values[:3]))
   unif1f(loc(prog,b'view_longitude'),values[3]);unif1f(loc(prog,b'view_latitude'),latitude)
   enable(0x0C11);scissor(*draw_plan['clip'])
   gl('glUniform2f',None,[I,F,F])(loc(prog,b'canvas_origin'),*draw_plan['origin'])
   gl('glUniform2f',None,[I,F,F])(loc(prog,b'resolution'),*draw_plan['size'])
   unif1f(loc(prog,b'sphere_radius'),draw_plan['radius'])
   gl('glDrawArrays',None,[U,I,I])(4,0,3)
  if repair is None: disable(0x0C11)
  else: enable(0x0C11);scissor(*repair)
  pixels=(C.c_ubyte*(width*height*4))();read(0,0,width,height,0x1908,0x1401,pixels)
  assert gl('glGetError',U,[])()==0
  output=Image.frombytes('RGBA',(width,height),bytes(pixels)).transpose(Image.Transpose.FLIP_TOP_BOTTOM)
  assert output.getchannel('A').getextrema()==(255,255)
  current(d,None,None,None);egl('eglDestroySurface',U,[P,P])(d,target)
  return output,plan
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
 # Explicit rotating Earth (physical sunlight) plus unchanged studio-lit Moon.
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
 results[kind]['dual_materials']={'frames':dual_metrics,'halo_pixels_equal_sky':halo_checked,'moon_surface_pixels':surface_checked,'moon_max_blue_minus_red':max_blue_excess,'mixed_material_repair':'PASS','reordered_outputs':'PASS','stationary_sky_both_materials':True,'clouds_earth_only':'PASS'}
 results[kind]['optional_sky']={'texture_loaded':sky_available,'texture_sha256':hashlib.sha256(sky_path.read_bytes()).hexdigest() if sky_available else None,'missing_fallback':'PASS','stationary_fallback':'PASS','fallback_damage':'PASS'}
 results[kind]['layouts']=layout_metrics
 results[kind]['layout_pixel_assertions']='PASS: dual centers, portrait radius, gaps, negative clipping, repairs, mirror dedup, stable overlaps, outside-root'
 current(d,None,None,None)
 egl('eglDestroySurface',U,[P,P])(d,wide)
 current(d,None,None,None)
 egl('eglDestroySurface',U,[P,P])(d,surf);egl('eglDestroyContext',U,[P,P])(d,ctx)
results['source_sha256']=hashlib.sha256(source.encode()).hexdigest()
results['production_astronomy_source_sha256']=hashlib.sha256((REPO/'src/backend/compositor_common/animated_wallpaper.rs').read_bytes()).hexdigest()

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
for group in ('fixed_earth','seasonal_earth','rotating_earth'):
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
 # Same dated instants and same unchanged Moon periods. Do not compare new
 # fixed Earth imagery with the old studio-lit Earth and call it a regression.
 unchanged=[]
 for kind in ('gl330','gles300'):
  controls=[(f'{kind}-earth-moon-{seconds:g}s.png',(2560,0,5120,1440)) for seconds in (0,1/15,1,10,60)]
  controls += [(f'{kind}-default-period-{elapsed}s-zone{zone}.png',(320,0,640,240)) for elapsed,zone in ((0,28800),(21600,28800),(86400,28800),(0,0))]
  controls.append((f'{kind}-galaxy-only.png',None))
  for filename,crop in controls:
   old=Image.open(baseline/filename).convert('RGB');new=Image.open(ROOT/filename).convert('RGB')
   if crop is not None: old=old.crop(crop);new=new.crop(crop)
   delta=ImageChops.difference(old,new);maximum=max(v[1] for v in delta.getextrema())
   assert maximum<=1, (filename,'Moon/galaxy baseline changed',delta.getextrema())
   unchanged.append({'file':filename,'scope':'moon-and-sky' if crop else 'galaxy-only','crop':crop,'pixel_identical':delta.getbbox() is None,'max_abs_channel':maximum})
 results['retained_moon_galaxy_baseline']=unchanged
results['gl_gles_max_channel_difference']=api_max_error
results['scope']='Actual surfaceless Mesa shader execution and production Rust UTC-solar/fixed-view/rotation/geometry helpers; exact dated fixed and seasonal/polar renders; simulated output layouts. No hardware compositor, real hotplug, or device deployment tested.'
print('PASS: GLSL/GLES, fixed Guangzhou UTC day/night, seasonal/polar dusk, explicit rotation, opaque frames, stationary sky, production multi-output plans; output:',ROOT)

(ROOT/'earth-validation.json').write_text(json.dumps(results,indent=2)+'\n')
print(json.dumps(results,indent=2))
