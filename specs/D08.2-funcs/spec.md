# D08.2 函数注册、类型推导、常量折叠与传播、collation 推导：spec

状态：第 1 部分——函数注册表、签名的 `baseBuiltinFunc` 构造、collation 推导。`newBaseBuiltinFuncWithTp`、`newBaseBuiltinCastFunc4String` 随 D08.8 第 2b-1 部分移植（D08.2 matched 79/151；TIME/DURATION/JSON/VECTOR 参数的包装为临时桩，随 D08.8 第 2b-2 部分）。第 2 部分随 D08.8 CAST 的第一批签名和 D08.1 2b；第 3 部分为 grouping sets。

收敛检查（第 1 部分）：D08.2 matched 68/151，missing 8（第 2 部分的 6 个 + cascades 的 2 个），另有 5 个 Go 文件（75 个函数）属于第 2、3 部分。govec `funcs` 领域 7,448 条，覆盖 57 个函数，豁免 94 个（`funcs_pending.go`）。

## 1. 范围

tracker 中 D08.2 共 151 个函数，分布在 9 个 Go 文件中：

| 部分 | Go 文件 | 函数数 | 内容 |
|---|---|--:|---|
| 1 | `builtin.go` | 51 中 45 | `funcs` 表与 `IsFunctionSupported`、`GetDisplayName`、`GetBuiltinList`、`VerifyArgsWrapper`；`baseFunctionClass.verifyArgs*`；`newBaseBuiltinFunc`、`newBaseBuiltinFuncWithFieldType`、`newReturnFieldTypeForBaseBuiltinFunc`、`adjustNullFlagForReturnType`；`setDecimalAndFlenFor*`；`builtinFuncCache`；`baseBuiltinFunc` 的其余方法（多数在 D08.1 2a 已移植） |
| 1 | `builtin_registry.go` | 1 | `RegisteredBuiltinFunctionNames` |
| 1 | `collation.go` | 23 中 21 | `deriveCollation`、`CheckAndDeriveCollationFromExprs`、`inferCollation`、`safeConvert`、`fixStringTypeForMaxLength`、`illegalMixCollationErr` 等；`collationInfo` 的 9 个方法（D08.1 第 1 部分） |
| 1 | `function_traits.go` | 1 | `IsDeferredFunctions` 与各个函数集合 |
| 2 | `builtin.go` | 6 | `newBaseBuiltinFuncWithTp`、`newBaseBuiltinFuncWithFieldTypes`（给参数包 CAST）、`baseBuiltinCastFunc` 的 4 个函数 |
| 2 | `constant_fold.go`、`constant_propagation.go` | 7 + 27 | 常量折叠与传播：要求值真实的签名 |
| 2 | `builtin_func_param.go` | 8 | REGEXP/ILIKE 向量版的参数辅助函数：随这两个函数族 |
| 3 | `grouping_sets.go` | 29 | Expand/ROLLUP 的 grouping sets：与 D08.3（`ToPB`）、GROUPING 函数（D08.9）一起 |
| — | `simple_rewriter.go` | 4 | `ParseSimpleExpr` 需要 planner 的表达式改写（D09.2，P3） |
| 豁免 | `collation.go` | 2 | `collationInfo.Hash64`/`Equals`：cascades 专用（EXC.B） |

## 2. 依赖

- D08.1（`Expr`、`BuiltinFunc`、`SigDesc`）；`tidb-sessionctx::exprctx::{BuildContext, EvalContext}`（D00.1）；`tidb-types::collate`（D01.9）、`FieldType` 与构造器（D01.1）；`tidb-parser::gocharset`。
- 函数名常量（`pkg/parser/ast/functions.go`）与 `funcs` 的条目由 `tools/expr-gen/gen_registry.py` 从 Go 源码生成（函数名常量在 tidb-parser 的 `ast/funcs.rs`，Go 中它们也属于 `pkg/parser/ast`，parser 的 restore 已改用这些常量；`funcs` 条目在 tidb-expression 的 `builtin_registry_gen.rs`），不手写。生成后用 rustfmt 格式化这两个文件。

## 3. 设计

### 3.1 `baseBuiltinFunc` 的构造

Go 的 `newBaseBuiltinFunc*` 返回 `baseBuiltinFunc` 值，函数类再把它嵌入具体签名（`&builtinXxxSig{bf}`）并 `setPbCode`。Rust 的 `BuiltinFunc` 已经包含 `baseBuiltinFunc` 的字段和签名描述（D08.1 2a）：构造函数返回一个签名为占位 `baseBuiltinFuncSig` 的 `BuiltinFunc`，函数类调用 `with_sig(&SIG)` 换成具体签名（同时设置 `pb_code`）。

- `adjustNullFlagForReturnType` 在 Go 中按值接收 `baseBuiltinFunc`，但其中的 `tp` 是指针，实际只改结果类型：Rust 直接接收 `&mut FieldType`。
- `newBaseBuiltinFunc` 用 `[retType]` 作为 `argTps` 调用 `deriveCollation`：`Case` 等读 `argTps[1]` 的分支在 Go 中越界 panic，Rust 同样 panic（govec 有这类记录）。
- Go 中 `ctx == nil` 时返回错误；Rust 的引用不会为空（L2）。

### 3.2 函数注册表

`funcs` 是 308 个条目的静态表（`FuncClassEntry { name, class, base: BaseFunctionClass, display_name }`），按名字建索引。只有 `compareFunctionClass`、`isTrueOrFalseFunctionClass` 实现 `getDisplayName`（`opcode.Op.Format`，即运算符字面量），生成器把它写进条目。

每个函数类的 `getFunction`（选择签名、推导类型）属于其函数族所在的块（D08.4–D08.9），移植时加到条目上；`NewFunction*`（D08.1）随第一个函数族（CAST）。tidb-rs 没有函数扩展（`pkg/extension`），`GetBuiltinList` 不追加扩展函数。

### 3.3 collation 推导

逐行对应 Go：`deriveCollation` 按函数名选择参与推导的参数，`inferCollation` 逐个聚合（`agg(a, b, c) = agg(agg(a, b), c)`），`safeConvert` 检查常量能否无损转换到目标字符集，`fixStringTypeForMaxLength` 在参数含 JSON 时把 collation 换成对应的 binary collation（pingcap/tidb#52833），错误文字由 `illegalMixCollationErr` 按参数个数生成，函数名用 `GetDisplayName`。

### 3.4 `builtinFuncCache`

LIKE、ILIKE、REGEXP、JSON_SCHEMA_VALID 按 `EvalContext.CtxID()` 缓存编译结果。Go 用 `atomic.Pointer` 无锁读、在互斥锁下构建；Rust 读和构建都在 `Mutex` 下（L2，结果相同）。只保存最后一个上下文的结果；构建出错时不缓存。签名的 `Clone` 在 Go 中新建结构体，只复制 `baseBuiltinFunc`，所以克隆后的缓存为空（D08.1 spec §3.5）。

## 4. 偏离与豁免

1. **`newBaseBuiltinFunc*` 不检查 nil 上下文**（引用不为空）。
2. **`getBinCollation` 遇到其他字符集不写错误日志**（Go 注释说明不可达），返回值与 Go 相同。
3. **`GetBuiltinList` 没有扩展函数**：tidb-rs 不支持 `pkg/extension`。
4. **`builtinFuncCache` 读也加锁**（L2）。
5. **`baseBuiltinFunc.Clone`**：Go 中只 panic，从不被调用（每个签名都有自己的 `Clone`）；Rust 中签名的克隆就是 `BuiltinFunc` 的克隆。
6. **govec 豁免**（`funcs_pending.go`）：第 2、3 部分的函数；`collationInfo.Hash64`/`Equals`（EXC.B）；`builtinFuncCache` 的两个泛型方法（无法 linkname，由 Rust 单元测试 `builtin_unit.rs` 覆盖，LIKE/REGEXP 移植后由它们的记录覆盖）；`baseBuiltinFunc` 的 7 个方法由 `expr` 领域的 `sf` 记录覆盖（D08.1 2a）。

7. **常量传播**（`constant_propagation.rs`，第 2b 部分）：
   - **Go 原地修改被多个条件共享的节点，Rust 修改副本**（D08.1 spec §4、D08.4 spec §4 第 1 条的同一类问题）。传播时新条件和旧条件共享常量、CAST 等节点，Go 在构建新条件时会原地修改它们，所有持有者都看到修改：
     - YEAR 列的 `RefineComparedConstant` 修改常量的值。例：`y1 = y2 AND y2 >= 0.5`：输入条件构建为 `y2 >= 1`（#99：`Ceil` 改写后不再按 YEAR 调整）；传播出 `y1 >= K` 时同一个常量被改成 2001，原条件也变成 `y2 >= 2001`。Rust 中原条件仍是 `y2 >= 1`。在 CNF 中两种结果等价（`y1 = y2`），**按推断用户看不到差别，未在集群上实测**；外连接的 join / filter 条件之间是否可见未分析。
     - `PropagateType`（ETReal）修改 DECIMAL 常量的 flen / decimal；`BuildCastFunction` 去掉共享类型的 NotNullFlag。只是类型元数据，值和签名相同。
   - govec：生成器在求值前给输入条件的每个节点做快照，求值后有节点被修改的记录标 `"known_deviation": "in-place"`，回放跳过（上限 64，目前 45 条）；修改发生在传播过程中新建的常量上的（检测不到），回放容许**只有常量的 flen / decimal / flag 不同**（上限 32，主文件 0 条，种子 1–4 共 1 条）。`GOVEC_CPROP_STRICT=1` 关闭这两项容忍，用于排查。
   - `tryToReplaceCond`：`NewFunctionInternal` 失败时保留原条件（Go 返回 nil 并追加进条件列表，之后在 `RemoveDupExprs` 中 panic）；`ColumnSubstitute` 的 CAST 分支构建失败时返回原表达式（Go 记日志后用没有签名的节点继续）；`ComposeCNF/DNFCondition` 返回 `Option`，失败时保留原条目。生成器不产生这些情况。
   - `newPropConstSolver` 不带上下文，`PropagateConstant` 设置（与 Go 相同）；外连接的求解器构造时给出。
   - `ExtractColumns` 把列复制进新的 `Arc`（Go 保留表达式中的 `*Column`），字段相同。
   - 依赖：D08.1 `SplitCNFItems` 等 6 个函数；D08.12 `ColumnSubstitute`、`ColumnSubstituteAll`、`ColumnSubstituteImpl`、`cowExprRef`、`SetExprColumnInOperand`、`checkCollationStrictness`、`ExtractColumns`/`extractColumns`、`RemoveDupExprs`、`IsMutableEffectsExpr`、`BuildNotNullExpr`、`ConstExprConsiderPlanCache`（`util.rs`）；D00.3 `disjointset.SimpleIntSet`（`tidb_base::disjointset`）；D08.5 IS NULL（`BuildNotNullExpr` 需要）。`CollationStrictnessGroup` / `CollationStrictness` 是 `collation.rs` 中的两个函数（Go 包级 map）。

## 5. 验证

- govec `funcs` 领域（tracker `govec/domains/funcs.go`），Rust 回放 `crates/expression/tests/govec_funcs.rs`：
  - `derive` 4,000 条：随机的函数名（`deriveCollation` 的所有分支和若干普通函数）、参数（各种字符集与 collation 的字符串列与常量、JSON、BIT、NULL 类型、数值，以及 concat/upper/coalesce 等小函数树）、显式 coercibility/repertoire 覆盖、连接字符集、new collation 开关；400 条专门覆盖"JSON 参数 + 显式非 binary collation"。
  - `newbase` 1,300 条：`newBaseBuiltinFunc`/`newBaseBuiltinFuncWithFieldType` 的结果类型、NOT NULL 标志、collation 信息，以及 collator 对探测字符串的 key。
  - `retft` 828、`verify` 400、`setflen` 300、`deferred` 618、`registry` 1、`baseerr` 1。
- Go 侧通过 `go:linkname` 调用未导出的函数；`baseBuiltinFunc` 用布局相同的镜像结构体接收（`init` 中核对大小为 152 字节）。
- 函数参数的树由 Go 的 `NewFunctionBase` 构建，以 dump 形式给出，Rust 用只有描述的签名重建（同 `expr` 领域）。
