/// Texture pool for reusing GPU texture objects
use std::collections::HashMap;
use std::sync::{Mutex, Arc};
use glow::HasContext;

/// Pooled texture for reuse
pub struct PooledTexture {
    texture: glow::Texture,
    width: u32,
    height: u32,
}

/// Manages a pool of reusable textures to reduce allocation overhead
pub struct TexturePool {
    available: Arc<Mutex<HashMap<(u32, u32), Vec<glow::Texture>>>>,
    in_use: Arc<Mutex<Vec<glow::Texture>>>,
    stats: TexturePoolStats,
}

#[derive(Clone, Default, Debug)]
pub struct TexturePoolStats {
    pub allocations: usize,
    pub reuses: usize,
    pub deallocations: usize,
}

impl TexturePool {
    pub fn new() -> Self {
        Self {
            available: Arc::new(Mutex::new(HashMap::new())),
            in_use: Arc::new(Mutex::new(Vec::new())),
            stats: TexturePoolStats::default(),
        }
    }

    /// Acquire or create a texture of the given size
    pub fn acquire(
        &mut self,
        gl: &glow::Context,
        width: u32,
        height: u32,
    ) -> Result<glow::Texture, String> {
        let key = (width, height);

        // Try to reuse from pool
        if let Ok(mut available) = self.available.lock() {
            if let Some(textures) = available.get_mut(&key) {
                if let Some(tex) = textures.pop() {
                    self.stats.reuses += 1;
                    if let Ok(mut in_use) = self.in_use.lock() {
                        in_use.push(tex);
                    }
                    return Ok(tex);
                }
            }
        }

        // Create new texture
        unsafe {
            let tex = gl.create_texture().map_err(|e| format!("Failed to create texture: {e}"))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            gl.bind_texture(glow::TEXTURE_2D, None);

            self.stats.allocations += 1;
            if let Ok(mut in_use) = self.in_use.lock() {
                in_use.push(tex);
            }
            Ok(tex)
        }
    }

    /// Release a texture back to the pool for reuse
    pub fn release(&mut self, _gl: &glow::Context, tex: glow::Texture, width: u32, height: u32) {
        let key = (width, height);

        if let Ok(mut in_use) = self.in_use.lock() {
            in_use.retain(|&t| t != tex);
        }

        if let Ok(mut available) = self.available.lock() {
            available.entry(key).or_insert_with(Vec::new).push(tex);
        }
    }

    /// Clear all pooled textures
    pub fn clear(&mut self, gl: &glow::Context) {
        unsafe {
            if let Ok(available) = self.available.lock() {
                for textures in available.values() {
                    for &tex in textures {
                        gl.delete_texture(tex);
                    }
                }
            }
            if let Ok(in_use) = self.in_use.lock() {
                for &tex in in_use.iter() {
                    gl.delete_texture(tex);
                }
            }
        }

        self.stats.deallocations += 1;
        if let Ok(mut available) = self.available.lock() {
            available.clear();
        }
        if let Ok(mut in_use) = self.in_use.lock() {
            in_use.clear();
        }
    }

    /// Get pool statistics
    pub fn stats(&self) -> &TexturePoolStats {
        &self.stats
    }

    /// Get number of available textures
    pub fn available_count(&self) -> usize {
        self.available.lock().ok().map(|a| {
            a.values().map(|v| v.len()).sum()
        }).unwrap_or(0)
    }

    /// Get number of in-use textures
    pub fn in_use_count(&self) -> usize {
        self.in_use.lock().ok().map(|u| u.len()).unwrap_or(0)
    }
}

impl Default for TexturePool {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for TexturePool {
    fn clone(&self) -> Self {
        Self {
            available: self.available.clone(),
            in_use: self.in_use.clone(),
            stats: self.stats.clone(),
        }
    }
}
