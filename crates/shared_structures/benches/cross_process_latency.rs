//! 跨进程唤醒延迟基准：真实两进程 ping-pong。
//!
//! 其余基准都在单进程多线程内计时，而本 crate 的真实使用形态是跨
//! 进程——进程间唤醒路径（futex/semaphore/eventfd 的 wait → signal）
//! 的延迟只有在独立地址空间、独立调度实体之间才有意义。
//!
//! 结构：父进程 create 缓冲区并 spawn 本可执行文件为 echo 子进程
//! （通过环境变量进入子模式）；每轮往返 = 父写消息 → 子收到后回 ack
//! 命令 → 父收到 ack。计时覆盖完整往返（两次唤醒 + 两次复制）。

use criterion::{BenchmarkId, Criterion};
use shared_structures::{
    SharedCommand, SharedMessage, SharedRingBuffer, SharedRingBufferOptions, SyncStrategy,
};
use std::env;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHILD_ENV: &str = "SRB_LATENCY_CHILD";
const PATH_ENV: &str = "SRB_LATENCY_PATH";
const SPINS_ENV: &str = "SRB_LATENCY_SPINS";

/// (基准名后缀, 自适应自旋次数)。
///
/// `adaptive_spin`：双方都有默认自旋预算，对端在自旋窗口内响应时几乎
/// 不进内核，测的是轮询快路径延迟；`kernel_wake`：自旋为 0，双方每次
/// 等待都直接进入内核（futex_wait/sem_wait/poll），测的是真实跨进程
/// 唤醒延迟——这是三个后端差异所在。
const MODES: &[(&str, u32)] = &[("adaptive_spin", 400), ("kernel_wake", 0)];
const CHILD_STEP_TIMEOUT: Duration = Duration::from_secs(30);

const COMPILED_BACKENDS: &[(&str, SyncStrategy)] = &[
    #[cfg(feature = "futex")]
    ("futex", SyncStrategy::Futex),
    #[cfg(feature = "semaphore")]
    ("semaphore", SyncStrategy::Semaphore),
    #[cfg(feature = "eventfd")]
    ("eventfd", SyncStrategy::EventFd),
];

fn unique_path(backend: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    env::temp_dir()
        .join(format!(
            "srb_latency_{}_{backend}_{nanos}",
            std::process::id()
        ))
        .into_os_string()
        .into_string()
        .expect("temp dir is valid UTF-8")
}

/// 子进程：收到一条消息就回一条 ack 命令，直到缓冲区被销毁。
fn run_child() {
    let path = env::var(PATH_ENV).expect("parent did not provide the buffer path");
    let spins = env::var(SPINS_ENV)
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let buffer = {
        let deadline = Instant::now() + CHILD_STEP_TIMEOUT;
        loop {
            match SharedRingBuffer::open_auto(&path, spins) {
                Ok(buffer) => break buffer,
                Err(error) if Instant::now() < deadline => {
                    let _ = error;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("echo child could not open buffer: {error}"),
            }
        }
    };

    loop {
        match buffer.read_message_timeout(Some(CHILD_STEP_TIMEOUT)) {
            Ok(Some(_message)) => loop {
                match buffer.try_send_command(SharedCommand::view_tag(1, 0)) {
                    Ok(true) => break,
                    Ok(false) => std::thread::yield_now(),
                    Err(_) => return, // destroyed
                }
            },
            // 超时或销毁：退出。基准侧 destroy 后这里返回 None。
            Ok(None) | Err(_) => return,
        }
    }
}

struct EchoFixture {
    buffer: SharedRingBuffer,
    child: Child,
    path: String,
}

impl EchoFixture {
    fn start(strategy: SyncStrategy, spins: u32) -> Self {
        let path = unique_path(strategy.as_str());
        let _ = std::fs::remove_file(&path);
        let buffer = SharedRingBufferOptions::new()
            .strategy(strategy)
            .capacity(64)
            .adaptive_poll_spins(spins)
            .create(&path)
            .expect("bench buffer create failed");
        let child = Command::new(env::current_exe().expect("current_exe"))
            .env(CHILD_ENV, "1")
            .env(PATH_ENV, &path)
            .env(SPINS_ENV, spins.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("could not spawn echo child");
        Self {
            buffer,
            child,
            path,
        }
    }

    fn round_trip(&self) {
        let message = SharedMessage::default();
        assert!(
            self.buffer
                .try_write_message(&message)
                .expect("bench message write failed"),
            "ping queue unexpectedly full"
        );
        let ack = self
            .buffer
            .receive_command_timeout(Some(CHILD_STEP_TIMEOUT))
            .expect("bench ack receive failed");
        assert!(ack.is_some(), "echo child did not acknowledge in time");
    }
}

impl Drop for EchoFixture {
    fn drop(&mut self) {
        let _ = self.buffer.destroy();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.path);
    }
}

fn bench_cross_process_ping_pong(c: &mut Criterion) {
    let mut group = c.benchmark_group("cross_process_ping_pong");
    group
        .sample_size(30)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3));

    for &(backend_name, strategy) in COMPILED_BACKENDS {
        for &(mode_name, spins) in MODES {
            let fixture = EchoFixture::start(strategy, spins);
            // 预热一轮，确保子进程完成 open 并进入 echo 循环。
            fixture.round_trip();

            group.bench_function(BenchmarkId::new(backend_name, mode_name), |b| {
                b.iter_custom(|iters| {
                    let started = Instant::now();
                    for _ in 0..iters {
                        fixture.round_trip();
                    }
                    started.elapsed()
                });
            });
            drop(fixture);
        }
    }
    group.finish();
}

// 手写 main：子进程模式必须在 criterion 解析命令行参数之前分流，
// 因此不用 criterion_main! 宏。
fn main() {
    if env::var(CHILD_ENV).is_ok() {
        run_child();
        return;
    }
    let mut criterion = Criterion::default().configure_from_args();
    bench_cross_process_ping_pong(&mut criterion);
    criterion.final_summary();
}
