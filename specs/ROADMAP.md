# ROADMAP：tidb-rs 只读阶段的功能块计划（v8.5.8）

状态：P0 草案，团队评审中；2026-11-04 按 PR #1 评审意见修订；2026-11-07 按 N0 评审拆分 D01.8（其余函数归属的微调未重新生成本文表格，以 tracker 为准）。规则见 [CONSTITUTION.md](../CONSTITUTION.md)，方法见 [docs/porting/methodology.md](../docs/porting/methodology.md)，例外见 [exceptions/](exceptions/README.md)。

## 1. 数据来源与口径

- 基线：Go TiDB **v8.5.8**（`8b857efa2`）。
- 函数清单、函数级引用图来自 [tidb-rs-tracker](https://github.com/dulao5/tidb-rs-tracker)（`go_funcs.tsv`、`go_decls.tsv`、引用图 `scripts/refgraph.sh`）。
- **划分规则**：按 Go 文件和函数名逐条匹配，每个函数恰好属于一个块。大文件（如 `logical_plan_builder.go`、`planbuilder.go`、`executor/builder.go`）按函数归属，而不是整文件归属。
- **规则横切**：Go 把每条逻辑优化规则写成"每个算子上的一个方法"（如 `PruneColumns`、`PredicatePushDown`、`DeriveStats`）。这些方法归**规则所在的块**，不归算子所在的块。例如 D09.7 列裁剪包含 22 个文件里的方法。
- **行数**：非生成代码的函数体行数，不含注释和类型定义；实际源码量约为它的 1.5 倍。
- **依赖**：块 A 中的函数引用了块 B 中的函数、类型、变量或常量，记为 A → B。

## 2. 总量

| 阶段 | 块数 | Go 函数体行数 | Go 单测行数¹ | 内容 |
|---|--:|--:|--:|---|
| P1 地基 | 22 | 53.5k | 44.0k | 常量、类型、编码、chunk、元数据模型、上下文接口、系统变量与 SessionVars、表达式框架与核心函数 |
| P2 读取链路 | 11 | 39.9k | 37.0k | meta/InfoSchema/table、schema 同步、kv driver、coprocessor、distsql、下推判定、协议层、执行统计结构 |
| P3 优化器 | 29 | 77.7k | 32.6k | planbuilder、表达式改写、逻辑规则、物理优化、ranger、代价、基数估算、统计信息加载、hint |
| P4 执行器 | 14 | 47.3k | 23.4k | 执行器框架与各算子、落盘容器、会话主流程 |
| P5 外围 | 13 | 62.7k | 56.3k | 字符串/时间/杂项函数、SHOW/EXPLAIN/SET、infoschema 内存表、权限、binding、可观测性、HTTP 接口 |
| 例外（EXC） | 6 | 13.7k | 10.3k | JSON（A4）、coprocessor cache 与 plan cache（A3）、内存配额（A1）、废弃/非默认路径（B1–B5） |
| 推迟（D1） | 1 | 5.3k | 1.1k | TiFlash / MPP |
| 按需（工具） | 1 | 11.7k | 14.6k | `util/*`：被已排期的块用到时整文件移植 |
| 不移植（X） | 5 | 161.8k | 152.2k | DDL、写路径、ANALYZE、lightning、BR、disttask、TTL、bootstrap、mock store、测试工具 |
| 已完成 | 3 | 19.1k | 14.9k | parser（语法表同构移植）、metrics |

只读主线 P1–P4 约 22 万行函数体（源码约 33 万行），直接对应的 Go 单测约 14 万行。

¹ 粗略统计：`foo_test.go` 归入 `foo.go` 所属的块，没有同名源文件的归入所在包函数最多的块；另有约 11 万行测试在只有测试的包里（如 `planner/core/casetest`，多为 SQL 级测试），未计入。测试工作量按宪章第七条处理：能用 govec 向量回放的不逐个翻译；依赖存储的用 SQL 级对照覆盖。tracker 会给出逐块的精确数字。

## 3. 依赖结构的结论

1. **不存在无环的移植顺序**。按函数级引用，P1–P5 的 84 个块构成一个强连通分量：Go TiDB 的模块之间普遍互相引用（大部分经由 `sessionctx.Context`、`expression`、`chunk`、`types` 等中心文件）。所以只能按阶段推进，把"早期阶段引用后期阶段"的地方逐个处理。
2. **阶段调整**：根据反向引用，把 kv 接口（D07.0）、系统变量与 SessionVars（D05.1、D05.2）、Vector 类型（D01.6）提前到 P1；把执行统计数据结构（D14.0）提前到 P2；把会话主流程（D05.3）推后到 P4（之前继续用 PoC 的会话层）。
3. **剩余的跨阶段引用**（引用数前列）都按"接口先行 + 边界桩"处理。边界桩按宪章第四条 3 分为临时、永久不支持、委托三类；指向 X（不移植）的引用属于委托或永久类，不会随某个块完成而消失，排期时与临时桩分开统计：

| 引用方 → 被引用方 | 引用数 | 处理 |
|---|--:|---|
| D08.3 下推判定 → 字符串/时间/杂项函数（P5） | 245 | 下推表按函数签名列全，未移植的函数在根节点求值时报"不支持" |
| D03.1 协议层 → D05.3 会话主流程（P4） | 87 | P4 之前协议层接 PoC 会话 |
| D09.0 planbuilder → D14.2 慢日志/语句摘要（P5） | 50 | 桩：不记录 |
| D05.3 会话 → D04.1 权限（P5） | 46 | 权限检查接口先行，P5 之前沿用 PoC 的认证 |
| D06.5 domain → D11.2 统计信息加载（P3） | 45 | 桩：domain 暂不启动统计信息后台任务 |
| D07.3 distsql → D10.4 ranger（P3） | 23 | 只依赖 `ranger.Range` 类型；按 N0 评审，类型定义（`types.go`、`ranger/context`、`Full*Range`）归 D00.1，P1 移植 |

4. **例外要按依赖时间定稿**：P1 的 CAST、类型转换、Datum 大量引用 JSON 类型（D01.5，例外 A4），chunk 引用内存统计（D15.1，例外 A1）。所以 **A4 和 A1 的正式 spec 必须在 P1 开工前确认**，与 N1–N3 一起。

## 4. 各阶段的块

"层"是阶段内部的依赖层次（按引用数 ≥ 10 的依赖计算）：L1 先做，同一层可以并行。P1 内部耦合最紧（类型、表达式、上下文互相引用），层次只作为参考，实际按"类型与接口先行、实现随后"分两三波推进。

### P1（22 块，Go 函数体 53.5k 行）

| 层 | 块 | 内容 | Go 行数 | 文件数 | 需要边界桩 / 接口的依赖（引用数 ≥ 10） |
|---|---|---|--:|--:|---|
| L1 | [D00.2](D00.2-base/spec.md) | 常量、错误码与配置（parser/mysql、terror、errno、dbterror、config）。**P1 第一个块**：几乎所有块都引用错误码；`parser/mysql` 的常量放进一个不依赖语法表生成的小 crate | 1,281 | 13 | — |
| L1 | [D01.6](D01.6-vector/spec.md) | Vector 类型 | 409 | 2 | — |
| L1 | [D01.7](D01.7-chunk/spec.md) | Chunk 与列存（列格式按 N5a，内存部分按 A1；2026-11-08 复用池内部按 M-POOL 归 X，349 → 330 个函数；crate `tidb-chunk`，converge 276/330，其余等 D01.1 / D01.10） | 3,082 | 11 | D01.5(11), D15.1(10) |
| L1 | [D01.9](D01.9-collation/spec.md) | Collation 与字符集 | 637 | 10 | — |
| L1 | [D07.0](D07.0-kv/spec.md) | kv 接口与 key 类型（pkg/kv；2026-11-08 按只读读路径划分：40 个归 X、7 个归 DEF.MPP，158 → 111 个函数；crate `tidb-kv`，converge 102/111，其余等 D01.8 / D02.2 / D07.1） | 1,348 | 15 | — |
| L1 | D08.1 | 表达式框架（Expression、Column、Constant、ScalarFunction、Schema、求值与向量化接口；按 N4） | 4,208 | 13 | EXC.B(33) |
| L2 | D01.1 | Datum、FieldType 与类型辅助 | 3,906 | 13 | D01.5(21) |
| L1 | D02.3 | parser 的 model、types、auth、duration 的忠实移植（D02.2 中只有 PoC 替身；D06.1 的前置，[spec](D02.3-parser-model-types/spec.md)） | 960 | 7 | — |
| L2 | [D06.1](D06.1-model/spec.md) | 元数据模型（meta/model；2026-11-07 DDL job 的写路径函数改归 X，371 → 153 个函数。Part 1（PR #20，已合并）：Go-JSON 层、schema 结构体与 139 个函数；Part 2（PR (b)）：`job.go`、`reorg.go` 的 13 个函数与 `ColumnNeedRestoredData`，converge 153/153） | 1,475 | 10 | — |
| L3 | [D00.1](D00.1-ctx/spec.md) | 上下文接口骨架（sessionctx、planctx、exprctx、stmtctx、errctx、base 接口；2026-11-09 第 1 部分：errctx、util/context、stmtctx 核心；第 2a 部分：exprctx、expropt 的 provider 集合、exprstatic、ranger 类型，converge 294/433；第 2b 部分：expropt 的 provider、planctx、PlanCounterTp、debugtrace、stmtctx 的 tracker，converge 304/394；util/context 的 39 个函数改归 D00.4（tidb-base）） | 2,773 | 29 | — |
| L3 | D01.2 | Decimal（MyDecimal；流程试点） | 2,004 | 1 | — |
| L3 | [D01.3](D01.3-time/spec.md) | 时间与 Duration | 3,400 | 3 | — |
| L3 | [D08.2](D08.2-funcs/spec.md) | 函数注册、类型推导、常量折叠与传播、collation 推导（第 1 部分：注册表、baseBuiltinFunc 构造、collation 推导） | 2,385 | 9 | — |
| L4 | D01.4 | 类型转换（convert）与溢出、截断 | 883 | 3 | D01.5(63) |
| L4 | D01.8 | 编码：util/codec（Q3：从 Go 移植 L1，TiKV 作为双向逐字节 oracle，#39） | 1,913 | 5 | — |
| L4 | [D01.11](D01.11-tablecodec/spec.md) | 表与行编码：tablecodec、rowcodec（N0 评审从 D01.8 拆出，位于 kv 之上；2026-11-08 按只读读路径划分：63 个归 X，169 → 106 个函数；crate `tidb-tablecodec`，converge 106/106，TiKV 双向对照） | 2,736 | 6 | — |
| L4 | [D05.1](D05.1-sysvar/spec.md) | 系统变量定义与校验（N8；2026-11-08 划分：写路径 / extension / TTL 辅助 7 个归 X，需要会话状态的 10 个归 D05.2，96 → 79 个函数；crate `tidb-sessionctx`，converge 79/79） | 1,027 | 8 | — |
| L4 | D08.12 | 表达式工具（util.go 等，按引用归属后再拆） | 2,384 | 4 | — |
| L4 | D08.5 | 算术、数学与逻辑运算 | 6,228 | 6 | — |
| L5 | D05.2 | SessionVars 与 StatementContext（N2） | 2,181 | 2 | — |
| L5 | D08.4 | 比较函数 | 3,368 | 2 | D01.5(10) |
| L5 | [D08.8](D08.8-cast/spec.md) | CAST（2026-11-11：`builtin_convert_charset.go`——to_binary/from_binary、`HandleBinaryLiteral`——从 D08.6 移入，CAST 到 STRING 需要它） | 4,181 | 2 | D01.5(81) |

### P2（11 块，Go 函数体 39.9k 行）

| 层 | 块 | 内容 | Go 行数 | 文件数 | 需要边界桩 / 接口的依赖（引用数 ≥ 10） |
|---|---|---|--:|--:|---|
| L1 | [D06.2](D06.2-meta/spec.md) | meta 读取（meta、structure、autoid 读取；2026-11-11 按只读读路径划分：写入 76 个归 X，316 → 240 个函数；crate `tidb-infoschema`，第 1 部分 converge 130/240） | 3,808 | 13 | — |
| L1 | D07.1 | store driver（snapshot 读） | 2,473 | 16 | — |
| L1 | [D08.3](D08.3-pushdown/spec.md) | 下推判定与 protobuf 转换（infer_pushdown、expr_to_pb、distsql_builtin；crate `tidb-expr-pushdown`。第 1 部分：infer_pushdown、expr_to_pb，converge 34/50；第 2 部分：distsql_builtin 的 `PBToExpr`） | 2,073 | 3 | D08.7(97), D08.6(77), D08.9(71), D08.10(26) |
| L1 | D14.0 | 执行统计数据结构（execdetails、runtime stats） | 1,747 | 3 | — |
| L2 | D06.4 | table 抽象（tables、index、partition 的读取路径） | 7,136 | 17 | — |
| L2 | D07.2 | coprocessor 客户端（不含 batch/MPP） | 2,306 | 5 | D15.1(16), D07.4(10) |
| L3 | D06.3 | InfoSchema 构建与缓存（v2 按 A3） | 4,396 | 10 | D06.6(11), X(10) |
| L3 | D07.3 | distsql 请求构建与结果 | 1,838 | 4 | D10.4(23), D15.1(11), X(10) |
| L4 | D05.4 | 事务上下文（sessiontxn：isolation、staleread，只读路径） | 1,931 | 14 | D10.1(12) |
| L4 | D06.5 | schema 同步与 domain（schema validator、infosync） | 5,786 | 21 | X(205), DEF.MPP(47), D11.2(45), D13.2(14) |
| L5 | D03.1 | 连接与协议（conn、conn_stmt、server、driver_tidb、internal） | 6,394 | 20 | D05.3(87), X(66), D01.10(26), D12.14(15) |

### P3（29 块，Go 函数体 77.7k 行）

| 层 | 块 | 内容 | Go 行数 | 文件数 | 需要边界桩 / 接口的依赖（引用数 ≥ 10） |
|---|---|---|--:|--:|---|
| L1 | D08.11 | 聚合函数描述（expression/aggregation） | 1,592 | 16 | EXC.B(13) |
| L1 | D10.10 | 物理属性与优化器工具（property、planner/util） | 1,745 | 12 | EXC.B(11) |
| L1 | D10.4 | ranger（区间构造） | 3,855 | 6 | — |
| L2 | D09.14 | 逻辑算子公共部分与其余逻辑算子 | 1,597 | 14 | EXC.B(32), D14.2(15) |
| L2 | D10.1 | 物理算子定义与 task（physical_plans、task、common_plans） | 6,197 | 7 | — |
| L3 | D09.0 | planbuilder 骨架、语句分发、preprocess、名字解析 | 3,850 | 10 | D14.2(50), D09.16(14), D09.17(13) |
| L3 | D10.7 | IndexMerge 路径 | 1,823 | 2 | — |
| L3 | D13.3 | hint 处理 | 2,007 | 5 | — |
| L4 | D09.13 | 优化器驱动、规则框架、其他规则、函数依赖 | 4,895 | 24 | D13.2(14), D13.1(12), D04.1(10) |
| L4 | D10.8 | 点查快速路径（point_get_plan） | 2,100 | 1 | — |
| L5 | D09.11 | TopN / Limit 下推与推导 | 538 | 13 | — |
| L5 | D09.12 | 分区裁剪（仅 dynamic，B1） | 2,202 | 3 | — |
| L5 | D09.3 | join 构建与逻辑 join/apply 算子 | 2,264 | 3 | — |
| L5 | D09.4 | 聚合、窗口、only_full_group_by 构建与逻辑算子 | 3,160 | 4 | — |
| L5 | D09.5 | 集合运算、CTE、Expand | 931 | 6 | — |
| L5 | D10.5 | 代价模型 v2 | 1,195 | 2 | EXC.B(43) |
| L5 | D11.1 | 统计结构：直方图、TopN、表/列/索引统计（v1 专用部分按 B2） | 4,961 | 16 | D15.1(13) |
| L6 | D09.1 | SELECT 构建：数据源、投影、选择、排序、limit、通配符 | 2,208 | 1 | — |
| L6 | D09.10 | join 重排与 join 消除 | 3,487 | 8 | — |
| L6 | D09.2 | 表达式改写（expression_rewriter，AST → Expression） | 2,835 | 2 | — |
| L6 | D09.6 | 子查询与去关联 | 1,012 | 16 | — |
| L6 | D09.7 | 列裁剪与投影消除（P0 设计试点 spike 的对象，正式移植仍在 P3） | 772 | 22 | D06.6(12) |
| L6 | D09.8 | 谓词下推、谓词简化、外连接转内连接、约束 | 886 | 20 | — |
| L6 | D09.9 | 聚合相关规则（下推、消除、skew、max/min） | 1,394 | 4 | — |
| L6 | D10.6 | 基数估算（cardinality） | 3,526 | 30 | — |
| L6 | D10.9 | 后处理：resolve indices、plan → pb、EXPLAIN 输出、plan 编码 | 4,516 | 9 | D14.2(22) |
| L6 | D11.2 | 统计信息加载、缓存与同步加载（缓存按 A3） | 3,055 | 21 | X(41) |
| L7 | D10.2 | FindBestTask 与访问路径 | 5,886 | 23 | — |
| L8 | D10.3 | ExhaustPhysicalPlans（join/agg/sort 等物理实现枚举） | 3,174 | 19 | — |

### P4（14 块，Go 函数体 47.3k 行）

| 层 | 块 | 内容 | Go 行数 | 文件数 | 需要边界桩 / 接口的依赖（引用数 ≥ 10） |
|---|---|---|--:|--:|---|
| L1 | D01.10 | 落盘容器与磁盘 chunk（按 A1） | 1,102 | 4 | D15.1(40) |
| L1 | D12.0 | 执行器框架与构建骨架（exec 接口、builder 分发、adapter、compiler） | 4,537 | 17 | D12.14(52), X(28), D14.2(17), D12.12(15) |
| L1 | D12.4 | 聚合函数实现（aggfuncs） | 6,702 | 26 | D01.5(20) |
| L2 | D05.3 | 会话执行主流程（session.go、tidb.go，只读部分；PoC 会话先保留） | 6,780 | 12 | X(85), D04.1(46), D13.1(17), D15.1(15) |
| L2 | D12.1 | 读取类执行器（table/index reader、index lookup、point get、union scan） | 6,633 | 9 | D15.1(22) |
| L2 | D12.2 | 基础算子（projection、selection、limit、dual、max_one_row、union） | 1,823 | 5 | D15.1(45) |
| L2 | D12.3 | 聚合执行器框架（hash/stream agg） | 1,959 | 8 | D15.1(63) |
| L2 | D12.5 | 排序与 TopN（sortexec） | 2,763 | 12 | D15.1(69) |
| L2 | D12.6 | hash join（v1/v2）与 joiner | 7,126 | 18 | D15.1(52) |
| L2 | D12.8 | merge join | 395 | 2 | D15.1(12) |
| L2 | D12.9 | window | 903 | 3 | — |
| L3 | D12.10 | apply、CTE、shuffle、expand | 1,707 | 6 | D15.1(19) |
| L3 | D12.11 | IndexMerge reader | 1,893 | 2 | D15.1(22) |
| L3 | D12.7 | index join 系列 | 2,933 | 4 | D15.1(42) |

### P5（13 块，Go 函数体 62.7k 行）

| 层 | 块 | 内容 | Go 行数 | 文件数 | 需要边界桩 / 接口的依赖（引用数 ≥ 10） |
|---|---|---|--:|--:|---|
| L1 | D03.2 | HTTP status 接口与 handler | 3,445 | 12 | X(46) |
| L1 | D04.1 | 权限检查与认证。注：`crates/mysql-protocol` 中 PoC 手写的 scramble（`auth.rs`）要换成 `parser/auth` 认证插件（`mysql_native_password.go`、`caching_sha2.go`、`tidb_sm3.go`）的忠实移植（D02.3 spec §2） | 3,557 | 8 | D01.5(20) |
| L1 | D06.6 | infoschema 内存表定义与 cluster 表 | 1,336 | 7 | — |
| L1 | D08.6 | 字符串函数（含 like、regexp；字符集转换的 `builtin_convert_charset.go` 已移到 D08.8） | 8,001 | 9 | — |
| L1 | D08.7 | 时间函数 | 8,231 | 2 | — |
| L1 | D08.9 | 控制流、信息、杂项、加密、grouping 函数 | 7,958 | 10 | — |
| L1 | D09.16 | SHOW / ADMIN / SET / SIMPLE / EXPLAIN 等语句的计划构建 | 1,269 | 1 | D09.17(12) |
| L1 | D13.2 | SQL binding | 1,478 | 9 | — |
| L1 | D14.2 | slow log、stmt summary、topsql、trace | 5,881 | 25 | — |
| L1 | D15.2 | resource group / runaway | 2,057 | 16 | — |
| L2 | D10.11 | 内存表谓词提取（memtable extractor） | 2,264 | 7 | — |
| L2 | D12.12 | SHOW、EXPLAIN、SET、PREPARE/EXECUTE、SIMPLE（只读语句） | 8,723 | 18 | X(56), D12.14(17), D15.1(10) |
| L3 | D12.13 | infoschema / 内存表 / slow query / inspection 读取 | 8,544 | 10 | D15.1(15) |

### 不在 P1–P5 的块

| 块 | 状态 | 内容 | Go 行数 |
|---|---|---|--:|
| D00.3 | ON-DEMAND | 通用工具（util/*）：不单独排期，被已排期的块用到时整文件移植（宪章 4.4） | 11,654 |
| D01.5 | EXC | JSON 类型（例外 A4） | 2,349 |
| D02.1 | DONE | AST 节点与 Restore | 12,501 |
| D02.2 | DONE | parser 运行时（lexer、语法动作、hint parser、format、auth、model） | 4,257 |
| D07.4 | EXC | coprocessor cache（A3） | 158 |
| D08.10 | EXC | JSON 函数（A4） | 2,812 |
| D09.15 | X | DML 计划构建（insert/update/delete/load，写路径） | 2,091 |
| D09.17 | X | ANALYZE / DDL / IMPORT / BRIE / 分裂 region 的计划构建（由 Go TiDB 承担） | 2,740 |
| D11.3 | X | 收集与维护：analyze、autoanalyze、存储写入、锁统计（由 Go TiDB 承担） | 0 |
| D12.14 | X | 写路径执行器、DDL、ANALYZE、IMPORT、BRIE、admin（由 Go TiDB 承担或后续阶段） | 15,221 |
| D13.1 | EXC | Plan Cache（A3） | 2,369 |
| D14.1 | DONE | metrics | 2,357 |
| [D15.1](D15.1-memory/spec.md) | EXC | 内存统计与配额（A1） | 819 |
| DEF.MPP | DEFER | TiFlash / MPP 计划与执行 | 5,296 |
| EXC.B | EXC | 废弃或非默认路径：cascades（旧/新）、代价模型 v1、静态分区 | 5,158 |
| X | X | DDL、lightning、BR、disttask、TTL、import、mock store、测试工具等 | 141,776 |

### TiKV 客户端（L3，client-rust fork）

TiKV 客户端不按函数移植：client-go（约 1.7 万行 Go）由我们的 client-rust fork 替代，见 [ADR 0008](../docs/adr/0008-client-rust-replaces-client-go.md)。工作量按 [client-go 对等对照表](D07.1-store/client-go-parity.md) §10 的项计，**不计入上面各阶段的"Go 函数体行数"**：

| 阶段 | 项 |
|---|---|
| D07.1 / D07.2 接入真实集群之前 | P0-1 退避预算、P0-2 RPC 超时、P0-3 接收消息上限（小）；锁解析 R1–R12 已完成（#101） |
| P2 之前 | 按时间计的 Backoffer、store 存活状态与后台刷新、region cache 的 TTL / `CurrentRegions` / 批量加载、`max_execution_duration_ms` 与读超时、HTTP/2 keepalive（各为小到中） |
| 按需 | replica read（中到大）、BatchCommands（大，先测量）、region bucket（中，随 D07.2）、RU 拦截器（大，见 N10） |

## 5. 开工前的前置条件（P0 剩余工作）

1. **基础设计 spec 确认**：N1（计划树/表达式树表示）、N2（会话状态拆分）、N3（并发与取消）、A1（内存模型）、A4（JSON），以及 **N4**（表达式求值框架：D08.1 和全部内置函数的代码形态取决于它）、**N8**（系统变量注册表：D05.1、D05.2 依赖它）、**N5a**（根节点内存中的列格式：D01.7 依赖它）。"定稿"包括验收指标的 Go 基线已经实测、数字已回填。
2. **全局映射规则表**（[docs/porting/mappings.md](../docs/porting/mappings.md)）与基础 spec 一起定稿，对应的 lint 配置进入 CI。
3. **两个试点**（方法论第 8 节）：流程试点 **D01.2 MyDecimal**，之后做一次上游同步演练；设计试点 **D09.7 列裁剪**（spike，不合入，不计覆盖率），产出 N1 的修订和 plan 模板中的固定写法。
4. **工具**：govec（Go 测试用例表 → 输入输出向量；已完成第一版，tracker PR #1）、来源标注抽取与 lint（D01.2 试点的 `scripts/converge.py` 迁入 tracker，并输出 `rs_provenance.tsv`）、并排对照生成、`go_renames.tsv`。
5. **tracker 支持块**：`blocks.tsv`（块定义与阶段）、`func_blocks.tsv`（函数 → 块）、块依赖表（已完成）；按块统计移植覆盖率、测试行数和测试归属的报表；`stubs.tsv` 记录桩的三类。
6. **PoC 代码定性**：逐文件标注 `glue: poc`，列出各自将被哪个块替换。
7. **crate 分层**（[N0](exceptions/N0-crate-layering/spec.md)）：每个块所在的 crate 和 crate 之间的依赖方向以仓库根目录的 `layers.toml` 为准，CI 检查。

## 6. 维护

- 划分规则在 [tidb-rs-tracker](https://github.com/dulao5/tidb-rs-tracker) 中维护（`scripts/blocks/rules.py`，数据 `data/blocks.tsv`、`data/func_blocks.tsv`、`data/block_edges.tsv`），由脚本生成本文的表格。调整划分时先改规则，再重新生成。
- 每个块开工时，在 `specs/<块 ID>-<名称>/` 下建立 spec/plan/tasks，并在本文中链接。
