#!/usr/bin/env python3
"""Compile/link/render actual wallpaper on surfaceless Mesa desktop and GLES.
No window system, installed dependencies only. Outputs PNGs and JSON metrics.
"""
import ctypes as C, os, json, hashlib, subprocess, datetime, tempfile, shutil
from pathlib import Path
from PIL import Image, ImageDraw, ImageChops, ImageStat
os.environ.setdefault('LIBGL_ALWAYS_SOFTWARE','1')
os.environ.setdefault('MESA_SHADER_CACHE_DISABLE','true')
REPO=Path(__file__).resolve().parents[1]
ROOT=Path(os.environ.get('ASTRONOMY_TEST_OUTPUT', tempfile.mkdtemp(prefix='jwm-astronomy-')))
ROOT.mkdir(parents=True, exist_ok=True)
SOURCE=REPO/'src/backend/x11/compositor/shaders/animated_wallpaper.frag'
W,H=int(os.environ.get("PREVIEW_WIDTH",960)),int(os.environ.get("PREVIEW_HEIGHT",540))
helper=ROOT/'astronomy_values.rs'
helper.write_text('#[path = '+json.dumps(str(REPO/'src/backend/compositor_common/animated_wallpaper.rs'))+'] mod astronomy;\nfn main() { let a: Vec<String> = std::env::args().collect(); let t:f64=a[1].parse().unwrap(); let o:i32=a[2].parse().unwrap(); let v=astronomy::solar_direction(t); println!("{} {} {} {}", v[0], v[1], v[2], astronomy::viewing_longitude(t,o)); }')
subprocess.run([shutil.which('rustc') or 'rustc', str(helper), '-Awarnings', '-o', str(ROOT/'astronomy_values')],check=True)
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
 gl('glUniform2f',None,[I,F,F])(loc(prog,b'resolution'),W,H)
 tex=U();gl('glGenTextures',None,[I,C.POINTER(U)])(1,C.byref(tex));gl('glBindTexture',None,[U,U])(0x0DE1,tex)
 param=gl('glTexParameteri',None,[U,U,I])
 for key,val in ((0x2801,0x2601),(0x2800,0x2601),(0x2802,0x2901),(0x2803,0x812F)):param(0x0DE1,key,val)
 texture=Image.open(Path(os.environ.get('ASTRONOMY_TEST_TEXTURE', str(REPO/'assets/astronomy/earth-nasa-january.jpg')))).convert('RGBA');raw=texture.tobytes();data=C.create_string_buffer(raw)
 gl('glTexImage2D',None,[U,I,I,I,I,I,U,U,P])(0x0DE1,0,0x1908,texture.width,texture.height,0,0x1908,0x1401,data)
 unif1i(loc(prog,b'earth_map'),0)
 images=[];metrics=[]
 cases=[(h,z) for z in (0,8) for h in (0,6,12,18)]+[(12+1/60,0),(12+1,0),(6+1/54000,0),(6+1/3600,0)]
 for hour,zone in cases:
  values=list(map(float,subprocess.check_output([str(ROOT/'astronomy_values'),str(BASE+hour*3600),str(zone*3600)]).split()))
  gl('glUniform3f',None,[I,F,F,F])(loc(prog,b'sun_direction'),*values[:3]);unif1f(loc(prog,b'view_longitude'),values[3])
  gl('glDrawArrays',None,[U,I,I])(4,0,3)
  buf=(C.c_ubyte*(W*H*4))();gl('glReadPixels',None,[I,I,I,I,U,U,P])(0,0,W,H,0x1908,0x1401,buf)
  assert gl('glGetError',U,[])()==0
  im=Image.frombytes('RGBA',(W,H),bytes(buf)).transpose(Image.Transpose.FLIP_TOP_BOTTOM)
  assert im.getchannel('A').getextrema()==(255,255)
  name=f'{kind}-earth-utc{hour:.8f}-zone{zone:+d}.png';im.save(ROOT/name)
  metrics.append({'hour':hour,'zone':zone,'sun_direction':values[:3],'view_longitude':values[3],'file':name,'sha256':hashlib.sha256(im.tobytes()).hexdigest(),'rgb_extrema':im.convert('RGB').getextrema()})
  images.append(im)
 # Fixed sky must never blink with clock/timezone updates. Corners are outside Earth.
 reference=images[0].crop((0,0,W//8,H//8)).convert('RGB')
 assert all(ImageChops.difference(reference,im.crop((0,0,W//8,H//8)).convert('RGB')).getbbox() is None for im in images)
 assert max(reference.convert('RGB').getextrema()[0]) > 40, 'missing stars'
 assert len({m['sha256'] for m in metrics})==len(cases)
 sheet=Image.new('RGB',(W*4,(H+28)*2),(16,19,25)); draw=ImageDraw.Draw(sheet)
 for i,im in enumerate(images[:8]):
  x=(i%4)*W;y=(i//4)*(H+28);sheet.paste(im,(x,y+28));draw.text((x+10,y+8),f'2026-10-15 UTC {cases[i][0]:02d}:00 | timezone UTC{cases[i][1]:+d} | {kind}',fill='white')
 sheet.save(ROOT/f'{kind}-earth-contact-sheet.png')
 diffs=[]
 for baseline,index,seconds in ((2,8,60),(2,9,3600),(1,10,1/15),(1,11,1)):
  delta=ImageChops.difference(images[baseline].convert('RGB'),images[index].convert('RGB'))
  diffs.append({'seconds':seconds,'mean_abs_rgb':sum(ImageStat.Stat(delta).mean)/3,'max_abs_channel':max(x[1] for x in delta.getextrema())})
 results[kind]={'version':version,'renderer':renderer,'compile_link':'PASS','opaque_all_frames':True,'distinct_frames':len(cases),'motion':diffs,'metrics':metrics}
 # Full root framebuffer test, two 2560x1440 outputs joined at x=2560.
 wide=surface(d,cfg,attrs(0x3057,5120,0x3056,1440,0x3038));assert wide
 assert current(d,wide,wide,ctx)
 gl('glViewport',None,[I,I,I,I])(0,0,5120,1440)
 gl('glUniform2f',None,[I,F,F])(loc(prog,b'resolution'),5120,1440)
 values=list(map(float,subprocess.check_output([str(ROOT/'astronomy_values'),str(PREVIEW_INSTANT),str(8*3600)]).split()))
 gl('glUniform3f',None,[I,F,F,F])(loc(prog,b'sun_direction'),*values[:3]);unif1f(loc(prog,b'view_longitude'),values[3])
 gl('glDrawArrays',None,[U,I,I])(4,0,3)
 buf=(C.c_ubyte*(5120*1440*4))();gl('glReadPixels',None,[I,I,I,I,U,U,P])(0,0,5120,1440,0x1908,0x1401,buf)
 assert gl('glGetError',U,[])()==0
 im=Image.frombytes('RGBA',(5120,1440),bytes(buf)).transpose(Image.Transpose.FLIP_TOP_BOTTOM)
 assert im.getchannel('A').getextrema()==(255,255)
 im.save(ROOT/f'{kind}-earth-dualoutput-5120x1440.png')
 # Physical labels can be (-2560,0)+(0,0); root framebuffer positions remain 0..5120.
 results[kind]['dualoutput']={'utc_instant':datetime.datetime.fromtimestamp(PREVIEW_INSTANT,datetime.timezone.utc).isoformat(),'view_offset_hours':8,'size':[5120,1440],'earth_center':[2560,720],'earth_radius':604.8,'opaque':True,'expected':'One globe spans output boundary; no per-monitor duplication.'}
 current(d,None,None,None)
 egl('eglDestroySurface',U,[P,P])(d,wide)
 current(d,None,None,None)
 egl('eglDestroySurface',U,[P,P])(d,surf);egl('eglDestroyContext',U,[P,P])(d,ctx)
results['source_sha256']=hashlib.sha256(source.encode()).hexdigest()

# Both real APIs must agree within one byte of raster/float rounding.
for a,b in zip(results['gl330']['metrics'],results['gles300']['metrics']):
 delta=ImageChops.difference(Image.open(ROOT/a['file']).convert('RGB'),Image.open(ROOT/b['file']).convert('RGB'))
 assert max(v[1] for v in delta.getextrema()) <= 1, (a['file'],delta.getextrema())
print('PASS: GLSL/GLES, opaque frames, stationary sky, UTC motion; output:',ROOT)

(ROOT/'earth-validation.json').write_text(json.dumps(results,indent=2)+'\n')
print(json.dumps(results,indent=2))
