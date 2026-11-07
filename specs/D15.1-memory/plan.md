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

**有增长才执行**（D15.1 评审）：`record` 在 `bs > 0` 时沿路径给每个 tracker 置"有增长"标志（已置位就只读不写，减少共享节点上的写），`checkpoint` 读出并清零自己的标志，没有增长就直接返回，对应 Go 只在 `Consume` 的 `bs > 0` 时执行动作。这样超限但只在释放内存的算子不会每个 chunk 都执行动作链，也就不会在落盘后立刻沿 fallback 升级（`tests/checkpoint.rs`）。祖先的检查点能看到后代的增长；后代自己的检查点各自消耗自己的标志。代价：两次检查点之间的瞬时峰值看不到，作为 A1 的偏离写进 A1 spec 3.3。

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
| `cgroup.rs` | 不解析 `/proc/self/mountinfo`，挂载点假定为 `/sys/fs/cgroup`（v2）或 `/sys/fs/cgroup/memory`（v1）；`InContainer` 与 Go 相同 | 临时的简化版。D15.1 评审指出它在 cgroup v1 的宿主机上会读错（不用 `/proc/self/cgroup` 里的路径，systemd `MemoryLimit=` 时退回整机内存），而这个值是 `tidb_server_memory_limit` 默认值的基础。tracker 把 `pkg/util/cgroup` 的内存部分改为新块 **D15.3**（tracker PR #7，CPU 部分仍为 X），也落在 `tidb-memory`；**必须在 servermemorylimit 之前按 L1 移植**，`cgroup_mock_test.go` 的 v1、v2、混合、docker、k8s fixture 做成 govec 向量 |
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

`Consume` 在"所有线程打同一个根"的压力下比 Go 慢约 40%。多出来的是每上溯一层对父节点 `Arc` 强计数的一加一减（`Weak::upgrade` 和 drop），在共享的根上形成争用。Go 同样在根上争用 `parMu` 和 `bytesConsumed`。试过把父指针换成 `ArcSwap<Weak>`，更慢（386 ns），已回退。A1 的分配路径每 64KiB 才上溯一次，所以单次分配的开销是 0.4 ns。评审（Lucky，Linux arm64 10 核）把开销拆开：大头是共享 root 上 `parMu`（parking_lot，争用时 park / futex 唤醒）的争用，其次是 `upgrade` 的 CAS（约 27ns）；Go 的 `getParent` 同样锁 `parMu`。v8.5.8 中 session 的 tracker 不挂到全局 tracker，实际的共享父节点是同一条语句的执行器 tracker，并发度是算子的 worker 数。所以现在不优化，门槛是 A1 §5 的"记账开销 ≤ 2%"，在 D01.7、D12 接上后测；超标时的两个候选（不要把普通父指针改成强引用：子节点是强引用，两边都强就成环）：

1. **全局 tracker 的父指针用强引用**：全局 tracker 不保留子节点，子节点对它的强引用不成环；它是共享程度最高的节点，可以省掉对它的 `upgrade`。
2. **拆出记账链**：每个 Tracker 持有 `Arc<Counters>`（consumed、released、max、limits、label、session 根标志、killer），Counters 之间的父指针用强引用的 `ArcSwapOption`（Counters 不持有子节点，不成环），上溯时用 guard，不动引用计数；需要执行动作时，再通过 Counters 上指回 Tracker 的 `Weak` 做一次 `upgrade`。改动较大。

评审中的测量（本地实验，未提交）：`ArcSwapOption` 强引用 162ns、`Mutex<Option<Arc>>` 181ns、PR 原样 208ns、`ArcSwapAny<Weak>` 229ns、`RwLock<Weak>` 326ns；Go 133–149ns。
