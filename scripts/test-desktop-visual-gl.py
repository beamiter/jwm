#!/usr/bin/env python3
"""Exercise the production DVR GLSL with surfaceless software EGL (no sockets).

This is a shader/alpha/origin test, not an end-to-end compositor or real-GPU test.
Missing EGL is a nonzero exit, never a silent pass. Only system GL libraries are
used; no desktop, capture, input, or external service is accessed.
"""
import ctypes as C
import os
from pathlib import Path
import re
import unittest

os.environ.setdefault("LIBGL_ALWAYS_SOFTWARE", "1")
os.environ.setdefault("MESA_SHADER_CACHE_DISABLE", "true")
ROOT = Path(__file__).resolve().parents[1]


def function(lib, name, restype, *argtypes):
    f = getattr(lib, name)
    f.restype, f.argtypes = restype, argtypes
    return f


class ShaderTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.egl = C.CDLL("libEGL.so.1")
        cls.gl = C.CDLL("libGL.so.1")
        ptr, integer, uint = C.c_void_p, C.c_int, C.c_uint
        cls.display = function(cls.egl, "eglGetPlatformDisplay", ptr, uint, ptr, ptr)(0x31DD, None, None)
        initialize = function(cls.egl, "eglInitialize", uint, ptr, C.POINTER(integer), C.POINTER(integer))
        major, minor = integer(), integer()
        if not cls.display or not initialize(cls.display, C.byref(major), C.byref(minor)):
            raise RuntimeError("surfaceless EGL initialization unavailable")
        assert function(cls.egl, "eglBindAPI", uint, uint)(0x30A2)
        attrs = (integer * 15)(0x3033, 1, 0x3040, 8, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3021, 8, 0x3038, 0, 0)
        config, count = ptr(), integer()
        choose = function(cls.egl, "eglChooseConfig", uint, ptr, C.POINTER(integer), C.POINTER(ptr), integer, C.POINTER(integer))
        assert choose(cls.display, attrs, C.byref(config), 1, C.byref(count)) and count.value
        ctxattrs = (integer * 7)(0x3098, 3, 0x30FB, 3, 0x30FD, 1, 0x3038)
        cls.context = function(cls.egl, "eglCreateContext", ptr, ptr, ptr, ptr, C.POINTER(integer))(cls.display, config, None, ctxattrs)
        pbattrs = (integer * 5)(0x3057, 4, 0x3056, 4, 0x3038)
        cls.surface = function(cls.egl, "eglCreatePbufferSurface", ptr, ptr, ptr, C.POINTER(integer))(cls.display, config, pbattrs)
        assert cls.context and cls.surface
        cls.make_current = function(cls.egl, "eglMakeCurrent", uint, ptr, ptr, ptr, ptr)
        assert cls.make_current(cls.display, cls.surface, cls.surface, cls.context)
        signatures = {
            "glCreateShader": (uint, uint), "glShaderSource": (None, uint, integer, C.POINTER(C.c_char_p), C.POINTER(integer)),
            "glCompileShader": (None, uint), "glGetShaderiv": (None, uint, uint, C.POINTER(integer)),
            "glGetShaderInfoLog": (None, uint, integer, C.POINTER(integer), C.c_char_p),
            "glCreateProgram": (uint,), "glAttachShader": (None, uint, uint), "glLinkProgram": (None, uint),
            "glGetProgramiv": (None, uint, uint, C.POINTER(integer)), "glDeleteShader": (None, uint),
            "glGenVertexArrays": (None, integer, C.POINTER(uint)), "glBindVertexArray": (None, uint),
            "glGenTextures": (None, integer, C.POINTER(uint)), "glBindTexture": (None, uint, uint),
            "glTexParameteri": (None, uint, uint, integer),
            "glTexImage2D": (None, uint, integer, integer, integer, integer, integer, uint, uint, ptr),
            "glViewport": (None, integer, integer, integer, integer),
            "glClearColor": (None, C.c_float, C.c_float, C.c_float, C.c_float), "glClear": (None, uint),
            "glEnable": (None, uint), "glBlendFunc": (None, uint, uint), "glUseProgram": (None, uint),
            "glGetUniformLocation": (integer, uint, C.c_char_p), "glUniform1i": (None, integer, integer),
            "glUniform4f": (None, integer, C.c_float, C.c_float, C.c_float, C.c_float),
            "glUniformMatrix4fv": (None, integer, integer, C.c_ubyte, C.POINTER(C.c_float)),
            "glDrawArrays": (None, uint, integer, integer),
            "glReadPixels": (None, integer, integer, integer, integer, uint, uint, ptr),
            "glGetString": (C.c_char_p, uint), "glGetError": (uint,),
            "glDeleteTextures": (None, integer, C.POINTER(uint)), "glDeleteVertexArrays": (None, integer, C.POINTER(uint)),
            "glDeleteProgram": (None, uint),
        }
        for name, sig in signatures.items():
            setattr(cls, name, staticmethod(function(cls.gl, name, *sig)))
        print("Software EGL renderer:", cls.glGetString(0x1F01).decode(), flush=True)
        def source(path, name):
            text = (ROOT / path).read_text()
            return re.search(rf'const {name}: &str = r#"(.*?)"#;', text, re.S).group(1).encode()
        vs = source("src/backend/x11/compositor/common/shaders.rs", "VERTEX_SHADER")
        fs = source("src/backend/x11/compositor/desktop_visual.rs", "FRAGMENT_SHADER")
        shaders = []
        for kind, code in [(0x8B31, vs), (0x8B30, fs)]:
            shader = cls.glCreateShader(kind)
            c = C.c_char_p(code)
            cls.glShaderSource(shader, 1, C.byref(c), None)
            cls.glCompileShader(shader)
            ok = integer()
            cls.glGetShaderiv(shader, 0x8B81, C.byref(ok))
            if not ok.value:
                log = C.create_string_buffer(8192)
                cls.glGetShaderInfoLog(shader, len(log), None, log)
                raise AssertionError(log.value.decode())
            shaders.append(shader)
        cls.program = cls.glCreateProgram()
        for shader in shaders: cls.glAttachShader(cls.program, shader)
        cls.glLinkProgram(cls.program)
        ok = integer(); cls.glGetProgramiv(cls.program, 0x8B82, C.byref(ok)); assert ok.value
        for shader in shaders: cls.glDeleteShader(shader)
        cls.vao, cls.texture = uint(), uint()
        cls.glGenVertexArrays(1, C.byref(cls.vao)); cls.glGenTextures(1, C.byref(cls.texture))

    @classmethod
    def tearDownClass(cls):
        cls.glDeleteTextures(1, C.byref(cls.texture))
        cls.glDeleteVertexArrays(1, C.byref(cls.vao))
        cls.glDeleteProgram(cls.program)
        cls.make_current(cls.display, None, None, None)
        function(cls.egl, "eglDestroySurface", C.c_uint, C.c_void_p, C.c_void_p)(cls.display, cls.surface)
        function(cls.egl, "eglDestroyContext", C.c_uint, C.c_void_p, C.c_void_p)(cls.display, cls.context)
        function(cls.egl, "eglTerminate", C.c_uint, C.c_void_p)(cls.display)

    def render(self, pixels, width, height, alpha, canvas=None, background=(0, 0, 0, 1)):
        w, h = canvas or (width, height)
        self.glViewport(0, 0, w, h)
        self.glClearColor(*background); self.glClear(0x4000)
        self.glEnable(0x0BE2); self.glBlendFunc(1, 0x0303)
        self.glBindTexture(0x0DE1, self.texture.value)
        for p in [0x2800, 0x2801]: self.glTexParameteri(0x0DE1, p, 0x2601)
        for p in [0x2802, 0x2803]: self.glTexParameteri(0x0DE1, p, 0x812F)
        data = (C.c_ubyte * len(pixels))(*pixels)
        self.glTexImage2D(0x0DE1, 0, 0x8058, width, height, 0, 0x1908, 0x1401, data)
        self.glUseProgram(self.program)
        loc = lambda s: self.glGetUniformLocation(self.program, s.encode())
        self.glUniform1i(loc("u_texture"), 0); self.glUniform1i(loc("u_alpha"), alpha)
        self.glUniform4f(loc("u_rect"), 0, 0, w, h)
        matrix = (C.c_float * 16)(2/w,0,0,0, 0,-2/h,0,0, 0,0,1,0, -1,1,0,1)
        self.glUniformMatrix4fv(loc("u_projection"), 1, 0, matrix)
        self.glBindVertexArray(self.vao.value); self.glDrawArrays(5, 0, 4)
        result = (C.c_ubyte * (w*h*4))()
        self.glReadPixels(0,0,w,h,0x1908,0x1401,result)
        self.assertEqual(self.glGetError(), 0)
        rows = [list(result[y*w*4:(y+1)*w*4]) for y in reversed(range(h))]
        return sum(rows, [])

    def test_top_left_origin_opaque_and_canvas_resize(self):
        p = [255,0,0,0, 0,255,0,20, 0,0,255,100, 255,255,255,128]
        expected = [255,0,0,255, 0,255,0,255, 0,0,255,255, 255,255,255,255]
        self.assertEqual(self.render(p,2,2,1), expected)
        self.assertEqual(self.render([17,33,99,0],1,1,1,(4,3)), [17,33,99,255]*12)

    def test_straight_and_premultiplied_source_over(self):
        straight = self.render([255,0,0,128],1,1,2,background=(0,0,1,1))
        premult = self.render([128,0,0,128],1,1,3,background=(0,0,1,1))
        self.assertEqual(straight,premult)
        self.assertEqual(straight,[128,0,127,255])

    def test_transparent_rgb_does_not_bleed_when_scaled(self):
        self.assertEqual(self.render([255,0,0,0, 0,0,255,255],2,1,2,(1,1)), [0,0,128,255])

    def test_invalid_premultiplication_is_clamped(self):
        self.assertEqual(self.render([255,255,255,0],1,1,3), [0,0,0,255])


if __name__ == "__main__":
    unittest.main(verbosity=2)
