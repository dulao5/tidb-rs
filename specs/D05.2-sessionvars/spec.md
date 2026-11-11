# D05.2：SessionVars 与会话钩子（spec）

依据：基础设计 spec N2（`specs/exceptions/N2-session-context/spec.md`，§3.1–3.4）、N8（`specs/exceptions/N8-sysvar/spec.md`，§3.2、§3.4）。crate `tidb-sessionctx`（rank 7，和 D05.1 同一 crate），模块 `variable`。

所属：P1 L5。前置：D05.1（已合并，#43；评审后续 #55 / tracker #26）。

## 1. 范围

### 1.1 函数（tracker `data/func_blocks.tsv`，块 D05.2）

| 文件 | 函数 | 说明 |
|---|--:|---|
| `pkg/sessionctx/variable/session.go` | 184 | `SessionVars`、`TransactionContext`、`UserVars`、`Concurrency`、`TxnReadTS`、plan cache 参数、prepared 语句等 |
| `pkg/sessionctx/variable/slow_log.go` | 22 | 慢日志规则与格式（读路径同样写慢日志） |
| `pkg/sessionctx/sessionstates/session_token.go` | 13 | 会话迁移的 token（TiProxy） |
| `pkg/session/cursor/tracker.go` | 8 | 服务端游标（`COM_STMT_FETCH`） |
| `pkg/sessionctx/variable/{varsutil,variable,sysvar}.go` | 10 | 从 D05.1 移来：`*FromHook`、`setSnapshotTS`、`setTxnReadTS`、`setReadStaleness`、`parseTSFromNumberOrTime`、`getTiDBTableValue`、`setTiFlashComputeDispatchPolicy` |
| 合计 | **237** | 范围划分前 276 个，39 个归入 X（§1.3） |

`stmtctx.StatementContext` 属于 D00.1（`pkg/sessionctx/stmtctx/` 归 D00.1），不在本块。本块只用到它的接口：warning 追加、语句类型、时区等。D00.1 合并前，沿用 D05.1 的替身（`session.rs` 中的 `StatementContext` 部分，标为 stand-in: D00.1）。

### 1.2 非校验钩子（tracker `data/sysvar_hooks.tsv`）

444 个变量带 SetSession（273）/ SetGlobal（155）/ GetSession（27）/ GetGlobal（134）/ GetStateValue（6）钩子。它们是 `sysvar.go` / `noop.go` 表字面量中的闭包，不在 `func_blocks.tsv` 里，由这份清单归给 D05.2。**验收按清单逐行核对**：每一行在 Rust 中有对应的钩子（L1 / L2），或者在清单的处置列中标明 X / runtime-only / DEF 及原因（§3.4）。

### 1.3 范围划分（2026-11-08，tracker `rules.py`）

tidb-rs 是只读计算节点，以下部分只服务于 Go TiDB 执行的写，归 X（原因写在 `rules.py`）：

| 归 X | 原因 |
|---|---|
| `RetryInfo`、`retryInfoAutoIDs` | INSERT 重试时复用自增 / auto-random ID |
| `RowIDShardGenerator`、`NewRowIDShardGenerator`、`GetRowIDShardGenerator` | INSERT 的行 ID 分片 |
| `TableDelta` | DML 的统计增量 |
| `temporaryTableData`、`NewTemporaryTableData`、`GetTemporaryTable` | 本地临时表由创建它的会话用 DDL 建立；tidb-rs 不执行 DDL |
| `WriteStmtBufs`、`GetWriteStmtBufs` | INSERT / UPDATE / DELETE 的缓冲 |
| `{Set,Get,Clear}DiskFullOpt` | 磁盘满时的 DML 选项 |
| `setDDLReorgPriority` | DDL reorg 优先级 |
| `TransactionContext` 的悲观锁缓存、unchanged keys、增量（9 个）、`PessimisticLockEligible` | `SELECT … FOR UPDATE` 和 DML 加锁；tidb-rs 不加锁 |

**保留**（读路径可达）：savepoint（只读事务中 `SAVEPOINT` / `ROLLBACK TO` 仍是合法语句）、for-update TS、用户变量、prepared 语句、`TxnReadTS`、会话状态的编码（server 的 `EncodeSessionStates`）、游标。

注意：静态引用图看不到闭包里的调用（钩子调 `setSnapshotTS` 等），所以"没有调用方"不能作为归 X 的依据。上表每一项都是逐个核对调用方后决定的。

## 2. 设计

### 2.1 结构（N2 §3.1、N8 §3.2）

```text
Session（Send，同一时刻只有一个任务持有，&mut）
 ├── SessionVars                                    本块
 │     ├── globals: Arc<GlobalVarsSnapshot>         会话首次使用时取（N8 §3.2）
 │     ├── delta: HashMap<&'static str, String>      本会话 SET 过的值（Go 的 systems 中与快照不同的部分）
 │     ├── 强类型字段                                 按 Go 的字段名和含义，L1
 │     ├── user_vars: UserVars                        N2 §3.2：语句中有赋值时以共享句柄交给 StmtCtx
 │     ├── txn_ctx: TransactionContext                （去掉 §1.3 的写路径部分）
 │     └── prepared statements / plan cache 参数 / 游标
 ├── shared: Arc<SessionShared>                     killer、根内存 tracker、ProcessInfo（N2 §3.1；由 D00.1 / server 接线）
 └── 当前语句 Arc<StmtCtx>                           D00.1
```

- **PR A 的实现（L1）**：`systems` map 与 Go 相同（会话的值），全局值经 `GlobalVarAccessor` 读取，与 Go 的 `GlobalVarsAccessor` 相同；"delta + 共享快照"的形式随 T10（读集群的全局值）一起做，届时只换 accessor 的实现，`SessionVars` 的存取函数不变。
- **`GetSystemVar` / `GetSessionOrGlobalSystemVar`**：先查 `delta`，再查 `globals`；有 GetSession 钩子的 27 个变量由钩子从强类型字段现算（N8 §3.2）。快照缺项时取 domain 当前值并写入 `delta`，与 Go 的懒加载兜底相同（L1）。
- **`SetSystemVar*`**：校验（D05.1）→ 写 `delta` → 调 SetSession 钩子更新强类型字段，顺序与 Go 相同。`SetSystemVarWithOldValAsRet` 返回旧值，供 `SET_VAR` hint 在下一条语句开始时恢复（N2 §3.2，L1）。
- **会话初始化**：Go 在第一次 `Parse` / `ExecuteStmt` / `PrepareStmt` 时对每个 `!SkipInit()` 变量调用 SetSession。Go 按 map 的随机顺序调用；`Depended` 的 4 个变量要求在其依赖之后执行。Rust 按注册表顺序，并把 `Depended` 变量放到最后（D05.1 的 `OrderByDependency`，M-MAPORDER）。
- **`Cloned` 字段**：Go 的 `cachedStmtCtx[2]` 轮换复用、`StmtCtx.Reset` 不移植（N2 §3.4，L2）。

### 2.2 全局值的来源（P1–P2 桩）

N8 §8：§3.3（domain 的全局缓存、etcd 通知、30 秒兜底重读）属于 D06.5（P2）。P1–P2 之间，`GlobalVarsSnapshot` = **注册表默认值 + 实例覆盖（`SetSysVar`）**，作为桩登记在 tracker（`glue: stand-in: D06.5`）。接口按 D06.5 的需要定：domain 发布新快照（`ArcSwap`），会话取 `Arc`。

**SET GLOBAL**（owner issue #58；负责人已决定方案 (c)，设计见 §6。下面是 PR A 的过渡行为）：
- **INSTANCE 作用域的 27 个变量**：与 Go 完全相同。Go 的 `SET GLOBAL` 对它们只改本实例，在写表之前就返回（`session/session.go:1347`），从不持久化。tidb-rs 照此执行：校验 → SetGlobal 钩子 → 本实例生效（Lucky 的评审）。
- **持久化的全局变量**：校验照常执行（错误与 Go 相同），校验通过后返回"不支持"，不写任何地方。若负责人选择 (b)，只影响这一个入口。

**全局值的读取提前**（Lucky 的评审）：D06.5 的**读**的一半提前到 P1。启动时和每 30 秒用 tidb-kv / tablecodec / codec 读一次 `mysql.global_variables`（不接 etcd），生成新的 `GlobalVarsSnapshot`，按 N8 §3.3 第 1 点对有 SetGlobal 钩子的变量执行钩子（GC 的 6 个除外）。这样新会话的默认值与集群一致：`sql_mode`、`time_zone`、`div_precision_increment` 等直接影响查询结果。etcd 通知（1 秒内生效）仍留给 D06.5。**做成同一系列中单独的一个 PR**（T10），理由：它依赖 D01.11（tablecodec，评审中）和 tidb-kv 的快照读，与本块的 SessionVars 移植没有代码交叉；拆开后本块可以先合并，T10 等 D01.11 合并后基于 main 开发，评审也更聚焦。

### 2.3 钩子的处置（N8 §3.4）

每个钩子按落点分四类，结果写入 tracker `sysvar_hooks.tsv` 的处置列：

| 类别 | 处置 |
|---|---|
| 落点在 SessionVars 的强类型字段（绝大多数 SetSession / GetSession） | L1 移植 |
| 落点是进程级状态，tidb-rs 有对应物（内存限制 A1、client-rust 配置等） | L1 / L3（client-rust） |
| 落点是 Go runtime、client-go 专有、DDL / GC / 统计 owner 等 tidb-rs 不承担的组件 | runtime-only：值照常校验和存储，取值不同于默认值时追加 warning（N8 §3.4） |
| 只影响推迟的功能（MPP、TiFlash） | DEF.MPP：值照常存储，效果随该功能 |

SetGlobal 钩子由全局缓存重建时调用（N8 §3.3，D06.5）；本块移植钩子本身，调用点在 D06.5 接上。

## 3. 验证

- **govec `sessionvars` 领域**（tracker）：对每个变量执行 SET（session / global）、`GetSessionOrGlobalSystemVar`、`GetSystemVar`，记录 delta、钩子执行后受影响的强类型字段（通过反射导出的字段快照）、warning 和错误；用户变量、prepared 语句、`TxnReadTS`、savepoint、游标、slow log 规则解析的脚本；minelits 抽取 `session_test.go`、`varsutil_test.go`、`slow_log_test.go` 的字面量；多个种子。
- **钩子清单的机器核对**：Rust 侧每个 SetSession / GetSession / SetGlobal / GetGlobal / GetStateValue 钩子都对应清单中的一行，清单中每一行要么有 Rust 钩子，要么有处置列的原因；单元测试检查两者一致（与 D05.1 的 `registry_matches_hand_written_hooks` 同样的做法）。
- **编译期断言**：`SessionVars: Send`，`SessionVars: !Sync`（N2 §5 第 1 条）。
- **初始化顺序无关**（Lucky 的评审）：回放时取一组变量，按两种不同的顺序执行 SetSession 初始化，要求得到相同的强类型字段和 `GetSystemVar` 结果，用来发现依赖顺序的钩子（Go 按 map 随机顺序调用）。
- 反证：故意弄坏若干钩子和 delta 逻辑，回放必须报错。

## 4. 偏离

1. `cachedStmtCtx` 轮换复用不移植（N2 §3.4，L2）。
2. 会话初始化时 SetSession 的调用顺序：注册表顺序 + `Depended` 最后（Go 是 map 随机顺序，M-MAPORDER）。
3. 全局值快照在 P1–P2 是"默认值 + 实例覆盖"的桩（§2.2）。
4. SET GLOBAL 的持久化：负责人已决定 (c)（#58，设计见 §6）；在 §6 的写路径合并之前，PR A 对需要持久化的变量暂时报"不支持"（过渡行为），INSTANCE 作用域的变量与 Go 相同。实现为 `SessionVars::set_global_sys_var`（Go 的对应物是 session 包的 `SetGlobalSysVar`，块 D05.3），错误是 `ErrNotSupportedYet`（1235），参数为 `SET GLOBAL on tidb-rs; run it on Go TiDB`。
5. 写路径字段（X）的 10 个 SetSession 钩子（`tidb_batch_insert` 等）只更新 `systems[]`，不设字段（字段在 D05.2 的划分中归 X）。
6. 写全局 config 中非原子字段的 INSTANCE 钩子（Go 原地写字段）：Rust 的 config 是不可变快照（M-SNAP），钩子克隆 → 修改 → store；并发的写者由 `config::update_global_config` 的写锁串行化（`config::update_global` 也走这把锁），不同字段的修改不会丢失（B1 评审第 1 条；测试 `crates/base/tests/config_update.rs`）。
7. 上游修复的移植（宪章 R1）：`fixcontrol.ParseToMap` 在冒号后为空时（`'123:'`），v8.5.8 取 `s[0]` 越界 panic；master 已修（pingcap/tidb#65884，`811a10e115`，未回移到 release-8.5）：值为 `""`，无错误、无 warning。Rust 移植修复（`// go-fix`），govec 用 tracker `govec/fixes` 的修复版生成这些记录（`src: "fix pingcap/tidb#65884"`）。
8. 错误的客户端视图（M-ERR）：Go 的 server 向客户端写 `errors.Cause(err)`，`Wrap` / `Wrapf` / `Annotate` 的前缀只进 Go 的日志。Rust 不移植这些包装，返回客户端看到的错误；govec `sessionvars` 记录 `errors.Cause(err)`。目前涉及的只有 LDAP `ca_path` 钩子（`initializeCAPool`，#64）。已移植代码中其他 Wrap / Annotate 的位置（B1 评审第 3 条的核查）：`types` 的 enum / set 转换（D01.1，已按 cause 处理）；`util/cgroup` 的 `Wrapf`（D15.3，按拼接移植，但只在启动检测内存限制时出现在日志里，不到达客户端）；`rowcodec` checksum 编码的 `Annotatef`（D01.11，不到达客户端）。

9. `tidb_slow_log_rules` 的 `RawRules`（M-MAPORDER，L2）：Go 用 map 构造一条规则的条件，`ParseGlobalSlowLogRules` 也按 map 顺序拼接各连接的规则集，所以一条规则有两个以上条件、或全局值有两个以上连接组时，`SHOW VARIABLES` 的值和 `RawRulesHash` 每次运行都可能不同。Rust 把条件按字段名排序、连接组按连接 ID 排序。govec 在 Go 与 Rust 两侧都把读回值规范化（每条规则的条件排序，再把规则排序）后比较（`slow_log_rules.rs`，T7）。只移植了 `SlowLogRuleFieldAccessors` 的 `Parse`；`Setter` / `Match` 在语句结束后匹配慢日志，属于 D14.2。
10. `net/url` 的移植（`tidb_base::gostd::url`，`ast.RedactURL` 用到的部分）：IP 字面量的检查（`netip.ParseAddr` + `Is4`）用 Rust 的 IPv4 / IPv6 解析器加 Go 的 zone 规则（L2）；这一分支的错误文字与 Go 不同，`RedactURL` 丢弃解析错误，只看成败。`urlmaxqueryparams` GODEBUG 不读（用默认上限 10000）。
11. SET GLOBAL 的写入（PR C，块 D06.7）：
    - **row format 1**：Go 的 `replaceGlobalVariablesTableValue` / `SetTiDBTableValue` 经 `sysSessionPool` 的内部会话执行，它不加载全局的 `tidb_row_format_version`（`RowEncoder.Enable` 为 false），row checksum 也关闭（`InRestrictedSQL`），所以总是写 row format 1。原因是内部会话由 `createSessionWithDomainFunc`（session.go:1214）创建时直接设置 `CommonGlobalLoaded = true`，**所有**全局变量都不加载，与启动时序无关；`tidb_retry_limit` 同理取默认值 10。unistore 会在 prewrite 时把 format 1 的行转成 format 2 存储（`unistore/tikv/mvcc.go:1194` `encodeFromOldRow`），在 unistore 上看不出来，需在 TiKV 上验证（Lucky 评审 #70，记录在 #74）。tidb-rs 照此写入（`tablecodec.EncodeOldRow` 从 X 移回 D01.11），playground 上与 Go 写入的行逐字节相同。§6.2 第 5 步原先"按集群全局值"的设计因此作废。
    - **assertion 和 request source**：tidb-systable 产出与 Go 相同的 key flags（govec 逐条比对），但 client-rust 分支不支持 assertion 和 request source，提交时丢弃（TiKV 在任何 `tidb_txn_assertion_level` 下都接受不带 assertion 的 mutation）。另外，内部会话的 `tidb_txn_assertion_level` 也是默认值 OFF（同样不加载全局变量，#74），所以 Go 在 TiKV 上同样不检查 assertion，丢弃 assertion 与 Go 比较时观察不到差别（据代码推断，未实测）；presume-not-exists 的 key 以 `Op_Insert` 提交，与 client-go 相同。
    - **PD GlobalConfig**：client-rust 分支没有 GlobalConfig API，`NotifyGlobalConfigChange` 只记 warning（Go 写 PD 失败时也只记日志）。
    - **watch 关闭或 compaction**：Go 只重建 watch、不立即重读；tidb-rs 重建 watch 后立即重读一次（§6.2）。
    - **`rebase4Signed`**：只在 offset > 1 时用到，`AllocHandle` 总是传 1，没有移植，遇到时返回错误。
    - **超长值**：按严格模式返回 `ErrDataTooLong`；Go 的内部会话 SQL mode 是否严格未核对（SET GLOBAL 的值不会超过 16383 字符的情况下不可达）。

12. **`tidb_redact_log` 只有 GLOBAL 作用域（go-fix pingcap/tidb#62892）**：v8.5.8 中它有 SESSION 作用域，SetSession 钩子写进程级的 `errors.RedactLogEnabled`，一个会话的 `SET SESSION tidb_redact_log=ON` 会让同一实例上所有会话的错误和 warning 被打码（#62，上游 pingcap/tidb#62890）。上游 master 改为只有 GLOBAL 作用域、由 SetGlobal 钩子写这个开关（commit `bc0d97a44d`，未 backport 到 release-8.5），tidb-rs 按宪章 R1 移植（与 #65884 相同）：`SET SESSION` 返回 1229，`SET GLOBAL` 写进程级开关（`tidb_base::errors`，原先 `tidb_vars` 中另有一份不相连的替身，已删除）。govec 用打了补丁的 Go 生成 sessionvars、sysvar、exprctx 三个领域（header 的 `fixes` 中列出），`set_global_hook` 记录读回开关的值（`redact`）。上游后来（#63709）又加回 SESSION 作用域并标为 `InternalSessionVariable`，用户 SET SESSION 报 unknown variable；那是新功能，不移植。
13. **PR D 的会话方法**：
    - **`TxnReadTS`**：Go 是 `*TxnReadTS`，新会话中为 nil，只有 `tx_read_ts` / `tidb_snapshot` 的钩子和 `CleanupTxnReadTSIfUsed` 会赋值；nil 接收者的 `UseTxnReadTS` 返回 0、`SetTxnReadTS` 什么也不做。Rust 用 `Option<TxnReadTS>`，两个方法接收 `Option<&mut TxnReadTS>`（L1）。
    - **`UserVars`**：Go 用 `sync.RWMutex` 保护两个 map，按指针共享；Rust 是 `Arc<UserVars>` 加 `parking_lot::RwLock`。`GetUserVarType` 返回存储的 `FieldType` 的副本（Go 返回指针，TiDB 的调用方只读，L2）。
    - **`PreparedStmts` / `MapScalarSubQ`**：Go 的 `map[uint32]any` / `[]any` 存 planner 的 `PlanCacheStmt`、子查询；Rust 存 `Arc<dyn Any + Send + Sync>`，由 planner 向下转型（M-IFACE）。`metrics.PreparedStmtGauge`、`metrics.ConnGauge` 是 runtime-only。
    - **`PlanID` / `PlanColumnID`**：Go 的 `atomic.Int32` / `atomic.Int64`，Rust 同样用原子量，`AllocNewPlanID` 经 `&SessionVars` 调用；`SessionState` 克隆时复制当前值。
    - **`relatedTableForMDL`**：Go 的 `*sync.Map`（表 ID → schema 版本）由 `tdmLock` 保护；Rust 是 `Option<RelatedTableForMDL>`（`Arc<Mutex<HashMap<i64, i64>>>`），`&mut self` 代替 `tdmLock`。
    - **`AddPlanReplayerFinishedTaskKey`**：Go 在 map 从未初始化时直接写 nil map（panic）；调用方总是先调用 `CheckPlanReplayerFinishedTaskKey`（它会初始化），Rust 在这种情况下先建 map。govec 的记录按 Go 调用方的顺序先 Check 再 Add。
    - **`TransactionContext.Cleanup`**：只清理读路径的字段（`relatedTableForMDL`、`IsStaleness`、`EnableMDL`）；`Binlog`、`History`、`TableDeltaMap`、悲观锁缓存、`Savepoints` 属于写路径（X）或 PR E（L2）。
    - **`ChooseMppExchangeCompressionMode`** 返回 `i64`（`kv.ExchangeCompressionMode` 属于 DEF.MPP，常量就地定义）；`GetSplitRegionTimeout` 返回纳秒（`time.Duration`）。
    - **`PlanCacheParamList.String`、`PrepareDedupCacheKey`** 返回 Go string 的字节（`Vec<u8>`，M-BYTES）；`GetParamValue` 越界时 panic 的文字与 Go 的 runtime 错误相同（负数下标没有 `with length`）。

## 5. 验证结果（PR A）

- **govec `sessionvars`**：41,447 条（`fresh` 1、`set` 30,939、`get` 945、`set_global` 9,482、`init` 80）。回放跳过仍有 pending 钩子的记录：36,046 条比对、0 条不一致，5,401 条跳过；钩子落地后自动开始比对，不需要重新生成。
- **生成的确定性**：INSTANCE 变量的 SET GLOBAL 会改进程级配置，生成器在每条记录后用 SetGlobal 钩子恢复原值；`timestamp`（未设置时取当前时间）、`rand_seed1/2`（会话随机数）和依赖宿主机的 `version_compile_os` / `version_compile_machine` / `tidb_config` 的读回值记为 `<volatile>`；经 GetGlobal 钩子读回的值（读进程级状态，同一进程中前面的领域会改它）记为 `<global-hook>`，等 GetGlobal 移植时再配合状态重置生成。
- **进程级状态在记录之间的泄漏**（Lucky 评审 #57）：Go 的 `tidb_redact_log` 的 SetSession 钩子写的是进程级的 `errors.RedactLogEnabled`（上游问题 #62：它会让所有会话的 warning 被打码），生成器中一条记录设为 ON 后，后面所有记录的 warning 都变成 `'?'`。现在每条记录之前都重置进程级状态（tracker `resetSysvarAtomics`，Rust 回放的 `reset_atomics` 同样处理）。按 v8.5.8 源码逐个检查，钩子写到的进程级状态只有：
  - Validation 钩子：密码校验的 4 个 atomic、`GOGCTunerThreshold`、`RunAutoAnalyze`、`EnableAutoAnalyzePriorityQueue`（每条记录前重置）；
  - SetSession 钩子：`tidb_redact_log` → `errors.RedactLogEnabled`（每条记录前重置）；`tidb_constraint_check_in_place_pessimistic` → 一个 metrics 计数器（任何记录都观察不到）；
  - SetGlobal 钩子：27 个 INSTANCE 作用域变量（日志、慢日志、插件、内存与 expensive query 阈值、`max_connections`、DDL / stats owner、statement summary 文件、service scope 等），`set_global` 在调用后立即写回原值。
  其余 SetSession 钩子只写会话。**#62 的进程级行为**：负责人决定移植上游修复 pingcap/tidb#62892（§4 第 12 条），记录之间不再因 SET SESSION 泄漏。
  - 验证：去掉 `RedactLogEnabled` 的重置后，种子 84 和 2 的生成结果中出现 720、724 处 `'?'`，Rust 回放种子 84 报 425 条不一致；加上重置后种子 1–4、81–84 本地生成并回放全部一致（比对约 36,030 条，跳过约 5,410 条）。提交的种子的向量逐字节不变。
- **随机设置组为空**（Lucky 评审 #57）：8 个随机变量都没有 SESSION 作用域时，组为空，Go 写出 `"args":[null]`，Rust 回放会 panic。生成器现在跳过空组；回放把 `null` 当作空列表处理。
- **跳过数量的上限**：回放要求提交的向量中因 pending 钩子跳过的记录不超过上限（当前 5,401），系列中每个 PR 下调，PR B 之后为 0，避免 pending 项长期留着。
- **一次性翻译脚本**：183 个 SetSession 钩子由 `tools/d05.2-hook-translation/` 中的脚本翻译（只为留档，不参与构建，README 写明了做法和输入）。
- **T11**：40 组随机设置，每组按正反两种顺序各生成一条；Go 的 40 组结果都与顺序无关，Rust 每条都与 Go 相同。
- **反证**：

| 弄坏的地方 | 不一致 |
|---|--:|
| SetSessionFromHook 不写别名的 systems[] | 112 |
| GetSessionOrGlobalSystemVar 不把全局值懒加载进 systems[] | 1,070 |
| 一个翻译出的钩子取反（`!tidb_opt_on`） | 22 |
| SetSystemVarWithRelaxedValidation 不做 relaxed 校验 | 837 |

  去掉 INSTANCE 变量 SET GLOBAL 时的 SetGlobal 调用，回放**没有**发现：这些记录都因 SetGlobal 钩子 pending 被跳过，T5 之后才有覆盖。

- **钩子清单**：`registry_matches_hand_written_hooks` 检查每个变量的五类钩子与 Go 的标志一致（已移植或 pending，Rust 没有 Go 没有的钩子）。当前：SetSession 227/273、GetSession 12/27、GetStateValue 3/6、SetGlobal 0/155、GetGlobal 0/134。

## 6. SET GLOBAL 的持久化与全局值同步（负责人决定 #58 → 方案 (c)；设计已经 Lucky 评审同意，按 #57 评审的 7 点补充修订）

负责人决定：tidb-rs 的 `SET GLOBAL` 与 Go 完全一致：持久化到 `mysql.global_variables` 并通知其他实例；INSTANCE 作用域仍不持久化（与 Go 相同）。本节是设计，**写路径尚未实现**；在它合并前，PR A 中 `set_global_sys_var` 对持久化变量仍返回 "not supported"（§4 第 4 条，过渡行为）。T10（读集群全局值）并入本设计。

### 6.1 Go v8.5.8 的实际行为（已核对源码；Lucky 复核）

**写**（`session.SetGlobalSysVar`，session.go:1336）：
1. `sv.Validate(ScopeGlobal)` → `sv.SetGlobalFromHook`（**先**在本实例执行 SetGlobal 钩子）。
2. INSTANCE 作用域在这里返回（session.go:1347），不写表、不通知。
3. 有 `GlobalConfigName` 的变量：`domain.NotifyGlobalConfigChange` → PD 的 GlobalConfig（`globalconfigsync`，`pd.GlobalConfigItem{Name, Value: OnOffToTrueFalse(v), PUT}`）。
4. `replaceGlobalVariablesTableValue`（session.go:1285）：内部 SQL `REPLACE INTO mysql.global_variables (variable_name, variable_value) VALUES (?, ?)`，`InternalTxnSysVar` 作为 request source，经 `sysSessionPool` 的内部会话执行。
5. `domain.NotifyUpdateSysVarCache(updateLocal=true)`（domain.go:3065）：etcd `Put("/tidb/sysvars", "")`，失败只记 warning；然后本地 `rebuildSysVarCache`。

**表**：`CREATE TABLE mysql.GLOBAL_VARIABLES(VARIABLE_NAME VARCHAR(64) NOT NULL PRIMARY KEY, VARIABLE_VALUE VARCHAR(16383) DEFAULT NULL)`（bootstrap.go:195）。bootstrap 时 `tidb_enable_clustered_index = INT_ONLY`，VARCHAR 主键因此是 **NONCLUSTERED**：行键是 `_tidb_rowid`，主键是唯一索引（Lucky 在 v8.5.8 上确认）。

**REPLACE 的 KV 效果**：按唯一索引查到旧行 → 删除旧行的 record key 和索引 key → 用表的 autoid 分配器取新的 `_tidb_rowid` → 写新行（row format 由内部会话的 `tidb_row_format_version` 决定，默认 v2）和唯一索引（值里带 handle）。

**事务**：自动提交的语句在 `PessimisticAutoCommit = false`（默认）时是**乐观事务**（session.go:4197）。自动提交的乐观事务"总是可以重试"（`isOptimisticTxnRetryable`，optimistic.go:89），提交遇到可重试错误（写冲突、schema 变化）时，最多重试 `tidb_retry_limit`（默认 10）次，每次重新执行整条 REPLACE（session.go:812–818）。所以两个实例并发 `SET GLOBAL` 同一变量时，结果是**后提交者生效**，不会报错给用户（除非重试耗尽）。

**读与同步**（`domain.LoadSysVarCacheLoop`，domain.go:2034；`rebuildSysVarCache`，sysvar_cache.go:104）：
- 启动时先 rebuild 一次；之后 `select` 三路：etcd `Watch("/tidb/sysvars")` 事件、30 秒定时、退出。watch 通道关闭时重建 watch（超过 10 次后退避）。
- rebuild：`SELECT variable_name, variable_value FROM mysql.global_variables`；对每个注册变量取表值或默认值；`!SkipInit()` 的进 session cache，有 GLOBAL 作用域的进 global cache；有 SetGlobal 钩子且 `!SkipSysvarCache()` 的，先 `ValidateWithRelaxedValidation` 再调用 SetGlobal（所以其他实例也执行钩子）。重建串行（`rebuildLock`）。
- 新会话的 `systems` 拷贝自 session cache；`GetGlobalSysVar` 读 global cache，缺项时回表读，再 `ValidateFromType`，失败用默认值。

### 6.2 tidb-rs 的设计

**读（T10，先做）**：`GlobalVarsCache`（domain 级，`ArcSwap<Arc<Snapshot>>`）按 §6.1 的 rebuild 逻辑实现，快照 = session map + global map；`GlobalVarAccessor` 的实现读它（替换 PR A 中由调用方提供的 accessor）。读表用 tidb-kv 快照读 + tablecodec 解码：通过 D06.2（meta 读取）拿到 `mysql.global_variables` 的 TableInfo，扫描 record 范围。驱动：启动时一次 + etcd watch + 30 秒定时，与 Go 相同。rebuild 时执行 SetGlobal 钩子（T5 移植完成后生效）。

**写**：只做 `mysql.global_variables` 这一张表的 REPLACE，不引入通用 DML 执行器：
0. **前置检查（版本与表结构，弥补没有 schema validator 和 MDL）**：Go 的 DML 由 schema validator（schema 变化是可重试的提交错误）和 MDL（DDL 等待进行中的 DML）保护，专用 writer 两者都没有；滚动升级时 bootstrap 会对 `mysql.*` 执行 DDL。规则：只有当 `mysql.tidb` 中的 `tidb_server_version` 等于本 writer 所针对的 bootstrap 版本，**并且**事务开始时读到的 `mysql.global_variables` 的 TableInfo（列、索引、`UpdateTS` / revision）在提交前没有变化时才写入；否则返回明确的错误（"cluster bootstrap version / table schema changed, SET GLOBAL refused"），不猜测。
1. 开一个**乐观**事务（client-rust `TransactionClient::begin_optimistic`），request source 标为 `internal_sysvar`（与 Go 的 `InternalTxnSysVar` 一致，便于资源管控和监控对比）。
2. 读唯一索引 key（`tablecodec.EncodeIndexSeekKey` + `codec.EncodeKey(collation key of name)`）→ 得到旧 handle。
3. 有旧行：`delete(record key)`、`delete(index key)`。
4. 分配 `_tidb_rowid`：NONCLUSTERED 表用**行 ID 分配器**，即 meta 中该表的 `TID:<id>` auto-ID key，按 Go 的默认批量步长、在 meta 写冲突时按 Go 的方式重试，与 Go 实例共享同一个计数器。分配在**单独的 meta 事务**中进行（与 Go 相同），不在第 1 步的事务里。表的 `SHARD_ROW_ID_BITS = 0`，没有 shard 位；但要检查表上的 `AUTO_ID_CACHE`（旧集群可能设置过），不是预期值时按第 0 步拒绝写入。handle 不必与 Go 相同，只要不冲突；行内容逐字节相同。
5. `put(record key, 两列的行编码)`、`put(index key, 唯一索引值：带 handle 的 version 0/1 格式，与 Go 相同)`。
   - **编码设置跟随集群**：Go 的内部会话使用 `tidb_row_format_version` 的全局值（从旧版本升级上来的集群是 v1）和 `tidb_enable_row_level_checksum`（row v2 中的 checksum）。从全局值缓存（§6.2 的读）读取这两个值：row v1、v2 都支持，或者在格式为 v1 时拒绝写入（实现时二选一，在 PR 中写明）。
   - **断言：要么与 Go 完全一致，要么不加**。`tidb_txn_assertion_level = FAST` 时，错误的断言会让 TiKV 拒绝提交，而不加断言总是被接受。先**不加断言**；只有混合集群测试确认 flag 与 Go 相同（删除的行和索引 key 为 Exist，新的为 NotExist）后再加。
6. 提交（2PC）。遇到可重试错误按 Go 重试：**每次重试都开新的事务（新的 start ts）**，从第 1 步重新执行整条 REPLACE（包括第 0 步的检查），而不是在原事务中从第 2 步重做，否则会读到旧的快照；最多 `tidb_retry_limit` 次。
7. `PUT /tidb/sysvars` 到 etcd（失败只 warning），然后本地 rebuild。
8. 有 `GlobalConfigName` 的变量：写 PD GlobalConfig（client-rust 的 PD 客户端需要补 GlobalConfig 的 `StoreGlobalConfig` 调用；若 client-rust 暂不支持，作为同系列中的一个小 PR）。

**需要从 X 拿回的函数**（D01.11、D07.0 中按"写路径"归入 X 的部分），建议放进一个新块 **D06.7 "系统表写入：mysql.global_variables REPLACE"**，在 tracker `rules.py` 中重新归属并写明理由：
- tablecodec / rowcodec：行的 v2 编码器（`rowcodec.Encoder` 及其排序）、`EncodeRow`、唯一索引值的编码（`GenIndexValuePortal` / `GenIndexValueForClusteredIndexVersion1` 中非聚簇唯一索引的路径）、`GenIndexKey`。
- kv：事务写接口（`Set`/`Delete`、断言 flag 的最小子集），对应 D07.0 中归 X 的 keyflags 的一部分。
- autoid：`meta.GenAutoTableID` 的 key 布局与增量写（D06.2 的写的一小部分）、分配器的批量与本地缓存。
- 其余（通用 REPLACE / INSERT 执行器、DML 计划构建）仍为 X。

**etcd watch 的重建**：watch 遇到 compaction（`ErrCompacted`）时，和通道关闭一样处理：从当前 revision 重新 watch，并立即 rebuild 一次（错过的事件由这次 rebuild 覆盖）。

**etcd 客户端**：选 `etcd-client` crate（tonic，tokio 原生，支持 TLS、watch、lease，维护活跃）。地址与 Go 相同，来自 PD（`path = "pd1:2379,…"`，Go 的 `etcdClient` 就是连 PD 内嵌的 etcd），TLS 证书沿用集群安全配置。备选 `etcd-rs` 维护较少，不选。

### 6.3 与并发的 Go 实例互操作

- 行和索引的格式与 Go 相同，Go 实例的 `rebuildSysVarCache` 和 `ADMIN CHECK TABLE mysql.global_variables` 都能处理 tidb-rs 写的行。
- 并发写同一变量：都是乐观事务 + 自动重试，最终"后提交者生效"，与 Go 实例之间的行为相同。
- autoid：共享 meta 计数器，批量分配互不重叠；tidb-rs 进程重启会丢弃本地未用完的区间（Go 也一样）。
- 通知：任一实例写 etcd key，所有实例（Go 和 tidb-rs）收到 watch 事件后重读表；watch 丢失时 30 秒兜底。

### 6.4 失败模式

| 情况 | Go 的行为 | tidb-rs |
|---|---|---|
| SetGlobal 钩子已在本实例执行，写表失败 | 本实例已生效，表未变，返回错误；30 秒后本实例 rebuild 会把值改回表中的值 | 相同 |
| 写表成功，etcd PUT 失败 | warning；其他实例最多 30 秒后看到 | 相同 |
| 重试耗尽（持续写冲突） | 返回写冲突错误 | 相同 |
| etcd 不可用 | 只能靠 30 秒定时 | 相同 |
| 表结构不是预期的（例如旧版本升级上来的集群） | Go 用当前 TableInfo | 用 D06.2 读到的 TableInfo；列 / 索引 / `AUTO_ID_CACHE` 与预期不符时拒绝写入并报错（不猜测） |
| 滚动升级中，`tidb_server_version` 与本 writer 的 bootstrap 版本不同 | DDL 与 DML 由 MDL 协调 | 拒绝写入（§6.2 第 0 步），升级完成后恢复 |
| 事务开始后表的 TableInfo 变化（并发 DDL） | schema validator 报可重试错误，重试 | 提交前再次检查 TableInfo，变化则放弃本次事务，按第 6 步开新事务重试 |
| tidb-rs 在 prewrite 之后、commit 之前被杀 | 锁由后续读者 resolve | 相同：锁由 Go 和 tidb-rs 的读者按 Percolator 协议 resolve |

### 6.5 其他副作用（待在集群上确认）

- **统计信息增量**：Go 的事务提交会把表的 modify count / row count 增量交给 stats handle（`TableDelta`），系统表也不例外。tidb-rs 不维护统计信息的写入（ANALYZE 等归 X），这一项不更新会导致 `mysql.stats_meta` 中该表的 modify_count 与"全由 Go 写入"时不同。需要在 playground 上确认 Go 是否真的为 mysql 库的表记 delta；若记，评估是否需要照做（影响很小：只影响 auto-analyze 对这张小表的判断）。
- **TiCDC**：changefeed 的默认 filter 排除 `mysql` 等系统库，tidb-rs 写入与 Go 写入在 TiKV 层完全一样，TiCDC 看到的变化相同。binlog 已废弃，不考虑。

### 6.6 测试计划

1. **单元级**：用 govec 生成 Go 对 `mysql.global_variables` 一行的 record 值和唯一索引 key / value（同一 TableInfo、同一 handle），Rust 编码逐字节相同。
2. **混合集群**（tiup playground 起 Go TiDB，`scripts/playground-join.sh` 加入 tidb-rs，使用各自独立的 playground，不碰别人的）：
   - Go 上 `SET GLOBAL`，tidb-rs 在 1 秒内（etcd）和 30 秒内（断开 etcd 时）看到；反之亦然。双方 `SHOW GLOBAL VARIABLES` 逐行相同。
   - 两边交替、并发对同一变量 `SET GLOBAL`，最终值与只用两个 Go 实例时的行为相同；无错误返回（重试吸收冲突）。
   - tidb-rs 写入后在 Go 上执行 `ADMIN CHECK TABLE mysql.global_variables`，通过。
   - 原始 KV 对比：同一变量分别由 Go 和 tidb-rs 写入，行值与索引值逐字节相同（handle 除外）。
   - autoid：tidb-rs 连续写入多次后，Go 写入不冲突。
   - 有 GlobalConfigName 的变量：PD GlobalConfig 中的值与 Go 写入时相同。
   - INSTANCE 作用域：不写表、不通知。
   - 统计信息增量和 TiCDC：按 §6.5 观察并记录。
   - **在 prewrite 和 commit 之间杀掉 tidb-rs**（failpoint 或 SIGKILL）：Go 读者 resolve 掉残留的锁，`SHOW GLOBAL VARIABLES` 与 `ADMIN CHECK TABLE` 正常。
   - **滚动升级**：bootstrap 版本不同期间，tidb-rs 的持久化写入被拒绝（明确的错误），升级完成后恢复。
3. **反证**：故意用错 row format、漏写索引、不发 etcd 通知，上述检查必须失败。

### 6.7 验证结果（PR C 第三轮，2026-11-11）

- **govec `write_systable`**（tracker `govec/domains/systable.go`）：oracle 是 Go 在 bootstrap 过的 mockstore 上经 `sysSessionPool` 会话真实执行的 REPLACE（`BEGIN OPTIMISTIC` 内，取事务 memdb 的 key、value、flags 后回滚），168 条：replace 97（rf 1 93、rf 2 4）、alloc 29、next_step 42、tableinfo 2；Rust 回放 0 条不一致，种子 1–4 都通过。反证：索引删除去掉 need_locked 报 25 条；assertion 改为后者覆盖报 25 条；行比较忽略大小写报 1 条；去掉 `MaxInt64 - newBase` 上限报 3 条；`EncodeOldRow` 的列 ID 加 1 报 81 条。
- **混合集群**（`scripts/systable-mixed.sh`，自己的 v8.5.8 playground）全部通过：
  1. tidb-rs 写入，Go 在 1.5 秒内读到（etcd 通知），`ADMIN CHECK TABLE mysql.global_variables` 通过；
  2. Go 写入，tidb-rs 读到；
  3. 同一行分别由 Go 和 tidb-rs 写入，record 和 index value 逐字节相同（handle 除外），mysql.tidb 同样，`ADMIN CHECK TABLE mysql.tidb` 通过；
  4. 不发 etcd 通知时，Go 在 30 秒的定时重读后看到（实测 30 秒）；
  5. Go 和 tidb-rs 各 40 次并发 SET GLOBAL 同一变量：两边都没有错误，只有一行，`ADMIN CHECK TABLE` 通过。
- **§6.5**：Go 和 tidb-rs 写入后 `mysql.stats_meta` 中都没有这两张表的行，Go 不为它们记统计增量。
- **未测**：prewrite 与 commit 之间杀进程（需要 failpoint）、滚动升级（只有单元测试：bootstrap 版本不是 232 时拒绝）。

