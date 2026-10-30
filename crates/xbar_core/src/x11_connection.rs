//! X11 connection with Linux's authenticated abstract-socket fallback.
//!
//! A server can retain its abstract listener after a stale socket cleanup
//! removes the filesystem entry. Match libX11/libxcb's local connectivity
//! without changing DISPLAY, authentication policy, or remote destinations.

use x11rb::{errors::ConnectError, rust_connection::RustConnection};

/// Connect to the requested display, retaining x11rb's normal authorization.
pub fn connect(display: Option<&str>) -> Result<(RustConnection, usize), ConnectError> {
    match x11rb::connect(display) {
        Ok(connection) => Ok(connection),
        Err(original) => {
            #[cfg(target_os = "linux")]
            {
                use x11rb::reexports::x11rb_protocol::{parse_display, xauth};
                use x11rb::rust_connection::DefaultStream;
                if let Ok(parsed) = parse_display::parse_display(display) {
                    for address in parsed.connect_instruction() {
                        let parse_display::ConnectAddress::Socket(path) = address else {
                            continue;
                        };
                        let Ok(stream) = abstract_stream(&path) else {
                            continue;
                        };
                        let Ok((stream, (family, address))) =
                            DefaultStream::from_unix_stream(stream)
                        else {
                            continue;
                        };
                        let (name, data) = xauth::get_auth(family, &address, parsed.display)
                            .unwrap_or(None)
                            .unwrap_or_default();
                        let screen = usize::from(parsed.screen);
                        return RustConnection::connect_to_stream_with_auth_info(
                            stream, screen, name, data,
                        )
                        .map(|connection| (connection, screen));
                    }
                }
            }
            Err(original)
        }
    }
}

#[cfg(target_os = "linux")]
fn abstract_stream(path: &str) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixStream};
    UnixStream::connect_addr(&SocketAddr::from_abstract_name(path.as_bytes())?)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener};

    #[test]
    fn abstract_listener_survives_without_a_filesystem_socket() {
        let path = format!(
            "/tmp/jwm-x11-abstract-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let listener =
            UnixListener::bind_addr(&SocketAddr::from_abstract_name(path.as_bytes()).unwrap())
                .unwrap();
        assert!(!std::path::Path::new(&path).exists());
        let mut client = abstract_stream(&path).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        client.write_all(b"X11").unwrap();
        let mut bytes = [0; 3];
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"X11");
    }

    #[test]
    fn remote_tcp_displays_never_acquire_a_local_socket_fallback() {
        use x11rb::reexports::x11rb_protocol::parse_display::{ConnectAddress, parse_display};
        let display = parse_display(Some("tcp/example.invalid:7.2")).unwrap();
        assert_eq!(display.screen, 2);
        assert!(
            !display
                .connect_instruction()
                .any(|address| matches!(address, ConnectAddress::Socket(_)))
        );
    }
}
