use shared_structures::{
    CommandType, SharedCommand, SharedMessage, SharedRingBuffer, SyncStrategy,
};
use std::env;
use std::fs;
use std::io;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CHILD_MODE_ENV: &str = "SHARED_STRUCTURES_CROSS_PROCESS_CHILD";
const PATH_ENV: &str = "SHARED_STRUCTURES_CROSS_PROCESS_PATH";
const TOKEN_ENV: &str = "SHARED_STRUCTURES_CROSS_PROCESS_TOKEN";
const BACKEND_ENV: &str = "SHARED_STRUCTURES_CROSS_PROCESS_BACKEND";
const ACK_MONITOR_ID: i32 = -20_260_713;
const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_SLICE: Duration = Duration::from_millis(25);

const COMPILED_BACKENDS: &[(&str, SyncStrategy)] = &[
    #[cfg(feature = "futex")]
    ("futex", SyncStrategy::Futex),
    #[cfg(feature = "semaphore")]
    ("semaphore", SyncStrategy::Semaphore),
    #[cfg(feature = "eventfd")]
    ("eventfd", SyncStrategy::EventFd),
];

static RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct SharedPathGuard(String);

impl Drop for SharedPathGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Keeps a spawned helper bounded even when an assertion or I/O operation fails.
struct ChildGuard {
    child: Child,
    status: Option<ExitStatus>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child,
            status: None,
        }
    }

    fn poll_status(&mut self) -> io::Result<Option<&ExitStatus>> {
        if self.status.is_none() {
            self.status = self.child.try_wait()?;
        }
        Ok(self.status.as_ref())
    }

    fn wait_until(&mut self, deadline: Instant) -> Result<(), String> {
        loop {
            if let Some(status) = self.poll_status().map_err(|error| error.to_string())? {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("child helper exited with {status}"))
                };
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for child helper to exit".to_owned());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[test]
fn cross_process_message_and_command_ack() {
    // A no-backend build is a supported compile/check configuration. There is no transport to
    // exercise in that configuration, so the integration test intentionally becomes a no-op.
    for &(backend_name, strategy) in COMPILED_BACKENDS {
        if let Err(error) = run_backend_exchange(backend_name, strategy) {
            panic!("cross-process {backend_name} exchange failed: {error}");
        }
    }
}

// The parent launches this exact test in a fresh process. Keeping it ignored prevents a normal
// `cargo test` invocation from running the helper without its required environment.
#[test]
#[ignore = "launched by cross_process_message_and_command_ack"]
fn cross_process_child_helper() {
    assert!(matches!(env::var(CHILD_MODE_ENV).as_deref(), Ok("1")));

    let path = env::var(PATH_ENV).expect("parent did not provide the shared-memory path");
    let backend_name = env::var(BACKEND_ENV).expect("parent did not provide the backend name");
    let token = env::var(TOKEN_ENV)
        .expect("parent did not provide the message token")
        .parse::<u32>()
        .expect("message token is not a u32");
    let deadline = Instant::now() + PROCESS_TIMEOUT;

    let buffer = open_with_deadline(&path, deadline)
        .unwrap_or_else(|error| panic!("child could not open {backend_name} buffer: {error}"));
    receive_identified_message(&buffer, token, &backend_name, deadline)
        .unwrap_or_else(|error| panic!("child did not receive parent message: {error}"));
    send_ack(&buffer, token, deadline)
        .unwrap_or_else(|error| panic!("child could not send acknowledgement: {error}"));
}

fn run_backend_exchange(backend_name: &str, strategy: SyncStrategy) -> Result<(), String> {
    let token = unique_token();
    let path = SharedPathGuard(unique_path(backend_name, token)?);
    let _ = fs::remove_file(&path.0);

    let buffer = SharedRingBuffer::create(&path.0, strategy, Some(16), Some(0))
        .map_err(|error| format!("create failed: {error}"))?;
    let child = spawn_child(&path.0, backend_name, token)
        .map_err(|error| format!("could not spawn child helper: {error}"))?;
    let mut child = ChildGuard::new(child);
    let deadline = Instant::now() + PROCESS_TIMEOUT;

    let mut message = SharedMessage::default();
    message.get_monitor_info_mut().monitor_num = token as i32;
    message
        .get_monitor_info_mut()
        .set_client_name(&expected_client_name(backend_name, token));
    match buffer.try_write_message(&message) {
        Ok(true) => {}
        Ok(false) => return Err("newly created message queue was unexpectedly full".to_owned()),
        Err(error) => return Err(format!("parent message write failed: {error}")),
    }

    receive_ack(&buffer, &mut child, token, deadline)?;
    child.wait_until(deadline)?;

    drop(buffer);
    Ok(())
}

fn spawn_child(path: &str, backend_name: &str, token: u32) -> io::Result<Child> {
    Command::new(env::current_exe()?)
        .arg("--ignored")
        .arg("--exact")
        .arg("cross_process_child_helper")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_MODE_ENV, "1")
        .env(PATH_ENV, path)
        .env(TOKEN_ENV, token.to_string())
        .env(BACKEND_ENV, backend_name)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
}

fn open_with_deadline(path: &str, deadline: Instant) -> Result<SharedRingBuffer, String> {
    loop {
        match SharedRingBuffer::open_auto(path, Some(0)) {
            Ok(buffer) => return Ok(buffer),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::WouldBlock
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn receive_identified_message(
    buffer: &SharedRingBuffer,
    token: u32,
    backend_name: &str,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        if let Some(message) = buffer
            .try_read_next_message()
            .map_err(|error| error.to_string())?
        {
            let monitor = message.get_monitor_info();
            if monitor.monitor_num != token as i32 {
                return Err(format!(
                    "unexpected message id {}; expected {token}",
                    monitor.monitor_num
                ));
            }
            let expected_name = expected_client_name(backend_name, token);
            let actual_name = monitor.get_client_name();
            if actual_name != expected_name {
                return Err(format!(
                    "unexpected client name {actual_name:?}; expected {expected_name:?}"
                ));
            }
            return Ok(());
        }

        let wait = remaining_slice(deadline)?;
        buffer
            .wait_for_message(Some(wait))
            .map_err(|error| error.to_string())?;
    }
}

fn send_ack(buffer: &SharedRingBuffer, token: u32, deadline: Instant) -> Result<(), String> {
    let ack = SharedCommand::view_tag(token, ACK_MONITOR_ID);
    loop {
        match buffer.try_send_command(ack) {
            Ok(true) => return Ok(()),
            Ok(false) => {
                remaining_slice(deadline)?;
                thread::yield_now();
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn receive_ack(
    buffer: &SharedRingBuffer,
    child: &mut ChildGuard,
    token: u32,
    deadline: Instant,
) -> Result<(), String> {
    loop {
        if let Some(command) = buffer
            .try_receive_command()
            .map_err(|error| error.to_string())?
        {
            if command.get_command_type() != CommandType::ViewTag
                || command.get_parameter() != token
                || command.get_monitor_id() != ACK_MONITOR_ID
            {
                return Err(format!("unexpected acknowledgement: {command:?}"));
            }
            return Ok(());
        }

        if let Some(status) = child.poll_status().map_err(|error| error.to_string())? {
            if !status.success() {
                return Err(format!(
                    "child helper exited before acknowledgement with {status}"
                ));
            }
            // A successful child may exit immediately after publishing the command. Continue
            // polling until the absolute deadline so the Acquire read can observe that publish.
        }

        // Use a short backend wait to exercise command notification while retaining an absolute
        // parent-side watchdog. If a child blocks in open/wait/cleanup, the loop still reaches the
        // deadline and ChildGuard can kill and reap it deterministically.
        let remaining = remaining_slice(deadline)?;
        buffer
            .wait_for_command(Some(remaining))
            .map_err(|error| error.to_string())?;
    }
}

fn remaining_slice(deadline: Instant) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err("absolute process deadline expired".to_owned())
    } else {
        Ok(remaining.min(WAIT_SLICE))
    }
}

fn unique_token() -> u32 {
    let sequence = RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mixed = nanos ^ (u64::from(std::process::id()) << 32) ^ sequence;
    (mixed % (i32::MAX as u64 - 1) + 1) as u32
}

fn unique_path(backend_name: &str, token: u32) -> Result<String, String> {
    let path = env::temp_dir().join(format!(
        "shared_structures_cross_process_{}_{}_{}",
        std::process::id(),
        backend_name,
        token
    ));
    path.into_os_string()
        .into_string()
        .map_err(|_| "temporary directory is not valid UTF-8".to_owned())
}

fn expected_client_name(backend_name: &str, token: u32) -> String {
    format!("cross-process-{backend_name}-{token}")
}

// ── 跨进程多生产者 ────────────────────────────────────────────────────────────

const PRODUCER_BASE_ENV: &str = "SHARED_STRUCTURES_CROSS_PROCESS_PRODUCER_BASE";
const PRODUCER_COUNT_ENV: &str = "SHARED_STRUCTURES_CROSS_PROCESS_PRODUCER_COUNT";
const MULTI_PRODUCERS: usize = 3;
const PER_PRODUCER: usize = 64;

/// 三个真实进程同时向容量远小于总量的消息环写入不相交的编号区间，
/// 父进程消费全部消息并验证不重、不漏、不越界。覆盖跨进程方向锁竞争
/// 与背压（队列满）路径。
#[test]
fn cross_process_multiple_producers() {
    for &(backend_name, strategy) in COMPILED_BACKENDS {
        if let Err(error) = run_multi_producer(backend_name, strategy) {
            panic!("cross-process {backend_name} multi-producer failed: {error}");
        }
    }
}

// The parent launches this exact test in fresh processes. Keeping it ignored prevents a normal
// `cargo test` invocation from running the helper without its required environment.
#[test]
#[ignore = "launched by cross_process_multiple_producers"]
fn cross_process_producer_helper() {
    assert!(matches!(env::var(CHILD_MODE_ENV).as_deref(), Ok("1")));

    let path = env::var(PATH_ENV).expect("parent did not provide the shared-memory path");
    let base = env::var(PRODUCER_BASE_ENV)
        .expect("parent did not provide the producer base")
        .parse::<i32>()
        .expect("producer base is not an i32");
    let count = env::var(PRODUCER_COUNT_ENV)
        .expect("parent did not provide the producer count")
        .parse::<i32>()
        .expect("producer count is not an i32");
    let deadline = Instant::now() + PROCESS_TIMEOUT;

    let buffer = open_with_deadline(&path, deadline)
        .unwrap_or_else(|error| panic!("producer could not open buffer: {error}"));
    for value in base..base + count {
        let mut message = SharedMessage::default();
        message.get_monitor_info_mut().monitor_num = value;
        loop {
            match buffer.try_write_message(&message) {
                Ok(true) => break,
                Ok(false) => {
                    // 队列满：短暂退让，父进程消费后重试。
                    if Instant::now() >= deadline {
                        panic!("producer timed out on a full queue at value {value}");
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("producer write failed at value {value}: {error}"),
            }
        }
    }
}

fn run_multi_producer(backend_name: &str, strategy: SyncStrategy) -> Result<(), String> {
    let token = unique_token();
    let path = SharedPathGuard(unique_path(&format!("mp_{backend_name}"), token)?);
    let _ = fs::remove_file(&path.0);

    // 容量 16 远小于 3×64 的总量，确保覆盖队列满/背压路径。
    let buffer = SharedRingBuffer::create(&path.0, strategy, Some(16), Some(0))
        .map_err(|error| format!("create failed: {error}"))?;

    let mut children = Vec::new();
    for producer in 0..MULTI_PRODUCERS {
        let child = spawn_producer(&path.0, (producer * PER_PRODUCER) as i32)
            .map_err(|error| format!("could not spawn producer {producer}: {error}"))?;
        children.push(ChildGuard::new(child));
    }

    let deadline = Instant::now() + PROCESS_TIMEOUT;
    let total = MULTI_PRODUCERS * PER_PRODUCER;
    let mut seen = vec![false; total];
    let mut received = 0usize;
    while received < total {
        let drained = buffer
            .drain_messages(usize::MAX)
            .map_err(|error| format!("drain failed: {error}"))?;
        if drained.is_empty() {
            let wait = remaining_slice(deadline)?;
            buffer
                .wait_for_message(Some(wait))
                .map_err(|error| error.to_string())?;
            continue;
        }
        for message in drained {
            let value = message.get_monitor_info().monitor_num;
            let index = usize::try_from(value)
                .ok()
                .filter(|&index| index < total)
                .ok_or_else(|| format!("received out-of-range value {value}"))?;
            if seen[index] {
                return Err(format!("received duplicate value {value}"));
            }
            seen[index] = true;
            received += 1;
        }
    }

    for mut child in children {
        child.wait_until(deadline)?;
    }
    drop(buffer);
    Ok(())
}

fn spawn_producer(path: &str, base: i32) -> io::Result<Child> {
    Command::new(env::current_exe()?)
        .arg("--ignored")
        .arg("--exact")
        .arg("cross_process_producer_helper")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(CHILD_MODE_ENV, "1")
        .env(PATH_ENV, path)
        .env(PRODUCER_BASE_ENV, base.to_string())
        .env(PRODUCER_COUNT_ENV, PER_PRODUCER.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
}
