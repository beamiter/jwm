//! Offline CLI checks and presentation. No X11, socket, or key-content reads.

use std::ffi::OsString;
use std::io;
use std::net::{Ipv6Addr, SocketAddr};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 48221;

pub fn default_key_path(xdg: Option<OsString>, home: Option<OsString>) -> io::Result<PathBuf> {
    let root = if let Some(xdg) = xdg.filter(|value| !value.is_empty()) {
        let path = PathBuf::from(xdg);
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "XDG_CONFIG_HOME must be absolute; use --key-file PATH explicitly (keygen: --output PATH)",
            ));
        }
        path
    } else {
        let home = home.filter(|value| !value.is_empty()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "HOME is missing; use --key-file PATH explicitly (keygen: --output PATH)",
            )
        })?;
        let path = PathBuf::from(home);
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HOME must be absolute; use --key-file PATH explicitly (keygen: --output PATH)",
            ));
        }
        path.join(".config")
    };
    Ok(root.join("jwm/remote.key"))
}

/// Accept an explicit old-style endpoint or supply the default port. Bare
/// IPv6 is always a whole address, never guessed to contain a port.
pub fn normalize_address(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
        || value.contains(['/', '\\'])
    {
        return Err("use HOST[:PORT] or [IPv6]:PORT, without whitespace, control characters or a URL scheme".into());
    }
    if let Ok(address) = value.parse::<SocketAddr>() {
        if address.port() == 0 {
            return Err("port must be between 1 and 65535".into());
        }
        return Ok(value.to_owned());
    }
    if let Ok(address) = value.parse::<Ipv6Addr>() {
        return Ok(format!("[{address}]:{DEFAULT_PORT}"));
    }
    if let Some(inner) = value.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let address = inner
            .parse::<Ipv6Addr>()
            .map_err(|_| "invalid bracketed IPv6 address")?;
        return Ok(format!("[{address}]:{DEFAULT_PORT}"));
    }
    if value.contains(['[', ']']) {
        return Err("use [IPv6]:PORT for an explicit IPv6 port".into());
    }
    match value.split_once(':') {
        None => Ok(format!("{value}:{DEFAULT_PORT}")),
        Some((host, port)) if !host.is_empty() && !port.contains(':') => {
            let port = port
                .parse::<u16>()
                .map_err(|_| "port must be between 1 and 65535")?;
            if port == 0 {
                return Err("port must be between 1 and 65535".into());
            }
            Ok(format!("{host}:{port}"))
        }
        _ => Err(
            "use HOST:PORT or [IPv6]:PORT; an IPv6 address without brackets uses the default port"
                .into(),
        ),
    }
}

/// Host listeners keep the system's hostname resolution at bind time and its
/// explicit port-zero contract. Parsing this option never resolves a name.
pub fn normalize_listen_address(value: &str) -> Result<String, String> {
    const HELP: &str = "use a local host IP/name with optional port (default 48221), for example 192.0.2.10 or 192.0.2.10:48221; use [IPv6]:PORT for an explicit IPv6 port";
    let invalid = || format!("invalid host --listen address: {HELP}");
    if value.is_empty()
        || value.chars().any(|c| c.is_whitespace() || c.is_control())
        || value.contains(['/', '\\'])
    {
        return Err(invalid());
    }
    if value.parse::<SocketAddr>().is_ok() {
        return Ok(value.to_owned());
    }
    if let Some(bracketed) = value.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']').ok_or_else(invalid)?;
        if !is_ipv6_listener_host(host) {
            return Err(invalid());
        }
        if suffix.is_empty() {
            return Ok(format!("[{host}]:{DEFAULT_PORT}"));
        }
        let port = suffix.strip_prefix(':').ok_or_else(invalid)?;
        port.parse::<u16>().map_err(|_| invalid())?;
        // Preserve zone spelling and defer any system-specific scope lookup to
        // the same bind operation that handled explicit endpoints before.
        return Ok(value.to_owned());
    }
    if value.contains(['[', ']']) {
        return Err(invalid());
    }
    if is_ipv6_listener_host(value) {
        // A bare IPv6 address is always the whole address, never a guessed port.
        return Ok(format!("[{value}]:{DEFAULT_PORT}"));
    }
    if let Some((host, port)) = value.split_once(':') {
        if host.is_empty() || port.parse::<u16>().is_err() {
            return Err(invalid());
        }
        Ok(value.to_owned())
    } else {
        Ok(format!("{value}:{DEFAULT_PORT}"))
    }
}

fn is_ipv6_listener_host(value: &str) -> bool {
    let address = if let Some((address, zone)) = value.split_once('%') {
        if zone.is_empty() || zone.contains(['%', ':', '[', ']']) {
            return false;
        }
        address
    } else {
        value
    };
    address.parse::<Ipv6Addr>().is_ok()
}

/// Keep bind failures separate from capture, authentication and peer errors.
/// The original I/O error remains available as the diagnostic's source.
pub fn listener_bind_error(address: &str, error: io::Error) -> io::Error {
    let hint = match error.kind() {
        io::ErrorKind::AddrNotAvailable => {
            "This listener address must belong to this host, not the viewer. Check this machine's interface address; no interface was inspected automatically."
        }
        io::ErrorKind::AddrInUse => {
            "Another listener is using this address/port. Stop only the intended existing host, or choose a different port and use that same port in connect."
        }
        io::ErrorKind::PermissionDenied => {
            "The operating system denied this bind. Check local port privileges and security policy; --allow-lan does not grant OS permission. Do not run the desktop helper as root to bypass this."
        }
        _ => {
            "Check the listener IP/name and port and the original OS error. SSH reachability does not verify the remote-desktop port; this error alone does not establish a firewall problem."
        }
    };
    io::Error::new(
        error.kind(),
        ListenerBindFailure {
            address: address.to_owned(),
            hint,
            cause: error,
        },
    )
}

#[derive(Debug)]
struct ListenerBindFailure {
    address: String,
    hint: &'static str,
    cause: io::Error,
}

impl std::fmt::Display for ListenerBindFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cannot bind host listener {:?}: {}. {}",
            self.address, self.cause, self.hint
        )
    }
}

impl std::error::Error for ListenerBindFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

/// Reports metadata only; even plausible length does not validate key bytes.
pub fn key_metadata(path: &Path, current_uid: u32) -> io::Result<String> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "key path must be a regular file, not a symlink or directory",
        ));
    }
    if metadata.uid() != current_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "key file is not owned by the current user",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "key file permits group/other access; review it and run chmod 600 on the intended file",
        ));
    }
    if metadata.mode() & 0o400 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "key file has no owner read permission; review the intended file's permissions",
        ));
    }
    if !(64..=66).contains(&metadata.len()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "key file length is not 64-66 bytes; key contents were not read",
        ));
    }
    Ok(format!(
        "owner=current user, mode={:03o}, bytes={}; contents NOT checked",
        metadata.mode() & 0o777,
        metadata.len()
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientStage {
    Setup,
    Connect,
    Negotiate,
    FirstFrame,
    LocalClipboard,
    Display,
    FirstFrameAck,
    Session,
}

pub fn failure_hint(stage: ClientStage, kind: Option<io::ErrorKind>) -> &'static str {
    match stage {
        ClientStage::Setup => {
            "Check the local key path/permissions, DISPLAY and X11 keyboard setup. No remote desktop connection was opened."
        }
        ClientStage::Connect => match kind {
            Some(io::ErrorKind::ConnectionRefused) => {
                "Check the host's printed listener address and that it is running; check firewall policy without opening access more broadly."
            }
            Some(io::ErrorKind::TimedOut) => {
                "Check the host address, route and firewall policy. No automatic retry was made."
            }
            _ => "Check HOST[:PORT] and local name resolution, then the host listener and route.",
        },
        ClientStage::Negotiate => {
            "Check the host's refusal reason, that both helpers are the same version, and that the intended private key was copied securely. Do not bypass authentication; another viewer may already be connected."
        }
        ClientStage::FirstFrame => {
            "Authentication completed, but the first frame was not ready. Check host capture errors and the reported receive/decode failure."
        }
        ClientStage::LocalClipboard => {
            "Authentication completed; local clipboard setup failed. Check the reported local clipboard/display error. No downgrade was attempted."
        }
        ClientStage::Display => {
            "Authentication completed; local viewer setup or drawing failed. Check the local X11 display and the reported rendering error."
        }
        ClientStage::FirstFrameAck => {
            "The first desktop frame was drawn, but its acknowledgement failed. Check the connection and host terminal before reconnecting."
        }
        ClientStage::Session => {
            "The session ended. Check both terminals; verify the host identity and selected monitor/region before rerunning your connect command. Input and clipboard permissions are negotiated again."
        }
    }
}

pub fn session_mode(pointer: bool, keyboard: bool) -> &'static str {
    match (pointer, keyboard) {
        (true, true) => "keyboard + pointer",
        (true, false) => "pointer only; keyboard disabled",
        (false, true) => "keyboard only",
        (false, false) => "view-only",
    }
}

pub fn viewer_title(
    pointer: bool,
    keyboard: bool,
    grab_input: bool,
    grabbed: bool,
    clipboard: bool,
    escape: &str,
) -> String {
    let mode = session_mode(pointer, keyboard);
    let input = if !(pointer || keyboard) {
        String::new()
    } else if grab_input && !grabbed {
        "; input released - click to control".to_owned()
    } else if grab_input {
        format!("; grabbed - double-tap {escape} to release")
    } else {
        "; input while focused".to_owned()
    };
    format!(
        "JWM Remote - {mode}{input}; clipboard {}",
        if clipboard { "ON" } else { "off" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_addresses_supply_default_port_without_losing_bind_compatibility() {
        for (input, expected) in [
            ("192.0.2.10", "192.0.2.10:48221"),
            ("127.0.0.1", "127.0.0.1:48221"),
            ("0.0.0.0", "0.0.0.0:48221"),
            ("localhost", "localhost:48221"),
            ("host.invalid", "host.invalid:48221"),
            ("localhost:0", "localhost:0"),
            ("host.invalid:1234", "host.invalid:1234"),
            ("127.0.0.1:0", "127.0.0.1:0"),
            ("192.0.2.10:48221", "192.0.2.10:48221"),
            ("::1", "[::1]:48221"),
            ("[::1]", "[::1]:48221"),
            ("[::1]:0", "[::1]:0"),
            ("[::1]:65535", "[::1]:65535"),
            ("2001:db8::1234", "[2001:db8::1234]:48221"),
            ("[fe80::1%3]:0", "[fe80::1%3]:0"),
            ("[fe80::1%eth0]:1234", "[fe80::1%eth0]:1234"),
            ("fe80::1%eth0", "[fe80::1%eth0]:48221"),
        ] {
            assert_eq!(
                normalize_listen_address(input).unwrap(),
                expected,
                "{input}"
            );
        }
        // Unlike a local listener, a viewer cannot connect to port zero.
        assert!(normalize_address("127.0.0.1:0").is_err());
        assert!(normalize_address("localhost:0").is_err());
        assert!(normalize_address("[::1]:0").is_err());
    }

    #[test]
    fn listener_addresses_reject_missing_hosts_bad_ports_and_controls() {
        for invalid in [
            "",
            ":48221",
            "host:",
            "host:65536",
            "host:-1",
            "host:no",
            "bad::address",
            "[::1]:",
            "[::1]:48221:4",
            "[::1",
            "::1]",
            "[host]:48221",
            "[]:48221",
            "[fe80::1%]:0",
            "[fe80::1%a%b]:0",
            "http://host:48221",
            "host name",
            " host",
            "host\n",
            "host\0",
            "host\x1b",
            "host\u{85}",
        ] {
            let error = normalize_listen_address(invalid).unwrap_err();
            assert!(error.contains("host --listen"));
            assert!(error.contains("local host IP/name"));
            assert!(!error.chars().any(char::is_control));
        }
    }

    #[test]
    fn listener_bind_diagnostics_preserve_kind_cause_and_correct_next_step() {
        for (kind, expected) in [
            (
                io::ErrorKind::AddrNotAvailable,
                "belong to this host, not the viewer",
            ),
            (io::ErrorKind::AddrInUse, "Another listener"),
            (
                io::ErrorKind::PermissionDenied,
                "--allow-lan does not grant OS permission",
            ),
            (io::ErrorKind::InvalidInput, "original OS error"),
        ] {
            let error =
                listener_bind_error("192.0.2.10:48221", io::Error::new(kind, "test OS cause"));
            assert_eq!(error.kind(), kind);
            let text = error.to_string();
            assert!(text.contains(expected), "{text}");
            assert!(text.contains("test OS cause"));
            let cause = error
                .get_ref()
                .unwrap()
                .source()
                .unwrap()
                .downcast_ref::<io::Error>()
                .unwrap();
            assert_eq!(cause.kind(), kind);
        }
        let escaped =
            listener_bind_error("host\nFAKE", io::Error::from(io::ErrorKind::Other)).to_string();
        assert!(escaped.contains("host\\nFAKE"));
        assert!(!escaped.contains("host\nFAKE"));
    }
    #[test]
    fn default_paths_never_fall_back_to_working_directory() {
        assert_eq!(
            default_key_path(Some("/config".into()), None).unwrap(),
            PathBuf::from("/config/jwm/remote.key")
        );
        assert_eq!(
            default_key_path(None, Some("/home/me".into())).unwrap(),
            PathBuf::from("/home/me/.config/jwm/remote.key")
        );
        for (xdg, home) in [
            (None, None),
            (None, Some("relative".into())),
            (Some("relative".into()), Some("/home/me".into())),
        ] {
            assert!(default_key_path(xdg, home).is_err());
        }
        assert_eq!(
            default_key_path(Some("".into()), Some("/home/me".into())).unwrap(),
            PathBuf::from("/home/me/.config/jwm/remote.key")
        );
    }
    #[test]
    fn addresses_keep_explicit_ports_and_do_not_guess_ipv6_ports() {
        for (input, expected) in [
            ("host", "host:48221"),
            ("127.0.0.1", "127.0.0.1:48221"),
            ("host:1234", "host:1234"),
            ("[::1]:1234", "[::1]:1234"),
            ("[::1]", "[::1]:48221"),
            ("::1", "[::1]:48221"),
            ("2001:db8::1234", "[2001:db8::1234]:48221"),
        ] {
            assert_eq!(normalize_address(input).unwrap(), expected);
        }
        for invalid in [
            "",
            "host:0",
            "host:65536",
            "host:no",
            "host:",
            ":48221",
            "http://host",
            "host name",
            "[oops]",
            "[::1]:",
            "bad::address",
            "host\0",
            "host\x1b",
            "host\u{85}",
        ] {
            assert!(normalize_address(invalid).is_err(), "{invalid}");
        }
    }
    #[test]
    fn hints_distinguish_connection_and_authenticated_refusal() {
        let refused = Some(io::ErrorKind::ConnectionRefused);
        assert!(failure_hint(ClientStage::Connect, refused).contains("listener"));
        assert!(failure_hint(ClientStage::Negotiate, refused).contains("another viewer"));
        assert!(!failure_hint(ClientStage::FirstFrame, None).contains("connected"));
        assert!(failure_hint(ClientStage::Session, None).contains("verify the host identity"));
    }
    #[test]
    fn metadata_checks_never_need_key_contents_and_reject_unsafe_paths() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        // A zero-filled fixture is deliberately not a usable key; no keygen,
        // randomness, socket, display or secret data is involved in this test.
        let directory = std::env::temp_dir().join(format!(
            "jwm-remote-offline-metadata-{}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("not-a-key");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(65).unwrap();
        let uid = file.metadata().unwrap().uid();
        drop(file);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(
            key_metadata(&path, uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
        assert!(
            key_metadata(&path, uid)
                .unwrap()
                .contains("contents NOT checked")
        );
        assert_eq!(
            key_metadata(&path, uid.wrapping_add(1)).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            key_metadata(&path, uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let link = directory.join("link");
        symlink(&path, &link).unwrap();
        assert_eq!(
            key_metadata(&link, uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            key_metadata(&directory, uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            key_metadata(&directory.join("missing"), uid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(3)
            .unwrap();
        assert_eq!(
            key_metadata(&path, uid).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn titles_show_actual_capabilities_and_grab_state() {
        assert!(viewer_title(false, false, true, false, false, "F12").contains("view-only"));
        assert!(!viewer_title(false, false, true, false, false, "F12").contains("click"));
        assert!(
            viewer_title(true, false, true, false, false, "F12")
                .contains("pointer only; keyboard disabled; input released")
        );
        assert!(
            viewer_title(true, true, true, true, true, "Pause")
                .contains("double-tap Pause to release; clipboard ON")
        );
    }
}
