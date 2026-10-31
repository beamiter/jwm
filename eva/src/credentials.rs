//! Local, interactive credential setup. Key values never appear in argv,
//! configuration, diagnostics, or child-process output.
use crate::config::Config;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufRead, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Stdio,
};

const MAX_KEY: usize = 8192;
const MAX_CONFIG: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, clap::ValueEnum, PartialEq, Eq)]
pub enum Storage {
    File,
    SecretService,
}

pub struct Saved {
    pub config: PathBuf,
    pub key_file: Option<PathBuf>,
}

struct Secret(Vec<u8>);
impl Drop for Secret {
    fn drop(&mut self) {
        // Keep the wipe observable even in optimized builds.
        unsafe { libc::explicit_bzero(self.0.as_mut_ptr().cast(), self.0.len()) };
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}

/// Only a controlling terminal may provide a key. Piped input is never treated
/// as a credential, nor is a shell-history-visible command-line key supported.
struct Terminal(File);
impl Terminal {
    fn open() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|_| invalid("请在交互式终端中运行 config api-key；不接受管道中的密钥。"))?;
        Ok(Self(file))
    }

    fn storage(&mut self) -> io::Result<Storage> {
        writeln!(self.0, "选择 API key 保存方式：")?;
        writeln!(self.0, "  1. 本地密钥文件（0600，无需额外依赖，默认）")?;
        writeln!(
            self.0,
            "  2. 系统钥匙串（需要 secret-tool 和可用的 Secret Service）"
        )?;
        loop {
            write!(self.0, "选择 [1/2，默认 1]：")?;
            self.0.flush()?;
            let mut line = String::new();
            if io::BufReader::new(&mut self.0).read_line(&mut line)? == 0 {
                return Err(invalid("已取消 API key 配置。"));
            }
            match line.trim() {
                "" | "1" => return Ok(Storage::File),
                "2" => return Ok(Storage::SecretService),
                _ => writeln!(self.0, "请输入 1 或 2。")?,
            }
        }
    }

    fn secret(&mut self) -> io::Result<Secret> {
        let fd = self.0.as_raw_fd();
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        // Handle Ctrl-C as an input byte so cancellation restores echo through
        // the guard instead of terminating in the middle of a hidden prompt.
        hidden.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
        hidden.c_cc[libc::VMIN] = 1;
        hidden.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
            return Err(io::Error::last_os_error());
        }
        struct Restore(i32, libc::termios);
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe { libc::tcsetattr(self.0, libc::TCSAFLUSH, &self.1) };
            }
        }
        let restore = Restore(fd, original);
        write!(self.0, "OpenAI API key（输入隐藏，Ctrl-C 取消）：")?;
        self.0.flush()?;
        let mut secret = Secret(Vec::with_capacity(MAX_KEY));
        let mut too_long = false;
        let result = loop {
            let mut byte = [0];
            if let Err(error) = self.0.read_exact(&mut byte) {
                break Err(error);
            }
            match byte[0] {
                b'\n' | b'\r' => {
                    break if too_long {
                        Err(invalid("API key 超过长度限制。"))
                    } else {
                        validate_key(&secret.0)
                    };
                }
                3 | 4 => break Err(invalid("已取消 API key 配置。")),
                8 | 127 => {
                    if let Some(last) = secret.0.last_mut() {
                        *last = 0;
                    }
                    secret.0.pop();
                }
                21 => {
                    secret.0.fill(0);
                    secret.0.clear();
                    too_long = false;
                }
                byte if secret.0.len() < MAX_KEY => secret.0.push(byte),
                _ => too_long = true,
            }
        };
        drop(restore);
        writeln!(self.0)?;
        result?;
        Ok(secret)
    }
}

fn validate_key(key: &[u8]) -> io::Result<()> {
    if key.is_empty() || key.len() > MAX_KEY || !key.iter().all(|b| (33..=126).contains(b)) {
        return Err(invalid(
            "API key 不能为空，也不能包含空格、换行或控制字符。",
        ));
    }
    Ok(())
}

fn absolute(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| invalid("配置或密钥文件必须具有父目录。"))
}

fn prepare_directory(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        return Err(invalid(
            "配置和密钥目录必须属于当前用户，且不能允许其他用户写入。",
        ));
    }
    Ok(())
}

/// A snapshot also holds an advisory lock on an existing file. Replacement is
/// atomic; observed concurrent edits and symlinks are rejected, not overwritten.
struct Snapshot {
    path: PathBuf,
    file: Option<File>,
    meta: Option<fs::Metadata>,
    bytes: Option<Vec<u8>>,
}
impl Snapshot {
    fn read(path: PathBuf) -> io::Result<Self> {
        let opened = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path);
        let mut file = match opened {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    file: None,
                    meta: None,
                    bytes: None,
                });
            }
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o7022 != 0
            || meta.nlink() != 1
        {
            return Err(invalid(
                "拒绝覆盖非当前用户所有的普通文件、硬链接或其他用户可写的文件。",
            ));
        }
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(invalid("配置或密钥文件正被其他配置程序使用。"));
        }
        let mut bytes = Vec::new();
        (&mut file).take(MAX_CONFIG + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONFIG {
            return Err(invalid("配置或密钥文件超过大小限制。"));
        }
        let result = Self {
            path,
            file: Some(file),
            meta: Some(meta),
            bytes: Some(bytes),
        };
        result.recheck()?;
        Ok(result)
    }

    fn recheck(&self) -> io::Result<()> {
        let current = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path);
        match (&self.meta, current) {
            (None, Err(e)) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            (Some(before), Ok(file)) => {
                let now = file.metadata()?;
                if !now.is_file()
                    || before.dev() != now.dev()
                    || before.ino() != now.ino()
                    || before.mode() != now.mode()
                    || before.uid() != now.uid()
                    || before.nlink() != now.nlink()
                    || before.mtime() != now.mtime()
                    || before.mtime_nsec() != now.mtime_nsec()
                    || before.ctime() != now.ctime()
                    || before.ctime_nsec() != now.ctime_nsec()
                {
                    return Err(invalid("配置或密钥在输入期间发生变化，请重新运行。"));
                }
                let mut bytes = Vec::new();
                file.take(MAX_CONFIG + 1).read_to_end(&mut bytes)?;
                if Some(&bytes) != self.bytes.as_ref() {
                    return Err(invalid("配置或密钥在输入期间发生变化，请重新运行。"));
                }
                Ok(())
            }
            _ => Err(invalid("配置或密钥在输入期间发生变化，请重新运行。")),
        }
    }

    fn replace(&self, bytes: &[u8]) -> io::Result<()> {
        let dir = parent(&self.path)?;
        let mut temp = tempfile::NamedTempFile::new_in(dir)?;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temp.write_all(bytes)?;
        temp.as_file().sync_all()?;
        self.recheck()?;
        temp.persist(&self.path).map_err(|e| e.error)?;
        File::open(dir)?.sync_all()
    }
}
impl Drop for Snapshot {
    fn drop(&mut self) {
        if let Some(bytes) = &mut self.bytes {
            unsafe { libc::explicit_bzero(bytes.as_mut_ptr().cast(), bytes.len()) };
        }
        // Keep the descriptor alive until the entire transaction completes.
        self.file.take();
    }
}

fn configuration(text: &str, storage: Storage, key_file: Option<&Path>) -> io::Result<String> {
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|_| invalid("配置文件格式无效；未保存 API key。"))?;
    let entry = document
        .as_table_mut()
        .entry("openai")
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    let table = entry
        .as_table_like_mut()
        .ok_or_else(|| invalid("openai 配置必须是 TOML 表。"))?;
    let mut put = |key: &str, text: &str| {
        let mut value = toml_edit::Value::from(text);
        if let Some(previous) = table.get(key).and_then(toml_edit::Item::as_value) {
            *value.decor_mut() = previous.decor().clone();
        }
        table.insert(key, toml_edit::Item::Value(value));
    };
    put(
        "api_key_source",
        match storage {
            Storage::File => "file",
            Storage::SecretService => "secret_service",
        },
    );
    if let Some(path) = key_file {
        put(
            "api_key_file",
            path.to_str()
                .ok_or_else(|| invalid("密钥文件路径必须为 UTF-8。"))?,
        );
    } else {
        table.remove("api_key_file");
    }
    let candidate = document.to_string();
    let config: Config = toml::from_str(&candidate)
        .map_err(|_| invalid("配置含未知字段或无效值；未保存 API key。"))?;
    config.validate()?;
    Ok(candidate)
}

async fn store_in_keyring(secret: &Secret) -> io::Result<()> {
    let mut child = tokio::process::Command::new("secret-tool")
        .args([
            "store",
            "--label=JWM Eva OpenAI API",
            "service",
            "jwm-eva",
            "provider",
            "openai",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| invalid("无法启动 secret-tool；请安装 libsecret-tools，或选择本地文件。"))?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| invalid("无法向钥匙串写入密钥。"))?;
        stdin.write_all(&secret.0).await?;
        stdin.shutdown().await?;
        drop(stdin);
        child.wait().await
    })
    .await;
    match result {
        Ok(Ok(status)) if status.success() => Ok(()),
        _ => Err(invalid(
            "系统钥匙串保存失败或超时；请检查 Secret Service 是否可用并已解锁，或选择本地文件。",
        )),
    }
}

pub async fn configure(path: &Path, storage: Option<Storage>) -> io::Result<Saved> {
    let mut terminal = Terminal::open()?;
    let path = absolute(path)?;
    prepare_directory(parent(&path)?)?;
    // This persistent sidecar coordinates setup processes across atomic renames,
    // including first-run setup when there is no configuration file yet.
    let config_name = path
        .file_name()
        .ok_or_else(|| invalid("配置路径必须指向文件。"))?;
    let lock_path = path.with_file_name(format!(".{}.api-key.lock", config_name.to_string_lossy()));
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)?;
    let meta = lock.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o777 != 0o600
        || meta.nlink() != 1
    {
        return Err(invalid("配置锁文件的类型、所有者或权限无效。"));
    }
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(invalid("另一个 API key 配置进程正在运行。"));
    }
    let source = Snapshot::read(path.clone())?;
    let text = match &source.bytes {
        Some(bytes) => std::str::from_utf8(bytes)
            .map_err(|_| invalid("配置文件不是 UTF-8。"))?
            .to_owned(),
        None => {
            toml::to_string_pretty(&Config::default()).map_err(|_| invalid("无法创建默认配置。"))?
        }
    };
    let current: Config =
        toml::from_str(&text).map_err(|_| invalid("配置文件无效；请先修复配置。"))?;
    current.validate()?;
    let storage = match storage {
        Some(storage) => storage,
        None => terminal.storage()?,
    };
    if storage == Storage::SecretService
        && !std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                fs::metadata(dir.join("secret-tool"))
                    .is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
            })
        })
    {
        return Err(invalid(
            "未找到 secret-tool；请安装 libsecret-tools，或选择本地密钥文件。",
        ));
    }
    let key_file = if storage == Storage::File {
        let file = current
            .openai
            .api_key_file
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| parent(&path).unwrap().join("openai.key"));
        if file == path || file.file_name() == Some(std::ffi::OsStr::new(".eva.toml.api-key.lock"))
        {
            return Err(invalid("密钥文件不能与配置或配置锁文件相同。"));
        }
        prepare_directory(parent(&file)?)?;
        Some(Snapshot::read(file)?)
    } else {
        None
    };
    let candidate = configuration(&text, storage, key_file.as_ref().map(|s| s.path.as_path()))?;
    let secret = terminal.secret()?;
    source.recheck()?;
    if let Some(file) = &key_file {
        file.replace(&secret.0)?;
        if let Err(error) = source.replace(candidate.as_bytes()) {
            // Restore only if the file still contains our write. Do not undo a
            // different writer's update while recovering a failed config save.
            if let Ok(written) = Snapshot::read(file.path.clone())
                && written.bytes.as_deref() == Some(secret.0.as_slice())
            {
                if let Some(before) = &file.bytes {
                    let _ = written.replace(before);
                } else {
                    let _ = fs::remove_file(&file.path);
                }
            }
            return Err(error);
        }
    } else {
        store_in_keyring(&secret).await?;
        source.replace(candidate.as_bytes()).map_err(|_| invalid("密钥已存入钥匙串，但配置更新失败；请重新运行或将 api_key_source 设为 secret_service。"))?;
    }
    Ok(Saved {
        config: path,
        key_file: key_file.map(|s| s.path.clone()),
    })
}

/// Restart only after the credential/configuration transaction succeeds. A
/// service failure must never roll back a saved key or report it as unsaved.
pub async fn restart_service() -> io::Result<()> {
    async fn systemctl(args: &[&str]) -> io::Result<bool> {
        let mut child = tokio::process::Command::new("systemctl")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        tokio::time::timeout(std::time::Duration::from_secs(30), child.wait())
            .await
            .map_err(|_| invalid("服务操作超时。"))?
            .map(|status| status.success())
    }

    if !systemctl(&["--user", "restart", "jwm-eva.service"])
        .await
        .unwrap_or(false)
    {
        return Err(invalid(
            "API key 和配置已保存，但服务重启失败或超时。请在桌面登录会话中运行 systemctl --user restart jwm-eva.service；无需重新输入密钥。",
        ));
    }
    if !systemctl(&["--user", "is-active", "--quiet", "jwm-eva.service"])
        .await
        .unwrap_or(false)
    {
        return Err(invalid(
            "API key 和配置已保存，但重启后的 jwm-eva.service 未确认处于运行状态。请用 systemctl --user status jwm-eva.service 检查；无需重新输入密钥。",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_only_credential_settings_and_preserves_comments() {
        let text = "# user settings\n[openai]\napi_key_source = 'env' # keep this\nmodel = 'gpt-realtime'\n[applications]\nterminal = ['xterm', '-e', 'bash']\n";
        let changed = configuration(text, Storage::File, Some(Path::new("/tmp/a key"))).unwrap();
        assert!(changed.starts_with("# user settings\n"));
        assert!(changed.contains("# keep this"));
        assert!(changed.contains("terminal = ['xterm', '-e', 'bash']"));
        let config: Config = toml::from_str(&changed).unwrap();
        assert_eq!(config.openai.api_key_source, "file");
        assert_eq!(config.openai.api_key_file.unwrap(), Path::new("/tmp/a key"));
        let changed = configuration(&changed, Storage::SecretService, None).unwrap();
        assert!(!changed.contains("api_key_file"));
        assert!(changed.contains("# keep this"));
    }

    #[test]
    fn invalid_input_is_rejected_without_echoing_it() {
        for key in [
            b"".as_slice(),
            b"sk-secret\nvalue",
            b"sk secret",
            b"sk-\x1bsecret",
        ] {
            assert!(!validate_key(key).unwrap_err().to_string().contains("sk-"));
        }
        assert!(validate_key(b"sk-test-placeholder").is_ok());
        assert!(validate_key(&vec![b'x'; MAX_KEY + 1]).is_err());
    }

    #[test]
    fn replacement_is_private_and_refuses_changed_or_linked_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credential");
        let before = Snapshot::read(path.clone()).unwrap();
        before.replace(b"first").unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(before.replace(b"clobber").is_err());
        let before = Snapshot::read(path.clone()).unwrap();
        fs::write(&path, b"external change").unwrap();
        assert!(before.replace(b"clobber").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"external change");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(Snapshot::read(link).is_err());
        let hard = dir.path().join("hard");
        fs::hard_link(&path, &hard).unwrap();
        assert!(Snapshot::read(hard).is_err());
    }

    #[test]
    fn supports_first_run_and_inline_toml_without_dropping_other_fields() {
        let initial = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(configuration(&initial, Storage::File, Some(Path::new("/tmp/key"))).is_ok());
        let inline = "openai = { api_key_source = 'env', voice = 'marin' }\n";
        let changed = configuration(inline, Storage::File, Some(Path::new("/tmp/key"))).unwrap();
        let parsed: Config = toml::from_str(&changed).unwrap();
        assert_eq!(parsed.openai.voice, "marin");
        assert_eq!(parsed.openai.api_key_source, "file");
    }
}
