# D08.3 下推判定与 protobuf 转换：spec

状态：**第 1 部分**——下推判定（`infer_pushdown.go`）与表达式到 tipb 的转换（`expr_to_pb.go`），新 crate `tidb-expr-pushdown`（N0 rank 12）。第 2 部分为反方向的 `distsql_builtin.go`（`PBToExpr`，按 pb code 构造各函数族的签名）。

收敛检查（第 1 部分）：D08.3 matched **34/50**（第 1 部分之前 0/50），missing 0，extra 0，duplicated 0；其余 16 个是第 2 部分 `distsql_builtin.go` 的函数（tracker 豁免表中写 "D08.3 part 2"）。govec `pushdown` 领域 7,259 条（§5）。

## 1. 范围（第 1 部分）

| Go | Rust（`crates/expr-pushdown/src/`） |
|---|---|
| `expr_to_pb.go`（13 个）：`ExpressionsToPBList`、`ProjectionExpressionsToPBList`、`NewPBConverter`、`PbConverter.ExprToPB`、`conOrCorColToPBExpr`、`encodeDatum`、`columnToPBExpr`、`scalarFuncToPBExpr`、`ToPBFieldType`、`ToPBFieldTypeWithCheck`、`FieldTypeFromPB`、`GroupByItemToPB`、`SortByItemToPB` | `expr_to_pb.rs` |
| `infer_pushdown.go`（21 个）：`canFuncBePushed`、`canScalarFuncPushDown`、`canExprPushDown`、`scalarExprSupportedByTiDB/TiKV/Flash`、`canEnumPushdownPreliminarily`、`IsPushDownEnabled`、`PushDownContext` 及其 5 个方法、`NewPushDownContext(FromSessionVars)`、`PushDownExprs(WithExtraInfo)`、`CanExprsPushDown(WithExtraInfo)`、`storeTypeMask`、`init`；包级变量 `DefaultExprPushDownBlacklist`、`ExprPushDownBlackListReloadTimeStamp` | `infer_pushdown.rs` |
| tipb Go 模块的 `ScalarFuncSig` 常量与 `ScalarFuncSig.String()` | `tipb_sig.rs`、生成的 `tipb_sig_gen.rs`（§3.1） |

第 2 部分（16 个）：`PbTypeToFieldType`、`getSignatureByPB`、`newDistSQLFunctionBySig`、`PBToExprs`、`PBToExpr`、`convertTime/Int/Uint/String/Float/Decimal/Duration/JSON/VectorFloat32/Enum`、`decodeValueList`。

## 2. 依赖、位置与 TiFlash

- crate `tidb-expr-pushdown`，N0 rank 12（N0 §3：下推表按签名引用全部函数族，不能放在 rank 8 的表达式框架里）。依赖 `tidb-expression`（`Expr`、`BuiltinFunc.pb_code`、`metadata`）、`tidb-kv`（`kv.Client`、请求类型）、`tidb-codec`、`tidb-sessionctx`（`EvalContext`、`SessionVars`）、`tidb-base`（`kv.StoreType`、`WarnAppender`、`plannererrors`）、Rust `tipb` crate（与 tablecodec、execdetails 相同的 git 依赖）、`arc-swap`（M-SNAP）。
- 下推判定只读签名的描述（名字、pb code、结果类型、collation、CAST 的 `inUnion`），不求值，所以**第 1 部分不需要边界桩**：未移植的函数族（D08.6 字符串、D08.7 时间、D08.9 杂项、D08.10 JSON）在 Go 中建出的签名同样能判定和转换（govec 用 Go 建树，Rust 用只有描述的签名重建，§5）。
- **TiFlash（例外 D1）**：`ExprToPB` 用 `kv.UnSpecified` 调用 `canFuncBePushed`，即 TiDB、TiKV、TiFlash 任一支持就转换，所以 TiFlash 的函数表也决定发往 TiKV 的请求。`scalarExprSupportedByFlash`、`canExprPushDown` 中 TiFlash 的类型与 decimal 检查、`ToPBFieldTypeWithCheck` 都是纯判定，全部按 L1 移植，不做桩。MPP 计划本身（使用 `kv.TiFlash` 的调用方）在 D1 之后排期。

## 3. 设计

### 3.1 tipb

- Rust `tipb` crate（`5f9928e`）比 TiDB v8.5.8 的 Go 模块（`v0.0.0-20260605083900-f9f651ef5fbc`）新，多了若干枚举值（`UUIDv4` 等、`FTSMatch*`），并把枚举名改写成 Rust 风格（`LTJson` → `LtJson`、`TiDBVectorFloat32` → `TiDbVectorFloat32`）。下推表、warning 文本（`signature: LTJson`）和黑名单（`lt.ltjson`）都依赖 Go 的名字，所以 `tools/expr-gen/gen_tipb_sigs.py` 从 Go 模块的 `proto/expression.proto` 生成 `tipb_sig_gen.rs`：631 个与 Go 同名的常量（代码写 `tipb::LTJson`，对应 Go 的 `tipb.ScalarFuncSig_LTJson`）和按值排序的 `ScalarFuncSig_name`；`scalar_func_sig_string` 是 `ScalarFuncSig.String()`（未知的值输出数字，与 `proto.EnumName` 相同）。Rust 枚举只用来构造 `tipb::Expr`；单元测试 `pb_codes` 核对 Go 的每个值都是 Rust 枚举的值。
- gogoproto 的 non-nullable 字段（`Expr.tp`、`Expr.sig`、`Expr.has_distinct`、`FieldType` 的全部标量字段与 `array`、`ByItem.desc`）Go 总是序列化，Rust 消息只序列化设置过的字段：Go 的结构体字面量写成 `pb_expr` / `to_pb_field_type`，把这些字段全部设上，序列化的字节与 Go 相同（govec 逐字节比较）。`val` 只在 NULL 常量时不设（Go 的 `nil`；空字符串常量是非 nil 的空切片，会序列化，向量中可见）。
- `metadata()`：只有 CAST 的签名有（`tipb.InUnionMetadata`，D08.8 的 `SigExtra::Cast`）；GROUPING 的 `GroupingFunctionMetadata` 随 D08.9 的 `BuiltinGroupingImplSig`。

### 3.2 相关列的值

Go 的 `conOrCorColToPBExpr` 对相关列求值，读 `CorrelatedColumn.Data`：规划时是零值 Datum（NULL），Apply 的内表执行器重建请求时是外表当前行的值。Rust 中相关列的值在 `EvalState` 中（N4 §3.4、D08.1 spec §3.2），所以转换函数多一个 `st: &EvalState` 参数（`ExpressionsToPBList(ctx, st, exprs, client)` 等）；`PushDownContext` 的构造函数用 `empty_eval_state()`（规划时的零值），`with_eval_state` 换成执行器的状态。同一个 UniqueID 的相关列共享一个值（Apply 从外表行一次写入全部）；govec 的配方遵守这一点。

### 3.3 其他

- `DefaultExprPushDownBlacklist`（`atomic.Value`）：`LazyLock<ArcSwap<HashMap<String, u32>>>`，Go 的 `init` 是它的初始化函数（M-SNAP）；`ExprPushDownBlackListReloadTimeStamp` 是 `AtomicI64` 静态量。由 `mysql.expr_pushdown_blacklist` 的加载（D06.5 / D12）写入。
- `PushDownContext` 持有 `EvalContextRef`、`Arc<dyn Client>`（与 `planctx.BuildPBContext` 相同）；`NewPushDownContextFromSessionVars` 读 `StmtCtx.WarnHandler` 当前的值与 `ExtraWarnHandler`。
- `storeTypeMask`：Go 的 `1 << storeType`（uint32）在移位不小于 32 时为 0，Rust 用 `checked_shl`（M-INT）。
- 日志：Go 用 `logutil.BgLogger().Error`，这里用 `log::error!`（`ExplainInfo` 换成 `StringWithCtx`，只影响日志文本）。

## 4. 偏离与豁免

1. **测试专用的 failpoint 不移植**：`PushDownTestSwitcher`（`canFuncBePushed`）、`PanicIfPbCodeUnspecified`（`scalarFuncToPBExpr`、`canScalarFuncPushDown`）。这三个函数标 L2。
2. **`ToPBFieldType` 的 `elems` 不是 UTF-8 时有损转换**（L2）：tipb 的 `elems` 是 `repeated string`，Rust 消息的元素是 `String`；ENUM/SET 由二进制字面量定义、元素不是合法 UTF-8 时，Go 序列化原字节，Rust 写入替换字符。只影响这类列推到 TiKV 的类型信息；D07.3 构造 DAG 请求时如需完全一致，再改为直接写字段 7 的字节。
3. **`FieldTypeFromPB` 中 `elems` 为空时是 `None`**：Go 的 nil 与空切片在一次 protobuf 往返后都是 nil，Rust 消息只有空列表这一种，统一当作 nil。govec 的 `ft`/`ftpb` 记录只给出 nil 或非空的 `Elems`（Go 中直接构造的非 nil 空切片在实际流程里不出现）。
4. **`canScalarFuncPushDown` 的 metadata 序列化**：Go 序列化一次只为了在失败时记日志并返回 false；`InUnionMetadata` 的序列化不会失败，Rust 不做。
5. 四个未导出的函数（`scalarExprSupportedByTiDB/TiKV/Flash`、`canEnumPushdownPreliminarily`）为了向量回放设为 `#[doc(hidden)] pub`。

看起来像上游问题、按 Go 照搬的行为（只记录，不提交上游）：

- `columnToPBExpr` 检查黑名单时，BIT 列按 `kv.TiKV`（黑名单中 `bit` 只对 TiFlash 生效的条目不起作用），ENUM 列按 `kv.UnSpecified`（`enum` 必须对三种存储都列入黑名单才起作用）；调用方并不知道请求发往哪种存储。
- `ExprToPB` 对相关列不检查列类型，而 `canExprPushDown` 对相关列另外调用 `columnToPBExpr(&x.Column, true)`：SET/GEOMETRY 类型的相关列能被 `ExprToPB` 转换，但判定为不能下推。

## 5. 验证

- govec `pushdown` 领域（tracker `govec/domains/pushdown.go`，种子 20261114），Rust 回放 `crates/expr-pushdown/tests/govec_pushdown.rs`；`GOVEC_PUSHDOWN_FILES` 回放额外种子（CI 的 `govec-extra-seeds` 产物，种子 1–8 本地全部通过）。7,259 条：
  - `expr` 5,497 条：Go 的全部内置函数（`GetBuiltinList` 加 `istrue_with_null`，除 GROUPING）各取随机的参数个数与叶子（33 种列类型、39 种常量、带值或不带值的相关列）；嵌套树 1,500；每种列类型到 20 种 CAST 目标；单独的列（ID 0 / -1 / 正数）、常量、相关列；CONV 的混合类型与二进制字面量参数；REGEXP/LIKE 的 binary 与非 binary 参数；ROUND/RAND/TRUNCATE/FLOOR/CEIL 的各个签名；JSON/DURATION 参数的比较与控制函数。每条随机选择 new collation、客户端（`mock.Client` / 不支持 DAG / 都不支持）、时区、黑名单、`InExplainStmt`、两个 warn handler 是否存在、`canEnumPush`。输出：`ExprToPB`、`ExpressionsToPBList`、`ProjectionExpressionsToPBList`、`GroupByItemToPB`、`SortByItemToPB` 的序列化字节或错误，四种存储的 `canFuncBePushed`，三个 `scalarExprSupportedBy*`，`canEnumPushdownPreliminarily`，四种存储的 `CanExprsPushDownWithExtraInfo` 与两个 handler 收到的 warning。涉及 295 个函数名、441 个 pb code；2,896 条能转换，2,368 条能推到 TiKV。
  - `split` 700（多个条件的 `PushDownExprs*`、`CanExprsPushDown`）、`ft` 352、`ftpb` 300、`enabled` 400、`ctx` 10。
- 反证（arm64，7,259 条中报错的记录数）：不设 `has_distinct` 2,904；非函数节点不设 `sig` 2,841；TiKV 表去掉 CAST 1,720；TiFlash 表不排除 `LTJson` 1（`lt` 的 JSON 签名只有这一个 pb code；其他 JSON 比较各自的 pb code 同样各有记录）；TiKV 推带小数位的 ROUND 7；CONV 不检查混合类型 8；TiKV 不检查 REGEXP 的 binary collation 63；`UnSpecified` 的掩码只含 TiKV 98；接受 ID 为 0 的列 73；EXPLAIN 时选 extra handler 957；`canEnumPushdownPreliminarily` 不含 DECIMAL 100；不把 collation 写入结果类型 692；`approx_count_distinct` 只对 TiKV 禁止 18；TiFlash 接受非法 decimal 79；TIMESTAMP 常量不转 UTC 9；黑名单不查 pb 名 69；decimal 常量按列类型的精度编码 33；TiFlash 接受 YEAR 到时间的 CAST 13。
- 单元测试 `tests/pb_code_unit.rs`：Go 的 631 个 `ScalarFuncSig` 值都在 Rust 枚举中；P1-1（D08.1，Lucky 评审 #102）：IS TRUE/FALSE 的 6 个 `...WithNull` pb code（`builtin_op.rs` 的 `+ 20`）与 tipb 一致，`istrue_with_null` 建出的 3 个签名的 pb code 与 tipb 一致。
