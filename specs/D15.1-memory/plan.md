# D15.1 内存记账与配额：plan

## 1. Rust 模块布局

crate `crates/memory`（包名 `tidb-memory`，N0 rank 1）。Go 文件对应 Rust 文件。

| Go | Rust | 说明 |
|---|---|---|
| `pkg/util/memory/tracker.go` | `tracker.rs` | tracker 树、标签、限额、`MemUsageTop1Tracker`、`FormatBytes` |
| `pkg/util/memory/action.go` | `action.rs` | `ActionOnExceed`、`BaseOOMAction`、`actionWithPriority`、`LogOnExceed`、`PanicOnExceed`、优先级常量 |
| `pkg/util/memory/meminfo.go` | `meminfo.rs` | `MemTotal` / `MemUsed` 钩子、缓存、gopsutil `VirtualMemory` |
| `pkg/util/memory/memstats.go` | `memstats.rs` | 堆统计（jemalloc） |
| `pkg/util/sqlkiller/sqlkiller.go` | `sqlkiller.rs` | D00.3，整文件 |
| `pkg/util/cgroup`（X） | `cgroup.rs` | glue，简化版 |
| — | `alloc.rs` | A1 `TrackedAlloc` |

## 2. 类型与写法映射

| Go | Rust | 规则 |
|---|---|---|
| `*Tracker` | `Arc<Tracker>` | — |
| `mu.children map[int][]*Tracker`（nil 表示没有） | `Mutex<Option<HashMap<isize, Vec<Arc<Tracker>>>>>`：父节点强引用子节点，与 Go 相同，所以从没 Detach 的子节点仍在 `String()`、`SearchTrackerWithoutLock` 中可见（EXPLAIN ANALYZE 依赖这一点） | — |
| `parMu.parent *Tracker` | `Mutex<Weak<Tracker>>`：Go 的父子互指由 GC 处理，`Arc` 会造成环、泄漏整棵树，所以父指针是 `Weak`。被挂着的 tracker 的祖先都能从根到达，只要根还在，`Consume` 的上溯就不会提前停 | L1（结构上的唯一差别） |
| `bytesLimit atomic.Pointer[bytesLimits]` | `ArcSwap<BytesLimits>` | `M-SNAP` |
| `actionMuForHardLimit` / `actionMuForSoftLimit`（mutex + 接口） | `Mutex<Option<Arc<dyn ActionOnExceed>>>` | `M-LOCK`（parking_lot，无毒化） |
| `Killer *SQLKiller`、`IsRootTrackerOfSess`、`SessionID`、`label` | `ArcSwapOption<SqlKiller>`、`AtomicBool`、`AtomicU64`、`AtomicIsize`（Go 中是普通字段或 `atomicutil`，Rust 需要内部可变） | — |
| `sync/atomic` | `SeqCst` | Go 原子操作是顺序一致的 |
| `int64(float64(limit) * 0.8)` | `gostd::conv::f64_to_i64` | `M-F2I` |
| `panic(err)`（kill、`PanicOnExceed`） | `Err(err)`，调用 `Consume` 的函数都返回 `Result` | N3 3.4 |
| `Go 接口值 ==` | `std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b))` | — |
| 包级变量（`ServerMemoryLimit` 等） | `static` 原子量 / `ArcSwap`，保持 Go 名 | `M-GONAME` |
| `MemTotal`、`MemUsed` 函数变量，`init()` 中选定 | `RwLock<MemHooks>`（函数指针），第一次使用时执行 `init` | — |
| `BaseOOMAction.fallbackAction`（Go 不加锁读写） | `Mutex<Option<ActionRef>>` | — |
| `LogOnExceed.mutex` + `acted` | `Mutex<bool>` | — |

## 3. A1 的两半：`record` 与 `checkpoint`

A1 3.3 要求分配路径只做原子加、不执行动作，动作在检查点执行。`Consume` 拆成两个内部步骤：

- `walk(bs)`：Go `Consume` 中沿父链的循环（原子加、更新 `maxConsumed`、找到最上层的超硬限额和超软限额节点、找到 session 根）；`bs == 0` 时只求值、不加。
- `Walk::act()`：Go `Consume` 循环之后、`bs > 0` 时的部分：更新 Top1 → kill 信号 → 硬限额动作 → 软限额动作。

对外：

| 函数 | 内容 | 用途 |
|---|---|---|
| `consume(bs)` | `walk(bs)`，`bs > 0` 时再 `act()`：与 Go 的 `Consume` 完全相同 | 移植代码中 Go 的 `Consume` 调用 |
| `record(bs)` | 只 `walk(bs)`，不失败 | `TrackedAlloc` |
| `checkpoint()` | `walk(0).act()`：按当前值判断超限，执行动作链 | A1 3.3 第 3 条：放在 Go 正向 `Consume` 的位置 |

A1 原稿写的是"越过限额时在节点上置标志，检查点检查标志"。这里改为检查点时重新沿父链判断：热路径更省（不写标志），结果与"在检查点时刻调用一次 Go 的 `Consume`"相同。

`TrackedAlloc` 每个句柄（含其克隆）有一个待记账计数器，每次分配一次原子加，累计超过 ±64KiB 时调用 `record` 沿父链传播；`flush()` 和最后一个克隆被 drop 时记掉剩余部分。批量误差上界是"64KiB × 存活句柄数"。A1 写的是"每个线程按句柄批量"，按线程拆分是之后由基准决定的优化（A1 §5 记账开销）。

## 4. L2 偏离与已知差异

| 位置 | 差异 | 理由 |
|---|---|---|
| `Release` / `BufferedRelease` | GC-aware（`EnableGCAwareMemoryTrack`）时，`recordRelease` 后立即 `release`，`BytesReleased` 不会停在非 0 | 没有 GC finalizer；Rust 在 Drop 时就释放了（A1 3.2、待定 3）。只有 ANALYZE（块 X）用 |
| `SearchTrackerWithoutLock` | 加锁读子节点表 | 不允许无同步的读；没有可观察的差别 |
| `Tracker.LessThan` | Go 是 nil 接收者上的方法，Rust 是 `Tracker::less_than(Option<&Tracker>, Option<&Tracker>)` | Rust 方法不能有空接收者 |
| `HandleKillSignal`、`Consume`、`PanicOnExceed.Action` | 返回 `Err` 而不是 panic | N3 3.4 |
| `ForceReadMemStats` / `ReadMemStats` | jemalloc `stats.allocated` / `active` / `mapped` / `resident` 代替 `HeapAlloc` / `HeapInuse` / `HeapSys` / `Sys` | A1 3.2（同 TiKV） |
| `InstanceMemUsed` | 读 jemalloc `stats.allocated` | 同上 |
| `cgroup.rs` | 不解析 `/proc/self/mountinfo`，挂载点假定为 `/sys/fs/cgroup`（v2）或 `/sys/fs/cgroup/memory`（v1）；`InContainer` 与 Go 相同 | X 块的简化版；容器里通常就是这样挂载的 |
| `meminfo` 在 macOS | 用 `sysctl -n hw.memsize` 和 `vm_stat` 命令（gopsutil 的无 cgo 路径） | crate 不写 `unsafe`；60s / 500ms 缓存下开销可以接受（基准 §6） |
| `Default for Tracker` | Go 的零值 `Tracker{}` 没有限额（nil 指针，直接 `Consume` 会 panic），Rust 的默认值是无限额 | 只与 `init_tracker` 一起用 |
| sqlkiller `intest.InTest` | 本 crate 的单元测试中为真 | Go 在所有测试二进制中为真（build tag） |

## 5. 反向引用与处理（N0 §4）

`check_layers.py --block-edges` 中 `tidb-memory` 没有向上或同层引用。D15.1 的出边：D00.2（2）、D14.1（3）都在下层；D00.3（9，sqlkiller）已整文件移入本 crate；X（5：cgroup、sysutil、failpoint）按 §3 处理。

## 6. 测试与基准

**govec `memory` 领域**（tracker `govec/domains/memory.go`，回放 `tests/govec_memory.rs`）：

- `script`：一串步骤，作用于用编号标识的 tracker、动作和 killer，记录每一步的结果和期间触发的动作（按顺序）。覆盖：
  - tracker_test.go 的 14 个测试，改写成脚本；
  - kill 信号 1–5 与 session 根；
  - LOG / CANCEL 动作；
  - Top1；
  - 限额边界（`DefMemQuotaQuery`、≤ 0、软限额的截断）；
  - 动作链（优先级包装、`UnbindActionFromHardLimit`、`TriggerFallBackAction`）；
  - 字符串化与查询；
  - 1,500 个随机脚本：随机森林（保持无环）、全局 tracker、动作绑定、限额、Attach/Detach/Replace/Reset 等。
- `format_bytes`、`bytes_to_string`：`TestFormatBytesWithPrune` 的用例表（经测试自己的 `parseByte`）、边界、随机值。
- 共 1,850 条。Go 的 panic（`panic(err)`、字符串 panic）记为 `{"panic": …}` 并结束该脚本，Rust 侧对应 `Err` 或同消息的 panic。

**Rust 测试**：

| 测试 | 内容 |
|---|---|
| `tests/concurrency.rs` | 16 线程 × 2 万次 `Consume` 的总量（Go `TestConsume` 的并发部分放大）；三层树在并发下的总量与 `MaxConsumed`、并发中 re-attach；GC-aware `Release` 立即归还 |
| `alloc.rs` 单元测试 | `TrackedAlloc` 记真实字节、grow、drop 后归零、小分配批量、分配路径不执行动作而 `checkpoint` 执行 |
| `sqlkiller.rs` 单元测试 | 信号与错误映射、CAS 只认第一个信号、连接存活检查、`FinishResultSet` |
| `meminfo.rs` 单元测试 | `/proc/meminfo`、`vm_stat` 解析；本机读数；`init_memory_hook` |

**基准**（`benches/tracker.rs`，Apple M4 Pro，12 线程；Go 为 `go test -bench` v8.5.8 `pkg/util/memory`）：

| 项目 | Go | Rust |
|---|--:|--:|
| `BenchmarkConsume`（12 线程，每线程一个子节点挂同一个根，`Consume(256<<20)`） | 190.7 ns | 270 ns |
| 三层树单线程 `Consume(+64)` + `Consume(-64)` | — | 43.5 ns |
| `MemTotal()` / `MemUsed()`（缓存命中） | 16.7 ns | 24.1 ns |
| 4 KiB 分配 + 释放：全局分配器 / `TrackedAlloc` | — | 9.15 / 9.55 ns |

`Consume` 在"所有线程打同一个根"的压力下比 Go 慢约 40%。多出来的是每上溯一层对父节点 `Arc` 强计数的一加一减（`Weak::upgrade` 和 drop），在共享的根上形成争用。Go 同样在根上争用 `parMu` 和 `bytesConsumed`。试过把父指针换成 `ArcSwap<Weak>`，更慢（386 ns），已回退。A1 的分配路径每 64KiB 才上溯一次，所以单次分配的开销是 0.4 ns。是否需要进一步优化（例如手递手加锁，配合强引用父指针、弱引用子节点，代价是改变"未 Detach 的子节点仍可见"的语义），等 D01.7 / D12 的真实负载基准再定。
