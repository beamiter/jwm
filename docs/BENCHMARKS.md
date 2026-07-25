# Benchmark 与 sanitizer 指南

## 基准测试

先编译全部基准而不执行：

```bash
cargo bench --all-features --no-run
```

执行三个后端的独立基线和同进程策略对比：

```bash
./benches/run_benches.sh
```

脚本对每个基线使用 `--no-default-features --features <backend>`，保证名为
`eventfd` 或 `semaphore` 的结果没有实际落到默认 `futex` 后端。传入 `--clean`
可以先清除旧的 Criterion 数据；设置 `RUN_STRESS=1` 才会执行耗时更长的压力组。

跨进程唤醒延迟使用独立的两进程 ping-pong 基准（父进程 spawn 真实 echo
子进程，往返 = 写消息 → 对端回 ack 命令）：

```bash
cargo bench --bench cross_process_latency
```

每个后端出两组数字：`adaptive_spin`（默认自旋预算内命中，测轮询快路径）
与 `kernel_wake`（自旋为 0，每次等待都进内核，测真实跨进程唤醒延迟，
三后端的差异集中在这里）。参考量级：自旋路径约几百纳秒且后端无关；
内核唤醒路径为微秒级，futex < semaphore < eventfd。

基准遵循以下计时边界：

- 共享内存创建、打开、消息预构建和工作线程创建通常在计时区间之外；专门的
  `create_destroy_ring_buffer` 基准除外。
- 读基准逐轮在计时外重新填充 ring，避免 Criterion 批处理 setup 共享同一 fixture
  时，后续迭代读取一个已经耗尽的队列。
- 跨线程基准的 Barrier 包含驱动线程；驱动线程记录开始时间后才释放工作线程，
  防止工作在线程创建后、计时开始前提前发生。
- 每个固定工作量基准使用 release profile 下仍生效的 `assert!` 校验收发条数；后端
  错误会终止本次基准，不会被误当作正常背压。
- `/tmp` 中的共享内存链接包含进程 ID、时间戳和单调 nonce，并由 RAII 清理，以便
  并行运行和 panic 展开时不复用旧 fixture。

## Sanitizer

`san.sh` 不会安装工具链。它会先检查 Linux、nightly 和 nightly `rust-src`，缺少前置
条件时给出明确错误。默认对 futex、semaphore、eventfd 分别运行 AddressSanitizer 和
ThreadSanitizer：

```bash
./san.sh
```

MemorySanitizer 需要原生依赖也被正确插桩，否则可能出现假阳性，因此仅显式启用：

```bash
./san.sh memory
```

可以缩小调试矩阵：

```bash
SANITIZER_BACKENDS='futex eventfd' ./san.sh address
```

脚本使用 `-Zbuild-std` 让标准库一并插桩，并为每个 sanitizer/backend 组合使用独立的
`target/sanitizers/` 构建目录。sanitizer 成功与否应以脚本实际退出状态和报告为准。
