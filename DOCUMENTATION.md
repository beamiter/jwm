# 技术文档索引

完整的shared_structures技术文档套件。快速导航和文档概览。

## 📚 文档结构

### 📖 核心文档

| 文档 | 内容 | 适合人群 | 快读时间 |
|------|------|---------|---------|
| **[README.md](./README.md)** | 项目概览、快速开始、功能特性 | 所有人 | 5 min |
| **[ARCHITECTURE.md](./ARCHITECTURE.md)** | 系统架构、设计原理、数据流 | 架构师、核心贡献者 | 15 min |
| **[API.md](./API.md)** | 完整的API参考、所有公开接口 | 库使用者 | 20 min |
| **[IMPLEMENTATION_DETAILS.md](./IMPLEMENTATION_DETAILS.md)** | 实现细节、算法、优化 | 贡献者、深度使用者 | 25 min |
| **[PERFORMANCE.md](./PERFORMANCE.md)** | 性能指标、优化策略、基准测试 | 性能优化者 | 20 min |
| **[DEVELOPMENT.md](./DEVELOPMENT.md)** | 开发指南、贡献流程、调试技巧 | 开发者、贡献者 | 15 min |

## 🎯 快速导航

### 我是库的使用者，我想...

**快速开始**
→ [README.md - 快速开始部分](./README.md#快速开始)

**了解如何使用API**
→ [API.md](./API.md)

**了解消息格式**
→ [API.md - 数据结构部分](./API.md#sharedmessage)

**优化性能**
→ [PERFORMANCE.md - 性能优化策略](./PERFORMANCE.md#性能优化策略)

**解决问题**
→ [PERFORMANCE.md - 常见瓶颈](./PERFORMANCE.md#常见瓶颈和解决方案)

### 我是贡献者，我想...

**理解项目架构**
→ [ARCHITECTURE.md](./ARCHITECTURE.md)

**学习编码规范**
→ [DEVELOPMENT.md - 代码规范](./DEVELOPMENT.md#3-代码规范)

**添加新功能**
→ [DEVELOPMENT.md - 添加新功能](./DEVELOPMENT.md#4-添加新功能)

**运行测试和基准**
→ [DEVELOPMENT.md - 测试策略](./DEVELOPMENT.md#2-测试策略)

**调试问题**
→ [DEVELOPMENT.md - 调试技巧](./DEVELOPMENT.md#5-调试技巧)

### 我是维护者，我想...

**理解整个系统**
→ [ARCHITECTURE.md](./ARCHITECTURE.md) → [IMPLEMENTATION_DETAILS.md](./IMPLEMENTATION_DETAILS.md)

**审查性能**
→ [PERFORMANCE.md](./PERFORMANCE.md)

**管理依赖**
→ [DEVELOPMENT.md - 依赖管理](./DEVELOPMENT.md#依赖管理)

**设置CI/CD**
→ [DEVELOPMENT.md - 持续集成](./DEVELOPMENT.md#持续集成)

## 🔑 核心概念速览

### SharedRingBuffer

- **用途**: 高性能的SPSC进程间通信缓冲区
- **模型**: Single Producer - Single Consumer（单生产者-单消费者）
- **基础**: 共享内存（/dev/shm 或 flink）
- **延迟**: 无数据时 < 1 μs，有竞争时 1-10 ms

📍 详见: [ARCHITECTURE.md](./ARCHITECTURE.md#核心设计原则)

### SharedMessage

- **大小**: ~5KB（固定）
- **包含**: 时间戳 + MonitorInfo
- **用途**: 在进程间传递监控信息
- **序列化**: 支持serde/bincode

📍 详见: [API.md - SharedMessage](./API.md#sharedmessage)

### 同步策略

| 策略 | 延迟 | CPU | 场景 |
|------|------|-----|------|
| **Futex** | 极低 | 中 | 高频 (> 1000/s) |
| **EventFd** | 中 | 低 | 中频 (10-1000/s) |
| **Semaphore** | 中 | 低 | 低频 (< 100/s) |

📍 详见: [PERFORMANCE.md - 同步策略选择](./PERFORMANCE.md#3-同步策略选择)

## 📊 性能对标

### 延迟（单线程，无竞争）

```
write()         ~850 ns
read() 有数据    ~125 ns
signal()        ~2 μs
```

### 吞吐量

```
单向消息        ~1.2M msg/s
双向命令        ~500K msg/s
```

📍 详见: [PERFORMANCE.md - 性能基准](./PERFORMANCE.md#性能基准)

## 🛠️ 常见任务

### 创建和打开缓冲区

```rust
// 创建
let buffer = SharedRingBuffer::create(
    "/tmp/my_ipc",
    SyncStrategy::Futex,
    Some(64),
    Some(400)
)?;

// 打开
let buffer = SharedRingBuffer::open(
    "/tmp/my_ipc",
    SyncStrategy::Futex,
    None
)?;

// 便捷方式：尝试打开，失败则创建
let buffer = SharedRingBuffer::create_shared_ring_buffer(
    "/tmp/my_ipc",
    SyncStrategy::Futex
)?;
```

📍 详见: [API.md - SharedRingBuffer](./API.md#sharedringbuffer)

### 读写消息

```rust
// 写入
let mut msg = SharedMessage::new();
msg.get_monitor_info_mut().set_client_name("my_app");
buffer.write(&msg)?;

// 读取（非阻塞）
if let Ok(msg) = buffer.try_read() {
    println!("{:?}", msg.get_timestamp());
}

// 读取（带超时）
match buffer.read(Some(Duration::from_secs(1))) {
    Ok(msg) => println!("Got message"),
    Err(e) => println!("Timeout or error: {}", e),
}
```

📍 详见: [API.md - 消息操作](./API.md#消息操作)

### 发送命令

```rust
// 创建命令
let cmd = SharedCommand::view_tag(1 << 2, 0);

// 发送
buffer.write_command(&cmd)?;

// 接收
if let Ok(cmd) = buffer.try_read_command() {
    match cmd.get_command_type() {
        CommandType::ViewTag => println!("View tag command"),
        _ => println!("Other command"),
    }
}
```

📍 详见: [API.md - 消息操作](./API.md#消息操作)

## 📈 性能优化清单

- [ ] 选择合适的同步策略（基于消息频率）
- [ ] 调整缓冲区大小（64-128 推荐起点）
- [ ] 优化轮询参数（高频用400，低频用100）
- [ ] 避免频繁的字符串操作（set_client_name）
- [ ] 使用bind_cpu提升缓存局部性
- [ ] 在NUMA系统上使用numactl
- [ ] 定期运行基准测试监控性能

📍 详见: [PERFORMANCE.md - 性能优化策略](./PERFORMANCE.md#性能优化策略)

## 🔍 故障诊断

### 问题：缓冲区频繁满

**症状**: 写入返回 WouldBlock，吞吐量低

**解决方案**:
1. 增加缓冲区大小：`Some(256)` 而非 `Some(64)`
2. 优化消费者处理逻辑
3. 监控队列深度

📍 详见: [PERFORMANCE.md - 瓶颈1](./PERFORMANCE.md#瓶颈1缓冲区频繁满)

### 问题：高CPU占用

**症状**: 进程CPU占用接近100%

**解决方案**:
1. 降低轮询次数：`Some(100)` 而非 `Some(400)`
2. 改用EventFd后端
3. 增加处理批大小

📍 详见: [PERFORMANCE.md - 瓶颈2](./PERFORMANCE.md#瓶颈2高cpu占用)

### 问题：延迟不稳定

**症状**: P99延迟很高

**解决方案**:
1. 禁用CPU动态调频
2. 使用CPU亲和性
3. 使用实时调度（需权限）

📍 详见: [PERFORMANCE.md - 瓶颈3](./PERFORMANCE.md#瓶颈3延迟不稳定)

## 📦 编译配置

### 启用所有后端（默认）

```bash
cargo build --release
```

### 仅启用特定后端

```bash
cargo build --release --features futex
cargo build --release --features semaphore
cargo build --release --features eventfd
```

### 选择运行时默认后端

```bash
cargo build --release --features "futex use-futex"
# 或 use-semaphore / use-eventfd
```

📍 详见: [README.md - 构建项目](./README.md#构建项目)

## 🧪 测试和基准

### 运行测试

```bash
cargo test --all-features
cargo test --all-features -- --nocapture
cargo test test_specific_function
```

### 运行基准

```bash
cargo bench --all-features
cargo bench --bench ring_buffer_bench --release
```

### 压力测试

```bash
cargo bench --bench stress_test --release
```

📍 详见: [DEVELOPMENT.md - 测试策略](./DEVELOPMENT.md#2-测试策略)

## 🤝 贡献指南

### 基本工作流

```bash
# 1. 创建特性分支
git checkout -b feat/my-feature

# 2. 进行开发、测试、提交
cargo test --all-features
cargo fmt
cargo clippy --all-features -- -D warnings
git commit -m "feat: add my feature"

# 3. 推送并创建PR
git push origin feat/my-feature
gh pr create --title "Add my feature"
```

### 代码规范

- **格式**: 使用 `cargo fmt`
- **Lint**: 通过 `cargo clippy -- -D warnings`
- **注释**: 解释 "为什么"，而非 "做什么"
- **测试**: 新功能需要单元测试

📍 详见: [DEVELOPMENT.md - 开发工作流](./DEVELOPMENT.md#1-代码修改流程)

## 📚 相关资源

### Linux系统调用

- [Futex Man Page](https://man7.org/linux/man-pages/man2/futex.2.html)
- [EventFd Man Page](https://man7.org/linux/man-pages/man2/eventfd.2.html)
- [POSIX信号量](https://man7.org/linux/man-pages/man3/sem_overview.3.html)

### Rust资源

- [Rust Book](https://doc.rust-lang.org/book/)
- [Cargo文档](https://doc.rust-lang.org/cargo/)
- [并发编程](https://doc.rust-lang.org/book/ch16-00-concurrency.html)

### 性能分析工具

- [Perf](https://perf.wiki.kernel.org/index.php/Main_Page)
- [Criterion.rs](https://bheisler.github.io/criterion.rs/book/)
- [Valgrind](https://valgrind.org/)

## 📝 文档变更日志

| 版本 | 日期 | 变更 |
|------|------|------|
| 1.0 | 2026-04-30 | 初始文档套件完成 |

## ❓ 常见问题

**Q: 我应该从哪个文档开始？**

A: 如果你是库的新用户，从 [README.md](./README.md) 开始。如果你想贡献代码，阅读 [DEVELOPMENT.md](./DEVELOPMENT.md)。

**Q: 如何选择合适的同步策略？**

A: 根据消息频率：
- > 1000/s → Futex
- 10-1000/s → EventFd
- < 100/s → Semaphore

详见 [PERFORMANCE.md](./PERFORMANCE.md)

**Q: 如何解决性能问题？**

A: 参考 [PERFORMANCE.md - 常见瓶颈](./PERFORMANCE.md#常见瓶颈和解决方案) 部分，或者 [DEVELOPMENT.md - 调试技巧](./DEVELOPMENT.md#5-调试技巧)。

**Q: 可以在Windows或macOS上使用吗？**

A: 不能直接使用（futex/eventfd是Linux特定的）。但可以在这些平台编译和测试API层。跨平台方案见 [ARCHITECTURE.md - 扩展点](./ARCHITECTURE.md#扩展点)。

**Q: 支持多生产者或多消费者吗？**

A: 否。库设计为SPSC模型。多个生产者/消费者需要外部同步机制。

## 📞 获取帮助

- **Bug报告**: 提交Issue，包含复现代码和系统信息
- **功能请求**: 在Issues中讨论
- **代码审查**: 创建PR进行讨论
- **性能优化**: 参考 [PERFORMANCE.md](./PERFORMANCE.md)
- **开发帮助**: 参考 [DEVELOPMENT.md](./DEVELOPMENT.md)

## 📄 许可证

项目遵循指定的开源许可证。详见项目根目录的LICENSE文件。

---

**最后更新**: 2026-04-30  
**文档版本**: 1.0  
**项目版本**: 0.1.0
