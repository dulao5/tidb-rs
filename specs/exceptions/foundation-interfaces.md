# 基础设计 spec 之间的接口汇总

状态：草案，随 N1、N2、N3+A1、A4（以及之后的 N4、N5a、N8）一起定稿。各块 plan 模板中的函数签名以本页为准。这里只列类型签名和"谁拥有谁"。理由和细节看各 spec，映射规则看 [mappings.md](../../docs/porting/mappings.md)。

## 1. 所有权关系

```
连接任务（唯一的 tokio 运行时；Steal 或 NoSteal 拓扑，N3 3.1）
 └─ Session: Send                                   N2   由连接任务持有；语句在同一个 task 里执行，不交接
     ├─ vars: SessionVars            (!Sync)        N2   只有持有会话的任务可写；系统变量存储见 N8
     ├─ shared: Arc<SessionShared>   (Send+Sync)    N2   其他会话和后台可见：SqlKiller、会话根 MemTracker、ArcSwap<ProcessInfo>
     └─ 语句（在连接任务内；先过 Q1 准入，N3 3.6）
         ├─ PlanCtx<'s>                              N1/N2 规划期：&mut SessionVars、&StmtCtx、InfoSchema、PlanArena
         │    └─ PlanArena                            N1   逻辑计划节点（Box<dyn LogicalPlan>），PlanId 引用；随规划结束释放
         ├─ Arc<StmtCtx>             (Send+Sync)     N2   冻结的只读变量 + 执行期共享部分（warnings、RuntimeStatsColl、语句 tracker）
         ├─ Arc<dyn PhysicalPlan>    (Send+Sync)     N1   DoOptimize 末尾冻结；可进 plan cache
         └─ 执行器树 Box<dyn Executor> (Send)          N3   由物理计划构建；worker task 只拿 ExecCtx
              └─ worker task（pool.spawn，仅当计划带并行 worker） N3   捕获 ExecCtx + 自己的 Arc<…Shared>；子执行器可被 move 进 fetcher，Close 前交回

服务器级共享对象（Arc，进程内唯一）：Q1 Admission、Q2 StoreWindows（每 store 一个在途窗口）、Q3 阻塞池、ExecPool 句柄   N3 3.6
```

## 2. 类型签名（示意）

```rust
// ---- N1 计划表示 ----
pub struct PlanId(u32);                         // arena 下标，仅内部使用
pub struct PlanArena { /* Vec<Box<dyn LogicalPlan>>, 旁表 task_map 等 */ }
pub trait LogicalPlan: Any + Send {
    fn kind(&self) -> LogicalKind;              // M-IFACE：type switch → match
    fn id(&self) -> i32;                        // Go 的 plan id（PlanID.Add(1)），不同于 PlanId
    fn as_any(&self) -> &dyn Any;
    // …与 Go base.LogicalPlan 一一对应的方法，接收 (&mut PlanCtx, PlanId)
}
pub trait PhysicalPlan: Any + Send + Sync { fn kind(&self) -> PhysicalKind; fn id(&self) -> i32; /* … */ }
pub type Expr = Arc<Expression>;                // 不可变共享，写时复制（M-EXPR）；具体结构见 N4

// ---- N2 上下文 ----
pub struct Session { pub vars: SessionVars, pub shared: Arc<SessionShared>, /* … */ }
pub struct SessionShared { pub killer: SqlKiller, pub mem_root: Arc<MemTracker>, pub process: ArcSwap<ProcessInfo> }
pub struct StmtCtx { pub vars: FrozenVars, pub cop_meta: CopRequestMeta, /* warnings, rows, runtime stats, mem/disk tracker … */ }
pub struct PlanCtx<'s> { /* &'s mut SessionVars, &'s StmtCtx, InfoSchema, PlanArena */ }
impl<'s> PlanCtx<'s> {
    pub fn split(&mut self) -> (&mut PlanArena, &EvalCtx);   // 唯一允许的同时借用写法（N1 §4）
}
pub struct EvalCtx;   // 对应 Go exprstatic.EvalContext：冻结快照的视图
pub struct BuildCtx;  // 对应 Go exprctx.BuildContext：列 ID 分配器、plan cache 标志等

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

## 3. 待补

- **N4**：`Expression` 的结构（规划侧树）、执行侧编译后的形式、内置函数的签名。
- **N5a**：`Chunk` / `Column` 的类型（Go Chunk 移植或 TiKV 列类型），以及它与 `TrackedAlloc` 的关系。
- **N8**：`SessionVars` 中系统变量的存储方式、注册表的类型。
- **N5a**（补充）：`Chunk` 的输出分区数固定为 1。以后加分区并行，是新增一类交换算子，不改 `Executor` 接口。
