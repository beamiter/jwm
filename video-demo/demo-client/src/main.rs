use clap::Parser;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask,
    Gcontext, PropMode, Rectangle, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::x11_utils::Serialize;

const ICONIC_STATE: u32 = 3;
const EWMH_SOURCE_APPLICATION: u32 = 1;

#[derive(Parser, Debug)]
#[command(about = "Deterministic X11 windows for JWM video automation")]
struct Args {
    #[arg(long, default_value = "MASTER")]
    title: String,
    #[arg(long = "class", default_value = "JwmDemo")]
    class_name: String,
    #[arg(long, default_value = "master")]
    instance: String,
    #[arg(long, default_value = "blue")]
    theme: String,
    #[arg(long, default_value = "grid")]
    content: String,
    #[arg(long, default_value_t = 720)]
    width: u16,
    #[arg(long, default_value_t = 480)]
    height: u16,
    #[arg(long)]
    animate: bool,
    #[arg(long)]
    urgent: bool,
    #[arg(long, default_value_t = 1.0)]
    opacity: f32,
    #[arg(long)]
    socket: Option<PathBuf>,
}

enum Control {
    Close,
    Minimize,
    Restore,
    Title(String),
    Theme(String),
    Urgent(bool),
}

fn intern<C: Connection>(conn: &C, name: &[u8]) -> Result<u32, Box<dyn std::error::Error>> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

fn set_title<C: Connection>(
    conn: &C,
    window: Window,
    title: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
    )?;
    let net_name = intern(conn, b"_NET_WM_NAME")?;
    let utf8 = intern(conn, b"UTF8_STRING")?;
    conn.change_property8(PropMode::REPLACE, window, net_name, utf8, title.as_bytes())?;
    Ok(())
}

fn set_urgent<C: Connection>(
    conn: &C,
    window: Window,
    urgent: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let hints = intern(conn, b"WM_HINTS")?;
    let flags = if urgent { 1u32 << 8 } else { 0 };
    conn.change_property32(
        PropMode::REPLACE,
        window,
        hints,
        hints,
        &[flags, 0, 0, 0, 0, 0, 0, 0, 0],
    )?;
    Ok(())
}

fn root_wm_request_mask() -> EventMask {
    EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY
}

fn minimize_request(window: Window, wm_change_state: Atom) -> ClientMessageEvent {
    ClientMessageEvent::new(32, window, wm_change_state, [ICONIC_STATE, 0, 0, 0, 0])
}

fn restore_request(window: Window, net_active_window: Atom) -> ClientMessageEvent {
    ClientMessageEvent::new(
        32,
        window,
        net_active_window,
        [EWMH_SOURCE_APPLICATION, x11rb::CURRENT_TIME, 0, 0, 0],
    )
}

fn send_root_wm_request<C: Connection>(
    conn: &C,
    root: Window,
    event: ClientMessageEvent,
) -> Result<(), Box<dyn std::error::Error>> {
    conn.send_event(false, root, root_wm_request_mask(), event.serialize())?;
    conn.flush()?;
    Ok(())
}

fn colors(theme: &str) -> (u32, u32, u32) {
    match theme {
        "red" => (0x33151b, 0xee5266, 0xffd6dc),
        "green" => (0x102a24, 0x42d392, 0xd5fff0),
        "purple" => (0x26183d, 0xa879ff, 0xeee3ff),
        "orange" => (0x382312, 0xffa640, 0xffe4c2),
        "gray" => (0x20242a, 0x8290a3, 0xf0f3f7),
        _ => (0x10243d, 0x3b9eff, 0xd9efff),
    }
}

struct DrawingContext<'a, C> {
    conn: &'a C,
    window: Window,
    gc: Gcontext,
    width: u16,
    height: u16,
}

impl<C: Connection> DrawingContext<'_, C> {
    fn draw(
        &self,
        title: &str,
        content: &str,
        theme: &str,
        phase: u16,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (bg, accent, fg) = colors(theme);
        self.conn.change_gc(
            self.gc,
            &x11rb::protocol::xproto::ChangeGCAux::new().foreground(bg),
        )?;
        self.conn.poly_fill_rectangle(
            self.window,
            self.gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
            }],
        )?;
        self.conn.change_gc(
            self.gc,
            &x11rb::protocol::xproto::ChangeGCAux::new().foreground(accent),
        )?;
        self.conn.poly_fill_rectangle(
            self.window,
            self.gc,
            &[Rectangle {
                x: 0,
                y: 0,
                width: self.width,
                height: 12,
            }],
        )?;
        if content == "grid" || content == "color-test" {
            let mut lines = Vec::new();
            for x in (0..self.width as usize).step_by(48) {
                lines.push(Rectangle {
                    x: x as i16,
                    y: 0,
                    width: 1,
                    height: self.height,
                });
            }
            for y in (0..self.height as usize).step_by(48) {
                lines.push(Rectangle {
                    x: 0,
                    y: y as i16,
                    width: self.width,
                    height: 1,
                });
            }
            self.conn
                .poly_fill_rectangle(self.window, self.gc, &lines)?;
        }
        if content == "chart" || content == "video" {
            let x = ((phase as u32 * 7) % self.width.saturating_sub(100).max(1) as u32) as i16;
            self.conn.poly_fill_rectangle(
                self.window,
                self.gc,
                &[Rectangle {
                    x,
                    y: (self.height / 2) as i16,
                    width: 100,
                    height: 60,
                }],
            )?;
        }
        self.conn.change_gc(
            self.gc,
            &x11rb::protocol::xproto::ChangeGCAux::new().foreground(fg),
        )?;
        let label = format!("{}  |  {}", title, content.to_ascii_uppercase());
        self.conn
            .image_text8(self.window, self.gc, 28, 48, label.as_bytes())?;
        self.conn.image_text8(
            self.window,
            self.gc,
            28,
            self.height.saturating_sub(24) as i16,
            b"JWM AUTOMATION DEMO",
        )?;
        self.conn.flush()?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

fn control_socket_identity(path: &Path) -> std::io::Result<Option<SocketIdentity>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            Ok(Some(SocketIdentity::from_metadata(&metadata)))
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "refusing to replace non-socket control path {}",
                path.display()
            ),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn remove_stale_socket_if_unchanged(path: &Path, expected: SocketIdentity) -> std::io::Result<()> {
    match control_socket_identity(path)? {
        Some(current) if current == expected => fs::remove_file(path),
        None => Ok(()),
        Some(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "control socket changed while checking whether it was stale: {}",
                path.display()
            ),
        )),
    }
}

fn remove_socket_if_owned(path: &Path, expected: SocketIdentity) -> std::io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_socket()
                && SocketIdentity::from_metadata(&metadata) == expected =>
        {
            fs::remove_file(path)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

struct ControlSocketGuard {
    path: PathBuf,
    identity: SocketIdentity,
}

impl Drop for ControlSocketGuard {
    fn drop(&mut self) {
        let _ = remove_socket_if_owned(&self.path, self.identity);
    }
}

fn bind_control_socket(path: &Path) -> std::io::Result<(UnixListener, SocketIdentity)> {
    if let Some(identity) = control_socket_identity(path)? {
        match UnixStream::connect(path) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!("control socket is already active at {}", path.display()),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                remove_stale_socket_if_unchanged(path, identity)?;
            }
            // The name disappeared after metadata inspection; binding can
            // proceed without treating the benign race as an error.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    let listener = UnixListener::bind(path)?;
    let identity = control_socket_identity(path)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("bound control socket disappeared: {}", path.display()),
        )
    })?;
    if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
        drop(listener);
        let _ = remove_socket_if_owned(path, identity);
        return Err(error);
    }
    match control_socket_identity(path)? {
        Some(current) if current == identity => {}
        _ => {
            drop(listener);
            let _ = remove_socket_if_owned(path, identity);
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!(
                    "control socket changed while setting its permissions: {}",
                    path.display()
                ),
            ));
        }
    }
    Ok((listener, identity))
}

fn control_server(listener: UnixListener, tx: mpsc::Sender<Control>) -> std::io::Result<()> {
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(value) => value,
            Err(_) => continue,
        };
        let reader = BufReader::new(stream.try_clone()?);
        for line in reader.lines().map_while(Result::ok) {
            let value: serde_json::Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let command = match value.get("command").and_then(|v| v.as_str()) {
                Some("close") => Some(Control::Close),
                Some("minimize") => Some(Control::Minimize),
                Some("restore") => Some(Control::Restore),
                Some("title") => value
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(|v| Control::Title(v.to_string())),
                Some("theme") => value
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(|v| Control::Theme(v.to_string())),
                Some("urgent") => Some(Control::Urgent(
                    value.get("value").and_then(|v| v.as_bool()).unwrap_or(true),
                )),
                _ => None,
            };
            if let Some(command) = command {
                let close = matches!(command, Control::Close);
                if tx.send(command).is_err() {
                    return Ok(());
                }
                let _ = writeln!(stream, "{{\"success\":true}}");
                if close {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let window = conn.generate_id()?;
    let gc = conn.generate_id()?;
    conn.create_window(
        screen.root_depth,
        window,
        screen.root,
        0,
        0,
        args.width,
        args.height,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new()
            .background_pixel(colors(&args.theme).0)
            .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY),
    )?;
    conn.create_gc(
        gc,
        window,
        &CreateGCAux::new().foreground(colors(&args.theme).2),
    )?;
    set_title(&conn, window, &args.title)?;
    let class = format!("{}\0{}\0", args.instance, args.class_name);
    conn.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        class.as_bytes(),
    )?;
    let wm_protocols = intern(&conn, b"WM_PROTOCOLS")?;
    let wm_delete = intern(&conn, b"WM_DELETE_WINDOW")?;
    let wm_change_state = intern(&conn, b"WM_CHANGE_STATE")?;
    let net_active_window = intern(&conn, b"_NET_ACTIVE_WINDOW")?;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        wm_protocols,
        AtomEnum::ATOM,
        &[wm_delete],
    )?;
    let opacity_atom = intern(&conn, b"_NET_WM_WINDOW_OPACITY")?;
    let opacity = (args.opacity.clamp(0.0, 1.0) * u32::MAX as f32) as u32;
    conn.change_property32(
        PropMode::REPLACE,
        window,
        opacity_atom,
        AtomEnum::CARDINAL,
        &[opacity],
    )?;
    set_urgent(&conn, window, args.urgent)?;
    conn.map_window(window)?;
    conn.flush()?;
    let socket = args
        .socket
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/jwm-demo-{}.sock", std::process::id())));
    // Bind before publishing the path so automation never races the server
    // thread. Existing regular files and symlinks are never unlinked, while a
    // stale socket is replaced and immediately restricted to its owner.
    let (listener, socket_identity) = bind_control_socket(&socket)?;
    let _socket_guard = ControlSocketGuard {
        path: socket.clone(),
        identity: socket_identity,
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = control_server(listener, tx);
    });
    println!(
        "{}",
        serde_json::json!({"window_id": window, "window_id_hex": format!("0x{window:x}"), "socket": socket})
    );
    let mut title = args.title;
    let mut theme = args.theme;
    let mut phase = 0u16;
    let mut last_frame = Instant::now() - Duration::from_secs(1);
    let mut running = true;
    let drawing = DrawingContext {
        conn: &conn,
        window,
        gc,
        width: args.width,
        height: args.height,
    };
    while running {
        while let Ok(control) = rx.try_recv() {
            match control {
                Control::Close => running = false,
                Control::Minimize => {
                    send_root_wm_request(
                        &conn,
                        screen.root,
                        minimize_request(window, wm_change_state),
                    )?;
                }
                Control::Restore => send_root_wm_request(
                    &conn,
                    screen.root,
                    restore_request(window, net_active_window),
                )?,
                Control::Title(value) => {
                    title = value;
                    set_title(&conn, window, &title)?;
                }
                Control::Theme(value) => theme = value,
                Control::Urgent(value) => set_urgent(&conn, window, value)?,
            }
        }
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::ClientMessage(event)
                    if event.type_ == wm_protocols && event.data.as_data32()[0] == wm_delete =>
                {
                    running = false
                }
                Event::DestroyNotify(_) => running = false,
                Event::Expose(_) => last_frame = Instant::now() - Duration::from_secs(1),
                _ => {}
            }
        }
        if last_frame.elapsed() >= Duration::from_millis(if args.animate { 33 } else { 250 }) {
            drawing.draw(&title, &args.content, &theme, phase)?;
            phase = phase.wrapping_add(1);
            last_frame = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    conn.destroy_window(window)?;
    conn.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn socket_fixture(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "jwm-demo-client-{}-{label}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn minimize_uses_icccm_change_state_message() {
        let window = 0x0102_0304;
        let atom = 0x0506_0708;
        let event = minimize_request(window, atom);

        assert_eq!(event.format, 32);
        assert_eq!(event.window, window);
        assert_eq!(event.type_, atom);
        assert_eq!(event.data.as_data32(), [ICONIC_STATE, 0, 0, 0, 0]);
        assert_eq!(
            root_wm_request_mask(),
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY
        );
    }

    #[test]
    fn restore_uses_ewmh_active_window_message() {
        let window = 0x0102_0304;
        let atom = 0x0506_0708;
        let event = restore_request(window, atom);

        assert_eq!(event.format, 32);
        assert_eq!(event.window, window);
        assert_eq!(event.type_, atom);
        assert_eq!(
            event.data.as_data32(),
            [EWMH_SOURCE_APPLICATION, x11rb::CURRENT_TIME, 0, 0, 0]
        );
    }

    #[test]
    fn control_socket_is_private_and_does_not_replace_regular_files() {
        let directory = socket_fixture("control-socket");
        fs::create_dir(&directory).expect("fixture directory");
        let path = directory.join("control.sock");

        fs::write(&path, b"keep me").expect("regular-file fixture");
        let error = bind_control_socket(&path).expect_err("regular file must be preserved");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).expect("preserved fixture"), b"keep me");

        fs::remove_file(&path).expect("remove fixture");
        let (listener, _) = bind_control_socket(&path).expect("bind private socket");
        let error = bind_control_socket(&path).expect_err("active socket must not be replaced");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        drop(listener);

        // Once the listener is gone, the filesystem entry is stale and may be
        // replaced safely by a new owner-private listener.
        let (listener, _) = bind_control_socket(&path).expect("replace stale socket");
        let mode = fs::symlink_metadata(&path)
            .expect("socket metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        drop(listener);
        fs::remove_file(&path).expect("remove socket");
        fs::remove_dir(&directory).expect("remove fixture directory");
    }

    #[test]
    fn control_socket_cleanup_preserves_a_replacement_path() {
        let directory = socket_fixture("cleanup-identity");
        fs::create_dir(&directory).expect("fixture directory");
        let path = directory.join("control.sock");
        let (listener, identity) = bind_control_socket(&path).expect("bind private socket");
        let guard = ControlSocketGuard {
            path: path.clone(),
            identity,
        };

        fs::remove_file(&path).expect("unlink bound socket");
        fs::write(&path, b"replacement").expect("replacement fixture");
        drop(guard);

        assert_eq!(
            fs::read(&path).expect("preserved replacement"),
            b"replacement"
        );
        drop(listener);
        fs::remove_file(&path).expect("remove replacement");
        fs::remove_dir(&directory).expect("remove fixture directory");
    }

    #[test]
    fn stale_socket_cleanup_rejects_an_identity_change() {
        let directory = socket_fixture("stale-identity");
        fs::create_dir(&directory).expect("fixture directory");
        let path = directory.join("control.sock");
        let (listener, identity) = bind_control_socket(&path).expect("bind private socket");

        fs::remove_file(&path).expect("unlink bound socket");
        fs::write(&path, b"replacement").expect("replacement fixture");
        let error = remove_stale_socket_if_unchanged(&path, identity)
            .expect_err("changed path must not be removed");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&path).expect("preserved replacement"),
            b"replacement"
        );

        drop(listener);
        fs::remove_file(&path).expect("remove replacement");
        fs::remove_dir(&directory).expect("remove fixture directory");
    }
}
