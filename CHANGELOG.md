# Changelog

本项目的显著变更记录在此。0.x 阶段仍可能调整公共 API；共享内存协议的不兼容变化会单独标明。

## [0.2.0] - 2026-07-13

### Added

- 新增协议 v10 自描述 header，记录映射总长度、容量、同步后端、槽大小与布局标记。
- 新增 `SharedRingBufferOptions` builder，可配置策略、消息容量和自适应轮询次数，并执行 create、open 或安全的 open-or-create。
- 新增 `SharedRingBuffer::open_auto`，从已验证 header 自动选择当前构建包含的后端。
- 新增 `SharedRingBuffer::open_or_create`；只在 flink 确实不存在时创建。
- 新增显式 `destroy`，并增加 `capacity`、`command_capacity`、`strategy`、`is_creator`、`last_message_timestamp` 与 `stats` 查询。
- 新增带错误返回的 `try_receive_command`；保留旧 `receive_command` 作为兼容入口。
- 新增 `SharedRingBufferStats` 状态快照。
- 为消息和命令槽增加稳定字节校验和。
- 新增本架构、安全和版本迁移文档。

### Changed

- 最低支持 Rust 版本调整为 1.86。
- 协议由 v9 提升到 v10；v9 与 v10 映射不能混用，升级时必须协调重启并重建映射。
- 将公开领域消息与内部共享 wire 表示分离，避免直接从共享字节构造包含 Rust `bool` 的值。
- 消息写/读、命令写/读游标分别放入独立 cache line，并增加进程共享方向锁。安全 API 可串行化同方向的多个生产者或消费者，同时继续优化无竞争 SPSC 路径。
- opener Drop 只分离本地句柄；creator Drop 才执行所有者级 destroyed 标记、唤醒和 flink 清理。
- `SyncStrategy` 使用稳定协议编号，并支持名称、显示、默认策略和 unsupported 检查。
- EventFd 创建者 cleanup 会主动唤醒并 join FD 传递线程；普通 opener 不再停止创建者 listener。
- Semaphore shutdown 按已登记等待者数量唤醒，不再销毁仍可能被其他进程引用的 semaphore 对象。
- README 不再宣称零复制或跨平台：当前实现是 Linux-only、无序列化的固定大小共享内存复制。

### Fixed

- 打开映射时验证后端和完整布局，避免调用者传错策略后按错误偏移解释共享内存。
- 在派生指针前验证映射长度、容量、槽大小和布局标记，并使用 checked arithmetic 计算内存区域。
- open-or-create 不再把权限、版本或损坏错误误当成“不存在”，也不会因此覆盖 flink。
- 创建过程先初始化私有映射，再通过同目录硬链接原子发布完整 flink，消除 opener 与 header 构造之间的数据竞争。
- 任意 opener Drop 不再让其他参与者看到全局 destroyed。
- 写入完成后的通知失败不再被表达成“数据未提交”，避免调用者安全重试时产生重复消息。
- Futex、Semaphore 和 EventFd 使用 sequence/waiter 的 SeqCst 注册握手，在无人等待时跳过 syscall，同时避免丢唤醒；Semaphore/EventFd 也不再因轮询消费积累无界历史 token。
- feature-only 构建：Futex、Semaphore 和 EventFd 均可独立编译；EventFd 正确启用 `libc` 与 `nix`。
- 无后端构建不再重复定义 `SyncStrategy`。

### Packaging

- `criterion` 移至开发依赖，不再进入库消费者的普通依赖图。
- `nix` 改为 EventFd 专用可选依赖。
- `use-futex`、`use-semaphore`、`use-eventfd` 保留为 legacy 默认后端别名，并各自启用对应后端。
- 增加 crate description、README、repository、documentation、keywords、categories、docs.rs Linux 构建配置和明确 MSRV。

### Migration from 0.1

1. 同时停止所有使用 v9 映射的进程，并移除旧 flink。
2. 升级全部参与者到 0.2，再由一个创建者创建 v10 映射。
3. 优先把多参数构造调用迁移到 `SharedRingBufferOptions`。
4. opener 使用 `open_auto`，除非应用需要明确拒绝非预期后端。
5. 把依赖“任意句柄 Drop 即全局关闭”的逻辑改为显式 `destroy`。
6. 命令接收迁移到 `try_receive_command`，以处理校验和 I/O 错误。
7. CI 使用 `--no-default-features --features <backend>` 分别验证每个后端。

## [0.1.0]

- 初始共享消息环、命令环以及 Futex、Semaphore、EventFd 后端。
