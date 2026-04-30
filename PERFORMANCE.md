# 性能指南

## 性能基准

### 环形缓冲区操作延迟

基于 Criterion 基准测试（单线程，无竞争）：

| 操作 | 延迟 | 说明 |
|------|------|------|
| write() | 800-1000 ns | 消息写入 |
| read() (有数据) | 100-200 ns | 消息读取 |
| read() (无数据，超时) | 1-10 ms | 进入事件等待 |
| write_command() | 600-800 ns | 命令写入 |
| read_command() (有数据) | 80-150 ns | 命令读取 |

### 同步后端性能对比

| 后端 | 唤醒延迟 | CPU开销 | 最适场景 |
|------|---------|--------|----------|
| **Futex** | 1-5 μs | 中 | 高频消息 (> 1000/s) |
| **Semaphore** | 50-200 μs | 低 | 低频消息 (< 100/s) |
| **EventFd** | 50-100 μs | 低 | 中频消息，可与epoll集成 |

### 吞吐量

| 场景 | 吞吐量 | 说明 |
|------|--------|------|
| 单向传输（Futex） | ~1.2M msg/s | 小消息，无竞争 |
| 双向通信（Commands） | ~500K msg/s | 考虑同步开销 |
| 满缓冲区写入 | ~100K msg/s | 缓冲区满，需要等待读取 |

## 性能优化策略

### 1. 缓冲区大小选择

缓冲区大小直接影响吞吐量和延迟：

```rust
// 低延迟场景：较小缓冲区，更频繁的上下文切换
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::Futex,
    Some(16),   // 最小缓冲区
    Some(400)
)?;

// 高吞吐量场景：较大缓冲区，减少竞争
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::Futex,
    Some(256),  // 较大缓冲区
    Some(100)
)?;

// 一般推荐：64-128
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::Futex,
    Some(64),   // 平衡方案
    None
)?;
```

**影响:**
- 缓冲区大小加倍 → 满概率减半 → 吞吐量提升 ~40-60%
- 缓冲区大小加倍 → 共享内存增加 ~5-10KB

### 2. 轮询参数调优

自适应轮询参数 `adaptive_poll_spins` 对延迟影响很大：

```rust
// 极低延迟模式：强力轮询
Some(2000)   // 延迟 < 20 μs，CPU开销高
// 场景: 金融交易、实时控制

// 低延迟模式：适度轮询
Some(400)    // 延迟 100-500 μs，CPU开销中等
// 场景: 音视频处理、游戏

// 普通模式：较少轮询
Some(100)    // 延迟 1-5 ms，CPU开销较低
// 场景: 一般IPC、日志系统

// 节能模式：几乎不轮询
Some(10)     // 延迟 5-50 ms，CPU开销很低
// 场景: 低频通知、监控系统
```

**轮询开销估计:**
```
CPU_time_per_spin ≈ 1-2 ns (取决于CPU)
400 spins × 1.5 ns = 600 ns ≈ 0.6 μs
```

### 3. 同步策略选择

不同应用场景选择合适的同步策略：

#### Futex（推荐用于高频）

```rust
use shared_structures::SyncStrategy;

// 高频场景：使用Futex
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::Futex,  // 最低延迟
    Some(64),
    Some(400)
)?;

// 适用：消息频率 > 1000/s，对延迟敏感
```

**优化建议:**
- 增加轮询次数以减少sleep
- 使用较小的缓冲区，减少锁定等待时间

#### EventFd（推荐用于中频）

```rust
// 中频场景：使用EventFd
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::EventFd,  // 平衡方案
    Some(64),
    Some(100)
)?;

// 适用：消息频率 10-1000/s，可与epoll集成
```

**优化建议:**
- 与epoll/select集成进行多路复用
- 批量处理多个缓冲区上的数据

#### Semaphore（推荐用于低频）

```rust
// 低频场景：使用Semaphore
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::Semaphore,  // 最节能
    Some(16),
    Some(10)
)?;

// 适用：消息频率 < 100/s，跨平台需求
```

**优化建议:**
- 使用较小缓冲区
- 最小化轮询次数

### 4. 消息结构优化

SharedMessage的大小固定为~5KB。优化使用方式：

```rust
// 好：复用消息结构，避免重复创建
let mut msg = SharedMessage::new();
for i in 0..10000 {
    msg.get_monitor_info_mut().monitor_num = i;
    buffer.write(&msg)?;
}

// 不好：每次创建新消息
for i in 0..10000 {
    let mut msg = SharedMessage::new();
    msg.get_monitor_info_mut().monitor_num = i;
    buffer.write(&msg)?;
}
```

**内存布局优化:**
```rust
// 避免频繁的字符串操作
msg.get_monitor_info_mut().set_client_name("app");  // 仅需一次

// 预设置字符串，降低每条消息的开销
// set_client_name涉及内存复制，避免多次调用
```

### 5. 批处理策略

虽然是SPSC，但可以在单条消息中携带多项数据：

```rust
use shared_structures::{SharedRingBuffer, SharedMessage};

// 方案1：单条消息多数据
let mut msg = SharedMessage::new();
let info = msg.get_monitor_info_mut();
info.monitor_num = 1;
info.monitor_width = 1920;
info.monitor_height = 1080;
// 一次写入多个相关信息
buffer.write(&msg)?;

// 方案2：序列化多个值（如需要）
// 在client_name字段中存储编码数据
info.set_client_name("value1|value2|value3");

// 方案3：利用tag_status存储位标志信息
for i in 0..9 {
    info.set_tag_status(i, TagStatus::new(
        (data >> i) & 1 == 1, false, false, false
    ));
}
```

## 基准测试说明

### 运行基准测试

```bash
# 完整基准测试套件
cargo bench --release

# 仅运行ring_buffer_bench
cargo bench --bench ring_buffer_bench --release

# 仅运行压力测试（10M消息）
cargo bench --bench stress_test --release

# 带火焰图的基准测试
CARGO_PROFILE_BENCH_DEBUG=true cargo bench --bench ring_buffer_bench --release
```

### 基准测试指标

Criterion生成的报告包含：

```
write (no contention)
  time: [850.5 ns 851.2 ns 852.1 ns]
  √ 0.05% (std dev)

read (with data)
  time: [123.4 ns 124.1 ns 124.8 ns]
  √ 0.06% (std dev)

signal overhead
  time: [2.1 us 2.2 us 2.3 us]
  √ 0.4% (std dev)
```

**指标含义:**
- **time**: 平均执行时间（下界-中位数-上界）
- **√ x%**: 标准差百分比（越小越稳定）

### 解读结果

| 对比 | 含义 | 影响 |
|------|------|------|
| 两条线重叠 | 性能无显著变化 | 说明改进无效或误差范围内 |
| 新线明显右移 | 性能下降 | 需要调查原因 |
| 新线明显左移 | 性能提升 | 优化成功 |

## 常见瓶颈和解决方案

### 瓶颈1：缓冲区频繁满

**症状:** 写入操作经常返回 `WouldBlock`，吞吐量低于预期

**原因:**
- 消费者处理速度慢
- 缓冲区太小

**解决方案:**
```rust
// 1. 增加缓冲区大小
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::Futex,
    Some(256),  // 从64增加到256
    None
)?;

// 2. 优化消费者处理
// 使用更高效的消息处理算法

// 3. 监控队列深度
let write_idx = buffer.get_write_index();
let read_idx = buffer.get_read_index();
let queue_depth = write_idx.wrapping_sub(read_idx);
if queue_depth > buffer_size * 80 / 100 {
    println!("Warning: queue 80% full");
}
```

### 瓶颈2：高CPU占用

**症状:** 进程CPU使用率接近100%

**原因:**
- 轮询次数太多
- 活跃轮询造成的忙等待

**解决方案:**
```rust
// 1. 降低轮询次数
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::Futex,
    Some(64),
    Some(100)  // 从400降低到100
)?;

// 2. 改用更节能的后端
use shared_structures::SyncStrategy;
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::EventFd,  // 改用EventFd
    Some(64),
    Some(50)
)?;

// 3. 增加处理批大小，减少轮询频率
loop {
    if let Ok(msg) = buffer.try_read() {
        process_batch(&msg);
    } else {
        std::thread::sleep(Duration::from_millis(1));
    }
}
```

### 瓶颈3：延迟不稳定

**症状:** P99延迟很高，但平均延迟正常

**原因:**
- 动态调频CPU
- 其他进程抢占
- 垃圾回收或内存分配

**解决方案:**
```bash
# 1. 禁用CPU动态调频
echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor

# 2. 使用CPU亲和性
taskset -c 0 ./my_app

# 3. 增加Rust栈大小，减少栈溢出开销
RUST_MIN_STACK=8388608 ./my_app

# 4. 启用实时调度（需要权限）
sudo chrt -f 50 ./my_app
```

### 瓶颈4：缓存局部性差

**症状:** 性能随着消息频率增加而低于预期

**原因:**
- 缓冲区超出L3缓存
- 多核间的缓存一致性开销

**解决方案:**
```rust
// 1. 减小缓冲区大小（让数据留在缓存）
let buffer = SharedRingBuffer::create(
    "/tmp/ipc",
    SyncStrategy::Futex,
    Some(16),  // 较小缓冲区，更好的缓存局部性
    None
)?;

// 2. 使用numactl绑定内存和CPU
// numactl --cpunodebind=0 --membind=0 ./my_app

// 3. 在NUMA系统中，creator应该在实际使用的NUMA节点
// （库会自动使用creator节点）
```

## 扩展性分析

### 纵向扩展（单机多进程）

SharedRingBuffer支持一个生产者-一个消费者的SPSC模型。多进程场景：

```
生产者P1 ─┐
         ├─→ Buffer1 ─→ 消费者C1
生产者P2 ─┘

消费者C1 ─┐
         ├─→ Buffer2 ─→ 生产者P1
消费者C2 ─┘
```

**可扩展到:** 
- 10+ 个独立的缓冲区对（每对独立SPSC）
- 单机 100K+ msg/s（累计吞吐量）

### 横向扩展（多机器）

SharedRingBuffer仅支持本机共享内存，跨机器通信需要：

```rust
// 方案1：使用网络序列化
use bincode;

let msg = SharedMessage::new();
let encoded = bincode::encode_to_vec(&msg, config)?;
// 通过TCP/UDP发送编码数据
socket.send(&encoded)?;

// 方案2：代理模式
// 本机缓冲区 ← 代理进程 ← 网络
// 网络 → 代理进程 ← 本机缓冲区
```

## 功耗分析

### Futex后端

```
活跃轮询:     ~10W   (高功耗)
轮询等待:      ~8W   (中功耗)
事件等待:      ~2W   (低功耗)
```

**建议:** 调整轮询阈值平衡延迟和功耗

### EventFd后端

```
活跃轮询:     ~10W   (高功耗)
事件等待:      ~1W   (极低功耗)
```

**建议:** 倾向使用EventFd实现节能

### Semaphore后端

```
活跃轮询:     ~10W   (高功耗)
信号量等待:    ~1W   (极低功耗)
```

**建议:** 低频应用首选

## 监控和调试

### 获取性能指标

```rust
use std::time::Instant;

let start = Instant::now();
buffer.write(&msg)?;
let write_duration = start.elapsed();

println!("Write took: {:?}", write_duration);
```

### 使用perf进行性能分析

```bash
# 记录性能事件
perf record -g -F 99 ./my_app

# 生成火焰图
perf report

# 详细分析
perf stat ./my_app
```

### 启用日志进行调试

```bash
# 设置日志级别
RUST_LOG=shared_structures=debug ./my_app

# 输出示例
[INFO  shared_structures] Successfully created shared ring buffer: /tmp/ipc
[DEBUG shared_structures] Header magic: 0x52494e4742554646
[DEBUG shared_structures] Version: 8
```

## 最佳实践

1. **选择合适的后端**: 根据消息频率选择Futex/EventFd/Semaphore
2. **调优轮询参数**: 平衡延迟、吞吐量和功耗
3. **合理设置缓冲区**: 64-128通常是好的起点
4. **监控队列深度**: 避免缓冲区频繁满
5. **定期基准测试**: 确保性能符合预期
6. **考虑处理器绑定**: 在NUMA或多核系统中可显著提升性能
7. **利用批处理**: 在消息中承载多项数据以提高吞吐量
8. **避免频繁分配**: 复用消息结构，减少内存管理开销

## 参考资源

- [Linux Futex](https://man7.org/linux/man-pages/man2/futex.2.html)
- [EventFd](https://man7.org/linux/man-pages/man2/eventfd.2.html)
- [POSIX Semaphores](https://man7.org/linux/man-pages/man3/sem_overview.3.html)
- [Criterion.rs基准测试](https://bheisler.github.io/criterion.rs/book/)
- [Perf性能分析](https://perf.wiki.kernel.org/index.php/Main_Page)
