use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub(crate) struct WatchedShader {
    path: PathBuf,
    name: String,
    last_mtime: SystemTime,
    last_check: Instant,
}

pub(crate) struct ShaderHotReload {
    enabled: bool,
    shaders: Vec<WatchedShader>,
    check_interval: Duration,
    last_check: Instant,
    reload_count: u32,
}

impl ShaderHotReload {
    pub(crate) fn new() -> Self {
        Self {
            enabled: false,
            shaders: Vec::new(),
            check_interval: Duration::from_millis(500),
            last_check: Instant::now(),
            reload_count: 0,
        }
    }

    /// Enable hot reload by scanning the given directory for .vert and .frag files.
    /// Records each file's path, name (stem), and current mtime.
    pub(crate) fn enable(&mut self, shader_dir: &str) {
        let dir_path = Path::new(shader_dir);
        self.shaders.clear();

        if let Ok(entries) = std::fs::read_dir(dir_path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(ext) = path.extension() {
                    let ext_str = ext.to_string_lossy();
                    if ext_str == "vert" || ext_str == "frag" {
                        let name = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();

                        let mtime = std::fs::metadata(&path)
                            .and_then(|m| m.modified())
                            .unwrap_or(SystemTime::UNIX_EPOCH);

                        self.shaders.push(WatchedShader {
                            path,
                            name,
                            last_mtime: mtime,
                            last_check: Instant::now(),
                        });
                    }
                }
            }
        }

        self.enabled = true;
        self.last_check = Instant::now();
    }

    /// Disable hot reload and stop watching files.
    pub(crate) fn disable(&mut self) {
        self.enabled = false;
    }

    /// Poll watched shader files for modifications.
    /// Returns a list of shader names that have changed since the last check.
    /// Respects the check_interval to avoid excessive filesystem access.
    pub(crate) fn poll(&mut self) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }

        if self.last_check.elapsed() < self.check_interval {
            return Vec::new();
        }
        self.last_check = Instant::now();

        let mut changed = Vec::new();

        for shader in &mut self.shaders {
            let current_mtime = std::fs::metadata(&shader.path)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);

            if current_mtime != shader.last_mtime {
                shader.last_mtime = current_mtime;
                shader.last_check = Instant::now();
                changed.push(shader.name.clone());
            }
        }

        if !changed.is_empty() {
            self.reload_count += changed.len() as u32;
        }

        changed
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn reload_count(&self) -> u32 {
        self.reload_count
    }

    /// Manually add a shader to the watch list.
    pub(crate) fn add_shader(&mut self, name: &str, path: &Path) {
        let mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        self.shaders.push(WatchedShader {
            path: path.to_path_buf(),
            name: name.to_string(),
            last_mtime: mtime,
            last_check: Instant::now(),
        });
    }

    /// Read and return the source contents of a watched shader by name.
    pub(crate) fn shader_source(&self, name: &str) -> Option<String> {
        self.shaders
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| std::fs::read_to_string(&s.path).ok())
    }
}
