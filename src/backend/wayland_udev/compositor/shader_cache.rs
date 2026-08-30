use smithay::backend::renderer::gles::ffi;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Includes the four-byte GL binary-format prefix. Real linked programs are
/// normally far smaller, while this still leaves generous driver headroom.
const MAX_SHADER_CACHE_BYTES: u64 = 16 * 1024 * 1024;
static CACHE_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn read_cache_file(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        // Avoid following a cache-entry symlink or blocking on a FIFO swapped
        // into place between directory creation and startup.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shader cache entry is not a regular file",
        ));
    }
    if metadata.len() > MAX_SHADER_CACHE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shader cache entry exceeds the 16 MiB limit",
        ));
    }

    let mut data = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_SHADER_CACHE_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_SHADER_CACHE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shader cache entry exceeds the 16 MiB limit",
        ));
    }
    Ok(data)
}

fn atomic_write_cache_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    if contents.len() as u64 > MAX_SHADER_CACHE_BYTES {
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

struct CachedProgram {
    program: u32,
    vert_hash: u64,
    frag_hash: u64,
}

pub(crate) struct ShaderCache {
    cache: HashMap<String, CachedProgram>,
    cache_dir: PathBuf,
    enabled: bool,
}

impl ShaderCache {
    pub(crate) fn new(cache_dir: PathBuf) -> Self {
        let enabled = fs::create_dir_all(&cache_dir).is_ok();
        Self {
            cache: HashMap::new(),
            cache_dir,
            enabled,
        }
    }

    pub(crate) unsafe fn get_or_compile(
        &mut self,
        gl: &ffi::Gles2,
        name: &str,
        vert_src: &str,
        frag_src: &str,
    ) -> Result<u32, String> {
        unsafe {
            let vert_hash = Self::hash_source(vert_src);
            let frag_hash = Self::hash_source(frag_src);

            if let Some(cached) = self.cache.get(name) {
                if cached.vert_hash == vert_hash && cached.frag_hash == frag_hash {
                    return Ok(cached.program);
                }
                gl.DeleteProgram(cached.program);
            }

            if self.enabled {
                let bin_path = self.cache_dir.join(format!("{}.bin", name));
                if let Ok(data) = read_cache_file(&bin_path) {
                    if data.len() > 4 {
                        let format = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                        let binary = &data[4..];

                        let program = gl.CreateProgram();
                        gl.ProgramBinary(
                            program,
                            format,
                            binary.as_ptr() as *const _,
                            binary.len() as i32,
                        );

                        let mut link_status = 0i32;
                        gl.GetProgramiv(program, ffi::LINK_STATUS, &mut link_status);

                        if link_status != 0 {
                            self.cache.insert(
                                name.to_string(),
                                CachedProgram {
                                    program,
                                    vert_hash,
                                    frag_hash,
                                },
                            );
                            return Ok(program);
                        }

                        gl.DeleteProgram(program);
                        let _ = fs::remove_file(&bin_path);
                    }
                }
            }

            let program = Self::compile_program(gl, vert_src, frag_src)?;

            if self.enabled {
                self.save_program_binary(gl, program, name);
            }

            self.cache.insert(
                name.to_string(),
                CachedProgram {
                    program,
                    vert_hash,
                    frag_hash,
                },
            );

            Ok(program)
        }
    }

    pub(crate) unsafe fn clear(&mut self, gl: &ffi::Gles2) {
        unsafe {
            for (_, cached) in self.cache.drain() {
                gl.DeleteProgram(cached.program);
            }
        }
    }

    pub(crate) fn count(&self) -> usize {
        self.cache.len()
    }

    pub(crate) unsafe fn invalidate(&mut self, gl: &ffi::Gles2, name: &str) {
        unsafe {
            if let Some(cached) = self.cache.remove(name) {
                gl.DeleteProgram(cached.program);
            }
        }
        if self.enabled {
            let bin_path = self.cache_dir.join(format!("{}.bin", name));
            let _ = fs::remove_file(bin_path);
        }
    }

    fn hash_source(source: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        source.hash(&mut hasher);
        hasher.finish()
    }

    /// Compile and link a program. Associated rather than a method: the cache
    /// contributes nothing here, and passes that own their own GL objects and
    /// share nothing — the recorder's packing and cursor passes — need the
    /// compile without the bookkeeping.
    ///
    /// # Safety
    /// Requires a current GL context.
    pub(crate) unsafe fn compile_program(
        gl: &ffi::Gles2,
        vert_src: &str,
        frag_src: &str,
    ) -> Result<u32, String> {
        unsafe {
            let vert_shader = Self::compile_shader(gl, ffi::VERTEX_SHADER, vert_src)?;
            let frag_shader = Self::compile_shader(gl, ffi::FRAGMENT_SHADER, frag_src)?;

            let program = gl.CreateProgram();
            gl.AttachShader(program, vert_shader);
            gl.AttachShader(program, frag_shader);
            gl.LinkProgram(program);

            let mut link_status = 0i32;
            gl.GetProgramiv(program, ffi::LINK_STATUS, &mut link_status);

            gl.DeleteShader(vert_shader);
            gl.DeleteShader(frag_shader);

            if link_status == 0 {
                let mut log_len = 0i32;
                gl.GetProgramiv(program, ffi::INFO_LOG_LENGTH, &mut log_len);
                let mut log = vec![0u8; log_len as usize];
                gl.GetProgramInfoLog(program, log_len, &mut log_len, log.as_mut_ptr() as *mut _);
                log.truncate(log_len as usize);
                gl.DeleteProgram(program);
                return Err(format!(
                    "Program link failed: {}",
                    String::from_utf8_lossy(&log)
                ));
            }

            Ok(program)
        }
    }

    /// # Safety
    /// Requires a current GL context.
    unsafe fn compile_shader(
        gl: &ffi::Gles2,
        shader_type: u32,
        source: &str,
    ) -> Result<u32, String> {
        unsafe {
            let shader = gl.CreateShader(shader_type);
            let src_ptr = source.as_ptr() as *const i8;
            let src_len = source.len() as i32;
            gl.ShaderSource(shader, 1, &src_ptr, &src_len);
            gl.CompileShader(shader);

            let mut compile_status = 0i32;
            gl.GetShaderiv(shader, ffi::COMPILE_STATUS, &mut compile_status);

            if compile_status == 0 {
                let mut log_len = 0i32;
                gl.GetShaderiv(shader, ffi::INFO_LOG_LENGTH, &mut log_len);
                let mut log = vec![0u8; log_len as usize];
                gl.GetShaderInfoLog(shader, log_len, &mut log_len, log.as_mut_ptr() as *mut _);
                log.truncate(log_len as usize);
                gl.DeleteShader(shader);
                let type_name = if shader_type == ffi::VERTEX_SHADER {
                    "vertex"
                } else {
                    "fragment"
                };
                return Err(format!(
                    "{} shader compile failed: {}",
                    type_name,
                    String::from_utf8_lossy(&log)
                ));
            }

            Ok(shader)
        }
    }

    unsafe fn save_program_binary(&self, gl: &ffi::Gles2, program: u32, name: &str) {
        unsafe {
            let mut binary_len = 0i32;
            gl.GetProgramiv(program, ffi::PROGRAM_BINARY_LENGTH, &mut binary_len);
            if binary_len <= 0 || binary_len as u64 > MAX_SHADER_CACHE_BYTES - 4 {
                return;
            }

            let mut binary = vec![0u8; binary_len as usize];
            let mut actual_len = 0i32;
            let mut format = 0u32;
            gl.GetProgramBinary(
                program,
                binary_len,
                &mut actual_len,
                &mut format,
                binary.as_mut_ptr() as *mut _,
            );

            if actual_len <= 0 || actual_len > binary_len {
                return;
            }
            binary.truncate(actual_len as usize);

            let bin_path = self.cache_dir.join(format!("{}.bin", name));
            let mut data = Vec::with_capacity(4 + binary.len());
            data.extend_from_slice(&format.to_le_bytes());
            data.extend_from_slice(&binary);
            let _ = atomic_write_cache_file(&bin_path, &data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt as _;

    fn temp_cache_dir() -> PathBuf {
        let sequence = CACHE_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "jwm-wayland-shader-cache-{}-{sequence}",
            std::process::id()
        ))
    }

    #[test]
    fn cache_write_is_private_atomic_and_does_not_follow_destination_symlinks() {
        let directory = temp_cache_dir();
        fs::create_dir_all(&directory).unwrap();
        let victim = directory.join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        let cache = directory.join("program.bin");
        std::os::unix::fs::symlink(&victim, &cache).unwrap();

        let binary = b"\x01\x00\x00\x00compiled";
        atomic_write_cache_file(&cache, binary).unwrap();

        assert_eq!(fs::read(&victim).unwrap(), b"unchanged");
        let metadata = fs::symlink_metadata(&cache).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(read_cache_file(&cache).unwrap(), binary);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cache_reads_reject_oversized_and_special_entries() {
        let directory = temp_cache_dir();
        fs::create_dir_all(&directory).unwrap();

        let oversized = directory.join("oversized.bin");
        fs::File::create(&oversized)
            .unwrap()
            .set_len(MAX_SHADER_CACHE_BYTES + 1)
            .unwrap();
        assert_eq!(
            read_cache_file(&oversized).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let fifo = directory.join("fifo.bin");
        let fifo_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        assert_eq!(
            read_cache_file(&fifo).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let target = directory.join("target");
        fs::write(&target, b"compiled").unwrap();
        let symlink = directory.join("symlink.bin");
        std::os::unix::fs::symlink(&target, &symlink).unwrap();
        assert!(read_cache_file(&symlink).is_err());

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cache_writes_reject_oversized_payloads_without_creating_a_file() {
        let directory = temp_cache_dir();
        let cache = directory.join("program.bin");
        let oversized = vec![0_u8; MAX_SHADER_CACHE_BYTES as usize + 1];

        assert_eq!(
            atomic_write_cache_file(&cache, &oversized)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert!(!cache.exists());
        assert!(!directory.exists());
    }
}
