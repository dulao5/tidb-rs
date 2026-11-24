# D08.11 聚合函数描述（expression/aggregation）：spec

状态：**已确认**（负责人 2026-11-24 在 Raft 上确认，§9 全部选 A，由 Maya 在 PR #341 转达；只写 spec，#333）。按 §2 的部分拆成实现任务。

块：D08.11（ROADMAP P3 的第 1 层；Go 函数体约 1,592 行，16 个文件），crate `tidb-expression`（N0 rank 8，`layers.toml` 已登记）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/expression/aggregation/`，另有说明的除外。

相关：[N0](../exceptions/N0-crate-layering/spec.md)（crate 分层，§4 的反向引用处理）、[N1](../exceptions/N1-plan-representation/spec.md)（计划表示）、[N13](../exceptions/N13-shared-metadata/spec.md)（共享元数据）、[mappings.md](../../docs/porting/mappings.md)、[D08.3 plan](../D08.3-pushdown/plan.md)（pb 转换的反向引用）、[D09.14 spec](../D09.14-logical-operators/spec.md) §1（聚合目前走 PoC 的原因）。同时在写的兄弟 spec：聚合相关规则（D09.9，#334）；之后是聚合执行器框架（D12.3，#335）、聚合函数实现（D12.4，#336）。

## 1. 这个块在 Go 里做什么

### 1.1 内容

`pkg/expression/aggregation` 是聚合函数和窗口函数的**描述层**：规划器用它表示"一个聚合函数调用"，执行器再按描述建出真正计算的 `aggfuncs.AggFunc`（D12.4）。它本身不在执行期逐行计算（执行器用的是 D12.4 的 `aggfuncs`）。

| Go 文件 | 内容 |
|---|---|
| `base_func.go` | `baseFuncDesc`（名字、参数、返回类型）；`TypeInfer` 按函数名推导返回类型（count、sum、avg、max/min/firstrow、group_concat、bit_*、approx_*、json_*agg、var/stddev，以及 row_number、rank、ntile、lead/lag、cume_dist、percent_rank、first/last/nth_value 等窗口函数）；`WrapCastForAggArgs` 给参数包 CAST；`GetDefaultValue`（空输入时的结果）；`clone`、`equal`、`StringWithCtx`、`MemoryUsage` |
| `descriptor.go` | `AggFuncDesc`（在 `baseFuncDesc` 上加 `Mode`、`HasDistinct`、`OrderByItems`、`GroupingID`）；`NewAggFuncDesc`；`Split`（并行 hash agg 拆成 partial / final 两个描述）；`EvalNullValueInOuterJoin`（外连接内表无匹配时聚合函数的取值，聚合下推穿过 join 用）；`UpdateNotNullFlag4RetType`（无 GROUP BY 时去掉 NOT NULL）；`Clone`、`Equal`、`StringWithCtx`、`MemoryUsage`；`GetAggFunc`（旧求值器，见 §2.3） |
| `aggregation.go` | `AggFunctionMode`（Complete / Final / Partial1 / Partial2 / Dedup）与 `ToString`；`NeedCount`、`NeedValue`、`IsAllFirstRow`；下推判定 `CheckAggPushDown`、`checkVectorAggPushDown`、`CheckAggPushFlash`；旧求值器接口 `Aggregation`、`AggEvaluateContext`、`NewDistAggFunc` |
| `agg_to_pb.go` | 聚合函数与 tipb 的互转：`GetTiPBExpr`、`AggFuncToPBExpr`、`AggFunctionModeToPB`、`PBAggFuncModeToAggFuncMode`、`PBExprToAggFuncDesc` |
| `window_func.go` | `WindowFuncDesc`；`NewWindowFuncDesc`（参数检查、按函数调整 NOT NULL）；`UseDefaultFrame`、`NeedFrame`；`WindowFuncToPBExpr`、`CanPushDownToTiFlash` |
| `explain.go` | `ExplainAggFunc`：EXPLAIN 中聚合函数的文本 |
| `avg.go`、`sum.go`、`count.go`、`max_min.go`、`first_row.go`、`bit_and.go`、`bit_or.go`、`bit_xor.go`、`concat.go`、`util.go` | 旧求值器的实现（`Update`、`GetResult`、`GetPartialResult`、`CreateContext`、`ResetContext`），distinct 检查（`distinctChecker`，基于 `util/mvmap`），`calculateSum` |

### 1.2 调用方（tracker `data/block_edges.tsv`，指向 D08.11 的引用数）

| 块 | 引用数 | 调用什么 |
|---|--:|---|
| D12.4 聚合函数实现（aggfuncs，#336） | 57 | `aggfuncs.Build` / `BuildWindowFunctions` 读描述的 `Name`、`Args`、`RetTp`、`Mode`、`HasDistinct`、`OrderByItems` |
| D10.1 物理算子与 task | 39 | `task.go`：聚合下推时 `Clone`、`TypeInfer`、`TypeInfer4AvgSum`、`AggFuncToPBExpr`；`physical_plans.go`：`Clone`、`MemoryUsage` |
| D09.9 聚合相关规则（#334） | 36 | 聚合下推（`EvalNullValueInOuterJoin`、`NewAggFuncDesc`、`Clone`）、聚合消除、skew 改写、max/min 消除 |
| X | 35 | mock store（`mockcopr`、`unistore/cophandler`）的旧求值器 |
| D09.4 聚合与窗口的构建 | 17 | `buildAggregation`、`buildDistinct`（`NewAggFuncDesc`、`UpdateNotNullFlag4RetType`、`IsAllFirstRow`）、`buildWindowFunctions`（`NewWindowFuncDesc`、`WrapCastForAggArgs`、`UseDefaultFrame`、`NeedFrame`） |
| D10.3 物理计划枚举 | 11 | `exhaust_physical_plans.go`：`TypeInfer4FinalCount`、`CanPushDownToTiFlash`、`CheckAggPushDown` |
| EXC.B | 11 | cascades 规则（不移植） |
| D10.9 后处理与 EXPLAIN | 10 | `ExplainAggFunc`、`plan_to_pb.go`（`AggFuncToPBExpr`、`WindowFuncToPBExpr`）、`pb_to_plan.go`（`PBExprToAggFuncDesc`） |
| D09.2 表达式改写 | 7 | 子查询改写成聚合（`NewAggFuncDesc`） |
| D12.3 聚合执行器框架（#335） | 5 | `pkg/executor/builder.go`：`Split`、`GetDefaultValue`、`IsAllFirstRow` |
| D09.6、D10.10、D09.13、D09.3、D09.7、D10.5、D12.9、D09.0 | 1–4 | 去相关、`WrapCastForAggFuncs`、`InjectProjBelowAgg`、列裁剪等 |

sysbench `oltp_read_only` 的 `SELECT SUM(k) ... WHERE id BETWEEN ? AND ?` 和 `SELECT DISTINCT c ... ORDER BY c` 走的链路：D09.4 建出 `sum` / `firstrow` 的描述 → D09.9 规则 → D10.1、D10.3 物理计划（下推到 TiKV 时 `CheckAggPushDown`、`AggFuncToPBExpr`）→ D12.3 执行器（`Split`、`GetDefaultValue`）→ D12.4 聚合函数。

### 1.3 依赖

本块用到的其他块的函数都已移植（逐个用 `grep '// go: <Symbol>'` 核对过；所在的块不一定整块完成）：

| 依赖 | 用到的 | Rust 位置 |
|---|---|---|
| D08.1 表达式框架 | `Expression`、`Column`、`Constant`、`ScalarFunction`、`Schema`、`ConstLevel`、`EvalInt`、`Equal`、`Clone`、`StringWithCtx`、`MemoryUsage`、`EvaluateExprWithNull` | `crates/expression` |
| D08.8 CAST | `BuildCastFunction`、`WrapWithCastAs{Int,Real,String,Decimal,Time,Duration,JSON,VectorFloat32}` | `builtin_cast.rs` |
| D08.9 控制函数 | `InferType4ControlFuncs`（lead/lag 三个参数时） | `builtin_control.rs` |
| D08.12 表达式工具 | `GetUint64FromConstant` | `util.rs` |
| D08.3 下推 | `PbConverter.ExprToPB`、`NewPBConverter`、`ToPBFieldType(WithCheck)`、`SortByItemToPB`、`PBToExprs`、`FieldTypeFromPB`、`IsPushDownEnabled`、`CanExprsPushDown`、`PushDownContext` | `crates/expr-pushdown`（rank 12，比本块高，见 §4.8） |
| D01.1 / D02.3 类型 | `FieldType` 的 setter、`UpdateFlenAndDecimalUnderLimit`、`SetBinChsClnFlag`、`NewFieldTypeBuilder`、`GetDefaultFieldLengthAndDecimal`、`GetDefaultCharsetAndCollate`、`Datum` | `crates/types`、`crates/parser`、`crates/base` |
| D00.1 上下文 | `BuildContext`、`EvalContext.GetDivPrecisionIncrement` | `crates/sessionctx` |
| D02.1 parser | 函数名常量（`ast::AggFuncSum` 等）、`ast.FrameClause` | `crates/parser` |
| `pkg/planner/util.ByItems`（原归 D10.10 计划工具，现归本块） | `AggFuncDesc.OrderByItems` 的元素类型 | 已移到 `crates/expression/src/byitem.rs`，规划器 `pub use`，见 §4.7 和决定点 2 |

未移植、要写成边界的：

| 块 | 用到的 | 处理 |
|---|---|---|
| D10.9（`pkg/expression/explain.go`，D10.9 tasks P1-1） | `ExplainAggFunc` 要 `ScalarFunction` / `Constant` 的 `ExplainInfo`、各表达式的 `ExplainNormalizedInfo` | `ExplainAggFunc` 排在 D10.9 P1-1 之后移植（第 D 部分），不新增桩，见 §2.1 |
| EXC.B（`pkg/planner/cascades/base`） | `base.Hasher`（`Hash64` / `Equals`） | 豁免，见 §2.4 |
| X（mock store） | 旧求值器的唯一使用者 | 归 X，见 §2.3 |

本块没有 `blocked-by:D01.10`（落盘）的需要：描述层不持有数据，旧求值器的 distinct 检查只在内存里（`util.go:26-57`），而它归 X。

## 2. 函数清单的划分

tracker `data/func_blocks.tsv` 中 block 为 D08.11 的函数共 **111** 个。划分如下（类型、常量和包级变量 `noNeedCastAggFuncs`、`noFrameWindowFuncs`、`useDefaultFrameWindowFuncs` 随所在部分移植）：

| 类别 | 函数数 | crate |
|---|--:|---|
| 第 A 部分：描述与类型推导 | 43 | `tidb-expression` |
| 第 B 部分：pb 转换与下推判定 | 8 | `tidb-expr-pushdown`（§4.8） |
| 第 C 部分：窗口函数描述 | 6 | `tidb-expression` 4 个、`tidb-expr-pushdown` 2 个 |
| 第 D 部分：EXPLAIN 文本 | 1 | `tidb-expression` |
| 归 X：旧求值器（只有 mock store 用） | 49 | 不移植 |
| 豁免：cascades 的 `Hash64` / `Equals`（EXC.B） | 4 | 不移植 |
| 合计 | 111 | |

### 2.1 移植（只读范围内，58 个）

**第 A 部分（43）**：sysbench 链路和 D09.4 / D09.9 / D10.1 / D12.3 / D12.4 都要用的部分。依赖都已移植（只需先把 `ByItems` 移到本 crate，§4.7），**不新增 `blocked-by` 桩**。

- `base_func.go`（26）：`newBaseFuncDesc`、`baseFuncDesc.equal`、`baseFuncDesc.clone`、`baseFuncDesc.StringWithCtx`、`baseFuncDesc.TypeInfer`、`typeInfer4Count`、`typeInfer4ApproxCountDistinct`、`typeInfer4ApproxPercentile`、`typeInfer4Sum`、`TypeInfer4AvgSum`、`TypeInfer4FinalCount`、`typeInfer4Avg`、`typeInfer4GroupConcat`、`typeInfer4MaxMin`、`typeInfer4BitFuncs`、`typeInfer4JsonArrayAgg`、`typeInfer4JsonObjectAgg`、`typeInfer4NumberFuncs`、`typeInfer4CumeDist`、`typeInfer4Ntile`、`typeInfer4PercentRank`、`typeInfer4LeadLag`、`typeInfer4PopOrSamp`、`GetDefaultValue`、`WrapCastForAggArgs`、`baseFuncDesc.MemoryUsage`。`TypeInfer` 是一个按函数名分支的函数，窗口函数的分支也在里面，一起移植（依赖都已具备）。
- `descriptor.go`（13）：`NewAggFuncDesc`、`NewAggFuncDescForWindowFunc`、`AggFuncDesc.StringWithCtx`、`AggFuncDesc.Equal`、`AggFuncDesc.Clone`、`AggFuncDesc.Split`、`AggFuncDesc.EvalNullValueInOuterJoin`、`evalNullValueInOuterJoin4Count`、`evalNullValueInOuterJoin4Sum`、`evalNullValueInOuterJoin4BitAnd`、`evalNullValueInOuterJoin4BitOr`、`AggFuncDesc.UpdateNotNullFlag4RetType`、`AggFuncDesc.MemoryUsage`。
- `aggregation.go`（4）：`AggFunctionMode.ToString`、`NeedCount`、`NeedValue`、`IsAllFirstRow`。

第 A 部分合并后，现有的两个 `blocked-by:D08.11` 桩可以解除：`crates/planner/src/core/task.rs` 的 `compute_partial_cursor_offset`（要 `NeedCount` / `NeedValue`）和 `crates/planner/src/util/logicaltrace.rs` 的 `append_function_prune_trace_step`（要 `AggFuncDesc` 的 `StringWithCtx`）。是在同一个 PR 里解除还是交给各自块的任务，由实现任务决定。

**第 B 部分（8）**：要用 D08.3 的函数，按 N0 规则 6 写在 `tidb-expr-pushdown`（§4.8）。

- `agg_to_pb.go`（5）：`baseFuncDesc.GetTiPBExpr`、`AggFuncToPBExpr`、`AggFunctionModeToPB`、`PBAggFuncModeToAggFuncMode`、`PBExprToAggFuncDesc`。
- `aggregation.go`（3）：`CheckAggPushDown`（要 `IsPushDownEnabled`），以及只被它调用的 `checkVectorAggPushDown`、`CheckAggPushFlash`（放在一起：分开的话私有的 `checkVectorAggPushDown` 在第 A 部分里没有调用者，clippy 的 `dead_code` 会报错）。

sysbench 的 SUM 下推到 TiKV 要用 `CheckAggPushDown` 和 `AggFuncToPBExpr`，所以第 B 部分应紧跟第 A 部分（是否合成一个任务见决定点 5）。`PBExprToAggFuncDesc` 只有 `pb_to_plan.go` 的 `PBPlanBuilder`（D10.9）调用，它用于本节点作为 coprocessor 服务端处理其他 TiDB 发来的集群内存表请求（`pkg/executor/coprocessor.go`，D12.1）。

**第 C 部分（6）**：`window_func.go`。`NewWindowFuncDesc`、`UseDefaultFrame`、`NeedFrame`、`WindowFuncDesc.Clone`（`tidb-expression`）；`WindowFuncToPBExpr`、`WindowFuncDesc.CanPushDownToTiFlash`（`tidb-expr-pushdown`）。调用方是 D09.4 的 `buildWindowFunctions` 和 D10.1、D10.3 的窗口下推，sysbench 用不到，可以在 D09.4 的窗口部分之前任何时候做。§8 第 1–4 条的可见影响都在窗口函数上，在这一部分复现和处理（第 1 条的 `typeInfer4PercentRank` 本身随第 A 部分照 Go 移植）。

**第 D 部分（1）**：`ExplainAggFunc`。它对每个参数调用 `ExplainInfo` / `ExplainNormalizedInfo`（`explain.go:44-76`），而 `ScalarFunction`、`Constant` 的这两个方法和 `Column.ExplainNormalizedInfo` 属于 D10.9 tasks 的 P1-1，还没有移植（`crates/expression/src/explain.rs` 目前只有 `Column` 的 `ColumnExplainInfo`、`ColumnExplainInfoNormalized`、`ExplainInfo`）。如果在那之前移植，就得为非列参数新增一个 `blocked-by:D10.9` 桩，桩棘轮（`scripts/check_stub_ratchet.py`）的两种允许情况都不适用。所以放在 D10.9 P1-1 之后，可以并入当时正在做的任意一部分。

### 2.2 不在本块、但与本块绑在一起的函数

- **`pkg/planner/util/byitem.go`（7 个函数，原归 D10.10 计划工具）**：N0 §4 的反向引用表已决定把 `ByItems` "下沉到 D08.11"。`AggFuncDesc.OrderByItems` 是 `[]*util.ByItems`，`tidb-expression`（rank 8）不能依赖 `tidb-planner`。第 A + B 部分的第一个提交已执行：tracker 改归 D08.11，Rust 文件移到 `crates/expression/src/byitem.rs`，见 §4.7 和决定点 2。

### 2.3 归 X：旧求值器（49 个）

`Aggregation` 接口及其实现在 v8.5.8 中**只有 mock store 调用**（`pkg/store/mockstore/mockcopr/cop_handler_dag.go:335`、`mockcopr/aggregate.go`、`unistore/cophandler/cop_handler.go:445`、`mpp.go:597`、`closure_exec.go`、`mpp_exec.go`，tracker 都归 X）和本包的测试。`AggFuncDesc.GetAggFunc` 在 `pkg/` 中除了本包的测试以外没有调用者。真正的执行器用 D12.4 的 `aggfuncs`，TiKV / TiFlash 自己做 coprocessor 聚合。tidb-rs 不带 mock store，所以这些函数在 tidb-rs 中没有调用者。

| Go 文件 | 函数 |
|---|---|
| `aggregation.go`（5） | `NewDistAggFunc`、`newAggFunc`、`aggFunction.CreateContext`、`aggFunction.ResetContext`、`aggFunction.updateSum` |
| `descriptor.go`（1） | `AggFuncDesc.GetAggFunc` |
| `avg.go`（5） | `avgFunction.updateAvg`、`ResetContext`、`Update`、`GetResult`、`GetPartialResult` |
| `sum.go`（3）、`count.go`（4）、`first_row.go`（4）、`max_min.go`（3） | 各自的 `Update`、`GetResult`、`GetPartialResult`（count、first_row 还有 `ResetContext`） |
| `bit_and.go`、`bit_or.go`、`bit_xor.go`（各 5） | `CreateContext`、`ResetContext`、`Update`、`GetResult`、`GetPartialResult` |
| `concat.go`（6） | `writeValue`、`initSeparator`、`Update`、`GetResult`、`ResetContext`、`GetPartialResult` |
| `util.go`（3） | `createDistinctChecker`、`distinctChecker.Check`、`calculateSum` |

建议在 tracker `scripts/blocks/rules.py` 把这 49 个函数改归 X（与 mock store 相同），由实现任务的 tracker PR 做。备选是照样移植（没有调用者，只能靠测试和 govec 维持），或者登记为豁免。见决定点 1。

### 2.4 豁免：cascades 的 `Hash64` / `Equals`（4 个，EXC.B）

`baseFuncDesc.Hash64`、`baseFuncDesc.Equals`、`AggFuncDesc.Hash64`、`AggFuncDesc.Equals` 实现 `pkg/planner/cascades/base` 的 `HashEquals`，只被 cascades 规划器使用（`LogicalAggregation.Equals`，`pkg/planner/core/operator/logicalop/logical_aggregation.go:99`）。与 D08.1 spec §4 第 2 条、`ByItems.Hash64` / `Equals` 的处理相同，代码中写 `// go-exception: EXC.B ...`。

## 3. 边界桩

| Rust 位置 | Go 函数 | 类型 | 调用时 |
|---|---|---|---|
| — | 第 A、B、C 部分 | — | 不新增桩：依赖都已移植 |
| — | `ExplainAggFunc`（第 D 部分） | — | 不先移植，不写桩；调用它的 `LogicalAggregation.ExplainInfo`（D09.4）、`PhysicalHashAgg` 等的 EXPLAIN（D10.9）在各自的块里按现有写法返回 `blocked-by:D10.9` |
| — | 旧求值器（49） | 归 X | 不移植，没有调用者，不需要桩 |

如果决定点 1 选择"照样移植"，旧求值器也不需要桩：它的依赖（`codec.EncodeValue`、`ComputePlus`、`DecimalDiv`、`Datum.Compare`、`GetCollator`）都已移植，只有 `util/mvmap`（D00.3 叶子工具文件，可按宪章第四条 4 整文件移植）和 `ErrCutValueGroupConcat` 要补。

## 4. 承载方式（Go → Rust）

### 4.1 类型

| Go | Rust | 说明 |
|---|---|---|
| `baseFuncDesc{Name string; Args []Expression; RetTp *FieldType}` | `BaseFuncDesc { name: String, args: Vec<ExprRef>, ret_tp: Arc<FieldType> }` | `Name` 是小写的函数名（`newBaseFuncDesc` 中 `strings.ToLower`，`base_func.go:45`），只承载 SQL 标识符文本，按 M-BYTES 用 `String`。参数按 M-EXPR 共享。返回类型按 M-FT / N13 §8 用 `Arc<FieldType>`，与 `Column.ret_type` 一致 |
| `AggFuncDesc` 嵌入 `baseFuncDesc` | `AggFuncDesc { base: BaseFuncDesc, mode, has_distinct, order_by_items: Vec<ByItems>, grouping_id: isize }`，`Deref` / `DerefMut` 到 `BaseFuncDesc`（`// glue: M-SELF`） | Go 调用方大量直接写 `desc.Name`、`desc.Args`、`desc.RetTp`；`Deref` 让 Rust 调用方照样写 `desc.name`。仓库里已有同样的写法（`statistics::Table`、`SessionVars`）。`GroupingID int` 按 M-INT 用 `isize` |
| `WindowFuncDesc` 嵌入 `baseFuncDesc` | `WindowFuncDesc { base: BaseFuncDesc }`，同样 `Deref` | |
| `AggFunctionMode int` 与 5 个常量 | `#[repr(isize)]` 的枚举，变体名照 Go（`CompleteMode` 等，M-GONAME） | `Hash64` 里的 `int(a.Mode)` 不移植（EXC.B） |
| `[]*AggFuncDesc`（计划节点中） | 计划节点按值持有 `Vec<AggFuncDesc>` | 见 §4.3 和决定点 4 |
| `RetTp == nil` | 不表示 | 范围内只有一处会出现 nil（lead/lag 三个参数时 `InferType4ControlFuncs` 出错，§8 第 4 条）。`ret_tp` 不做成 `Option`，那一处单独处理，见决定点 8。`NewAggFuncDescForWindowFunc` 的 nil 检查（`descriptor.go:57-59`）在 Rust 中不可达（L2） |

方法命名：`Clone()` 与 Rust 的 `Clone` trait 同名，按 `ByItems.clone_item` 的先例写成固有方法 `clone_desc`（`AggFuncDesc`、`WindowFuncDesc`），`baseFuncDesc.clone` 写成 `clone_base`（M-NAME）。结构体本身 `#[derive(Clone)]`，那是共享 `Arc` 的浅拷贝，用在 Go 复制值的地方（`clone := *a`，`descriptor.go:139`）。

### 4.2 类型推导和参数改写

- `TypeInfer` 和各个 `typeInfer4*` 按 L1 逐行移植：分支顺序、`SetFlenUnderLimit` 等的调用顺序、flag 的设置方式（`SetFlag` 是整体替换，`AddFlag` 是或上）都照抄，包括 §8 列出的看起来有问题的写法。
- 改 `a.RetTp` 的地方：新建的类型直接改；从参数取来的类型按 M-FT 先 `Arc::make_mut`（写时复制）。逐处结论见 §5。
- 改 `a.Args[i]` 的地方（`base_func.go:297`、`:312`、`:332`、`:343`、`:475`）：替换 `Vec<ExprRef>` 中的元素。D08.8 的 `build_cast_function`、`wrap_with_cast_as_*` 在 Rust 中返回 `Result`（D08.8 的 L2：Go 记日志后留下一个签名为 nil 的节点），所以 `type_infer`、`wrap_cast_for_agg_args` 返回 `Result`，把这个错误往上传，见 §7 第 1 条。
- `WrapCastForAggArgs` 的 `castFunc` 是按 `EvalType` 选出的函数指针（`base_func.go:444-466`），Rust 用一个 `match` 得到闭包或函数指针，循环照抄（跳过 lead / lag / nth_value 的第二个参数和 NULL 类型的参数）。
- `typeInfer4ApproxPercentile` 的报错顺序照抄：参数个数 → 是否常量 → 求值出错（丢弃原错误，换成 `Invalid argument` 文本，`base_func.go:186-189`）→ NULL → 越界（`:190-195`）。这些都是 Go 的 `errors.New` / `fmt.Errorf`，没有错误码，Rust 用 `tidb_base::errors::errorf` / `Error::plain`，到客户端都是 1105。
- `typeInfer4Avg` 读 `ctx.GetDivPrecisionIncrement()`，Rust 的 `EvalContext` 已有这个方法。

### 4.3 描述在计划中的持有方式与修改

Go 的计划节点持有 `[]*AggFuncDesc`。多数从逻辑计划到物理计划的地方先 `Clone`（如 `NewPhysicalHashAgg`，`pkg/planner/core/physical_plans.go:2320-2327`，注释说明是为了避免原地改写影响逻辑计划），之后原地改：`WrapCastForAggFuncs`（`pkg/planner/util/coreusage/cast_misc.go:24-31`，由 `InjectProjBelowAgg` 在后优化阶段调用，`pkg/planner/core/rule_inject_extra_projection.go:121`）、`TypeInfer4AvgSum`（`pkg/planner/core/task.go:1587`）、列下标的解析等。

建议：计划节点**按值**持有 `Vec<AggFuncDesc>`。`AggFuncDesc` 的字段都是 `Arc` 或小值，克隆便宜；Go 调 `Clone()` 的地方对应 `clone_desc()`，其余 Go 原地改的地方在 Rust 中改本节点的副本：逻辑计划在 arena 中可直接改（N1），物理计划经 `make_mut_plan` 拿到可写节点后改（N1、`crates/executor/clippy.toml` 禁止执行器这样做）。Go 中有没有"依赖多个计划节点共享同一个 `*AggFuncDesc`、改一处其他地方都看得到"的写法，属于调用方的审计（D09.4、D09.9、D10.1、D10.3 的 spec 逐处列出），本块只提供类型。物理计划会进实例级 plan cache、跨会话共享，所以**执行期只读**：执行器不改描述（Go 的执行器构建中 `Split` 是新建描述而不是原地改，与按值持有相容）。备选是 `Vec<Arc<AggFuncDesc>>`，见决定点 4。

### 4.4 并发、取消与 KILL

本块没有 goroutine、channel、锁和后台任务（`grep 'go func\|chan \|sync\.'` 为空）。描述在规划期由会话的 task 同步构建和读取，不涉及 M-GO / M-CH / M-WG / M-LOCK，也没有要放 `checkpoint` 的循环（参数数量很小）。`AggFuncDesc` 只含 `Arc` 和普通值，是 `Send + Sync`，可以随物理计划进 plan cache 跨会话共享（只读）。

### 4.5 内存记账与落盘

- 本块不调用 `memory.Tracker`，不需要 M-MEM。`MemoryUsage`（`base_func.go:480-493`、`descriptor.go:369-379`）是计划大小的估算，plan cache 用，按已有先例移植（L1；用到 Go 结构大小的写 L2，理由同 `Plan.MemoryUsage`："Go's size on 64-bit targets"），常量用 `crates/expression/src/size.rs`。
- 没有落盘路径，不涉及 `blocked-by:D01.10`。

### 4.6 错误、panic 和 `defer`

- 本包没有 `defer`（`grep defer` 为空），不需要 drop guard。
- Go 的 `errors.Errorf` / `errors.New` / `fmt.Errorf`：用 `tidb_base::errors` 的对应函数，文本照抄。
- 显式 `panic`：`base_func.go:465`（`WrapCastForAggArgs` 遇到未知 `EvalType`）、`descriptor.go:245`（`EvalNullValueInOuterJoin` 遇到不支持的函数）、`descriptor.go:274`（`GetAggFunc`，归 X）、`avg.go:68`（归 X）。前两处从 SQL 到不了（调用方先按函数名筛过：`EvalNullValueInOuterJoin` 只用于聚合下推穿过外连接，被下推的函数先经 `isDecomposableWithJoin` 限定为 max / min / firstrow / sum / count，`pkg/planner/core/rule_aggregation_push_down.go:47-61`），按 M-PANIC 保留为同样文本的 `panic!`，由 task 边界兜底。
- 隐式的 nil 解引用：只有 §8 第 4 条一处，见决定点 8。
- `ExplainAggFunc` 中的 failpoint `show-agg-mode`（`explain.go:30-34`）是测试钩子，不移植（`showMode` 恒为 false）。

### 4.7 `ByItems` 移到 `tidb-expression`

按 N0 §4 已定的"下沉"：

1. tracker `rules.py` 把 `pkg/planner/util/byitem.go` 改归 D08.11（重新生成 `func_blocks.tsv`）；
2. `crates/planner/src/util/byitem.rs` 整文件移到 `crates/expression/src/`（文件名照 Go 叫 `byitem.rs`，来源标注不变）；
3. `tidb-planner` 的 `util` 模块 `pub use` 它，现有调用方不改。

`StringifyByItemsWithCtx` 只被规划器调用，但同文件一起移（文件对应文件）。这一步不改任何行为，`check_one_translation.py` 保证不会留下两份。见决定点 2。

### 4.8 要用 D08.3 的函数放在 `tidb-expr-pushdown`

N0 §4 的反向引用表和 D08.3 plan 已定：aggregation 中调用下推和 pb 转换的函数"上移到 D08.3 的 crate，写成自由函数"（N0 规则 6）。具体是第 B 部分的 8 个和第 C 部分的 2 个，放在 `crates/expr-pushdown/src/aggregation/`（`agg_to_pb.rs`、`aggregation.rs` 中的下推判定、`window_func.rs` 中的两个），接收者是下层类型的方法（`baseFuncDesc.GetTiPBExpr`、`WindowFuncDesc.CanPushDownToTiFlash`）写成自由函数 `get_tipb_expr(desc: &BaseFuncDesc, …)` 等，来源标注照常写 Go 的方法名。`tidb-expression` 不依赖 tipb，所以 `GetTiPBExpr` 也只能放在这里。

tracker 中这 10 个函数归哪个块（留在 D08.11 还是改归 D08.3）见决定点 3。

### 4.9 其他映射规则

- **M-IFACE**：`a.Args[0].(*expression.ScalarFunction)`（`base_func.go:303`）、`result.(*expression.Constant)`（`descriptor.go:284` 等）写成对 `Expr` 的 `match`。
- **M-INT**：`Split` 的 `ordinal []int` 是列下标，与 `Column.index`（`isize`）同型；`SetFlenUnderLimit(flen + 21)`、`min(MaxDecimalWidth, flen+22)` 不会溢出（flen 来自 `FieldType`，范围有限），照常写。
- **M-MAPORDER**：三个包级 map 只做查找，不遍历，没有顺序问题；Rust 用 `match` 或静态集合。
- **M-FMA / M-F2I**：tracker `data/go_fma_sites.tsv` 中本包没有位置；范围内的函数没有浮点转整数。
- **新代码不进默认路径**：描述只由新栈的规划器（D09.4 等）构建，`--new-stack=off` 时不会被调用；不需要额外开关。

## 5. 共享修改的审计（M-FT，N1 §5）

对本包非测试文件运行 plan 模板 §4 的两个 grep，并读代码补充。结论分类见 [D09.7 试点报告](../D09.7-column-pruning/pilot-report.md) 附录 B（S：只改本处新建或已复制的对象；V0：共享但不可见；V1：可能改变 golden test / EXPLAIN 对照或客户端能看到的结果）。

| Go 位置 | 改的是什么 | 结论 | 处理 | 负责人结论 |
|---|---|---|---|---|
| `base_func.go:166-386` 各 `typeInfer4*` 对 `a.RetTp` 的 `Set*` / `AddFlag` | 刚 `NewFieldType` 新建，或 `Clone()` 过（`:210`、`:212`、`:317`、`:254`） | S | 照抄 | |
| `base_func.go:314` 之后的写入（`typeInfer4MaxMin` 对 firstrow、first_value、last_value、nth_value 让 `a.RetTp` 直接指向参数的类型，不复制） | 自身不写；但后面的写入会改到参数的类型，见下面两行 | — | Rust：`ret_tp = Arc::clone(参数类型)` | |
| `descriptor.go:361-363` `UpdateNotNullFlag4RetType` | 先 `Clone()` 再 `DelFlag` | S | 照抄（`Arc::make_mut` 写时复制，结果相同） | |
| `window_func.go:74`、`:76` `NewWindowFuncDesc` 的 `DelFlag(NotNullFlag)`，函数为 first_value / last_value / nth_value 时 | `base.RetTp` 就是参数的类型（`:314`）。参数通常是子节点 schema 中的列，它的 `RetType` 指针又被窗口算子的输出 schema 共享（`pkg/planner/core/logical_plan_builder.go:7251` `np.Schema().Clone()` 只复制列，不复制类型） | **疑似 V1** | Go 中 `SELECT a, first_value(a) OVER () FROM t`（`a NOT NULL`）的输出列 `a` 可能因此丢掉 NOT NULL，客户端的列定义能看到。Rust 默认写时复制，只改描述的副本。第 C 部分用计划向量或结果元数据向量确认；确认可见就按 M-FT 在这一处模拟（`// Go aliasing (M-FT): window_func.go:76, ...`，配回归测试），否则登记为 V0。见决定点 6 | |
| `window_func.go:66`、`:71` `SetFlag(NotNullFlag)` | count、bit_*、row_number 等是新建的类型；lead / lag 三个参数时是 `InferType4ControlFuncs` 新建的类型 | S | 照抄（行为问题见 §8 第 2、3 条） | |
| `base_func.go:248` `TypeInfer4AvgSum` 原地改 `a.RetTp` | 调用方（`pkg/planner/core/task.go:1577-1587`）先 `TypeInfer` 重建了类型 | S | 照抄 | |
| `descriptor.go:164`、`:174`、`:188` `Split` 让 final 描述和新建的 `Column` 共享 `a.RetTp` | 之后没有人通过它们写类型（D12.4 只读） | V0 | 共享同一个 `Arc` | |
| `base_func.go:297` `BuildCastFunction(ctx, a.Args[i], tp)`，`tp` 是参数自己的类型指针 | `BuildCastFunctionWithCheck` 只在参数可空时 `DelFlag(NotNullFlag)`（`pkg/expression/builtin_cast.go:2469-2471`），对这个 `tp` 是空操作 | S | Rust 的 `build_cast_function` 按值接收类型 | |
| `base_func.go:445`、`:456` `WrapCastForAggArgs` 的时间分支把 `retTp`（就是 `a.RetTp` 本身）交给 `WrapWithCastAsTime` | `WrapWithCastAsTime` 就地改这个类型（decimal、flen、flag），`BuildCastFunctionWithCheck` 在参数可空时再 `DelFlag(NotNullFlag)`，建出的 CAST 节点也指向同一个类型；所以循环中后面的参数看到的是前面改过的类型，最后描述的返回类型和每个 CAST 的类型是同一个值。CAST 被折叠成常量时，常量得到的是一份复制（`pkg/expression/constant_fold.go:234`） | **V1**（govec `wrap` / `new` 记录可见：返回类型的 decimal、NOT NULL 与逐个复制不同） | 按 M-FT 模拟（`// Go aliasing (M-FT): base_func.go:456`，`shared_time_tp_after`）：循环中带着同一个可变类型，结束后把它写回描述的 `ret_tp` 和每个被包的 CAST 的类型；由 govec `aggdesc` 的 `wrap` / `new` 记录钉住。已在 v8.5.8 上从 SQL 复现并记为 `upstream-pitfall` issue #357：`LEAD` / `LAG` 的默认值是 TIME、第一个参数是 DATETIME(n) / TIMESTAMP(n) 时，结果列报 fsp 0，取自源列的值仍带小数秒；默认值是字符串时类型推成 VARCHAR，不走时间分支，不出现 | 实现时发现，待负责人确认 |
| `base_func.go:332` `WrapWithCastAsInt` 对 ENUM 参数 | D08.8 已处理（Go 对 `Column` 先复制，其他节点原地改） | 随 D08.8 | 不另做 | |

调用方对 `desc.RetTp` 的原地写入（如 D09.9 的聚合下推、D09.4 的 grouping sets 可空性调整）由各自的 spec 审计。

## 6. 测试计划

### 6.1 要移植的 Go 单测

| Go 测试 | 处理 |
|---|---|
| `base_func_test.go`：`TestClone`、`TestBaseFunc_InferAggRetType` | 第 A 部分移植 |
| `agg_to_pb_test.go`：`TestAggFunc2Pb` | 第 B 部分移植（比较 tipb 的 JSON；用 D08.3 测试已有的 mock client） |
| `aggregation_test.go`：`TestAggFuncDesc` | 不移植：测的是 `Hash64`（EXC.B） |
| `aggregation_test.go`：`TestAvg`、`TestAvgFinalMode`、`TestSum`、`TestBitAnd`、`TestBitOr`、`TestBitXor`、`TestCount`、`TestConcat`、`TestFirstRow`、`TestMaxMin`；`util_test.go`：`TestDistinct`；`bench_test.go` | 不移植：测的是旧求值器（归 X，决定点 1）。如果决定移植旧求值器，这些测试随之移植 |

### 6.2 govec

新领域 `aggdesc`（tracker `govec/domains/aggdesc.go`，Rust 回放 `crates/expression/tests/govec_aggdesc.rs`，第 B、C 部分的 pb 相关记录回放在 `crates/expr-pushdown/tests/`）。Go 侧直接调用 v8.5.8 的函数（未导出的经 `go:linkname`，与 `pointget` 领域的做法相同）。每条记录输出完整的返回类型（tp、flag、flen、decimal、charset、collate、elems）、改写后的参数（`StringWithCtx` 和类型）、错误。两个目标（amd64 / arm64）的向量都在本地生成（作者的环境下载不到 CI 的产物），由 CI 的 `scripts/govec.sh --check` 在两个架构的 runner 上各自重新生成、逐字节核对已提交的文件。

| op | 覆盖 | 输入 |
|---|---|---|
| `new_agg_desc` | `NewAggFuncDesc`、`newBaseFuncDesc`、`TypeInfer` 与全部 `typeInfer4*` | `TypeInfer` 中的每个函数名（含窗口函数名、未知名字）× 参数：各 MySQL 类型的列（带 / 不带 NOT NULL、UNSIGNED，flen / decimal 含 -1、0、上限和越界值）、常量（含 NULL）、标量函数（覆盖 FLOAT 包 CAST 的分支）、ENUM / SET；approx_percentile 的百分比取 0、1、50、100、101、NULL、非整数、非常量和参数个数不对；`div_precision_increment` 取 0、4、30；hasDistinct 两种 |
| `wrap_cast_for_agg_args` | `WrapCastForAggArgs` | 在上一行的结果上调用，覆盖每个 `EvalType` 分支和三处跳过的条件 |
| `update_not_null_flag` | `UpdateNotNullFlag4RetType` | 函数名 × hasGroupBy × allAggsFirstRow × BIT 类型 |
| `default_value` | `GetDefaultValue` | 每个函数名；approx_count_distinct 的返回类型取 STRING 与否 |
| `eval_null_outer_join` | `EvalNullValueInOuterJoin` 与 4 个辅助函数 | 参数为内表列、外表列、常量、NULL 常量、含列的表达式；schema 随机 |
| `split` | `Split`、`Clone`、`clone` | 4 种 mode × avg、approx_count_distinct、group_concat、approx_percentile、其他 |
| `infer_avg_sum`、`infer_final_count` | `TypeInfer4AvgSum`、`TypeInfer4FinalCount` | avg 返回类型为 DECIMAL 与否；flen 接近上限 |
| `string`、`equal`、`memory_usage` | `StringWithCtx`（两个）、`Equal`、`equal`、`MemoryUsage`（两个） | 带 distinct、带 ORDER BY 的 group_concat；同名不同参数等 |
| `misc` | `ToString`、`NeedCount`、`NeedValue`、`IsAllFirstRow` | 全部取值 |
| `check_push_down`（第 B 部分） | `CheckAggPushDown`、`checkVectorAggPushDown`、`CheckAggPushFlash` | 存储类型 TiKV / TiFlash / TiDB / UnSpecified × 函数名 × 参数类型（VECTOR、DURATION、JSON）× ORDER BY；`expr_pushdown_blacklist` 中有无该函数 |
| `agg_to_pb`、`pb_to_agg`、`mode_pb`（第 B 部分） | `AggFuncToPBExpr`、`GetTiPBExpr`、`AggFunctionModeToPB`、`PBAggFuncModeToAggFuncMode`、`PBExprToAggFuncDesc` | 每个函数名 × mode × distinct；group_concat 的 ORDER BY 与 `group_concat_max_len`；client 不支持的类型；pb 侧未知的函数类型、`AggFuncMode` 为空 |
| `new_window_desc`（第 C 部分） | `NewWindowFuncDesc`、`NewAggFuncDescForWindowFunc`、`WindowFuncDesc.Clone` | 每个窗口函数名和可作窗口的聚合函数名，**名字分大写和小写两种**（§8 第 2 条）；nth_value / ntile 的 0、NULL、非常量；lead / lag 的偏移量为 NULL、非常量，默认值与第一个参数 collation 冲突（§8 第 4 条）；skipCheckArgs 两种 |
| `window_frame`、`window_to_pb`、`window_tiflash`（第 C 部分） | `UseDefaultFrame`、`NeedFrame`、`WindowFuncToPBExpr`、`CanPushDownToTiFlash` | 函数名大小写；参数能否下推 |
| `explain`（第 D 部分） | `ExplainAggFunc` | normalized 两种；group_concat 带 ORDER BY 与分隔符 |

领域的 `Coverage` 覆盖第 A–D 部分的 58 个函数；`Exempt` 登记 EXC.B 的 4 个和归 X 的 49 个（理由写明"只有 mock store 调用"）。Go panic 的记录（§8 第 4 条那类）按 govec README 记为 `panic`，Rust 侧的要求见决定点 8。

实现（第 A + B 部分）：op 的名字取得更短，对应关系是 `new`（`new_agg_desc`）、`wrap`、`notnull`、`default`、`outer`、`split`、`avgsum` / `finalcount`、`string` / `equal` / `mem`、`names` / `mode` / `allfirst`（`misc`）、`window`（`NewAggFuncDescForWindowFunc`）、`push`、`topb` / `tipb`、`mode2pb` / `pb2mode`、`frompb`；格式见 tracker `govec/domains/aggdesc.go` 的文件头。种子 20261124，5575 条记录（第 A 部分 4531 条，第 B 部分 1044 条），两个目标的记录相同。第 C 部分的窗口函数和第 D 部分的 `ExplainAggFunc` 暂时登记在 `Exempt` 中（理由写明部分），随各自的部分移入 `Coverage`。

M-FT 疑似 V1（§5）用规划器的计划向量（`selectbuild` 领域或 VS1 plan §7.1 的 `plan` 领域）确认：查询的输出列类型（含 flag）与 Go 逐列比较。这要等 D09.4 的窗口构建，在第 C 部分处理。

### 6.3 其他

- 单元测试：`ByItems` 移动后，`tidb-planner` 现有测试不改即通过；`AggFuncDesc` 是 `Send + Sync` 的静态断言；`clone_desc` 后改参数不影响原描述（`TestClone` 的 Rust 版）。
- 端到端：sysbench 的 SUM / DISTINCT 要等 D09.4、D09.9、D10.1、D10.3、D12.3、D12.4 接上，属于集成验收（`tools/ab`），不在本块。

## 7. 与 Go 的已知差异

1. **CAST 构建失败时**：Go 的 `BuildCastFunction` 记日志后返回签名为 nil 的节点（`pkg/expression/builtin_cast.go:2459-2463`），本包把它放进参数（`base_func.go:297`、`:312`、`:332`、`:343`、`:475`），之后求值时才出错或 panic；Rust 沿用 D08.8 的 L2，`type_infer` / `wrap_cast_for_agg_args` 直接返回这个错误。只在 CAST 本身构建失败时不同，govec 未必能构造出来（如构造得出，按错误记录钉住）。
2. **`RetTp` 为 nil 的表示**：Go 的 `RetTp` 可以是 nil（`base_func.go:57` 的 `Hash64`、`descriptor.go:57` 的检查）。按决定点 8 的 A，Rust 的 `ret_tp` 不做成 `Option`，而用内部标记 `ret_tp_is_nil()` 表示 Go 的 nil（只有 `typeInfer4LeadLag` 的错误路径会设）；`NewAggFuncDescForWindowFunc` 的 nil 分支（`descriptor.go:57-59`）因此照 Go 逐行移植（L1），`MemoryUsage` 同样跳过 nil 的类型，`WrapCastForAggArgs` 在 nil 上以 Go 的空指针文本 panic。只是表示方式不同，没有可见差异。
3. **`Clone` 不深拷贝**：Go 的 `clone` 复制 `FieldType` 值、对每个参数调 `Clone()`（`base_func.go:98-107`、`descriptor.go:138-146`）；Rust 共享 `Arc`，靠写时复制（M-FT、M-EXPR），之后的修改互不影响，结果相同。
4. **`NewWindowFuncDesc` 返回 `nil, nil`**（`window_func.go:41`、`:47`、`:55`，参数不合法，调用方报 `ErrWrongArguments`，`pkg/planner/core/logical_plan_builder.go:7264-7266`）：Rust 返回 `Ok(None)`，只是表示方式不同。
5. **不移植的部分**：cascades 的 `Hash64` / `Equals`（§2.4）；旧求值器（§2.3，决定点 1）；failpoint `show-agg-mode`（`explain.go:30-34`）。tidb-rs 没有 cascades、没有 mock store，所以没有用户可见的影响。
6. 如果决定点 6 确认需要模拟，模拟位置登记在 §5 的表中；如果决定点 7、8 选择不照搬 Go，在这里补上对应的差异和 `docs/testing/rulings.toml` 的条目。

## 8. 可能的 Go 问题（读代码发现，尚未在运行中的 v8.5.8 上复现）

按宪章第一条 2，默认照 Go 移植。每条都要在 v8.5.8 上复现后，按 CLAUDE.md 规则 6 在本仓库开英文 issue（标签 `upstream-pitfall`），再在这里引用 issue 号。本 spec 没有开 issue（理由见决定点 9）。

1. **`typeInfer4PercentRank` 把长度写成了 flag**：`a.RetTp.SetFlag(mysql.MaxRealWidth)`（`base_func.go:368`），应是 `SetFlen`。`MaxRealWidth` = 23 = NOT_NULL | PRI_KEY | UNIQUE_KEY | BLOB（`BlobFlag` = 1<<4，`pkg/parser/mysql/type.go:58`）。`percent_rank` 只能作窗口函数，函数名是小写 `percent_rank` 时 `window_func.go:66` 的 `SetFlag(NotNullFlag)` 又把 flag 整体换成 NOT NULL，剩下的影响是 flen 为 -1（同类的 `cume_dist` 是 23）；写成大写 `PERCENT_RANK` 时走 `:76` 的 `DelFlag`，flag 剩 PRI_KEY | UNIQUE_KEY | BLOB（见下一条）。只影响结果的列定义。
2. **窗口函数的 NOT NULL 调整区分函数名大小写**：`NewWindowFuncDesc` 做参数检查时用 `strings.ToLower(name)`（`window_func.go:36`），调整 NOT NULL 时却用原样的 `name`（`:63`）。`name` 来自 `ast.WindowFuncExpr.Name`，即 SQL 中原样的写法（lexer 的 `v.ident = lit`，`pkg/parser/lexer.go:246`；规划器不转小写，`pkg/planner/core/logical_plan_builder.go:7260`）。所以 `ROW_NUMBER() OVER ()` 的结果列可空，`row_number() over ()` 的不可空；`COUNT(a) OVER ()` 与 `count(a) over ()` 的 flag 也不同。影响列定义的 NOT NULL，可能还影响依赖可空性的改写。Rust 要照抄：传给 `NewWindowFuncDesc` 的名字必须是原样的写法（实现时核对 tidb-rs parser 的 `WindowFuncExpr.name` 是否保留大小写），govec 的 `new_window_desc` 两种都测。
3. **`bit_and` / `bit_or` / `bit_xor` 作窗口函数时丢掉 UNSIGNED（疑似结果错误）**：`typeInfer4BitFuncs` 设了 UNSIGNED | NOT NULL（`base_func.go:331`），小写函数名时 `window_func.go:66` 的 `SetFlag(NotNullFlag)` 整体替换，UNSIGNED 没了。窗口算子输出列的类型就是 `desc.RetTp`（`pkg/planner/core/logical_plan_builder.go:7273` `RetType: desc.RetTp`），执行器的 `bitAndUint64` 等写入 uint64（`pkg/executor/aggfuncs/builder.go:540-546`；`pkg/executor/aggfuncs/func_bitfuncs.go:44-48` 的 `baseBitAggFunc.AppendFinalResult2Chunk`），而服务端按有符号输出 LONGLONG（`pkg/server/internal/column/column.go:174-179`）。因此 `bit_and(a) over ()` 在结果 ≥ 2^63 时（例如帧内全是 NULL 时的 18446744073709551615）可能显示为负数，MySQL 显示无符号值。大写函数名走 `DelFlag`，不受影响。如果复现，这属于宪章第一条 2 的"结果错误"，要负责人裁定，见决定点 7。
4. **lead / lag 三个参数时忽略类型推导的错误，随后解引用 nil**：`a.RetTp, _ = expression.InferType4ControlFuncs(...)`（`base_func.go:378`，旁边的 FIXME 就是这个例子）。第一个和第三个参数的 collation 不能合并时（如 `lead(b COLLATE utf8mb4_unicode_ci, 1, 'x' COLLATE utf8mb4_general_ci) OVER ()`），`InferType4ControlFuncs` 返回 `nil, err`（`pkg/expression/builtin_control.go:284-287`），`RetTp` 为 nil，`newBaseFuncDesc` 不报错，`NewWindowFuncDesc` 在 `window_func.go:71` 或 `:74` 对 nil 调 `SetFlag` / `DelFlag` 而 panic，被 `planner.Optimize` 的 `recover` 转成错误（`pkg/planner/optimize.go:139-143`，`util.GetRecoverError`），客户端收到 1105 `runtime error: invalid memory address or nil pointer dereference`，而不是 collation 冲突的错误。见决定点 8。
5. **`AggFuncDesc.Equals` 恒为 false**：`a.baseFuncDesc.Equals(otherAgg.baseFuncDesc)` 传的是值（`descriptor.go:92`），`baseFuncDesc.Equals` 里 `other.(*baseFuncDesc)` 的断言失败（`base_func.go:70-73`）。只有 cascades 用（EXC.B，不移植），对 tidb-rs 没有影响，只记录。
6. `NewWindowFuncDesc` 先用 `base.RetTp` 再检查 `err`（`window_func.go:60-81`）：`TypeInfer` 出错且没设 `RetTp` 时会 nil 解引用。范围内能走到的只有第 4 条；`approx_percentile` 不能带窗口子句（parser 中没有 `OptWindowingClause`，`pkg/parser/parser.y:8487`），到不了。

## 9. 需要负责人决定的点

**负责人 2026-11-24 确认：全部选 A**（Raft，由 Maya 在 PR #341 转达）。

Duet 不能确认 spec，下面每一点都请负责人决定；括号内是建议。

1. **旧求值器（49 个函数，只有 mock store 用）怎么登记？**
   - A：tracker 改归 X，不移植（与 mock store 相同；不需要桩）。
   - B：照样移植，配 Go 单测和 govec（111 个函数中有 49 个，都没有调用者）。
   - C：登记为豁免（例外）。
   - （建议 A。理由：tidb-rs 不带 mock store，真正的执行器用 D12.4；改归 X 只是 tracker 的归属，不是行为差异。）
2. **`ByItems` 的位置**（`AggFuncDesc.OrderByItems` 需要它，而它现在在 `tidb-planner`）。
   - A：执行 N0 §4 已定的下沉：tracker 把 `byitem.go` 改归 D08.11，Rust 文件移到 `tidb-expression`，规划器 `pub use`（§4.7）。
   - B：`AggFuncDesc` 对排序项类型做泛型，或在 `tidb-expression` 定义 trait 由规划器实现（N0 规则 2 的接口反转）。
   - （建议 A：N0 已经这样决定，`ByItems` 本身只依赖表达式；B 让一个纯数据类型多一层间接。）
3. **第 B、C 部分中要用 D08.3 的 10 个函数在 tracker 里归哪个块？**（代码按 N0 规则 6 放在 `tidb-expr-pushdown`，这一点不变。）
   - A：留在 D08.11，D08.11 的 converge 统计包含 `tidb-expr-pushdown` 中的这些文件。
   - B：改归 D08.3（D08.3 已标为完成 50/50，要重新打开）。
   - （建议 A：不动已完成的块；`layers.toml` 的"一个块一个 crate"按块的主体判断，这里在 spec 中写明例外。如果负责人认为 `layers.toml` 的规则必须严格成立，则选 B。）
4. **计划节点怎么持有聚合函数描述？**
   - A：按值 `Vec<AggFuncDesc>`，Go `Clone()` 处 `clone_desc()`，原地修改改本节点的副本（物理计划经 `make_mut_plan`）。
   - B：`Vec<Arc<AggFuncDesc>>`，修改经 `Arc::make_mut`。
   - （建议 A：描述的字段已经是 `Arc`，按值克隆便宜，也不需要第二层写时复制；Go 自己在逻辑→物理时就先 `Clone`。这一点影响 D09.4、D09.9、D10.1、D10.3、D12.3、D12.4 的写法，所以请在这些 spec 之前定下来。）
5. **部分怎么拆成实现任务？**
   - A：第 A + B 部分一个任务（51 个函数，sysbench 链路需要的全部），第 C 部分在 D09.4 的窗口构建之前单独一个任务，第 D 部分在 D10.9 P1-1 之后并入当时的任务。
   - B：A、B、C、D 各一个任务。
   - （建议 A：第 B 部分只有 8 个函数，而 sysbench 的 SUM 下推离不开它；`ByItems` 的移动放在 A + B 任务的第一个提交。）
6. **M-FT 疑似 V1（§5：`window_func.go:76` 经共享的参数类型改掉子节点列的 NOT NULL）**：确认可见后是否按 M-FT 模拟？
   - A：在第 C 部分用计划或结果元数据向量确认；可见就在这一处模拟 Go 的别名效果。
   - B：不模拟，作为已知差异登记（Rust 的列保留 NOT NULL）。
   - （建议 A，这是 M-FT 规则的默认做法：影响发给客户端的列定义。）
7. **§8 第 3 条（窗口 `bit_*` 丢 UNSIGNED，疑似结果错误）复现后怎么办？**
   - A：按宪章第一条 2 的例外修正：保留 UNSIGNED（与 MySQL 一致），开 `upstream-pitfall` issue，在 `docs/testing/rulings.toml` 登记裁定（`approver = "pending-owner"`，`test` 指向钉住它的测试）。
   - B：照 Go 移植（结果与 Go 一致，与 MySQL 不同），只开 issue。
   - （建议 A，前提是复现确认是结果错误；如果只是元数据不同、显示正确，则 B。）
8. **§8 第 4 条（lead / lag collation 冲突时 Go 返回 nil 解引用的运行时错误）怎么移植？**
   - A：照 Go：在 `NewWindowFuncDesc` 中 Go 解引用的同一位置返回同样文本的 1105 错误（`ret_tp` 不做成 `Option`，`typeInfer4LeadLag` 用内部标记把"类型为 nil"带出来），开 `upstream-pitfall` issue。
   - B：返回 `InferType4ControlFuncs` 的 collation 错误（与 MySQL 的报错接近，是有意的差异，需要裁定）。
   - （建议 A：两种都是语句失败，不涉及安全或结果正确性，按宪章默认照搬。）
9. **§8 的可能问题现在开 issue，还是复现后再开？**
   - A：由第 C 部分的实现任务在 v8.5.8 上复现（govec 可以直接调用这些函数，第 3 条还要一次端到端查询），复现后开 issue 并更新本 spec。
   - B：现在按读代码的结论开 issue，注明尚未复现。
   - （建议 A：CLAUDE.md 要求 issue 写明在 v8.5.8 上怎么复现；本任务没有运行中的集群，也不改 tracker。）

## 本文所用 ID

- D00.1：上下文接口骨架（sessionctx、exprctx 等）
- D00.3：`util/*` 叶子工具文件（按需整文件移植）
- D01.1：Datum 与 FieldType 等基础类型
- D01.10：落盘容器与磁盘 chunk
- D02.1：parser
- D02.3：parser 的 model / types
- D08.1：表达式框架
- D08.3：下推判定与 protobuf 转换（crate `tidb-expr-pushdown`）
- D08.8：CAST
- D08.9：控制流、信息、杂项函数
- D08.11：本块，聚合函数描述（expression/aggregation）
- D08.12：表达式工具函数
- D09.0：planbuilder
- D09.2：表达式改写
- D09.3：join 构建与逻辑 join 算子
- D09.4：聚合、窗口、only_full_group_by 的构建与逻辑算子
- D09.6：去相关、半连接改写
- D09.7：列裁剪与投影消除
- D09.9：聚合相关规则（下推、消除、skew、max/min）
- D09.13：优化器驱动、规则框架、其他规则
- D09.14：逻辑算子公共部分与基本逻辑改写规则
- D10.1：物理算子定义与 task
- D10.3：物理计划枚举（exhaust_physical_plans）
- D10.5：代价模型 v2
- D10.9：后处理与 EXPLAIN 输出（含 `pkg/expression/explain.go`，其 tasks 的 P1-1）
- D10.10：物理属性与优化器工具（`pkg/planner/util`）
- D12.1：读取类执行器（含 coprocessor 服务端 `pkg/executor/coprocessor.go`）
- D12.3：聚合执行器框架（hash / stream agg）
- D12.4：聚合函数实现（aggfuncs）
- D12.9：其他执行器（引用 1 处）
- EXC.B：例外 B，废弃或非默认路径（cascades、代价模型 v1 等），不移植
- X：不在 tidb-rs 范围内的代码（DDL、写路径、mock store、测试工具等）
- N0：crate 分层的设计（§4 是反向引用的处理，规则 6 是跨 crate 的方法）
- N1：计划表示的设计（arena、`make_mut_plan`）
- N13：共享元数据的设计
- M-BYTES、M-EXPR、M-FT、M-GONAME、M-IFACE、M-INT、M-MAPORDER、M-NAME、M-PANIC、M-SELF、M-FMA、M-F2I、M-GO、M-CH、M-WG、M-LOCK、M-MEM：`docs/porting/mappings.md` 中的映射规则（依次为：Go string 的承载、表达式共享、FieldType 共享与修改、包级常量命名、接口与类型断言、定宽整数、map 顺序、与 Rust trait 同名的方法、panic、struct 嵌入、浮点乘加、浮点转整数、goroutine、channel、WaitGroup、锁、内存记账）
- L1 / L2：来源标注的等级（L1 逐行对应；L2 有理由的改写）
- S / V0 / V1：M-FT 审计的结论（只改本处对象 / 共享但不可见 / 可能可见）
- VS1：第一条纵向切片（VS1 plan §7.1 是计划向量）
- P3：ROADMAP 的第三阶段
