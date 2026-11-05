# N8：系统变量静态注册表（基础设计 spec，草案）

状态：草案。类型：实现例外（基础设计）。计划定稿：P1 之前（README 索引 2026-11-04 已改为"P1 之前"）。相关：N2（会话与语句上下文）、N3+A1（并发、内存）、B1/B5/D1（不支持的取值）。

所属块：D05.1 系统变量定义与校验（P1 L4，1,027 行，8 个文件）；同时约束 D05.2 SessionVars（P1 L5）、D06.5 domain（P2）、D12.12 SET/SHOW（P5）、D09.x 表达式改写中的 `@@` 求值。

所有 Go 位置均在 `~/src/pingcap/tidb-all-source-code/tidb-v858/pkg/` 下，tag v8.5.8。计数方法见文末脚注。

## 1. 要解决的问题

Go 在 `init()` 里把约 950 个 `*SysVar` 放进一个加锁的全局 map，运行中还会改它；每个会话在第一条语句时把几百个变量的字符串值拷进自己的 `systems` map，并对每个变量调用一次 `SetSession` 钩子来设置强类型字段。Rust 中要回答四件事：注册表用什么形态、会话里存什么、全局值怎么在混合集群里同步、tidb-rs 做不到的变量怎么处理。这些决定 D05.1/D05.2 的代码形态，所以要在 P1 之前定。

### 1.1 v8.5.8 中的事实

| 事实 | 位置 | 对 Rust 的影响 |
|---|---|---|
| 注册表 = `defaultSysVars`（499 项）+ `noopSysVars`（445 项，`init` 时统一置 `IsNoop=true`），共 944 项 | `sessionctx/variable/sysvar.go:74`、`noop.go:28`、`variable.go:656-669` | 规模固定，可静态化 |
| 作用域（全部 / 非 noop）：GLOBAL\|SESSION 358 / 259；GLOBAL 351 / 149；NONE 151 / 17；SESSION 57 / 47；INSTANCE 27 / 27 | 同上，脚注 [1] | INSTANCE 不进 `mysql.global_variables`（`session/session.go:1347`） |
| 类型：Str 443（405 未写 Type）、Bool 241、Int 94、Unsigned 79、Float 42、Enum 34、Time 6、Duration 5 | `variable.go:46-61`，脚注 [1] | `ValidateFromType` 按类型分派（`variable.go:330-355`），L1 照搬 |
| 钩子（全部为非 noop 项）：`SetSession` 273、`SetGlobal` 155、`GetGlobal` 134、`Validation` 93（非 noop 85）、`GetSession` 27、`GetStateValue` 6、`Aliases` 6；`skipInit` 18、`ReadOnly` 23、`Depended` 4、`GlobalConfigName` 2；`RequireDynamicPrivileges` 0 | `variable.go:132-196`，脚注 [1] | 钩子都是包级字面量里的闭包，不捕获局部变量，可直接写成 `fn` 指针 |
| 注册表运行时可变：`RegisterSysVar`/`UnregisterSysVar`（extension、示例插件）；`SetSysVar` 复制一份改 `Value` 再注册，用于启动时的 version、port、socket、hostname、ssl_*、system_time_zone，以及 SEM 开关 | `variable.go:592-627`；`extension/manifest.go:197-199`；`cmd/tidb-server/main.go:659 setGlobalVars`（L756-793）；`server/server.go:410-418`、`:1143`；`util/sem/sem.go:75-87` | 静态表之外需要一个小的"实例覆盖层" |
| `GetSysVars()` 每次深拷贝整张表；SHOW VARIABLES 每次都调它，输出顺序由 planner 加的 `LogicalSort` 决定，与注册顺序无关 | `variable.go:629-639`；`executor/show.go:968-1036`；`planner/core/planbuilder.go:3836-3843` | Rust 不需要拷贝；不必保持 map 顺序 |
| 会话的变量存储：`systems map[string]string` + 约 300 个强类型字段（N2 写约 296）；`warning_count`/`error_count` 已单独抽成字段 | `sessionctx/variable/session.go:775-1789`（`systems` 在 L786，L787-790） | 强类型字段由 `SetSession` 写，`systems` 只是字符串镜像 |
| `SetSessionFromHook`：先调 `SetSession`，再写 `systems[name]`，再对 `Aliases` 逐个调 `SetSession` 并写 `systems` | `variable.go:242-267` | 字符串值和强类型字段两份，必须同步更新 |
| 会话初始化：`NewSessionVars` 用 `Def*` 常量填强类型字段；第一次 `Parse`/`ExecuteStmt`/`PrepareStmt` 时调 `loadCommonGlobalVariablesIfNeeded`，从 domain 取会话缓存的**深拷贝**，对每个名字调 `SetSystemVarWithRelaxedValidation`（即 relaxed 校验 + `SetSession`），客户端是 interactive 时再用 `interactive_timeout` 覆盖 `wait_timeout` | `session.go:2204-2410`；`session/session.go:4144-4186`，调用点 L1578、L2026、L2493；`domain/sysvar_cache.go:60-69` | 每会话 288 次校验 + 255 次钩子 + 一次 map 拷贝；这里就是 N8 要省的成本 |
| 会话缓存的内容：所有 `!SkipInit()` 的变量（= 有 SESSION 作用域、非 noop、非 skipInit），值取 `mysql.global_variables`，没有就取默认值 | `domain/sysvar_cache.go:104-150`；`variable.go:565-570` | 会话起点是"全局当前值"，**不是**编译期默认值 |
| 懒加载兜底：`GetSessionOrGlobalSystemVar` 发现 `systems` 里没有某个会话变量时，有 GLOBAL 作用域就取当时的全局值，否则取默认值，并写回 `systems`；有 `GetSession` 钩子的直接走钩子 | `session.go:2836-2886` | 27 个变量的会话值是从强类型字段现算的（如 `last_insert_id`、`timestamp`、`warning_count`） |
| 全局缓存：domain 的 `sysVarCache{global, session}`，30 秒 ticker 或 etcd key `/tidb/sysvars` 变化时整表重读 `SELECT variable_name, variable_value FROM mysql.global_variables`；重建时对每个有 `SetGlobal` 的 GLOBAL 变量调一次 `SetGlobal`（GC 相关 6 个除外），让所有节点的进程级状态一致 | `domain/sysvar_cache.go:85-150`；`domain/domain.go:2034-2091`、`:3009`；`variable.go:579-588` | tidb-rs 要实现同样的监听和重建，否则混合集群中 SET GLOBAL 不生效 |
| SET GLOBAL：`Validate` → `SetGlobalFromHook` → 非 INSTANCE 时 `REPLACE INTO mysql.global_variables` 并 `NotifyUpdateSysVarCache`（写 etcd + 本地重建）；有 `GlobalConfigName` 的还写 PD | `session/session.go:1285-1378`；`domain/domain.go:3065-3080` | SET GLOBAL 需要写系统表，这是写路径 |
| 进程级状态：`tidb_vars.go` 中 88 个 `atomic.New*` 包级变量，被 `SetGlobal`/`GetGlobal` 读写，其他包直接读 | `tidb_vars.go:1704-1837`（脚注 [2]） | 对应 Rust 的 `static` 原子量，L1 |
| SET 语句：未知变量但属于已删除变量时**静默成功**；noop 变量在 `tidb_enable_noop_variables=OFF` 时给 warning 8144；INSTANCE 变量在 legacy 模式下改成 GLOBAL 并给 warning 8142 | `executor/set.go:112-149`；`removed.go:17-30`；`errno/errcode.go:1057-1060`、`errname.go:1053-1054` | 8144 文案是 `setting %s has no effect in TiDB`；`tidb_enable_noop_variables` 默认 ON（`tidb_vars.go:1467`），即默认**不**给这条 warning |
| `@@` 求值：在表达式改写阶段一次性取值并折成常量；`GetNativeValType` 决定类型（Unsigned/Bool → LONGLONG，其余 VARSTRING 用连接字符集） | `planner/core/expression_rewriter.go:1797-1856`；`variable.go:545-563` | 取值在会话线程上，结果是常量，不进执行器 |
| `SET_VAR` hint：在 `planner.Optimize` 中通过 `SetSystemVarWithOldValAsRet` 改 SessionVars，旧值记入 `StmtCtx.SetVarHintRestore`，**下一条语句**的 `ResetContextOfStmt` 开头才恢复 | `planner/optimize.go:194-227`；`executor/select.go:933-940` | 见 §8 对 N2 的修正 |
| `sysvar.go` 直接依赖 client-go（`tikvcfg`、`tikvstore`、`tikvcliutil`）；6 个变量的钩子写 client-go 的全局量（如 `tidb_store_limit`、`tidb_committer_concurrency`） | `sysvar.go:15-66`、`:433`、`:887`、`:893`、`:1339`、`:2317`、`:2321` | 这些钩子的"落点"换成 client-rust（L3），与 Go-runtime 变量是另一类 |

PoC 现状：`crates/session/src/lib.rs:322-335` 有一个 `IGNORED_SYSVARS` 列表（接受并忽略 10 个变量），其余 `SET` 报"不支持"，`SET GLOBAL` 一律报错（L646-648）；`crates/session/src/constant.rs:42-85` 硬编码约 30 个 `@@` 的值。没有注册表，也不读 `mysql.global_variables`。这些都属于 `glue: poc`，将被 D05.1/D05.2 替换。

## 2. 方案比较

### 2.1 注册表形态

| 方案 | 做法 | 评价 |
|---|---|---|
| a. 照搬 Go | `RwLock<HashMap<String, Arc<SysVar>>>`，启动时注册 | 与 Go 一样能动态增删；每次查找要加读锁、SHOW 要深拷贝。动态注册只服务于 extension/插件，不在路线图内。**不推荐** |
| b. 全部代码生成 | 脚本从 `sysvar.go` 生成 Rust 表 | 静态字段（名字、作用域、类型、默认值、范围、枚举值）能生成；但 400 多个闭包钩子是任意 Go 代码，无法机械翻译。**只适用于静态字段** |
| c. 手工逐项移植的静态表 + 机械校验 | `static DEFAULT_SYS_VARS: LazyLock<Vec<SysVar>>`，顺序、分段与 `sysvar.go` 完全一致，每项标注 Go 行号；钩子是 `fn` 指针；tracker 脚本解析 Go 和 Rust 两边的表做逐项比对 | 一项对一项，上游同步时按行号定位；静态字段由脚本保证不漂移；钩子按普通函数做 L1 评审。**推荐** |

### 2.2 会话中的变量存储

| 方案 | 做法 | 评价 |
|---|---|---|
| a. 照搬 Go | 每会话一份完整 `HashMap<String,String>` + 强类型字段，首条语句时跑全部钩子 | 语义最直接；建连成本与 Go 相同，没有收益 |
| b. N2 原设想：只存改过的值，其余读静态注册表 | `delta` + 编译期默认值 | **语义错误**：会话起点是全局当前值而不是默认值；强类型字段仍需初始化；`GetSession` 变量不在任何 map 里（见 §3.2） |
| c. 共享全局快照 + 会话增量 + 强类型字段 | `Arc<GlobalVarsSnapshot>`（会话创建时取当时的快照，多会话共享）+ `delta: HashMap<&'static str, String>` + Go 的强类型字段；钩子照常执行 | 语义与 Go 相同；省掉 map 深拷贝和大部分字符串分配。**推荐**；"模板克隆"作为后续优化（§3.2 第 4 点） |

## 3. 设计

### 3.1 注册表（D05.1）

```rust
// go: pkg/sessionctx/variable/variable.go SysVar L2 (closures -> fn pointers)
pub struct SysVar {
    pub scope: ScopeFlag, pub name: &'static str, pub value: Cow<'static, str>,
    pub typ: TypeFlag, pub min_value: i64, pub max_value: u64,
    pub possible_values: &'static [&'static str], /* ReadOnly、AllowEmpty… 同 Go 逐个字段 */
    pub validation: Option<fn(&mut SessionVars, &str, &str, ScopeFlag) -> Result<String>>,
    pub set_session: Option<fn(&mut SessionVars, &str) -> Result<()>>,
    pub set_global:  Option<fn(&GlobalCtx, &mut SessionVars, &str) -> Result<()>>,
    pub get_session: Option<fn(&SessionVars) -> Result<String>>,
    pub get_global:  Option<fn(&GlobalCtx, &SessionVars) -> Result<String>>,
    pub get_state_value: Option<fn(&SessionVars) -> Result<(String, bool)>>,
    pub aliases: &'static [&'static str], pub is_noop: bool, pub runtime_only: bool, /* §3.4 */
}
```

1. **表**：`default_sys_vars()` 返回 `Vec<SysVar>`，顺序与 `sysvar.go:74-3732` 一致，保留五个分段注释；`noop_sys_vars()` 对应 `noop.go`。`LazyLock<Registry>` 持有两张表和一个 `name → index` 的哈希表（用 `phf` 或启动时建的 `HashMap` 都可以，P1 实测决定）。默认值里有计算项（`strconv.Itoa(Def...)`、`mysql.ServerVersion`、`runtime.GOOS`），所以用 `LazyLock` 而不是 `const`。
2. **来源标注**：每项上方一行 `// go: sysvar.go:L<行号> <常量名>`，钩子函数按普通函数标注（`// go: sysvar.go:L933 TiDBEnableGOGCTuner.SetGlobal L1`）。钩子写成具名函数还是内联闭包（不捕获的闭包可以转成 `fn` 指针）由 plan 模板统一，建议简单钩子内联、超过 10 行的提成具名函数。
3. **机械校验**（tracker）：脚本解析 Go 的两张表（脚注 [1] 的解析器即可）和 Rust 的表，逐项比对名字、作用域、类型、默认值表达式、范围、枚举值、各标志位、"有无某钩子"。上游同步时差异直接列出。
4. **实例覆盖层**：`SetSysVar` 的语义是"改注册表里的默认值"。Rust 用 `ArcSwap<HashMap<&'static str, String>>` 保存覆盖值，`Registry::value(name)` 先查覆盖层。写入点与 Go 相同（启动、TLS 重载、SEM 开关、system_time_zone）。L2。
5. **不支持动态注册**：`RegisterSysVar` 只服务于 extension 和插件，二者不在路线图内，不移植（边界桩）。`UnregisterSysVar` 只用于测试。
6. `GetSysVars()` 的深拷贝不需要（表不可变），改为返回迭代器（L2）。`OrderByDependency`、`setHintUpdatable` L1。

### 3.2 会话存储（D05.2）

1. **强类型字段**全部按 Go 保留（L1），由同样的 `SetSession` 钩子写入。热路径读字段，不查 map。这部分与 N2 一致。
2. **字符串值**：`systems` 换成两层（L2）：
   - `globals: Arc<GlobalVarsSnapshot>`：domain 每次重建缓存时生成的不可变快照（内容 = Go 的 `sysVarCache.session`，按注册表下标存成 `Box<[Option<Arc<str>>]>`）。会话在 Go 调 `loadCommonGlobalVariablesIfNeeded` 的同一时机取一次 `Arc`，之后不再变。这保留了 Go 的语义：之后的 SET GLOBAL 不影响已有会话。
   - `delta: HashMap<&'static str, String>`：本会话 SET、SET_VAR、`wait_timeout` 覆盖等写入的值。
   - `get_system_var(name)`：先 `delta`，再 `globals`；`warning_count`/`error_count` 照 Go 特判。
3. **钩子顺序的机械检查**（`M-MAPORDER`）：D05.1 的 plan 中做一项机械检查——钩子函数体中不读取 `SessionVars` 的其他字段；不满足的（含 `Depended` 的 4 个）逐个列入审计，说明为何与顺序无关或如何固定顺序。
4. **初始化仍然逐个执行 `SetSession`**（L1）：快照里有 288 个值，对其中 255 个有钩子的变量按快照值调钩子，设置强类型字段。顺序：Go 遍历 map，顺序随机；Rust 用注册表顺序（确定），把"Go 中依赖顺序的钩子"列为审计项（§7）。
4. **后续优化（不在本 spec 承诺）**：domain 在重建快照时顺便用一个模板 `SessionVars` 跑一遍钩子，新会话直接 `clone` 模板的强类型部分。前提是审计 255 个钩子都是"只写字段、不依赖连接信息、无副作用"。不满足的单独跑。做不做由 §5 的建连指标决定。
5. **这样做破坏/不破坏什么**：

| 场景 | Go 行为 | 方案 c 下 |
|---|---|---|
| 会话起点取全局值，后续 SET GLOBAL 不影响旧会话 | 首条语句时拷贝 | 取快照 `Arc`，一致 |
| 首条语句前就读 `@@var`（如握手后直接 `SELECT @@x`） | 同样会先触发加载（`Parse` 中调用） | 同一时机取快照，一致 |
| `GetSession` 钩子（27 个，如 `timestamp`、`last_insert_id`、`tidb_last_query_info`） | 每次现算 | 照常走钩子，不读 `delta`/快照 |
| 懒加载兜底（`systems` 中缺失时取**当时**的全局值） | `session.go:2851-2857` | 快照缺项时同样取 domain 当前值并写入 `delta`，L1 |
| SHOW VARIABLES / `information_schema.session_variables` | 每个变量走 `GetSessionOrGlobalSystemVar` | 相同；排序由 planner 完成 |
| session states 迁移（`GetSessionStatesSystemVar` 只取"已缓存的值"） | `session.go:2868-2886` | 语义变化：Go 返回 `systems` 中所有已加载值，Rust 若只返回 `delta` 会少。**按 Go 返回快照 + delta**，L1 |

### 3.3 全局变量与混合集群（D06.5，P2）

1. tidb-rs 节点按 Go 的方式维护 `GlobalVarsCache`：启动时整表读取 `mysql.global_variables`，之后监听 etcd `/tidb/sysvars`，并保留 30 秒兜底重读。PoC 已有 `etcd-client`（`crates/domain/Cargo.toml:13`）。重建时对每个有 `SetGlobal` 的 GLOBAL 变量调钩子（GC 的 6 个除外），与 `rebuildSysVarCache` 相同（L1）。新快照用 `ArcSwap` 发布（L2，代替 `RWMutex`）。
2. 读：`GetGlobalSysVar` 的"缓存缺项 → 直接读表 → 失败回默认值 → `ValidateFromType` 失败回默认值"链条 L1（`session/session.go:1296-1332`）。
3. 写（SET GLOBAL）：需要一个写事务（`REPLACE INTO mysql.global_variables`）和一次 etcd put。只读阶段写路径是 X。两个选择见待定 1。
4. **tidb-rs 不能履行的全局值**：Go 节点可能把全局值设成 tidb-rs 不支持的取值（如 B1 的静态分区裁剪、B5 的代价模型 v1、D1 的 MPP 开关、§3.4 的 Go-runtime 变量）。加载时没有会话可以报 warning，所以分三类：
   - Go-runtime 变量：加载即接受，钩子落点是空操作，进程启动时日志记一次。
   - B1/B5 等"设置时报错"的取值：快照照常保存（否则 SHOW 显示与集群不一致）；会话初始化调 `SetSession` 时不报错，到**使用该取值的语句**再报不支持错误（由 B1/B5 各自 spec 定错误码）。
   - 其余未知或校验失败的值：按 Go 的 relaxed 校验回落默认值（`variable.go:374-396`）。

### 3.4 Go-runtime 变量（项目负责人已决定）

规则：**这些变量接受 SET（SESSION/GLOBAL 均可），值照常校验、照常存储和显示（包括写入 `mysql.global_variables`，以便 Go 节点生效）；取值与默认值不同时追加一条 warning**。读取 `@@` 不报 warning。

- warning：Level `Warning`，code **8144**（沿用 `ErrSettingNoopVariable` 的错误码），文案 `setting %s has no effect in tidb-rs`。不受 `tidb_enable_noop_variables` 控制（Go 中该开关默认 ON 时不报 noop warning，这里是另一种原因，始终报）。
- 实现：钩子按 L1 移植，Go 中维护的原子量（如 `GOGCTunerThreshold`）照样保留，所以 `GetGlobal` 读回的是用户设置的值，互相约束的校验（`max_value > min_value` 等）照常工作；只把最后调用 Go runtime 的一步（`gctuner.Tuning`、`SetMaxGCPercent`、`UpdateMemoryLimit` 等）换成空操作，标注 `L2 runtime-only`。
- 注册表字段 `runtime_only: bool` 是唯一的清单来源；下表由它生成，SET 执行器读它决定是否加 warning。

| 变量 | 作用域 | Go 默认 | 位置 | 理由 |
|---|---|---|---|---|
| `tidb_enable_gogc_tuner` | GLOBAL | ON | `sysvar.go:933` | 调 Go GC 百分比 |
| `tidb_gogc_tuner_max_value` | GLOBAL | 500 | `sysvar.go:942` | 同上 |
| `tidb_gogc_tuner_min_value` | GLOBAL | 100 | `sysvar.go:959` | 同上 |
| `tidb_gogc_tuner_threshold` | GLOBAL | 0.6 | `sysvar.go:1143` | 同上 |
| `tidb_server_memory_limit_gc_trigger` | GLOBAL | 0.7 | `sysvar.go:1228` | Go 软内存上限触发 GC |
| `tidb_enable_gc_aware_memory_track` | GLOBAL | OFF | `sysvar.go:1545` | 按 Go GC 统计修正内存 |
| `tidb_pprof_sql_cpu` | INSTANCE | 0 | `sysvar.go:497` | Go pprof 标签 |
| `tidb_enable_reuse_chunk` | GLOBAL\|SESSION | ON | `sysvar.go:2957` | Go 绕开 GC 的 chunk 复用；A1 删除复用池（`M-POOL`） |

待定（不在表内，见待定 3）：`tidb_memory_usage_alarm_ratio`/`_keep_record_num`（`sysvar.go:1559`、`:1565`，告警会落 Go heap profile）。`tidb_server_memory_limit` 不在表内：它同时驱动 A1 的实例级内存控制。`tidb_gc_*` 是 TiKV 的 MVCC GC，不是 Go GC。

### 3.5 线程安全（与 N2、N3 一致）

| 对象 | 归属 | 约束 |
|---|---|---|
| `Registry`（静态表 + 名字索引） | 进程 `static` | 不可变，`Sync`；钩子是 `fn` 指针，天然 `Send + Sync` |
| 实例覆盖层、`GlobalVarsCache` 当前快照 | domain，`ArcSwap` | 读无锁；写只在启动、重建时 |
| 88 个进程级原子量 | `static Atomic*` | L1 |
| `SessionVars`（强类型字段 + `globals` + `delta`） | 会话独占，`&mut` | 必须 `Send`（N3 §3.1 第 9 条：语句在 ExecPool 上执行，会话整体 move 进语句 future），不要求 `Sync`；不得含 `Rc`/`RefCell` |
| 执行器 worker | 只读 N2 的冻结快照（`ExecCtx`） | 不访问注册表以外的任何变量存储 |

钩子的签名带 `&mut SessionVars`，只能在会话线程上调用；`SetGlobal` 在缓存重建时由 domain 的内部会话调用（Go 同样用 `sysSessionPool` 中的会话，`domain/sysvar_cache.go:108-114`）。

## 4. 对 L1 移植的影响（示例）

Go（`sysvar.go:2957`）：

```go
{Scope: ScopeGlobal | ScopeSession, Name: TiDBEnableReusechunk, Value: BoolToOnOff(DefTiDBEnableReusechunk), Type: TypeBool,
    SetSession: func(s *SessionVars, val string) error {
        s.EnableReuseChunk = TiDBOptOn(val)
        return nil
    }},
```

Rust：

```rust
// go: sysvar.go:L2957 TiDBEnableReusechunk
SysVar { scope: GLOBAL | SESSION, name: TIDB_ENABLE_REUSECHUNK,
    value: bool_to_on_off(DEF_TIDB_ENABLE_REUSECHUNK).into(), typ: TypeFlag::Bool,
    set_session: Some(|s, val| { s.enable_reuse_chunk = tidb_opt_on(val); Ok(()) }),
    ..SysVar::DEFAULT },
```

Go-runtime 变量（`sysvar.go:942`）只换最后一步：

```rust
// go: sysvar.go:L942 TiDBGOGCTunerMaxValue.SetGlobal L2 runtime-only (gctuner calls are no-ops)
set_global: Some(|_, _s, val| {
    let max_value = tidb_opt_int64(val, DEF_TIDB_GOGC_MAX_VALUE);
    gctuner::set_max_gc_percent(max_value as u32); // 只存原子量，不调 runtime
    Ok(())
}),
```

## 5. 验收（初拟）

1. **表一致**：tracker 比对脚本对 v8.5.8 报告 0 差异（944 项；静态字段逐项相等，钩子"有/无"一致）。
2. **行为**：`sessionctx/variable` 包内的 `sysvar_test.go`、`variable_test.go`、`varsutil_test.go`、`session_test.go` 中不依赖存储的测试随代码移植并通过；`executor` 的 SET/SHOW VARIABLES 测试、`session/test/variable` 按 SQL 级对照测试通过。
3. **混合集群**：Go 节点 `SET GLOBAL x=v` 后，tidb-rs 节点新会话的 `@@x` 在 etcd 通知后 1 秒内、无 etcd 时 35 秒内变为 `v`；已有会话不变（与 Go 节点并排对照）。
4. **runtime-only**：表中 8 个变量 SET 非默认值时 `SHOW WARNINGS` 恰好一条 8144；SET 默认值时无 warning；`@@global` 读回设置值。
5. **建连成本**（与 N2 一起测）：新会话首条语句前的变量初始化耗时和每会话变量存储内存不高于 Go 实测值；目标是内存下降（不再每会话一份 288 项字符串 map）。Go 基线在定稿前实测回填。
6. **线程**：`SessionVars: Send` 由一个编译期断言固定；`Registry: Sync` 同上。

## 6. 待定项（需要项目负责人决定）

1. **SET GLOBAL 在只读阶段怎么办**：(a) P1–P4 一律报"不支持"（第五条），P5 随 D12.12 实现窄写路径；(b) 尽早实现只写 `mysql.global_variables` 的内部写事务。建议 (a)，并在混合集群文档中说明"请在 Go 节点上执行 SET GLOBAL"。
2. **warning 用 8144 还是新码**：建议沿用 8144（客户端已把它当"设置无效"处理），文案改为 `tidb-rs`。如果要求文案与 Go 逐字一致，则改用新错误码。
3. **runtime-only 清单的边界**：`tidb_enable_reuse_chunk` 已按 A1 修订（2026-11-04，删除复用池）入表；`tidb_memory_usage_alarm_*` 是否入表，与 A1 一起定。
4. **INSTANCE 作用域**：Go 不写表，只改本节点。tidb-rs 对 27 个 INSTANCE 变量中涉及 Go 专有组件的（插件、`tidb_general_log` 等）是否逐个归入 runtime-only 或"不支持"，建议 D05.1 开工时逐项列出。
5. **模板克隆优化**（§3.2 第 4 点）是否作为 N8 的承诺指标，还是只作为后续优化。

## 7. 我不确定的地方

1. 255 个 `SetSession` 钩子中，有多少依赖其他变量或连接信息（是否有顺序依赖），我没有逐个读。Go 按 map 随机顺序调用，理论上不应有顺序依赖，但 `Depended`（4 个）的存在说明会话状态回放时有依赖。需要在 D05.1 开工时审计。
2. 计数是用一个简易 Go 字面量解析器得到的（脚注 [1]）；与 `grep -c "Scope:"` 对得上（501 − 2 个 `case ...Scope:` = 499；noop 445），但"有某钩子"的计数依赖字段名匹配，个别跨行写法可能漏计 1–2 个。
3. 30 秒兜底和 etcd 通知是否足以满足"混合集群一致"，取决于 Go 节点的 `SetGlobal` 副作用是否也要求 tidb-rs 执行（例如影响 TiKV 的 `tidb_gc_*` 由 Go 节点负责）。我认为 tidb-rs 不需要执行写 `mysql.tidb` 的那 6 个，但没有核实 GC leader 的行为。
4. B1/B5 的取值"使用时报错"是否与它们各自的 spec 一致，要等那两个 spec。
5. 6 个写 client-go 全局量的变量映射到 client-rust 后是否都有对应配置项，没有核实；没有的应按 §3.4 同样处理或报不支持。

## 8. 与其他 spec 的关系

- **N2**（2026-11-04 已按下述两点修正）：§3.1 的"系统变量：只存改过的值，其余查 N8 的静态注册表"需要改为"只存改过的值，其余查**会话创建时取得的全局值快照**（N8 §3.2）；`GetSession` 变量由钩子现算"。N2 §3.2 表中 `SET_VAR` 一行"在冻结前应用，结束后无需恢复"与 Go 不符：Go 通过 `SetSystemVarWithOldValAsRet` 改 SessionVars（含强类型字段），planner 在会话线程上直接读这些字段，旧值在下一条语句开始时恢复（`planner/optimize.go:194-227`、`executor/select.go:933-940`）。Rust 仍需要恢复，写进 `delta` 后按 Go 的时机恢复即可（L1）。N2 §5 第 4 条"建连成本"由本 spec §5 第 5 条细化。
- **N3+A1**：会话 `Send` 的要求见 §3.5；`tidb_enable_reuse_chunk` 按 runtime-only 处理（§3.4）；`tidb_server_memory_limit` 由 A1 履行。
- **ROADMAP**：D05.1（P1 L4）直接实现本 spec 的 §3.1；D05.2（P1 L5）实现 §3.2；D06.5（P2）实现 §3.3；D09 的 `@@` 改写、D12.12（P5）的 SET/SHOW 使用 §3.4 的规则。因为 D05.1/D05.2 在 P1，**§3.1、§3.2、§3.4 必须在 P1 前定稿**；§3.3 可以在 P2 开工前补充，P1–P2 之间用"快照 = 默认值 + 实例覆盖"的桩，并在 tracker 中登记。
- **B1、B5、D1**：它们定义的"不支持的取值"在本 spec 中统一走 §3.3 第 4 点。
- **PoC**：`IGNORED_SYSVARS` 与 `ConstEnv::sysvar` 在 D05.1 合并后删除。注意 PoC 当前接受并忽略 `time_zone`、`max_execution_time` 等，这在 Go 中有实际效果，属于第五条所说的"静默忽略"，应在 D05.1 前作为已知偏差登记。

---

脚注

[1] 计数：用一次性脚本（将作为 §3.1 第 3 条的比对脚本放入 tracker；逐字符跟踪花括号、跳过字符串和注释，取 `defaultSysVars`/`noopSysVars` 两个切片字面量的一级元素，再取每个元素一级的 `字段名:`）解析 `sysvar.go`、`noop.go`。核对：`grep -c "Scope:" sysvar.go` = 501，其中 2 处是 L132/L134 的 `case kv.GlobalTxnScope:`/`case kv.LocalTxnScope:`，余 499；`grep -c "Scope:" noop.go` = 445。`skipInit` 用 `grep -c "skipInit: true" sysvar.go` 核对为 18。"首条语句时加载" = 有 SESSION 作用域 ∧ 非 noop ∧ 非 skipInit = 288，其中有 `SetSession` 的 255。

[2] `grep -c -E "^\s+[A-Z][A-Za-z0-9]*\s+=\s+atomic\.New" sessionctx/variable/tidb_vars.go` = 88。强类型字段数：`awk 'NR>775 && NR<1789' session.go | grep -c -E "^\t[A-Z][A-Za-z0-9]*( |$)"` = 301（含嵌入类型行，N2 的约 296 口径不同）。Go-runtime 变量候选：在解析结果中按 `gctuner|debug\.Set|runtime\.|MemoryLimit|ReuseChunk|MemoryUsageAlarm|pprof` 匹配钩子正文后人工筛选。
