# D12.4 聚合函数实现（executor/aggfuncs）：spec

状态：已确认（2026-11-25），只写 spec（#336）。第 9 节记录各决定点的结论：落盘格式和 APPROX_PERCENTILE 的随机数由负责人决定，其余由维护者按宪章第九条（谁来决定，#374）决定。下一步按 §3 的部分拆成实现任务。

块：D12.4（ROADMAP P4，聚合函数实现；Go 约 6,700 行，26 个文件），crate `tidb-executor`（N0 rank 14，`layers.toml` 已登记）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/executor/aggfuncs/`，另有说明的除外。

相关：[N3 / A1 并发与内存](../exceptions/N3-A1-concurrency-memory/spec.md)（§3 内存记账、落盘）、[N5a 列格式](../exceptions/N5a-chunk/spec.md)、[A4 JSON](../exceptions/A4-json/spec.md) §1.2（本块的 JSON 用法）、[mappings.md](../../docs/porting/mappings.md)、[D12.1 spec](../D12.1-read-executors/spec.md)（执行器块的写法）。同批的兄弟 spec：聚合函数描述（D08.11，#333，`specs/D08.11-aggregation/spec.md`）、聚合相关规则（D09.9，#334，`specs/D09.9-agg-rules/spec.md`）、聚合执行器框架（D12.3，#335，与本 spec 同时在写，这里只按块号和 issue 号引用）。

## 1. 这个块在 Go 里做什么

### 1.1 内容

`pkg/executor/aggfuncs` 是聚合函数和窗口函数在执行期**真正计算**的部分。规划器给出聚合函数描述（`aggregation.AggFuncDesc`，D08.11），执行器的 builder 调用本包的 `Build` / `BuildWindowFunctions`，按函数名、模式（Complete / Partial1 / Partial2 / Final / Dedup）、是否 DISTINCT、参数或返回值的求值类型选出一个具体实现。每个实现对外是同一个接口：

| Go 接口（`aggfuncs.go`） | 方法 | 用途 |
|---|---|---|
| `AggFunc` | `AllocPartialResult`、`ResetPartialResult`、`UpdatePartialResult`、`MergePartialResult`、`AppendFinalResult2Chunk`，以及嵌入的 `serializer`（`SerializePartialResult`、`DeserializePartialResult`） | 为每个分组分配一份"部分结果"（`PartialResult`，即 `unsafe.Pointer`），逐批输入行更新它，并行时合并两份，最后把结果写进输出 chunk；落盘时把部分结果序列化进 chunk 再读回。除 `Reset` 外都返回 `memDelta`（内存增量的估算值） |
| `SlidingWindowAggFunc` | `Slide` | 窗口帧滑动时只加减进出帧的行 |
| `MaxMinSlidingWindowAggFunc` | `SetWindowStart` | 滑动窗口的 max / min 记录窗口起点 |

| Go 文件 | 内容 |
|---|---|
| `aggfuncs.go` | 接口、`baseAggFunc`（默认的 `Merge` 和空的序列化）、`AggPartialResultMapper`（`hack.MemAwareMap[string, []PartialResult]`，hash agg 的分组表类型）、`deserializePartialResultCommon` |
| `builder.go` | `Build`、`BuildWindowFunctions` 和各个 `buildXxx`：按描述选实现 |
| `func_count.go`、`func_sum.go`、`func_avg.go`、`func_first_row.go`、`func_max_min.go`、`func_bitfuncs.go` | COUNT、SUM、AVG、FIRST_ROW、MAX / MIN、BIT_AND / OR / XOR，按求值类型各一个实现（如 `countOriginal4Int`、`sum4Decimal`、`firstRow4String`、`maxMin4Uint`），含 DISTINCT 的 SUM / AVG |
| `func_count_distinct.go` | COUNT(DISTINCT)（单参数按类型、多参数编码成字节串去重）、APPROX_COUNT_DISTINCT（自带的开放寻址哈希集合，`partialResult4ApproxCountDistinct`） |
| `func_group_concat.go` | GROUP_CONCAT：普通、DISTINCT、ORDER BY（`topNRows` 堆，按 `group_concat_max_len` 截断）、DISTINCT + ORDER BY；截断时的 warning / 错误 |
| `func_percentile.go` | APPROX_PERCENTILE（收集全部值，用 `util/selection` 选第 k 小） |
| `func_varpop.go`、`func_varsamp.go`、`func_stddevpop.go`、`func_stddevsamp.go` | VAR_POP / VAR_SAMP / STDDEV_POP / STDDEV_SAMP（含 DISTINCT） |
| `func_json_arrayagg.go`、`func_json_objectagg.go` | JSON_ARRAYAGG、JSON_OBJECTAGG |
| `func_value.go`、`func_rank.go`、`row_number.go`、`func_lead_lag.go`、`func_percent_rank.go`、`func_ntile.go`、`func_cume_dist.go` | 窗口函数：FIRST_VALUE / LAST_VALUE / NTH_VALUE、RANK / DENSE_RANK、ROW_NUMBER、LEAD / LAG、PERCENT_RANK、NTILE、CUME_DIST |
| `func_max_min.go`（后半） | 窗口中 max / min 的滑动实现（`MinMaxDeque` 单调队列，`maxMin4XxxSliding`） |
| `spill_serialize_helper.go`、`spill_deserialize_helper.go` | 并行 hash agg 落盘时各类部分结果的序列化 / 反序列化（基于 `util/serialization`） |

本包没有 goroutine、channel、锁和后台任务（`grep 'go func\|chan '` 为空）；唯一的并发原语是 GROUP_CONCAT 的截断标志 `atomic.CompareAndSwapInt32`（`func_group_concat.go:76`），因为同一个 `AggFunc` 对象被并行 hash agg 的所有 partial worker 共用（§5.2）。

### 1.2 调用方（tracker `data/block_edges.tsv`，指向 D12.4 的引用数）

| 块 | 引用数 | 调用什么 |
|---|--:|---|
| D12.3 聚合执行器框架（hash / stream agg，#335） | 56 | `pkg/executor/builder.go:2069-2131`：`Build`；group_concat 的 `GetTruncated` / `SetTruncated`（`builder.go:2084-2089`，鸭子类型）；`aggregate/`：`AllocPartialResult`、`UpdatePartialResult`、`MergePartialResult`（`agg_hash_final_worker.go:94`）、`AppendFinalResult2Chunk`、`ResetPartialResult`、`NewAggPartialResultMapper`、`NewSerializeHelper`、`SerializePartialResult`（`agg_hash_partial_worker.go:352`）、`DeserializePartialResult`（`agg_spill.go:273`） |
| D12.9 其他执行器（window、pipelined window） | 37 | `pkg/executor/builder.go:5297`：`BuildWindowFunctions`；`window.go`、`pipelined_window.go`：`Slide`、`SetWindowStart` 等 |

sysbench `oltp_read_only` 的 `SELECT SUM(k) ... WHERE id BETWEEN ? AND ?` 和 `SELECT DISTINCT c ... ORDER BY c` 在本块只用到 SUM（`sum4Decimal`，Complete 或下推后的 Final 模式）、FIRST_ROW（`firstRow4String` 等）和下推后 Final 模式的 COUNT（`countPartial`）。链路：D09.4 建描述 → D09.9 规则 → D10.1、D10.3 物理计划 → D12.3 执行器 → 本块。目前这两条语句走 PoC（D09.14 spec §1）。

## 2. 依赖

### 2.1 已移植（逐个用 `grep '// go: <Symbol>'` 核对过，所在的块不一定整块完成）

| 依赖 | 用到的 | Rust 位置 |
|---|---|---|
| D01.7 chunk | `Chunk.Append*`（含 `AppendMyDecimal`、`AppendJSON`、`AppendVectorFloat32`、`AppendEnum`、`AppendSet`）、`Row.Get*`、`Chunk.Column`、`GetCompareFunc` | `crates/chunk` |
| D08.1 表达式框架 | `Expression.Eval*`（逐行）、`GetType`、`StringWithCtx`、`Constant`、`NewNull` | `crates/expression` |
| D08.12 表达式工具 | `GetUint64FromConstant`（nth_value、ntile、lead / lag） | `crates/expression/src/util.rs` |
| D00.1 上下文 | `EvalContext`（`AggFuncUpdateContext` 的别名）、`ExprContext`（`AggFuncBuildContext`：`GetEvalCtx`、`GetWindowingUseHighPrecision`、`GetGroupConcatMaxLen`）、`TypeCtx` 的 flag 与 warning | `crates/sessionctx/src/exprctx` |
| D01.1 / D01.2 / D01.3 / D01.4 类型 | `Datum`（`Compare`、`ConvertTo`、`Clone`）、`MyDecimal`（`DecimalAdd/Sub/Mul/Div`、`Round`、`Compare`、`ToHashKey`、`ToFloat64`、`FromInt`）、`Time` / `Duration` 的比较、`CompareString`、Enum / Set、`ToString` | `crates/types` |
| D01.5 JSON（A4） | `CreateBinaryJSONWithCheck`、`BinaryJSON.Copy`、`HashValue`、`CompareBinaryJSON` | `crates/types/src/json` |
| D01.6 向量 | `VectorFloat32.Compare`、`Clone`、`SerializeTo` | `crates/types/src/vector*.rs` |
| D01.8 codec | `EncodeCompactBytes`、`EncodeBytes`、`EncodeUvarint` | `crates/codec` |
| D01.9 collation | `GetCollator`、`Collator.Key` | `crates/types/src/collate` |
| D00.2 / D02.x | 错误（`ErrCutValueGroupConcat`、`plannererrors.ErrInternal`、JSON 的三个错误）、`mysql` 类型常量、`FieldType`、`ast` 函数名常量 | `crates/base`、`crates/parser`、`crates/expression/src/errors.rs` |
| D15.1 内存（A1） | tracker、`TrackedAlloc` | `crates/memory` |
| D00.3 `util/stringutil`、`util/hack/map_abi.go`（部分） | `stringutil.Copy`；`MemAwareMap` 的 `Set`、`Get`、`Len`、`Init` | `crates/types/src/stringutil.rs`、`crates/kv/src/hack.rs` |
| Go 标准库 `sort.Sort` | GROUP_CONCAT ORDER BY 的最终排序（`func_group_concat.go:413`） | `crates/base/src/gostd/sort.rs` |

### 2.2 未移植、要先补或写成边界的

| 依赖 | 用到的 | 处理 |
|---|---|---|
| **D08.11 聚合函数描述**（#333，spec 待确认） | `AggFuncDesc`（`Name`、`Args`、`RetTp`、`Mode`、`HasDistinct`、`OrderByItems`）、`AggFunctionMode` 常量 | `Build` 的输入，**硬前提**：第 A 部分在 D08.11 第 A 部分合并后开工，不写桩 |
| D00.3 `util/set/set_with_memory_usage.go`（及其用到的 `set.go`、`float64_set.go`、`int_set.go`、`string_set.go` 中的类型） | DISTINCT 的 SUM / AVG / COUNT / VAR 用的三种集合 | 叶子工具文件，按宪章第四条 4 在第 A 部分整文件移植（已移植：`crates/kv/src/set/`，集合按分配器参数化，见 §5.3） |
| D00.3 `util/hack/map_abi.go`（其余部分） | `MemAwareMap.SetExt`、`Init` 传入已有 map、`Bytes`、`M` 的遍历 | 补进现有的 `crates/kv/src/hack.rs`（一型一译，不另写一份），第 A 部分（`AggPartialResultMapper`）和第 B1 部分（JSON_OBJECTAGG）。第 A 部分已补 `Empty`、`Exist`、`SetExt`，并把 `MemAwareMap` 的底层 map 换成按分配器参数化的 `hashbrown::HashMap`（默认全局分配器，原有调用方不变），DISTINCT 的集合由此用 `TrackedAlloc`（§5.3） |
| D00.3 `util/selection/selection.go` | APPROX_PERCENTILE 的 `Select` | 第 B2 部分整文件移植；随机主元见 §5.6 和 §9 第 7 条 |
| D00.3 `util/serialization`（3 个文件） | 落盘序列化 | 第 S 部分整文件移植 |
| Go 标准库 `container/heap` | GROUP_CONCAT ORDER BY 的 `topNRows`（`func_group_concat.go:377`、`:388`、`:392`） | 第 B1 部分按 `go-std:` 移植到 `crates/base/src/gostd/`（堆的调整顺序决定并列行中谁被截掉，必须与 Go 相同） |
| 第三方 `github.com/dgryski/go-farm` | APPROX_COUNT_DISTINCT 的 `farm.Hash64`（`func_count_distinct.go:810`） | 第 B2 部分按 `go-dep:` 移植（或用现成的 Rust 实现，但必须由 govec 逐位验证） |
| D01.10 落盘容器（`chunk.DataInDiskByChunks`） | 本块不直接调用；D12.3 的落盘路径（`agg_spill.go`、`agg_hash_partial_worker.go:336-365`）用它写盘 | 本块不写桩，见 §5.4：落盘入口 `blocked-by:D01.10` 由 D12.3 负责（#335） |
| D12.9 窗口执行器、D09.4 窗口构建、D08.11 第 C 部分（窗口函数描述） | 窗口函数的唯一调用方和描述 | 第 W 部分排在它们之前或同时，不写桩 |
| N6（hash agg / join / sort 的数据结构，例外"原则同意"，spec 未写，要求 P4 之前定） | 部分结果在分组表中的存放方式 | 本 spec 只定 `PartialResult` 的承载（§9 第 2 条：不等 N6）；分组表本身属于 D12.3 / N6 |

### 2.3 与兄弟 spec 待定事项的关系

- **D08.11 决定点 4（计划节点按值持有 `AggFuncDesc`，还是 `Arc<AggFuncDesc>`）**：本块只在 `Build` 时借用描述（`&AggFuncDesc`），复制出 `Args`（`Arc` 共享的表达式）和 `RetTp`（`Arc<FieldType>`），之后不再引用描述。两个选项下本块的签名都是 `build(ctx, desc: &AggFuncDesc, ordinal)`，选 `Arc` 时调用方传 `&*arc`。所以本 spec **依赖**这一点的结论（类型定义在 D08.11），但不需要它来定本块的写法，也不替负责人决定它。
- **D08.11 决定点 2（`ByItems` 的位置）**：GROUP_CONCAT ORDER BY 用 `[]*util.ByItems`（`func_group_concat.go:53`）。`tidb-executor`（rank 14）在 `tidb-planner` 和 `tidb-expression` 之上，两种结论都能直接引用。
- **D12.3（#335）**：hash agg 的分组表、partial / final worker、落盘的触发与写盘、`memDelta` 的消费都在 D12.3。本块与它的接口就是 §5.1 的 trait。决定点 1、2、6 也影响 D12.3 的写法，已与 #335 一起定（§9）。

## 3. 函数清单的划分

tracker `data/func_blocks.tsv` 中 block 为 D12.4 的函数共 **663** 个，全部在 `pkg/executor/aggfuncs/`。

**归 X：0 个。** 聚合函数和窗口函数都是 SELECT 的功能；落盘也是只读查询在内存超限时的行为（`tidb_enable_tmp_storage_on_oom`、`tidb_enable_parallel_hashagg_spill`），不属于写路径。**豁免：0 个**（只有一种例外：决定点 1 定为 B，§3.6 的 8 个纯记账函数登记为 A1 例外）。

按依赖分成五个部分（类型、常量随所在部分移植）：

| 部分 | 内容 | 函数数 | 前提 | 用户 |
|---|---|--:|---|---|
| A 核心 | 接口与 `Build`；COUNT、SUM、AVG、FIRST_ROW、MAX / MIN（非滑动）、BIT_* | 189 | D08.11 第 A 部分；`util/set` | D12.3，sysbench |
| B1 其他常用聚合 | COUNT(DISTINCT)、GROUP_CONCAT、VAR / STDDEV、JSON_*AGG | 102 | A；`container/heap`；`go_fma_sites` 登记表补本包（§5.6） | D12.3 |
| B2 近似聚合 | APPROX_COUNT_DISTINCT、APPROX_PERCENTILE | 82 | A；`util/selection`；`go-farm` | D12.3 |
| W 窗口 | `BuildWindowFunctions`、各窗口函数、滑动 max / min、所有 `Slide` | 128 | A；D08.11 第 C 部分 | D12.9 |
| S 落盘序列化 | 两个 helper 文件，各实现的 `Serialize` / `Deserialize` / `deserializeForSpill` | 162 | A、B1；`util/serialization`；与 D12.3 的落盘部分同时（决定点 4） | D12.3 落盘 |
| 合计 | | 663 | | |

### 3.1 第 A 部分：核心（189）

| Go 文件 | 数 | 函数 |
|---|--:|---|
| `aggfuncs.go` | 3 | `NewAggPartialResultMapper`、`NewAggPartialResultMapperWithCap`、`baseAggFunc.MergePartialResult` |
| `builder.go` | 9 | `Build`、`buildCount`、`buildSum`、`buildAvg`、`buildFirstRow`、`buildMaxMin`、`buildBitOr`、`buildBitXor`、`buildBitAnd` |
| `func_count.go` | 13 | `baseCount` 的 Alloc / Reset / AppendFinal；8 个 `countOriginal4Xxx.UpdatePartialResult`；`countPartial` 的 Update / Merge |
| `func_sum.go` | 18 | `baseSum4Float64`、`sum4Decimal`、`sum4DistinctFloat64`、`sum4DistinctDecimal` 的 Alloc / Reset / Update / Merge / AppendFinal |
| `func_avg.go` | 20 | `baseAvgDecimal`、`baseAvgFloat64`、`avgOriginal4*`、`avgPartial4*`、两个 DISTINCT 实现 |
| `func_first_row.go` | 55 | 11 个类型（Int、Float32、Float64、Decimal、Time、Duration、String、JSON、VectorFloat32、Enum、Set）各 5 个方法 |
| `func_max_min.go` | 60 | 12 个非滑动类型的 Alloc / Reset / Update / Merge / AppendFinal（VectorFloat32 共 5 个） |
| `func_bitfuncs.go` | 11 | `baseBitAggFunc` 的 Alloc / Reset / AppendFinal；三个函数的 Update / Merge；`bitAndUint64` 的 Alloc / Reset |

`Build` 是一个按函数名分支的函数（`builder.go:37-77`）。第 A 部分移植它的全部分支顺序；指向第 B1、B2 部分的 9 个函数名（group_concat、var_pop、stddev_pop、json_arrayagg、json_objectagg、approx_count_distinct、approx_percentile、var_samp、stddev_samp）在那两部分合并之前返回临时桩，见 §4 和 §9 第 3 条。

### 3.2 第 B1 部分：其他常用聚合（102）

- `func_count_distinct.go`（31）：5 个单参数 `countOriginalWithDistinct4Xxx` 和多参数 `countOriginalWithDistinct` 的 Alloc / Reset / Update / AppendFinal；`evalAndEncode`、`appendInt64`、`appendFloat64`、`appendDecimal`、`WriteTime`、`appendTime`、`appendDuration`。
- `func_group_concat.go`（35）：`baseGroupConcat4String`（AppendFinal、`handleTruncateError`、`truncatePartialResultIfNeed`）、四种实现、`topNRows` 的堆方法与 `tryToAdd`、`reset`、`concat`、`GetDatumMemSize`。
- `func_varpop.go`、`func_varsamp.go`、`func_stddevpop.go`、`func_stddevsamp.go`（17）：`calculateIntermediate`、`calculateMerge` 和各实现。
- `func_json_arrayagg.go`、`func_json_objectagg.go`（12）：含 `getRealJSONValue`、`getValMemDelta`。
- `builder.go`（7）：`buildGroupConcat`、`buildVarPop`、`buildStdDevPop`、`buildVarSamp`、`buildStddevSamp`、`buildJSONArrayagg`、`buildJSONObjectAgg`。

### 3.3 第 B2 部分：近似聚合（82）

- `func_count_distinct.go`（29）：`NewPartialResult4ApproxCountDistinct`、`intHash64`、`partialResult4ApproxCountDistinct` 的 18 个方法（哈希集合的插入、扩容、合并、`Serialize`、`readAndMerge`）、`baseApproxCountDistinct` 与四种模式的实现。
- `func_percentile.go`（50）：`percentile`、`basePercentile`、五种 `partialResult4PercentileXxx` 的 `Len` / `Less` / `Swap` / `MemSize`、五种 `percentileOriginal4Xxx`。
- `builder.go`（3）：`buildApproxCountDistinct`、`getEvalTypeForApproxPercentile`、`buildApproxPercentile`。

### 3.4 第 W 部分：窗口（128）

- `builder.go`（13）：`BuildWindowFunctions`、`buildRank`、`buildRowNumber`、`buildFirstValue`、`buildLastValue`、`buildCumeDist`、`buildNthValue`、`buildNtile`、`buildPercentRank`、`buildLeadLag`、`buildLead`、`buildLag`、`buildMaxMinInWindowFunction`。
- `func_value.go`（31）、`func_rank.go`（6，含 `buildRowComparer`、`rowComparer.compareRows`）、`row_number.go`（5）、`func_lead_lag.go`（5）、`func_percent_rank.go`（4）、`func_ntile.go`（4）、`func_cume_dist.go`（4）。
- `func_max_min.go`（43）：`MinMaxDeque` 的 9 个方法、`NewDeque`、`windowInfo.SetWindowStart`、8 个 `maxMin4XxxSliding` 的方法。
- 第 A 部分类型上的 `Slide`（13）：`countOriginal4Xxx`（8）、`sum4Float64`、`sum4Decimal`、`avgOriginal4Decimal`、`avgOriginal4Float64`、`bitXorUint64`。它们在第 A 部分的文件里，但只有窗口执行器调用，trait `SlidingWindowAggFunc` 也随本部分引入。

### 3.5 第 S 部分：落盘序列化（162）

`spill_serialize_helper.go`（33）、`spill_deserialize_helper.go`（33）、`aggfuncs.go` 的 `baseAggFunc.SerializePartialResult`、`baseAggFunc.DeserializePartialResult`、`deserializePartialResultCommon`（3），以及 count、sum、avg、first_row、max_min、bitfuncs、group_concat、json_arrayagg、json_objectagg 各实现的 `SerializePartialResult`、`DeserializePartialResult`、`deserializeForSpill`（93）。

### 3.6 只用于内存估算的函数（8）

`partialResult4PercentileXxx.MemSize`（5，B2）、`partialResult4ApproxCountDistinct.MemUsage`（B2）、`getValMemDelta`（B1）、`GetDatumMemSize`（B1）只用来算 `memDelta`。决定点 1 定为 B：不移植，tracker 登记为 A1 例外（理由：A1 删除估算公式），代码中不留桩。

## 4. 边界桩

| Rust 位置 | Go 函数 | 类型 | 调用时 |
|---|---|---|---|
| `aggfuncs/builder.rs` `build_not_ported` | 第 B1、B2 部分合并前，`Build` 中指向它们的 9 个函数名的分支，以及 `buildCount` 中 DISTINCT 的分支（complete / partial1 模式的 `countOriginalWithDistinct*`，第 B1 部分） | 临时 `blocked-by:D12.4`（§9 第 3 条） | 返回 `blocked("D12.4", …)`，路由器回退到 PoC。这些分支共用一个桩函数，所以棘轮只 +2 |
| — | 落盘入口 | 不在本块 | D12.3 在触发落盘之前返回 `blocked-by:D01.10`（#335）；本块在第 S 部分之前不定义序列化方法，所以没有可以被调到的空实现（§5.4） |
| — | 窗口函数（第 W 部分） | 不写 | 唯一调用方是 D12.9，它移植之前到不了 |

这个临时桩让 `executor` 区域的桩棘轮（`scripts/check_stub_ratchet.py`）上升 2，属于维护者批准的例外（§9 第 3 条）；第 A 部分的 PR 正文逐条列出净增的标注，第 B 部分合并时移除。

## 5. Go → Rust 的结构映射

### 5.1 trait 与部分结果

模块放在 `crates/executor/src/aggfuncs/`，每个 Go 文件一个 Rust 文件（`aggfuncs.rs`、`builder.rs`、`func_count.rs` …）。

| Go | Rust | 说明 |
|---|---|---|
| `AggFunc` 接口 | `pub trait AggFunc: Send + Sync`，方法照 Go 改 snake_case | 同一个对象被多个 partial worker 共用（`pkg/executor/aggregate/agg_hash_executor.go:312` 把同一个 `e.PartialAggFuncs` 交给每个 partial worker），所以方法取 `&self`，类型要 `Sync` |
| `PartialResult`（`unsafe.Pointer`），各实现强转成自己的结构 | `pub type PartialResult = Box<dyn Any + Send>`，各实现 `downcast_mut::<PartialResult4Xxx>()` | §9 第 2 条。`Send`：部分结果随分组表在 partial worker 和 final worker 之间传递（`agg_hash_final_worker.go:84-91` 直接把 partial worker 的结果放进 final 的表）。**注意**：partial 与 final 是两个不同的 `AggFunc` 对象（`Split` 后分别 `Build`），解释同一份部分结果；Go 靠构造保证结构一致，Rust 的 `downcast` 不一致时 panic，单测对 `Split` 出的每一对检查一次 |
| `AllocPartialResult() (PartialResult, int64)` | `fn alloc_partial_result(&self) -> PartialResult` | 决定点 1 定为 B（§9 第 1 条）：五个方法都不返回 `memDelta`（L2，A1） |
| `UpdatePartialResult(ctx, []chunk.Row, pr) (int64, error)` | `fn update_partial_result(&self, ctx: &dyn EvalContext, st: &EvalState, rows_in_group: &[Row<'_>], pr: &mut PartialResult) -> Result<()>`（`st` 是表达式的每次执行状态，即关联列的值，N4 §3.4：Rust 的表达式求值都要它，调用方 D12.3 / D12.9 传入执行器自己的状态） | Go 出错时也返回非零的 `memDelta`（如 `func_group_concat.go:126` 的 `defer`），但所有调用方出错时都丢弃它（`agg_hash_executor.go:780-783`、`agg_hash_partial_worker.go:275-278`、`agg_stream_executor.go:184-187`）；决定点 1 定为 B 后不再返回它（L2，理由写在 trait 上） |
| `MergePartialResult(ctx, src, dst)` | `fn merge_partial_result(&self, ctx, src: &mut PartialResult, dst: &mut PartialResult) -> Result<()>`，默认实现是 `baseAggFunc` 的 `Ok(())` | `src` 可写：Go 会改它（APPROX_PERCENTILE `*p1 = nil`，`func_percentile.go:164`）或把它的缓冲区直接交给 `dst`（GROUP_CONCAT `p2.buffer = p1.buffer`，`func_group_concat.go:168`）。Rust 用 `std::mem::take` 移走，Go 的调用方合并后都不再用 `src`，结果相同 |
| `AppendFinalResult2Chunk(ctx, pr, chk)` | `fn append_final_result2_chunk(&self, ctx, pr: &mut PartialResult, chk: &mut Chunk) -> Result<()>` | `pr` 可写：Go 会原地改（DECIMAL 的 `p.val.Round(&p.val, …)`，`func_sum.go:220`、`func_first_row.go:680`、`func_max_min.go:910`，M-ALIAS 的原地入口；APPROX_PERCENTILE 的选择会重排数组） |
| `baseAggFunc{args, ordinal, retTp}` 嵌入各实现 | `BaseAggFunc { args: Vec<ExprRef>, ordinal: isize, ret_tp: Option<Arc<FieldType>> }`，按 M-SELF 用字段加访问器 | `ret_tp` 是 `Option`：bit、var、json、窗口函数等的 builder 不设它（Go 的 nil），而 avg / sum / first_row / max_min 的 `AppendFinal` 检查 nil 并报错（`func_avg.go:97`、`:293`，`func_sum.go:214`，`func_first_row.go:674`，`func_max_min.go:904`），照抄检查。`ordinal int` 按 M-INT 用 `isize` |
| 类型断言 / 鸭子类型：`base.(type)`（`buildMaxMinInWindowFunction`）、`interface{ SetTruncated(*int32) }` | M-IFACE：`as_any()` 向下转型；GROUP_CONCAT 的 `get_truncated` / `set_truncated` 写成 trait 的可选方法（默认 `None`），`set_truncated(&mut self)` 在 builder 独占对象时调用 | Go 中 `groupConcatDistinctOrder` 没有这两个方法，断言会 panic；但带 DISTINCT 或 ORDER BY 时 hash agg 不并行（`pkg/executor/builder.go:2056-2059`），到不了。Rust 照样不给它实现 |
| `Build` 返回 nil（未知函数名、Dedup 模式、类型不匹配） | `Ok(None)` | Go 的 nil 由调用方（D12.3 / D12.9）处理，Rust 的调用方对 `None` 做 Go 对 nil 做的事 |
| `AggPartialResultMapper = *hack.MemAwareMap[string, []PartialResult]` | `MemAwareMap<Vec<u8>, Vec<PartialResult>>`（`crates/kv/src/hack.rs`），键按 M-BYTES 用字节 | 第 A 部分只移植构造函数；分组表的结构若被 N6 替换，由 D12.3 决定 |

`Slide`、`SetWindowStart`（第 W 部分）是另外两个 trait，`AggFunc` 用 `as_sliding_window()` / `as_max_min_sliding_mut()` 返回 `Option` 对应 Go 的 `agg.(SlidingWindowAggFunc)` 断言。`SetWindowStart` 改的是函数对象本身（`windowInfo.start`，`func_max_min.go:983-990`），窗口执行器独占自己的函数对象，所以取 `&mut self`。

### 5.2 并发与共享

- 本块不起 task、不用 channel（M-GO、M-CH、M-WG 不适用），也没有要放 `checkpoint` 的长循环：每次 `Update` 只处理调用方交来的一批行，KILL 和内存检查点由 D12.3 / D12.9 在调用前后放（M-KILL）。
- GROUP_CONCAT 的截断标志：Go 的 `truncated *int32` 被同一语句中 partial 和 final 两个函数对象共用（`pkg/executor/builder.go:2084-2089`），用 `atomic.CompareAndSwapInt32` 保证"一个 GROUP_CONCAT 一生只报一次截断"（`func_group_concat.go:56-60`、`:76`）。Rust 用 `Arc<AtomicI32>`，`compare_exchange(0, 1)`。不是锁，M-LOCK 不涉及。
- LEAD / LAG 的值求值器：Go 把 `valueEvaluator` 放在函数对象里（`func_lead_lag.go:31`，注释"TODO: move it to partial result when parallel execution is supported"），`AppendFinalResult2Chunk` 写它。这和 §5.1 的 `&self` 冲突。照 Go 注释的方向放进部分结果（L2，结果不变），§9 第 6 条。FIRST_VALUE / LAST_VALUE / NTH_VALUE 本来就放在部分结果里（`func_value.go:272-275`）。
- 部分结果跨 `Update` 保存行（RANK、CUME_DIST、PERCENT_RANK、LEAD / LAG 的 `[]chunk.Row`）：按 M-ROW 存 `RowHandle { Arc<Chunk>, idx }`，调用内仍用借用的 `Row<'_>`。
- GROUP_CONCAT ORDER BY 的 `topNRows` 把最近一次 `Update` 的 `sctx` 存进部分结果（`func_group_concat.go:308`、`:470`、`:574`），`AppendFinal` 排序时用它比较。Rust 不在部分结果里存上下文（生命周期），排序时用 `AppendFinal` 传入的上下文（L2；同一语句内是同一个上下文，结果相同）。

### 5.3 内存记账（A1）

Go 的每个方法返回按 Go 结构大小估算的 `memDelta`（`unsafe.Sizeof` 常量 `DefPartialResult4XxxSize`、`cap()` 的增减、`GetDatumMemSize` 等），由 D12.3 `Consume`。A1 已定"算子中的 `Consume(±delta)` 和估算公式：删除 delta 计算；正向 Consume 的位置改为 `mem.checkpoint()?`"，而且"所有大块内存（……聚合状态……）通过 `TrackedAlloc` 分配"（A1 spec §3.2 表、§3.3）。另一方面，`crates/kv/src/hack.rs` 移植 `MemAwareMap` 时保留了 Go 的估算数字。本块按 A1 做（§9 第 1 条，决定点 1 定为 B）：

- `memDelta` 不再返回，trait 方法的返回值去掉这一项（L2，按调用点在 tracker 登记为 A1 例外）；
- 部分结果中会增长的容器（DISTINCT 的集合、GROUP_CONCAT 的缓冲区与堆、APPROX_PERCENTILE 的数组、APPROX_COUNT_DISTINCT 的哈希表、JSON_*AGG 的数组与 map、窗口函数保存的行）用 `TrackedAlloc`（`Vec<_, TrackedAlloc>`、`hashbrown::HashSet<_, _, TrackedAlloc>`），分配器的 tracker 由 D12.3 / D12.9 在 `Build` 后交给函数对象：trait 方法 `set_alloc(&mut self, ColAlloc)`（默认什么也不做；DISTINCT 的 SUM / AVG 保存它，之后分配的集合用它；没调用时用全局分配器）。字符串集合的键也在同一个分配器里（`Box<[u8], A>`），所以键的字节也记账（Go 由调用方把 `len(key)` 加进 `memDelta`，如 `func_sum.go:400`、`func_avg.go:268`）。集合的移植仍按 Go 算出估算值，本块丢弃它；
- 定长的部分结果本身（`Box`）记在哪一层，与 N6 一起定（D12.3）；
- Go 的 `TestMemXxx` 按 A1 的"数值不对照"规则改写成性质测试（§6.1）。

### 5.4 落盘（不静默）

并行 hash agg 内存超限时，partial worker 把每个分组的部分结果逐个 `SerializePartialResult` 进临时 chunk，再用 `chunk.DataInDiskByChunks`（D01.10）写盘；恢复时 `DeserializePartialResult` 读回并 `MergePartialResult`（`agg_hash_partial_worker.go:336-365`，`agg_spill.go:253-319`）。本块只负责"部分结果 ↔ 字节"，写盘与触发都在 D12.3。

- D01.10 没有移植之前，D12.3 在**触发落盘的地方**返回 `blocked-by:D01.10`（不能静默不落盘、也不能静默继续占内存超过配额），这是 #335 的范围；本块不另写桩。
- 第 S 部分之前，Rust 的 `AggFunc` trait **不定义**序列化方法，所以不存在"被调到却什么也没写"的空实现；第 S 部分引入它们时，默认实现照 Go 的 `baseAggFunc`（不写、读回空，`aggfuncs.go:229-235`）。
- 序列化格式：A1 已定落盘格式"第一步按 Go 的格式移植"。Go 的 `util/serialization` 直接按内存布局写（`*(*types.MyDecimal)(unsafe.Pointer(&tmp[0])) = *value` 等，`SerializeMyDecimal`，`pkg/util/serialization/serialization_util.go:107-111`），Rust 按 Go 结构的字段和填充逐字节写出同样的布局（不依赖 Rust 结构的布局），govec 比较字节。格式是本进程内部的，不跨进程、不与 Go 节点交换。
- 反序列化条数不符时 Go `panic("Fail to deserialize partial result")`（`aggfuncs.go:272`），Rust 按 M-PANIC 保留同样文本的 `panic!`，由 worker 的 task 边界兜底转成错误（与 Go worker 的 `recover` 相同）。

### 5.5 错误、panic 和 `defer`

- `errors.New` / `errors.Errorf`（`func_avg.go:97`、`func_count_distinct.go:419`、`:594`、`func_max_min.go:49`、`:59`、`:103`、`:122` 等）：`tidb_base::errors` 的对应函数，文本照抄，客户端看到 1105。
- `ErrCutValueGroupConcat`：`TypeCtx` 的 `TruncateAsWarning` 为假时报错，否则加 warning（`func_group_concat.go:73-83`），先判断标志再报，顺序照抄；参数是 `args[0].StringWithCtx(ctx, RedactLogDisable)`。
- `plannererrors.ErrInternal`（`func_group_concat.go:515`、`:635`，ORDER BY 版本的 `Merge` 不应被调用）：照抄。
- JSON_OBJECTAGG 的报错顺序：求键 → 键为 NULL（`ErrJSONDocumentNULLKey`）→ 键是 binary 字符集（`ErrInvalidJSONCharset`）→ 求值 → `getRealJSONValue` → 不支持的值类型（`ErrUnsupportedSecondArgumentType`）（`func_json_objectagg.go:71-101`）。出错时已经插入的键不回滚，照抄。
- 显式 `panic`：`builder.go:481`（GROUP_CONCAT 分隔符求值失败）、`func_value.go:261`（未知求值类型）、`aggfuncs.go:272`（见 §5.4）。前两处从 SQL 到不了（分隔符是描述层保证的非空字符串常量；求值类型已枚举完），按 M-PANIC 保留同样文本的 `panic!`。
- `builder.go:171`：APPROX_PERCENTILE 的百分比求值出错时记 error 日志并返回 nil，Rust 用 `log::error!` 加 `Ok(None)`。
- `defer`：只有 GROUP_CONCAT 的三处（`func_group_concat.go:126`、`:239`、`:577`），都是在返回前把缓冲区容量的变化加进具名返回值 `memDelta`，包括出错返回。决定点 1 定为 B，这三处整个消失（只有记账，没有要释放的资源，所以不需要 drop guard）。
- 被 Go 忽略的错误：`topNRows.Less` 中 `Datum.Compare` 的错误被丢弃、当作"不小于"（`func_group_concat.go:330-342`，Go 注释说明了这一点，并引用上游 issue `52449`）。照抄。

### 5.6 其他映射规则

- **M-INT**：`uint64` 的下标运算照 Go 回绕。LEAD 的 `p.curIdx+v.offset`（`func_lead_lag.go:66`）在 offset 接近 2^64 时回绕，写 `wrapping_add`（是否是 Go 的问题见 §8 第 1 条）。ntile、nth_value 的减法有前置判断，不会回绕。
- **M-F2I**：`int(math.Ceil(float64(n) * (float64(percent) / 100)))`（`func_percentile.go:42`）值域在 `[1, n]`，仍按规则调用 `gostd::conv::f64_to_i64`，标 `[M-F2I]`。
- **M-FMA**：tracker 的乘加登记表 `data/go_fma_sites.tsv` 没有扫描本包（`scripts/go_fma_sites.py` 的 `DEFAULT_PKGS` 只有 `types`、`planner/cardinality`、`statistics`、`planner/core`）。本包的候选位置是 `calculateIntermediate` 的 `float64(count)*input - sum`（`func_varpop.go:68`）和 `calculateMerge` 的 `(src/dst)*dstSum - srcSum`、`… + c*t*t`（`func_varpop.go:97-98`），arm64 上可能被融合。第 B1 部分的 tracker PR 先把本包加进 `DEFAULT_PKGS` 重新生成，再按登记表写 `mul_add` 或分开的乘加；VAR / STDDEV 的向量按目标分别生成。其他浮点运算（SUM、AVG 的累加，PERCENT_RANK、CUME_DIST 的除法，`math.Sqrt`）没有乘加。
- **M-MAPORDER**：JSON_OBJECTAGG 的 `Merge` 遍历 Go map（`func_json_objectagg.go:208`），同一个键 `src` 覆盖 `dst`，与遍历顺序无关；最终的 JSON 对象按键排序，也与顺序无关。第 S 部分的 `serializePartialResult4JsonObjectAgg` 按 map 顺序写字节（`spill_serialize_helper.go:164`），字节依赖顺序：govec 对它只比较反序列化后的内容。
- **随机数**：APPROX_PERCENTILE 用的 `selection.Select` 取随机主元（`pkg/util/selection/selection.go:84-86`，`math/rand`）。选出的**值**与主元无关；但如果多个元素比较相等而表示不同（如 DECIMAL 的 `1.0` 与 `1.00`），Go 每次运行可能选出不同的那个。负责人定为 C（§9 第 7 条）：生产代码照 Go 用真随机数（L1，与 Go 一样不确定）；随机源是 `Select` 的一个参数，测试和 govec 回放注入固定种子。govec 生成器仍排除这类输入，所以向量与主元无关。
- **M-BYTES**：GROUP_CONCAT 的分隔符、缓冲区和 MAX / MIN 的字符串是用户的原始字节，用 `Vec<u8>`。Go 的 `stringutil.Copy`（防止引用被复用的 chunk 缓冲区，`func_max_min.go:1103-1108`）在 Rust 中就是拷贝成自有的 `Vec<u8>`。滑动 max / min 的队列在 Go 中直接存 `EvalString` 返回的字符串、不拷贝（`func_max_min.go:1187`、`:1213`），Rust 的队列存自有的副本（所有权要求；Go 的那段内存在窗口执行期间不会被复用，结果相同）。
- **浮点集合**：DISTINCT 的 `Float64SetWithMemoryUsage` 是 Go 的 `map[float64]struct{}`：`+0` 与 `-0` 是同一个键，NaN 与任何键都不相等（每插入一个 NaN 就多一个元素）。Rust 的集合按这个语义实现（键为规范化 `-0` 后的位模式，NaN 单独计数），L1。
- **多参数 COUNT(DISTINCT) 的编码**（`evalAndEncode`，`func_count_distinct.go:357-422`）：整数按本机字节序写 8 字节（`unsafe.Pointer`，`:424-429`）；按裁定 **R23**（#390，宪章第九条 3），`append_float64` 先将 `-0.0` 归一化为 `+0.0` 再写 8 字节、`append_duration` 只写 `Duration` 的 8 字节（省略 `Fsp`）、`append_time` 调用 `WriteTime` 后将 `Type`（字节 12）和 `Fsp`（字节 13）清零，使多参数/回退路径与单参数路径和 SQL 相等语义一致（`WriteTime` 本身保持与 Go 相同的 16 字节布局）。
- **M-NAME**：方法名与 Rust trait 不冲突（没有 `Clone` / `String`）。`Len` / `Less` / `Swap` / `Push` / `Pop`（`sort.Interface` 与 `heap.Interface`）写成固有方法，由 gostd 的 `sort` / `heap` 通过闭包调用，与 `crates/base/src/gostd/sort.rs` 现有的写法相同。
- **M-POOL**：本包没有对象池。
- **新代码不进默认路径**：本块只由新栈的执行器 builder（D12.3、D12.9）调用，`--new-stack=off` 时不会被构建，不需要额外开关。

## 6. 测试计划

### 6.1 要移植的 Go 单测

Go 的单测都基于 `aggfunc_test.go` / `window_func_test.go` 的帮助函数和 `mock.NewContext()`，不依赖存储（`func_max_min_test.go` 的 `TestMaxSlidingWindow` 除外）。Rust 用现有的静态表达式上下文（`tidb_sessionctx::exprstatic`）代替 `mock.NewContext()`，帮助函数移植到 `crates/executor/src/aggfuncs/tests/`。

| Go 测试 | 部分 | 处理 |
|---|---|---|
| `TestCount`、`TestSum`、`TestAvg`、`TestMaxMin`、`TestMergePartialResult4{Count,Sum,Avg,FirstRow,MaxMin,BitFuncs}` | A | 移植 |
| `TestGroupConcat`、`TestJsonArrayagg`、`TestJsonObjectagg`、`TestVarpop`、`TestVarsamp`、`TestStddevpop`、`TestStddevsamp`、`TestMergePartialResult4{GroupConcat,JsonArrayagg,JsonObjectagg,Varpop,Varsamp,Stddevpop,Stddevsamp}`、`TestWriteTime` | B1 | 移植（`TestWriteTime` 在 `func_count_test.go`，测的是 B1 的 `WriteTime`） |
| `TestPercentile`、`TestFix26807`、`TestFix40463` | B2 | 移植 |
| `TestLeadLag`、`TestWindowFunctions`、`TestDequeReset`、`TestDequePushPop` | W | 移植 |
| `spill_helper_test.go` 的 29 个 `TestPartialResult4*` / `TestBasePartialResult4GroupConcat` | S | 移植 |
| 17 个 `TestMemXxx`（avg、bit、count、first_row、group_concat、json_arrayagg、json_objectagg、max_min、sum、varpop、percent_rank、cume_dist、ntile、rank、row_number、value、lead_lag） | 各部分 | 决定点 1 定为 B：按 A1"数值不对照"改写成性质测试（分配后 tracker 增长、Drop 后归零），在实现 PR 中逐个列出 |
| `TestMaxSlidingWindow` | W | 不移植：用 `testkit` 建表跑 SQL，属于结果级对照（`tools/ab`，合并后由维护者跑） |
| `BenchmarkAvg`、`BenchmarkCount` | — | 不移植；性能问题先登记到性能剖析清单 |

另加 Rust 单测：`AggFunc` 与 `PartialResult` 的 `Send` / `Sync` 静态断言；`Split` 出的每对 partial / final 函数能互相解释部分结果（§5.1）；GROUP_CONCAT 截断标志在两个函数对象间共享、只报一次。

### 6.2 govec

新领域 `aggfuncs`（tracker `govec/domains/aggfuncs.go`，Rust 回放 `crates/executor/tests/govec_aggfuncs.rs`）。Go 侧直接用 v8.5.8 的 `aggfuncs.Build` / `BuildWindowFunctions`，未导出的函数经 `go:linkname`（与 `pointget` 领域相同）。记录中带**完整的描述**（函数名、模式、DISTINCT、参数的类型和列下标、返回类型、ORDER BY 项），Rust 直接构造 `AggFuncDesc`，不经 D08.11 的类型推导，这样本块的向量不受 D08.11 推导结果的影响（推导由 D08.11 的 `aggdesc` 领域覆盖）。向量在本地生成，CI 的 `scripts/govec.sh --check` 在两个架构上逐字节验证；VAR / STDDEV 按目标分别生成（M-FMA）。

| op | 部分 | 覆盖 | 输入 |
|---|---|---|---|
| `build` | A、B1、B2、W | `Build`、`BuildWindowFunctions`、各 `buildXxx` 选出的实现（Go 的 `%T`，含 nil） | 每个函数名 × 模式 × DISTINCT × 参数 / 返回类型的每种求值类型（含 BIT、ENUM、SET、带 `EnumSetAsIntFlag` 的 ENUM / SET、UNSIGNED、FLOAT 与 DOUBLE）× `tidb_window_use_high_precision` |
| `update_final` | A、B1、B2 | `Alloc` → 多次 `Update` → `AppendFinal` | 每个实现；行分成多批（含空批、全 NULL、单行），值含边界（INT64 / UINT64 极值、DECIMAL 进位与 65 位、-0.0、零日期、负 Duration、非 UTF-8 字符串、各 collation 下相等的字符串）；SUM / AVG 的 DECIMAL 溢出错误；`div_precision_increment` 取 0、4、30 |
| `reset_reuse` | A、B1、B2 | stream agg 的用法：`AppendFinal` → `Reset` → 再 `Update` | 每个实现；检查上一组的状态不残留（如 `sum4Decimal.ResetPartialResult` 只清计数不清值，`func_sum.go:202-205`） |
| `merge` | A、B1、B2 | partial 函数分别更新 k 份，final 函数 `Merge` 到一份再 `AppendFinal`；也记录 Partial1 的 `AppendFinal` 输出（如 AVG 的两列、APPROX_COUNT_DISTINCT 的 `Serialize` 字节） | 每个可拆分的函数；`Split` 的描述由 Go 生成；`src` 或 `dst` 为空的组合 |
| `group_concat` | B1 | 分隔符、`group_concat_max_len`（1、2、分隔符中间截断）、截断时 warning 与错误（`TruncateAsWarning` 两种）、只报一次、ORDER BY 多列与 DESC、**并列行**（`sort.Sort` 和 `container/heap` 的顺序）、各 collation 的 DISTINCT | |
| `count_distinct_encode` | B1 | `evalAndEncode` 的每种求值类型；多参数与单参数在 -0.0、Duration 不同 fsp、Time 不同类型上的区别（§8 第 3 条） | |
| `approx_count_distinct` | B2 | 跨越扩容阈值的个数、`readAndMerge` 的正常与"too large size degree"错误、四种模式与 STRING 返回类型 | |
| `percentile` | B2 | 百分比 1、50、100；各类型；排除比较相等而表示不同的输入（§5.6） | |
| `json_agg` | B1 | 各值类型（含 Opaque、BINARY 补零、DECIMAL 转 DOUBLE）、重复键、NULL 键、binary 键、不支持的类型 | |
| `window` | W | `Update` / `Slide` / `AppendFinal` 的序列（帧的起止、行与范围帧）；RANK / DENSE_RANK 的并列；NTILE、NTH_VALUE 取 0 与 NULL；LEAD / LAG 的默认值转换；滑动 max / min 的单调队列 | |
| `spill` | S | 每种可序列化部分结果的字节（与 Go 逐字节相同，JSON_OBJECTAGG 只比较反序列化后的内容）、往返后再 `Merge` 的结果 | |

领域的 `Coverage` 覆盖 663 个函数（§3.6 的 8 个登记为 `Exempt`，理由写"A1 例外"）。回放中 `blocked` 的条数按部分固定（`EXPECTED_BLOCKED`），第 B 部分合并前 `build` 中落到 §4 那个临时桩上的记录计入这里。

### 6.3 其他

- CI：`govec_aggfuncs` 加进 arm64 job（与 `govec_pointget` 相同）。本块不读时区（TIMESTAMP 在求值前已转换），不需要非 UTC 的 `TZ` 步骤。
- 端到端：sysbench 的 SUM / DISTINCT 要等 D09.4、D09.9、D10.1、D10.3、D12.3 和本块第 A 部分接上，属于集成验收（`tools/ab`），不在本块。

## 7. 与 Go 的已知差异

1. **`memDelta`**（§9 第 1 条）：Rust 不返回 Go 的估算值，内存按实际分配记账（A1）。可见影响只在"刚好卡在某个配额上"的落盘或取消时机（A1 spec §5 已接受）。涉及 trait 的五个方法和 §3.6 的 8 个函数。
2. **`UpdatePartialResult` / `MergePartialResult` 出错时**不返回 `memDelta`（§5.1；Go 的调用方都丢弃它）。
3. **LEAD / LAG 的值求值器**放在部分结果里（`func_lead_lag.go:31`，§9 第 6 条），结果不变。
4. **GROUP_CONCAT ORDER BY 排序时的上下文**用 `AppendFinal` 传入的，而不是最近一次 `Update` 存下的（`func_group_concat.go:470`、`:574`，§5.2）。同一语句内相同。
5. **`MergePartialResult` 中 `src` 被移走**（`func_group_concat.go:168`、`func_percentile.go:164` 等，§5.1）：Go 让 `dst` 与 `src` 共享缓冲区，Rust 移动。Go 的调用方合并后都不再用 `src`，结果相同。
6. **滑动 max / min 的字符串**存副本（`func_max_min.go:1187`、`:1213`，§5.6）。
7. **APPROX_PERCENTILE 的主元**：生产代码与 Go 相同（真随机数，§9 第 7 条），不是差异；只有测试和 govec 回放注入固定种子（`pkg/util/selection/selection.go:84-86`）。
8. **`Build` 的 nil** 表示为 `Ok(None)`，`baseAggFunc.retTp` 的 nil 表示为 `None`，只是表示方式不同。
9. 第 B 部分合并前 `Build` 对那 9 个函数名返回 `blocked`（路由器回退到 PoC），不是用户可见的差异。
10. **多参数/回退 COUNT(DISTINCT) 的去重键规范化（裁定 R23，#390，宪章第九条 3）**：`func_count_distinct.go:431-436`（`appendFloat64`）、`:459-464`（`appendTime`）、`:466-471`（`appendDuration`）在 Go v8.5.8 上分别将 `-0.0`、Time 的 `Type`/`Fsp`、Duration 的 `Fsp` 编入去重键，导致相等值被多算（见 §8 第 3 条）。Rust 按裁定 R23（`docs/testing/rulings.toml`）在 `append_float64` 将 `-0.0` 归一化为 `+0.0`、`append_duration` 只编 `Duration` 值、`append_time` 清零 `Type`/`Fsp` 字节；govec 保留 Go 原始向量，回放将命中该规则的 3 条向量（`count_distinct_encode` 多参数 `{+0.0, -0.0}` 与 `DATETIME` 异 `Type`、`reset_reuse` `TIMESTAMP` 异 `Type`/`Fsp`）逐组核验 Go 原始计数与 Rust 规范计数后标记为 R23 裁定差异。
11. **两个 NaN 相加时得到哪一个**：SUM / AVG 的浮点累加（`func_sum.go:100`、`:111`、`:348`，`func_avg.go:377`、`:438`、`:446`、`:486`）以及 VAR / STDDEV 的方差累加（`func_varpop.go:70`、`:98`）在两个操作数都是 NaN 时，硬件返回按机器码操作数顺序的第一个（x86 的目的寄存器、ARM 的 Rn），gc 和 Rust 的编译器（LLVM 认为浮点加法可交换）排的顺序可能不同，所以结果的 NaN 位模式（符号、payload）可能不同（第 A 部分的 govec 在 amd64 上看到 12 条 DISTINCT 求和的记录不同；第 B1 部分在同组同时出现 `+Inf` 与 `-Inf` 时，`variance` 为 `+NaN`、`FNMSUBD` 算出的 `t` 为 `-NaN`，`variance += (t*t)/...` 同样触发两个 NaN 相加）。只有一个 NaN 时、以及 Inf - Inf 的默认 NaN 都与 Go 相同。Go 写入时拒绝 NaN 与 ±Inf，计算也不产生 NaN / ±Inf 输入，所以从 SQL 到不了；govec 的 SUM / AVG 记录不含 NaN 输入，VAR / STDDEV 记录不含 NaN 和 ±Inf 输入。维护者已定不逐位跟随 gc（§9.2 第 12 条），保持现在的写法。
12. **`UpdatePartialResult` 多一个参数** `st: &EvalState`（§5.1），只是 Rust 表达式求值的接口，结果相同。

## 8. 可能的 Go 问题（读代码发现，尚未在运行中的 v8.5.8 上复现）

按宪章第一条 2，默认照 Go 移植。每条都要在 v8.5.8 上复现后，按 CLAUDE.md 规则 6 在本仓库开英文 issue（标签 `upstream-pitfall`），再在这里引用 issue 号。本 spec 没有开 issue（§9 第 8 条：由各部分的实现任务复现后再开）。

1. **LEAD 的偏移量很大时回绕（疑似结果错误）**：`p.curIdx+v.offset < uint64(len(p.rows))`（`func_lead_lag.go:66`）。偏移量只检查"非负整数常量"（`pkg/expression/aggregation/window_func.go:50-56`，`GetUint64FromConstant` 接受 UINT64 字面量），`LEAD(a, 18446744073709551615) OVER (ORDER BY a)` 在第 2 行时 `1 + (2^64 - 1)` 回绕成 0，返回第 1 行的值而不是默认值 NULL。LAG 有 `p.curIdx >= v.offset` 的判断，没有这个问题。如果复现，属于宪章第一条 2 的"结果错误"，要负责人裁定。
2. **没有序列化方法的聚合函数在并行 hash agg 落盘时（疑似崩溃）**：VAR_POP / VAR_SAMP / STDDEV_POP / STDDEV_SAMP（非 DISTINCT）、APPROX_COUNT_DISTINCT、APPROX_PERCENTILE、VECTOR 类型的 FIRST_ROW / MAX / MIN 都可以并行执行（并行只排除 DISTINCT 和 ORDER BY，`pkg/executor/builder.go:2056-2066`），但它们用 `baseAggFunc` 的空序列化（`aggfuncs.go:229-235`）：落盘时这一列不写任何行（`agg_hash_partial_worker.go:351-353`），恢复时 `DeserializePartialResult` 返回空切片，`partialResultsRestored[aggPos][rowPos]` 越界（`agg_spill.go:301`、`:315`）。要在 `tidb_mem_quota_query` 很小、`tidb_enable_parallel_hashagg_spill = ON` 时复现。这是 D12.3 与本块共同的路径，在第 S 部分和 D12.3 的落盘部分一起复现；如果复现，属于安全 / 正确性问题，要负责人裁定（Rust 可能的处理：对这些函数禁止落盘，或补序列化）。
3. **多参数/回退 COUNT(DISTINCT) 的去重键与比较语义不一致**（#390，裁定 **R23**）：已在 v8.5.8 上全部复现：（a）单参数 REAL 用 `map[float64]`（`+0 == -0`），多参数 `appendFloat64` 按位编码（`func_count_distinct.go:431-436`），SQL 上 `COUNT(DISTINCT x)=1` 而 `COUNT(DISTINCT x, 1)=2`；（b）单参数 Duration 只比较 `Duration`（`:233-237`），多参数 `appendDuration` 含 `Fsp`（`:466-471`），`TIME(str)` 在 `{'00:00:01', '00:00:01.000'}` 上单参数为 1、多参数为 2；（c）`appendTime` 经 `WriteTime` 写入 `Type` 和 `Fsp`（`:445-464`），`TIMESTAMP(str)` 在 `{'2020-01-01 00:00:00', '2020-01-01 00:00:00.000'}` 上返回 2、`COALESCE(date, dt)` 在 `{'2020-01-01', '2020-01-01 00:00:00'}` 上返回 2，而比较忽略 `Type` 与 `Fsp`。按宪章第九条 3（维护者与评审一致同意，见 #390）全部改为正确行为并登记裁定 **R23**（§7 第 10 条、`docs/testing/rulings.toml`）。
4. **GROUP_CONCAT ORDER BY 忽略比较错误**（`func_group_concat.go:330-342`）：Go 自己的注释已经指出（上游 issue `52449`）。照抄，只记录。

## 9. 已决定的点

### 9.1 负责人已定（2026-11-24，由维护者 du-maya 在 #342 转达）

5. **落盘字节格式**：照 A1，落盘文件与 Go 的内存布局逐字节相同（§5.4），govec 比较字节（JSON_OBJECTAGG 只比较反序列化后的内容）。
7. **APPROX_PERCENTILE 的随机主元**：选 C，生产代码用真随机数（与 Go 相同），只在测试和 govec 中注入固定种子（§5.6）。

### 9.2 维护者已定（宪章第九条，#374：按本 spec 的建议）

1. **`memDelta` 与内存记账**：B，按 A1 的 `TrackedAlloc` 按实际分配记账，trait 不返回 `memDelta`，§3.6 的 8 个函数登记为 A1 例外（与 D12.3 决定点 8 相同）。
2. **`PartialResult` 的承载**：A，`Box<dyn Any + Send>`；不等 N6 定稿（与 D12.3 决定点 9 相同）。
3. **实现任务的拆分**：A，A、B1、B2、W、S 各一个任务（B1、B2 可合并）；第 A 部分的 `Build` 对 9 个第 B 部分的函数名返回临时 `blocked-by:D12.4`，`executor` 区域的桩棘轮因此 +2，作为按部分移植的例外（PR 正文列出，第 B 部分合并时移除）。
4. **第 S 部分的时间**：A，与 D12.3 的落盘部分同时（在 D01.10 之后）。
6. **LEAD / LAG 的值求值器**：A，放进部分结果（L2）。
8. **§8 的可能问题**：A，由对应部分的实现任务在 v8.5.8 上复现后开 issue；第 1、2 条若复现，按宪章第一条 2 请负责人裁定。

以下由维护者 du-lucky-agent 在第 A 部分的实现 PR #378 的评审中决定（宪章第九条，内部实现，不改变结果）：

9. **`update_partial_result` 加 `st: &EvalState`**（§5.1）：可以。关联列的值从这里来，与其他执行器的求值方式一致。
10. **COUNT(DISTINCT) 与第 B 部分的 9 个函数名共用一个桩站点**（§4）：可以，第 B1 部分合并时一起移除。
11. **`set_alloc(&mut self, ColAlloc)`，`MemAwareMap` 对分配器泛型**（§5.3）：可以，前提是字符串集合的键也在同一个分配器里（已照做）。`&mut self` 保证只能在函数对象被多个 worker 共享之前设置。
12. **两个 NaN 相加时得到哪一个**（§7 第 11 条）：不逐位跟随 gc，保持现在的写法。只有 NaN 的位模式可能不同，输出给客户端时都格式化为 `NaN`；SQL 中只能由 Inf - Inf 得到 NaN，同一架构上两边是同一个默认 NaN。

## 本文所用 ID

- A1：内存模型的例外（按实际分配记账、`TrackedAlloc`、落盘纳入动作链；与 N3 合在一份 spec）
- A4：JSON 实现的例外
- D00.1：上下文接口骨架（sessionctx、exprctx 等）
- D00.2：parser/mysql 常量、errno、terror、dbterror
- D00.3：`util/*` 叶子工具文件（按需整文件移植）
- D01.1 / D01.2 / D01.3 / D01.4：Datum 与 FieldType、MyDecimal、时间类型、类型转换
- D01.5：JSON 二进制格式
- D01.6：向量类型
- D01.7：Chunk 与列存
- D01.8：codec
- D01.9：collation
- D01.10：落盘容器与磁盘 chunk
- D02.x：parser（D02.1 语法与 AST、D02.2 charset、D02.3 model / types）
- D08.1：表达式框架
- D08.11：聚合函数描述（expression/aggregation，#333）
- D08.12：表达式工具函数
- D09.4：聚合、窗口的构建与逻辑算子
- D09.9：聚合相关规则（#334）
- D09.14：逻辑算子公共部分与基本逻辑改写规则
- D10.1：物理算子定义与 task
- D10.3：物理计划枚举
- D12.3：聚合执行器框架（hash / stream agg，#335）
- D12.4：本块，聚合函数实现（executor/aggfuncs）
- D12.9：其他执行器（window、pipelined window 等）
- D15.1：内存统计与配额（A1 的实现）
- N0：crate 分层的设计
- N3：并发与取消的设计（与 A1 合在一份 spec）
- N4：表达式的设计（`EvalState`：表达式的每次执行状态，§3.4）
- N5a：根节点内存中的列格式
- N6：hash agg / join / sort 数据结构的例外（原则同意，spec 未写）
- M-ALIAS、M-BYTES、M-CH、M-ERRVAL、M-F2I、M-FMA、M-GO、M-IFACE、M-INT、M-KILL、M-LOCK、M-MAPORDER、M-NAME、M-PANIC、M-POOL、M-ROW、M-SELF、M-WG：`docs/porting/mappings.md` 中的映射规则（依次为：输出参数与输入同一对象、Go string 的承载、channel、错误时仍有意义的返回值、浮点转整数、浮点乘加、goroutine、接口与类型断言、定宽整数、KILL 检查点、锁、map 顺序、与 Rust trait 同名的方法、panic、对象池、跨调用保存的行、struct 嵌入、WaitGroup）
- L1 / L2：来源标注的等级（L1 逐行对应；L2 有理由的改写）
- X：不在 tidb-rs 范围内的代码（写路径、mock store、测试工具等）
- PoC：原型实现（新栈路由回退的目标）
- P4：ROADMAP 的第四阶段（执行器）
- VS1：第一条纵向切片
