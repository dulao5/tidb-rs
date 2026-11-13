# D08.2 函数注册、类型推导、常量折叠与传播、collation 推导：spec

状态：第 1 部分——函数注册表、签名的 `baseBuiltinFunc` 构造、collation 推导。`newBaseBuiltinFuncWithTp`、`newBaseBuiltinCastFunc4String` 随 D08.8 第 2b-1 部分移植（D08.2 matched 79/151；TIME/DURATION/JSON/VECTOR 参数的包装为临时桩，随 D08.8 第 2b-2 部分）。第 2 部分随 D08.8 CAST 的第一批签名和 D08.1 2b；第 3 部分为 grouping sets。

收敛检查（第 2 部分收尾，`foldConstant` 的 null-reject 分支与 `EvaluateExprWithNull`）：D08.2 matched 108/151（`constant_fold.go` 7/7、`constant_propagation.go` 27/27、`builtin.go` 51/51；其余为第 2 部分的 `builtin_func_param.go` 8 个（随 REGEXP/ILIKE）、第 3 部分 `grouping_sets.go` 29 个、`simple_rewriter.go` 4 个（D09.2）、cascades 的 2 个）；D08.1 296/348；D08.8 266/266。govec `cast` 追加 `evnull` 2,000 条（§5）。

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
     - YEAR 列的 `RefineComparedConstant` 修改常量的值（`refineArgs` 把 `AdjustYear` 的结果写进共享的 `*Constant`，`builtin_compare.go:1802`；重建时已改写的整数再被调整一次，1 → 2001）。**用户可见，结果行不同**（v8.5.8 实测，Lucky，[upstream-pitfall #104](https://github.com/dulao5/tidb-rs/issues/104)）：`y2 < 1.0` 只返回 0000；加上 `y1 = y2` 后返回 0000、1990、2000（EXPLAIN 中原条件变成 `lt(y2, 2001)`）；`y1 = y2 AND y2 >= 0.5` 丢掉 (1990,1990)、(2000,2000)。tidb-rs 对前一对查询都只返回 0000（不随 `y1 = y2` 变化）；最后一条与 Go 相同（#99 的 `Ceil` 改写照原样移植）。**这是有意的偏离：不复制 Go 的原地修改**（上游 bug，给出错误的行；副本是我们的模型，D08.1 §4），记入 `docs/compatibility.md` "Deliberate deviations"。有了经过 `PropagateConstant` 的 SQL 路径后，用 SQL 测试固定 tidb-rs 的结果（`specs/D08.2-funcs/tasks.md` T8c）。外连接的 join / filter 条件之间同理。
     - `PropagateType`（ETReal）修改 DECIMAL 常量的 flen / decimal；`BuildCastFunction` 去掉共享类型的 NotNullFlag。只是类型元数据，值和签名相同。
   - govec：生成器在求值前给输入条件的每个节点做快照，求值后有节点被修改的记录标 `"known_deviation": "in-place"`，回放照常执行每条记录：结果相同即通过（45 条中 27 条相同）；不同、有标记且没有 panic 的计为偏离（上限为 cprop 记录数的 2 %，目前 18 条；Lucky 在 10 倍记录、3 个种子上约 1.7 %）；修改发生在传播过程中新建的常量上的（检测不到），回放容许**只有常量的 flen / decimal / flag 不同**（上限 0.1 %，主文件 0 条，种子 1–4 共 1 条，Lucky 10 倍记录 1–3 条）。上限按比例，换种子或加记录不必改测试。`GOVEC_CPROP_STRICT=1` 关闭这两项容忍，用于排查。
   - `tryToReplaceCond`：`NewFunctionInternal` 失败时保留原条件（Go 返回 nil 并追加进条件列表，之后在 `RemoveDupExprs` 中 panic）；`ColumnSubstitute` 的 CAST 分支构建失败时返回原表达式（Go 记日志后用没有签名的节点继续）；`ComposeCNF/DNFCondition` 返回 `Option`，失败时保留原条目。生成器不产生这些情况。
   - `newPropConstSolver` 不带上下文，`PropagateConstant` 设置（与 Go 相同）；外连接的求解器构造时给出。
   - `ExtractColumns` 把列复制进新的 `Arc`（Go 保留表达式中的 `*Column`），字段相同。
   - 依赖：D08.1 `SplitCNFItems` 等 6 个函数；D08.12 `ColumnSubstitute`、`ColumnSubstituteAll`、`ColumnSubstituteImpl`、`cowExprRef`、`SetExprColumnInOperand`、`checkCollationStrictness`、`ExtractColumns`/`extractColumns`、`RemoveDupExprs`、`IsMutableEffectsExpr`、`BuildNotNullExpr`、`ConstExprConsiderPlanCache`（`util.rs`）；D00.3 `disjointset.SimpleIntSet`（`tidb_base::disjointset`）；D08.5 IS NULL（`BuildNotNullExpr` 需要）。`CollationStrictnessGroup` / `CollationStrictness` 是 `collation.rs` 中的两个函数（Go 包级 map）。

8. **常量折叠的 null-reject 分支与 `EvaluateExprWithNull`**（第 2 部分收尾；`EvaluateExprWithNull`、`evaluateExprWithNull`、`evaluateExprWithNullInNullRejectCheck`、`isColumnInOperand` 属 D08.1 `expression.go`，随本部分移植）：
   - 原来的临时桩（不建 dummy 调用、保留原表达式）已删除：`foldConstant` 在 `IsInNullRejectCheck` 下用 `NewFunctionBase` 建把非常量参数换成 1 的 dummy 调用，结果为 NULL 或假时折叠成常量，与 Go 相同（L1，只有"没有扩展函数"一处 L2）。dummy 调用失败（函数类未移植）时保留原表达式，与 Go 构建失败的处理相同；被折叠的节点本身已经由同一个函数类建出，所以这种情况不会出现。
   - **M-FT 审查**（D09.7 pilot report 的规则，本部分移植的代码中的原地 `RetType`/`FieldType` 修改与共享）：
     | Go 位置 | Go 的做法 | tidb-rs | 可见影响 |
     |---|---|---|---|
     | `foldConstant` null-reject 分支 `NewFunctionBase(ctx, x.FuncName.L, x.GetType(ctx), constArgs...)` | 把节点自己的 `*FieldType` 传给 `newFunctionImpl`；后者只替换局部变量，不写入。会写入的 `ast.Cast` 等只有一个参数，进不了这个分支（一个参数时"不全是常量"就没有 NULL 参数） | 传副本 | 无 |
     | 同一分支的 `&Constant{Value: value, RetType: x.RetType}`（NULL 与假两处） | 常量与被折叠的节点共享 `*FieldType`；之后 `FoldConstant` 写回 charset/collation（同值，无变化）；被折叠的节点在这条路径上总是 `NewFunction` 刚建的，随即丢弃 | 复制 | 无 |
     | `evaluateExprWithNull*` 的 `NewFunction(ctx, x.FuncName.L, x.RetType.Clone(), args...)` | 复制 | 复制 | 无 |
     | `evaluateExprWithNull*` 把输入中的常量原样交给重建的函数 | 重建比较时 `PropagateType`（ETReal）原地修改输入中 DECIMAL 常量的 flen / decimal（与第 7 条同一处），输入条件被改；govec `evnull` 2,000 条中 13 条（生成器标 `known_deviation`） | 修改副本，输入不变 | 结果相同：这 13 条在回放中逐字节一致，所以 `evnull` 不用第 7 条的容忍，要求全部相同 |
   - 测试上下文（`exprstatic`）默认开启 plan cache、类型为 `DefaultNoCache`：`EvaluateExprWithNull` 遇到 deferred 常量时 `SetSkipPlanCache` 追加 warning "unknown cache type"，记录中可见（Go 与 tidb-rs 相同）。
   - 没有被覆盖的分支：`ConcatWS` 的排除（CONCAT_WS 属 D08.6，尚未移植，构建不出这种节点）。

## 5. 验证

- govec `funcs` 领域（tracker `govec/domains/funcs.go`），Rust 回放 `crates/expression/tests/govec_funcs.rs`：
  - `derive` 4,000 条：随机的函数名（`deriveCollation` 的所有分支和若干普通函数）、参数（各种字符集与 collation 的字符串列与常量、JSON、BIT、NULL 类型、数值，以及 concat/upper/coalesce 等小函数树）、显式 coercibility/repertoire 覆盖、连接字符集、new collation 开关；400 条专门覆盖"JSON 参数 + 显式非 binary collation"。
  - `newbase` 1,300 条：`newBaseBuiltinFunc`/`newBaseBuiltinFuncWithFieldType` 的结果类型、NOT NULL 标志、collation 信息，以及 collator 对探测字符串的 key。
  - `retft` 828、`verify` 400、`setflen` 300、`deferred` 618、`registry` 1、`baseerr` 1。
- govec `cast` 领域 `evnull`（tracker `govec/domains/cast_d082d.go`）2,000 条，回放 `tests/cast_cprop/mod.rs`：`EvaluateExprWithNull` 在普通上下文与 `WithNullRejectCheck` 上下文（约 2/3）下，条件取 `cprop` 的条件，另加 deferred 常量（一半带过期的值，折叠后应换成重新求出的值；最外层直接是 deferred 常量）、与 NULL 常量的算术与比较、`INTERVAL(n, col, NULL)`（dummy 调用结果为 0：假的分支）、`COALESCE(NULL, col)`、AND/OR 中的 NULL 字面量和不决定结果的常量（AND 1、OR 0）。记录逐字节一致（`GOVEC_CPROP_STRICT=1` 下也是）。反证（arm64 回放，81,164 条中报错的条数）：删掉 null-reject 分支（回到原来的桩）275；dummy 参数用 0 代替 1 145；不处理 NULL 结果 258、不处理假结果 17；AND 中来自 schema 的 NULL 换成 0 33、OR 中换成 1 28；`allArgsNullFromSet` 不置假 68、条件取反 88；列的 `nullFromSet` 返回假 60；忽略 null-reject 上下文 60；`hasNonConstantArg` 恒真 127；两条路径不折叠 deferred 常量 20 / 35；不调用 `SetSkipPlanCache` 433；去掉 `NullEQ` 的排除 13；`schema.Contains` 取反 952。
- Go 侧通过 `go:linkname` 调用未导出的函数；`baseBuiltinFunc` 用布局相同的镜像结构体接收（`init` 中核对大小为 152 字节）。
- 函数参数的树由 Go 的 `NewFunctionBase` 构建，以 dump 形式给出，Rust 用只有描述的签名重建（同 `expr` 领域）。
