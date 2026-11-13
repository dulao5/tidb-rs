# N9：可观测性（profile、慢日志与语句摘要、EXPLAIN ANALYZE、Top SQL、TRACE）——调研与设计草案

状态：调研草案（2026-11-13，负责人在 DM 中提出：这几个主题要在现阶段调研清楚，避免项目成型后难以实现）。2026-11-14 按 PR #114 Lucky 评审修订：CPU 记账的代价与机制（线程 CPU 时钟是系统调用）、Go 已有的语句级 `Tidb_cpu_time`、`resource_group_tag` 按请求计算、记录在语句结束后封存、TRACE 实现方式待比较、span 数量更正。类型：实现例外（基础设计）+ 若干待决定的特性问题。计划定稿：P2 之前（执行器框架 D12.0、会话主流程 D05.3、coprocessor 客户端 D07.2 开工前）。相关：N2（会话与语句上下文，含 `StmtShared` 拆分，#53）、N3+A1（运行时、CPU 记账、内存）、D14.0（`tidb-execdetails`）、D00.1（stmtctx）、D05.2（`slow_log.rs` 的规则匹配）、#101（client-rust 锁处理）、#116（D3）。

本文只做调研和接口设计，**不写生产代码**。结论分三类：Go 怎么做（兼容契约）、TiKV 怎么做（同源的 Rust 做法）、tidb-rs 的设计；最后列出"现在必须定的接口"和需要负责人决定的问题。

## 0. 总结

| 主题 | Go v8.5.8 的数据来源 | 对外契约（必须兼容） | TiKV 的做法 | tidb-rs 建议 |
|---|---|---|---|---|
| 1. CPU / heap profile | Go runtime pprof；`cpuprofile` 包常驻 1 秒一轮的 CPU profiler，HTTP 请求和 Top SQL 共享它 | status 端口 `/debug/pprof/{profile,heap,goroutine,mutex,allocs,block,trace,…}`、`/debug/zip`；pprof protobuf 格式；Dashboard 的"实例性能分析"按这些路径取数 | `pprof-rs`（信号采样）出 CPU profile；jemalloc `prof.dump` 出 heap profile；`/debug/pprof/{profile,heap,heap_activate,heap_deactivate,heap_list}` | 照 TiKV：`pprof-rs` + jemalloc profiling（A1 已定 jemalloc）；路径与 Go 一致，CPU profile 格式一致，heap profile 的格式需要验证（§4，可能要转成 pprof protobuf）；goroutine/mutex 用"任务转储"和锁等待指标替代（待定 D1） |
| 2. 慢日志、语句摘要 | `ExecStmt.LogSlowQuery` / `SummaryStmt` 在语句结束时从 `StmtCtx`、`SessionVars`、`ExecDetails`、`RuntimeStatsColl`、`CopTasksDetails` 拼字段；计划经 `plancodec` 编码 | 慢日志文本格式（约 70 个 `# Key: value` 字段，`INFORMATION_SCHEMA.SLOW_QUERY` 84 列靠解析它）；`STATEMENTS_SUMMARY(_HISTORY)` 列；`Plan`（`tidb_decode_plan` 可解）和 `Binary_plan`（`tidb_decode_binary_plan`）编码 | 无慢日志表；TiKV 的 slow log 是普通日志行，按请求的 `Tracker` 计时 | **统一的语句执行记录**（§2.3 `StmtExecRecord`）：执行中各处只写这一个结构，慢日志、语句摘要、EXPLAIN ANALYZE、Top SQL 都从它派生；输出层逐字节照 Go |
| 3. EXPLAIN / EXPLAIN ANALYZE | `planner/core` 的 `ExplainInfo`/`OperatorInfo`；`RuntimeStatsColl` 按 plan id 收集根和 cop 统计，各算子自定义 `RuntimeStats.String()`（14 种） | 结果集列与每个字段的文字（`execution info`、`operator info`、`memory`、`disk`）；`FORMAT = 'brief'/'dot'/'row'/'verbose'/'tidb_json'/'true_card_cost'/'cost_trace'`；`EXPLAIN FOR CONNECTION` | coprocessor 返回 `ExecutorExecutionSummary`（每个 cop 算子的行数、时间、迭代数） | 算子统计按 plan id 进 `StmtExecRecord` 的算子表（D14.0 的 `RuntimeStatsColl` 就是它的 Go 形态）；文字照 Go 移植；`EXPLAIN FOR CONNECTION` 读 `StmtShared` 中的同一份数据 |
| 4. Top SQL | TiDB 侧 CPU：1 秒 CPU profile + goroutine pprof 标签（`sql_digest`、`plan_digest`，以及 `sql_global_uid` 用于语句级 CPU）事后归因；执行次数、时长、网络字节：`stmtstats` 按 (sql, plan) 聚合；TiKV 侧：每个 KV 请求带 `resource_group_tag`，由事务/快照上的 tagger 按请求的第一个 key 算出（SQL/plan digest + label + table id） | ng-monitoring 通过 TiDB status 端口的 gRPC `TopSQLPubSub.Subscribe` 拉流；`tidb_enable_top_sql` 等变量；`resource_group_tag` 的编码；**语句级 CPU 字段**：慢日志 `Tidb_cpu_time`/`Tikv_cpu_time`、`STATEMENTS_SUMMARY.AVG_TIDB_CPU_TIME`、PROCESSLIST 的 CPU，只在 Top SQL 开启时有值 | `resource_metering`：future 包装器在 poll 前后挂上/取下线程标签，后台每秒读各线程 CPU 时间（`/proc/<pid>/task/<tid>/stat`）记到当前标签 | **poll 级记账代替 profile 采样**：每次 poll 只读 `Instant`（vDSO），按任务标签累计墙钟时间；每个 worker 线程每 1–10ms 读一次线程 CPU 时钟，把这段 CPU 按墙钟比例分给这段时间里运行过的标签（§6，需要基准，见 D3）；tagger 作为请求上下文随事务/快照传递（同 Go）；gRPC 照 Go |
| 5. TRACE | opentracing + basictracer，26 个文件 49 处手工开 span（`StartSpanFromContext`、`tracing.StartRegion`、`opentracing.StartSpan`），加上 client-go 的 24 处（`SendReqCtx`、`prewriteMutations`、`tikvSnapshot.get` 等）；`FORMAT='row'/'json'/'log'`；`TRACE PLAN`（优化器追踪，写文件） | 结果集：`row` 是三列树（operation、startTS、duration），`json` 是 span 树，`log` 是带时间戳的事件行；契约是相同事件的 span 名称和层次（时间、重试次数本来就不可重复） | 早期 minitrace（已从主线移除），现在靠 `Tracker` 的阶段时间戳 | 两种实现待比较（§7）：`tracing` crate 的 span，或 `ObsCtx` 中的小型记录器（`if let Some(t) = obs.trace()`）；span 位置照 Go 的 49 处 + client-rust fork 中对应 client-go 的 24 处 |

**核心建议**：Go 的问题不是"记录得多"，而是同一件事在几处各记一遍（`ExecDetails` 合并、`RuntimeStatsColl`、`stmtstats`、`ppcpuusage`、opentracing span、pprof 标签各自独立），字段散在 `StmtCtx`、`SessionVars`、执行器局部变量里，靠语句结束时一个 400 行的函数（`LogSlowQuery` + `SummaryStmt` + `getEncodedPlan`）拼起来。基础的算子时间和行数 Go 已经在 `exec.Next` 中统一记录（`executor/internal/exec/executor.go:443`），分散的是其余部分。tidb-rs 用**一个语句级执行记录 + 一个任务级观测上下文**承载这些事实，五个输出都是它的"视图"。下面 §2 是这个模型，§3–§7 逐个主题说明怎么从它得到 Go 的输出。

## 1. 调研：Go v8.5.8

### 1.1 分散程度（实测）

| 机制 | 规模 | 位置 |
|---|---|---|
| opentracing span | 26 个文件、49 处（`StartSpanFromContext`、`tracing.StartRegion`、`opentracing.StartSpan`、`Tracer().StartSpan`）；client-go 另有 24 处，也出现在 TRACE 输出中 | `executor`、`session`、`store/copr`、`distsql`、`planner` 等；client-go `internal/locate`、`txnkv/transaction` 等 |
| 基础算子统计 | `exec.Next` 统一记录每个执行器的时间和行数（`executor/internal/exec/executor.go:443–445`） | 执行器框架 |
| `RuntimeStatsColl.RegisterStats` | 23 个文件、27 处；14 种自定义 `RuntimeStats.String()`（算子特有统计） | 各执行器 |
| pprof 标签（Top SQL 归因） | 14 处 `SetGoroutineLabels` / `pprof.Do` | `server/conn.go`、`session/session.go`、`executor/select.go`、`util/topsql` |
| 慢日志字段 | `slow_log.go` 约 70 个字段名常量；`SLOW_QUERY` 84 列 | `sessionctx/variable/slow_log.go`、`infoschema/tables.go` |
| 语句摘要记录 | `stmtsummary/v2/record.go` 约 105 个字段 | `util/stmtsummary` |
| `ExecDetails` 合并 | `MergeExecDetails`、`CopTasksDetails`、`TimeDetail`、`ScanDetail` 在 executor、copr、txn 中多处累加 | `util/execdetails`（D14.0 已移植）、client-go `util/execdetails.go` |

### 1.2 主题 1：profile

- `server/http_status.go` L311–316：`/debug/pprof/cmdline`、`/debug/pprof/profile`（→ `cpuprofile.ProfileHTTPHandler`）、`/debug/pprof/symbol`、`/debug/pprof/trace`，其余 `/debug/pprof/*`（heap、goroutine、mutex、allocs、block、threadcreate）交给 `pprof.Index`；`/debug/zip`（L352）一次打包 profile、goroutine、heap、mutex、配置、版本；`/debug/gogc`。
- `util/cpuprofile`：常驻的"并行 CPU profiler"，每 `DefProfileDuration = 1s` 一轮，多个消费者（HTTP 请求、Top SQL）共享同一份 profile，避免 Go 一次只能有一个 CPU profile 的限制。
- Dashboard 的"实例性能分析"和 Clinic 按这些路径抓取，期望 pprof protobuf（CPU、heap）和文本（goroutine `?debug=1/2`）。

### 1.3 主题 2：慢日志与语句摘要

- 写入时机：`ExecStmt.FinishExecuteStmt` → `LogSlowQuery`（`executor/adapter.go` L1770）、`SummaryStmt`（L1996）。字段从 `StmtCtx`（`GetExecDetails`、`CopTasksDetails`、`MemTracker.MaxConsumed`、`DiskTracker`、`Tables`、`IndexNames`、warnings）、`SessionVars`（`DurationParse`、`DurationCompile`、`RewritePhaseInfo`、`DurationWaitTS`、`TxnCtx`）、`RuntimeStatsColl`（`GetRootStats`、`GetCopStats`）拼出。
- 计划：`getEncodedPlan`（L1964）用 `plannercore.EncodeFlatPlan` 得到 `Plan` 字段（`plancodec` 压缩 + base64，`tidb_decode_plan` 解码）；`getBinaryPlan`（L1926）得到 `Binary_plan`（tipb `ExplainData` protobuf → snappy → base64，`tidb_decode_binary_plan` 解码）；`NormalizeFlatPlan` 得到 plan digest。这些编码依赖 `FlattenPhysicalPlan` 的扁平树（`flat_plan.go`）。
- 慢日志是**文本契约**：`SLOW_QUERY` 表、Dashboard 慢查询页、`pt-query-digest` 一类工具都解析它；字段顺序和格式要逐字节一致（D05.2 已移植规则匹配部分 `MatchEqual`/`matchGE`/`CollectWarningsForSlowLog`，`SlowLogFormat` 等 5 个函数归 D14.2）。
- 语句摘要：`stmtsummary` v1（内存 map + 历史窗口）和 v2（持久化到文件）；记录按 (schema, digest, plan digest, resource group) 聚合，`STATEMENTS_SUMMARY`、`_HISTORY`、`_EVICTED`、`CLUSTER_*` 表读它。
- **语句级 CPU（`util/ppcpuusage`）**：慢日志 `Tidb_cpu_time`/`Tikv_cpu_time`（`slow_log.go:139–142`，只在 > 0 时写出）、`STATEMENTS_SUMMARY.AVG_TIDB_CPU_TIME`（`infoschema/tables.go:1387`）、PROCESSLIST 的 TiDB/TiKV CPU（`processinfo.go:154`）。TiDB 部分来自 Top SQL 的 CPU profile：`server/conn.go:1341` **只在 Top SQL 开启时**分配 `sql_global_uid` 标签（`SQLCPUUsages.AllocNewSQLID`），`collector/cpu.go` 把采样到的 CPU 按 (conn, sqlID) 合并进去（`MergeTidbCPUTime`），sqlID 用来丢弃上一条语句迟到的样本。所以 Top SQL 关闭时没有 `Tidb_cpu_time`，列为 0（`Tikv_cpu_time` 不受影响：它是 TiKV 返回的 ProcessTime，无条件累加，`select_result.go:665`、`point_get.go:266`）；开启时短语句会少算（1 秒一轮的 profile 晚于慢日志写出）。

### 1.4 主题 3：EXPLAIN / EXPLAIN ANALYZE

- 计划侧：每个物理算子实现 `ExplainInfo`/`OperatorInfo`/`ExplainNormalizedInfo`，`Explain.RenderResult` 按 `FORMAT` 组织行；`tidb_json`、`dot`、`verbose`（带代价）、`true_card_cost`、`cost_trace` 等格式。
- 执行侧：`RuntimeStatsColl`（D14.0）按 plan id 存 `RootRuntimeStats`（根算子的 loops、rows、time、自定义统计）和 `CopRuntimeStats`（cop 任务的汇总，来自 TiKV 回传的 `ExecutorExecutionSummary` 加 `ScanDetail`、`TimeDetail`）；算子自己的统计（hash join build/probe、IndexLookUp 的回表、并发 worker 等）实现 `RuntimeStats` 接口，通过 `RegisterStats` 合并。`memory`、`disk` 列来自 `memory.Tracker` 树（A1）。
- `EXPLAIN FOR CONNECTION`：读另一个会话 `ProcessInfo` 中的计划和 `RuntimeStatsColl`（N2 中 `StmtShared` 要包含它）。

### 1.5 主题 4：Top SQL

- TiDB 侧 CPU：`topsql.AttachSQLInfo`/`AttachSQLAndPlanInfo` 在会话主 goroutine 上 `pprof.SetGoroutineLabels(sql_digest, plan_digest)`；`collector/cpu.go` 消费 1 秒 CPU profile，按标签汇总样本得到每个 (sql, plan) 的 CPU 毫秒。goroutine 会继承创建者的 pprof 标签，所以语句自己起的 worker 自动带标签；漏算的是**预先存在的池化 goroutine**（处理多条语句的 worker 池）。
- 其他指标：`stmtstats.StatementStats` 按 (sql, plan) 记 `ExecCount`、`SumDurationNs`、`DurationCount`、`NetworkInBytes`/`NetworkOutBytes`，以及按 TiKV 地址的 `KvExecCount`。
- TiKV 侧：语句构造一个 tagger（`StmtCtx.GetResourceGroupTagger`，`stmtctx.go:868`：SQL + plan digest），作为快照/事务的选项设置（`executor/select.go:1312`、`sessiontxn/isolation/base.go:490/552`、`update.go:431`；DDL backfill 设置自己的 tagger，`backfilling.go:1199`）。每个请求的 tag 由请求的**第一个 key** 算出 `TableId` 和 row/index `Label`（`kv/kv.go:872–899`），编码为 tipb `ResourceGroupTag`。client-go 把 tagger 拷进 committer（`2pc.go:762`），secondary 和 async commit 在语句返回后的后台 goroutine 中仍用这个 tag；COMMIT 用最后一条语句的 tagger。TiKV 按 tag 统计 CPU、读写 key 数，ng-monitoring 直接从 TiKV 拉。
- 上报：现在的方向是 ng-monitoring **订阅** TiDB 的 gRPC 服务 `TopSQLPubSub.Subscribe`（`reporter/pubsub.go`），TiDB 每分钟（`tidb_top_sql_report_interval_seconds`）推送记录和 SQL/plan meta（规范化 SQL 文本、规范化计划）；旧的 `TopSQLAgent.ReportTopSQLRecords` 客户端模式（`single_target.go`）仍保留。
- 开关：`tidb_enable_top_sql`、`tidb_top_sql_max_time_series_count`、`tidb_top_sql_max_meta_count`。

### 1.6 主题 5：TRACE

- `executor/trace.go`：`FORMAT='row'`（默认，三列树：`operation`、`startTS`、`duration`，用 appdash 收集后 `dfsTree` 画树）、`'json'`（span 树 JSON）、`'log'`（basictracer 的 `RawSpan` 按时间输出事件行）；`TRACE PLAN [TARGET='estimation'|'debug']` 执行优化器追踪并把结果打包成文件，返回文件名（另有 `/optimize_trace/dump/` HTTP 下载）。
- span 由 `pkg/` 中 49 处手工开的 span（加 client-go 的 24 处）产生，只在 context 中有 tracer（TRACE 语句或 `tidb_general_log` 相关调试）时生效。

## 2. 调研：TiKV（同源 Rust 做法）

| 项 | 做法 | 位置 |
|---|---|---|
| CPU profile | `pprof::ProfilerGuardBuilder`（SIGPROF 采样，可选 frame-pointer）；`/debug/pprof/profile?seconds=N&frequency=…`，输出 protobuf 或火焰图 | `src/server/status_server/profile.rs`、`mod.rs` L797 |
| heap profile | jemalloc `prof.active` / `prof.dump`（`tikv_alloc::jemalloc::activate_prof/dump_prof`）；`/debug/pprof/heap_activate`、`heap_deactivate`、`heap_list`、`heap`；需要编译时开 jemalloc profiling | `components/tikv_alloc/src/jemalloc.rs` L416–545 |
| Top SQL（resource metering） | `ResourceMeteringTag::attach()` 把标签写进线程局部存储；future 用 `InTags` 包一层，每次 poll 前后挂上/取下；后台每秒遍历已注册线程，读 `/proc/<pid>/task/<tid>/stat` 的 CPU 时间差，记到该线程当前标签上；读写字节另有 summary recorder；按 `resource_group_tag` 上报 | `components/resource_metering/src/{lib.rs,recorder/sub_recorder/cpu.rs}` |
| 请求级计时 | `tracker::Tracker`：每个请求一个 slab 槽，各阶段写时间戳和扫描统计，结束时写进回包的 `TimeDetailV2`/`ScanDetailV2`；`FutureTrack` 的 `on_poll_begin/finish` 记 poll 时间 | `components/tracker/src/{lib.rs,future.rs}` |
| 调度与 CPU | yatp 多级反馈队列按任务累计时间降级；coprocessor 每 32 个 key 看一次时钟，超时间片 `reschedule().await` | `src/read_pool.rs`、`components/tidb_query_common/src/storage/scanner.rs` |
| 全链路追踪 | 曾用 minitrace，已从主线移除；现在 `tracing` 只作日志后端 | `Cargo.toml` |

**结论**：TiKV 的 Top SQL 归因是"线程当前标签 × 线程 CPU 时间差"，每秒抽样一次。tidb-rs 的 N3 poll 包装器已经知道每次 poll 属于哪个任务，可以做得更细，但**线程 CPU 时钟在 Linux 上是系统调用**（`CLOCK_THREAD_CPUTIME_ID` 不走 vDSO，Lucky 在 Linux arm64 上实测约 200ns，`CLOCK_MONOTONIC` 约 13ns；`tidb-runtime` 的 `stall.rs` 也因约 400ns/poll 而默认关闭），不能每次 poll 读两次。§6 的做法是每次 poll 只读 `Instant`，线程 CPU 时钟按时间窗口读、按墙钟比例分摊。

## 3. tidb-rs 设计：统一的观测模型

### 3.1 原则

1. **一个事实只记一次**：执行中产生的每个数字（行数、时间、CPU、扫描 key 数、内存峰值、RPC 次数、等待时间）只写入一个地方；五个输出从同一份数据派生。
2. **输出层逐字节照 Go**：慢日志格式、`SLOW_QUERY`/`STATEMENTS_SUMMARY` 的列、EXPLAIN 的文字、计划编码、`resource_group_tag`、Top SQL 的 gRPC 消息、TRACE 的三种格式都是兼容契约，按 L1 移植（各自归 D14.x、D12.12 等块）。内部收集方式是实现例外。
3. **默认路径几乎零开销**：常开的只有计数器和 poll 级记账（N3 的 poll 包装器本来就读 `Instant`，不增加时钟读取；线程 CPU 时钟按窗口读，§6.1）；span、算子级详细统计只在需要时开（EXPLAIN ANALYZE、TRACE、慢日志阈值、采样）。
4. **按 N3 的任务模型挂载**：语句 = 连接 task（加上它 `pool.spawn` 的 worker task），任务继承观测上下文；不依赖 Go 那样的 context 透传约定。

### 3.2 三个对象

| 对象 | 生命周期 | 内容 | 谁写 | 谁读 |
|---|---|---|---|---|
| **`ObsCtx`**（任务级观测上下文） | 随 task 继承，挂在 N3 的 task 局部存储里（扩展现有的 `CpuRecorder`） | 当前语句记录的**弱引用或封存标志**（§3.5）；CPU 归因用的 Top SQL 键 (sql digest, plan digest)；任务内累计的墙钟和分摊到的 CPU（定期刷出，§3.6）；TRACE 记录器或当前 span（默认无）。**不含** `resource_group_tag` 的 label/table id：它们按请求计算，随事务/快照传递（§6） | 运行时（poll 包装器）、会话（语句开始/结束时替换） | poll 包装器 |
| **`StmtExecRecord`**（语句执行记录） | 一条语句一份，放在 N2 的 `StmtShared` 中（`Arc`，内部可变，#53 的拆分）；语句输出产生后**封存**（§3.5） | 阶段时间（parse、compile、optimize、wait TS、执行、Q1/Q2 等待）；TiDB CPU（`Tidb_cpu_time`，§6）与 TiKV CPU（`Tikv_cpu_time`，来自 TiKV 回包）；`ExecDetails`（D14.0）；`RuntimeStatsColl`（按 plan id 的算子统计，D14.0 已移植）；内存/磁盘峰值（A1 tracker）；KV RPC 统计；warnings 计数；plan 句柄（N1 冻结的物理计划） | 执行器、copr 客户端、运行时 | 语句结束时的输出层；`EXPLAIN FOR CONNECTION`、`PROCESSLIST`（读进行中的） |
| **`ObsSinks`**（进程级） | 进程 | 慢日志写出器、语句摘要（v1/v2）、Top SQL 聚合器、TRACE 收集器、Prometheus 指标 | 语句结束时调用 | 系统表、HTTP、gRPC |

`StmtExecRecord` 的字段就是 Go 的 `ExecDetails` + `RuntimeStatsColl` + `SessionVars.Duration*` + `StmtCtx` 中执行期字段的并集，**不是新发明**；不同的只是放在一个地方、由运行时和执行器写，而不是语句结束时到处收集。

### 3.3 写入点（取代 Go 的散落记录）

| 事实 | Go 的记录位置 | tidb-rs 的写入点 |
|---|---|---|
| CPU 时间 | Top SQL：pprof 标签 + 采样；语句级 `Tidb_cpu_time`（`ppcpuusage`）也来自这次采样，只在 Top SQL 开启时有 | poll 包装器 + 线程级时间窗口分摊（§6）：任务内累计，定期刷到 Top SQL 聚合器和语句记录（**一处**） |
| 算子行数、循环次数、时间 | `exec.Next` 统一记录（`executor/internal/exec/executor.go:443–445`）；算子特有统计由各执行器 `RegisterStats` | 照 Go：执行器框架（D12.0）在 `next()` 外层统一记；算子特有的统计（hash join 的 build/probe 等）仍由算子实现 `RuntimeStats` 写入同一个 `RuntimeStatsColl` |
| cop 任务统计 | `copr` 中 `RecordOneCopTask`、`MergeExecDetails`、`CopTasksDetails` | copr 客户端（D07.2）收到响应时写一次 `RuntimeStatsColl` 和 `ExecDetails` |
| KV RPC、backoff | client-go `ExecDetails`（`util.ExecDetails`）经 context 回传 | client-rust 请求完成回调写 `ObsCtx` 的记录（需要 client-rust 加钩子，与 N3 的 Q2 钩子同一处） |
| 阶段时间 | `SessionVars.DurationParse/Compile/…` | 会话主流程（D05.3）写记录 |
| span | 49 处 `StartSpanFromContext` 等，加 client-go 24 处 | 同样的位置；实现方式见 §7 |
| 内存峰值 | `MemTracker.MaxConsumed` | A1 tracker 根节点，结束时读一次 |

### 3.4 派生输出

| 输出 | 怎么从记录得到 | 归属块 |
|---|---|---|
| EXPLAIN ANALYZE | 计划行（N1 冻结计划 + `ExplainInfo` 文字）× `RuntimeStatsColl.get(plan_id)` 的 `String()` | D10.9（EXPLAIN 输出、计划编码）、D12.12（EXPLAIN 语句）；D14.0 已有 `String()` |
| 慢日志 | 语句结束、超过阈值或匹配 `tidb_slow_log_rules`（D05.2 已移植规则）时，按 Go 的字段顺序格式化记录 | D14.2（`SlowLogFormat` 等） |
| 语句摘要 | 记录按 key 聚合进 `stmtsummary` | D14.2 |
| Top SQL | CPU：任务内累计、按窗口分摊、定期刷到进程级聚合器（不在每次 poll 写共享结构，§3.6）；执行次数、时长、网络字节：语句结束时从记录取；每分钟推给订阅者 | D14.3（拆出，D4 已同意）、D14.2（上报） |
| TRACE | span 树（§7 选定的记录方式）→ `row`/`json`/`log` 三种格式 | D14.2（trace）、D12.12（TRACE 语句） |
| EXPLAIN FOR CONNECTION / PROCESSLIST | 读另一个会话 `StmtShared` 中的记录（进行中的统计） | D12.12 / D05.3 |
| 指标 | 记录结束时更新直方图；运行时、队列指标已在 N3 | 各块 |

### 3.5 生命周期：语句结束后仍在运行的任务

语句起的任务可能比语句活得久：后台 secondary commit / async commit（Go `2pc.go:2001`，`CommitTxn` 返回后仍写 `CommitDetails`，D14.0 评审已记）、预取、copr 迭代器在 drop 之后的收尾。现在定：

1. **记录在输出产生时封存**：慢日志、语句摘要、Top SQL 的执行次数等写出之后，`StmtExecRecord` 封存，之后的写入被丢弃（计数器指标）。
2. **封存后的 CPU 仍记到 Top SQL 键**：Go 的 goroutine 标签会一直留在后台 goroutine 上，CPU 仍算给那条 SQL；tidb-rs 同样把这部分 CPU 记到 `ObsCtx` 里的 (sql, plan) 键，但不进封存的记录。
3. **不能因为长期任务让旧记录一直存活**：`ObsCtx` 对记录只持有弱引用（`Weak`）或"语句代号 + 封存标志"，不持有强引用；没有 `Arc` 环。
4. PROCESSLIST 的 CPU 列按 Go 的 sqlID 语义：每条命令分配一个 sqlID，迟到的、属于上一条语句的 CPU 被丢弃（Go `MergeTidbCPUTime`）。

### 3.6 避免热路径上的争用

每次 poll 都对共享的 `StmtExecRecord` 做原子加、对 Top SQL 的 `DashMap` 加分片锁，是最热路径上的跨核写。改为（同 TiKV 的 recorder）：

- 任务（或线程）局部累计：墙钟、分摊到的 CPU、poll 次数存在 task 局部存储里；
- 在任务结束、或每个时间窗口（与 §6 的线程 CPU 窗口同一个时机）刷一次到语句记录和 Top SQL 聚合器；
- **语句结束时先刷出**（Lucky 评审 #114）：连接任务在语句结束时并不结束，所以在语句产生输出和封存（§3.5）**之前**，语句自己的任务要先刷出累计值，并结束当前的 CPU 窗口（还没读到的部分按墙钟分摊）。否则每条短语句最后一个窗口的 CPU 都会在封存之后才到、只进 Top SQL，像 Go 一样少算；
- 读进行中的统计（PROCESSLIST、`EXPLAIN FOR CONNECTION`）看到的是上一次刷出的值，误差不超过一个窗口（Go 的 1 秒 profile 误差更大）。

## 4. 主题 1：profile（细节）

- **CPU**：`pprof-rs`（TiKV 同版本系列；D2 已定 frame pointer，**待验证**：目标平台的预编译 `std` 是否带 frame pointer，否则经过 `std` 的栈会被截断，除非自己编译 std），`/debug/pprof/profile?seconds=N` 输出 pprof protobuf（与 Go 同格式，Dashboard 可直接用）；需要 frame pointer 或 DWARF 展开（TiKV 用 `pprof-fp` feature + `-C force-frame-pointers`），这是**构建配置**，要现在定（D2 已定：frame pointer）。
- **heap**：jemalloc profiling（A1 已定 jemalloc）。需要编译时开 `profiling` feature、运行时 `prof:true,prof_active:false`，按 TiKV 的 `heap_activate/heap_deactivate/heap_list` 加上 Go 的 `/debug/pprof/heap`。**待验证（不阻塞）**：tidb-rs 节点注册为 TiDB 实例，Dashboard / ng-monitoring 取 `/debug/pprof/heap` 时期望 Go 的 pprof protobuf；jemalloc `prof.dump` 是 jeprof 格式（TiKV 的 heap 在 Dashboard 中走 TiKV 专用的路径）。需要先查 Dashboard 的抓取代码，可能要在进程内把 jemalloc 的 profile 转成 pprof protobuf。
- **goroutine / mutex 的对应物**（D1，**负责人决定：返回 404 / not supported**，#115，2026-11-13）：`/debug/pprof/goroutine`、`/debug/pprof/mutex` 返回 404（not supported），写进 `docs/compatibility.md`。下面的任务转储和锁等待统计仍可作为 tidb-rs 自己的诊断手段（例如另设路径或 `/debug/zip` 中的文件），不占用 Go 的这两个路径：
  - goroutine → **任务转储**：tokio 的 `Handle::dump()`（需要 `tokio_unstable` + `taskdump`，只在 Linux x86_64/aarch64），或 N3 运行时自己维护"活跃任务表"（task id、所属连接、当前语句 digest、已运行 CPU、最近一次 poll 开始时间）。建议后者：开销可控、跨平台，且对 PROCESSLIST 也有用。按 D1 的决定，`/debug/pprof/goroutine` 返回 404；这个表作为 tidb-rs 自己的诊断，放在别的路径或 `/debug/zip` 中。
  - **待验证**：ng-monitoring 持续性能分析对 goroutine/mutex 请求的是哪种 `debug=` 变体，确认 ng-monitoring 持续性能分析遇到 goroutine/mutex 的 404 时，是只跳过这两项，还是让这个节点的持续性能分析整体失败（D1 已定返回 404）。
  - mutex/block → 锁等待：N3 已有停顿检测（`on_cpu`/`blocked`/`preempted` 分类）；`parking_lot` 锁等待时间可以用 `lock_api` 包装统计。`/debug/pprof/mutex` 无法给出 Go 格式。
- `/debug/zip`：打包 CPU profile、heap、任务转储、配置、版本，路径保留。
- **持续 profile**（Dashboard "持续性能分析"由 ng-monitoring 定期拉 `/debug/pprof/*`）：只要上面的路径和格式兼容即可，不需要 tidb-rs 内部常驻 profiler。Go 的常驻 1 秒 profiler 是为 Top SQL 服务的；tidb-rs 的 Top SQL 不用采样（§6），所以**不需要常驻 profiler**。

## 5. 主题 2、3：慢日志、语句摘要、EXPLAIN（细节）

- **计划编码依赖扁平计划**：`Plan`/`Binary_plan`/plan digest 都基于 `FlattenPhysicalPlan`，而 N1 规定计划在 `DoOptimize` 结束时冻结。所以扁平化和编码只读冻结计划，可以放到语句结束后（甚至后台）做；但 **plan digest 必须在执行开始前算出**——Top SQL 标签和 `resource_group_tag` 要用（Go 在 `ExecStmt.Exec` 前 `AttachSQLAndPlanInfo`）。这一点碰 N1 和 D05.3 的顺序，要现在定。
- **算子统计的写入点集中在执行器框架**：Go 已经这样做——`exec.Next`（`executor/internal/exec/executor.go:443–445`）对每个执行器统一记时间和行数，分散的只是 14 种算子特有的 `RuntimeStats`。D12.0 照此移植：`next()` 由框架包一层（计时、行数、loops、checkpoint），算子特有统计通过 `RuntimeStats` trait 注册。
- **进行中可读**：`StmtExecRecord` 内部可变（原子计数 + 少量 `Mutex`），`EXPLAIN FOR CONNECTION` 和 `PROCESSLIST` 读的是同一份。这要求 N2 的 `StmtShared` 包含 `RuntimeStatsColl` 和 `ExecDetails`（#53 已列 `ExecDetails`，要补 `RuntimeStatsColl` 和 plan 句柄）。Go 在语句之间复用 `RuntimeStatsColl`；在 Rust 中它在 `Arc` 后面，可能被另一个会话持有（`EXPLAIN FOR CONNECTION`），所以只有唯一持有时才复用，否则新建。
- **兼容测试**：慢日志行、`SLOW_QUERY` 解析、计划编码/解码、EXPLAIN 文字都能用 govec 对照（输入是构造好的记录/计划）；SQL 层用混合集群对照 `SLOW_QUERY`、`STATEMENTS_SUMMARY` 和 `tidb_decode_plan` 的结果（Go 解码 tidb-rs 写的计划）。

## 6. 主题 4：Top SQL 与语句级 CPU（细节）

### 6.1 TiDB 侧 CPU 的记账机制（需要基准，见 D3）

前提更正（Lucky 评审 #114）：`clock_gettime(CLOCK_THREAD_CPUTIME_ID)` 在 Linux 上不走 vDSO，是真正的系统调用（Linux arm64 实测约 200ns；x86 打开漏洞缓解或在虚拟机中通常更慢），`CLOCK_MONOTONIC` 约 13ns。点查每条语句 3–10 次 poll，每次 poll 读两次线程 CPU 时钟就是每条语句 1.2–4µs，超出 §9 的 1% 预算。

做法：

1. **每次 poll 只读 `Instant`**（vDSO，两次约 30ns），把这次 poll 的墙钟时间加到任务局部的累计（§3.6）。N3 的 poll 包装器本来就读 `Instant`，不增加时钟读取。
2. **每个 worker 线程按时间窗口读线程 CPU 时钟**：在 poll 包装器里摊销，距离上次读取超过窗口（1–10ms，可配置）才读一次 `CLOCK_THREAD_CPUTIME_ID`。窗口内这个线程运行过的各个任务（各自的 Top SQL 键）按墙钟时间比例分摊这段 CPU。
3. **精度**：线程没有被抢占时，墙钟 = CPU，分摊是精确的；被抢占时，被抢占的时间按比例分给窗口内的任务，而不是像现在这样全算给正在运行的那个任务——这同时解决了 N3 §3.1 第 5 条"CPU 超额订阅时把被抢占的时间也算进去"的问题。窗口越短越准，代价是更频繁的系统调用：10ms 窗口时每秒每线程 100 次，约 20µs/s，可以忽略。
4. **备选**：`perf_event_open` 的 task-clock 加 mmap 用户页，在用户态读取线程 CPU 时间，单次读约几十纳秒，但实现和权限（容器里的 `perf_event_paranoid`）更复杂。只有基准显示窗口方案不够准时才考虑。
5. **macOS**：线程 CPU 时钟是 `thread_info` 系统调用（约 1µs），按窗口读同样可以接受。

**基准（D3 决定之前必须做）**：sysbench oltp_read_only 与 oltp_point_select，比较 (a) 关闭 CPU 记账、(b) 每次 poll 读 `Instant`、(c) (b) + 1ms / 10ms 窗口读线程 CPU 时钟；指标为 QPS、CPU 利用率，以及在 CPU 超额订阅（客户端线程数 > 核数）下按语句分摊的 CPU 与 `perf stat` 总 CPU 的误差。

### 6.1a 动态开关与采样率（负责人决定，#116，2026-11-13）

负责人决定采用 §6.1 的统一方案（前提仍是 §6.1 的基准满足 §9），并要求：**可以动态开启和关闭**，并支持用户设置**采样率**——1% 表示每 100 次语句执行采样 1 次，100% 表示全部采样，0 表示关闭。已有的 `tidb_enable_top_sql` 作为负债暂时兼容。这是**功能上的例外**（新增系统变量），按宪章登记在 `specs/exceptions/`，并写进 `docs/compatibility.md`。

设计（细节由 Maya、Lucky 在 D14.3 spec 中定稿）：

- **新系统变量**（名字待定，暂称 `tidb_top_sql_sample_rate`）：GLOBAL，可动态修改，取值 0–100（百分比，允许小数）或 0.0–1.0，在 D14.3 中定一种；Go 中没有这个变量。
- **与 `tidb_enable_top_sql` 的关系**（Lucky 评审 #118）：**实际采样率 = `tidb_enable_top_sql` ? 采样率 : 0**，采样率默认 100%。原因：系统变量总有值，"新变量没设置"需要隐藏状态；更重要的是 `tidb_enable_top_sql` 是 Dashboard / ng-monitoring 的开关，并同步到 PD 全局配置 `enable_resource_metering`（TiKV 也跟着它）。这样默认行为和 Go 完全一样，Dashboard 上关闭 Top SQL 永远生效，新变量只负责往下调。
- **采样的粒度是一次语句执行**：在语句开始时（plan digest 附上之前）决定是否采样，决定结果记在 `ObsCtx` 中，派生的任务继承。没采中的执行完全跳过 CPU 窗口分摊、Top SQL 打标和累计（§3.6），开销和采样率成正比；`resource_group_tag` 仍按 Go 照常计算（它也用于资源管控，与采样无关，§6.4）。
- **用户可见的影响**：`Tidb_cpu_time`、PROCESSLIST 的 TiDB CPU 列只在采中的执行上出现（`Tikv_cpu_time` 和 TiKV CPU 列与 Top SQL 无关，照 Go 无条件出现）；语句摘要的 `AVG_TIDB_CPU_TIME` 等按采中的执行求平均。Top SQL 上报的是采中执行的原始值还是按采样率外推，结合 ng-monitoring 的格式在 D14.3 中决定。
- **动态修改**：修改采样率只影响之后开始的语句；正在执行的语句保持开始时的决定。
- **留给 D14.3**（Lucky 评审 #118，不阻塞）：随机采样（每次执行按概率抽），不用"每 100 次取 1 次"，以免和周期性负载对齐；§6.1 的基准加上 0%/1%/100% 三档（`Instant` 本来就要读、CPU 窗口按线程读，不采样能省下的可能很少）；如果 100% 的开销已在预算内，可以请负责人考虑"Top SQL 开启时慢日志的 `Tidb_cpu_time` 总是计算，采样率只用于 Top SQL 聚合"，否则 1% 时几乎所有慢日志都没有 TiDB CPU；上报倾向按采样率外推，因为 Dashboard 把 TiDB 和全量统计的 TiKV CPU 并排显示。

### 6.1b 基准结果（2026-11-13，#53 / §6.1 / §6.1a / §9）

**结论**：§6.1 的窗口方案满足 §9 的预算，可以作为 D3 的依据。10 ms 窗口、100% 采样时，sysbench 中 tidb-rs 每条查询的 CPU 与关闭记账相比多 +0.1%～+1.8%，与当前只读 `Instant` 的做法相比差 −0.3%～+1.3%，都在噪声内（同一配置三轮之间相差 1–2.6%）。0% / 1% 采样省下的开销小于噪声。每次 poll 读两次线程 CPU 时钟的做法确实不可行：Linux 上每次 poll 多 1.3 µs。建议窗口取 10 ms：1 ms 窗口的分摊稍准，但在 32 线程时开销 +1.6%～+1.8%，已接近预算。

**复现**：微基准 `cargo bench -p tidb-runtime --bench cpu_accounting`（`crates/runtime/benches/cpu_accounting.rs`，直接输出下面的表）。sysbench 用分支 `bench/N9-sysbench-switch`（8711ac3，不合并）：poll 包装器读环境变量 `TIDB_RS_CPU_ACCT=off|instant|win1|win10` 和 `TIDB_RS_CPU_SAMPLE`，语句开始时按采样率挂上或摘掉 `CpuRecorder`；驱动脚本 `scripts/n9-sysbench.sh`。

#### 时钟读取（ns / 次）

| 时钟 | Linux arm64（10 核虚拟机） | macOS arm64（Mac mini，12 核） |
|---|---|---|
| `Instant::now` | 13.7 | 25.3 |
| `CLOCK_MONOTONIC` | 14.9 | 18.8 |
| `CLOCK_THREAD_CPUTIME_ID` | 201.1 | 78.6 |

Linux 的线程 CPU 时钟是真正的系统调用（与 §6.1 的前提一致）；macOS 上它比 Linux 便宜，而 `Instant` 更贵。

#### 微基准：每次 poll 的开销

人造负载：每个 worker 线程 8 个任务，每个任务 2000 次 poll，每次 poll 做 1 µs 或 4 µs 的计算后让出（比点查的 poll 短，开销的百分比因此偏高）。表中是进程 CPU 比 `off`（不记账）多出的 ns / poll，各配置交替跑 15 轮取中位数。`instant` = 只读 `Instant`；`winN` = `instant` 加 N ms 窗口读线程 CPU 时钟并按墙钟分摊；`perpoll` = 每次 poll 读两次线程 CPU 时钟（参照）。

| 模式 | 采样率 | Linux 单 worker | Linux 每核一个 worker | macOS 单 worker | macOS 每核一个 worker |
|---|---|---|---|---|---|
| 噪声（`off` 的四分位距） | | ±8.1 | ±28.2 | ±42.8 | ±153.9 |
| instant | 0% / 1% / 100% | −22 / −23 / −1 | 0 / +8 / +48 | +3 / +4 / +40 | −41 / +3 / −4 |
| win1ms | 0% / 1% / 100% | +1 / −2 / +40 | +48 / +52 / +150 | +54 / +50 / +58 | +54 / +29 / +36 |
| win10ms | 0% / 1% / 100% | −3 / −1 / +6 | +62 / +42 / +114 | +50 / +52 / +54 | −18 / +48 / +161 |
| perpoll | 0% / 1% / 100% | −3 / −2 / **+1302** | +53 / +60 / **+983** | +51 / +50 / **+124** | −50 / −81 / **+311** |

说明：macOS 上窗口方案在 0% 采样时也多约 50 ns，原因不是窗口检查，而是基准中窗口模式每次 poll 读两次 `Instant`（`instant` 模式没采中时只读一次）；`perpoll` 0% 同样只多读一次 `Instant`，也是 +50 ns。真实的 N3 poll 包装器本来就每次 poll 读两次 `Instant`（时间片、停顿检测），所以这不是窗口方案的额外开销；窗口方案真正增加的是一次线程局部累加，以及每个窗口一次线程 CPU 时钟读取。

#### 微基准：分摊的准确度（100% 采样）

一半任务每次 poll 做 1 份计算，一半做 4 份。heavy/light = 分到每个重任务的 CPU 与每个轻任务之比；attributed/process = 分摊给所有任务的 CPU 与进程 CPU 之比。超额订阅 = 另起与核数相同的空转线程。

| 负载 | 模式 | Linux heavy/light | Linux attributed/process | macOS heavy/light | macOS attributed/process |
|---|---|---|---|---|---|
| 空闲 | win1ms | 3.993 | 0.992 | 3.915 | 0.993 |
| 空闲 | win10ms | 4.012 | 0.946 | 3.882 | 0.947 |
| 空闲 | perpoll | 3.646 | 0.907 | 3.677 | 0.853 |
| 超额订阅 | win1ms | 3.979 | — | 3.897 | — |
| 超额订阅 | win10ms | 3.616 | — | 3.941 | — |
| 超额订阅 | perpoll | 3.641 | — | 3.762 | — |

`perpoll` 只计 poll 内部的 CPU；窗口方案还把两次 poll 之间运行时自身的 CPU（调度、唤醒）按墙钟分给任务，所以总量更接近进程 CPU，比值也更接近工作量之比 4。attributed/process 低于 1 的部分是基准结束时尚未关闭的窗口（10 ms 窗口更明显）；实现中任务结束时要刷出（§3.6）。

#### sysbench（Linux arm64，tiup playground v8.5.8，同机 1 PD + 1 TiKV + tidb-rs）

1 张表 10 万行，`--db-ps-mode=auto --skip_trx=on`，每次 30 s（另有 5 s 预热），三轮交替取中位数。指标是 tidb-rs 进程每条查询的 CPU（`/proc/<pid>/stat`），QPS 主要受同机 TiKV 限制，各配置相同。32 线程超过 10 核，属于超额订阅。

| 负载 | 线程 | off（µs / 查询） | instant | win10 0% | win10 1% | win10 100% | win1 100% |
|---|---|---|---|---|---|---|---|
| oltp_point_select | 8 | 75.43 | +0.50% | +0.45% | +0.69% | +0.17% | +0.56% |
| oltp_point_select | 32 | 49.37 | +0.51% | +0.32% | +0.81% | +0.97% | +1.78% |
| oltp_read_only | 8 | 115.72 | −0.06% | −0.16% | +0.09% | +0.11% | +0.74% |
| oltp_read_only | 32 | 85.68 | +0.49% | +0.81% | −0.34% | +1.79% | +1.55% |

噪声：同一配置三轮之间相差 1–2.6%（例如 read_only 32 线程的 `off` 为 85.16～87.40）。所有差值都在这个范围内；只有 1 ms 窗口在 32 线程两种负载下都在 +1.5% 以上，倾向于真实但很小。

#### 对 §9 和 §6.1a 的含义

- §9 默认路径（Top SQL 关）：`instant` 相对 `off` +0.5% 以内，满足 ≤ 1%。
- §9 Top SQL 开（10 ms 窗口，100%）：相对 `instant` 在噪声内（−0.3%～+1.3%），满足"再增加 ≤ 1%"的目标，但 32 线程 read_only 的 +1.3% 接近上限，D14.3 实现后应在真实 TiKV 集群上复测，并以 Go 开 Top SQL 的开销作参照。
- §6.1a 采样：既然 100% 的开销已在预算内，0% / 1% 省不下可测的 CPU。这支持 Lucky 在 #118 提出的选项：Top SQL 开启时慢日志的 `Tidb_cpu_time` 总是计算，采样率只用于 Top SQL 聚合（需负责人决定）。
- 未测：`perf_event_open` 备选（§6.1 第 4 条）。在窗口方案已满足预算的情况下不需要。

### 6.2 语句级 CPU 字段（`Tidb_cpu_time` 等）

Go v8.5.8 的慢日志 `Tidb_cpu_time`/`Tikv_cpu_time`、`STATEMENTS_SUMMARY.AVG_TIDB_CPU_TIME`、PROCESSLIST 的 CPU 列（`util/ppcpuusage`）就是 `StmtExecRecord` 中的 CPU 数值。按 Go：

- **`Tidb_cpu_time` 是否出现跟随 `tidb_enable_top_sql`**（及采样，§6.1a）：Top SQL 关闭时，慢日志不写 `Tidb_cpu_time`，表中的列为 0；开启时才有值（Go `server/conn.go:1341` 只在 Top SQL 开启时分配 sqlID）。tidb-rs 照此（L1）；如果以后想在 Top SQL 关闭时也给出数值，是功能例外，要写进 `docs/compatibility.md`。
- **数值的计算方法不同**：Go 是 1 秒 profile 采样，短语句常常少算（profile 晚于慢日志写出）；tidb-rs 是 §6.1 的窗口分摊，语句结束时刷出，数值更接近真实值。这是实现例外，归 D3。
- `Tikv_cpu_time` 来自 TiKV 回包中的 ProcessTime（D14.0 的 `ExecDetails`），**与 Top SQL 无关、无条件累加**（Go `select_result.go:665`、`point_get.go:266`），Top SQL 关闭时慢日志中也有，和 Go 相同；采样率也不影响它。

### 6.3 执行次数、时长、网络字节

语句结束时从 `StmtExecRecord` 取，进 Top SQL 聚合器（Go `stmtstats` 的语义，L1），包括按 TiKV 地址的 KV 次数。

### 6.4 `resource_group_tag`：按请求计算，随事务/快照传递（同 Go）

Go 中 label 和 table id 是**每个请求**按它的第一个 key 算的（§1.5），tagger 随快照/事务选项传递，后台 commit 在语句结束后仍用原来的 tag，COMMIT 用最后一条语句的 tagger。所以：

- `resource_group_tag` **不放在 `ObsCtx`**。任务局部的查找对后台 commit、以及处理多条语句的池化任务都会给出错误的 tag。
- tagger（SQL digest + plan digest，等同 Go 的 `ResourceGroupTagBuilder`）作为**显式的请求上下文**，设置在 tidb-rs 的快照/事务对象上（D07.1 的 snapshot、事务上下文 D05.4），client-rust fork 在构造每个请求时按请求的第一个 key 算出 label 和 table id 并编码（与 Go `kv.go:872–899` 相同）。committer 复制 tagger，后台 secondary commit 继续用它。
- `request_source` 同样随请求上下文传递。
- client-rust 的 kvrpcpb 已有 `resource_group_tag`、`request_source` 字段；fork 要加的是请求构造时的 tagger 参数，与 N3 的 Q2 钩子、#101 一起改。

### 6.5 上报与归属

实现 `TopSQLPubSub` gRPC 服务（tipb proto，L1），挂在 status 端口；`TopSQLAgent` 客户端模式可以晚做。SQL meta / plan meta 的规范化文本照 Go（`NormalizeSQL`、`NormalizeFlatPlan`）。归属：标签与 CPU 记账拆成 D14.3 放进 P2（D4 已由 Lucky 同意），上报留在 D14.2（P5）。

## 7. 主题 5：TRACE（细节）

### 7.1 契约

用户看到的是三种格式的结果集：`row`（照 `dfsTree` 的树形和列）、`json`（span 树）、`log`（事件行）。时间和重试次数本来就不能重复，所以**契约是相同事件的 span 名称和层次**。span 的位置照 Go 的 49 处（`pkg/` 中）加 client-go 的 24 处（`regionRequest.SendReqCtx`、`RPCClient.SendRequest`、`twoPhaseCommitter.prewriteMutations`、`tikvSnapshot.get`、`loadRegion` 等），后者要加在 client-rust fork 中（D6 已定"照 Go"）。

### 7.2 两种实现，开工前比较

| | `tracing` crate 的 span | `ObsCtx` 中的小型记录器 |
|---|---|---|
| 做法 | 各处 `info_span!`；TRACE 时安装收集层 | 各处 `if let Some(t) = obs.trace() { t.enter("name") }`，记录到语句私有的树 |
| 关闭时的开销 | 只有当没有 subscriber 打开这个 callsite 时才是一次原子读。tidb-rs 用 `tracing` 作日志后端，这些 span 必须放在日志过滤永远不会打开的 target/level 上；"只对这条语句打开"会把 callsite 的 interest 变成 sometimes，每次调用都要检查 `enabled()` | 一次 task 局部读加一个 `Option` 判断 |
| 跨线程 | 按语句 `with_default` 安装的 subscriber 是线程作用域的，任务换线程后就丢了，需要自己实现按 span 归属的收集层 | 记录器在 `ObsCtx` 里，随任务继承，换线程不受影响 |
| 生态 | 以后接 OpenTelemetry 容易 | 需要自己写导出 |
| 输出 | 要从 `tracing` 的事件重建 Go 的三种格式 | 记录结构直接按 Go 的格式设计 |

倾向小型记录器（真正零开销、跟随任务、输出更直接），但要在 D14.2 开工前用一个原型比较两者的关闭开销和实现量再定。§8 第 1 条只要求 `ObsCtx` 能携带"可选的 TRACE 记录器"，两种实现都满足。

### 7.3 `TRACE PLAN`

优化器追踪（`planner/util/debugtrace`，D00.1 已移植树结构）随 P3 规划器移植；输出打包文件的路径和 HTTP 下载接口照 Go。

### 7.4 以后可选

把同一棵 span 树按 OpenTelemetry 导出，用于全链路追踪（不在 Go 中，功能例外，默认关闭）。

## 8. 现在必须定的接口

这些一旦晚定，就要回头改大量调用点：

1. **`ObsCtx` 挂在 N3 的 task 局部存储上，随 spawn 继承**（扩展现有 `set_cpu_recorder`/`CpuRecorder`）：内容是 CPU 归因用的 Top SQL 键 (sql digest, plan digest)、语句记录的弱引用（§3.5）、任务局部累计（§3.6）、可选的 TRACE 记录器。`ExecPool::spawn` 和阻塞池提交都要传播它。**不含** `resource_group_tag` 的 label/table id（第 4 条）。
2. **CPU 记账语义**：每次 poll 只读 `Instant`；每个 worker 线程按 1–10ms 窗口读线程 CPU 时钟，按墙钟比例分摊给窗口内运行过的 Top SQL 键（§6.1）。"语句 CPU = 分摊到的线程 CPU"是契约，Top SQL、资源管控、慢日志的 `Tidb_cpu_time` 都依赖它。改动只在 `tidb-runtime::slice`；D3 决定之前先跑 §6.1 的基准。
3. **plan digest 在执行开始前可得**：N1 冻结计划后、执行前计算规范化计划和 digest。D05.3 的语句主流程顺序：parse → digest(SQL) → optimize/freeze → digest(plan) → 设置 `ObsCtx` 的 Top SQL 键和事务/快照的 tagger → 执行。
4. **tagger 作为显式的请求上下文（同 Go）**：tagger（SQL + plan digest）设置在快照/事务对象上，client-rust fork 在构造每个请求时按第一个 key 算出 label 和 table id，并带上 `request_source`；committer 复制 tagger，后台 commit 继续用它（§6.4）。与 N3 的 Q2 钩子、#101 一起改 fork，在 D07.1/D07.2 之前完成。
5. **`StmtShared`（N2，#53）包含 `StmtExecRecord`**：`ExecDetails`、`RuntimeStatsColl`、阶段时间、plan 句柄、warnings 计数，以及 PROCESSLIST 读的 CPU（带每条命令的 sqlID，丢弃上一条语句迟到的 CPU）；在 Compile 之前发布（Go `SetProcessInfo`，`session.go:2082`）；内部可变、可被其他会话读。`RuntimeStatsColl` 只在唯一持有时复用。
6. **记录的封存与生命周期**（§3.5）：输出产生后封存；之后的 CPU 只记到 Top SQL 键；`ObsCtx` 不持有记录的强引用。
7. **执行器框架统一包装 `next()`**（D12.0，照 Go `exec.Next`）：计时、行数、loops 在框架层记到 `RuntimeStatsColl`，算子特有统计通过 `RuntimeStats` trait 注册。
8. **构建配置**：jemalloc 开 `profiling` feature（运行时默认不激活）；CPU profile 用 frame pointer（D2 已定，所有 crate `-C force-frame-pointers=yes`；`std` 是否带 frame pointer 待验证）。

**可以以后再做**：慢日志/语句摘要/EXPLAIN 的文字格式（L1，按块排期）；Top SQL 的 gRPC 上报；TRACE 的实现选择（§7.2，D14.2 开工前定）和三种格式；`TRACE PLAN`；`/debug/zip`；任务转储的具体格式；heap profile 的格式转换（待验证）；OpenTelemetry 导出。

## 9. 开销预算（验收）

| 项 | 目标 |
|---|---|
| 默认路径（Top SQL 关、无 TRACE、未超慢日志阈值） | sysbench oltp_read_only 与"关闭全部观测"的编译版本相比 CPU 差 ≤ 1%。poll 级只读 `Instant`，不读线程 CPU 时钟 |
| Top SQL 开（窗口分摊） | 再增加 ≤ 1%；用 §6.1 的基准验证，并以 Go 同场景开 Top SQL 的实测开销作为上限参照 |
| 共享结构写入 | 每次 poll 不写跨核共享的结构；刷出频率不高于每任务结束一次加每窗口一次（§3.6） |
| EXPLAIN ANALYZE | 与 Go 同一查询的执行时间比，额外开销不高于 Go |
| TRACE 关闭 | 按 §7.2 选定的实现测量，目标是关闭时只有一次 task 局部读 |
| heap profiling 未激活 | jemalloc `prof:true,prof_active:false` 的开销 ≤ 1%（TiKV 默认如此） |

## 10. 需要负责人决定（候选 `block` issue）

2026-11-13：D2、D4、D5、D6 由评审者 Lucky 同意（技术决定，frame pointer；D14.3 拆分），不需要负责人决定。**D1（#115）和 D3（#116）等负责人决定。**

| # | 问题 | 建议 | 状态 |
|---|---|---|---|
| D1 | `/debug/pprof/goroutine`、`/debug/pprof/mutex` 没有 Go 格式的对应物 | 返回 404 / not supported，写进 `docs/compatibility.md`；ng-monitoring 请求 `debug=` 变体的影响仍需验证 | **负责人已决定（#115，2026-11-13）** |
| D2 | CPU profile 的栈展开：frame pointer 还是 DWARF | frame pointer | 已同意（Lucky，技术决定） |
| D3 | Top SQL 与语句级 CPU 用按窗口分摊的记账代替 Go 的采样归因；**另加动态开关和采样率**（§6.1a），`tidb_enable_top_sql` 作为负债兼容 | 前提是 §6.1 的基准满足 §9 | **负责人已决定（#116，2026-11-13）** |
| D4 | `util/topsql` 的标签与 CPU 记账拆成 D14.3 提前到 P2，上报留在 P5 | 同意 | 已同意（Lucky，技术决定） |
| D5 | 只读阶段先做慢日志文件 + 语句摘要 v1，v2 随 P5 | 同意 | 已同意（Lucky，技术决定） |
| D6 | TRACE 的 span 名称、层次照 Go（含 client-go 的 24 处，在 client-rust fork 中加） | 照 Go | 已同意（Lucky，技术决定） |

## 11. 风险

1. **`StmtShared` 设计反复**：#53 的拆分如果先按 N2 现有字段实现、没包含观测数据，之后要再拆一次。缓解：#53 按 §8 第 5、6 条的清单设计。
2. **client-rust fork 的改动量**：tagger 与按请求计算的 `resource_group_tag`、`request_source`、Q2 钩子、请求完成回调（RPC 统计、backoff）、#101 的锁处理、TRACE 要求的 client-go 对应 span（24 处）都在 fork 中，和上游 client-rust 的差距会变大。缓解：集中在一个"请求上下文"参数里，尽量提交给上游。
3. **计划编码的复杂度**：`plancodec`、`FlattenPhysicalPlan`、binary plan 依赖完整的物理计划类型（P3），慢日志的 `Plan` 字段要到 P3 之后才能完整；P2 期间慢日志只能用 PoC 计划或空字段。需要在只读节点交付范围中说明。
4. **CPU 分摊的精度**：窗口方案在线程被抢占时按比例分摊，短于窗口、且和长任务同在一个窗口的语句会有误差；需要 §6.1 的基准量化。若不够准，备选 `perf_event_open`（权限、容器配置的风险）。
5. **ng-monitoring / Dashboard 兼容**：Top SQL 的 gRPC、profile 的格式（尤其 heap，§4 待验证）需要实际接上 ng-monitoring 验证。缓解：D14.3 完成时做一次端到端联调（Dashboard 的 Top SQL 页面能显示 tidb-rs 实例）。

## 12. 与现有 spec 的关系

- N3 §3.1 第 5 条的"CPU 记账改为线程 CPU 时间"：本文 §6.1、§8 第 2 条给出具体做法（窗口分摊），N3 定稿时引用。
- N2 / #53 的 `StmtShared`：按 §8 第 5、6 条补充字段（含 PROCESSLIST 的 CPU 与 sqlID、封存、`RuntimeStatsColl` 唯一持有时复用）。
- D14.0（`tidb-execdetails`）：`ExecDetails`、`RuntimeStatsColl`、`String()` 已移植，正是 `StmtExecRecord` 的主体；不需要重写。
- D05.2 的 `slow_log.rs`：规则匹配已移植；`SlowLogFormat` 等归 D14.2。
- D05.4（事务上下文）、D07.1（snapshot）：承载 tagger（§6.4）。
- ROADMAP：新增"D14.3 Top SQL 标签与 CPU 记账（P2）"（D4 已由 Lucky 同意）。
