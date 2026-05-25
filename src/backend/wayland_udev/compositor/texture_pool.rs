use super::*;
use std::collections::HashMap;

pub(crate) struct TexturePoolStats {
    pub allocations: usize,
    pub reuses: usize,
    pub deallocations: usize,
}

pub(crate) struct TexturePool {
    available: HashMap<(u32, u32), Vec<u32>>,
    in_use: Vec<u32>,
    stats: TexturePoolStats,
}

impl TexturePool {
    pub fn new() -> Self {
        Self {
            available: HashMap::new(),
            in_use: Vec::new(),
            stats: TexturePoolStats {
                allocations: 0,
                reuses: 0,
                deallocations: 0,
            },
        }
    }

    pub unsafe fn acquire(&mut self, gl: &ffi::Gles2, w: u32, h: u32) -> u32 {
        unsafe {
            if let Some(textures) = self.available.get_mut(&(w, h)) {
                if let Some(tex) = textures.pop() {
                    self.stats.reuses += 1;
                    self.in_use.push(tex);
                    return tex;
                }
            }

            let mut tex: u32 = 0;
            gl.GenTextures(1, &mut tex);
            gl.BindTexture(ffi::TEXTURE_2D, tex);
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_MIN_FILTER,
                ffi::LINEAR as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_MAG_FILTER,
                ffi::LINEAR as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_S,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_T,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexImage2D(
                ffi::TEXTURE_2D,
                0,
                ffi::RGBA as i32,
                w as i32,
                h as i32,
                0,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                std::ptr::null(),
            );
            gl.BindTexture(ffi::TEXTURE_2D, 0);

            self.stats.allocations += 1;
            self.in_use.push(tex);
            tex
        }
    }

    pub fn release(&mut self, tex: u32, w: u32, h: u32) {
        if let Some(pos) = self.in_use.iter().position(|&t| t == tex) {
            self.in_use.swap_remove(pos);
        }
        self.available.entry((w, h)).or_default().push(tex);
    }

    pub unsafe fn clear(&mut self, gl: &ffi::Gles2) {
        unsafe {
            for textures in self.available.values() {
                for &tex in textures {
                    gl.DeleteTextures(1, &tex);
                    self.stats.deallocations += 1;
                }
            }
            self.available.clear();

            for &tex in &self.in_use {
                gl.DeleteTextures(1, &tex);
                self.stats.deallocations += 1;
            }
            self.in_use.clear();
        }
    }

    pub fn stats(&self) -> &TexturePoolStats {
        &self.stats
    }

    pub fn available_count(&self) -> usize {
        self.available.values().map(|v| v.len()).sum()
    }

    pub fn in_use_count(&self) -> usize {
        self.in_use.len()
    }
}
