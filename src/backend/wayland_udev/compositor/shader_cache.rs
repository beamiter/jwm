use smithay::backend::renderer::gles::ffi;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

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
                if let Ok(data) = fs::read(&bin_path) {
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

            let program = self.compile_program(gl, vert_src, frag_src)?;

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

    unsafe fn compile_program(
        &self,
        gl: &ffi::Gles2,
        vert_src: &str,
        frag_src: &str,
    ) -> Result<u32, String> {
        unsafe {
            let vert_shader = self.compile_shader(gl, ffi::VERTEX_SHADER, vert_src)?;
            let frag_shader = self.compile_shader(gl, ffi::FRAGMENT_SHADER, frag_src)?;

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

    unsafe fn compile_shader(
        &self,
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
            if binary_len <= 0 {
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

            if actual_len <= 0 {
                return;
            }
            binary.truncate(actual_len as usize);

            let bin_path = self.cache_dir.join(format!("{}.bin", name));
            let mut data = Vec::with_capacity(4 + binary.len());
            data.extend_from_slice(&format.to_le_bytes());
            data.extend_from_slice(&binary);
            let _ = fs::write(bin_path, data);
        }
    }
}
