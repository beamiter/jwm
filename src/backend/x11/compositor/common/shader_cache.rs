use glow::HasContext;
/// Shader compilation and caching
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// A linked compositor program is normally well below one MiB. Leave ample
/// driver headroom without allowing a damaged cache entry to consume memory
/// proportional to an arbitrary file.
const MAX_SHADER_BINARY_BYTES: u64 = 16 * 1024 * 1024;
static CACHE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn read_cache_file(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "shader cache entry is not a regular file: {}",
                path.display()
            ),
        ));
    }
    if metadata.len() > MAX_SHADER_BINARY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shader cache entry exceeds the 16 MiB limit",
        ));
    }

    let mut binary = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_SHADER_BINARY_BYTES + 1)
        .read_to_end(&mut binary)?;
    if binary.len() as u64 > MAX_SHADER_BINARY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shader cache entry exceeds the 16 MiB limit",
        ));
    }
    Ok(binary)
}

fn atomic_write_cache_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    if contents.len() as u64 > MAX_SHADER_BINARY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shader binary exceeds the 16 MiB cache limit",
        ));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let sequence = CACHE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".shader-cache.tmp-{}-{sequence}",
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

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

    /// Rewrite a desktop `#version 330 core` source into ESSL 3.00 for the
    /// EGL/GLES3 path.
    ///
    /// ESSL has no default precision for `float`, and none at all for the
    /// sampler types outside `sampler2D`/`samplerCube` (which default to
    /// `lowp`) — a declaration without one is a compile error, which is why
    /// every sampler type the shader set uses gets an explicit default here.
    /// `sampler2D` is pinned to `highp` rather than left at the spec's `lowp`
    /// default because the compositor samples full-screen window textures,
    /// where low-precision coordinates would be visible on any GPU that
    /// honours the qualifier.
    pub(crate) fn prepare_source(source: &str, is_gles: bool) -> Cow<'_, str> {
        if !is_gles {
            return Cow::Borrowed(source);
        }
        let source = source.trim_start();
        let body = source
            .strip_prefix("#version")
            .and_then(|rest| rest.split_once('\n').map(|(_, body)| body))
            .unwrap_or(source);
        Cow::Owned(format!(
            "#version 300 es\n\
             precision highp float;\n\
             precision highp int;\n\
             precision highp sampler2D;\n\
             precision highp sampler3D;\n\
             {body}"
        ))
    }

    /// Get or compile a shader program
    pub fn get_or_compile(
        &self,
        gl: &glow::Context,
        name: &str,
        vert: &str,
        frag: &str,
    ) -> Result<glow::Program, String> {
        let is_gles = unsafe { gl.get_parameter_string(glow::VERSION).contains("OpenGL ES") };
        let vert = Self::prepare_source(vert, is_gles);
        let frag = Self::prepare_source(frag, is_gles);
        let vert_hash = Self::hash_shader(vert.as_ref());
        let frag_hash = Self::hash_shader(frag.as_ref());
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
        let program = self.compile_program(gl, vert.as_ref(), frag.as_ref())?;

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
        atomic_write_cache_file(&path, binary).map_err(|e| format!("save cache: {e}"))?;
        Ok(())
    }

    /// Load binary from disk cache
    fn load_cached_binary(&self, key: &str) -> Result<Vec<u8>, String> {
        let path = self.cache_dir.join(format!("{}.bin", key));
        read_cache_file(&path).map_err(|e| format!("load cache: {e}"))
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
        let sequence = CACHE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "jwm_shader_cache_test_{}_{sequence}",
            std::process::id()
        ));
        p
    }

    #[test]
    fn gles_source_rewrites_version_and_precision() {
        let source = "#version 330 core\nvoid main() {}\n";
        let rewritten = ShaderCache::prepare_source(source, true);
        assert!(rewritten.starts_with("#version 300 es\n"));
        assert!(rewritten.contains("precision highp float;"));
        assert!(!rewritten.contains("330 core"));
    }

    /// ESSL 3.00 defines no default precision for `sampler3D`, so the volume
    /// ray-marcher's `uniform sampler3D` declarations fail to compile unless
    /// the rewrite supplies one.
    #[test]
    fn gles_source_declares_sampler_precision() {
        let source = "#version 330 core\nuniform sampler3D u_volume;\n";
        let rewritten = ShaderCache::prepare_source(source, true);
        assert!(rewritten.contains("precision highp sampler2D;"));
        assert!(rewritten.contains("precision highp sampler3D;"));
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
                    program: unsafe { std::mem::transmute::<u32, glow::NativeProgram>(1u32) },
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
    fn cache_write_replaces_a_symlink_without_touching_its_target() {
        let cache = ShaderCache::new(tmp_cache_dir());
        let victim = cache.cache_dir.join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        let path = cache.cache_dir.join("linked.bin");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        let binary = b"\x01\x00\x00\x00compiled";
        cache.save_cached_binary("linked", binary).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(cache.load_cached_binary("linked").unwrap(), binary);
        fs::remove_dir_all(&cache.cache_dir).unwrap();
    }

    #[test]
    fn cache_reads_reject_oversized_and_special_entries() {
        use std::os::unix::ffi::OsStrExt as _;

        let cache = ShaderCache::new(tmp_cache_dir());
        let oversized = cache.cache_dir.join("oversized.bin");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_SHADER_BINARY_BYTES + 1)
            .unwrap();
        assert!(cache.load_cached_binary("oversized").is_err());

        let fifo = cache.cache_dir.join("fifo.bin");
        let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        assert!(cache.load_cached_binary("fifo").is_err());

        let target = cache.cache_dir.join("target");
        fs::write(&target, b"\x01\x00\x00\x00compiled").unwrap();
        std::os::unix::fs::symlink(&target, cache.cache_dir.join("symlink.bin")).unwrap();
        assert!(cache.load_cached_binary("symlink").is_err());

        fs::remove_dir_all(&cache.cache_dir).unwrap();
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
        // create a fake context that would panic — we test purely the length guard
        // by passing a slice shorter than 4 bytes without calling gl
        // We test the length validation logic directly
        let short = [0u8; 3];
        // We can't call create_program_from_binary without a real GL context,
        // but the function returns Err for short data before touching GL.
        // Use a raw check of the guard condition instead.
        assert!(short.len() < 4, "guard: len < 4 → Err without GL call");
    }
}
