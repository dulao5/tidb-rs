# N9：可观测性（profile、慢日志与语句摘要、EXPLAIN ANALYZE、Top SQL、TRACE）——调研与设计草案

状态：调研草案（2026-11-13，负责人在 DM 中提出：这几个主题要在现阶段调研清楚，避免项目成型后难以实现）。类型：实现例外（基础设计）+ 若干待决定的特性问题。计划定稿：P2 之前（执行器框架 D12.0、会话主流程 D05.3、coprocessor 客户端 D07.2 开工前）。相关：N2（会话与语句上下文，含 `StmtShared` 拆分，#53）、N3+A1（运行时、CPU 记账、内存）、D14.0（`tidb-execdetails`）、D00.1（stmtctx）、D05.2（`slow_log.rs` 的规则匹配）、#101（client-rust 锁处理）。

本文只做调研和接口设计，**不写生产代码**。结论分三类：Go 怎么做（兼容契约）、TiKV 怎么做（同源的 Rust 做法）、tidb-rs 的设计；最后列出"现在必须定的接口"和需要负责人决定的问题。

## 0. 总结

| 主题 | Go v8.5.8 的数据来源 | 对外契约（必须兼容） | TiKV 的做法 | tidb-rs 建议 |
|---|---|---|---|---|
| 1. CPU / heap profile | Go runtime pprof；`cpuprofile` 包常驻 1 秒一轮的 CPU profiler，HTTP 请求和 Top SQL 共享它 | status 端口 `/debug/pprof/{profile,heap,goroutine,mutex,allocs,block,trace,…}`、`/debug/zip`；pprof protobuf 格式；Dashboard 的"实例性能分析"按这些路径取数 | `pprof-rs`（信号采样）出 CPU profile；jemalloc `prof.dump` 出 heap profile；`/debug/pprof/{profile,heap,heap_activate,heap_deactivate,heap_list}` | 照 TiKV：`pprof-rs` + jemalloc profiling（A1 已定 jemalloc）；路径和格式与 Go 一致；goroutine/mutex 用"任务转储"和锁等待指标替代（待定 D1） |
| 2. 慢日志、语句摘要 | `ExecStmt.LogSlowQuery` / `SummaryStmt` 在语句结束时从 `StmtCtx`、`SessionVars`、`ExecDetails`、`RuntimeStatsColl`、`CopTasksDetails` 拼字段；计划经 `plancodec` 编码 | 慢日志文本格式（约 70 个 `# Key: value` 字段，`INFORMATION_SCHEMA.SLOW_QUERY` 84 列靠解析它）；`STATEMENTS_SUMMARY(_HISTORY)` 列；`Plan`（`tidb_decode_plan` 可解）和 `Binary_plan`（`tidb_decode_binary_plan`）编码 | 无慢日志表；TiKV 的 slow log 是普通日志行，按请求的 `Tracker` 计时 | **统一的语句执行记录**（§2.3 `StmtExecRecord`）：执行中各处只写这一个结构，慢日志、语句摘要、EXPLAIN ANALYZE、Top SQL 都从它派生；输出层逐字节照 Go |
| 3. EXPLAIN / EXPLAIN ANALYZE | `planner/core` 的 `ExplainInfo`/`OperatorInfo`；`RuntimeStatsColl` 按 plan id 收集根和 cop 统计，各算子自定义 `RuntimeStats.String()`（14 种） | 结果集列与每个字段的文字（`execution info`、`operator info`、`memory`、`disk`）；`FORMAT = 'brief'/'dot'/'row'/'verbose'/'tidb_json'/'true_card_cost'/'cost_trace'`；`EXPLAIN FOR CONNECTION` | coprocessor 返回 `ExecutorExecutionSummary`（每个 cop 算子的行数、时间、迭代数） | 算子统计按 plan id 进 `StmtExecRecord` 的算子表（D14.0 的 `RuntimeStatsColl` 就是它的 Go 形态）；文字照 Go 移植；`EXPLAIN FOR CONNECTION` 读 `StmtShared` 中的同一份数据 |
| 4. Top SQL | TiDB 侧 CPU：1 秒 CPU profile + goroutine pprof 标签（`sql_digest`、`plan_digest`）事后归因；执行次数、时长、网络字节：`stmtstats` 按 (sql, plan) 聚合；TiKV 侧：每个 KV 请求带 `resource_group_tag`（tipb `ResourceGroupTag`：SQL/plan digest + label），TiKV 自己统计 | ng-monitoring 通过 TiDB status 端口的 gRPC `TopSQLPubSub.Subscribe` 拉流（`tipb.TopSQLSubResponse`：record / SQL meta / plan meta）；`tidb_enable_top_sql`、`tidb_top_sql_max_*` 等变量；`resource_group_tag` 的编码 | `resource_metering`：请求处理时 `attach` 一个标签到线程局部存储（future 包装器 `InTags` 在每次 poll 前后挂上/取下），后台每秒读各线程的 CPU 时间（`/proc/<pid>/task/<tid>/stat`）按当前标签累加；读写字节数同样按标签记 | **不用采样 profile 归因**：N3 的 poll 包装器已经在每次 poll 前后拿到线程 CPU 时钟（`CLOCK_THREAD_CPUTIME_ID`），直接按任务携带的 (sql, plan) 标签累加，精确且便宜；KV 请求带同样编码的 `resource_group_tag`；对 ng-monitoring 的 gRPC 照 Go |
| 5. TRACE | opentracing + basictracer，约 27 个文件 55 处手工开 span（`StartSpanFromContext`、`tracing.StartRegion`）；`FORMAT='row'/'json'/'log'`；`TRACE PLAN`（优化器追踪，写文件） | 结果集：`row` 是三列树（operation、startTS、duration），`json` 是 appdash 风格 span 树，`log` 是带时间戳的事件行；`TRACE PLAN` 返回打包文件名 | 早期 minitrace（已从主线移除），现在靠 `Tracker` 的阶段时间戳；全链路追踪不是 TiKV 的主要手段 | 用 `tracing` 生态的 span（N3 的 task 天然是 span 的边界）；span 只在 TRACE 或采样开启时记录（`Span::none()` 零开销）；`row`/`json`/`log` 三种输出从同一棵 span 树生成；手工 span 的位置照 Go 的 55 处，不追求更多 |

**核心建议**：Go 的问题不是"记录得多"，而是同一件事在几处各记一遍（`ExecDetails` 合并、`RuntimeStatsColl`、`stmtstats`、opentracing span、pprof 标签各自独立），而且字段散在 `StmtCtx`、`SessionVars`、执行器局部变量里，靠语句结束时一个 400 行的函数（`LogSlowQuery` + `SummaryStmt` + `FormatSQL`/`getEncodedPlan`）拼起来。tidb-rs 用**一个语句级执行记录 + 一个任务级观测上下文**承载全部事实，五个输出都是它的"视图"。下面 §2 是这个模型，§3–§7 逐个主题说明怎么从它得到 Go 的输出。

## 1. 调研：Go v8.5.8

### 1.1 分散程度（实测）

| 机制 | 规模 | 位置 |
|---|---|---|
| opentracing span | 27 个文件、55 处 | `executor`、`session`、`store/copr`、`distsql`、`planner` 等 |
| `RuntimeStatsColl.RegisterStats` | 23 个文件、27 处；14 种自定义 `RuntimeStats.String()` | 各执行器 |
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

### 1.4 主题 3：EXPLAIN / EXPLAIN ANALYZE

- 计划侧：每个物理算子实现 `ExplainInfo`/`OperatorInfo`/`ExplainNormalizedInfo`，`Explain.RenderResult` 按 `FORMAT` 组织行；`tidb_json`、`dot`、`verbose`（带代价）、`true_card_cost`、`cost_trace` 等格式。
- 执行侧：`RuntimeStatsColl`（D14.0）按 plan id 存 `RootRuntimeStats`（根算子的 loops、rows、time、自定义统计）和 `CopRuntimeStats`（cop 任务的汇总，来自 TiKV 回传的 `ExecutorExecutionSummary` 加 `ScanDetail`、`TimeDetail`）；算子自己的统计（hash join build/probe、IndexLookUp 的回表、并发 worker 等）实现 `RuntimeStats` 接口，通过 `RegisterStats` 合并。`memory`、`disk` 列来自 `memory.Tracker` 树（A1）。
- `EXPLAIN FOR CONNECTION`：读另一个会话 `ProcessInfo` 中的计划和 `RuntimeStatsColl`（N2 中 `StmtShared` 要包含它）。

### 1.5 主题 4：Top SQL

- TiDB 侧 CPU：`topsql.AttachSQLInfo`/`AttachSQLAndPlanInfo` 在会话主 goroutine 上 `pprof.SetGoroutineLabels(sql_digest, plan_digest)`；`collector/cpu.go` 消费 1 秒 CPU profile，按标签汇总样本得到每个 (sql, plan) 的 CPU 毫秒。**只能统计带标签的 goroutine**：执行器 worker 若没有继承标签就算不进去（Go 靠 `pprof.Do` 和 context 继承，有遗漏）。
- 其他指标：`stmtstats.StatementStats` 按 (sql, plan) 记 `ExecCount`、`SumDurationNs`、`DurationCount`、`NetworkInBytes`/`NetworkOutBytes`，以及按 TiKV 地址的 `KvExecCount`。
- TiKV 侧：每个 KV/cop 请求的 `Context.resource_group_tag` = `resourcegrouptag.EncodeResourceGroupTag(sqlDigest, planDigest, label)`（tipb `ResourceGroupTag` protobuf，label 区分 row/index/unknown）；TiKV 按 tag 统计 CPU、读写 key 数，ng-monitoring 直接从 TiKV 拉。
- 上报：现在的方向是 ng-monitoring **订阅** TiDB 的 gRPC 服务 `TopSQLPubSub.Subscribe`（`reporter/pubsub.go`），TiDB 每分钟（`tidb_top_sql_report_interval_seconds`）推送记录和 SQL/plan meta（规范化 SQL 文本、规范化计划）；旧的 `TopSQLAgent.ReportTopSQLRecords` 客户端模式（`single_target.go`）仍保留。
- 开关：`tidb_enable_top_sql`、`tidb_top_sql_max_time_series_count`、`tidb_top_sql_max_meta_count`。

### 1.6 主题 5：TRACE

- `executor/trace.go`：`FORMAT='row'`（默认，三列树：`operation`、`startTS`、`duration`，用 appdash 收集后 `dfsTree` 画树）、`'json'`（span 树 JSON）、`'log'`（basictracer 的 `RawSpan` 按时间输出事件行）；`TRACE PLAN [TARGET='estimation'|'debug']` 执行优化器追踪并把结果打包成文件，返回文件名（另有 `/optimize_trace/dump/` HTTP 下载）。
- span 由 55 处手工 `StartSpanFromContext` 产生，只在 context 中有 tracer（TRACE 语句或 `tidb_general_log` 相关调试）时生效。

## 2. 调研：TiKV（同源 Rust 做法）

| 项 | 做法 | 位置 |
|---|---|---|
| CPU profile | `pprof::ProfilerGuardBuilder`（SIGPROF 采样，可选 frame-pointer）；`/debug/pprof/profile?seconds=N&frequency=…`，输出 protobuf 或火焰图 | `src/server/status_server/profile.rs`、`mod.rs` L797 |
| heap profile | jemalloc `prof.active` / `prof.dump`（`tikv_alloc::jemalloc::activate_prof/dump_prof`）；`/debug/pprof/heap_activate`、`heap_deactivate`、`heap_list`、`heap`；需要编译时开 jemalloc profiling | `components/tikv_alloc/src/jemalloc.rs` L416–545 |
| Top SQL（resource metering） | `ResourceMeteringTag::attach()` 把标签写进线程局部存储；future 用 `InTags` 包一层，每次 poll 前后挂上/取下；后台每秒遍历已注册线程，读 `/proc/<pid>/task/<tid>/stat` 的 CPU 时间差，记到该线程当前标签上；读写字节另有 summary recorder；按 `resource_group_tag` 上报 | `components/resource_metering/src/{lib.rs,recorder/sub_recorder/cpu.rs}` |
| 请求级计时 | `tracker::Tracker`：每个请求一个 slab 槽，各阶段写时间戳和扫描统计，结束时写进回包的 `TimeDetailV2`/`ScanDetailV2`；`FutureTrack` 的 `on_poll_begin/finish` 记 poll 时间 | `components/tracker/src/{lib.rs,future.rs}` |
| 调度与 CPU | yatp 多级反馈队列按任务累计时间降级；coprocessor 每 32 个 key 看一次时钟，超时间片 `reschedule().await` | `src/read_pool.rs`、`components/tidb_query_common/src/storage/scanner.rs` |
| 全链路追踪 | 曾用 minitrace，已从主线移除；现在 `tracing` 只作日志后端 | `Cargo.toml` |

**结论**：TiKV 的 Top SQL 归因是"线程当前标签 × 线程 CPU 时间差"，粒度是每秒一次抽样。tidb-rs 的 N3 已经在每次 poll 前后读线程 CPU 时钟（`tidb-runtime` 的 `stall::sample`），可以做得更精确：直接把这一次 poll 的 CPU 时间加到任务的标签上，不需要后台线程，也不受"一秒内多个标签"的抽样误差影响。

## 3. tidb-rs 设计：统一的观测模型

### 3.1 原则

1. **一个事实只记一次**：执行中产生的每个数字（行数、时间、CPU、扫描 key 数、内存峰值、RPC 次数、等待时间）只写入一个地方；五个输出从同一份数据派生。
2. **输出层逐字节照 Go**：慢日志格式、`SLOW_QUERY`/`STATEMENTS_SUMMARY` 的列、EXPLAIN 的文字、计划编码、`resource_group_tag`、Top SQL 的 gRPC 消息、TRACE 的三种格式都是兼容契约，按 L1 移植（各自归 D14.x、D12.12 等块）。内部收集方式是实现例外。
3. **默认路径几乎零开销**：常开的只有计数器和 poll 级 CPU 记账（已经在 N3 里，约 2 次时钟读）；span、算子级详细统计只在需要时开（EXPLAIN ANALYZE、TRACE、慢日志阈值、采样）。
4. **按 N3 的任务模型挂载**：语句 = 连接 task（加上它 `pool.spawn` 的 worker task），任务继承观测上下文；不依赖 Go 那样的 context 透传约定。

### 3.2 三个对象

| 对象 | 生命周期 | 内容 | 谁写 | 谁读 |
|---|---|---|---|---|
| **`ObsCtx`**（任务级观测上下文） | 随 task 继承，挂在 N3 的 task 局部存储里（扩展现有的 `CpuRecorder`） | 指向当前语句的 `StmtExecRecord`；Top SQL 标签 (sql digest, plan digest, resource group)；当前 span（`tracing::Span`，默认 `none`）；当前算子 plan id（可选） | 运行时（poll 包装器）、会话（语句开始/结束时替换） | poll 包装器、KV 客户端（取标签编码 `resource_group_tag`） |
| **`StmtExecRecord`**（语句执行记录） | 一条语句一份，放在 N2 的 `StmtShared` 中（`Arc`，内部可变，#53 的拆分） | 阶段时间（parse、compile、optimize、wait TS、执行、Q1/Q2 等待）；CPU（按任务累加）；`ExecDetails`（D14.0）；`RuntimeStatsColl`（按 plan id 的算子统计，D14.0 已移植）；内存/磁盘峰值（A1 tracker）；KV RPC 统计（次数、字节、backoff、按 store 计数）；warnings 计数；plan 句柄（N1 冻结的物理计划） | 执行器、copr 客户端、运行时 | 语句结束时的输出层；`EXPLAIN FOR CONNECTION`、`PROCESSLIST`（读进行中的） |
| **`ObsSinks`**（进程级） | 进程 | 慢日志写出器、语句摘要（v1/v2）、Top SQL 聚合器、TRACE 收集器、Prometheus 指标 | 语句结束时调用 | 系统表、HTTP、gRPC |

`StmtExecRecord` 的字段就是 Go 的 `ExecDetails` + `RuntimeStatsColl` + `SessionVars.Duration*` + `StmtCtx` 中执行期字段的并集，**不是新发明**；不同的只是放在一个地方、由运行时和执行器写，而不是语句结束时到处收集。

### 3.3 写入点（取代 Go 的散落记录）

| 事实 | Go 的记录位置 | tidb-rs 的写入点 |
|---|---|---|
| CPU 时间 | Top SQL：pprof 标签 + 采样；`ExecDetails` 没有 TiDB 侧 CPU | poll 包装器：每次 poll 的线程 CPU 时钟差 → `ObsCtx` 指向的记录与 Top SQL 标签（**一处**） |
| 算子行数、循环次数、时间 | 各执行器 `runtimeStats.Record(d, rows)` + `RegisterStats` | 执行器框架（D12.0）在 `next()` 外层统一记（Go 的 `exec.Next` 包装 `RecordOneCopTask` 也在框架层）；算子特有的统计（hash join 的 build/probe 等）仍由算子实现 `RuntimeStats` 写入同一个 `RuntimeStatsColl` |
| cop 任务统计 | `copr` 中 `RecordOneCopTask`、`MergeExecDetails`、`CopTasksDetails` | copr 客户端（D07.2）收到响应时写一次 `RuntimeStatsColl` 和 `ExecDetails` |
| KV RPC、backoff | client-go `ExecDetails`（`util.ExecDetails`）经 context 回传 | client-rust 请求完成回调写 `ObsCtx` 的记录（需要 client-rust 加钩子，与 N3 的 Q2 钩子同一处） |
| 阶段时间 | `SessionVars.DurationParse/Compile/…` | 会话主流程（D05.3）写记录 |
| span | 55 处 `StartSpanFromContext` | 同样的位置用 `tracing::info_span!`，父子关系来自 `ObsCtx` 的当前 span；不开 TRACE 时 `Span::none()` |
| 内存峰值 | `MemTracker.MaxConsumed` | A1 tracker 根节点，结束时读一次 |

### 3.4 派生输出

| 输出 | 怎么从记录得到 | 归属块 |
|---|---|---|
| EXPLAIN ANALYZE | 计划行（N1 冻结计划 + `ExplainInfo` 文字）× `RuntimeStatsColl.get(plan_id)` 的 `String()` | D10.9（EXPLAIN 输出、计划编码）、D12.12（EXPLAIN 语句）；D14.0 已有 `String()` |
| 慢日志 | 语句结束、超过阈值或匹配 `tidb_slow_log_rules`（D05.2 已移植规则）时，按 Go 的字段顺序格式化记录 | D14.2（`SlowLogFormat` 等） |
| 语句摘要 | 记录按 key 聚合进 `stmtsummary` | D14.2 |
| Top SQL | CPU：poll 包装器实时按标签累加（进程级 `DashMap<(sql, plan), Stats>`）；执行次数、时长、网络字节：语句结束时从记录取；每分钟推给订阅者 | D14.x（新子块，见 §6） |
| TRACE | span 树（`tracing` 的收集层）→ `row`/`json`/`log` 三种格式 | D14.2（trace）、D12.12（TRACE 语句） |
| EXPLAIN FOR CONNECTION / PROCESSLIST | 读另一个会话 `StmtShared` 中的记录（进行中的统计） | D12.12 / D05.3 |
| 指标 | 记录结束时更新直方图；运行时、队列指标已在 N3 | 各块 |

## 4. 主题 1：profile（细节）

- **CPU**：`pprof-rs`（TiKV 同版本系列），`/debug/pprof/profile?seconds=N` 输出 pprof protobuf（与 Go 同格式，Dashboard 可直接用）；需要 frame pointer 或 DWARF 展开（TiKV 用 `pprof-fp` feature + `-C force-frame-pointers`），这是**构建配置**，要现在定（待定 D2）。
- **heap**：jemalloc profiling（A1 已定 jemalloc）。需要编译时开 `profiling` feature、运行时 `prof:true,prof_active:false`，按 TiKV 的 `heap_activate/heap_deactivate/heap_list` 加上 Go 的 `/debug/pprof/heap`（返回 jemalloc 的 dump，Dashboard 需要能解析：TiKV 的 heap profile 已被 Dashboard 支持，用同样的格式）。
- **goroutine / mutex 的对应物**（待定 D1）：
  - goroutine → **任务转储**：tokio 的 `Handle::dump()`（需要 `tokio_unstable` + `taskdump`，只在 Linux x86_64/aarch64），或 N3 运行时自己维护"活跃任务表"（task id、所属连接、当前语句 digest、已运行 CPU、最近一次 poll 开始时间）。建议后者：开销可控、跨平台，且对 PROCESSLIST 也有用。`/debug/pprof/goroutine` 返回这个表的文本形式（不是 Go 格式，Dashboard 的 goroutine 视图不可用——需要负责人确认）。
  - mutex/block → 锁等待：N3 已有停顿检测（`on_cpu`/`blocked`/`preempted` 分类）；`parking_lot` 锁等待时间可以用 `lock_api` 包装统计。`/debug/pprof/mutex` 无法给出 Go 格式。
- `/debug/zip`：打包 CPU profile、heap、任务转储、配置、版本，路径保留。
- **持续 profile**（Dashboard "持续性能分析"由 ng-monitoring 定期拉 `/debug/pprof/*`）：只要上面的路径和格式兼容即可，不需要 tidb-rs 内部常驻 profiler。Go 的常驻 1 秒 profiler 是为 Top SQL 服务的；tidb-rs 的 Top SQL 不用采样（§6），所以**不需要常驻 profiler**。

## 5. 主题 2、3：慢日志、语句摘要、EXPLAIN（细节）

- **计划编码依赖扁平计划**：`Plan`/`Binary_plan`/plan digest 都基于 `FlattenPhysicalPlan`，而 N1 规定计划在 `DoOptimize` 结束时冻结。所以扁平化和编码只读冻结计划，可以放到语句结束后（甚至后台）做；但 **plan digest 必须在执行开始前算出**——Top SQL 标签和 `resource_group_tag` 要用（Go 在 `ExecStmt.Exec` 前 `AttachSQLAndPlanInfo`）。这一点碰 N1 和 D05.3 的顺序，要现在定。
- **算子统计的写入点集中在执行器框架**：D12.0 设计 `Executor` trait 时，`next()` 由框架包一层（计时、行数、loops、checkpoint），算子只实现内部逻辑。这是"减少笨力气"的关键：Go 每个执行器自己 `if e.RuntimeStats() != nil { ... }`，tidb-rs 在框架层做一次。只有算子特有的统计（Go 的 14 种自定义 `RuntimeStats`）由算子自己写。
- **进行中可读**：`StmtExecRecord` 内部可变（原子计数 + 少量 `Mutex`），`EXPLAIN FOR CONNECTION` 和 `PROCESSLIST` 读的是同一份。这要求 N2 的 `StmtShared` 包含 `RuntimeStatsColl` 和 `ExecDetails`（#53 已列 `ExecDetails`，要补 `RuntimeStatsColl` 和 plan 句柄）。
- **兼容测试**：慢日志行、`SLOW_QUERY` 解析、计划编码/解码、EXPLAIN 文字都能用 govec 对照（输入是构造好的记录/计划）；SQL 层用混合集群对照 `SLOW_QUERY`、`STATEMENTS_SUMMARY` 和 `tidb_decode_plan` 的结果（Go 解码 tidb-rs 写的计划）。

## 6. 主题 4：Top SQL（细节）

- **TiDB 侧 CPU（推荐做法，实现例外）**：N3 的 poll 包装器已经在每次 poll 前后可以读 `CLOCK_THREAD_CPUTIME_ID`（`tidb-runtime::stall::sample`，目前只在停顿分类时读）。改为**每次 poll 都读线程 CPU 时钟**（约 20–50ns × 2），把差值加到 `ObsCtx` 的 Top SQL 标签上。这同时解决了 N3 §3.1 第 5 条的遗留问题（"当前按 poll 的墙钟时间累计，CPU 超额订阅时会把被抢占的时间也算进去；接 Top SQL、资源管控之前要改成线程 CPU 时间"）。精度比 Go 的 10ms 采样高，worker task 自动继承标签，没有 Go"worker 漏标签"的问题。
  - 开销预算：每次 poll 多两次 vDSO 时钟读；sysbench 点查每条语句约 3–10 次 poll，可忽略。验收：开/关 Top SQL 的 CPU 差 ≤ 1%（与 Go 开 Top SQL 的约 3% 开销比较）。
  - macOS：`CLOCK_THREAD_CPUTIME_ID` 可用，但走 `thread_info` 系统调用（约 1µs），开销见 §11 风险 4。
- **不在 task 内的 CPU**：阻塞池（N3 的 Q3）上的工作同样带 `ObsCtx`；网络读写、TLS 在连接 task 内，归到该连接当前语句（Go 也是如此）。
- **执行次数、时长、网络字节、按 store 的 KV 次数**：语句结束时从 `StmtExecRecord` 取，进同一个聚合器（Go 的 `stmtstats` 语义，L1）。
- **TiKV 侧**：每个 KV/cop 请求的 `resource_group_tag` 用 Go 相同的编码（tipb `ResourceGroupTag`），从 `ObsCtx` 取 digest 和 label。client-rust 的 kvrpcpb 已有 `resource_group_tag` 字段（`src/generated/kvrpcpb.rs`），需要 fork 中的请求构造入口能传入（与 N3 的 Q2 钩子、#101 一起改 client-rust）。**这是必须现在定的接口**：每个发往 TiKV 的请求都要能拿到语句的标签，晚加就要改所有 KV 调用点。
- **上报**：实现 `TopSQLPubSub` gRPC 服务（tipb proto，L1），挂在 status 端口；`TopSQLAgent` 客户端模式可以晚做。SQL meta / plan meta 的规范化文本照 Go（`NormalizeSQL`、`NormalizeFlatPlan`）。
- **归属**：Go 的 `util/topsql` 在 ROADMAP 中属于 D14.2（slow log、stmt summary、topsql、trace，P5）；计划编码和 EXPLAIN 输出属于 D10.9。建议按本文把"标签与 CPU 记账"拆成一个小子块放进 P2（随 D07.2 copr 客户端），上报和 gRPC 仍在 P5。

## 7. 主题 5：TRACE（细节）

- `tracing` crate 的 span 作为唯一的 span 来源；N3 的 task 创建时 `in_current_span()` 继承，`ObsCtx` 保存当前 span。
- 不开 TRACE 时 span 为 `Span::none()`（`tracing` 的禁用开销是一次原子读）；`TRACE <stmt>` 时为该语句安装一个收集层（per-statement subscriber，记录到语句私有的 span 树），结束后转成三种格式：`row`（照 `dfsTree` 的树形和列）、`json`（照 appdash 的结构）、`log`（照 basictracer `RawSpan` 的事件行）。三种格式归 D12.12，L1。
- span 的位置照 Go 的 55 处（`TRACE` 的输出是用户可见的，span 名称要一致）；不追求更多，否则 `row` 格式的树会和 Go 不同。
- `TRACE PLAN`：优化器追踪（`planner/util/debugtrace`，D00.1 已移植 debugtrace 的树结构）随 P3 规划器移植；输出打包文件的路径和 HTTP 下载接口照 Go。
- **以后可选**（不在 Go 中，功能例外，默认关闭）：把同一棵 span 树按 OpenTelemetry 导出，用于全链路追踪。现在只需保证 span 结构不阻碍这一点。

## 8. 现在必须定的接口

这些一旦晚定，就要回头改大量调用点：

1. **`ObsCtx` 挂在 N3 的 task 局部存储上，随 spawn 继承**（扩展现有 `set_cpu_recorder`/`CpuRecorder`）：内容至少包括语句记录句柄、Top SQL 标签 (sql digest, plan digest, label, resource group)、当前 span。`ExecPool::spawn` 和阻塞池提交都要传播它。
2. **poll 包装器改为每次读线程 CPU 时钟**（而不是墙钟），并把差值同时记到语句和 Top SQL 标签。改动只在 `tidb-runtime::slice`，但语义（"CPU 时间 = 线程 CPU 时钟"）是契约，资源管控、Top SQL、慢日志的 `Process_time`/CPU 字段都依赖它。
3. **plan digest 在执行开始前可得**：N1 冻结计划后、执行前计算规范化计划和 digest，写入 `ObsCtx` 标签。D05.3 的语句主流程顺序：parse → digest(SQL) → optimize/freeze → digest(plan) → 设置标签 → 执行。
4. **每个 KV/cop 请求能拿到 `resource_group_tag`、`request_source`**：client-rust fork 的请求构造要从调用方（或 task 局部的 `ObsCtx`）取这两个字段。与 N3 的 Q2 钩子、#101 的锁处理一起改 fork，在 D07.1/D07.2 之前完成。
5. **`StmtShared`（N2，#53）包含 `StmtExecRecord`**：至少 `ExecDetails`、`RuntimeStatsColl`、阶段时间、plan 句柄、warnings 计数；内部可变、可被其他会话读（`EXPLAIN FOR CONNECTION`、`PROCESSLIST`）。#53 的拆分要按这个清单设计。
6. **执行器框架统一包装 `next()`**（D12.0）：计时、行数、loops 在框架层记到 `RuntimeStatsColl`，算子不再各自写；算子特有统计通过 `RuntimeStats` trait 注册。这是 D12.0 的接口，开工前要定。
7. **构建配置**：jemalloc 开 `profiling` feature（运行时默认不激活）；CPU profile 的栈展开方式（frame pointer：所有 crate 编译 `-C force-frame-pointers=yes`，约 1–2% 开销；或 DWARF：无运行时开销但采样慢）。影响全部二进制，要现在定（待定 D2）。

**可以以后再做**：慢日志/语句摘要/EXPLAIN 的文字格式（L1，按块排期）；Top SQL 的 gRPC 上报；TRACE 的三种格式；`TRACE PLAN`；`/debug/zip`；任务转储的具体格式；OpenTelemetry 导出。

## 9. 开销预算（验收）

| 项 | 目标 |
|---|---|
| 默认路径（Top SQL 关、无 TRACE、未超慢日志阈值） | sysbench oltp_read_only 与"关闭全部观测"的编译版本相比 CPU 差 ≤ 1% |
| Top SQL 开 | 再增加 ≤ 1%；Go 同场景开 Top SQL 的实测开销作为上限参照 |
| EXPLAIN ANALYZE | 与 Go 同一查询的执行时间比，额外开销不高于 Go |
| TRACE | 只影响被 TRACE 的语句 |
| heap profiling 未激活 | jemalloc `prof:true,prof_active:false` 的开销 ≤ 1%（TiKV 默认如此） |

## 10. 需要负责人决定（候选 `block` issue）

| # | 问题 | 建议 |
|---|---|---|
| D1 | `/debug/pprof/goroutine`、`/debug/pprof/mutex` 没有 Go 格式的对应物。可选：(a) 返回 tidb-rs 自己的任务转储/锁等待文本，Dashboard 对应视图不可用；(b) 返回 404；(c) 与 Dashboard 协调新格式 | (a)，并在 `docs/compatibility.md` 中列出 |
| D2 | CPU profile 的栈展开：frame pointer（全量编译 `-C force-frame-pointers`，约 1–2% 开销，采样快、准）还是 DWARF（无常驻开销，采样慢） | frame pointer（与 TiKV 的 `pprof-fp` 一致） |
| D3 | Top SQL 的 TiDB 侧 CPU 用"poll 级线程 CPU 时钟精确记账"代替 Go 的"CPU profile 采样归因"：数值会比 Go 更准，与 Go 不完全一致（实现例外，用户可见的是 Dashboard 中的 CPU 数值） | 同意例外 |
| D4 | `util/topsql` 的标签与 CPU 记账拆成子块提前到 P2（随 D07.2），上报留在 P5 | 同意 |
| D5 | 是否在只读阶段实现慢日志文件、语句摘要 v2（文件持久化）；还是先只做内存中的 `STATEMENTS_SUMMARY`/`SLOW_QUERY` | 慢日志文件 + 语句摘要 v1 先做，v2 随 P5 |
| D6 | TRACE 的 span 名称、层次照 Go（用户可见），不额外加 span | 照 Go |

## 11. 风险

1. **`StmtShared` 设计反复**：#53 的拆分如果先按 N2 现有字段实现、没包含观测数据，之后要再拆一次。缓解：#53 按 §8 第 5 条的清单设计。
2. **client-rust fork 的改动量**：`resource_group_tag`、`request_source`、Q2 钩子、请求完成回调（RPC 统计、backoff）、#101 的锁处理都在 fork 中，和上游 client-rust 的差距会变大。缓解：集中在一个"请求上下文"参数里，尽量提交给上游。
3. **计划编码的复杂度**：`plancodec`、`FlattenPhysicalPlan`、binary plan 依赖完整的物理计划类型（P3），慢日志的 `Plan` 字段要到 P3 之后才能完整；P2 期间慢日志只能用 PoC 计划或空字段。需要在只读节点交付范围中说明。
4. **线程 CPU 时钟的平台差异**：Linux 用 vDSO，开销低；macOS 的 `thread_info` 系统调用约 1µs，在 macOS 开发机上会放大 poll 开销。缓解：macOS 上默认按墙钟近似（只影响开发机），Linux 用线程 CPU 时钟；验收在 Linux 上做。
5. **ng-monitoring / Dashboard 兼容**：Top SQL 的 gRPC、profile 的格式需要实际接上 ng-monitoring 验证；晚接会发现格式细节问题。缓解：P2 子块完成时做一次端到端联调（Dashboard 的 Top SQL 页面能显示 tidb-rs 实例）。

## 12. 与现有 spec 的关系

- N3 §3.1 第 5 条的"CPU 记账改为线程 CPU 时间"：本文 §6、§8 第 2 条给出具体做法，N3 定稿时引用。
- N2 / #53 的 `StmtShared`：按 §8 第 5 条补充字段。
- D14.0（`tidb-execdetails`）：`ExecDetails`、`RuntimeStatsColl`、`String()` 已移植，正是 `StmtExecRecord` 的主体；不需要重写。
- D05.2 的 `slow_log.rs`：规则匹配已移植；`SlowLogFormat` 等归 D14.2。
- ROADMAP：建议新增"D14.3 Top SQL 标签与 CPU 记账（P2）"（待 D4 决定）。
