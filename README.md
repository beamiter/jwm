# Shared Structures

高性能的跨进程共享内存数据结构库，基于Rust编写。提供线程安全、零复制的进程间通信（IPC）机制。

## 功能概览

### 核心组件

- **SharedRingBuffer**: 基于共享内存的高性能SPSC（Single Producer Single Consumer）环形缓冲区
- **SharedMessage**: 监控信息消息结构，包含时间戳和监控详情
- **SharedCommand**: 命令结构，支持多种命令类型（查看标签、切换标签、设置布局等）

### 同步策略

支持多种进程间同步机制（可在编译时或运行时选择）：

- **Futex**: 基于Linux futex系统调用的轻量级同步
- **Semaphore**: 基于POSIX信号量的传统同步
- **EventFd**: 基于Linux eventfd的事件通知机制

## 快速开始

### 安装

在 `Cargo.toml` 中添加：

```toml
[dependencies]
shared_structures = { path = "../shared_structures" }
```

### 基本用法

```rust
use shared_structures::{SharedRingBuffer, SharedMessage, SyncStrategy};

// 创建或打开共享环形缓冲区
let buffer = SharedRingBuffer::create(
    "/tmp/my_buffer",
    SyncStrategy::Futex,
    Some(16),
    None
)?;

// 写入消息
let mut msg = SharedMessage::new();
msg.get_monitor_info_mut().set_client_name("my_app");
msg.get_monitor_info_mut().monitor_num = 1;
buffer.write(&msg)?;

// 读取消息
if let Ok(msg) = buffer.read(None) {
    println!("Received: {:?}", msg.get_monitor_info().get_client_name());
}

// 发送命令
use shared_structures::SharedCommand;
let cmd = SharedCommand::view_tag(1 << 2, 0);
buffer.write_command(&cmd)?;

// 接收命令
if let Ok(cmd) = buffer.read_command(None) {
    println!("Command received: {:?}", cmd.get_command_type());
}
```

## 特性

- **零复制**: 基于共享内存，无需数据序列化/反序列化开销
- **低延迟**: 自适应轮询与事件通知的混合机制
- **可靠性**: 内置校验和验证消息完整性
- **灵活性**: 支持多种同步后端，可根据场景选择最优方案
- **类型安全**: 充分利用Rust类型系统保证内存安全
- **跨平台**: 支持多种Linux同步机制

## 项目结构

```
shared_structures/
├── src/
│   ├── lib.rs                      # 库根文件
│   ├── shared_message.rs           # 消息与命令定义
│   ├── shared_ring_buffer.rs       # 核心环形缓冲区
│   └── backends/
│       ├── mod.rs                  # 模块定义
│       ├── common.rs               # 公共接口与头部定义
│       ├── futex.rs                # Futex同步实现
│       ├── semaphore.rs            # 信号量同步实现
│       └── eventfd.rs              # EventFd同步实现
├── benches/
│   ├── ring_buffer_bench.rs        # 基准测试
│   └── stress_test.rs              # 压力测试
├── Cargo.toml                      # 项目配置
└── README.md                       # 本文件
```

## 文档

- [架构设计](./ARCHITECTURE.md) - 详细的系统架构和设计原理
- [API文档](./API.md) - 完整的公开API参考
- [实现细节](./IMPLEMENTATION_DETAILS.md) - 深入的实现细节和优化策略
- [性能指南](./PERFORMANCE.md) - 性能特性和优化建议

## 编译和测试

### 构建项目

```bash
# 使用默认特性构建（包含所有同步后端）
cargo build --release

# 仅启用特定后端
cargo build --release --features futex
cargo build --release --features semaphore
cargo build --release --features eventfd
```

### 运行测试

```bash
# 运行所有测试
cargo test

# 运行基准测试
cargo bench

# 运行压力测试
cargo bench --bench stress_test
```

## 系统要求

- **操作系统**: Linux（支持futex、eventfd、POSIX信号量）
- **Rust版本**: 1.56+
- **依赖库**: libc, nix, serde, bincode等（详见Cargo.toml）

## 许可证

遵循项目指定的开源许可证。

## 贡献指南

欢迎提交Issue和Pull Request。请确保：

1. 代码遵循项目风格规范
2. 新功能包含相应的测试
3. 提交信息清晰描述修改内容
4. 更新相关文档

## 常见问题

### Q: 为什么需要多个同步策略？
**A**: 不同场景对同步的需求不同。Futex延迟最低但需要活跃轮询，EventFd更节能，Semaphore最传统可靠。

### Q: 支持多个生产者或消费者吗？
**A**: 目前设计为SPSC（单生产者-单消费者）。多生产者场景可使用外部同步机制。

### Q: 消息大小有限制吗？
**A**: SharedMessage有固定的内存布局。MonitorInfo包含最多9个标签和固定大小的字符数组。

### Q: 如何处理共享内存清理？
**A**: SharedRingBuffer会在drop时自动清理（如果是创建者）。使用destroy()方法显式清理。

## 相关资源

- [Futex系统调用](https://man7.org/linux/man-pages/man2/futex.2.html)
- [EventFd机制](https://man7.org/linux/man-pages/man2/eventfd.2.html)
- [POSIX信号量](https://man7.org/linux/man-pages/man3/sem_overview.3.html)
- [Rust 异步编程](https://async.rust-lang.org/)
