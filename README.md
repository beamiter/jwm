# Shared Structures

`shared_structures` 是面向 Linux 的共享内存 IPC crate。它提供固定容量的消息环和命令环、可选择的 Futex/Semaphore/EventFd 等待后端，以及可验证的自描述共享协议。

当前队列把领域值编码到固定大小的 wire slot，并在写入和读取时复制该 slot。传输过程不使用 serde/rkyv 序列化，但也不是借用共享槽位的零复制 API。

## 主要能力

- `SharedRingBuffer`：有界的共享内存消息/命令双向队列；安全 API 支持多个生产者和消费者，每个方向分别串行化，并针对无竞争 SPSC 快路径优化。
- `SharedMessage`、`MonitorInfo`、`TagStatus`：固定上限的监控消息领域类型。
- `SharedCommand`、`CommandType`：查看标签、切换标签和设置布局等命令。
- `SharedRingBufferOptions`：集中配置同步策略、消息容量和自适应轮询次数。
- 协议 v10：header 记录后端、映射长度、容量、槽大小和布局标记；打开时先验证再派生槽位地址。
- 三种 Linux 同步后端：Futex、进程共享 POSIX Semaphore 和 EventFd。
- 消息与命令校验和、显式 `destroy`、运行状态快照。

详细设计参见 [架构设计](docs/ARCHITECTURE.md)，安全前提参见 [安全说明](docs/SAFETY.md)。

## 系统要求

- Linux；
- Rust 1.86 或更新版本；
- flink 所在文件系统支持同目录硬链接（用于原子发布已初始化映射）；
- 共享同一映射的进程使用协议 v10、相同端序和兼容目标架构；
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
    if !buffer.send_command(SharedCommand::view_tag(1 << 2, 0))? {
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

默认选项是当前构建的默认策略、消息容量 16、自适应轮询 400 次。消息容量必须是非零的 2 的幂。命令容量由协议固定，目前可用 `command_capacity()` 查询。

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

显式策略的 `open` 会验证该策略与 v10 header 一致。若当前构建没有包含映射所需后端，`open_auto` 返回 `Unsupported`，不会用另一种布局解释共享内存。

## 队列 API 结果

| 方法 | 成功结果 | 并发状态 |
| --- | --- | --- |
| `try_write_message(&message)` | `Ok(true)`：消息已提交 | `Ok(false)`：消息环已满 |
| `try_read_next_message()` | `Ok(Some(message))`：按 FIFO 读取 | `Ok(None)`：当前为空或已关闭 |
| `try_read_latest_message()` | `Ok(Some(message))`：读取最新消息并丢弃更旧待读消息 | `Ok(None)`：当前为空或已关闭 |
| `send_command(command)` | `Ok(true)`：命令已提交 | `Ok(false)`：命令环已满 |
| `try_receive_command()` | `Ok(Some(command))`：读取命令 | `Ok(None)`：当前为空或已关闭 |
| `wait_for_message(timeout)` | `Ok(true)`：应重新检查消息环 | `Ok(false)`：超时或已关闭 |
| `wait_for_command(timeout)` | `Ok(true)`：应重新检查命令环 | `Ok(false)`：超时或已关闭 |

等待允许伪唤醒，也可能有其他消费者先取走数据。唤醒后必须再次调用 `try_read_next_message` 或 `try_receive_command`，不能把 `true` 当作下一次读取必然成功。

兼容方法 `receive_command() -> Option<SharedCommand>` 仍然存在，但无法报告校验错误；新代码应使用 `try_receive_command()`。

## 状态与统计

`capacity()`、`command_capacity()`、`strategy()`、`is_creator()` 和 `last_message_timestamp()` 提供单项查询。`stats()` 返回 `SharedRingBufferStats` 快照，包括：

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
- `SIGKILL`、`abort`、断电等情况不会运行 Drop。需要崩溃恢复的应用应在外层实现监督、心跳和陈旧 flink 重建。

如果必须处理完所有已提交消息，应先停止生产、排空队列，再由所有者调用 `destroy`。不要把 Drop 当成可靠的业务级停机握手。

## 后端与 feature

| Feature | 后端 | 适用场景 |
| --- | --- | --- |
| `futex` | Linux Futex | sequence/waiter 注册握手关闭丢唤醒窗口；无等待者时避免 wake syscall。 |
| `semaphore` | 进程共享 POSIX Semaphore | 注册握手避免丢唤醒和空闲 token 累积；destroy 广播已登记等待者。 |
| `eventfd` | EventFd + Unix socket FD 传递 | `EFD_SEMAPHORE` 通知；创建者负责受限权限的 FD 传递线程。 |

构建命令：

```bash
# 默认：三种后端
cargo build --release

# 只编译一种后端
cargo build --release --no-default-features --features futex
cargo build --release --no-default-features --features semaphore
cargo build --release --no-default-features --features eventfd
```

`use-futex`、`use-semaphore` 和 `use-eventfd` 是兼容旧调用入口的默认后端选择别名；每个别名会同时启用对应后端。新代码应通过 `SharedRingBufferOptions::strategy` 显式选择创建策略，通过 `open_auto` 打开未知策略的映射。

例如，只编译 Futex 并让旧的 `_aux` 构造器默认选择 Futex：

```bash
cargo build --release --no-default-features --features use-futex
```

## 固定布局与兼容性

- `MonitorInfo` 最多包含 `MAX_TAGS` 个标签状态；client name 和 layout symbol 使用固定字节数组。
- 时间戳是 Unix epoch 起的毫秒数。
- v10 映射不兼容旧 v9 映射。升级进程必须协调重启并重建共享映射。
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

本仓库当前没有声明许可证。获得或分发本项目之前，请先由项目维护者明确许可证；不要根据源码可见性推断使用授权。
