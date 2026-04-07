/// Shader compilation and caching
use std::collections::HashMap;
use std::path::PathBuf;
use std::fs;
use std::sync::Arc;
use std::sync::Mutex;
use glow::HasContext;

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
                            programs.insert(cache_key.clone(), CachedProgram {
                                program,
                                vert_hash,
                                frag_hash,
                            });
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
            programs.insert(cache_key, CachedProgram {
                program,
                vert_hash,
                frag_hash,
            });
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
            let program = gl.create_program().map_err(|e| format!("create_program: {e}"))?;

            // Compile vertex shader
            let vert_shader = gl.create_shader(glow::VERTEX_SHADER)
                .map_err(|e| format!("create_vertex_shader: {e}"))?;
            gl.shader_source(vert_shader, vert);
            gl.compile_shader(vert_shader);

            if !gl.get_shader_compile_status(vert_shader) {
                let info = gl.get_shader_info_log(vert_shader);
                gl.delete_shader(vert_shader);
                return Err(format!("vertex shader compile error: {}", info));
            }

            // Compile fragment shader
            let frag_shader = gl.create_shader(glow::FRAGMENT_SHADER)
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
    fn get_program_binary(&self, _gl: &glow::Context, _program: glow::Program) -> Result<Vec<u8>, String> {
        // Note: This requires GL_ARB_get_program_binary extension
        // For now, we just return empty to indicate binary caching is not available
        Ok(vec![])
    }

    /// Create program from binary
    fn create_program_from_binary(
        &self,
        _gl: &glow::Context,
        _binary: &[u8],
    ) -> Result<glow::Program, String> {
        // This would need GL_ARB_get_program_binary support
        // For now, just fail and fall back to source compilation
        Err("binary loading not available".to_string())
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
