# 架构设计

本文描述 `shared_structures` 0.3 系列的共享内存协议与实现边界。安全前提和故障模型另见 [安全说明](./SAFETY.md)。

## 目标与范围

`shared_structures` 为 Linux 进程提供两个固定容量的共享内存方向：

- 消息队列传递 `SharedMessage`；
- 命令队列传递 `SharedCommand`。

每个方向支持多个生产者和多个消费者。相同方向上的生产者、消费者分别通过共享方向锁串行化；无竞争的 SPSC 是重点优化路径。同步后端只负责“等待与唤醒”，队列内容和游标始终位于共享内存中。

本库不是通用对象存储，也不承诺 Windows、macOS 或不同 CPU 架构之间共享同一映射。

## 分层

```text
SharedMessage / SharedCommand        公开领域类型
              │ 转换与校验
              ▼
WireMessage / WireCommand            固定、可验证的共享表示（WireSafe）
              │
              ▼
TypedRingBuffer<M, C>                泛型双通道核心 + 协议 v13 header
              │
              ▼
Futex / Semaphore / EventFd          等待与唤醒后端（共享 WaiterGate 握手）
```

`TypedRingBuffer<M, C>` 是公开的泛型核心：布局计算、跨进程方向锁、游标协调、后端与生命周期全部与 payload 解耦，任意满足 `WireSafe` 契约的 POD 类型都可作为槽位类型。`SharedRingBuffer` 只是 `TypedRingBuffer<WireMessage, WireCommand>` 加领域转换。校验和按 payload 整体字节计算，`WireSafe` 契约（无 padding、任意位模式有效）正是该计算的安全前提。

公开领域类型用于正常 Rust 代码；内部 wire 类型用于共享内存。二者分离可避免把 Rust 的 `bool` 有效位模式、公开字段演进和共享协议布局绑在一起。写入时领域值被编码到固定大小槽位，读取时先验证再转换回来。因此当前传输路径是固定大小内存复制，不是借用共享槽位的零复制 API，也不使用 serde 或 rkyv 做队列传输。

## 协议 v13 布局

映射按以下顺序布局，各区域按自身对齐要求向上取整：

1. `GenericHeader`；
2. 所选同步后端的 header；
3. `capacity` 个消息槽；
4. `command_capacity` 个命令槽（可配置，默认 16）。

通用 header 是自描述的，包含：

- magic 与协议版本；
- 映射总长度；
- 消息和命令容量；
- 稳定的同步后端编号；
- 两类槽位大小；
- 布局标记；
- destroyed 状态与创建者 PID（元数据 cache line）；
- 最后写入时间（独立 cache line，隔离生产者的高频写入）；
- 消息写、消息读、命令写、命令读四个游标。

`create` 先创建一个尚无公开 flink 的私有 OS 映射，构造 header 中的原子对象和普通元数据、初始化后端，再以 Release 顺序发布 magic。最后，它把已经完整写入的临时 flink 通过同目录硬链接原子发布到目标路径；并发创建者中只有一个能成功，opener 不会看到半写的 os-id 或正在构造的 Rust 原子对象。

`open_auto` 先读取稳定 header，校验 magic、版本、总长度、容量、槽大小和布局标记，再选择当前构建中对应的后端。显式指定策略的旧打开入口也必须验证策略与 header 一致。任何验证失败都应在构造 slot 裸指针前返回错误。

v13 的 `WireMessage` 携带 WM session、最小化投影 generation 和最多 16 个固定长度窗口条目；`WireCommand` 为无 padding 的 64 字节结构。三类 Dock 命令除窗口/session、来源 monitor 和动效 anchor 外，还必须回显它们据以产生的 generation。

v13 不直接打开 v12 或更早的映射。升级双方需要先停止旧进程、移除旧 flink，再共同创建 v13 映射。

## 游标、方向锁与内存顺序

四个队列游标分别占用独立 cache line。每个游标包含一个单调递增索引和一个跨进程方向锁：

- 消息生产者竞争消息写锁；
- 消息消费者竞争消息读锁；
- 命令生产者竞争命令写锁；
- 命令消费者竞争命令读锁。

这使同一方向上的操作可安全串行化，同时避免消息与命令、生产与消费之间不必要的 false sharing。典型 SPSC 场景中锁没有竞争，只增加一次很短的原子快路径。

方向锁的锁字存放持有者 PID（0 表示空闲）。竞争者在自旋若干轮后，会读取 `/proc/<pid>/stat` 探测持有者是否仍然存活；目录已消失或进程已进入 zombie/dead 状态时原子夺回锁，使单个进程崩溃（即使尚未被父进程 `wait`）不会永久卡死整个方向。被夺回的锁可能留下半写的 slot，由槽位校验和在读取端兜底；未发布的游标推进随崩溃一起丢弃。PID 复用可能让探测误判"仍存活"，其代价只是退回等待行为，不会错误夺锁。

生产者在持有写锁时检查容量、写入完整槽位，然后以 Release 顺序发布新索引。消费者以 Acquire 顺序观察写索引，在持有读锁时校验并复制槽位，最后发布新读索引。索引使用 wrapping 运算，容量必须是 2 的幂。

消息槽与命令槽都带校验和（8 字节块化 FNV-1a），用于发现撕裂写入或内容损坏，不是密码学完整性保护。序列化、校验和与时间戳都在获取方向锁之前计算，锁内只有容量判断、槽位复制与索引发布。

## 同步后端

| 后端 | 等待机制 | 特点与约束 |
| --- | --- | --- |
| Futex | Linux `futex` | 默认低延迟选择；sequence/waiter 注册握手关闭丢唤醒窗口，二者分离 cache line，无等待者时避免 wake syscall。 |
| Semaphore | 进程共享 POSIX semaphore | sequence/waiter 注册握手让无人等待时跳过 `sem_post`；超时以单调钟为权威（`sem_timedwait` 切成短 REALTIME 片段，墙钟跳变影响有界）；destroy 按已登记等待者数量唤醒，不在仍可能被其他进程引用时调用 `sem_destroy`。 |
| EventFd | Linux `eventfd` + Unix socket FD 传递 | 使用 `EFD_SEMAPHORE` 和 sequence/waiter 注册握手；创建者在属主私有（0700）目录内监听 Unix socket，用 `SO_PEERCRED` 校验对端 UID 后传递 CLOEXEC 描述符，cleanup 时标记"已关闭"并 join 监听线程。 |

所有后端都先执行可配置次数的自适应轮询（周期性复核超时），再进入内核等待。sequence/waiter 注册握手收敛在 `common.rs` 的 `WaiterGate` 原语中（RAII 注销登记），三个后端共用同一份实现：signal 总会推进 sequence，但只在确有已注册 waiter 时进入内核，因此既不会丢失注册窗口中的唤醒，也不会让 Semaphore/EventFd 的轮询型消费者积累无限历史 token。通知只是优化：队列索引才是数据是否可读的事实来源。调用者被唤醒后仍需重新检查队列状态。

feature 决定二进制包含哪些后端，`SyncStrategy` 决定创建映射时使用哪个已编译后端。打开者如果没有编译映射所需后端，会得到 unsupported 错误，而不是按另一种布局解释内存。

## 创建、打开与生命周期

- `create` 创建并初始化新映射；容量和轮询参数可通过 `SharedRingBufferOptions` 设置。
- `open_auto` 从 v13 header 自动发现后端与容量。
- `open_or_create` 只在 flink 确实不存在时创建；权限、协议或布局错误不会触发覆盖创建。
- `SharedRingBufferOptions::reclaim_stale(true)` 是唯一的例外：`open_or_create` 打开映射后发现 `creator_pid` 对应的进程已不存在时，移除残留 flink 并重建。该选项应只由单一监督者角色开启。
- `capacity` 返回经验证的消息容量，`stats` 提供当前队列状态快照。
- opener 的 `Drop` 只清理本地资源，不会销毁其他参与者看到的映射。
- creator 的 `Drop` 标记映射已销毁、唤醒等待者、清理后端所有者资源并移除 flink。
- `destroy` 提供显式的全局终止；其他句柄随后观察到 closed/destroyed 状态。

Drop 无法替代跨进程的优雅停机协议。需要保证消息处理完毕时，应用应先停止生产、排空队列，再由所有者显式销毁。

## 可观测性

状态查询是并发快照，而不是事务：调用 `stats` 后队列可能立即变化。`available_messages`、`available_commands` 和等待结果同样只能用于调度，不能作为下一次操作必然成功的保证。

库使用 `log` facade 报告后端诊断，不初始化日志实现。应用负责选择 logger 和过滤级别。

## 演进规则

- 只改变 Rust API、且不改变 wire 布局的兼容修改不提升协议版本。
- header、槽位、原子协调规则或 backend header 的不兼容变化必须提升协议版本。
- 公共领域类型新增能力不应依赖改变 wire 类型；需要新字段时，先定义明确的版本转换策略。
- 新同步后端必须分配稳定 backend id，并保证未知 id 被拒绝。
- 发布前通过 feature 组合、跨进程、并发压力、布局 golden fixture 和旧协议拒绝测试。
