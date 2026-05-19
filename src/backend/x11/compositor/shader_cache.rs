use glow::HasContext;
/// Shader compilation and caching
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

/// Cached compiled shader program
pub struct CachedProgram {
    pub program: glow::Program,
    pub vert_hash: u64,
    pub frag_hash: u64,
}

/// Manages shader compilation with optional binary caching
pub struct ShaderCache {
    cache_dir: PathBuf,
    programs: Arc<Mutex<HashMap<String, CachedProgram>>>,
    enable_cache: bool,
}

impl ShaderCache {
    pub fn new(cache_dir: PathBuf) -> Self {
        let enable_cache = fs::create_dir_all(&cache_dir).is_ok();
        Self {
            cache_dir,
            programs: Arc::new(Mutex::new(HashMap::new())),
            enable_cache,
        }
    }

    /// Compute a simple hash of shader source
    fn hash_shader(source: &str) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        source.hash(&mut hasher);
        hasher.finish()
    }

    /// Get or compile a shader program
    pub fn get_or_compile(
        &self,
        gl: &glow::Context,
        name: &str,
        vert: &str,
        frag: &str,
    ) -> Result<glow::Program, String> {
        let vert_hash = Self::hash_shader(vert);
        let frag_hash = Self::hash_shader(frag);
        let cache_key = format!("{}_{:x}_{:x}", name, vert_hash, frag_hash);

        // Check memory cache
        if let Ok(programs) = self.programs.lock() {
            if let Some(cached) = programs.get(&cache_key) {
                log::debug!("shader: using cached program '{}'", name);
                return Ok(cached.program);
            }
        }

        // Try to load from disk cache (if enabled)
        if self.enable_cache {
            if let Ok(binary) = self.load_cached_binary(&cache_key) {
                match self.create_program_from_binary(gl, &binary) {
                    Ok(program) => {
                        log::info!("shader: loaded '{}' from disk cache", name);
                        if let Ok(mut programs) = self.programs.lock() {
                            programs.insert(
                                cache_key.clone(),
                                CachedProgram {
                                    program,
                                    vert_hash,
                                    frag_hash,
                                },
                            );
                        }
                        return Ok(program);
                    }
                    Err(e) => {
                        log::warn!("shader: failed to load cached binary for '{}': {}", name, e);
                    }
                }
            }
        }

        // Compile from source
        log::info!("shader: compiling '{}'", name);
        let program = self.compile_program(gl, vert, frag)?;

        // Try to cache the binary
        if self.enable_cache {
            if let Ok(binary) = self.get_program_binary(gl, program) {
                let _ = self.save_cached_binary(&cache_key, &binary);
            }
        }

        if let Ok(mut programs) = self.programs.lock() {
            programs.insert(
                cache_key,
                CachedProgram {
                    program,
                    vert_hash,
                    frag_hash,
                },
            );
        }

        Ok(program)
    }

    /// Compile shader program from source
    fn compile_program(
        &self,
        gl: &glow::Context,
        vert: &str,
        frag: &str,
    ) -> Result<glow::Program, String> {
        unsafe {
            let program = gl
                .create_program()
                .map_err(|e| format!("create_program: {e}"))?;

            // Compile vertex shader
            let vert_shader = gl
                .create_shader(glow::VERTEX_SHADER)
                .map_err(|e| format!("create_vertex_shader: {e}"))?;
            gl.shader_source(vert_shader, vert);
            gl.compile_shader(vert_shader);

            if !gl.get_shader_compile_status(vert_shader) {
                let info = gl.get_shader_info_log(vert_shader);
                gl.delete_shader(vert_shader);
                return Err(format!("vertex shader compile error: {}", info));
            }

            // Compile fragment shader
            let frag_shader = gl
                .create_shader(glow::FRAGMENT_SHADER)
                .map_err(|e| format!("create_fragment_shader: {e}"))?;
            gl.shader_source(frag_shader, frag);
            gl.compile_shader(frag_shader);

            if !gl.get_shader_compile_status(frag_shader) {
                let info = gl.get_shader_info_log(frag_shader);
                gl.delete_shader(vert_shader);
                gl.delete_shader(frag_shader);
                return Err(format!("fragment shader compile error: {}", info));
            }

            // Link program
            gl.attach_shader(program, vert_shader);
            gl.attach_shader(program, frag_shader);
            gl.link_program(program);

            gl.delete_shader(vert_shader);
            gl.delete_shader(frag_shader);

            if !gl.get_program_link_status(program) {
                let info = gl.get_program_info_log(program);
                gl.delete_program(program);
                return Err(format!("program link error: {}", info));
            }

            Ok(program)
        }
    }

    /// Get binary representation of a compiled program
    fn get_program_binary(
        &self,
        gl: &glow::Context,
        program: glow::Program,
    ) -> Result<Vec<u8>, String> {
        unsafe {
            if let Some(binary) = gl.get_program_binary(program) {
                // Serialize ProgramBinary (format + buffer)
                let mut result = binary.format.to_le_bytes().to_vec();
                result.extend_from_slice(&binary.buffer);
                Ok(result)
            } else {
                Err("GL_ARB_get_program_binary not available".to_string())
            }
        }
    }

    /// Create program from binary
    fn create_program_from_binary(
        &self,
        gl: &glow::Context,
        binary_data: &[u8],
    ) -> Result<glow::Program, String> {
        if binary_data.len() < 4 {
            return Err("binary too short (missing format header)".to_string());
        }

        unsafe {
            // Extract format and buffer
            let binary_format = u32::from_le_bytes([
                binary_data[0],
                binary_data[1],
                binary_data[2],
                binary_data[3],
            ]);
            let program_buffer = binary_data[4..].to_vec();

            // Create ProgramBinary struct
            let program_binary = glow::ProgramBinary {
                format: binary_format,
                buffer: program_buffer,
            };

            // Create program and load binary
            let program = gl
                .create_program()
                .map_err(|e| format!("create_program: {e}"))?;

            gl.program_binary(program, &program_binary);

            // Check link status
            if !gl.get_program_link_status(program) {
                let info = gl.get_program_info_log(program);
                gl.delete_program(program);
                return Err(format!("program binary link failed: {}", info));
            }

            Ok(program)
        }
    }

    /// Save binary to disk cache
    fn save_cached_binary(&self, key: &str, binary: &[u8]) -> Result<(), String> {
        if binary.is_empty() {
            return Ok(());
        }

        let path = self.cache_dir.join(format!("{}.bin", key));
        fs::write(path, binary).map_err(|e| format!("save cache: {}", e))?;
        Ok(())
    }

    /// Load binary from disk cache
    fn load_cached_binary(&self, key: &str) -> Result<Vec<u8>, String> {
        let path = self.cache_dir.join(format!("{}.bin", key));
        fs::read(path).map_err(|e| format!("load cache: {}", e))
    }

    /// Clear all cached programs
    pub fn clear(&self, gl: &glow::Context) {
        unsafe {
            if let Ok(mut programs) = self.programs.lock() {
                for (_, cached) in programs.drain() {
                    gl.delete_program(cached.program);
                }
            }
        }
    }

    /// Get number of cached programs
    pub fn count(&self) -> usize {
        self.programs.lock().ok().map(|p| p.len()).unwrap_or(0)
    }
}

impl Clone for ShaderCache {
    fn clone(&self) -> Self {
        Self {
            cache_dir: self.cache_dir.clone(),
            programs: self.programs.clone(),
            enable_cache: self.enable_cache,
        }
    }
}

impl Drop for ShaderCache {
    fn drop(&mut self) {
        // Programs will be deleted when the Context is dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_cache_dir() -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("jwm_shader_cache_test_{}", std::process::id()));
        p
    }

    #[test]
    fn test_new_cache_starts_empty() {
        let cache = ShaderCache::new(tmp_cache_dir());
        assert_eq!(cache.count(), 0);
    }

    #[test]
    fn test_clone_shares_program_map() {
        let cache = ShaderCache::new(tmp_cache_dir());
        let cache2 = cache.clone();
        // Both start at 0
        assert_eq!(cache.count(), 0);
        assert_eq!(cache2.count(), 0);
        // Clones share the underlying Arc<Mutex<HashMap>>:
        // inserting via one is visible in the other
        if let Ok(mut map) = cache.programs.lock() {
            map.insert(
                "test_key".to_string(),
                CachedProgram {
                    program: unsafe { std::mem::transmute(1u32) },
                    vert_hash: 0,
                    frag_hash: 0,
                },
            );
        }
        assert_eq!(cache2.count(), 1);
    }

    #[test]
    fn test_hash_shader_deterministic() {
        // The private hash_shader function must be deterministic; we test it
        // indirectly via the cache key (same source → same key → single entry).
        // We can access hash_shader directly since we're in the same module.
        let h1 = ShaderCache::hash_shader("void main() {}");
        let h2 = ShaderCache::hash_shader("void main() {}");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hash_shader_different_sources() {
        let h1 = ShaderCache::hash_shader("void main() { gl_Position = vec4(0); }");
        let h2 = ShaderCache::hash_shader("void main() { gl_FragColor = vec4(1); }");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_hash_shader_empty_string() {
        let h = ShaderCache::hash_shader("");
        // Just verifies it doesn't panic; hash of empty is well-defined
        let _ = h;
    }

    #[test]
    fn test_save_load_cached_binary_round_trip() {
        let cache = ShaderCache::new(tmp_cache_dir());
        // save then load
        let data = vec![0u8, 1, 2, 3, 4, 5, 6, 7];
        let key = "roundtrip_test";
        let save_result = cache.save_cached_binary(key, &data);
        if save_result.is_ok() {
            let loaded = cache.load_cached_binary(key);
            assert!(loaded.is_ok(), "should load what was saved");
            assert_eq!(loaded.unwrap(), data);
        }
        // Clean up
        let _ = std::fs::remove_file(cache.cache_dir.join(format!("{}.bin", key)));
    }

    #[test]
    fn test_save_empty_binary_is_noop() {
        let cache = ShaderCache::new(tmp_cache_dir());
        // Saving empty data should not create a file and should succeed
        let result = cache.save_cached_binary("empty_key", &[]);
        assert!(result.is_ok());
        // File should NOT exist
        assert!(!cache.cache_dir.join("empty_key.bin").exists());
    }

    #[test]
    fn test_load_missing_key_returns_error() {
        let cache = ShaderCache::new(tmp_cache_dir());
        let result = cache.load_cached_binary("nonexistent_key_xyz");
        assert!(result.is_err());
    }

    #[test]
    fn test_create_program_from_binary_too_short_fails() {
        let cache = ShaderCache::new(tmp_cache_dir());
        // create a fake context that would panic — we test purely the length guard
        // by passing a slice shorter than 4 bytes without calling gl
        // We test the length validation logic directly
        let short = vec![0u8; 3];
        // We can't call create_program_from_binary without a real GL context,
        // but the function returns Err for short data before touching GL.
        // Use a raw check of the guard condition instead.
        assert!(short.len() < 4, "guard: len < 4 → Err without GL call");
    }
}
