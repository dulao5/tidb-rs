# D10.4 ranger：由条件构造范围（spec）

状态：整块（VS1-25）评审中（2026-11-15）。块：D10.4（ROADMAP P3），crate `tidb-planner`，模块 `ranger`。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（VS1-25）；[D00.1 spec](../D00.1-ctx/spec.md)（`Range` 类型、`RangerContext`、`Full*Range` 已在 D00.1）；[D10.6 spec](../D10.6-cardinality/spec.md)（`Selectivity` 是第一个使用者）。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/`。

## 1. 范围

Go 范围：`util/ranger` 的 `checker.go`、`detacher.go`、`points.go`、`ranger.go`，共 95 个函数（tracker `func_blocks.tsv` 中 D10.4 的全部函数）。`types.go`、`ranger/context` 和 `points.go` 的 `FullIntRange` / `FullRange` / `FullNotNullRange` 是 D00.1，已经移植。

本 PR 移植整块：converge **matched 95/95**，没有块内的桩。

| Go | Rust（`crates/planner/src/ranger`） | 内容 |
|---|---|---|
| `checker.go` | `checker.rs` | `conditionChecker`：一个条件能否用来建范围（access），是否还要留作过滤（reserve） |
| `points.go` | `points.rs` | 一个列上的一个条件 → 端点（`point`）；端点的交、并；`NullRange` |
| `ranger.go` | `ranger.rs` | 端点 → 范围（类型转换、区间合法性、内存上限）；`BuildTableRange` / `BuildColumnRange`；多列索引的范围拼接；`UnionRanges`；前缀索引截断；`RangesToString` |
| `detacher.go` | `detacher.rs` | 从 CNF / DNF 条件中分出索引的 access 条件并建范围（`DetachCondAndBuildRangeForIndex` 等）、`ExtractEqAndInCondition`、`MergeDNFItems4Col`、shard 索引（`tidb_shard()`）前缀 |

另外：

- `planner/util/fixcontrol/get.go` 的 `GetBool`、`GetBoolWithDefault`（D09.13，没有出边，按宪章第四条 5 提前移植到 `crates/planner/src/util/fixcontrol.rs`，计入 D09.13），以及 ranger 读的两个编号 `Fix44389`、`Fix54337`。
- `PlanCtx::ranger_ctx()`（glue）：Go 的 `GetRangerCtx()`，由会话变量和语句上下文组成 `RangerContext`。为此 `StatementContext.range_fallback_handler` 改为 `Arc`（与 `plan_cache_tracker` 一样，Go 中 `RangerContext` 持有指向语句上下文中这两个对象的指针）。
- `cardinality::selectivity` 的 4 个 `blocked-by:D10.4` 桩换成真正的调用。

### 1.1 边界

| 边界 | 阻塞块 | 说明 |
|---|---|---|
| `points2EqOrInCond` 重建 `in(...)` | D08.9（IN 内置函数） | 同一列上的多个 eq / in 条件合并后剩下多个点时，Go 用 `NewFunctionInternal(in, ...)` 重建条件。IN 的函数类未移植，返回 `blocked-by:D08.9`。只剩一个点（`eq`）或含 NULL（`isnull` / `or`）时不受影响 |
| `AddGcColumn4EqCond` / `AddGcColumn4InCond` 计算 `tidb_shard(x)` | D08.9 | 生产中建不出 `tidb_shard()` 表达式（D08.9），shard 索引在 tidb-rs 中还不存在；govec 回放中求值返回 `blocked-by:D08.9` |

这两处都是 `glue` 标注的边界，登记在 tracker `data/stubs.tsv`（只有第一处是函数级的桩，`new_function_internal_or_blocked`；第二处是表达式求值本身的错误）。

## 2. 承载方式

| Go | Rust | 说明 |
|---|---|---|
| `[]*point`，原地修改（`convertPointsInPlace`、`cutPrefixForPoints`、`excludeToIncludeForIntPoint`） | `Vec<Point>`，按值 | 逐个检查过：没有两个槽位指向同一个 point 又被原地修改的情况，按值与 Go 结果相同 |
| `builder.err` 字段 | 同样保留在 `Builder` 上 | Go 的函数返回端点、把错误留在 builder 上，调用方每次调用后检查；照搬 |
| `slices.SortFunc`（`buildFromIn`、`UnionRanges`） | `tidb_base::gostd::sort::sort`（Go pdqsort 的移植） | 比较结果相等的元素（如 `_ci` 排序规则下的 `'a'` 与 `'A'`）排序后的顺序、以及 `buildFromIn` 中"最后一次比较的错误"都与 Go 相同 |
| `Ranges = []*Range`，`UnionRanges` 把合并后的上界写回调用方的 `*Range`、复用输入切片 | `Vec<Range>` 按值，返回新的 `Vec` | 所有调用方都用返回值替换原来的范围，效果相同 |
| 批量分配范围的底层数组（`lowValBuf[i : i+1 : i+1]`，防止后续 `append` 覆盖相邻范围） | 每个范围自己的 `Vec` | 效果相同（Go 这样做正是为了让各范围互不影响） |
| `[]*valueInfo`，`valueInfo.value` 指向常量的 datum | `Vec<Option<ValueInfo>>`，值是 datum 的拷贝；Go 的 nil 切片是 `None` | Go 只替换槽位，从不原地改 `valueInfo` |
| `ExtractEqAndInCondition` 的 `accesses`（有 nil 空洞，最后截到第一个空洞） | 构造时用 `Vec<Option<ExprRef>>`，返回截断后的 `Vec<ExprRef>` | |
| `MergeDNFItems4Col` 按 map 遍历（顺序随机） | 按 `UniqueID` 升序（`BTreeMap`，M-MAPORDER） | govec 中 Go 的结果排序后比较 |
| 返回 `(T, error)` 且出错时 T 有意义（`RangesToString` 出错返回 `"false"`、`AddGcColumnCond` 返回原条件） | 只返回 `Err`（M-ERRVAL） | 调用方遇到错误都直接返回 |
| `RangesToString` 返回 Go string | `Vec<u8>`（M-BYTES） | 范围的值可能是排序键（任意字节） |

**共享（N13）**：ranger 不持有长期共享的元数据。输入的条件是 `ExprRef`（`Arc<Expr>`，不可变，M-EXPR）；新建的条件（`eq` / `in` / `or` / `and` / `isnull`）都是新节点。范围、端点都是按值的局部数据，结果交给调用方所有。`RangerContext` 中共享的只有语句级的 `plan_cache_tracker` / `range_fallback_handler`（`Arc`，内部用锁 / `Once`，与 Go 相同的语义：同一语句中范围退化的 warning 只报一次）。`Column` 的 `ret_type` 是共享的 `Arc<FieldType>`，ranger 只读；`newFieldType` / `convertStringFTToBinaryCollate` 先复制再改（Go 的 `ft.Clone()`）。

## 3. 与 Go 的差异（均已在代码中标注）

1. `points2EqOrInCond`、`ExtractEqAndInCondition` 及其调用链返回 `Result`：只为 D08.9 的边界（§1.1）。
2. `convertPointInPlace` 在 `InPreparedPlanBuilding` 时记录跳过 plan cache 的原因 `"<err> when converting %v"`：Go 的 `%v` 打印 `types.Datum` 结构体的字段（`Datum.String` 是指针接收者），这里用 `Datum.String()`。只影响 warning 文本（`skip prepared plan-cache: ...`），不影响计划。
3. `RangeSingleColToString` 打印值用的 `driver.ValueExpr.Restore`（D02.2）：Rust 的 parser 的 `ValueExpr` 不是 `types.Datum`，这里按 Go 对零值 `Type`、`DefaultRestoreFlags` 的行为写了一个 glue（`restore_value`）。
4. `getPotentialEqOrInColOffset` 中 Go 的 `intest.AssertFunc`（测试构建的断言）不移植。
5. `UnaryNot` 的参数不是 `ScalarFunction`、`checkLikeFunc` 的转义参数不是 `Constant` 等 Go 用单值类型断言（失败即 panic）的地方，Rust 同样 panic（`checker` 保证走不到）。

## 4. 评审要点逐项对照（Lucky，VS1-25）

| 要点 | Go 位置 | 移植与测试 |
|---|---|---|
| 新排序规则下字符串范围用排序键 | `convertPointsToSortKeyInPlace` / `convertPointToSortKeyInPlace`（points.go:105-145）；`convertToSortKey` 时交并用 binary 比较器（points.go:974-986）；`convertStringFTToBinaryCollate` 后建范围（ranger.go:466、611） | L1。govec 宽表覆盖 `utf8mb4_bin`、`_general_ci`、`_unicode_ci`、`_0900_ai_ci`、`_0900_bin`、`gbk_bin`、`gbk_chinese_ci`、`ascii`、`latin1`、`binary`，以及 TestIndexRange 的 `\x00A` 形式的排序键；LIKE 的前缀范围：上界是排序键加一（`KeyWithoutTrimRightSpace`），PAD SPACE 排序规则下起点不排除、条件保留 |
| 前缀索引用 `CutDatumByPrefixLen` 截断，截断后的端点变为闭区间 | `cutPrefixForPoints`（ranger.go:723-743）、`CutDatumByPrefixLen`（:747-771）、`ReachPrefixLen`（:774-785）；`NOT IN` 在 `buildFromNot` 中先求补再截断（points.go:877-920） | L1。`binary` / `ascii` 按字节截，其余按字符（`utf8.RuneCount`，非法字节各算一个字符，与 Go 相同）；截断过的点、以及起点长度恰好等于前缀长度时 `excl = false`。govec：TestPrefixIndexRange、TestPrefixIndexRangeScan、宽表的 `s(3)`、`sg(2)`、`bn(2)`、`vb(3)`、`tx(3)`、`bl(2)`、`ac(2)`、`l1(2)` 前缀索引，`cut` op 直接对照两个函数 |
| 类型转换的边界：int 列对 `1.5`、越界常量、unsigned 列对负数——越界后的取整和开闭端点 | `handleUnsignedCol`（points.go:444-474）、`handleBoundCol`（:478-516）、`convertPointInPlace`（ranger.go:176-251：忽略 `ErrOverflow` 等错误、按转换前后的比较结果调整 `excl`）、`validInterval`（:43-62，空区间丢弃）、`excludeToIncludeForIntPoint`（detacher.go:582-620） | L1，逐分支。govec 常量池：`1.5`、`-1.5`、`0.5`、`1.4`、`1.6`、`127/128/-128/255/256`、`2147483648`、`9223372036854775807/8`、`-9223372036854775809`、`18446744073709551615/6`、`1e20`、`99999999999999999999.5`、`'1'`、`'abc'`、年份 `69/70/2155/2156`、`3.5e38` 对 float 列等，作用于 signed / unsigned 的 tinyint、int、bigint、float、double、decimal（含 unsigned）、year、bit；以及 TestIndexRangeForUnsignedAndOverflow、TestIndexRangeForYear、TestIssue50051 |
| 合并与去重：端点排序、重叠合并、IN 列表去重、空范围的表示、`MinNotNull` / `MaxValue` | `buildFromIn` 的排序与去重（points.go:620-716）、`merge` / `mergeSorted`（:1000-1054）、`rangePointCmp`（:66-102）、`UnionRanges`（ranger.go:653-701）、`allSinglePoints`（detacher.go:624-651） | L1。排序用 Go pdqsort 的移植（§2）；空范围是空的点 / 范围列表（Go 的 nil，Rust 的空 `Vec`，结果里两者都输出 `[]`）；`MinNotNull` / `MaxValue` 在表范围中换成整数的最小 / 最大值（`convertPointsInPlace`）。govec：IN 列表随机加入重复值、NULL、大小写 / 尾随空格不同的字符串；`union`、`append` op 直接对照 |
| `UnionRanges` 的结果是返回值 | Go 的 `UnionRanges` 把合并后的上界写回调用方的 `*Range`、并复用输入切片（ranger.go:653-701），调用方看到输入"被改了"；Rust 按值取入、返回新的 `Vec`（§2） | **评审清单（以后的调用方）**：plan cache 重建范围（`rebuildRange`）、index merge 等新调用方必须使用 `union_ranges` 的返回值，不能依赖输入被原地修改（Rust 中输入已被移走，编译器会拒绝复用；若改成先 `clone` 再传入，原值不会被更新） |
| 解除后 `selectivity` 的 410 条必须变为 0 | — | `cardinality` 回放：410 条全部与 Go 一致，pin 从 `[("D08.9", 32), ("D10.4", 410)]` 改为 `[("D08.9", 32)]`。余下 32 条是条件中含 `in(...)` 的记录：回放用 `new_function_internal` 构造输入条件，IN 的函数类是 D08.9，未移植，输入本身建不出来（与 ranger 无关） |

## 5. 可能的上游问题与发现（记录，不提 issue）

1. **（D00.1 的修正，不是上游问题）Go 中零大小类型的排序器彼此相等。** Go 的各排序器都是 `&T{}`，零大小类型的堆分配都指向 `runtime.zerobase`，所以接口比较中同类型的排序器相等，例如 `GetCollator("utf8mb4_0900_bin") == GetBinaryCollator()`（两者都是 `derivedBinCollator`）。Rust 的零大小 static 与 `Box` 地址不同，D00.1 的 `same_collator` 原来按地址比较，与 Go 不同（影响 `Range.Subset` / `IntersectRange` 的 `checkCollators`）。已改为：类型相同、且该类型**在 Go 中**大小为 0 时相等，否则按地址比较。由 govec 中 `utf8mb4_0900_bin` 列的记录发现。
   - **这固定的是 gc 的实现行为，不是 Go 语言规范**：规范说指向不同零大小变量的指针"可能相等也可能不相等"；gc 让它们相等，`rangerbuild` 向量证明 v8.5.8 就是这样。换编译器或 gc 改变这一点时，这里要跟着改（上游同步时检查）。
   - Go 的 `gbkBinCollator` 含一个 `*encoding.Encoder`，不是零大小（Rust 的 `GbkBinCollator` 是），按地址比较。剩下的差别：Go 中按名字取（`GetCollator`）和按 ID 取（`GetCollatorByID`）的两个 `gbk_bin` 实例不相等，Rust 中两个零大小 `Box` 地址相同而相等；ranger 只按名字取排序器，走不到。
   - 单元测试 `ranger::types::same_collator_tests`：同类型零大小相等、不同类型零大小不等、`gbk_bin` 按地址比较。
2. `MergeDNFItems4Col` 按 map 遍历，合并后的条件顺序每次运行不同，`Selectivity` 中各项相乘的顺序随之变化，浮点结果的末位可能不同（D10.6 spec §2 已记录）。
3. `convertPointInPlace` 的 plan cache 跳过原因用 `%v` 打印 `types.Datum` 值，输出的是结构体字段（含指针地址），不是可读的值（§3 第 2 条）。

## 6. 测试

**govec 领域 `rangerbuild`**（tracker `govec/domains/rangerbuild.go`、`rangerbuild_gen.go`、`rangerbuild_cases.go`；回放 `crates/planner/tests/govec_rangerbuild.rs`），5,721 条记录。

输入的条件与 `ranger_test.go` 一样来自 SQL：用 `CREATE TABLE` 建 `TableInfo`（`ddl.BuildTableInfoFromAST`）、组成 InfoSchema，对 `select * from t where <cond>` 做 preprocess 和逻辑计划构建（`core.MockContext()`），取 Selection 的条件（可选 `PushDownNot`）。记录的输出带上 Go 构建出来的条件和索引列（表达式树：函数名、签名类型、结果类型、字符集与排序规则、参数），回放用只有描述符的签名重建（ranger 只读函数名、类型、排序规则，只对常量求值），运行移植的 ranger，逐项与 Go 比较。

| op | 条数 | 内容 |
|---|---|---|
| `index` | 2,839 | 每条比较 4 个结果：`DetachCondAndBuildRangeForIndex`、`DetachSimpleCondAndBuildRangeForIndex`、`DetachCondAndBuildRangeForPartition`、`ExtractEqAndInCondition`（范围的每个 datum 的内部表示、开闭、排序器；access / 过滤条件；eq / in 计数；DNF 标志与 `MinAccessCondsForDNFCond`；列常量值；范围退化的 warning）。选项随机：`RegardNULLAsPoint`、`OptPrefixIndexSingleScan`、fix control 44389 / 54337、`tidb_opt_range_max_size` |
| `column` | 1,319 | `ExtractAccessConditionsForColumn` + `BuildColumnRange`（含前缀长度），`DetachCondsForColumn` + `BuildTableRange` |
| `merge_dnf` | 200 | `MergeDNFItems4Col` |
| `shard` | 62 | `IsValidShardIndex`、`ExtractColumnsFromExpr`、`AddExpr4EqAndInCondition`（TestTableShardIndex 的表与条件、TestShardIndexFuncSuites 的场景） |
| `union` / `append` / `to_string` / `cut` / `null_range` | 400 / 200 / 400 / 300 / 1 | 直接对照 `UnionRanges`、`AppendRanges2PointRanges`、`RangesToString`（`RangeSingleColToString`）、`CutDatumByPrefixLen` / `ReachPrefixLen`、`NullRange` |

来源：`ranger_test.go` 的全部用例表原样抄入（TestTableRange、TestIndexRangeForUnsignedAndOverflow、TestColumnRange、TestIndexRangeForYear、TestPrefixIndexRangeScan、TestIndexRange、TestPrefixIndexRange、TestMinAccessCondsForDNFCond、TestTableShardIndex），生成时检查 Go 的结果等于测试的期望值（证明生成器跑的就是测试跑的东西）；其余测试（TestRangeFallback*、TestBinCollationRangeForIndex、TestIssue40997、TestIssue50051、TestShardIndexFuncSuites）的查询；以及随机生成：一张 35 列、49 个索引的宽表（§4 的类型和排序规则），每个条件 1–4 个合取项，含单列 DNF、多列 DNF of CNF、IN / NOT IN、LIKE / NOT LIKE（含转义）、IS [NOT] TRUE / FALSE / NULL、BETWEEN、NOT。

结果：全部 18,198 个结果与 Go 一致；121 个停在 D08.9 边界（`EXPECTED_BLOCKED` 钉住：25 条 `index` 记录各 4 个结果因重建 `in(...)`，21 个 `shard` 结果因计算 `tidb_shard()`）。两个目标的记录相同（ranger 没有浮点乘加）。

另外：`cardinality` 回放的 `selectivity` 1,250 条中 1,218 条与 Go 一致（原来 410 条停在 D10.4），32 条停在 D08.9（§4 最后一行）；`point.String` 没有调用者（调试用），由单元测试覆盖（govec 豁免表登记）。
