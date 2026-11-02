use super::*;
use crate::AlphaMode;
use std::os::unix::fs::symlink;
use std::sync::atomic::AtomicUsize;

static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    directory: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let suffix = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "desktop-visual-test-{}-{suffix}",
            std::process::id()
        ));
        DirBuilder::new().mode(0o700).create(&directory).unwrap();
        Self { directory }
    }

    fn path(&self) -> PathBuf {
        self.directory.join("visual.sock")
    }

    fn server(&self) -> Server {
        Server::bind_with_timeouts(self.path(), output(), test_timeouts()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.directory).unwrap();
    }
}

fn output() -> OutputSize {
    OutputSize {
        width: 1920,
        height: 1080,
    }
}

fn test_timeouts() -> Timeouts {
    Timeouts {
        io_poll: Duration::from_millis(10),
        accept_poll: Duration::from_millis(2),
        frame: Duration::from_millis(150),
        idle: Duration::from_millis(350),
    }
}

fn connect(path: &Path) -> (UnixStream, Hello) {
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut hello = [0; HEADER_BYTES];
    stream.read_exact(&mut hello).unwrap();
    (stream, Hello::parse(&hello).unwrap())
}

fn header(sequence: u64) -> Header {
    Header {
        width: 2,
        height: 2,
        stride: 8,
        alpha: AlphaMode::Straight,
        sequence,
        timestamp_ns: 100,
    }
}

fn publish(stream: &mut UnixStream, sequence: u64) {
    stream
        .write_all(&header(sequence).encode().unwrap())
        .unwrap();
    stream
        .write_all(&[1, 2, 3, 255, 4, 5, 6, 128, 7, 8, 9, 0, 10, 11, 12, 64])
        .unwrap();
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !predicate() {
        assert!(Instant::now() < deadline, "condition did not become true");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn take_frame(server: &Server) -> Frame {
    let mut result = None;
    wait_until(|| {
        result = server.try_take_latest();
        result.is_some()
    });
    result.unwrap()
}

fn expect_disconnect(stream: &mut UnixStream) {
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
        result => panic!("expected disconnect, got {result:?}"),
    }
}

#[test]
fn complete_frames_preserve_pixels_and_compact_padding() {
    let fixture = Fixture::new();
    let server = fixture.server();
    let (mut stream, hello) = connect(&fixture.path());
    assert_eq!(hello.output, output());
    wait_until(|| server.connected());
    let header = Header {
        stride: 12,
        ..header(1)
    };
    stream.write_all(&header.encode().unwrap()).unwrap();
    stream
        .write_all(&[
            1, 2, 3, 4, 5, 6, 7, 8, 90, 91, 92, 93, 9, 10, 11, 12, 13, 14, 15, 16, 94, 95, 96, 97,
        ])
        .unwrap();
    let frame = take_frame(&server);
    assert_eq!(frame.header, header);
    assert_eq!(frame.rgba, (1..=16).collect::<Vec<u8>>());
}

#[test]
fn latest_wins_instead_of_growing_a_queue() {
    let fixture = Fixture::new();
    let server = fixture.server();
    let (mut stream, _) = connect(&fixture.path());
    for sequence in 1..=100 {
        publish(&mut stream, sequence);
    }
    wait_until(|| {
        server
            .shared
            .latest
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(_, frame)| frame.header.sequence == 100)
    });
    assert_eq!(take_frame(&server).header.sequence, 100);
    assert!(server.try_take_latest().is_none());
}

#[test]
fn render_handoff_never_waits_for_the_worker_mutex() {
    let fixture = Fixture::new();
    let server = fixture.server();
    let guard = server.shared.latest.lock().unwrap();
    let start = Instant::now();
    assert!(server.try_take_latest().is_none());
    assert!(start.elapsed() < Duration::from_millis(50));
    drop(guard);
}

#[test]
fn waker_and_pending_cover_frames_and_disconnects() {
    let fixture = Fixture::new();
    let server = fixture.server();
    let calls = Arc::new(AtomicUsize::new(0));
    let thread_calls = calls.clone();
    server.set_waker(Arc::new(move || {
        thread_calls.fetch_add(1, Ordering::Release);
    }));
    assert!(server.has_pending());
    assert!(server.try_take_latest().is_none());
    assert!(!server.has_pending());
    let (mut stream, _) = connect(&fixture.path());
    publish(&mut stream, 1);
    assert_eq!(take_frame(&server).header.sequence, 1);
    drop(stream);
    wait_until(|| !server.connected() && server.has_pending());
    assert!(server.try_take_latest().is_none());
    assert!(!server.has_pending());
    assert!(calls.load(Ordering::Acquire) >= 3);
}

#[test]
fn malformed_header_disconnects_then_valid_producer_reconnects() {
    let fixture = Fixture::new();
    let server = fixture.server();
    for offset in [0, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48] {
        let (mut stream, _) = connect(&fixture.path());
        let mut bytes = header(1).encode().unwrap();
        if offset == 44 {
            bytes[offset] = 1;
        } else {
            bytes[offset] = 0;
        }
        stream.write_all(&bytes).unwrap();
        expect_disconnect(&mut stream);
    }
    let (mut stream, _) = connect(&fixture.path());
    publish(&mut stream, 1);
    assert_eq!(take_frame(&server).header.sequence, 1);
}

#[test]
fn repeated_and_backwards_sequences_are_rejected() {
    let fixture = Fixture::new();
    let server = fixture.server();
    for next in [2, 1] {
        let (mut stream, _) = connect(&fixture.path());
        publish(&mut stream, 2);
        assert_eq!(take_frame(&server).header.sequence, 2);
        stream.write_all(&header(next).encode().unwrap()).unwrap();
        expect_disconnect(&mut stream);
        wait_until(|| !server.connected());
    }
}

#[test]
fn partial_header_or_payload_never_publishes() {
    let fixture = Fixture::new();
    let server = fixture.server();
    for payload_bytes in [0, 5, HEADER_BYTES, HEADER_BYTES + 7] {
        let (mut stream, _) = connect(&fixture.path());
        let mut wire = header(1).encode().unwrap().to_vec();
        wire.extend_from_slice(&[255; 16]);
        stream.write_all(&wire[..payload_bytes]).unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
        expect_disconnect(&mut stream);
        assert!(server.try_take_latest().is_none());
    }
}

#[test]
fn slow_partial_frame_cannot_extend_the_overall_deadline() {
    let fixture = Fixture::new();
    let server = fixture.server();
    let (mut stream, _) = connect(&fixture.path());
    stream.write_all(&header(1).encode().unwrap()).unwrap();
    stream.write_all(&[0]).unwrap();
    std::thread::sleep(Duration::from_millis(90));
    stream.write_all(&[1]).unwrap();
    let start = Instant::now();
    expect_disconnect(&mut stream);
    assert!(start.elapsed() < Duration::from_millis(140));
    assert!(server.try_take_latest().is_none());
    let (mut stream, _) = connect(&fixture.path());
    publish(&mut stream, 1);
    assert_eq!(take_frame(&server).header.sequence, 1);
}

#[test]
fn idle_timeout_releases_the_producer_slot() {
    let fixture = Fixture::new();
    let server = fixture.server();
    let (mut stream, _) = connect(&fixture.path());
    expect_disconnect(&mut stream);
    wait_until(|| !server.connected());
    let (mut replacement, _) = connect(&fixture.path());
    publish(&mut replacement, 1);
    assert_eq!(take_frame(&server).header.sequence, 1);
}

#[test]
fn canvas_resize_invalidates_old_frames_and_renegotiates() {
    let fixture = Fixture::new();
    let server = fixture.server();
    let (mut stream, _) = connect(&fixture.path());
    publish(&mut stream, 1);
    wait_until(|| server.shared.latest.lock().unwrap().is_some());
    server.set_output_size(output()).unwrap();
    assert!(server.connected());
    let resized = OutputSize {
        width: 3840,
        height: 2160,
    };
    server.set_output_size(resized).unwrap();
    assert!(!server.connected());
    assert!(server.try_take_latest().is_none());
    expect_disconnect(&mut stream);
    let (mut stream, hello) = connect(&fixture.path());
    assert_eq!(hello.output, resized);
    publish(&mut stream, 1); // sequence resets only on a fresh connection
    assert_eq!(take_frame(&server).header.sequence, 1);
    assert!(
        server
            .set_output_size(OutputSize {
                width: 0,
                height: 5
            })
            .is_err()
    );
    assert!(
        server
            .set_output_size(OutputSize {
                width: 65_536,
                height: 5
            })
            .is_err()
    );
    assert!(server.connected());
}

#[test]
fn start_stop_is_bounded_with_idle_or_partial_producer() {
    let fixture = Fixture::new();
    for partial in [false, true] {
        let server = fixture.server();
        let (mut stream, _) = connect(&fixture.path());
        if partial {
            stream.write_all(b"D").unwrap();
        }
        let start = Instant::now();
        drop(server);
        assert!(start.elapsed() < Duration::from_millis(250));
        assert!(!fixture.path().exists());
        expect_disconnect(&mut stream);
    }
}

#[test]
fn cleanup_preserves_replacement_path_and_never_connects_to_it() {
    let fixture = Fixture::new();
    let server = fixture.server();
    fs::remove_file(fixture.path()).unwrap();
    let replacement = UnixListener::bind(fixture.path()).unwrap();
    replacement.set_nonblocking(true).unwrap();
    drop(server);
    assert!(fixture.path().exists());
    assert_eq!(
        replacement.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
}

#[test]
fn active_endpoint_is_not_unlinked_but_stale_endpoint_is_reclaimed() {
    let fixture = Fixture::new();
    let listener = UnixListener::bind(fixture.path()).unwrap();
    let original = identity(&fs::symlink_metadata(fixture.path()).unwrap());
    let error = Server::bind(fixture.path(), output()).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    assert_eq!(
        identity(&fs::symlink_metadata(fixture.path()).unwrap()),
        original
    );
    drop(listener);
    let server = fixture.server();
    let (_stream, hello) = connect(&fixture.path());
    assert_eq!(hello.output, output());
    drop(server);
    assert!(!fixture.path().exists());
}

#[test]
fn unsafe_parent_and_symlink_or_regular_file_endpoints_are_rejected() {
    let fixture = Fixture::new();
    fs::set_permissions(&fixture.directory, fs::Permissions::from_mode(0o770)).unwrap();
    assert_eq!(
        Server::bind(fixture.path(), output()).err().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    fs::set_permissions(&fixture.directory, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(fixture.path(), b"preserve me").unwrap();
    assert!(Server::bind(fixture.path(), output()).is_err());
    assert_eq!(fs::read(fixture.path()).unwrap(), b"preserve me");
    fs::remove_file(fixture.path()).unwrap();
    symlink("missing-target", fixture.path()).unwrap();
    assert!(Server::bind(fixture.path(), output()).is_err());
    assert!(
        fs::symlink_metadata(fixture.path())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let alias = fixture.directory.join("alias");
    symlink(&fixture.directory, &alias).unwrap();
    assert!(Server::bind(alias.join("other.sock"), output()).is_err());
}

#[test]
fn missing_final_parent_is_created_private_and_owned() {
    let fixture = Fixture::new();
    let directory = fixture.directory.join("new");
    let path = directory.join("visual.sock");
    let server = Server::bind(path.clone(), output()).unwrap();
    assert_eq!(fs::metadata(&directory).unwrap().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(&path).unwrap().uid(), current_uid());
    drop(server);
}

fn mock_shared() -> Shared {
    Shared {
        stop: AtomicBool::new(false),
        connected: AtomicBool::new(false),
        output: AtomicU64::new(output().packed()),
        generation: AtomicU64::new(1),
        connection_generation: AtomicU64::new(0),
        connection_epoch: AtomicU64::new(0),
        latest: Mutex::new(None),
        revision: AtomicU64::new(0),
        observed_revision: AtomicU64::new(0),
        waker: OnceLock::new(),
    }
}

struct FragmentedReader {
    bytes: std::io::Cursor<Vec<u8>>,
    interrupted: bool,
    would_block: bool,
}

impl Read for FragmentedReader {
    fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(io::ErrorKind::Interrupted.into());
        }
        if !self.would_block {
            self.would_block = true;
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let count = destination.len().min(3);
        self.bytes.read(&mut destination[..count])
    }
}

#[test]
fn mock_fragmented_io_handles_interrupted_reads_without_losing_bytes() {
    let shared = mock_shared();
    let source = header(1).encode().unwrap().to_vec();
    let mut reader = FragmentedReader {
        bytes: std::io::Cursor::new(source.clone()),
        interrupted: false,
        would_block: false,
    };
    let mut destination = [0; HEADER_BYTES];
    let mut start = None;
    read_bounded(
        &mut reader,
        &mut destination,
        &shared,
        1,
        Instant::now(),
        &mut start,
        test_timeouts(),
    )
    .unwrap();
    assert_eq!(destination.as_slice(), source);
    assert!(start.is_some());
}

#[test]
fn mock_read_rejects_truncation_stop_resize_and_elapsed_deadlines() {
    let shared = mock_shared();
    let mut destination = [0; HEADER_BYTES];
    let mut start = None;
    let error = read_bounded(
        &mut std::io::Cursor::new([1; 7]),
        &mut destination,
        &shared,
        1,
        Instant::now(),
        &mut start,
        test_timeouts(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    let mut start = Some(Instant::now() - Duration::from_secs(1));
    let error = read_bounded(
        &mut std::io::Cursor::new([1; HEADER_BYTES]),
        &mut destination,
        &shared,
        1,
        Instant::now(),
        &mut start,
        test_timeouts(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let mut start = None;
    let error = read_bounded(
        &mut std::io::Cursor::new([1; HEADER_BYTES]),
        &mut destination,
        &shared,
        1,
        Instant::now() - Duration::from_secs(1),
        &mut start,
        test_timeouts(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    let error = read_bounded(
        &mut std::io::Cursor::new([1; HEADER_BYTES]),
        &mut destination,
        &shared,
        2,
        Instant::now(),
        &mut start,
        test_timeouts(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    shared.stop.store(true, Ordering::Release);
    let error = read_bounded(
        &mut std::io::Cursor::new([1; HEADER_BYTES]),
        &mut destination,
        &shared,
        1,
        Instant::now(),
        &mut start,
        test_timeouts(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
}

#[test]
fn mock_successful_final_read_cannot_publish_past_frame_deadline() {
    struct SlowFinalRead;
    impl Read for SlowFinalRead {
        fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(20));
            destination.fill(1);
            Ok(destination.len())
        }
    }
    let shared = mock_shared();
    let mut destination = [0; 4];
    let mut start = Some(Instant::now());
    let timeouts = Timeouts {
        frame: Duration::from_millis(10),
        ..test_timeouts()
    };
    let error = read_bounded(
        &mut SlowFinalRead,
        &mut destination,
        &shared,
        1,
        Instant::now(),
        &mut start,
        timeouts,
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
}

#[test]
fn mock_same_canvas_reconnect_cannot_retain_or_handoff_old_producer_frame() {
    let shared = mock_shared();
    let first = shared.begin_connection().unwrap();
    shared.connection_generation.store(1, Ordering::Release);
    shared.connected.store(true, Ordering::Release);
    *shared.latest.lock().unwrap() = Some((
        1,
        Frame {
            connection_epoch: first,
            header: header(1),
            rgba: vec![255; 16],
        },
    ));
    let displayed = shared.try_take_latest().unwrap();
    assert_eq!(displayed.connection_epoch, first);
    shared.disconnect();
    let second = shared.begin_connection().unwrap();
    shared.connection_generation.store(1, Ordering::Release);
    shared.connected.store(true, Ordering::Release);
    assert!(shared.connected());
    assert_ne!(
        displayed.connection_epoch,
        shared.connection_epoch.load(Ordering::Acquire)
    );
    assert!(second > first);
    // Even if an old publication were pending across the transition, it must
    // never be handed off as belonging to the new, currently connected peer.
    *shared.latest.lock().unwrap() = Some((
        1,
        Frame {
            connection_epoch: first,
            header: header(2),
            rgba: vec![255; 16],
        },
    ));
    assert!(shared.try_take_latest().is_none());
    *shared.latest.lock().unwrap() = Some((
        1,
        Frame {
            connection_epoch: second,
            header: header(1),
            rgba: vec![255; 16],
        },
    ));
    assert_eq!(shared.try_take_latest().unwrap().connection_epoch, second);
}

#[test]
fn reconnect_without_a_frame_changes_epoch_even_with_identical_canvas() {
    let fixture = Fixture::new();
    let server = fixture.server();
    let (mut first, _) = connect(&fixture.path());
    publish(&mut first, 1);
    let displayed = take_frame(&server);
    let first_epoch = server.connection_epoch();
    assert_eq!(displayed.connection_epoch, first_epoch);
    drop(first);
    wait_until(|| !server.connected());
    let (_second, hello) = connect(&fixture.path());
    wait_until(|| server.connected());
    assert_eq!(hello.output, output());
    assert!(server.connection_epoch() > first_epoch);
    assert_ne!(displayed.connection_epoch, server.connection_epoch());
    assert!(server.try_take_latest().is_none());
}
