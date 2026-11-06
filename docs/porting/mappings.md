# 全局映射规则：Go 的承载方式 → Rust 对应物

状态：草案。各条随所依据的基础设计 spec 一起定稿。规则依据 [宪章](../../CONSTITUTION.md) 第二条。

## 用途

Go TiDB 的很多写法只是因为 Go 运行时才那样写（goroutine、channel、GC、panic/recover、`sync.Pool`）。这些写法在 Rust 里有成熟的对应物，应当**全局决定一次**，而不是每个块各自决定。

- 按本表替换载体、业务逻辑不变的代码，等级仍是 **L1**。标注只引用规则 ID，例如 `// go: (*ProjectionExec).Next L1 [M-GO, M-CH]`。
- 本表没有覆盖的偏离才是 L2，必须逐处写理由。同一种 L2 出现第三次时，应提出新增一条规则。
- tracker 从标注中统计每条规则的使用位置。上游同步或规则修订时，据此找到受影响的代码。
- 新增或修改规则需要项目负责人确认，和例外 spec 的流程相同。

## 规则

| ID | Go 写法 | Rust 对应物 | 依据 | 机器检查 |
|---|---|---|---|---|
| M-PLAN | 逻辑计划用接口指针互相引用，规则返回新子树根、由调用方 `SetChild` | 语句级 arena，节点用 `PlanId` 引用；物理计划在 `DoOptimize` 结束时冻结为不可变 `Arc` 树 | N1 | — |
| M-EXPR | 表达式指针在计划节点间共享，修改前先 `Clone()` | `Arc<Expr>` 不可变共享，写时复制 | N1 | — |
| M-SELF | `BaseLogicalPlan.self` 字段回调具体算子 | trait 默认方法 | N1 | — |
| M-IFACE | 接口 + 类型断言 / `switch x := p.(type)` | trait + `kind()` 枚举 `match`；必要时 `as_any` 向下转型 | N1 | — |
| M-CTX | 随处传 `sessionctx.Context`，直接读写 `SessionVars` / `StmtCtx` | 三层上下文：会话独占的可变 `SessionVars`；跨线程共享的 KILL / PROCESSLIST 小对象；语句开始时冻结的只读 `ExecCtx`。窄接口沿用 Go 的 `exprctx` / `planctx` / `DistSQLContext` 边界 | N2 | `ExecCtx: Send + Sync`、`Session: Send` 静态断言 |
| M-GOCTX | `context.Context` | 取消令牌 + `ExecCtx` 中的少数值（request source、internal source type、tracing span） | N3 | — |
| M-GO | `go f()` 启动 worker | 在 ExecPool 上 `spawn` 一个 task；worker 数量与 Go 相同，物理并行度由池大小决定 | N3 | 执行器 crate 禁止 `std::thread::spawn` |
| M-CH | channel 收发、`select` | 取消安全的异步 channel，容量照抄 Go；`select!`。所有收发都经过 `ctx.send` / `ctx.recv`，同时监听取消 | N3 | 禁止直接调用 channel 的 `send` / `recv` |
| M-WG | `sync.WaitGroup`、`errgroup` | 保存 `JoinHandle`，关闭时逐个 `await` | N3 | — |
| M-KILL | `HandleSQLKillerSignal` 等检查点；hash build、sort 等不经过 `Next` 的循环 | `ctx.checkpoint().await`，放在 Go 的检查点位置；不经过 `Next` 的循环（hash build、sort、merge、聚合 final 合并的内循环）也调用。每次都做 KILL 检查和内存检查（A1）；超过时间片（默认 1ms，TiKV `RescheduleChecker` 的模式）才让出 CPU | N3、A1 | — |
| M-PANIC | 用 `panic` / `recover` 处理内存超限、KILL 和运行时错误 | 返回 `Result`；只在 task 边界用 `catch_unwind` 兜底，转成与 Go 相同的错误文本 | N3 | — |
| M-BLOCK | 在 goroutine 里直接做阻塞 I/O（落盘等）；后台任务的长循环 | `spawn_blocking` 或专用阻塞池（N3 3.6 的 Q3）；后台任务的长循环放时间片让出点（N3 3.7） | N3 | 执行器 crate 禁止 `Runtime::block_on` |
| M-LOCK | `sync.Mutex` / `RWMutex` | `parking_lot` 的锁（无毒化）；不得跨 `await` 持有 | N3 | 禁用 `std::sync::Mutex`；`clippy::await_holding_lock` |
| M-SNAP | `atomic.Pointer` / `atomic.Value` 发布快照 | `ArcSwap` | N2 | — |
| M-DEFER | `defer` | `Drop` / scope guard | — | — |
| M-TICK | 后台 goroutine 定时轮询 | 运行时定时器。按语句的（expensivequery 检查 `max_execution_time`）：每条语句一个 deadline，到期发送与 Go 相同的 KILL 信号。按进程的（`servermemorylimit` 每 100ms 读内存用量、SQLKiller 检查连接存活）：一个周期定时器，周期与 Go 相同。判断逻辑（选谁、发什么信号、日志）仍按 L1 | N3 | — |
| M-MEM | `memory.Tracker.Consume` 按估算值记账 | `TrackedAlloc`：按实际分配记到同一棵 tracker 树；超限动作链按 L1 | A1 | — |
| M-POOL | `sync.Pool`、`chunk.Allocator` 等为减轻 GC 的对象复用 | 删除。相关变量按"接受并返回 warning"处理（N8） | A1、N8 | — |
| M-INT | Go 定宽整数：`int`、`int8`…`uint64`、下标、溢出回绕 | `int` → `isize`（下标处 `as usize`），其余同宽对应；Go 中可能回绕的运算一律显式写 `wrapping_*` / `as` 截断（Rust debug 构建溢出会 panic），并在块 plan 中列出位置 | D01.2 试点 | debug 构建跑全部向量 |
| M-ALIAS | 输出参数可以与输入是同一个对象（如 `x.Round(x, …)`、`DecimalAdd(a, b, a)`） | 提供"写入另一个对象"和"原地"两个入口；核心函数接收输入的快照（`Copy` 类型）和"是同一对象"标志。前提是逐行确认 Go 对输入的读取都在对输出的写入之前，并在函数注释中写明 另外，Go 的切片别名（`buf[:n:n]`、`SharedBytes`、`MakeRef`）→ `Bytes` / `ColBuf::Shared` 加写时复制 | D01.2 试点、N5a | 共享列的写入口在 debug 构建下断言引用计数为 1（N5a） |
| M-MAPORDER | 按 map 的随机顺序遍历（如会话初始化时逐个调用变量钩子） | 用确定的顺序（如注册表顺序），并要求该处逻辑不依赖顺序；块 plan 中逐处登记并说明为什么与顺序无关 | N8 | — |
| M-ERRVAL | 返回 `error`，或返回 `(T, error)` 且出错时 T 仍有意义（如 `ToInt` 返回 `MaxInt64` + `ErrOverflow`） | 只返回 `error` → `Result<(), Error>`；`(T, error)` 中所有出错路径的 T 都是零值 → `Result<T, Error>`，否则 → `(T, Result<(), Error>)`。修改接收者并返回错误的方法，出错时接收者的内容与 Go 相同 | D01.2 试点 | 向量比对出错时的值 |
| M-NAME | Go 方法名与 Rust 标准 trait 的方法同名（`String`、`ToString`、`Clone`、`Hash`、`Equal` 等） | 保留 Go 名的 snake_case 作为固有方法（如 `to_string() -> Vec<u8>`）；需要 `Display` 等 trait 时委托给它，委托代码标 `glue:` | D01.2 试点 | — |
| M-EVAL | 内置函数的逐行版 `evalXxx` 和向量化版 `vecEvalXxx` 两份实现 | 函数体只按逐行版移植一份，宏生成逐行和向量两种驱动；在 Go 走向量化的地方走向量驱动。两版不一致时以逐行版为准并开 issue | N4 | govec 对两种驱动分别比对 |
| M-ROW | 跨 `Next` 调用保存 `chunk.Row` | `RowHandle { Arc<Chunk>, idx }`；调用内仍用借用的 `Row<'a>` | N5a | — |
| M-ERR | `errors.Trace` / `errors.Annotate` 包装调用栈 | 错误码 + 可选的创建点调用栈；不移植包装函数 | D00.2 块 spec | — |

## 移植模块的统一约定

- **lint**：模仿 Go 分支结构的代码会触发一些 clippy 风格 lint（`int_plus_one`、`needless_range_loop`、`manual_range_contains`、`needless_late_init`、`field_reassign_with_default`、`manual_slice_fill`）。这些在 workspace 的移植 crate 中统一 allow，正确性 lint 全部保留。不要在单个文件中各写一份。
- **注释**：Go 注释原样复制；其中缩进 4 格的段落会被 rustdoc 当作代码块，要用 ```` ```text ```` 包起来。
- **浮点**：Go 允许编译器把 `x*y + z` 融合为 FMA（arm64 上会发生），Rust 不会。含乘加的浮点逻辑在块 spec 中标出，向量要在 amd64 和 arm64 上各生成一次比对。
- **全局分配器**：workspace 统一使用 jemalloc（与 A1 的实例内存度量一致），基准和服务端用同一个分配器。

## 机器检查的落地

执行器、distsql 及相关 crate 的 `clippy.toml`：

- `disallowed_methods`：channel 的直接 `send` / `recv`（`crossbeam_channel`、`std::sync::mpsc`、异步 channel）、`std::thread::spawn`、`tokio::runtime::Runtime::block_on`。
- `disallowed_types`：`std::sync::Mutex`、`std::sync::RwLock`。
- `await_holding_lock` 设为 deny。

这些检查和来源标注 lint 在同一个 CI 检查里运行（宪章第二条 5）。
