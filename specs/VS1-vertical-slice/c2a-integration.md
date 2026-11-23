# 点查纵向集成（C2a）：结果、性能与剩余缺口

状态：VS1-20 的集成记录（2026-11-23）。对应 tasks.md 中 VS1-20 一行的完成条件：ab `strict` 的 `Point_Get` 分类、性能（负责人的门槛：不比 Go 慢）、分配次数与 profile、`pseudoSelectivity` 用例。

## 1. 现在什么在新栈上跑

sysbench 的预处理点查 `SELECT c FROM sbtest%u WHERE id=?`（二进制协议 PREPARE / EXECUTE），在 `--new-stack=strict` 和 `fallback` 下都由新栈执行到底：

1. PREPARE：新栈准备（D05.3 第 2 部分、D13.1 的 `PlanCacheStmt`），列定义由新栈给出。
2. EXECUTE：协议层把二进制参数交给新栈（`parseBinaryParams`）→ `ExecutePreparedStmt` → `Compile` → `planner.Optimize`（plan cache 未命中的路径）→ 快速路径用参数值建点查计划（D10.8 spec §4a）→ `Compile` 保留 Execute 计划 → `ExecStmt.Exec` 换成其中的点查、新建点查执行器（D05.3 spec §10；VS1-20b 之后自动提交时改走点查短路径 `ExecStmt.PointGet`，MaxTS 时复用预处理语句里的执行器）→ 会话按 Go 的事务流程在 ported 存储驱动上开启只读事务（D07.1 spec），自动提交的主键点查用 MaxTS 读 → 结果由移植的 `writeResultSet` / `DumpBinaryRow` 编码（D03.1）→ `finishStmt` 提交只读事务。

一路上找到并修掉的缺口（按碰到的顺序），各自所在的 PR 见 §6：

| # | 现象 | 原因 | 修法 |
|---|---|---|---|
| 1 | EXECUTE 返回错误码 0 的 "malform packet error"，客户端报 "error packet without further information" | 协议层把参数段交给 PoC 解码时用 `Reader::rest()` 消费了读指针，再取一次给新栈时是空的（VS1-16 的结果编码与二进制参数 PR 引入） | 只取一次 |
| 2 | 停在 `getNameValuePairs`（`blocked-by:D13.1`） | 点查条件中的 `?` 没有接上参数值 | 照 Go 移植（D10.8 spec §4a） |
| 3 | 停在 `Compile`（`blocked-by:D13.1`） | Execute 计划的执行（复用点查执行器属于 plan cache，A3 未确认） | 走 Go 的非复用路径（D05.3 spec §10） |
| 4 | 停在 `active transaction fail: blocked-by:D07.1` | 新会话的存储是替身 | 存储驱动作为 `kv.Storage`、只读事务、PD oracle（D07.1 spec），服务器把它交给新会话 |
| 5 | 执行器打开之后报 `planner.IsReadOnly`（`blocked-by:D09.13`），路由器记为"打开之后的 blocked"（bug 路径） | `ExecStmt.IsReadOnly` 是桩 | 照 Go 移植 |
| 6 | 每次 EXECUTE 多一个 warning `skip prepared plan-cache: plan cache is disabled`（Go 没有） | 新会话从不加载全局变量（`loadCommonGlobalVariablesIfNeeded` 是桩），所以 SetSession 钩子没跑，`tidb_enable_prepared_plan_cache` 等字段停在结构体默认值（false） | 照 Go 移植加载，全局值的来源是替身（D05.3 spec §11） |
| 7 | 加载之后所有语句回退，原因 `session` | 路由器第 0 步把 `tx_read_ts` 的"非空"当作设置了读时间戳；Go 的钩子对空值也会放一个读时间戳为 0 的对象 | 只有读时间戳非 0 才算（D05.3 spec §11） |
| 8 | 加载之后点查报 1105 `interface conversion ... Duration (option LoadBasedReplicaReadThreshold)` | sessiontxn 把 `tidb_load_based_replica_read_threshold`（会话里是纳秒的 i64）原样作为选项值，存储驱动按 Go 的 `time.Duration` 取 `Duration` | sessiontxn 换成 `Duration` |
| 9 | `id='abc'` 这类有 warning 的点查：warning 数一致，但 `SHOW WARNINGS` 的内容 B 为空 | `SHOW WARNINGS` 总由 PoC 回答（VS1 plan §4.5 要求交给上一条语句的执行方） | 路由器在上一条语句由新栈执行时，用新会话语句上下文的 warning 回答（行同 Go 的 `fetchShowWarnings`） |

## 2. ab `strict` 结果（2026-11-23，本地 playground，Go TiDB v8.5.8，1 TiKV）

`ab sysbench -n 100` 加 `tools/ab/workloads/*.sql`，`--census-layer workload-strict`，tidb-rs 带 `--route-trace`；分类用 weekly 的 `tools/weekly/classify.py`（按 Go 的 EXPLAIN）。

| 分类 | match | mismatch | unsupported |
|---|--:|--:|--:|
| `Point_Get` | 155 | 0 | 168 |
| 合计（全部语句） | 859 | 13 | 936 |

- 预处理点查 `point_select / ps`：136 条全部在新栈上执行（普查 `new_stack`，原因 `ok`），136 条与 Go 一致（单一比较器，含 `SHOW WARNINGS` 的内容）；普查 `new_stack 136 (equal to Go: 136)`，负对照按要求失败。含边界参数：0、-1、`size+1`、region 边界两侧、字符串 `'abc'`（Go 与新栈都是 0 行、两条 1292 warning）、字符串数字。
- `Point_Get` 中的 168 条 unsupported（1235，都带 `blocked-by`，按比较器的 T1 不计一致也不计不一致）：
  - 140 条文本协议的点查（`point_select / text`）：`blocked-by:D05.3`，普通语句的 `Compile` 要一份可写的 AST（D05.3 spec §9.2，随 VS1-37）；
  - 6 条 `SELECT ?, c FROM sbtest1 WHERE id=?`（两种协议）：Go 的计划是 Projection + Point_Get，走完整的规划器，停在计划回放的采集（`blocked-by:D02.2`，见 §5 第 3 条），之后还要 Projection 执行器（D12.2，VS1-35）；
  - 4 条预处理点查的参数超出 `INT` 范围（`-2^63`、`2^63-1`、`2^63`、`2^64-1`）：Go 的快速路径在这里给 TableDual，新栈停在 `PhysicalTableDual`（`blocked-by:D10.1`）——按文本语句分类时它们算 `Point_Get`（ab 用模板的文本语句分类），Go 实际的计划是 TableDual；
  - 其余为 workload 文件中的点查形状（同样是文本协议 D05.3，或要 Projection）。
- 13 条 mismatch 都由 PoC 回答（不经新栈）：sysbench 的 4 条预处理 SUM / DISTINCT 范围查询和 constants 的 9 条无参数预处理语句，Go 有一条 plan cache 的 warning（`skip prepared plan-cache: ...`），PoC 没有。与本项改动无关（main 上相同，见 §2.1）。

所以按完成条件的字面（"`Point_Get` 分类 100%，普查显示 new_stack、没有回退"）**还没有达到**：0 不一致，但还有 168 条 1235。剩下的都不是预处理点查本身，见 §5。

### 2.1 与 main 对比

同一 playground、同样的命令，main = `edb7c16`：

| 运行 | 模式 | main：一致 / 不一致 / 1235 | 本项：一致 / 不一致 / 1235 | 新栈执行且与 Go 一致 |
|---|---|---|---|---|
| sysbench `-n 100` | strict | 668 / 483 / 479 | 804 / 4 / 822 | 0 → 136 |
| 全部（sysbench + 3 个 workload，按分类） | strict | 714 / 492 / 602 | 859 / 13 / 936 | 0 → 145 |
| `Point_Get` 分类 | strict | 10 / 143 / 170 | 155 / 0 / 168 | — |
| sysbench `-n 100` | fallback | 1143 / 483 / 4 | 1611 / 12 / 7 | 0 → 136 |
| constants / sample / sql-layer | fallback | 34/9/3、8/0/4、93/1/18 | 34/9/3、8/0/4、94/0/18 | sql-layer 0 → 9 |

main 上预处理语句的 EXECUTE 全部到达新栈后因参数段为空返回错误码 0（§1 第 1 条），所以大量不一致；本项之后不一致只剩 PoC 回答的语句上 Go 的 plan cache warning（fallback 下 12 条：4 条超出范围的点查参数由 PoC 回答、8 条范围查询；constants 的 9 条），与 main 上同类语句相同。默认模式（fallback）没有退化。

## 3. 性能（不带 `--route-trace`，`--new-stack=fallback`）

sysbench 1.0.20 `oltp_point_select`，`--db-ps-mode=auto`，1 张表 1 万行（4 个 region），每轮 20 秒，A（Go）与 B（tidb-rs）依次跑：

| 线程 | Go QPS | tidb-rs QPS | 比值 | Go 平均延迟 | tidb-rs 平均延迟 |
|--:|--:|--:|--:|--:|--:|
| 1 | 8490.7 | 7915.3 | 0.93 | 0.12 ms | 0.13 ms |
| 8 | 31311.4 | 27895.3 | 0.89 | 0.26 ms | 0.29 ms |

（全部语句由新栈执行：`tidbrs_route_total{engine="new",reason="ok"}` 等于语句数。）注意 Go 打开了 plan cache 并复用点查执行器（`IsSafeToReusePointGetExecutor`），新栈每次重新规划、新建执行器（D05.3 spec §10.2）。负责人的门槛（不比 Go 慢，#263）还没有达到；按任务要求这里只测量，不调优。

### 3.1 每条语句的分配次数

| | 每条点查的分配次数 | 测法 |
|---|--:|---|
| Go TiDB | 约 200（含后台每秒约 3.5 万次的分摊，约 4） | 状态端口 `go_memstats_mallocs_total` 在 10 万条前后的差 / 10 万 |
| tidb-rs 新栈 | 约 566 | jemalloc 退出时的 `nrequests`（`_RJEM_MALLOC_CONF=stats_print:true`），10 万条与 1 条两次运行之差 / 99999 |
| tidb-rs PoC（`--new-stack=off`，参考） | 约 157 | 同上 |

新栈是 Go 的约 2.8 倍。

### 3.2 profile（macOS `sample`，8 线程，10 秒）

忙碌样本中：系统调用与锁（网络收发、条件变量）约 44%，分配 / 释放约 10%，`arc_swap` 的 debt 偿还约 9%（主要是内存 tracker 的 `BytesLimit`，以及另一个 `Option<Arc<..>>` 的 ArcSwap），`memmove` / `memcpy` 约 8%，其余约 30%（规划、执行、编码分散在各处）。可以考虑的方向（不在本项做）：

1. 每条语句都重新规划、重建执行器（plan cache 与执行器复用是 A3，未确认）。
2. `arc_swap` 的 debt 偿还：内存 tracker 的 `BytesLimit` 用 ArcSwap 发布，每条语句多次 load / store。
3. 语句上下文、会话视图（sessionexpr 的 `SessionView`，每条语句一次）、`String` 克隆。

### 3.3 VS1-20 一行要求记录的两项

- **pseudo 表**（D11.1 spec §3）：预处理点查走快速路径，不建统计信息，也不建 pseudo 表；这一项在点查上不发生，要等非点查语句（范围查询）走完整规划器时再测。
- **InfoSchema 查表的两处额外开销**（D06.3 spec §11.2）：PREPARE 把 SQL 多解析一次（PoC 与新栈各一次，只在 PREPARE 时）；每条语句在规划前提前加载用到的表（`load_stmt_tables`）。profile 中后者的样本可以忽略（10 秒内 1 个样本）。

### 3.4 点查短路径之后（VS1-20b，2026-11-24）

同样的方法（`oltp_point_select`，1 张表 1 万行、4 个 region，`fallback`，每轮 20 秒，3 轮平均，同一台机器同一个 playground；main 为 `f07d50d`；"短路径、不复用"是本项的第一版，"短路径 + 复用"是最终版）：

| 线程 | Go 实例级缓存 ON | Go 默认 | tidb-rs main | 短路径、不复用 | 短路径 + 复用 | 复用 / Go ON | 复用 / Go 默认 |
|--:|--:|--:|--:|--:|--:|--:|--:|
| 1 | 9123 | 9550 | 8372 | 8448 | 8597 | 0.94 | 0.90 |
| 8 | 32527 | 32782 | 29003 | 29064 | 29743 | 0.91 | 0.91 |

每轮之间的波动约 ±5%（Go 1 线程 8541–9424）。每条语句的分配次数（jemalloc `nrequests`，方法同 §3.1）：main 约 505，短路径不复用约 487，短路径 + 复用约 410；Go 实例级缓存 ON 约 228，默认约 200。

复用第一次接上时 1 线程 QPS 约 28000（Go 的 3 倍）：client-rust 的 snapshot 缓存了读到的值，复用的执行器拿同一个 MaxTS snapshot 再读，根本不访问 TiKV，读到的是旧值（在 playground 上用 Go TiDB 改一行即可复现）。存储驱动改为 MaxTS 的读每次新建 client-rust snapshot 后，读到新值，QPS 回到上表（D07.1 spec §3）。

## 4. `pseudoSelectivity` 的唯一键查找（D10.6 spec §5 第 1 条）

这条路径只在完整规划器的选择率估算中、条件超过 63 个或 `HistColl` 没有列和索引时走到。预处理点查走快速路径，不估算选择率；能走到它的语句（例如带 64 个条件的范围查询）现在停在计划回放的采集（§5 第 3 条），之后还要统计信息推导（D10.6）、FindBestTask（VS1-28）和 Selection / TableReader 执行器（VS1-34 / VS1-35）。所以**这一项本次没有做到**：要等 C2b 的规划器和执行器接通后，在 ab 的 workload 中加一条带 64 个以上条件的点查形状（`id = ? AND k = ? AND ...`），并用路由跟踪确认它由新栈执行。

## 5. 剩余缺口

1. **文本协议的点查**（`Point_Get` 中最多的一类 unsupported）：普通语句的 `Compile` 需要一份可写的 AST（D05.3 spec §9.2）。路由器在回退时还要用同一个 AST 降级给 PoC，所以不能把它移交给新栈；要么回退时重新解析，要么 AST 支持复制。计划随 VS1-37。（更新：已做，见 §7。语句移交给新栈，回退时重新解析。）
2. **超出列范围的参数**：Go 给 TableDual（`PhysicalTableDual`，D10.1，VS1-27）。
3. **加载全局变量之后，非点查语句停得更早**：Go 的默认 `tidb_enable_plan_replayer_capture = ON`，会话加载全局变量后就是开着的，`CollectColumnStatsUsage` 于是走到 `recordTableRuntimeStats`（已有的边界桩，标为 D02.2）。原来新会话从不加载全局变量，这个开关是 false，非点查语句停在后面的物理优化边界。现在它们停在这里（ab 的 D02.2 由 0 变为 380 条，取代原来的 D10.x / D12.x 原因）。这是 Go 的真实行为，桩要由 C2b 移植（`recordSingleTableRuntimeStats` 要统计信息句柄，D11.x）。（更新：已移植。`recordTableRuntimeStats` 照 Go 记录 pseudo 表（新栈没有统计信息句柄，与 Go 句柄为 nil 时相同），语句结束时的采集检查移到执行之前，只有存在采集任务时才报边界；新栈不加载采集任务，所以这些语句继续走到后面的边界。见 `specs/D09.13-optimizer/spec.md` §3 与 `specs/D12.0-executor/tasks.md`。）
4. **plan cache 相关的 warning**（§2 的 13 条 mismatch，以及 D05.3 spec §10.2 第 2 条）：Go 打开 plan cache 时某些语句有 `skip prepared plan-cache: ...` 或 `plan rebuild failed` 的 warning；PoC 从来没有，新栈在不命中缓存的语句上与 Go 相同，命中缓存之后的情形（A3）不同。
5. **全局变量**：新会话的全局值是编译时的默认值；集群上改过全局值、又不在路由器同步白名单里的变量，新会话看不到，也不触发第 0 步（D05.3 spec §11.1 已知限制）。等 D06.5 的 sysvar 缓存（VS1-17）。
6. **性能**：0.93× / 0.89×，分配次数约为 Go 的 2.8 倍（§3）。

## 6. PR

1. 快速路径的参数标记与二进制参数段（§1 第 1、2 条）。
2. Execute 计划走 Go 的非复用路径、`ExecStmt.IsReadOnly`（第 3、5 条）。
3. 存储驱动作为 `kv.Storage`、只读事务、PD oracle（第 4 条，D07.1 spec）。
4. 集成：服务器与会话接上存储驱动、全局变量的加载、路由器的两处修正、`SHOW WARNINGS`、选项类型、本文（第 4 条的接线与第 6–9 条）。

## 7. 文本协议的点查（普通语句接入 `Compile`，2026-11-23）

做法见 D05.3 spec §9.2：路由器把语句交给新会话、回退时重新解析；`Compile` 收下语句，规划器在它上面写，写完冻结成 `Arc`，再交给 `ExecStmt` 和事务管理器。

同一台机器、同样的命令（weekly 的 `ab` 部分：sysbench `-n 100` 加 3 个 workload；第一个 tidb-rs 用 `--new-stack=fallback`，第二个用 `strict`），main = `6ce3744`：

| | main | 本项 |
|---|--:|--:|
| strict：sysbench 文本协议点查（126 条）在新栈执行且与 Go 一致 | 0 | 121 |
| strict：新栈执行的语句（普查 `new_stack ok`） | 131 | 261 |
| strict：`Point_Get` 分类 一致 / 不一致 / 1235 | 141 / 0 / 154 | 271 / 0 / 24 |
| strict：全部 一致 / 不一致 / 1235 / 双方同样报错 | 733 / 13 / 810 / 10 | 863 / 13 / 675 / 15 |
| fallback：全部 一致 / 不一致 / 1235 / 双方同样报错 | 1495 / 21 / 32 / 18 | 1496 / 21 / 31 / 18 |

fallback 的 21 条不一致在两边是同一批语句（plan cache 的 warning，§2），没有退化。

还没有在新栈上跑的 5 条文本协议点查：3 条常量超出 `BIGINT` 范围（Go 是 TableDual，停在 `D10.1`），2 条负数常量 `id=-1` / `id=-9223372036854775808`（一元负号不走快速路径，完整规划器的计划带 Projection，停在 `D12.2`）。

strict 普查中回退的原因（全部 workload，每条语句一次）：

| 原因 | main | 本项 |
|---|--:|--:|
| `D05.3`（普通语句不能编译） | 469 | 0 |
| `D12.1` TableReader 执行器 | 11 | 158 |
| `D12.2` Projection / Selection / Limit / TableDual 执行器 | 166 | 200 |
| `D12.5` Sort / TopN 执行器 | 143 | 284 |
| `D10.1` / `D10.2` / `D10.8` 物理优化 | 5 / 3 / 1 | 9 / 6 / 3 |
| `D04.1` 当前用户 | 0 | 3 |
| `ast` / `kind`（路由器不交给新栈的形状） | 618 / 4 | 618 / 4 |

另有 5 条 sql-layer 的语句由新栈返回与 Go 相同的错误（普查 `new_stack error`，比较器记为双方同样报错）。

## 本文所用 ID

- C2a / C2b：检查点 2 的点查纵向 / 其余读路径；VS1-20：本项；VS1-27 / VS1-28 / VS1-34 / VS1-35 / VS1-37：物理算子 / FindBestTask / TableReader / Selection 与 Projection / 普通语句的集成
- A3：plan cache 例外；D02.2：解析器的 AST 与 hint（计划回放采集的桩沿用这个块号）；D03.1：服务器与结果编码；D05.3：会话；D06.3：InfoSchema；D07.1：存储驱动；D09.13：优化器驱动；D10.1：物理算子；D10.6：基数估算；D10.8：点查快速路径；D11.1 / D11.x：统计信息；D12.2：Selection / Projection 执行器；D13.1：plan cache
- T1：比较器对 1235 的容差（`unsupported`）
- D04.1：权限（当前用户）；D10.2：FindBestTask；D12.1：TableReader 执行器；D12.5：Sort / TopN 执行器
