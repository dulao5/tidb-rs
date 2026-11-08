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

- **`GetSystemVar` / `GetSessionOrGlobalSystemVar`**：先查 `delta`，再查 `globals`；有 GetSession 钩子的 27 个变量由钩子从强类型字段现算（N8 §3.2）。快照缺项时取 domain 当前值并写入 `delta`，与 Go 的懒加载兜底相同（L1）。
- **`SetSystemVar*`**：校验（D05.1）→ 写 `delta` → 调 SetSession 钩子更新强类型字段，顺序与 Go 相同。`SetSystemVarWithOldValAsRet` 返回旧值，供 `SET_VAR` hint 在下一条语句开始时恢复（N2 §3.2，L1）。
- **会话初始化**：Go 在第一次 `Parse` / `ExecuteStmt` / `PrepareStmt` 时对每个 `!SkipInit()` 变量调用 SetSession。Go 按 map 的随机顺序调用；`Depended` 的 4 个变量要求在其依赖之后执行。Rust 按注册表顺序，并把 `Depended` 变量放到最后（D05.1 的 `OrderByDependency`，M-MAPORDER）。
- **`Cloned` 字段**：Go 的 `cachedStmtCtx[2]` 轮换复用、`StmtCtx.Reset` 不移植（N2 §3.4，L2）。

### 2.2 全局值的来源（P1–P2 桩）

N8 §8：§3.3（domain 的全局缓存、etcd 通知、30 秒兜底重读）属于 D06.5（P2）。P1–P2 之间，`GlobalVarsSnapshot` = **注册表默认值 + 实例覆盖（`SetSysVar`）**，作为桩登记在 tracker（`glue: stand-in: D06.5`）。接口按 D06.5 的需要定：domain 发布新快照（`ArcSwap`），会话取 `Arc`。

**SET GLOBAL**（owner issue #58，按建议 (a) 推进）：
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

## 4. 偏离（预计）

1. `cachedStmtCtx` 轮换复用不移植（N2 §3.4，L2）。
2. 会话初始化时 SetSession 的调用顺序：注册表顺序 + `Depended` 最后（Go 是 map 随机顺序，M-MAPORDER）。
3. 全局值快照在 P1–P2 是"默认值 + 实例覆盖"的桩（§2.2）。
4. SET GLOBAL 按 N8 待定 1 的建议 (a) 报"不支持"（§2.2），等负责人决定。
