# Shared Structures

`shared_structures` 是面向 Linux 的共享内存 IPC crate。它提供固定容量的消息环和命令环、可选择的 Futex/Semaphore/EventFd 等待后端，以及可验证的自描述共享协议。

当前队列把领域值编码到固定大小的 wire slot，并在写入和读取时复制该 slot。传输过程不使用 serde/rkyv 序列化，但也不是借用共享槽位的零复制 API。

## 主要能力

- `TypedRingBuffer<M, C>`：泛型双通道环形缓冲核心。任意满足 `WireSafe` 契约（repr(C)、无 padding、任意位模式有效）的 POD 类型都可作为槽位类型；固定宽度整数、浮点及其数组自带实现。
- `SharedRingBuffer`：`TypedRingBuffer` 的领域封装，消息/命令双向队列；安全 API 支持多个生产者和消费者，每个方向分别串行化，并针对无竞争 SPSC 快路径优化。
- `SharedMessage`、`MonitorInfo`、`TagStatus`：固定上限的监控消息领域类型。
- `SharedCommand`、`CommandType`：查看标签、切换标签和设置布局等命令。
- `SharedRingBufferOptions`：集中配置同步策略、消息容量和自适应轮询次数。
- 协议 v11：header 记录后端、映射长度、容量、槽大小、布局标记和创建者 PID；打开时先验证再派生槽位地址。
- 崩溃恢复：方向锁存持有者 PID，持有者崩溃后被自动夺回；`reclaim_stale` 可回收创建者已死的残留映射。
- 三种 Linux 同步后端：Futex、进程共享 POSIX Semaphore 和 EventFd。
- 消息与命令校验和、显式 `destroy`、运行状态快照。

详细设计参见 [架构设计](docs/ARCHITECTURE.md)，安全前提参见 [安全说明](docs/SAFETY.md)。

## 系统要求

- Linux；
- Rust 1.86 或更新版本；
- flink 所在文件系统支持同目录硬链接（用于原子发布已初始化映射）；
- 共享同一映射的进程使用协议 v11、相同端序和兼容目标架构；
- 使用 EventFd 时，创建者需要保持 FD 传递 socket 可用，直到不再接受新 opener。

## 引入

从本地工作区使用：

```toml
[dependencies]
shared_structures = { path = "../shared_structures" }
```

默认编译 Futex、Semaphore 和 EventFd 三种后端。若只需要一种后端，可关闭默认 feature；详见[后端与 feature](#后端与-feature)。

## 快速开始

下面示例使用 builder 打开已有映射，或仅在 flink 确实不存在时创建新映射：

```rust
use shared_structures::{
    SharedCommand, SharedMessage, SharedRingBufferOptions, SyncStrategy,
};
use std::io;
use std::time::Duration;

fn main() -> io::Result<()> {
    let path = "/tmp/shared-structures-example";
    let buffer = SharedRingBufferOptions::new()
        .strategy(SyncStrategy::Futex)
        .capacity(16)
        .adaptive_poll_spins(400)
        .open_or_create(path)?;

    let mut message = SharedMessage::new();
    message.get_monitor_info_mut().set_client_name("my_app");
    message.get_monitor_info_mut().monitor_num = 1;

    // Ok(false) 表示消息环当前已满；调用者可以稍后重试。
    if !buffer.try_write_message(&message)? {
        eprintln!("message queue is full");
    }

    if buffer.wait_for_message(Some(Duration::from_millis(100)))? {
        if let Some(received) = buffer.try_read_next_message()? {
            println!("client={}", received.get_monitor_info().get_client_name());
        }
    }

    // 命令参数按值传入；Ok(false) 表示命令环已满。
    if !buffer.try_send_command(SharedCommand::view_tag(1 << 2, 0))? {
        eprintln!("command queue is full");
    }

    if buffer.wait_for_command(Some(Duration::from_millis(100)))? {
        if let Some(command) = buffer.try_receive_command()? {
            println!("command={:?}", command.get_command_type());
        }
    }

    let stats = buffer.stats();
    println!(
        "backend={}, messages={}/{}, commands={}/{}",
        stats.strategy,
        stats.available_messages,
        stats.capacity,
        stats.available_commands,
        stats.command_capacity,
    );

    // 只有确定要全局终止该映射时才调用 destroy。
    if buffer.is_creator() {
        buffer.destroy()?;
    }

    Ok(())
}
```

`open_or_create` 不会把权限错误、协议不兼容或布局损坏误当成“不存在”。这些情况会原样返回错误。创建者先在私有映射中完成初始化，再原子发布 flink；并发 opener 不会观察到半初始化 header。

## 创建与打开

推荐使用 `SharedRingBufferOptions`：

```rust
use shared_structures::{SharedRingBufferOptions, SyncStrategy};

fn create_and_open() -> std::io::Result<()> {
    let options = SharedRingBufferOptions::new()
        .strategy(SyncStrategy::Futex)
        .capacity(64)
        .adaptive_poll_spins(400);

    let creator = options.create("/tmp/my-buffer")?;
    let opener = options.open("/tmp/my-buffer")?;

    drop(opener);
    creator.destroy()?;
    Ok(())
}
```

默认选项是当前构建的默认策略、消息容量 16、命令容量 16、自适应轮询 400 次。两种容量都必须是非零的 2 的幂，分别用 `capacity(..)` 与 `command_capacity(..)` 配置；`reclaim_stale(true)` 允许 `open_or_create` 回收创建者已崩溃的残留映射（应只由单一监督者角色开启）。

自定义 payload 类型使用泛型入口（类型与容量布局记录在共享 header 中，错配打开会被拒绝）：

```rust
use shared_structures::{SharedRingBufferOptions, TypedRingBuffer, WireSafe};

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct Sample {
    sequence: u64,
    value: f64,
}
// SAFETY: repr(C)、无 padding（8+8 字节）、所有位模式有效、无内部可变性。
unsafe impl WireSafe for Sample {}

fn typed() -> std::io::Result<()> {
    let ring: TypedRingBuffer<Sample, u64> = SharedRingBufferOptions::new()
        .capacity(64)
        .create_typed("/tmp/my-typed-ring")?;
    ring.try_write_message(&Sample { sequence: 1, value: 0.5 })?;
    Ok(())
}
```

当 opener 不应预先知道后端时，使用自动识别：

```rust
use shared_structures::SharedRingBuffer;

fn inspect_existing() -> std::io::Result<()> {
    let buffer = SharedRingBuffer::open_auto("/tmp/my-buffer", None)?;
    println!("strategy={}, capacity={}", buffer.strategy(), buffer.capacity());
    Ok(())
}
```

静态便捷入口也可用于保持现有调用风格：

```rust
use shared_structures::{SharedRingBuffer, SyncStrategy};

fn open_or_create() -> std::io::Result<()> {
    let _buffer = SharedRingBuffer::open_or_create(
        "/tmp/my-buffer",
        SyncStrategy::Futex,
        Some(16),
        Some(400),
    )?;
    Ok(())
}
```

显式策略的 `open` 会验证该策略与 v11 header 一致。若当前构建没有包含映射所需后端，`open_auto` 返回 `Unsupported`，不会用另一种布局解释共享内存。

## 队列 API 结果

| 方法 | 成功结果 | 并发状态 |
| --- | --- | --- |
| `try_write_message(&message)` | `Ok(true)`：消息已提交 | `Ok(false)`：消息环已满 |
| `try_read_next_message()` | `Ok(Some(message))`：按 FIFO 读取 | `Ok(None)`：当前为空或已关闭 |
| `try_read_latest_message()` | `Ok(Some(message))`：读取最新消息并丢弃更旧待读消息 | `Ok(None)`：当前为空或已关闭 |
| `try_peek_message()` | `Ok(Some(message))`：复制最早消息但不移除 | `Ok(None)`：当前为空或已关闭 |
| `drain_messages(max)` | `Ok(Vec<..>)`：单次持锁批量读取至多 `max` 条 | 空 `Vec`：当前为空或已关闭 |
| `write_message_overwrite(&message)` | `Ok(())`：已提交（满时覆盖最旧消息） | 仅在已销毁时报错 |
| `try_send_command(command)` | `Ok(true)`：命令已提交 | `Ok(false)`：命令环已满 |
| `try_receive_command()` | `Ok(Some(command))`：读取命令 | `Ok(None)`：当前为空或已关闭 |
| `wait_message(timeout)` / `wait_command(timeout)` | `Ok(WaitOutcome::Ready)`：应重新检查队列 | `TimedOut` / `Destroyed` |
| `read_message_timeout(timeout)` | `Ok(Some(message))`：等待并读到消息 | `Ok(None)`：超时或已销毁 |
| `receive_command_timeout(timeout)` | `Ok(Some(command))`：等待并读到命令 | `Ok(None)`：超时或已销毁 |
| `wait_for_message(timeout)` / `wait_for_command(timeout)` | `Ok(true)`：应重新检查队列 | `Ok(false)`：超时或已关闭 |

等待允许伪唤醒，也可能有其他消费者先取走数据。`wait_*` 返回就绪后必须再次调用 `try_read_next_message` 或 `try_receive_command`；需要独占语义时直接使用 `read_message_timeout` / `receive_command_timeout`，它们在内部完成「等待 → 读取 → 被抢走则重试」。

`send_command`、`receive_command` 及 `*_aux` 便捷构造器已标记 deprecated，将在后续大版本移除；新代码应使用 `try_send_command`、`try_receive_command` 与 `SharedRingBufferOptions`。

## 状态与统计

`capacity()`、`command_capacity()`、`strategy()`、`is_creator()`、`creator_pid()`、`creator_alive()` 和 `last_message_timestamp()` 提供单项查询。`available_messages()`/`available_commands()` 是免锁快照，可在热路径高频调用。`stats()` 返回 `SharedRingBufferStats` 快照，包括：

- 消息容量和当前可读消息数；
- 命令容量和当前可读命令数；
- 最后消息时间戳；
- destroyed/creator 状态；
- 同步策略。

这些值是并发快照。获取快照后，其他进程可能立即改变队列状态，因此不能用它替代实际操作的返回值。

## 生命周期

- opener 的 Drop 只关闭本地句柄和后端资源，不会把共享映射标记为 destroyed。
- creator 的 Drop 执行所有者清理：标记 destroyed、唤醒等待者、停止拥有的后端服务并移除 flink。
- `destroy(&self)` 显式执行全局关闭；其他句柄随后观察到 destroyed/closed。
- `SIGKILL`、`abort`、断电等情况不会运行 Drop。持锁进程崩溃后，方向锁会被其他参与者自动夺回（详见[安全说明](docs/SAFETY.md)）；创建者崩溃留下的残留映射可用 `creator_alive()` 探测、`reclaim_stale(true)` 回收。

如果必须处理完所有已提交消息，应先停止生产、排空队列，再由所有者调用 `destroy`。不要把 Drop 当成可靠的业务级停机握手。

## 后端与 feature

| Feature | 后端 | 适用场景 |
| --- | --- | --- |
| `futex` | Linux Futex | sequence/waiter 注册握手关闭丢唤醒窗口；无等待者时避免 wake syscall。 |
| `semaphore` | 进程共享 POSIX Semaphore | 注册握手避免丢唤醒和空闲 token 累积；destroy 广播已登记等待者。 |
| `eventfd` | EventFd + Unix socket FD 传递 | `EFD_SEMAPHORE` 通知；创建者在属主私有目录内监听并以 `SO_PEERCRED` 校验对端 UID。 |

构建命令：

```bash
# 默认：三种后端
cargo build --release

# 只编译一种后端
cargo build --release --no-default-features --features futex
cargo build --release --no-default-features --features semaphore
cargo build --release --no-default-features --features eventfd
```

领域类型的 serde 与 rkyv derive 现在是可选 feature（默认关闭），仅在下游需要把 `SharedMessage` 等类型另行序列化时启用；队列传输不使用它们：

```bash
cargo build --release --features serde        # serde derive
cargo build --release --features rkyv         # rkyv derive
```

`use-futex`、`use-semaphore` 和 `use-eventfd` 是兼容旧调用入口的默认后端选择别名；每个别名会同时启用对应后端。新代码应通过 `SharedRingBufferOptions::strategy` 显式选择创建策略，通过 `open_auto` 打开未知策略的映射。

例如，只编译 Futex 并让旧的 `_aux` 构造器默认选择 Futex：

```bash
cargo build --release --no-default-features --features use-futex
```

## 固定布局与兼容性

- `MonitorInfo` 最多包含 `MAX_TAGS` 个标签状态；client name 和 layout symbol 使用固定字节数组。
- 时间戳是 Unix epoch 起的毫秒数。`SharedMessage::default()` 是零值（`timestamp == 0`）；`SharedMessage::new()` 才打当前时间戳。
- v11 映射不兼容 v10 及更早映射。升级进程必须协调重启并重建共享映射。
- 校验和用于发现撕裂写入或损坏，不提供防篡改能力。
- 共享路径不是安全边界；应放在权限受控目录中。

领域类型上的 serde/rkyv trait 与共享 wire 协议相互独立。不能用“某类型仍可反序列化”推断两个 crate 版本能够共享同一映射。

## 开发与验证

```bash
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --doc
cargo bench --all-features --no-run
```

修改协议布局或同步规则时，还应运行 feature 组合、真实跨进程、并发压力、旧协议拒绝和布局 fixture 测试。

## 文档

- [架构设计](docs/ARCHITECTURE.md)
- [安全说明](docs/SAFETY.md)
- [Benchmark 与 sanitizer](docs/BENCHMARKS.md)
- [版本变更](CHANGELOG.md)

## 许可证

本项目采用 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双许可，可任选其一使用。

除非另有明确说明，你有意提交并包含在本项目中的任何贡献（按 Apache-2.0 许可证的定义）均按上述双许可授权，不附加任何额外条款或条件。
