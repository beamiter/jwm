// benches/ring_buffer_bench.rs
use criterion::{criterion_group, BenchmarkId, Criterion, Throughput};
use std::hint::black_box;
use std::io::ErrorKind;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use shared_structures::{SharedCommand, SharedMessage, SharedRingBuffer, SharedRingBufferOptions};

fn create_default(
    path: &str,
    capacity: Option<usize>,
    spins: Option<u32>,
) -> std::io::Result<SharedRingBuffer> {
    let mut options = SharedRingBufferOptions::new();
    if let Some(capacity) = capacity {
        options = options.capacity(capacity);
    }
    if let Some(spins) = spins {
        options = options.adaptive_poll_spins(spins);
    }
    options.create(path)
}

static PATH_NONCE: AtomicU64 = AtomicU64::new(0);

/// A process-unique shared-memory link that is also cleaned up during unwinding.
struct BenchPath(String);

impl AsRef<Path> for BenchPath {
    fn as_ref(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl Deref for BenchPath {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for BenchPath {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            if error.kind() != ErrorKind::NotFound {
                eprintln!("failed to clean up benchmark link {}: {error}", self.0);
            }
        }
    }
}

fn mk_path(name: &str) -> BenchPath {
    let nonce = PATH_NONCE.fetch_add(1, Ordering::Relaxed);
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    BenchPath(format!(
        "/tmp/shared_structures_{name}_{}_{}_{}",
        std::process::id(),
        epoch_millis,
        nonce
    ))
}

fn drain_all(buffer: &SharedRingBuffer) {
    while buffer
        .try_read_next_message()
        .expect("message drain failed")
        .is_some()
    {}
}

fn create_test_message(id: i32) -> SharedMessage {
    let mut message = SharedMessage::default();
    message.get_monitor_info_mut().monitor_num = id;
    message
        .get_monitor_info_mut()
        .set_client_name(&format!("test_client_{}", id));
    message.get_monitor_info_mut().set_ltsymbol("[]=");
    message
}

fn prebuild_messages(count: usize, base_id: i32) -> Vec<SharedMessage> {
    let mut v = Vec::with_capacity(count);
    for i in 0..count {
        v.push(create_test_message(base_id + i as i32));
    }
    v
}

// 1) 单线程写入
fn bench_single_threaded_write(c: &mut Criterion) {
    let test_path = mk_path("bench_single_write");
    let _ = std::fs::remove_file(&test_path);

    let buffer = create_default(&test_path, Some(1024), Some(0)).unwrap();
    let messages = prebuild_messages(100, 0);

    c.bench_function("single_threaded_write", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                drain_all(&buffer);

                let started = Instant::now();
                for m in &messages {
                    assert!(
                        buffer
                            .try_write_message(black_box(m))
                            .expect("message write failed"),
                        "sized benchmark queue unexpectedly filled"
                    );
                }
                elapsed += started.elapsed();

                assert_eq!(buffer.available_messages(), messages.len());
                black_box(buffer.available_messages());
            }
            elapsed
        })
    });

    drop(buffer);
    let _ = std::fs::remove_file(&test_path);
}

// 2) 单线程读取
fn bench_single_threaded_read(c: &mut Criterion) {
    let test_path = mk_path("bench_single_read");
    let _ = std::fs::remove_file(&test_path);

    let buffer = create_default(&test_path, Some(1024), Some(0)).unwrap();
    let messages = prebuild_messages(100, 10_000);

    c.bench_function("single_threaded_read", |b| {
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                drain_all(&buffer);
                for m in &messages {
                    assert!(buffer.try_write_message(m).expect("prefill failed"));
                }

                let started = Instant::now();
                let mut read = 0usize;
                while buffer
                    .try_read_next_message()
                    .expect("message read failed")
                    .is_some()
                {
                    read += 1;
                }
                elapsed += started.elapsed();

                assert_eq!(read, messages.len(), "read benchmark lost messages");
                black_box(read);
            }
            elapsed
        })
    });

    drop(buffer);
    let _ = std::fs::remove_file(&test_path);
}

// 3) 写吞吐（不同消息数）
fn bench_throughput_varying_sizes(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput_by_message_count");

    for &count in &[10usize, 100, 1000, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::new("write_messages", count),
            &count,
            |b, &count| {
                let test_path = mk_path(&format!("bench_throughput_{}", count));
                let _ = std::fs::remove_file(&test_path);

                let buffer = create_default(&test_path, Some(16_384), Some(0)).unwrap();
                let messages = prebuild_messages(count, 20_000);

                b.iter_custom(|iters| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iters {
                        drain_all(&buffer);

                        let started = Instant::now();
                        for m in &messages {
                            assert!(
                                buffer
                                    .try_write_message(black_box(m))
                                    .expect("message write failed"),
                                "sized benchmark queue unexpectedly filled"
                            );
                        }
                        elapsed += started.elapsed();

                        assert_eq!(buffer.available_messages(), messages.len());
                    }
                    elapsed
                });

                drop(buffer);
                let _ = std::fs::remove_file(&test_path);
            },
        );
    }
    group.finish();
}

// 4) 生产者-消费者（复用段与线程：一次样本内只创建一次）
// 修复点：
// - 不再让生产者在环满时调用 try_read_next_message（严格 SPSC）
// - 不再按“每轮读满 message_count”退出；改为按总目标条目数收敛（iters * message_count）
// - 用原子计数 sent/received 统计总量，消费者持续 wait+drain 直到达到目标
fn bench_producer_consumer(c: &mut Criterion) {
    let mut group = c.benchmark_group("producer_consumer");
    group.sample_size(10);

    for &spins in &[0u32, 1000, 5000, 10_000] {
        group.bench_with_input(
            BenchmarkId::new("adaptive_polling", spins),
            &spins,
            |b, &spins| {
                let test_path = mk_path(&format!("bench_pc_{}", spins));
                let _ = std::fs::remove_file(&test_path);

                b.iter_custom(|iters| {
                    // 构建共享段（生产者创建，消费者打开）
                    let producer =
                        Arc::new(create_default(&test_path, Some(2048), Some(spins)).unwrap());
                    let consumer =
                        Arc::new(SharedRingBuffer::open_auto(&test_path, Some(spins)).unwrap());

                    // 一次样本内的固定总工作量
                    let message_count_per_round = 1000usize;
                    let total_to_send = (iters as usize) * message_count_per_round;

                    // 预构建一批消息，循环复用，避免热路径分配
                    let messages = Arc::new(prebuild_messages(message_count_per_round, 30_000));

                    // 计数与启动同步
                    // The driver participates so workers cannot run before timing starts.
                    let start_barrier = Arc::new(Barrier::new(3));
                    let sent = Arc::new(AtomicU64::new(0));
                    let received = Arc::new(AtomicU64::new(0));

                    // 消费者线程：持续 wait + drain，直到收满 total_to_send
                    let cns = consumer.clone();
                    let start_c = start_barrier.clone();
                    let recv_cnt = received.clone();
                    let total_target = total_to_send as u64;
                    let h_cons = std::thread::spawn(move || {
                        start_c.wait();
                        while recv_cnt.load(Ordering::Acquire) < total_target {
                            // 避免空转，等待最多1ms
                            cns.wait_for_message(Some(Duration::from_millis(1)))
                                .expect("message wait failed");
                            // 尽可能多地读取
                            while cns
                                .try_read_next_message()
                                .expect("message read failed")
                                .is_some()
                            {
                                recv_cnt.fetch_add(1, Ordering::Release);
                                if recv_cnt.load(Ordering::Acquire) >= total_target {
                                    break;
                                }
                            }
                        }
                    });

                    // 生产者线程：严格 SPSC，只写不读，直到发满 total_to_send
                    let p = producer.clone();
                    let start_p = start_barrier.clone();
                    let sent_cnt = sent.clone();
                    let msgs = messages.clone();
                    let h_prod = std::thread::spawn(move || {
                        start_p.wait();
                        let mut idx = 0usize;
                        while sent_cnt.load(Ordering::Acquire) < total_target {
                            let m = &msgs[idx];
                            // 若满则忙等等待消费者清空（不可读！）
                            while !p.try_write_message(m).expect("message write failed") {
                                std::hint::spin_loop();
                            }
                            sent_cnt.fetch_add(1, Ordering::Release);
                            idx += 1;
                            if idx == msgs.len() {
                                idx = 0;
                            }
                        }
                    });

                    // Thread creation and fixture setup are outside the timed interval.
                    let t0 = Instant::now();
                    start_barrier.wait();
                    h_prod.join().expect("producer thread panicked");
                    h_cons.join().expect("consumer thread panicked");
                    let elapsed = t0.elapsed();

                    // Bench profiles disable debug assertions, so correctness checks must be hard.
                    assert_eq!(sent.load(Ordering::Acquire), total_to_send as u64);
                    assert_eq!(received.load(Ordering::Acquire), total_to_send as u64);

                    // 清理
                    drop(producer);
                    drop(consumer);
                    let _ = std::fs::remove_file(&test_path);

                    elapsed
                });
            },
        );
    }
    group.finish();
}

// 5) 命令往返
fn bench_command_latency(c: &mut Criterion) {
    let test_path = mk_path("bench_cmd_latency");
    let _ = std::fs::remove_file(&test_path);

    let sender = create_default(&test_path, Some(1024), Some(1000)).unwrap();
    let receiver = SharedRingBuffer::open_auto(&test_path, Some(1000)).unwrap();

    c.bench_function("command_round_trip", |b| {
        b.iter(|| {
            let command = black_box(SharedCommand::view_tag(1 << 3, 0));
            assert!(sender
                .try_send_command(command)
                .expect("command send failed"));
            assert!(
                receiver
                    .wait_for_command(Some(Duration::from_millis(5)))
                    .expect("command wait failed"),
                "command wait timed out"
            );
            black_box(
                receiver
                    .try_receive_command()
                    .expect("command receive failed")
                    .expect("notification without a command"),
            );
        })
    });

    drop(sender);
    drop(receiver);
    let _ = std::fs::remove_file(&test_path);
}

// 6) 内存布局压力
fn bench_memory_layout_efficiency(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_layout");

    for &buffer_size in &[16usize, 64, 256, 1024, 4096] {
        if (buffer_size as u32).is_power_of_two() {
            group.bench_with_input(
                BenchmarkId::new("buffer_size", buffer_size),
                &buffer_size,
                |b, &size| {
                    let test_path = mk_path(&format!("bench_layout_{}", size));
                    let _ = std::fs::remove_file(&test_path);

                    let buffer = create_default(&test_path, Some(size), Some(1000)).unwrap();
                    let prefill_msgs = prebuild_messages((size * 3) / 4, 40_000);
                    let alternation_msgs = prebuild_messages(100, 41_000);

                    b.iter(|| {
                        drain_all(&buffer);
                        // 预填充至 75%
                        for m in &prefill_msgs {
                            assert!(buffer
                                .try_write_message(black_box(m))
                                .expect("message prefill failed"));
                        }

                        // 交替读写
                        for m in &alternation_msgs {
                            buffer.try_read_next_message().expect("message read failed");
                            if !buffer
                                .try_write_message(black_box(m))
                                .expect("message write failed")
                            {
                                buffer
                                    .try_read_next_message()
                                    .expect("backpressure read failed");
                                assert!(buffer
                                    .try_write_message(black_box(m))
                                    .expect("message retry failed"));
                            }
                        }
                    });

                    drop(buffer);
                    let _ = std::fs::remove_file(&test_path);
                },
            );
        }
    }
    group.finish();
}

// 7) 突发性能
fn bench_burst_performance(c: &mut Criterion) {
    let mut group = c.benchmark_group("burst_performance");

    for &burst_size in &[10usize, 50, 100, 500] {
        group.bench_with_input(
            BenchmarkId::new("burst_write_read", burst_size),
            &burst_size,
            |b, &size| {
                let test_path = mk_path(&format!("bench_burst_{}", size));
                let _ = std::fs::remove_file(&test_path);

                let buffer = create_default(&test_path, Some(2048), Some(0)).unwrap();
                let burst_msgs = prebuild_messages(size, 50_000);

                b.iter(|| {
                    // 突发写入
                    for m in &burst_msgs {
                        assert!(buffer
                            .try_write_message(black_box(m))
                            .expect("burst write failed"));
                    }
                    // 突发读取
                    let mut read_count = 0usize;
                    while buffer
                        .try_read_next_message()
                        .expect("burst read failed")
                        .is_some()
                    {
                        read_count += 1;
                        if read_count >= size {
                            break;
                        }
                    }
                    assert_eq!(read_count, size, "burst benchmark lost messages");
                    black_box(read_count);
                });

                drop(buffer);
                let _ = std::fs::remove_file(&test_path);
            },
        );
    }
    group.finish();
}

// 8) 单条消息写→读往返延迟
fn bench_write_read_latency(c: &mut Criterion) {
    let test_path = mk_path("bench_latency");
    let _ = std::fs::remove_file(&test_path);

    let buffer = create_default(&test_path, Some(64), Some(0)).unwrap();
    let msg = create_test_message(0);

    c.bench_function("single_message_write_read_latency", |b| {
        b.iter(|| {
            assert!(buffer
                .try_write_message(black_box(&msg))
                .expect("message write failed"));
            black_box(
                buffer
                    .try_read_next_message()
                    .expect("message read failed")
                    .expect("round-trip message was unavailable"),
            );
        })
    });

    drop(buffer);
    let _ = std::fs::remove_file(&test_path);
}

// 9) read_next 与 read_latest 吞吐对比
fn bench_read_latest_vs_next(c: &mut Criterion) {
    let mut group = c.benchmark_group("read_mode");
    let msgs = prebuild_messages(64, 60_000);

    {
        let test_path = mk_path("bench_read_next");
        let _ = std::fs::remove_file(&test_path);
        let buffer = create_default(&test_path, Some(128), Some(0)).unwrap();

        group.bench_function("read_next_message", |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    drain_all(&buffer);
                    for m in &msgs {
                        assert!(buffer.try_write_message(m).expect("prefill failed"));
                    }

                    let started = Instant::now();
                    let mut read = 0usize;
                    while buffer
                        .try_read_next_message()
                        .expect("message read failed")
                        .is_some()
                    {
                        read += 1;
                    }
                    elapsed += started.elapsed();

                    assert_eq!(read, msgs.len(), "read-next benchmark lost messages");
                    black_box(read);
                }
                elapsed
            })
        });

        drop(buffer);
        let _ = std::fs::remove_file(&test_path);
    }

    {
        let test_path = mk_path("bench_read_latest");
        let _ = std::fs::remove_file(&test_path);
        let buffer = create_default(&test_path, Some(128), Some(0)).unwrap();

        group.bench_function("read_latest_message", |b| {
            b.iter_custom(|iters| {
                let mut elapsed = Duration::ZERO;
                for _ in 0..iters {
                    drain_all(&buffer);
                    for m in &msgs {
                        assert!(buffer.try_write_message(m).expect("prefill failed"));
                    }

                    let started = Instant::now();
                    let latest = buffer
                        .try_read_latest_message()
                        .expect("latest-message read failed");
                    elapsed += started.elapsed();

                    assert!(
                        latest.is_some(),
                        "prefilled queue returned no latest message"
                    );
                    assert_eq!(
                        buffer.available_messages(),
                        0,
                        "latest read did not advance to the writer"
                    );
                    black_box(latest);
                }
                elapsed
            })
        });

        drop(buffer);
        let _ = std::fs::remove_file(&test_path);
    }

    group.finish();
}

// 10) 命令通道单线程吞吐（不同命令数量）
fn bench_command_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("command_throughput");

    for &count in &[10usize, 100, 500, 1000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::new("send_receive_commands", count),
            &count,
            |b, &count| {
                let test_path = mk_path(&format!("bench_cmd_tp_{}", count));
                let _ = std::fs::remove_file(&test_path);
                let buffer = create_default(&test_path, Some(2048), Some(0)).unwrap();

                b.iter(|| {
                    while buffer
                        .try_receive_command()
                        .expect("command drain failed")
                        .is_some()
                    {}
                    let mut received = 0usize;
                    for i in 0..count {
                        let cmd = SharedCommand::view_tag(1 << (i % 9), (i % 4) as i32);
                        while !buffer
                            .try_send_command(black_box(cmd))
                            .expect("command send failed")
                        {
                            if buffer
                                .try_receive_command()
                                .expect("command receive failed")
                                .is_some()
                            {
                                received += 1;
                            }
                        }
                    }
                    while buffer
                        .try_receive_command()
                        .expect("command receive failed")
                        .is_some()
                    {
                        received += 1;
                    }
                    assert_eq!(received, count, "command benchmark lost commands");
                });

                drop(buffer);
                let _ = std::fs::remove_file(&test_path);
            },
        );
    }
    group.finish();
}

// 11) SharedRingBuffer 创建与销毁开销
fn bench_create_destroy_cost(c: &mut Criterion) {
    c.bench_function("create_destroy_ring_buffer", |b| {
        b.iter(|| {
            let path = mk_path("bench_create_destroy");
            let buf = create_default(&path, Some(64), Some(0)).unwrap();
            black_box(buf.available_messages());
            drop(buf);
        });
    });
}

// 12) 小容量缓冲区绕回压力（capacity = 1 / 2 / 4）
fn bench_small_buffer_wraparound(c: &mut Criterion) {
    let mut group = c.benchmark_group("small_buffer_wraparound");

    for &size in &[1usize, 2, 4] {
        group.bench_with_input(BenchmarkId::new("capacity", size), &size, |b, &size| {
            let test_path = mk_path(&format!("bench_small_{}", size));
            let _ = std::fs::remove_file(&test_path);
            let buffer = create_default(&test_path, Some(size), Some(0)).unwrap();
            let msg = create_test_message(77);

            b.iter(|| {
                for _ in 0..50 {
                    assert!(buffer
                        .try_write_message(black_box(&msg))
                        .expect("message write failed"));
                    black_box(
                        buffer
                            .try_read_next_message()
                            .expect("message read failed")
                            .expect("just-written message was unavailable"),
                    );
                }
            });

            drop(buffer);
            let _ = std::fs::remove_file(&test_path);
        });
    }
    group.finish();
}

// 13) available_messages / available_commands 查询开销
fn bench_availability_query(c: &mut Criterion) {
    let mut group = c.benchmark_group("availability_query");
    let test_path = mk_path("bench_avail");
    let _ = std::fs::remove_file(&test_path);
    let buffer = create_default(&test_path, Some(256), Some(0)).unwrap();

    // 半满状态下查询
    let msg = create_test_message(0);
    for _ in 0..128 {
        assert!(buffer.try_write_message(&msg).expect("prefill failed"));
    }

    group.bench_function("available_messages_half_full", |b| {
        b.iter(|| black_box(buffer.available_messages()))
    });
    group.bench_function("available_commands_empty", |b| {
        b.iter(|| black_box(buffer.available_commands()))
    });
    group.bench_function("has_message_true", |b| {
        b.iter(|| black_box(buffer.has_message()))
    });

    group.finish();
    drop(buffer);
    let _ = std::fs::remove_file(&test_path);
}

// ── 跨策略对比 ──────────────────────────────────────────────────────────────
// 以下 benchmark 在同一次 `cargo bench` 内直接对比三个同步后端的性能，
// 无需重复执行三次。每个子组内策略为 BenchmarkId 的标签。

use shared_structures::SyncStrategy;

fn enabled_strategies() -> Vec<(&'static str, SyncStrategy)> {
    vec![
        #[cfg(feature = "futex")]
        ("futex", SyncStrategy::Futex),
        #[cfg(feature = "semaphore")]
        ("semaphore", SyncStrategy::Semaphore),
        #[cfg(feature = "eventfd")]
        ("eventfd", SyncStrategy::EventFd),
    ]
}

// 1) 各策略单线程写吞吐
fn bench_strategy_write_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("strategy_write_throughput");
    let msgs = prebuild_messages(1000, 70_000);

    for (name, strategy) in enabled_strategies() {
        group.throughput(Throughput::Elements(msgs.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("backend", name),
            &strategy,
            |b, &strategy| {
                let path = mk_path(&format!("bench_strat_w_{}", name));
                let _ = std::fs::remove_file(&path);
                let buf = SharedRingBuffer::create(&path, strategy, Some(4096), Some(0)).unwrap();

                b.iter_custom(|iters| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iters {
                        drain_all(&buf);

                        let started = Instant::now();
                        for m in &msgs {
                            assert!(buf
                                .try_write_message(black_box(m))
                                .expect("message write failed"));
                        }
                        elapsed += started.elapsed();

                        assert_eq!(buf.available_messages(), msgs.len());
                    }
                    elapsed
                });

                drop(buf);
                let _ = std::fs::remove_file(&path);
            },
        );
    }
    group.finish();
}

// 2) 各策略单条往返延迟
fn bench_strategy_round_trip_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("strategy_round_trip_latency");
    let msg = create_test_message(0);

    for (name, strategy) in enabled_strategies() {
        group.bench_with_input(
            BenchmarkId::new("backend", name),
            &strategy,
            |b, &strategy| {
                let path = mk_path(&format!("bench_strat_rt_{}", name));
                let _ = std::fs::remove_file(&path);
                let buf = SharedRingBuffer::create(&path, strategy, Some(64), Some(0)).unwrap();

                b.iter(|| {
                    assert!(buf
                        .try_write_message(black_box(&msg))
                        .expect("message write failed"));
                    black_box(
                        buf.try_read_next_message()
                            .expect("message read failed")
                            .expect("round-trip message was unavailable"),
                    );
                });

                drop(buf);
                let _ = std::fs::remove_file(&path);
            },
        );
    }
    group.finish();
}

// 3) 各策略命令通道往返
fn bench_strategy_command_latency(c: &mut Criterion) {
    let mut group = c.benchmark_group("strategy_command_latency");
    let cmd = SharedCommand::view_tag(1 << 3, 0);

    for (name, strategy) in enabled_strategies() {
        group.bench_with_input(
            BenchmarkId::new("backend", name),
            &strategy,
            |b, &strategy| {
                let path = mk_path(&format!("bench_strat_cmd_{}", name));
                let _ = std::fs::remove_file(&path);
                let buf =
                    SharedRingBuffer::create(&path, strategy, Some(1024), Some(1000)).unwrap();

                b.iter(|| {
                    while buf
                        .try_receive_command()
                        .expect("command drain failed")
                        .is_some()
                    {}
                    assert!(buf
                        .try_send_command(black_box(cmd))
                        .expect("command send failed"));
                    black_box(
                        buf.try_receive_command()
                            .expect("command receive failed")
                            .expect("sent command was not available"),
                    );
                });

                drop(buf);
                let _ = std::fs::remove_file(&path);
            },
        );
    }
    group.finish();
}

// 4) 各策略 SPSC 生产者-消费者（跨线程）
fn bench_strategy_spsc_throughput(c: &mut Criterion) {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    let mut group = c.benchmark_group("strategy_spsc_throughput");
    group.sample_size(10);

    for (name, strategy) in enabled_strategies() {
        group.bench_with_input(
            BenchmarkId::new("backend", name),
            &strategy,
            |b, &strategy| {
                b.iter_custom(|iters| {
                    let path = mk_path(&format!("bench_strat_spsc_{}", name));
                    let _ = std::fs::remove_file(&path);

                    let producer = Arc::new(
                        SharedRingBuffer::create(&path, strategy, Some(2048), Some(400)).unwrap(),
                    );
                    let consumer =
                        Arc::new(SharedRingBuffer::open(&path, strategy, Some(400)).unwrap());

                    let per_round = 1000usize;
                    let total = (iters as usize) * per_round;
                    let msgs = Arc::new(prebuild_messages(per_round, 80_000));

                    let sent = Arc::new(AtomicU64::new(0));
                    let received = Arc::new(AtomicU64::new(0));
                    let barrier = Arc::new(Barrier::new(3));

                    let c2 = consumer.clone();
                    let recv = received.clone();
                    let bar_c = barrier.clone();
                    let target = total as u64;
                    let h_cons = thread::spawn(move || {
                        bar_c.wait();
                        while recv.load(Ordering::Acquire) < target {
                            c2.wait_for_message(Some(Duration::from_millis(1)))
                                .expect("message wait failed");
                            while c2
                                .try_read_next_message()
                                .expect("message read failed")
                                .is_some()
                            {
                                recv.fetch_add(1, Ordering::Release);
                                if recv.load(Ordering::Acquire) >= target {
                                    break;
                                }
                            }
                        }
                    });

                    let p2 = producer.clone();
                    let sent2 = sent.clone();
                    let msgs2 = msgs.clone();
                    let bar_p = barrier.clone();
                    let h_prod = thread::spawn(move || {
                        bar_p.wait();
                        let mut idx = 0usize;
                        while sent2.load(Ordering::Acquire) < target {
                            while !p2
                                .try_write_message(&msgs2[idx])
                                .expect("message write failed")
                            {
                                std::hint::spin_loop();
                            }
                            sent2.fetch_add(1, Ordering::Release);
                            idx = (idx + 1) % msgs2.len();
                        }
                    });

                    let t0 = Instant::now();
                    barrier.wait();
                    h_prod.join().expect("producer thread panicked");
                    h_cons.join().expect("consumer thread panicked");
                    let elapsed = t0.elapsed();

                    assert_eq!(sent.load(Ordering::Acquire), target);
                    assert_eq!(received.load(Ordering::Acquire), target);

                    drop(producer);
                    drop(consumer);
                    let _ = std::fs::remove_file(&path);

                    elapsed
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_single_threaded_write,
    bench_single_threaded_read,
    bench_throughput_varying_sizes,
    bench_producer_consumer,
    bench_command_latency,
    bench_memory_layout_efficiency,
    bench_burst_performance,
    bench_write_read_latency,
    bench_read_latest_vs_next,
    bench_command_throughput,
    bench_create_destroy_cost,
    bench_small_buffer_wraparound,
    bench_availability_query,
    bench_strategy_write_throughput,
    bench_strategy_round_trip_latency,
    bench_strategy_command_latency,
    bench_strategy_spsc_throughput,
);
#[cfg(any(feature = "futex", feature = "semaphore", feature = "eventfd"))]
criterion::criterion_main!(benches);

// `cargo test --all-targets` executes Criterion harnesses. A featureless
// build deliberately has no usable SyncStrategy, so there is nothing to
// benchmark and the target should exit successfully instead of unwrapping
// `Unsupported` from the first fixture.
#[cfg(not(any(feature = "futex", feature = "semaphore", feature = "eventfd")))]
fn main() {}
