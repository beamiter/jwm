//! Trusted-LAN remote desktop host.

use super::RemoteResult;
use super::frame::encode_frame;
use super::key::load_key_file;
use super::messages::{ClientHello, ServerHello, decode_input};
use super::protocol::{MessageKind, SessionReader, SessionWriter, server_handshake};
use super::x11_capture::{CaptureSource, X11Capture};
use super::x11_input::InputInjector;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(8);

struct HandshakeDeadline {
    cancelled: Arc<(Mutex<bool>, Condvar)>,
    worker: Option<thread::JoinHandle<()>>,
}

impl HandshakeDeadline {
    fn arm(stream: &TcpStream) -> io::Result<Self> {
        let deadline_stream = stream.try_clone()?;
        let cancelled = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker = thread::Builder::new()
            .name("jwm-remote-handshake-deadline".into())
            .spawn(move || {
                let (lock, wake) = &*worker_cancelled;
                let Ok(guard) = lock.lock() else {
                    let _ = deadline_stream.shutdown(Shutdown::Both);
                    return;
                };
                let Ok((cancelled, timeout)) =
                    wake.wait_timeout_while(guard, HANDSHAKE_TIMEOUT, |cancelled| !*cancelled)
                else {
                    let _ = deadline_stream.shutdown(Shutdown::Both);
                    return;
                };
                if !*cancelled && timeout.timed_out() {
                    let _ = deadline_stream.shutdown(Shutdown::Both);
                }
            })?;
        Ok(Self {
            cancelled,
            worker: Some(worker),
        })
    }

    fn cancel(&mut self) {
        let (lock, wake) = &*self.cancelled;
        if let Ok(mut cancelled) = lock.lock() {
            *cancelled = true;
            wake.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for HandshakeDeadline {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone, Debug)]
pub struct HostOptions {
    pub listen: String,
    pub key_file: PathBuf,
    pub display: Option<String>,
    pub fps: u16,
    pub jpeg_quality: u8,
    pub max_width: u16,
    pub capture_source: CaptureSource,
    pub allow_lan: bool,
    pub allow_input: bool,
    pub once: bool,
}

pub fn run_host(options: HostOptions) -> RemoteResult<()> {
    let key = load_key_file(&options.key_file)?;
    let listener = TcpListener::bind(&options.listen)?;
    listener.set_nonblocking(true)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&shutdown))?;
    flag::register(SIGTERM, Arc::clone(&shutdown))?;
    let local_address = listener.local_addr()?;
    if !local_address.ip().is_loopback() && !options.allow_lan {
        return Err(permission_denied(format!(
            "refusing non-loopback listener {local_address}; pass --allow-lan for a trusted LAN"
        ))
        .into());
    }

    eprintln!("jwm-remote: listening on {local_address}");
    if !local_address.ip().is_loopback() {
        eprintln!(
            "jwm-remote: trusted-LAN mode authenticates traffic but does not encrypt the screen or input"
        );
    }
    if !options.allow_input {
        eprintln!("jwm-remote: input control is disabled (view-only host)");
    }

    while !shutdown.load(Ordering::Acquire) {
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let peer = stream.peer_addr()?;
        eprintln!("jwm-remote: connection from {peer}");
        if let Err(error) = serve_client(stream, &key, &options, &shutdown) {
            eprintln!("jwm-remote: session with {peer} ended: {error}");
        }
        if options.once {
            return Ok(());
        }
    }
    Ok(())
}

fn serve_client(
    mut stream: TcpStream,
    key: &[u8],
    options: &HostOptions,
    shutdown: &Arc<AtomicBool>,
) -> RemoteResult<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut handshake_deadline = HandshakeDeadline::arm(&stream)?;
    let session_keys = server_handshake(&mut stream, key, rand::random())?;
    let reader_stream = stream.try_clone()?;
    reader_stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let (receive_key, send_key) = session_keys.into_server();
    let mut reader = SessionReader::new(reader_stream, receive_key);
    let mut writer = SessionWriter::new(stream, send_key);
    writer.get_ref().set_write_timeout(Some(WRITE_TIMEOUT))?;

    let (kind, hello_payload) = reader.read_message()?;
    if kind != MessageKind::Hello {
        return Err(
            invalid_data("first authenticated client message must negotiate the session").into(),
        );
    }
    let hello = ClientHello::decode(&hello_payload)?;
    handshake_deadline.cancel();
    reader
        .get_ref()
        .set_read_timeout(Some(SESSION_IDLE_TIMEOUT))?;

    let mut capture = X11Capture::connect(
        options.display.as_deref(),
        options.capture_source,
        options.max_width,
    )?;
    let mut keyboard_enabled = false;
    let mut verified_keymap = None;
    let injector = if options.allow_input && hello.request_input {
        match InputInjector::connect(options.display.as_deref()) {
            Ok(injector) => {
                match injector.keymap_fingerprint() {
                    Ok(fingerprint) if fingerprint == hello.keymap_fingerprint => {
                        keyboard_enabled = true;
                        verified_keymap = Some(fingerprint);
                    }
                    Ok(_) => {
                        eprintln!(
                            "jwm-remote: peer X11 keymap differs; keyboard disabled, pointer remains enabled"
                        );
                    }
                    Err(error) => {
                        eprintln!(
                            "jwm-remote: could not verify the host X11 keymap; keyboard disabled: {error}"
                        );
                    }
                }
                Some(injector)
            }
            Err(error) => {
                eprintln!("jwm-remote: XTEST input unavailable; continuing view-only: {error}");
                None
            }
        }
    } else {
        None
    };
    let pointer_enabled = injector.is_some();
    writer.write_message(
        MessageKind::HelloAck,
        &ServerHello {
            pointer_enabled,
            keyboard_enabled,
        }
        .encode(),
    )?;
    writer.flush()?;
    eprintln!(
        "jwm-remote: authenticated session started ({})",
        if keyboard_enabled {
            "screen + pointer + keyboard"
        } else if pointer_enabled {
            "screen + pointer"
        } else {
            "view only"
        }
    );

    let control = writer.get_ref().try_clone()?;
    let running = Arc::new(AtomicBool::new(true));
    let input_running = Arc::clone(&running);
    let input_thread = thread::Builder::new()
        .name("jwm-remote-input".into())
        .spawn(move || {
            let result = receive_input(
                reader,
                injector,
                keyboard_enabled,
                verified_keymap,
                &input_running,
            );
            input_running.store(false, Ordering::Release);
            result
        })?;

    let stream_result = stream_frames(&mut capture, &mut writer, &running, shutdown, options);
    running.store(false, Ordering::Release);
    let _ = control.shutdown(Shutdown::Both);
    match input_thread.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) if stream_result.is_ok() => return Err(error),
        Ok(Err(error)) => eprintln!("jwm-remote: input receiver stopped: {error}"),
        Err(_) => return Err(io::Error::other("remote input thread panicked").into()),
    }
    stream_result
}

fn stream_frames(
    capture: &mut X11Capture,
    writer: &mut super::protocol::SessionWriter<TcpStream>,
    running: &AtomicBool,
    shutdown: &AtomicBool,
    options: &HostOptions,
) -> RemoteResult<()> {
    let interval = Duration::from_secs_f64(1.0 / f64::from(options.fps.clamp(1, 60)));
    let mut next_frame = Instant::now();
    let mut report_started = Instant::now();
    let mut report_frames = 0_u64;
    let mut sequence = 0_u64;

    while running.load(Ordering::Acquire) && !shutdown.load(Ordering::Acquire) {
        let now = Instant::now();
        if now < next_frame {
            thread::sleep((next_frame - now).min(Duration::from_millis(20)));
            continue;
        }
        next_frame += interval;
        if next_frame < now {
            next_frame = now + interval;
        }

        let frame = capture.frame()?;
        let payload = encode_frame(sequence, &frame, options.jpeg_quality)?;
        writer.write_message(MessageKind::Frame, &payload)?;
        writer.flush()?;
        sequence = sequence.wrapping_add(1);
        report_frames += 1;

        let report_elapsed = report_started.elapsed();
        if report_elapsed >= Duration::from_secs(5) {
            eprintln!(
                "jwm-remote: sent {:.1} fps, latest JPEG {} KiB",
                report_frames as f64 / report_elapsed.as_secs_f64(),
                payload.len().div_ceil(1024)
            );
            report_started = Instant::now();
            report_frames = 0;
        }
    }
    Ok(())
}

fn receive_input(
    mut reader: SessionReader<TcpStream>,
    mut injector: Option<InputInjector>,
    keyboard_enabled: bool,
    verified_keymap: Option<[u8; 32]>,
    running: &AtomicBool,
) -> RemoteResult<()> {
    let session_result = (|| -> RemoteResult<()> {
        while running.load(Ordering::Acquire) {
            let (kind, payload) = reader.read_message()?;
            match kind {
                MessageKind::Pointer
                | MessageKind::Key
                | MessageKind::Button
                | MessageKind::ReleaseAll => {
                    let event = decode_input(kind, &payload)?;
                    if matches!(event, super::x11_input::InputEvent::Key { .. })
                        && !keyboard_enabled
                    {
                        return Err(invalid_data("keyboard input was not negotiated").into());
                    }
                    if let Some(injector) = injector.as_mut() {
                        if matches!(event, super::x11_input::InputEvent::Key { .. }) {
                            verify_injector_keymap(injector, verified_keymap)?;
                        }
                        injector.inject(event)?;
                    }
                }
                MessageKind::Close => break,
                MessageKind::Heartbeat if payload.is_empty() => {
                    if keyboard_enabled && let Some(injector) = injector.as_ref() {
                        verify_injector_keymap(injector, verified_keymap)?;
                    }
                }
                MessageKind::Heartbeat => {
                    return Err(invalid_data("client heartbeat payload must be empty").into());
                }
                MessageKind::Hello | MessageKind::HelloAck | MessageKind::Frame => {
                    return Err(invalid_data(format!("unexpected client message: {kind:?}")).into());
                }
            }
        }
        Ok(())
    })();

    let release_result = match injector.as_mut() {
        Some(injector) => injector.release_all(),
        None => Ok(()),
    };
    running.store(false, Ordering::Release);
    match (session_result, release_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => {
            eprintln!(
                "jwm-remote: failed to release remote input after session error: {release_error}"
            );
            Err(error)
        }
    }
}

fn verify_injector_keymap(
    injector: &InputInjector,
    expected: Option<[u8; 32]>,
) -> RemoteResult<()> {
    if !injector.take_keymap_change()? {
        return Ok(());
    }
    let expected = expected.ok_or_else(|| invalid_data("keyboard input has no verified keymap"))?;
    if injector.keymap_fingerprint()? != expected {
        return Err(
            invalid_data("host X11 keymap changed; reconnect to re-verify keyboard input").into(),
        );
    }
    Ok(())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn permission_denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_must_explicitly_opt_into_lan_and_input() {
        let options = HostOptions {
            listen: "127.0.0.1:48221".into(),
            key_file: PathBuf::from("key"),
            display: None,
            fps: 12,
            jpeg_quality: 70,
            max_width: 1280,
            capture_source: CaptureSource::Auto,
            allow_lan: false,
            allow_input: false,
            once: false,
        };
        assert!(!options.allow_lan);
        assert!(!options.allow_input);
    }
}
