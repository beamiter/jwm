#!/usr/bin/env python3
"""Render observed nebula focus/zoom on actual desktop GL and GLES, headlessly."""

import argparse
import ctypes as C
import json
import math
import os
from pathlib import Path

from PIL import Image, ImageChops, ImageStat

import importlib.util

os.environ.setdefault("LIBGL_ALWAYS_SOFTWARE", "1")
ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--output", type=Path, default=Path("/tmp/jwm-deep-sky-render"))
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=True)
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
    integer("celestial_body", 12)
    integer("cosmic_scientific", 1)
    gl("glUniform2f", None, [I, F, F])(location("resolution"), W, H)
    gl("glUniform2f", None, [I, F, F])(location("canvas_origin"), 0, 0)

    def world(q):
        return [-0.3746065934 * q[1] - 0.9271838546 * q[2], 0.9271838546 * q[1] - 0.3746065934 * q[2], q[0]]

    def render(index, fov, mapped=True, overview=False, back=False):
        geometry = prepare.geometry(targets[index])
        distance = math.sqrt(sum(v * v for v in geometry["position_ly"]))
        direction = [v / distance for v in geometry["position_ly"]]
        matrix = world(geometry["right"]) + world(geometry["up"]) + world(direction)
        if back:
            matrix = [-v if i < 3 or i >= 6 else v for i, v in enumerate(matrix)]
        gl("glUniformMatrix3fv", None, [I, I, C.c_ubyte, C.POINTER(F)])(location("panorama_view"), 1, 0, (F * 9)(*matrix))
        number("panorama_fov", fov)
        integer("cosmic_focus_target", 0 if overview else index + 1)
        integer("deep_sky_mapped", int(mapped))
        gl("glDrawArrays", None, [U, I, I])(4, 0, 3)
        pixels = (C.c_ubyte * (W * H * 4))()
        gl("glReadPixels", None, [I, I, I, I, U, U, P])(0, 0, W, H, 0x1908, 0x1401, pixels)
        assert gl("glGetError", U, [])() == 0
        return Image.frombytes("RGBA", (W, H), bytes(pixels)).transpose(Image.FLIP_TOP_BOTTOM).convert("RGB")

    rows = []
    for index, target in enumerate(targets):
        geometry = prepare.geometry(target)
        distance = target["distance_ly"]
        fov = math.degrees(2 * math.atan(max(geometry["half_size_ly"][1], geometry["half_size_ly"][0] / (W / H)) * 1.12 / distance))
        focus = render(index, fov)
        empty = render(index, fov, mapped=False)
        difference = sum(ImageStat.Stat(ImageChops.difference(focus, empty)).mean) / 3
        assert difference > 8, (api, target["id"], "observed image absent", difference)
        zoomed = render(index, fov / 2)
        assert sum(ImageStat.Stat(ImageChops.difference(focus, zoomed)).mean) / 3 > 3, (api, target["id"], "wheel only enlarged a cached frame")
        assert ImageChops.difference(render(index, fov, back=True), render(index, fov, mapped=False, back=True)).getbbox() is None, "image rendered behind observer"
        focus.save(args.output / f"{api}-{target['id']}.png")
        zoomed.save(args.output / f"{api}-{target['id']}-zoom.png")
        rows.append({"target": target["id"], "fov_degrees": fov, "distance_ly": distance, "mean_observed_difference": difference})
    render(1, 60, overview=True).save(args.output / f"{api}-overview.png")
    metrics[api] = rows
    assert egl("eglMakeCurrent", U, [P, P, P, P])(display, None, None, None)
    egl("eglDestroySurface", U, [P, P])(display, surface)
    egl("eglDestroyContext", U, [P, P])(display, context)

for target in targets:
    a = Image.open(args.output / f"gl330-{target['id']}.png")
    b = Image.open(args.output / f"gles300-{target['id']}.png")
    difference = sum(ImageStat.Stat(ImageChops.difference(a, b)).mean) / 3
    assert difference < 0.1, (target["id"], "GL/GLES disagreement", difference)
(args.output / "metrics.json").write_text(json.dumps(metrics, indent=2) + "\n")
print(f"PASS: 12 observed targets, focus, zoom and back-facing rejection on GL330/GLES300; {args.output}")
