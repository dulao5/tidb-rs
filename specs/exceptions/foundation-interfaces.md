# 基础设计 spec 之间的接口汇总

状态：草案，随 N1、N2、N3+A1、A4（以及之后的 N4、N5a、N8）一起定稿；N1 部分已于 2026-11-14 确认（#149）。各块 plan 模板中的函数签名以本页为准。这里只列类型签名和"谁拥有谁"。理由和细节看各 spec，映射规则看 [mappings.md](../../docs/porting/mappings.md)。

## 1. 所有权关系

```
连接任务（唯一的 tokio 运行时；Steal 或 NoSteal 拓扑，N3 3.1）
 └─ Session: Send                                   N2   由连接任务持有；语句在同一个 task 里执行，不交接
     ├─ vars: SessionVars            (!Sync)        N2   只有持有会话的任务可写；系统变量存储见 N8
     ├─ shared: Arc<SessionShared>   (Send+Sync)    N2   其他会话和后台可见：SqlKiller、会话根 MemTracker、ArcSwap<ProcessInfo>
     └─ 语句（在连接任务内；先过 Q1 准入，N3 3.6）
         ├─ PlanCtx<'s>                              N1/N2 规划期：&mut SessionVars、ExprContextRef、&StmtCtx、InfoSchema、PlanArena（字段公开）
         │    └─ PlanArena                            N1   逻辑计划节点（Box<dyn LogicalPlan>），PlanId 引用；调用方法时摘出再放回；随规划结束释放
         ├─ Arc<StmtCtx>             (Send+Sync)     N2   冻结的只读变量 + 执行期共享部分（warnings、RuntimeStatsColl、语句 tracker）
         ├─ Arc<dyn PhysicalPlan>    (Send+Sync)     N1   从一开始就是 Arc 树：独占时原地改，共享后写时复制；可进 plan cache
         └─ 执行器树 Box<dyn Executor> (Send)          N3   由物理计划构建；worker task 只拿 ExecCtx
              └─ worker task（pool.spawn，仅当计划带并行 worker） N3   捕获 ExecCtx + 自己的 Arc<…Shared>；子执行器可被 move 进 fetcher，Close 前交回

服务器级共享对象（Arc，进程内唯一）：Q1 Admission、Q2 StoreWindows（每 store 一个在途窗口）、Q3 阻塞池、ExecPool 句柄   N3 3.6
```

## 2. 类型签名（示意）

```rust
// ---- N1 计划表示 ----
pub struct PlanId(u32);                         // arena 下标，仅内部使用
pub struct PlanArena { /* Vec<Option<Box<dyn LogicalPlan>>>, 旁表 task_map 等 */ }
impl PlanArena {
    pub fn take(&mut self, id: PlanId) -> Box<dyn LogicalPlan>;     // 摘出；重复摘出 panic
    pub fn put(&mut self, id: PlanId, p: Box<dyn LogicalPlan>);    // 放回
    pub fn schema_mut(&mut self, id: PlanId) -> &mut Schema;       // 写到 schema 的拥有者（Go 的 Schema() 可能返回子节点的）
}
/// 标准写法（N1 §4）：摘出节点，以 &mut self 调用方法（旁边是 &mut PlanCtx），再放回
pub fn with_detached<R>(ctx: &mut PlanCtx<'_>, id: PlanId,
    f: impl FnOnce(&mut dyn LogicalPlan, &mut PlanCtx<'_>) -> R) -> R;
pub trait LogicalPlan: Any + Send {
    fn kind(&self) -> LogicalKind;              // M-IFACE：type switch → match
    fn id(&self) -> i32;                        // Go 的 plan id（PlanID.Add(1)），不同于 PlanId
    fn as_any(&self) -> &dyn Any;
    fn base(&self) -> &BaseLogicalPlan;         // M-SELF：Go 嵌入结构的访问器，宏生成
    fn base_mut(&mut self) -> &mut BaseLogicalPlan;
    fn schema_producer(&self) -> Option<&LogicalSchemaProducer> { None }
    fn schema_producer_mut(&mut self) -> Option<&mut LogicalSchemaProducer> { None }
    // …与 Go base.LogicalPlan 一一对应的方法：fn xxx(&mut self, ctx: &mut PlanCtx, this: PlanId, …)
    //   this = 节点自己的 PlanId（Go 的 return p → Ok(this)）；嵌入结构的方法写成默认方法
}
// 每个 Go 接口方法一个同名分派函数：fn xxx(ctx: &mut PlanCtx, p: PlanId, …) { with_detached(ctx, p, |n, ctx| n.xxx(ctx, p, …)) }
pub struct LogicalSchemaProducer { schema: OnceCell<Schema>, /* … */ } // 有副作用的 getter（惰性从子节点克隆）用 OnceCell
pub trait PhysicalPlan: Any + Send + Sync {
    fn kind(&self) -> PhysicalKind; fn id(&self) -> i32;
    fn children(&self) -> &[PhysicalPlanRef];
    fn children_mut(&mut self) -> &mut Vec<PhysicalPlanRef>;
    fn clone_node(&self) -> PhysicalPlanRef;    // 浅拷贝一个节点：子节点、表达式仍共享（取代 Go 的 CloneForPlanCache 深拷贝）
    /* … */
}
pub type PhysicalPlanRef = Arc<dyn PhysicalPlan>;
pub fn make_mut_plan(p: &mut PhysicalPlanRef) -> &mut dyn PhysicalPlan;  // 独占时原地；共享时先 clone_node（M-PLAN）
pub fn map_children(p: &mut PhysicalPlanRef, f: impl FnMut(PhysicalPlanRef) -> PhysicalPlanRef); // p.Children()[i] = f(child)；共享时只在子节点变化时复制
pub type Expr = Arc<Expression>;                // 不可变共享，写时复制（M-EXPR）；具体结构见 N4

// ---- N2 上下文 ----
pub struct Session { pub vars: SessionVars, pub shared: Arc<SessionShared>, /* … */ }
pub struct SessionShared { pub killer: SqlKiller, pub mem_root: Arc<MemTracker>, pub process: ArcSwap<ProcessInfo> }
pub struct StmtCtx { pub vars: FrozenVars, pub cop_meta: CopRequestMeta, /* warnings, rows, runtime stats, mem/disk tracker … */ }
pub struct PlanCtx<'s> {                         // 字段公开：同一函数内 ctx.arena / ctx.expr_ctx / ctx.vars 是互不重叠的字段借用
    pub arena: PlanArena,
    pub vars: &'s mut SessionVars,
    pub expr_ctx: ExprContextRef,
    /* &'s StmtCtx, InfoSchema … */
}
impl<'s> PlanCtx<'s> {
    pub fn eval_ctx(&self) -> EvalContextRef;
    pub fn alloc_plan_id(&self) -> i32;         // Go 的 PlanID.Add(1)，在 Go 分配的同一位置调用（N1 §3.6）
    // 备用写法（N1 §4）：只在需要跨函数边界一次传出多个借用时用；标准写法是摘出分派
    pub fn split(&mut self) -> (&mut PlanArena, &dyn ExprContext, &mut SessionVars);
}
// 表达式上下文（N4 已移植，crates/sessionctx exprctx）：共享引用，warning 和列 ID 分配器内部可变，不需要 &mut
pub type ExprContextRef = Arc<dyn ExprContext>;  // 对应 Go exprctx.ExprContext（含 BuildContext：列 ID 分配器、plan cache 标志等）
pub type EvalContextRef = Arc<dyn EvalContext>;  // 对应 Go exprctx.EvalContext：冻结快照的视图 + warning

// ---- N3 并发与取消 ----
#[derive(Clone)]
pub struct ExecCtx {                            // Send + Sync，廉价 clone；worker 唯一能拿到的上下文
    pub stmt: Arc<StmtCtx>,
    pub killer: SqlKiller,                      // 实为 Arc 句柄
    pub cancel: CancellationToken,
    pub mem: Arc<MemTracker>,
    pub pool: ExecPoolHandle,
    pub resource_group: Arc<str>,               // 2026-11-06：cop 请求元数据、CPU 记账、Q1/Q2 份额都用它
    pub cpu: CpuSliceRecorder,                  // checkpoint 记录本片 CPU 时间，归到语句与 resource group
    pub store_windows: Arc<StoreWindows>,       // Q2：服务器级、每 store 一个在途窗口（默认无限 = Go）
}
pub struct StoreWindows { /* store_id → 窗口；内含 Go tidb_store_limit 的 L1 语义；AIMD / 按组份额默认关闭 */ }
impl StoreWindows {
    pub async fn acquire(&self, ctx: &ExecCtx, store: u64) -> Result<InflightPermit>; // 响应到达即 drop permit
}
pub struct CopRequestMeta {                     // 由 StmtCtx 持有，每个 cop 请求都带上
    pub priority: Priority, pub resource_group: Arc<str>, pub override_priority: u64,
    pub task_id: u64, pub busy_threshold_ms: u32, pub deadline: Option<Instant>,
}
impl ExecCtx {
    pub async fn recv<T>(&self, rx: &mut Receiver<T>) -> Result<Option<T>>;   // 同时监听取消（M-CH）
    pub async fn send<T>(&self, tx: &Sender<T>, v: T) -> Result<()>;
    pub async fn checkpoint(&self) -> Result<()>;                              // 每次：kill + 内存检查；超过时间片（默认 1ms）才让出（M-KILL）
}
#[async_trait]
pub trait Executor: Send {
    async fn open(&mut self, ctx: &ExecCtx) -> Result<()>;
    async fn next(&mut self, ctx: &ExecCtx, chk: &mut Chunk) -> Result<()>;
    async fn close(&mut self) -> Result<()>;
}
pub enum Topology { Steal, NoSteal }           // N3 3.1 第 2 条：两种拓扑过同一套测试
pub trait ExecPool: Send + Sync {               // 第一步：tokio；以后可换 yatp
    fn spawn_with_recover<F, R>(&self, name: &'static str, fut: F,
        on_exit: impl FnOnce(Option<PanicError>) + Send + 'static) -> JoinHandle<R>
    where F: Future<Output = R> + Send + 'static, R: Send + 'static;
}

// ---- A1 内存 ----
pub struct MemTracker { /* Go Tracker 树：label、硬/软限额、MaxConsumed、动作链 */ }
#[derive(Clone)]
pub struct TrackedAlloc { /* 叶子 tracker 句柄；实现 allocator_api2::Allocator，转发给 jemalloc */ }

// ---- A4 JSON ----
pub struct BinaryJson { /* 拥有型，字节与 Go 逐字节一致 */ }
pub struct JsonRef<'a> { /* 借用视图，读取层复用 TiKV 布局代码（ADR 0007） */ }
```

## 3. N4、N5a、N8 的类型

```rust
// ---- N4 表达式 ----
pub enum Expression { Column(Column), CorrelatedColumn(CorrelatedColumn), Constant(Constant), ScalarFunction(ScalarFunction) }
pub struct BuiltinFunc { pub sig: &'static SigDesc, pub base: BaseBuiltinFunc, pub meta: SigMeta }
pub struct RpnProgram { /* 后序节点：ColumnRef / Constant / Call；Send + Sync，worker 间共享 */ }
pub struct RpnState   { /* 每个 worker 一份：中间列、签名缓存、关联列槽；不是 Sync */ }
pub struct EvalCtx<'a> { /* &'a 冻结快照 + &'a mut WarnBuf */ }
// 函数体：#[eval_fn(sig = DivideReal)] fn divide_real(ctx: &mut EvalCtx, sig: &BuiltinFunc, a: f64, b: f64) -> Result<Option<f64>>

// ---- N5a 列格式 ----
pub struct Chunk { /* Vec<Arc<Column>>, sel, 记账账户 */ }        // Send
pub struct Column { /* length, null_bitmap/offsets/data: ColBuf */ }
pub enum ColBuf { Owned(Vec<u8, TrackedAlloc>), Shared(RespBytes) } // 写时复制
pub struct Row<'a> { chk: &'a Chunk, idx: usize }                  // 调用内借用
pub struct RowHandle { chk: Arc<Chunk>, idx: u32 }                 // 跨 Next 持有（M-ROW）

// ---- N8 系统变量 ----
pub struct SysVar { /* 静态字段 + 钩子 fn 指针 + runtime_only */ }
pub static REGISTRY: LazyLock<Registry>;                           // 不可变；实例覆盖层用 ArcSwap
pub struct SessionVars { /* Go 的类型化字段 + globals: Arc<GlobalVarsSnapshot> + delta */ } // Send，不是 Sync
```

- `Chunk` 的输出分区数固定为 1（N3 2026-11-06）。以后加分区并行，是新增一类交换算子，不改 `Executor` 接口。
