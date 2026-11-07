# D15.1 内存记账与配额：块 spec

状态：草案（2026-11-07）。阶段：P1 B 线第 1 块（按 2026-11-07 确认的 P1 排序）；块本身属于例外 A1（ROADMAP 中的状态为 EXC）。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[mappings.md](../../docs/porting/mappings.md)、[N3+A1 spec](../exceptions/N3-A1-concurrency-memory/spec.md) 第二部分（A1，设计已定）、[N0](../exceptions/N0-crate-layering/spec.md)（本块落在 `tidb-memory`，rank 1，只依赖 `tidb-base`、`tidb-metrics`）。

## 1. Go 范围

函数清单来自 tracker `data/func_blocks.tsv`（block = D15.1，76 个函数）：

| Go 文件 | 函数数 | Rust 文件 |
|---|--:|---|
| `pkg/util/memory/tracker.go` | 50 | `tracker.rs` |
| `pkg/util/memory/action.go` | 13 | `action.rs` |
| `pkg/util/memory/meminfo.go` | 11 | `meminfo.rs` |
| `pkg/util/memory/memstats.go` | 2 | `memstats.rs` |

**块外但必须移植的依赖**：

| 来源 | 内容 | Rust |
|---|---|---|
| `pkg/util/sqlkiller/sqlkiller.go`（D00.3，9 个函数） | `SQLKiller`：信号、`SendKillSignal`、`HandleSignal` 的错误映射、连接存活检查、`FinishResultSet`、`Reset`。`Consume` 直接调用它，所以按宪章 4.4 整文件移植到用到它的最底层 crate（N0 §3） | `sqlkiller.rs` |
| `pkg/util/cgroup`（块 X，不移植） | `InContainer`、`GetMemoryLimit`、`GetMemoryUsage`：meminfo 需要这三个 | `cgroup.rs`（glue，简化版，见 plan §4） |
| `github.com/shirou/gopsutil/v3@v3.24.5` `mem.VirtualMemory`（`go-dep:`） | Linux `/proc/meminfo`、macOS 无 cgo 路径（`sysctl hw.memsize` + `vm_stat`）的 `Total`、`Used` | `meminfo.rs` |
| `metrics.MemoryUsage`（D14.1，已完成块中缺这个指标） | `tidb_server_memory_usage{module,type}` | `tidb-metrics` 的 `memory_usage_set` |

### 1.1 本块新增（A1 规定，Go 中没有）

| 内容 | 依据 | Rust |
|---|---|---|
| `TrackedAlloc`：实现 `allocator_api2::Allocator`，按实际字节记到 tracker，转发给全局分配器（tidb-server 中是 jemalloc） | A1 3.3 第 1 条、`M-MEM` | `alloc.rs` |
| `Tracker::record`（只记账、不执行动作，分配路径用）与 `Tracker::checkpoint`（执行动作链） | A1 3.3 第 2、3 条 | `tracker.rs` |

### 1.2 不移植

- `Tracker.Release` 的 GC finalizer：Rust 在 Drop 时释放，"已释放未回收"的字节立即归还（A1 3.2、待定 3）。`recordRelease` / `release` 两步照移植，紧接着执行。
- failpoint：`GetMemTotalError`、`ReadMemStats`、sqlkiller 的 `randomPanic`（tidb-rs 还没有 failpoint）。
- `sysutil.RegisterGetMemoryCapacity`（`INFORMATION_SCHEMA` 集群硬件表，块 X）。

## 2. 对外接口

- `Tracker` 在 `Arc` 中（Go `*Tracker`）。会改树结构或可能执行动作的方法接收 `self: &Arc<Self>`：`attach_to`、`detach`、`replace_child`、`set_label`、`consume`、`release`、`reset` 等。
- **Go 的 `panic(err)` 改为 `Err`**（N3 3.4，L2）：`Consume` 里 kill 信号和 `PanicOnExceed` 的 panic 都变成返回值，凡是调用 `Consume` 的函数都返回 `Result<(), Error>`。Go 的字符串 panic（`Attach to a non-GlobalTracker`）和 nil 指针 panic（session root 没有 killer）在 Rust 中照样 panic，消息相同。
- `ActionOnExceed` 是 trait，动作以 `Arc<dyn ActionOnExceed>` 传递，按指针判等（Go 接口值的 `==`）。`BaseOOMAction` 是公共结构体，实现方组合它（Go 嵌入）。
- 标签常量、进程级变量（`ServerMemoryLimit`、`MemUsageTop1Tracker` 等）保持 Go 名（`M-GONAME`）。

## 3. 依赖与边界桩

| 依赖 | 处理 | 类别 |
|---|---|---|
| `pkg/util/cgroup`（X） | 简化版 glue：检查逻辑和读取的文件与 Go 相同，但挂载点假定为 `/sys/fs/cgroup`（Go 解析 mountinfo） | 永久（X 块） |
| Go runtime 的 `ReadMemStats` | jemalloc 统计（A1 3.2：`servermemorylimit` 读 jemalloc） | 例外 A1 |
| failpoint | 不移植 | 临时（failpoint 机制到位后补） |
| N3 3.3 第 2 条的唤醒通道（`M-GOCTX`） | `SqlKiller::set_on_kill` 留好钩子，`ExecCtx`（D00.1）接上 `CancellationToken` | 临时（D00.1） |

## 4. 验收

- converge：76/76 匹配（sqlkiller 另 9/9）。
- govec `memory` 领域全部通过，两个目标都回放。
- A1 §5 中 `util/memory` 的行为类 Go 测试：`TestAttachTo`、`TestDetach`、`TestReplaceChild`、`TestMaxConsumed`、`TestOOMAction`、`TestOOMActionPriority`、`TestErrorCode`、`TestGlobalTracker`、`TestSetLabel*`、`TestToString`、`TestFormatBytesWithPrune` 都以 govec 脚本的形式通过（`TestErrorCode` 由 8001 的错误码覆盖）；`TestConsume` 的并发部分和 `TestRelease` 的 GC 部分是 Rust 测试（plan §6）。
- 基准：`Consume` 与 Go `BenchmarkConsume` 对照，`TrackedAlloc` 相对全局分配器的开销（plan §6）。
- clippy `-D warnings`、fmt、`cargo test -p tidb-memory`（debug 与 release-checked）、`check_layers`。

A1 §5 的其余指标（记账覆盖率、限额精度、RSS、记账开销 ≤ 2%、归零）要等 chunk 和执行器接上 `TrackedAlloc` 才能测，分别在 D01.7、D12.* 验收。
