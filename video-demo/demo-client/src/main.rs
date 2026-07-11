use clap::Parser;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt, CreateGCAux, CreateWindowAux, EventMask, Gcontext, PropMode,
    Rectangle, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

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

fn draw<C: Connection>(
    conn: &C,
    window: Window,
    gc: Gcontext,
    width: u16,
    height: u16,
    title: &str,
    content: &str,
    theme: &str,
    phase: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let (bg, accent, fg) = colors(theme);
    conn.change_gc(
        gc,
        &x11rb::protocol::xproto::ChangeGCAux::new().foreground(bg),
    )?;
    conn.poly_fill_rectangle(
        window,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width,
            height,
        }],
    )?;
    conn.change_gc(
        gc,
        &x11rb::protocol::xproto::ChangeGCAux::new().foreground(accent),
    )?;
    conn.poly_fill_rectangle(
        window,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width,
            height: 12,
        }],
    )?;
    if content == "grid" || content == "color-test" {
        let mut lines = Vec::new();
        for x in (0..width as usize).step_by(48) {
            lines.push(Rectangle {
                x: x as i16,
                y: 0,
                width: 1,
                height,
            });
        }
        for y in (0..height as usize).step_by(48) {
            lines.push(Rectangle {
                x: 0,
                y: y as i16,
                width,
                height: 1,
            });
        }
        conn.poly_fill_rectangle(window, gc, &lines)?;
    }
    if content == "chart" || content == "video" {
        let x = ((phase as u32 * 7) % width.saturating_sub(100).max(1) as u32) as i16;
        conn.poly_fill_rectangle(
            window,
            gc,
            &[Rectangle {
                x,
                y: (height / 2) as i16,
                width: 100,
                height: 60,
            }],
        )?;
    }
    conn.change_gc(
        gc,
        &x11rb::protocol::xproto::ChangeGCAux::new().foreground(fg),
    )?;
    let label = format!("{}  |  {}", title, content.to_ascii_uppercase());
    conn.image_text8(window, gc, 28, 48, label.as_bytes())?;
    conn.image_text8(
        window,
        gc,
        28,
        height.saturating_sub(24) as i16,
        b"JWM AUTOMATION DEMO",
    )?;
    conn.flush()?;
    Ok(())
}

fn control_server(path: PathBuf, tx: mpsc::Sender<Control>) -> std::io::Result<()> {
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    let listener = UnixListener::bind(&path)?;
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
    let (tx, rx) = mpsc::channel();
    let server_socket = socket.clone();
    std::thread::spawn(move || {
        let _ = control_server(server_socket, tx);
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
    while running {
        while let Ok(control) = rx.try_recv() {
            match control {
                Control::Close => running = false,
                Control::Minimize => {
                    conn.unmap_window(window)?;
                    conn.flush()?;
                }
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
            draw(
                &conn,
                window,
                gc,
                args.width,
                args.height,
                &title,
                &args.content,
                &theme,
                phase,
            )?;
            phase = phase.wrapping_add(1);
            last_frame = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    conn.destroy_window(window)?;
    conn.flush()?;
    let _ = std::fs::remove_file(Path::new(&socket));
    Ok(())
}
