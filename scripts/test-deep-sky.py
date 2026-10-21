#!/usr/bin/env python3
"""Render 3D nebula geometry, palette, orbit and dolly on desktop GL and GLES."""

import argparse
import ctypes as C
import json
import math
import os
import subprocess
import time
from pathlib import Path

from PIL import Image, ImageChops, ImageStat

import importlib.util

os.environ.setdefault("LIBGL_ALWAYS_SOFTWARE", "1")
ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--output", type=Path, default=Path("/tmp/jwm-deep-sky-render"))
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=True)
helper = args.output / "galaxy_shader_id.rs"
helper.write_text(
    "".join('#[path=' + json.dumps(str(ROOT / "src/backend/compositor_common" / (name + ".rs")))
            + '] mod ' + name + ';\n' for name in ("animated_wallpaper", "panorama", "deep_sky"))
    + 'fn main() { let distances: Vec<_> = deep_sky::DEEP_SKY_TARGETS.iter().map(|t| deep_sky::NebulaView::fitted(*t, 4.0/3.0).distance).collect(); '
    + 'println!("[{}, {:?}]", animated_wallpaper::CelestialBody::Galaxy.shader_id() + 1, distances); }\n'
)
subprocess.run(["rustc", "--edition=2024", "-Awarnings", str(helper), "-o", str(args.output / "galaxy_shader_id")], check=True)
GALAXY_SHADER_ID, CAMERA_DISTANCES = json.loads(subprocess.check_output([str(args.output / "galaxy_shader_id")]))
spec = importlib.util.spec_from_file_location("nebula_assets", ROOT / "scripts/prepare-astronomy-nebulae.py")
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)
targets = json.loads((ROOT / "assets/astronomy/deep-sky-catalog.json").read_text())
shaders = ROOT / "src/backend/x11/compositor/shaders"
source = (shaders / "animated_wallpaper.frag").read_text()
for marker, filename in (("COSMIC_SKY", "cosmic_sky.glsl"), ("DEEP_SKY_CATALOG", "deep_sky_catalog.glsl"), ("PLANET_SURFACE", "planet_surface.glsl"), ("SOLAR_OPTICS", "solar_optics.glsl")):
    source = source.replace("// JWM_" + marker + "_INCLUDE", (shaders / filename).read_text())
assert "// JWM_" not in source

E = C.CDLL("libEGL.so.1")
P, I, U, F = C.c_void_p, C.c_int, C.c_uint, C.c_float
E.eglGetProcAddress.argtypes = [C.c_char_p]
E.eglGetProcAddress.restype = P


def egl(name, result, types):
    fn = getattr(E, name)
    fn.restype, fn.argtypes = result, types
    return fn


def gl(name, result, types):
    address = E.eglGetProcAddress(name.encode())
    assert address, name
    return C.CFUNCTYPE(result, *types)(address)


def attrs(*values):
    return (I * len(values))(*values)


platform = C.CFUNCTYPE(P, U, P, C.POINTER(I))(E.eglGetProcAddress(b"eglGetPlatformDisplayEXT"))
display = platform(0x31DD, None, None)
major, minor = I(), I()
assert egl("eglInitialize", U, [P, C.POINTER(I), C.POINTER(I)])(display, C.byref(major), C.byref(minor))
W, H = 800, 600
metrics = {}
for es in (False, True):
    api = "gles300" if es else "gl330"
    assert egl("eglBindAPI", U, [U])(0x30A0 if es else 0x30A2)
    config, count = P(), I()
    assert egl("eglChooseConfig", U, [P, C.POINTER(I), C.POINTER(P), I, C.POINTER(I)])(
        display, attrs(0x3033, 1, 0x3040, 0x40 if es else 8, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3038), C.byref(config), 1, C.byref(count)
    ) and count.value
    context = egl("eglCreateContext", P, [P, P, P, C.POINTER(I)])(
        display, config, None, attrs(0x3098, 3, 0x3038) if es else attrs(0x3098, 3, 0x30FB, 3, 0x30FD, 1, 0x3038)
    )
    surface = egl("eglCreatePbufferSurface", P, [P, P, C.POINTER(I)])(display, config, attrs(0x3057, W, 0x3056, H, 0x3038))
    assert context and surface
    assert egl("eglMakeCurrent", U, [P, P, P, P])(display, surface, surface, context)

    def compile_shader(text, kind):
        if es:
            text = "#version 300 es\nprecision highp float;\nprecision highp int;\nprecision highp sampler2D;\n" + text.split("\n", 1)[1]
        shader = gl("glCreateShader", U, [U])(kind)
        string = C.c_char_p(text.encode())
        gl("glShaderSource", None, [U, I, C.POINTER(C.c_char_p), C.POINTER(I)])(shader, 1, C.byref(string), None)
        gl("glCompileShader", None, [U])(shader)
        good = I()
        gl("glGetShaderiv", None, [U, U, C.POINTER(I)])(shader, 0x8B81, C.byref(good))
        log = C.create_string_buffer(8192)
        gl("glGetShaderInfoLog", None, [U, I, C.POINTER(I), P])(shader, 8192, None, log)
        assert good.value, log.value.decode()
        return shader

    vertex = compile_shader("#version 330 core\nvoid main(){vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);gl_Position=vec4(p*2.0-1.0,0,1);}", 0x8B31)
    fragment = compile_shader(source, 0x8B30)
    program = gl("glCreateProgram", U, [])()
    for shader in (vertex, fragment):
        gl("glAttachShader", None, [U, U])(program, shader)
    gl("glLinkProgram", None, [U])(program)
    good = I()
    gl("glGetProgramiv", None, [U, U, C.POINTER(I)])(program, 0x8B82, C.byref(good))
    assert good.value, "deep sky shader link failed"
    gl("glUseProgram", None, [U])(program)
    vao = U()
    gl("glGenVertexArrays", None, [I, C.POINTER(U)])(1, C.byref(vao))
    gl("glBindVertexArray", None, [U])(vao)
    gl("glViewport", None, [I, I, I, I])(0, 0, W, H)

    def location(name):
        return gl("glGetUniformLocation", I, [U, C.c_char_p])(program, name.encode())

    def integer(name, value):
        gl("glUniform1i", None, [I, I])(location(name), value)

    def number(name, value):
        gl("glUniform1f", None, [I, F])(location(name), value)

    def upload(path, unit):
        image = Image.open(path).convert("RGBA")
        texture = U()
        gl("glGenTextures", None, [I, C.POINTER(U)])(1, C.byref(texture))
        gl("glActiveTexture", None, [U])(0x84C0 + unit)
        gl("glBindTexture", None, [U, U])(0x0DE1, texture)
        data = C.create_string_buffer(image.tobytes())
        gl("glTexImage2D", None, [U, I, I, I, I, I, U, U, P])(0x0DE1, 0, 0x8058, image.width, image.height, 0, 0x1908, 0x1401, data)
        for key, value in ((0x2800, 0x2601), (0x2801, 0x2703), (0x2802, 0x812F), (0x2803, 0x812F)):
            gl("glTexParameteri", None, [U, U, I])(0x0DE1, key, value)
        gl("glGenerateMipmap", None, [U])(0x0DE1)
        return texture

    sky = upload(ROOT / "assets/astronomy/galaxy-nasa-4k.png", 2)
    atlas = upload(ROOT / "assets/astronomy/deep-sky-atlas.jpg", 3)
    integer("sky_map", 2)
    integer("sky_enabled", 1)
    integer("deep_sky_map", 3)
    integer("panorama_enabled", 1)
    integer("celestial_body", GALAXY_SHADER_ID)
    integer("cosmic_scientific", 1)
    gl("glUniform2f", None, [I, F, F])(location("resolution"), W, H)
    gl("glUniform2f", None, [I, F, F])(location("canvas_origin"), 0, 0)

    def world(q):
        return [-0.3746065934 * q[1] - 0.9271838546 * q[2], 0.9271838546 * q[1] - 0.3746065934 * q[2], q[0]]

    timings = []

    def render(index, fov, mapped=True, overview=False, back=False, orbit=0, distance=None, blend=None, markers=True):
        geometry = prepare.geometry(targets[index])
        catalog_distance = math.sqrt(sum(v * v for v in geometry["position_ly"]))
        direction = [v / catalog_distance for v in geometry["position_ly"]]
        matrix = world(geometry["right"]) + world(geometry["up"]) + world(direction)
        if back:
            matrix = [-v if i < 3 or i >= 6 else v for i, v in enumerate(matrix)]
        gl("glUniformMatrix3fv", None, [I, I, C.c_ubyte, C.POINTER(F)])(location("panorama_view"), 1, 0, (F * 9)(*matrix))
        number("panorama_fov", fov)
        integer("cosmic_focus_target", 0 if overview else index + 1)
        integer("deep_sky_mapped", int(mapped))
        number("deep_sky_locator_radius", min(H * 0.42, max(5.5, 5.5 * math.tan(math.radians(30)) / math.tan(math.radians(fov / 2)))) if markers else -1)
        number("nebula_blend", (0 if overview else 1) if blend is None else blend)
        number("nebula_distance", CAMERA_DISTANCES[index] if distance is None else distance)
        c, t = math.cos(orbit), math.sin(orbit)
        gl("glUniformMatrix3fv", None, [I, I, C.c_ubyte, C.POINTER(F)])(location("nebula_orbit"), 1, 0, (F * 9)(c, 0, -t, 0, 1, 0, t, 0, c))
        started = time.perf_counter()
        gl("glDrawArrays", None, [U, I, I])(4, 0, 3)
        pixels = (C.c_ubyte * (W * H * 4))()
        gl("glReadPixels", None, [I, I, I, I, U, U, P])(0, 0, W, H, 0x1908, 0x1401, pixels)
        timings.append((time.perf_counter() - started) * 1000)
        assert gl("glGetError", U, [])() == 0
        return Image.frombytes("RGBA", (W, H), bytes(pixels)).transpose(Image.FLIP_TOP_BOTTOM).convert("RGB")

    def volume_difference(a, b):
        # Exclude the source-credit strip: only actual cloud pixels count.
        difference = ImageChops.difference(a, b).crop((0, 0, W, H - 120))
        return sum(ImageStat.Stat(difference).mean) / 3

    rows = []
    for index, target in enumerate(targets):
        geometry = prepare.geometry(target)
        distance = target["distance_ly"]
        fov = math.degrees(2 * math.atan(max(geometry["half_size_ly"][1], geometry["half_size_ly"][0] / (W / H)) * 1.12 / distance))
        focus = render(index, fov)
        empty = render(index, fov, mapped=False)
        difference = volume_difference(focus, empty)
        assert difference > 2, (api, target["id"], "observed source palette absent", difference)
        zoomed = render(index, fov, distance=CAMERA_DISTANCES[index] / 2)
        assert sum(ImageStat.Stat(ImageChops.difference(focus, zoomed)).mean) / 3 > 3, (api, target["id"], "camera dolly did not change the volume")
        assert ImageChops.difference(render(index, fov, back=True, overview=True), render(index, fov, mapped=False, back=True, overview=True)).getbbox() is None, "image rendered behind observer"
        side = render(index, fov, orbit=math.pi / 2)
        side_empty = render(index, fov, orbit=math.pi / 2, mapped=False)
        assert sum(ImageStat.Stat(ImageChops.difference(focus, side)).mean) / 3 > 2, (api, target["id"], "orbit did not reveal 3D depth")
        assert volume_difference(side, side_empty) > 0.25, (api, target["id"], "volume collapsed side-on")
        side.save(args.output / f"{api}-{target['id']}-side.png")
        focus.save(args.output / f"{api}-{target['id']}.png")
        zoomed.save(args.output / f"{api}-{target['id']}-zoom.png")
        # Geometry must exist independently of a photograph. Comparing with
        # an empty distant view excludes the invariant source-credit strip.
        distant_empty = render(index, fov, mapped=False, distance=30)
        assert volume_difference(empty, distant_empty) > 0.3, (api, target["id"], "density depended on source photograph")
        reverse = render(index, fov, orbit=math.pi)
        full_orbit = render(index, fov, orbit=math.tau)
        assert volume_difference(focus, full_orbit) < 0.02, (api, target["id"], "full orbit did not return to stable geometry")
        assert volume_difference(focus, reverse) > 0.25, (api, target["id"], "front and rear did not reveal distinct geometry")
        reverse.save(args.output / f"{api}-{target['id']}-rear.png")
        if index in (0, 3, 4, 9, 11):
            render(index, fov, orbit=math.pi / 4).save(args.output / f"{api}-{target['id']}-diagonal.png")
        rows.append({"target": target["id"], "fov_degrees": fov, "distance_ly": distance, "mean_source_palette_difference": difference})
    overview = render(1, 60, overview=True)
    overview.save(args.output / f"{api}-overview.png")
    # In a wide view this Hubble field is subpixel. Its centre must still have
    # a visible locator, using the actual scene ID uploaded by the compositor.
    pixel = overview.getpixel((W // 2, H // 2))
    assert pixel[2] > pixel[0] + 10, (api, "missing blue target locator", pixel)
    enlarged = render(1, 2, overview=True)
    unmarked = render(1, 2, overview=True, markers=False)
    ring_difference = ImageChops.difference(enlarged, unmarked)
    radius = 5.5 * math.tan(math.radians(30)) / math.tan(math.radians(1))
    ring_x = W // 2 + int(round(radius))
    assert max(ring_difference.getpixel((ring_x, H // 2))) > 15, (api, "locator did not grow with the lens")
    enlarged.save(args.output / f"{api}-enlarged-locator.png")
    start = render(0, 4, blend=0)
    half = render(0, 4, blend=0.5)
    end = render(0, 4, blend=1)
    assert ImageChops.difference(start, half).getbbox() and ImageChops.difference(half, end).getbbox(), "crossfade endpoint jumped"
    half.save(args.output / f"{api}-transition.png")
    near = render(0, 4, distance=0.7, orbit=2.1)
    assert sum(ImageStat.Stat(near).mean) > 1, "camera inside volume rendered black"
    # Driver JIT may defer compilation to the first draw. Report it separately
    # from steady draws; timings still include synchronous framebuffer readback.
    steady = timings[1:]
    metrics[api] = {"targets": rows, "first_frame_ms": timings[0],
                    "mean_frame_ms": sum(steady) / len(steady),
                    "p95_frame_ms": sorted(steady)[int(len(steady) * 0.95)]}
    assert egl("eglMakeCurrent", U, [P, P, P, P])(display, None, None, None)
    egl("eglDestroySurface", U, [P, P])(display, surface)
    egl("eglDestroyContext", U, [P, P])(display, context)

for target in targets:
    a = Image.open(args.output / f"gl330-{target['id']}.png")
    b = Image.open(args.output / f"gles300-{target['id']}.png")
    difference = sum(ImageStat.Stat(ImageChops.difference(a, b)).mean) / 3
    assert difference < 0.1, (target["id"], "GL/GLES disagreement", difference)
(args.output / "metrics.json").write_text(json.dumps(metrics, indent=2) + "\n")
print(f"PASS: 12 source-independent 3D volumes, full orbit/dolly, growing locators, crossfade and back-facing rejection on GL330/GLES300; {args.output}")
