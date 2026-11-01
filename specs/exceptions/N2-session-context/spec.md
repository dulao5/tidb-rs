# N2：会话与语句上下文的共享状态拆分（基础设计 spec，草案）

状态：草案。类型：实现例外（基础设计）。计划定稿：P1 之前。相关：N1（计划表示）、N3+A1（并发、取消、内存）、N8（系统变量注册表）。

## 1. 要解决的问题

Go 的 `SessionVars` 和 `StatementContext` 是两个巨大的可变结构体，通过 `sctx.GetSessionVars()` 在几乎所有模块中随处可达。它们同时被会话主 goroutine、执行器 worker、其他会话（SHOW PROCESSLIST、KILL）和后台监控读写；线程安全靠"约定 + 零散的锁和原子变量"。Rust 中如果照搬成 `Arc<Mutex<SessionVars>>`，要么到处加锁、要么死锁；照搬成裸指针则失去安全性。这件事决定了 P1 起每个块的函数签名（拿什么上下文参数），所以必须先定。

### v8.5.8 中的事实

| 事实 | 位置 | 对 Rust 的影响 |
|---|---|---|
| `SessionVars` 有约 296 个导出字段、117 个方法；源文件 3,773 行 | `sessionctx/variable/session.go` | 不能整体加锁；需要按访问方式拆 |
| 非测试代码中 `GetSessionVars()` 调用约 3,100 处，其中 planner/core 约 1,000 处 | 全库 | 绝大多数是"读某个变量的值"，只有少数写 |
| `StatementContext` 约 128 个字段；每条语句复用两个缓存对象轮换（`cachedStmtCtx[2]`） | `sessionctx/stmtctx/stmtctx.go`、`SessionVars.InitStatementContext` | Go 为省分配做的复用，Rust 无必要（L2 去掉） |
| 其他会话读本会话的 StmtCtx 时，用引用计数 + "冻结"协议防止它被重置（`ReferenceCount.TryIncrease/TryFreeze`） | `stmtctx.go` L94–130、`util/processinfo.go`、`util/util.go` | 这正是 `Arc` 的语义：持有者在，对象就在 |
| SHOW PROCESSLIST 读的是 `ProcessInfo` 快照，用 `atomic.Pointer` 整体发布 | `session.SetProcessInfo`、`session.ShowProcess` | 已经是"不可变快照 + 原子替换"，直接对应 `ArcSwap` |
| KILL 通过 `SQLKiller`（原子信号量），被嵌在 SessionVars 里，并共享给 KV 层和内存 tracker | `util/sqlkiller`、`SessionVars.SQLKiller`、`NewSessionVars` L2343/L2387 | 天然是跨线程共享的小对象，单独 `Arc` |
| StmtCtx 中执行期变化的计数（affected/found rows、warnings）由 `stmtCtxMu` 保护，因为执行器 worker 会并发追加 warning | `stmtCtxMu`、`AppendWarning` → `WarnHandler` | 需要一个可并发追加的子结构 |
| 执行器 worker 直接读 SessionVars（如 hash agg 读 `TrackAggregateMemoryUsage`、`EnableParallelHashaggSpill`、`ConnectionID`）以及 `StmtCtx.MemTracker`、`RuntimeStatsColl` | `executor/aggregate/agg_hash_executor.go` 等 | worker 需要的是语句开始时的只读值，不需要实时可变访问 |
| 事务上下文用单独的 `TxnCtxMu` 保护 | `sessiontxn/isolation/base.go` 等 | 小范围锁，保留 |
| **上游已经在拆**：表达式用窄接口 `EvalContext`、`BuildContext`、`ExprContext`；有两套实现：`sessionexpr`（实时读 SessionVars）和 `exprstatic`（不可变快照，`IntoStatic()`）。计划用 `planctx.PlanContext`，distsql 用 `DistSQLContext`，ranger 用 `RangerContext`；执行器开始给 TableReader、IndexLookUp、Projection、Selection 单独构造只读上下文，并能 `Detach()` 脱离会话 | `expression/exprctx/context.go`、`expression/sessionexpr`、`expression/exprstatic`、`planner/planctx`、`distsql/context`、`executor/detach.go`、`executor/table_reader.go` L82 等 | 设计的边界可以直接沿用这些接口，和上游演进方向一致；以后跟上游同步也更容易 |

## 2. 方案比较

| 方案 | 做法 | 评价 |
|---|---|---|
| a. 整体共享 | `Arc<RwLock<SessionVars>>`，到处 `read()` | 3,000 处加锁；worker 和会话线程争锁；KILL 时可能要等锁。**否决** |
| b. 整体独占 + 裸借用 | 会话线程持有 `&mut SessionVars`，worker 用 `unsafe` 指针读 | 等于把 Go 的数据竞争搬进 Rust。**否决** |
| c. 按访问方式拆三层 | 会话独占的可变部分、语句开始时冻结的只读快照、少量跨线程共享对象 | 编译器保证没有数据竞争；读变量无锁；与上游 `sessionexpr`/`exprstatic` 的拆法一致。**推荐** |

## 3. 设计

### 3.1 三层结构

```text
Session（连接线程独占，&mut）
 ├── SessionVars            可变；SET 语句、事务状态、用户变量、prepared 语句等
 │     └── 系统变量：只存改过的值，其余查 N8 的静态注册表
 ├── shared: Arc<SessionShared>        跨线程共享，字段都是原子或内部加锁
 │     ├── killer: SqlKiller           （原子信号；KILL / max_execution_time / 内存超限都写这里）
 │     ├── 会话根内存 tracker            （A1；内存控制器、expensivequery 从其他线程读）
 │     ├── process_info: ArcSwap<ProcessInfo>   （SHOW PROCESSLIST 读快照）
 │     └── 少量计数（InPacketBytes 等已是原子的字段）
 └── 当前语句：Arc<StmtCtx>
       ├── 只读部分：语句开始时从 SessionVars 冻结的值（sql_mode、时区、各类开关、并发度……）
       │            以及 EvalContext / BuildContext 所需的全部字段（= exprstatic 的内容）
       └── 执行期共享部分（内部加锁或原子）：warnings、affected/found rows、RuntimeStatsColl、
                                           内存与磁盘 tracker（A1 定义）、执行详情
```

- **会话线程**：拥有 `Session`，以 `&mut` 访问 `SessionVars`。解析、绑定、优化、`SET`、事务控制都在这里，按 Go 的顺序执行，没有锁。
- **执行器 worker**：只拿到 N3+A1 中定义的 `ExecCtx`（`Send + Sync`、廉价 clone：`Arc<StmtCtx>`、killer、取消令牌、语句 tracker、线程池句柄）。读变量直接读冻结值，追加 warning 走内部锁。worker 拿不到 `SessionVars`，编译器会拒绝在 worker 里写会话状态。
- **其他会话**：只能通过 `SessionManager` 拿到对方的 `Arc<SessionShared>`，读 `ProcessInfo` 快照、发送 KILL。`ProcessInfo` 里放 `Arc<StmtCtx>`，因此 Go 的引用计数 + 冻结协议直接由 `Arc` 代替（L2）。

### 3.2 语句开始时冻结什么

Go 中"实时读 SessionVars"和"读快照"的语义差别只在一种情况下可见：语句执行过程中 SessionVars 被修改。会话线程在执行期间不会处理新的 SET（同一连接是串行的），所以唯一的例外是**语句内部修改变量**：

| 情况 | Go 行为 | 处理 |
|---|---|---|
| `SELECT @a := @a + 1 ...` 等用户变量赋值 | 逐行读写 SessionVars 中的用户变量 | 用户变量不进冻结快照；`StmtCtx` 持有一个对用户变量表的共享句柄（`Arc<Mutex<UserVars>>`，只在含赋值的语句上加锁），语义与 Go 一致。对应上游 `GetUserVarsReader` |
| `SET_VAR` hint | 语句开始时临时改 SessionVars，结束后恢复 | 在冻结前应用，冻结值即包含 hint；结束后无需恢复 |
| 存储过程、触发器 | TiDB 不支持 | 不涉及 |
| `last_insert_id`、`found_rows`、`row_count` | 语句结束时写回 SessionVars | 语句结束后由会话线程从 `StmtCtx` 取回、写回（与 Go 的写回时机相同） |

冻结的具体字段列表 = 上游 `exprstatic.EvalContext`/`ExprContext` 的字段 + `DistSQLContext` + `RangerContext` + 执行器各 `*ExecutorContext` 用到的字段。P1 第一项工作就是从 v8.5.8 的这些结构机械地生成清单，每个字段标注来源变量。

### 3.3 函数签名约定（各块统一引用）

| Go | Rust | 级别 |
|---|---|---|
| `ctx exprctx.EvalContext` | `ctx: &EvalCtx`（冻结快照的视图） | L2 |
| `ctx exprctx.BuildContext` / `ExprContext` | `ctx: &mut BuildCtx`（含列 ID 分配器、plan cache 标志等会话线程上的可变状态） | L2 |
| `sctx base.PlanContext` / `planctx.PlanContext` | `ctx: &mut PlanCtx<'_>`，内含 `&mut SessionVars`、`&StmtCtx`、infoschema、N1 的 `PlanArena` | L2 |
| 执行器里的 `e.Ctx().GetSessionVars().X` | `self.ctx.x`（执行器上下文在 build 时从 `StmtCtx` 取出） | L2 |
| `sctx.GetSessionVars().StmtCtx.AppendWarning(err)` | `stmt.append_warning(err)` | L1 |

对 planner 中 1,000 处 `GetSessionVars()`：绝大多数是读变量，签名不变，只是从 `ctx.vars()` 读；少数写（如 `PlanID` 计数、`StmtCtx.SetSkipPlanCache`）对应到 `&mut PlanCtx`。来源标注照常按函数记 L1 或 L2。

### 3.4 Go 中为性能做的复用

`cachedStmtCtx[2]` 轮换复用、`StmtCtx.Reset()`、`RefCountOfStmtCtx`：Rust 中直接每条语句新建 `Arc<StmtCtx>`，旧的由最后一个持有者释放。登记为 L2（"Go 为绕开 GC 的复用，去掉"）。如果测得分配开销明显，再按 A1 的统一缓冲设计处理，不恢复 Go 的冻结协议。

## 4. 对 L1 移植的影响（示例）

Go（hash agg 打开时）：

```go
if e.Ctx().GetSessionVars().TrackAggregateMemoryUsage {
    e.memTracker.AttachTo(e.Ctx().GetSessionVars().StmtCtx.MemTracker)
}
```

Rust：

```rust
// go: HashAggExec.Open L2 (session vars read from frozen stmt snapshot)
if self.ctx.vars.track_aggregate_memory_usage {
    self.mem_tracker.attach_to(&self.ctx.stmt.mem_tracker);
}
```

逻辑相同，区别只在"从哪里读"。

## 5. 验收

1. **编译期**：执行器 worker 的闭包只捕获 `Send + Sync` 的 `Arc<StmtCtx>`、执行器上下文和取消令牌；`SessionVars` 不实现 `Sync`，无法被 worker 捕获（用一个 `compile_fail` 测试固定这一点）。
2. **行为**：tracker 中 v8.5.8 与以下特性相关的测试全部归到对应块并通过：用户变量赋值（`SELECT @a:=...`）、`SET_VAR` hint、`last_insert_id`/`found_rows`/`row_count`、warning 数量与顺序、SHOW PROCESSLIST、KILL 和 `max_execution_time`。
3. **并发**：并发执行 SHOW PROCESSLIST / KILL 与长语句的压力测试，在 `loom` 或 TSan 下无数据竞争报告。
4. **性能**：建连成本和每连接内存（与 N8 一起量）不高于 Go；sysbench 点查每语句的上下文创建开销不高于 Go 的复用方案。

## 6. 待定（需要确认）

1. 冻结快照的粒度：按语句冻结一次（推荐），还是按"执行器上下文"分别冻结（上游 `*ExecutorContext` 的做法）。前者简单；后者内存更小。建议先按语句，后者作为优化。
2. 用户变量句柄的锁：大多数语句不用用户变量。建议只在计划中出现变量赋值时才分配共享句柄，否则用只读快照。
3. 内部 SQL（`GetSQLExecutor`、`RestrictedSQLExecutor`，统计信息、DDL 等后台用）：Go 用会话池中的内部会话。建议同样建内部 `Session`，结构不变；具体在 P2 的会话块里定。
