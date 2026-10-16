#!/usr/bin/env python3
"""Actual procedural solar shader rendering on desktop GL330 and GLES300 Mesa."""
import ctypes as C, os, json, math, tempfile, subprocess, shutil, time, statistics
from pathlib import Path
from PIL import Image, ImageChops, ImageStat
os.environ.setdefault('LIBGL_ALWAYS_SOFTWARE','1')
REPO=Path(__file__).resolve().parents[1]
ROOT=Path(os.environ.get('SOLAR_TEST_OUTPUT','/tmp/jwm-solar-preview')); ROOT.mkdir(parents=True,exist_ok=True)
SOURCE=Path(os.environ.get('SOLAR_SHADER',str(REPO/'src/backend/x11/compositor/shaders/solar_system.frag')))
BENCH_ONLY=bool(os.environ.get('SOLAR_BENCH_ONLY'))
W,H=int(os.environ.get("PREVIEW_WIDTH",960)),int(os.environ.get("PREVIEW_HEIGHT",540))
# Exercise the production f64 phase reduction and camera basis, not Python copies.
helper=ROOT/'solar_values.rs'
helper.write_text('#[path='+json.dumps(str(REPO/'src/backend/compositor_common/solar_system.rs'))+'] mod solar;\n'+r"""
fn main(){let a:Vec<f64>=std::env::args().skip(1).map(|s|s.parse().unwrap()).collect();
let camera=solar::SolarCamera{azimuth:a[0] as f32,elevation:a[1] as f32,distance:a[2] as f32};
let (phases,surface)=solar::scene_phases(a[3]);for x in camera.eye().into_iter().chain(phases).chain([surface]).chain(solar::scene_spin_phases(a[3] * 718.035)){print!("{} ",x);}}
""")
rustc=os.environ.get('RUSTC') or shutil.which('rustc') or str(REPO.parent/'toolchain/bin/rustc')
subprocess.run([rustc,'--edition','2024',str(helper),'-Awarnings','-o',str(ROOT/'solar_values')],check=True)

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

 unif2=gl('glUniform2f',None,[I,F,F]);unif3=gl('glUniform3f',None,[I,F,F,F])
 unif2(loc(prog,b'resolution'),W,H)
 tex=U();gl('glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(tex));gl('glBindTexture',None,[U,U])(0x0DE1,tex)
 param=gl('glTexParameteri',None,[U,U,I])
 for key,val in ((0x2801,0x2601),(0x2800,0x2601),(0x2802,0x2901),(0x2803,0x812F)):param(0x0DE1,key,val)
 earth=REPO/'assets/astronomy/earth-nasa-october.jpg'
 earth_enabled=earth.is_file() and not os.environ.get('SOLAR_NO_EARTH')
 texture=Image.open(earth).convert('RGBA') if earth_enabled else Image.new('RGBA',(1,1),(0,0,0,255));data=C.create_string_buffer(texture.tobytes())
 gl('glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0x0DE1,0,0x1908,texture.width,texture.height,0,0x1908,0x1401,data)
 unif1i(loc(prog,b'earth_map'),0);unif1i(loc(prog,b'earth_enabled'),int(bool(earth_enabled)))

 samples=[]
 scenes= [('default',.4,.65,35,0),('orbit',1.2,.85,35,0),('below',2.,-.6,35,0),('zoom',.4,.65,13,0),('motion',.4,.65,35,20),('north',.4,1.45,35,0),('south',.4,-1.45,35,0),('near',.4,0.,5,0),('far',.4,.65,60,0),('ring-sun',-0.262,0.016,35,0),('phase-before',.4,.65,35,2.39996/.12-.0001),('phase-after',.4,.65,35,2.39996/.12+.0001),('surface-before',.4,.65,35,2*math.pi/.015-.0001),('surface-after',.4,.65,35,2*math.pi/.015+.0001)]
 if not BENCH_ONLY:
  scenes += [('spin-still',.4,.65,35,0),('spin-quarter',.4,.65,35,30),('spin-wrap-before',.4,.65,35,119.9999),('spin-wrap-after',.4,.65,35,120.0001)]
 if os.environ.get('SOLAR_SPIN_VIDEO') and not es:
  scenes += [(f'spin-frame-{i:03d}',.4,.65,35,i*3.) for i in range(40)]
 if BENCH_ONLY: scenes=scenes[:1]
 if os.environ.get('SOLAR_VIDEO') and not es:
  scenes += [(f'frame-{i:03d}',.4+i*.025,.65+.2*math.sin(i/20),35-6*math.sin(i/35),i*.15) for i in range(90)]
 for name,yaw,pitch,distance,t in scenes:
  values=list(map(float,subprocess.check_output([str(ROOT/'solar_values'),*map(str,[yaw,pitch,distance,t])]).split()))
  if name.startswith('spin-'):
   fixed=list(map(float,subprocess.check_output([str(ROOT/'solar_values'),*map(str,[yaw,pitch,distance,0])]).split()))
   values[3:12]=fixed[3:12]
  unif3(loc(prog,b'camera_eye'),*values[:3])
  for i,phase in enumerate(values[3:11]): unif1f(loc(prog,f'orbit_phase[{i}]'.encode()),phase)
  unif1f(loc(prog,b'surface_phase'),values[11])
  for i,phase in enumerate(values[12:20]): unif1f(loc(prog,f'spin_phase[{i}]'.encode()),phase)
  gl('glDrawArrays',None,[U,I,I])(4,0,3)
  buf=(C.c_ubyte*(W*H*4))();gl('glReadPixels',None,[I,I,I,I,U,U,P])(0,0,W,H,0x1908,0x1401,buf)
  assert gl('glGetError',U,[])()==0
  im=Image.frombytes('RGBA',(W,H),bytes(buf)).transpose(Image.Transpose.FLIP_TOP_BOTTOM)
  assert im.getchannel('A').getextrema()==(255,255)
  im.save(ROOT/f'{kind}-{name}.png');samples.append(im)
 for im in samples[1:9]: assert sum(ImageStat.Stat(ImageChops.difference(samples[0],im)).mean[:3])>.1
 for prefix in ([] if BENCH_ONLY else ['phase','surface','spin-wrap']):
  before=Image.open(ROOT/f'{kind}-{prefix}-before.png');after=Image.open(ROOT/f'{kind}-{prefix}-after.png')
  assert sum(ImageStat.Stat(ImageChops.difference(before,after)).mean[:3])<.03
 if not BENCH_ONLY:
  a=Image.open(ROOT/f'{kind}-spin-still.png');b=Image.open(ROOT/f'{kind}-spin-quarter.png')
  assert sum(ImageStat.Stat(ImageChops.difference(a,b)).mean[:3])>.01, 'self-rotation must move surfaces at fixed orbit/camera'
 # A partial repair must not touch pixels outside the compositor scissor.
 gl('glClearColor',None,[F,F,F,F])(1.,0.,1.,1.);gl('glClear',None,[U])(0x4000)
 gl('glEnable',None,[U])(0x0C11);gl('glScissor',None,[I,I,I,I])(W//4,H//4,W//2,H//2)
 gl('glDrawArrays',None,[U,I,I])(4,0,3)
 buf=(C.c_ubyte*(W*H*4))();gl('glReadPixels',None,[I,I,I,I,U,U,P])(0,0,W,H,0x1908,0x1401,buf)
 sc=Image.frombytes('RGBA',(W,H),bytes(buf));assert sc.getpixel((0,0))==(255,0,255,255);assert sc.getpixel((W//2,H//2))!=(255,0,255,255)
 gl('glDisable',None,[U])(0x0C11)
 finish=gl('glFinish',None,[]);finish();timings=[]
 for _ in range(6):
  start=time.perf_counter();gl('glDrawArrays',None,[U,I,I])(4,0,3);finish();timings.append((time.perf_counter()-start)*1000.)
 results[kind]={'renderer':renderer,'version':version,'samples':len(samples),'scissor_preserved':True,'median_full_frame_ms':statistics.median(timings),'resolution':[W,H]}

 # Compile diagnostics from the actual production functions, not Python copies.
 if not BENCH_ONLY:
  def diagnostic_program(fragment):
   shader=compile(fragment,0x8B30);program=gl('glCreateProgram',U,[])()
   attach(program,vs);attach(program,shader);gl('glLinkProgram',None,[U])(program)
   ok=I();gl('glGetProgramiv',None,[U,U,C.POINTER(I)])(program,0x8B82,C.byref(ok));assert ok.value
   gl('glUseProgram',None,[U])(program)
   return program
  probe_source=source[:source.index('void main(){')].replace('uniform float orbit_phase[8];','float orbit_phase[8];')+'''
uniform float orbit_before[8];
uniform float orbit_after[8];
void main(){
 int body=int(gl_FragCoord.x)+1;
 for(int i=0;i<8;i++)orbit_phase[i]=orbit_before[i];
 vec3 p=center(body);
 for(int i=0;i<8;i++)orbit_phase[i]=orbit_after[i];
 vec3 angular=normalize(cross(p,center(body)-p));
 vec3 axis=body_axis(body), x=normalize(cross(axis,vec3(0,0,1))),z=cross(x,axis);
 float expected=cos(obliquity[body-1]*PI/180.);
 bool ok=abs(dot(angular,axis)-expected)<.002;
 ok=ok&&length(surface_point(body,axis)-vec3(0,1,0))<.0001;
 vec3 local=normalize(vec3(.3,.4,.5));
 vec3 turned=rotate_y(local,-spin_phase[body-1]);
 vec3 world=x*turned.x+axis*turned.y+z*turned.z;
 ok=ok&&length(surface_point(body,world)-local)<.0001;
 // Positive body rotation has angular momentum along the chosen physical axis.
 vec3 moved=rotate_y(local,-spin_phase[body-1]-.001);
 vec3 next=x*moved.x+axis*moved.y+z*moved.z;
 ok=ok&&dot(cross(world,next-world),axis)>0.;
 frag_color=ok?vec4(0,1,0,1):vec4(1,0,0,1);
}
'''
  probe=diagnostic_program(probe_source)
  for label,t in [('orbit_before',0),('orbit_after',.1)]:
   v=list(map(float,subprocess.check_output([str(ROOT/'solar_values'),'0','0','35',str(t)]).split()))
   for i,phase in enumerate(v[3:11]):unif1f(loc(probe,f'{label}[{i}]'.encode()),phase)
  for i in range(8):unif1f(loc(probe,f'spin_phase[{i}]'.encode()),.7+i*.4)
  gl('glViewport',None,[I,I,I,I])(0,0,8,1);gl('glDrawArrays',None,[U,I,I])(4,0,3)
  pixels=(C.c_ubyte*32)();gl('glReadPixels',None,[I,I,I,I,U,U,P])(0,0,8,1,0x1908,0x1401,pixels)
  assert list(pixels)==[0,255,0,255]*8, ('actual axis/direction/rigid UV probes',list(pixels))
  # Replace only albedo; changing spin must not rotate world lighting or shadows.
  lighting=diagnostic_program(source.replace('material(body,n)','vec3(.5)'))
  gl('glViewport',None,[I,I,I,I])(0,0,W,H)
  unif2(loc(lighting,b'resolution'),W,H)
  v=list(map(float,subprocess.check_output([str(ROOT/'solar_values'),'.4','.65','35','0']).split()))
  unif3(loc(lighting,b'camera_eye'),*v[:3])
  for i,phase in enumerate(v[3:11]):unif1f(loc(lighting,f'orbit_phase[{i}]'.encode()),phase)
  images=[]
  for phase in (0.,2.):
   for i in range(8):unif1f(loc(lighting,f'spin_phase[{i}]'.encode()),phase)
   gl('glDrawArrays',None,[U,I,I])(4,0,3)
   buf=(C.c_ubyte*(W*H*4))();gl('glReadPixels',None,[I,I,I,I,U,U,P])(0,0,W,H,0x1908,0x1401,buf)
   images.append(bytes(buf))
  assert images[0]==images[1], 'self-rotation must not move world-space illumination'
  results[kind]['rigid_axis_direction_probes']=8
  results[kind]['spin_independent_world_lighting']=True

 current(d,None,None,None)
 egl('eglDestroySurface',U,[P,P])(d,surf);egl('eglDestroyContext',U,[P,P])(d,ctx)
for name in (['default'] if BENCH_ONLY else ['default','orbit','below','zoom','motion','north','south','near','far','ring-sun']):
 a=Image.open(ROOT/f'gl330-{name}.png');b=Image.open(ROOT/f'gles300-{name}.png')
 assert max(v[1] for v in ImageChops.difference(a,b).getextrema())<=2
(ROOT/'metrics.json').write_text(json.dumps(results,indent=2));print(json.dumps(results,indent=2))
