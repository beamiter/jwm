//! Embedded UI kept in a private, exclusively-created file for mpv's Lua loader.
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Script(PathBuf);
impl Script {
    pub fn create() -> std::io::Result<Self> {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        for _ in 0..64 {
            let path = std::env::temp_dir().join(format!(
                "jwm-player-ui-{}-{}.lua",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path);
            match file {
                Ok(mut file) => {
                    let script = Self(path);
                    file.write_all(include_bytes!("ui.lua"))?;
                    return Ok(script);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "cannot allocate player UI file",
        ))
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Script {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn script_is_private_unique_and_removed_on_drop() {
        let a = Script::create().unwrap();
        let b = Script::create().unwrap();
        assert_ne!(a.path(), b.path());
        let path = a.path().to_owned();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            include_str!("ui.lua")
        );
        drop(a);
        assert!(!path.exists());
        assert!(b.path().exists());
    }
}
