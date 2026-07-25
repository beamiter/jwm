# Changelog

本项目的显著变更记录在此。0.x 阶段仍可能调整公共 API；共享内存协议的不兼容变化会单独标明。

## [0.3.0] - 2026-07-25

### Added

- **泛型核心 `TypedRingBuffer<M, C>` 与 `WireSafe` 契约**：布局、跨进程锁、游标、后端与生命周期全部与 payload 解耦，任意满足契约（repr(C)、无 padding、任意位模式有效）的 POD 类型都可作为槽位类型；固定宽度整数、浮点与其数组自带 `WireSafe` 实现；`SharedRingBufferOptions` 新增 `create_typed`/`open_typed`/`open_or_create_typed`。`SharedRingBuffer` 成为 `TypedRingBuffer<WireMessage, WireCommand>` 的领域封装，公开 API 不变。
- 跨进程多生产者集成测试（3 个真实进程写不相交区间 + 小容量强制背压）与泛型 payload 测试（含槽大小错配拒绝）。

- 崩溃恢复：方向锁的锁字改存持有者 PID，其他进程通过 `/proc/<pid>` 探测发现持有者已死后原子夺回锁，消除持锁进程崩溃导致的永久卡死（半写 slot 由校验和兜底）。
- 僵尸映射回收：header 新增 `creator_pid`；新增 `creator_pid()`、`creator_alive()` 查询，`SharedRingBufferOptions::reclaim_stale(true)` 让 `open_or_create` 回收创建者已崩溃的残留映射。
- 新增 `WaitOutcome` 枚举与 `wait_message`/`wait_command`，区分「可读 / 超时 / 已销毁」；`timeout=None` 时保证只以 Ready 或 Destroyed 返回。
- 新增一体化阻塞读 `read_message_timeout`/`receive_command_timeout`，内部完成「等待 → 读取 → 被抢走则重试」循环。
- 新增 `try_peek_message`（不消费）、`drain_messages(max)`（单次持锁批量出队）、`write_message_overwrite`（队列满时覆盖最旧消息，面向状态广播场景）。
- 新增 `try_send_command`（`send_command` 的更名版本）。
- `SharedRingBufferOptions::command_capacity` 使命令环容量可配置（非零 2 的幂，默认仍为 16）。
- 新增测试：死锁持有者夺回、校验和损坏注入、覆盖写、批量读、等待结局、命令容量、僵尸回收。

### Changed

- 三个后端的 sequence/waiter 注册握手收敛为 `common.rs` 中的共享 `WaiterGate` 原语（RAII 注销登记），最微妙的并发逻辑只剩一份实现；行为不变。
- 校验和改为按 payload 整体字节计算（wire 类型以显式 `_reserved` 字段消除 padding）；命令通道引入独立的 `WireCommand` wire 表示，与消息通道的 wire/领域分离策略对齐。
- `SharedMessage::default()` 不再读取时钟：返回可复现的零值（`timestamp == 0`）；需要时间戳时用 `SharedMessage::new()` / `with_monitor_info()`。依赖旧行为的调用方需改用 `new()`。
- 自旋等待阶段周期性检查超时（长自旋预算配合短超时不再明显超期）；eventfd 单测不再串行化（socket 路径已含 PID+纳秒+nonce，无碰撞风险）。

- **协议 v11**（与 v10 不兼容，升级需协调重启并重建映射）：锁字语义改为持有者 PID；header 增加 `creator_pid` 并把高频写入的 `last_timestamp` 移到独立 cache line；消息 slot 移除从未使用的 `written_at` 字段；校验和改为 8 字节块化 FNV-1a（约一个量级提速）；semaphore backend header 按通道做 cache line 隔离；命令容量成为布局参数。
- `available_messages`/`available_commands` 改为免锁快照：等待路径的自旋探测不再每圈抢占两把跨进程方向锁。
- `try_write_message` 把序列化、校验和与时间戳计算移出临界区，缩短多生产者争用窗口。
- `wait_for_message`/`wait_for_command` 基于 `wait_message`/`wait_command` 实现；`timeout=None` 时不再可能虚假返回 `false`。
- serde、serde-big-array、rkyv 降级为可选依赖（feature `serde` / `rkyv`，默认关闭）。领域类型的这些 derive 从默认构建移除；队列传输从未使用它们。
- semaphore 后端超时改以单调钟为权威（`sem_timedwait` 切成短 REALTIME 片段），墙钟跳变不再把有界等待放大为近乎无限等待。
- eventfd 后端：本地 fd 缺失时等待如实返回 `NotConnected` 错误，不再伪装成超时；poll 超时毫秒向上取整，消除尾段忙轮询；opener 等待创建者就绪改为带 200ms deadline 的重试；创建者 cleanup 会把就绪状态标记为「已关闭」，此后新 opener 得到明确错误而不是 200ms 重连超时。
- eventfd fd 传递 socket 移入属主私有（0700）目录（优先 `$XDG_RUNTIME_DIR`），并用 `SO_PEERCRED` 校验对端 UID，拒绝其他用户连接。
- 全零（未初始化）header 的 backend id 0 在打开时被拒绝，不再静默映射到占位后端。
- `open` 与 `create` 使用同一套 flink 路径归一化。
- 试探式非阻塞等待（`timeout = Some(0)`）不再烧掉整个自适应自旋预算。

### Deprecated

- `create_shared_ring_buffer_aux`、`create_shared_ring_buffer`、`create_aux`、`open_aux`：改用 `SharedRingBufferOptions` / `open_auto`。
- `send_command`（更名为 `try_send_command`）、`receive_command`（改用 `try_receive_command`）。

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
