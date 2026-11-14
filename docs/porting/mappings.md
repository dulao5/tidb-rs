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
| M-PLAN | 逻辑计划用接口指针互相引用，规则返回新子树根、由调用方 `SetChild`；物理计划原地修改，进 plan cache 时深拷贝 | 语句级 arena，节点用 `PlanId` 引用。调用 arena 中节点的方法：同名分派函数 `with_detached` 摘出节点，以 `&mut self` 调用 trait 方法（旁边是 `&mut PlanCtx`），再放回；`PlanCtx::split()` 只作备用。物理计划从一开始就是 `Arc` 树：写节点用 `make_mut_plan`（独占时原地改，共享时浅拷贝一个节点），改子节点用 `map_children`；plan cache 命中后的改写用同一机制做路径复制 | N1 §3.4、§3.5、§4 | — |
| M-EXPR | 表达式指针在计划节点间共享，修改前先 `Clone()` | `Arc<Expr>` 不可变共享，写时复制 | N1 | — |
| M-FT | 改 `GetType()` 返回的 `*FieldType`（赋值 `.RetType =`，方法调用 `SetFlag`/`DelFlag`/`SetFlen`/`SetDecimal`/`SetCharset`/`SetCollate`/`SetType` 等）。Go 的 `Column.Clone()`、`Schema.Clone` 等只复制列，`FieldType` 指针仍共享，修改对同一查询中所有持有者可见（memory table 上甚至是进程全局的，#148）；`SetSchema(x.Schema())` 不 Clone 时 `*Schema` 同理 | **默认写时复制**：`Column.ret_type`、签名的 `tp` 是 `Arc<FieldType>`（N13 §8，共享 Go 共享的同一个类型），只经 `Arc::make_mut` 修改（`Column::ret_type_mut`、`ScalarFunction::ret_type_mut`），只改本处的副本；`Arc` 的指针身份不用来让写入对别的持有者可见。只有当 Go 的别名会改变 golden test 或 EXPLAIN 对照能看到的计划时，才在**那一处**显式模拟：除本处副本外，按 UniqueID 同步改同一查询中来源列的类型；加注释 `// Go aliasing (M-FT): <file>.go:<line>, <effect>`，配回归测试，在块 plan 的"已知差异 / 模拟"表中登记。跨查询的全局效果（#148）不模拟。只改 charset / collation / decimal 的修改，到达发给客户端的列定义时按可见差异处理。每块 plan 都运行两个审计 grep（赋值 `.Index =` / `.RetType =`；方法调用 `SetFlag`/`DelFlag`/`SetFlen`/`SetCharset`/`SetCollate`/…），逐处给出 S/V0/V1 结论 | N1 §3.3、§5；D09.7 试点报告 §3.3、§6、附录 A/B；#149 | `grep -rn 'Go aliasing (M-FT)'` 列出全部模拟位置，与块 plan 的登记表对照 |
| M-SELF | `BaseLogicalPlan.self` 字段回调具体算子；struct 嵌入（`LogicalProjection` 嵌 `LogicalSchemaProducer` 嵌 `BaseLogicalPlan`）；有副作用的 getter | trait 默认方法。嵌入结构用宏生成的 `base()` / `schema_producer()`（及 `_mut`）访问器，嵌入结构的方法写成默认方法；有副作用的 getter（如惰性从子节点克隆 schema）用 `OnceCell`，读路径仍是 `&self`；trait 方法多接收 `this: PlanId`（节点自己的下标），Go 的 `return p` 写成 `Ok(this)`、`s.Self()` 传 `this`；指针比较 `p.Schema() == child.Schema()` 用 `std::ptr::eq` | N1 §3.7 | — |
| M-SHARE | 长期存在的结构（InfoSchema、表对象、分区、缓存）共享的 `*T` / `[]*T`，持有者可以通过指针原地修改，修改对所有持有者可见（#148） | `Arc<T>` / `Vec<Arc<T>>`，只读共享；修改只经 `Arc::make_mut`（D06.1 模型类型用 `tidb_model::make_mut`，复制用 `GoCopy`）。共享的元数据内部不放 `Mutex` / `RefCell` / `Cell` / 原子类型等内部可变性；指针只向下，子对象找父对象按 ID 或参数，不存父对象的 `Arc` / `Weak`。`GoCopy`（Go `nt := *t`）克隆 `Arc`；手写的 `Clone`（Go `Clone` 方法）在 Go 深拷贝处深拷贝（`Arc::new(T::clone(&x))`）。Go 通过共享指针写入的位置逐处分为"Go 应该先复制"（写时复制即可）和"依赖共享可见性"（改设计，在块 plan 中写明）。单条语句、单行内的值仍按值持有或按 N1 | N13 §3、§4、§8；#229 | 编译期：共享的值只能拿到 `&T`；CI：`scripts/check_model_mutability.py`（`crates/model/src` 无内部可变性），`clippy::clone_on_ref_ptr`（克隆 `Arc` 写 `Arc::clone(&x)`）；评审按 N13 §7 |
| M-IFACE | 接口 + 类型断言 / `switch x := p.(type)` | trait + `kind()` 枚举 `match`；必要时 `as_any` 向下转型 | N1 | — |
| M-CTX | 随处传 `sessionctx.Context`，直接读写 `SessionVars` / `StmtCtx` | 三层上下文：会话独占的可变 `SessionVars`；跨线程共享的 KILL / PROCESSLIST 小对象；语句开始时冻结的只读 `ExecCtx`。窄接口沿用 Go 的 `exprctx` / `planctx` / `DistSQLContext` 边界 | N2 | `ExecCtx: Send + Sync`、`Session: Send` 静态断言 |
| M-GOCTX | `context.Context` | 取消令牌 + `ExecCtx` 中的少数值（request source、internal source type、tracing span） | N3 | — |
| M-GO | `go f()` 启动 worker | 在 ExecPool 上 `spawn` 一个 task；worker 数量与 Go 相同，物理并行度由池大小决定 | N3 | 执行器 crate 禁止 `std::thread::spawn` |
| M-CH | channel 收发、`select` | 取消安全的异步 channel，容量照抄 Go；`select!`。所有收发都经过 `ctx.send` / `ctx.recv`，同时监听取消 | N3 | 禁止直接调用 channel 的 `send` / `recv` |
| M-WG | `sync.WaitGroup`、`errgroup` | 保存 `JoinHandle`，关闭时逐个 `await` | N3 | — |
| M-KILL | `HandleSQLKillerSignal` 等检查点；hash build、sort 等不经过 `Next` 的循环 | `ctx.checkpoint().await`，放在 Go 的检查点位置；不经过 `Next` 的循环（hash build、sort、merge、聚合 final 合并的内循环）也调用。每次都做 KILL 检查和内存检查（A1），也每次都看时钟（Go 的检查点是 chunk 级的），本次 poll 超过时间片（默认 1ms）才让出 CPU。逐行调用的内层循环用 `CheckpointStride::new(n)`，每 n 次看一次时钟（TiKV `RescheduleChecker` 的模式） | N3、A1 | — |
| M-PANIC | 用 `panic` / `recover` 处理内存超限、KILL 和运行时错误 | 返回 `Result`；只在 task 边界用 `catch_unwind` 兜底，转成与 Go 相同的错误文本 | N3 | — |
| M-BLOCK | 在 goroutine 里直接做阻塞 I/O（落盘等）；后台任务的长循环 | `spawn_blocking` 或专用阻塞池（N3 3.6 的 Q3）；后台任务的长循环放时间片让出点（N3 3.7） | N3 | 执行器 crate 禁止 `Runtime::block_on` |
| M-LOCK | `sync.Mutex` / `RWMutex` | `parking_lot` 的锁（无毒化）；不得跨 `await` 持有 | N3 | 禁用 `std::sync::Mutex`；`clippy::await_holding_lock` |
| M-SNAP | `atomic.Pointer` / `atomic.Value` 发布快照 | `ArcSwap`。`load()` 返回的 guard 只用于短暂持有（一次读取、一个表达式内）：guard 占用 arc-swap 每个线程为数不多的快速槽位，长时间持有或同时持有多个时，之后的 `load()` 会退化到较慢的路径（每次增减引用计数）。需要跨 await 或长时间持有快照时，用 `load_full()`，或该模块提供的 `*_arc()` 访问函数（如 `get_global_config_arc()`），拿到独立的 `Arc` | N2 | — |
| M-DEFER | `defer` | `Drop` / scope guard | — | — |
| M-TICK | 后台 goroutine 定时轮询 | 运行时定时器。按语句的（expensivequery 检查 `max_execution_time`）：每条语句一个 deadline，到期发送与 Go 相同的 KILL 信号。按进程的（`servermemorylimit` 每 100ms 读内存用量、SQLKiller 检查连接存活）：一个周期定时器，周期与 Go 相同。判断逻辑（选谁、发什么信号、日志）仍按 L1 | N3 | — |
| M-MEM | `memory.Tracker.Consume` 按估算值记账 | `TrackedAlloc`：按实际分配记到同一棵 tracker 树；超限动作链按 L1 | A1 | — |
| M-POOL | `sync.Pool`、`chunk.Allocator` 等为减轻 GC 的对象复用 | 删除。相关变量按"接受并返回 warning"处理（N8） | A1、N8 | — |
| M-INT | Go 定宽整数：`int`、`int8`…`uint64`、下标、溢出回绕 | `int` → `isize`（下标处 `as usize`），其余同宽对应；Go 中可能回绕的运算一律显式写 `wrapping_*` / `as` 截断（Rust debug 构建溢出会 panic），并在块 plan 中列出位置 | D01.2 试点 | debug 构建跑全部向量 |
| M-ALIAS | 输出参数可以与输入是同一个对象（如 `x.Round(x, …)`、`DecimalAdd(a, b, a)`） | 提供"写入另一个对象"和"原地"两个入口；核心函数接收输入的快照（`Copy` 类型）和"是同一对象"标志。前提是逐行确认 Go 对输入的读取都在对输出的写入之前，并在函数注释中写明 另外，Go 的切片别名（`buf[:n:n]`、`SharedBytes`、`MakeRef`）→ `Bytes` / `ColBuf::Shared` 加写时复制 | D01.2 试点、N5a | 共享列的写入口在 debug 构建下断言引用计数为 1（N5a） |
| M-MAPORDER | 按 map 的随机顺序遍历（如会话初始化时逐个调用变量钩子、BurntSushi/toml 按 map 解码表、`Error()` 打印 map） | 用确定的顺序（如注册表顺序、键序）。逻辑本身与顺序无关时，块 plan 中逐处登记并说明为什么无关。**Go 自己的结果依赖顺序时**（如一个文件有多个解码错误时报哪一个、消息中 map 项的先后），Go 可能给出的任一结果都符合规格，Rust 取确定顺序下的那一个；govec 向量只保留与顺序无关的记录：重复运行结果稳定，并且生成器按结构排除依赖顺序的用例（小 map 的迭代顺序并不均匀，只靠重复运行不能可靠地发现） | N8；D00.2 评审 | govec 生成器的确定性检查（`TestDomains` 两次生成逐字节相同） |
| M-ERRVAL | 返回 `error`，或返回 `(T, error)` 且出错时 T 仍有意义（如 `ToInt` 返回 `MaxInt64` + `ErrOverflow`） | 只返回 `error` → `Result<(), Error>`；`(T, error)` 中所有出错路径的 T 都是零值 → `Result<T, Error>`，否则 → `(T, Result<(), Error>)`。修改接收者并返回错误的方法，出错时接收者的内容与 Go 相同 | D01.2 试点 | 向量比对出错时的值 |
| M-NAME | Go 方法名与 Rust 标准 trait 的方法同名（`String`、`ToString`、`Clone`、`Hash`、`Equal` 等） | 保留 Go 名的 snake_case 作为固有方法（如 `to_string() -> Vec<u8>`）；需要 `Display` 等 trait 时委托给它，委托代码标 `glue:` | D01.2 试点 | — |
| M-EVAL | 内置函数的逐行版 `evalXxx` 和向量化版 `vecEvalXxx` 两份实现 | 函数体只按逐行版移植一份，宏生成逐行和向量两种驱动；在 Go 走向量化的地方走向量驱动。两版不一致时以逐行版为准并开 issue | N4 | govec 对两种驱动分别比对 |
| M-ROW | 跨 `Next` 调用保存 `chunk.Row` | `RowHandle { Arc<Chunk>, idx }`；调用内仍用借用的 `Row<'a>` | N5a | — |
| M-FMA | 浮点乘加 `x*y + z`（含 `f += x*y`）。Go 规范允许编译器把它融合成一条 FMA 指令（只舍入一次），gc 是否融合取决于**平台**（GOARCH，amd64 还取决于 GOAMD64）和**具体位置**，只能从生成的汇编判断，不能从源码形状推断 | 按位置、按目标平台写：Go 在该目标上融合的位置用 `f64::mul_add`，不融合的用分开的乘和加（Rust 自己从不融合）。用 `#[cfg(target_arch = ...)]` 区分，条件照登记表写。amd64 的基线是 GOAMD64=v1（Go 的默认值，v8.5.8 的 Makefile 没有设置），v1 不融合任何位置；v3 只作参考。登记表是 tracker 的 `data/go_fma_sites.tsv`（`scripts/go_fma_sites.py` 对每个目标做 `go build -gcflags=<包>=-S`，扫描 `FMADDD`/`VFMADD231SD` 等指令并映射回源码行，每行 = 一个位置 × 各目标 yes/no）。函数标注写 `[M-FMA]`，每个位置旁注释 `// [M-FMA] <文件>.go:<行>`。向量按目标分别生成（govec 的 `vectors/<版本>/<目标>/`），测试按 Rust 的 target_arch 回放对应的一份 | D01.2 评审 | `converge.py --fma-sites`：登记表中在任一目标上融合、且所在 Go 文件已移植的每个位置，Rust 中都有 `[M-FMA] 文件:行` 注释，所在函数的 `go:` 标注带 `M-FMA`；注释不能指向登记表外的行 |
| M-F2I | 浮点转整数 `int64(f)`、`int32(f)`、`uint64(f)` 等，值超出目标类型或为 NaN。Go 规范说结果由实现决定，gc 的结果取决于**平台**：amd64 用 `CVTTSD2SQ`/`CVTTSD2SL`（越界和 NaN 得到最小值；`uint64` 先比较 2^63 再分两支（`int64(f - 2^63) \| 1<<63`，NaN 和 ≥ 2^64 得 2^63），负数按 int64 转换后重解释），arm64 用 `FCVTZS`/`FCVTZU`（饱和，NaN 得 0）；`int8/16`、`uint8/16` 先转 int32 再截断，`uint32` 先转 int64 再截断 | 一律调用 `gostd::conv::f64_to_{i64,i32,i16,i8,u64,u32,u16,u8}`（按 `target_arch` 复现 gc 的指令语义，表见该模块文档），不直接写 `as`。函数标注带 `[M-F2I]`，调用处注释 `// [M-F2I]`。这是用户可见的平台差异，Rust 照样复刻：例如 `ParseTimeFromFloat64(NaN)`（D01.3）中 `int64(NaN)` 在 arm64 上得 0，结果是 `0000-00-00`、没有错误；在 amd64 上得 MinInt64，报 `Incorrect time value` | D00.2 评审 | govec 向量按目标分别生成（`config` 领域的 `f2i`：各目标类型的边界、越界、NaN、±Inf、随机值） |
| M-ERR | `errors.Trace` / `errors.Annotate` 包装调用栈；`error` 接口 | 一个具体错误类型 `tidb_base::Error`（字段装箱，`Result<T, Error>` 只占一个指针宽）：错误码 + RFC 码 + 消息模板与参数 + 可选 cause + 创建点（`#[track_caller]`）+ 可选调用栈；不移植包装函数，`errors.Cause(err)` 即 `err`。Go 的 string 是字节串：`Error()`、`GetMsg()`、消息模板和 `fmt` 参数都是字节（`Vec<u8>` / `Arg::Str`），`Display` 只给日志、可有损。Go 调用方对具体错误类型做类型断言或 type switch 的地方（如 `config.Load` 的 `*ErrConfigValidationFailed`），返回枚举：每个被断言的类型一个变体，加 `Other(Error)`（D00.2 块 spec §4 第 7 条） | D00.2 块 spec §4 | govec `base` 向量（含非 UTF-8 参数） |
| M-BYTES | Go `string`：可能是任意字节（Go 的 string 是字节串），也可能只承载 UTF-8 文本 | 值来自 JSON（schema meta、配置）或 SQL 标识符、字符集名等 lexer 产出的文本：`String`（JSON 解码已把无效 UTF-8 换成 U+FFFD，与 Go 相同）。值可能是用户的原始字节（BIT / BINARY 默认值、ENUM 的二进制字面量元素、`Error()` 消息与参数、KV 键值）：`Vec<u8>`；`interface{}` 中由 Go 代码放进去的这类 string 用 `AnyValue::GoString(Vec<u8>)`。同一 Go 字段两种来源都有时按 `Vec<u8>` | D02.3 spec 3.2；D06.1 评审 | govec 向量含非 UTF-8 的值（`vec.GoString` 的 hex 形式） |
| M-GONAME | 包级常量、静态量、数据表（`mysql.ErrDupEntry`、`mysql.TypeLonglong`、`terror.ClassDDL`、`plannererrors.ErrUnknownColumn`、`mysql.MySQLErrName`） | 保留 Go 的标识符（不转 SCREAMING_CASE），便于与 Go 逐个 grep 对照。只用于**数据标识符**：函数、方法仍按 `M-NAME` 用 snake_case，类型用 CamelCase（Go 的小写类型名也转成 CamelCase）。声明或用作 `match` 模式的文件在模块级 `#![allow(non_upper_case_globals)]`（生成的文件自带） | D00.2 评审 | lint：只允许 `non_upper_case_globals`，`non_snake_case`/`non_camel_case_types` 不在豁免表中 |

## 移植模块的统一约定

- **lint**：模仿 Go 分支结构的代码会触发一些 clippy 风格 lint（`int_plus_one`、`needless_range_loop`、`manual_range_contains`、`needless_late_init`、`field_reassign_with_default`、`manual_slice_fill`）。这些在 workspace 的移植 crate 中统一 allow，正确性 lint 全部保留。不要在单个文件中各写一份。
- **注释**：Go 注释原样复制；其中缩进 4 格的段落会被 rustdoc 当作代码块，要用 ```` ```text ```` 包起来。
- **浮点**：乘加按 `M-FMA` 处理。块 plan 中列出本块在登记表里的位置；没有登记的乘加（各目标都不融合）照 Go 写分开的乘和加即可。Go 升级工具链后要重新生成登记表，融合位置可能变化。
- **全局分配器**：workspace 统一使用 jemalloc（与 A1 的实例内存度量一致），基准和服务端用同一个分配器。

## 机器检查的落地

执行器、distsql 及相关 crate 的 `clippy.toml`：

- `disallowed_methods`：channel 的直接 `send` / `recv`（`crossbeam_channel`、`std::sync::mpsc`、异步 channel）、`std::thread::spawn`、`tokio::runtime::Runtime::block_on`。
- `disallowed_types`：`std::sync::Mutex`、`std::sync::RwLock`。
- `await_holding_lock` 设为 deny。

这些检查和来源标注 lint 在同一个 CI 检查里运行（宪章第二条 5）。
