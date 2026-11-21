# D09.2 表达式改写（spec）

状态：第 1 部分（VS1-22，子查询之外的改写）评审中（2026-11-17）。块：D09.2（ROADMAP P3），crate `tidb-planner`。相关：[VS1 plan](../VS1-vertical-slice/plan.md) §3（D09.2 一行）、§3.2（子查询桩）、§5（VS1-22）；[tasks](tasks.md)；[N1](../exceptions/N1-plan-representation/spec.md)（计划与表达式的表示）。

## 1. 这一块做什么

SQL 文本经过 parser 变成语法树（AST）。优化器不直接处理语法树，而是处理"规划器表达式"（`Expression`：列、常量、标量函数）。表达式改写就是把语法树中的一个表达式（`WHERE` 条件、`SELECT` 列表中的一项、`ORDER BY` 的一项等）翻译成规划器表达式：

- 列名 `c` → 当前计划输出的第几列（`Column`），找不到或有歧义时报与 Go 相同的错误；
- 字面量 `1`、`'abc'` → 常量，类型按 Go 的规则推导；
- 预处理语句的 `?` → 常量，值取 `EXECUTE` 绑定的参数，并记住它是第几个参数（计划缓存要用）；
- `a = 1`、`a + 1`、`NOT a`、`a IN (1,2)`、`a BETWEEN 1 AND 5`、`a LIKE 'x%'`、`a IS NULL`、函数调用、`CAST`、`CASE` → 标量函数，类型推导和常量折叠都与 Go 相同。

它的使用方是 SELECT 的计划构建（VS1-21）：`buildSelect` 对 WHERE、投影、ORDER BY 中的每个表达式调用这里的 `rewrite`。

Go 代码：`pkg/planner/core/expression_rewriter.go`（2,876 行）。另外两个小函数归本块（P1 移交，见 [tasks](tasks.md)）：`pkg/expression/simple_rewriter.go` 的 `FindFieldName` / `FindFieldNameIdxByColName`，`pkg/expression/util.go` 的 `ParamMarkerExpression`。

## 2. 第 1 部分的范围（VS1-22）

函数清单是 `specs/VS1-vertical-slice/functions.tsv` 中 VS1-22 的 53 个函数（Go 中带子查询的分支不在清单内，见 §4）。

优先顺序按 sysbench 只读语句的需要：
1. `SELECT c FROM sbtest1 WHERE id=?`：列、参数标记、`=`；
2. 范围查询 `WHERE id BETWEEN ? AND ?`；
3. `SUM(k)`（聚合函数在改写时只是"查表取聚合结果列"，见 §3 第 5 条）；
4. `ORDER BY c`、`DISTINCT c`（ORDER BY 的项也走改写）；
5. 其余：一元 / 二元运算、`IN` 列表、`LIKE`、`IS NULL` / `IS TRUE`、函数调用（含 `IFNULL` / `NULLIF` 的特殊处理和计划缓存下的延迟函数）、`CAST`、`CASE`、行表达式 `(a, b)`、`COLLATE`。

## 3. 承载方式

1. **遍历。** Go 用访问者模式：`Accept` 先调 `Enter`，再按固定顺序访问子节点，最后调 `Leave`；`Leave` 把结果压进一个栈（`ctxStack`），父节点从栈顶取子节点的结果。Rust 的 AST 没有访问者接口，改写器写成一个递归函数，对每种节点按 Go `Accept` 中子节点的顺序递归，`Enter` / `Leave` 的逻辑原样保留（含 `asScalar`、常量折叠计数器 `disableFoldCounter` / `tryFoldCounter` 的进出）。Go 中出错时设置 `er.err`、之后的 `Leave` 立即返回 false、整棵树停止遍历；Rust 用 `Result` 和 `?` 达到同样的效果（第一个错误即返回，与 Go 返回的错误相同）。
2. **计划上下文。** Go 的 `rewrite(exprNode, p, aggMapper, asScalar)` 从计划 `p` 读输出 schema 和输出列名。Rust 的逻辑计划还没有输出列名（VS1-23 加），所以 `rewrite` 直接接收 schema 和列名（加上可选的计划 ID，供 `findFieldNameFromNaturalUsingJoin` 向下找 join）。Go 在改写结束后把 `p` 的输出列名截回原长度：只有子查询会给 `p` 加列（§4），子查询之外这一步什么也不改，不移植。
3. **改写器池。** Go 的 `PlanBuilder` 缓存改写器对象（`rewriterPool`、`rewriterCounter`）以减少分配，这是 `M-POOL` 的情形：Rust 每次新建，`getExpressionRewriter` 只保留"重置各字段"的语义。
4. **节点身份。** Go 的 `aggMapper`、`windowMapper`、`colMapper` 以 AST 节点指针为键。Rust 用节点地址（`usize`）为键，调用方在同一棵 AST 上建表和查表，与 Go 的指针身份等价。
5. **聚合函数。** 改写时聚合已经由 `buildAggregation`（D09.4）建好，`aggMapper` 告诉改写器"这个 `SUM(k)` 是聚合算子输出的第几列"，改写器取那一列；不在表中时报 `ErrInvalidGroupFuncUse`。`COUNT` 离开时收集表级权限（`collectPrivsForCount`）。窗口函数同理（`windowMapper`）。
6. **参数标记的值。** Go 在 `EXECUTE` 时把参数值写进 AST 的 `ParamMarkerExpr.Datum`，`ParamMarkerExpression` 从那里取值。Rust 的 AST 不带值（VS1-15 的决定：参数值只放在 `SessionVars.PlanCacheParams`），所以 Rust 的 `ParamMarkerExpression` 按参数序号从 `PlanCacheParams` 取值。两处的值是 Go 在同一处（`SetParameterValuesIntoSCtx`）写入的同一个值，等价。`PREPARE` 阶段（还没有值）Go 的 Datum 是空值（NULL），Rust 读不到参数时也给 NULL。
7. **字面量的类型。** Go 的字面量节点带 parser 推导的 `FieldType`（`DefaultTypeForValue`）。Rust 的字面量节点只带值、字符集和排序规则，类型在改写时用同一个 `DefaultTypeForValue` 算出，结果相同。
8. **函数调用的返回类型。** Go 传 `&v.Type`（AST 节点上的零值 `FieldType`）给 `NewFunction`，Rust 传 `FieldType` 的零值，`NewFunction` 内部按函数推导真实类型，与 Go 相同。
9. **Go 的别名写入（`M-FT`）。** `SetCollationExpr` 对常量和标量函数直接改它们的类型（`arg.GetType().SetCollate`）。Rust 的常量和函数是 `Arc` 共享的，改写器刚建出的表达式只有栈上一个持有者，用 `Arc::make_mut` 改本处副本，结果与 Go 相同（没有别的持有者能看到差别）。

## 4. 边界桩

| 位置 | 情况 | 处理 | 原因 |
|---|---|---|---|
| `Enter` | 标量子查询、`EXISTS`、`IN (SELECT ...)`、`= ANY / ALL (SELECT ...)` | `blocked-by:D09.6` | 子查询要建子计划并与外层计划组成 Apply / semi join，属于"子查询与去关联"一块；VS1 plan §3.2 的约定。任务说明写的是 D09.3（join 构建），子查询改写本身在 D09.6，桩写 D09.6 |
| `toColumn` | 外层查询的列（相关列） | 不会出现 | 外层 schema 只在子查询里非空，子查询是桩 |
| `findFieldNameFromNaturalUsingJoin` | `NATURAL JOIN` / `USING` 的合并列 | 走到 join 节点时 `blocked-by:D09.3` | join 构建（D09.3）还没有移植；没有 join 时与 Go 一样返回"没找到" |
| `Leave` | `@var`、`@var := expr`（用户变量） | `blocked-by:D08.9` | 要 `getvar` / `setvar` 函数（杂项函数，D08.9）和会话的用户变量类型表 |
| `Leave` | `@@var`（系统变量） | `blocked-by:D09.2`（第 2 部分） | 要按名字查系统变量定义并取会话值；改写器目前只拿到表达式上下文，拿不到会话变量，第 2 部分把会话变量传进来 |
| `toColumn` | 没有计划时按"源表"解析列（`buildSimpleExpr` 的路径） | `blocked-by:D09.2`（第 2 部分） | `buildSimpleExpr` / `ParseSimpleExpr`（P1 移交）在第 2 部分移植 |
| `Leave` | `DEFAULT(col)` | `blocked-by:D09.15` | 读列默认值的路径主要服务 INSERT / UPDATE（写路径的计划构建，D09.15） |
| `Enter` | `VALUES(col)` | `blocked-by:D09.15` | 只在 `INSERT ... ON DUPLICATE KEY UPDATE` 中有意义（P1 移交的 `NewValuesFunc`，D08.9） |
| `funcCallToExpressionWithPlanCtx` | `GROUPING()` | `blocked-by:D09.4` | 要 `ROLLUP` 的 Expand 算子（聚合构建，D09.4） |
| `Leave` | `ORDER BY ?`（`PositionExpr` 的参数形式） | `blocked-by:D02.2` | Rust AST 的位置节点只有数字，没有参数标记（parser，D02.2）；数字形式已移植 |
| `rewriteInsertOnDuplicateUpdate` | INSERT 的赋值 | 不在清单 | 写路径（D09.15） |
| `NewFunction`（表达式包） | 函数族还没有移植的内置函数（`IN`、`CASE`、`IF` / `IFNULL` / `NULLIF`、`LIKE`、字符串函数、时间函数等） | `blocked-by:<该函数族的块>`（D08.6 / D08.7 / D08.9 / D08.10） | 改写器照常建函数调用，函数注册表发现函数族没有移植时返回带块号的桩错误（原来是普通错误）。块号取自 tracker 中该函数族 `getFunction` 所在的块（`builtin_registry_blocks.rs`） |
| `evalAstExpr`、`buildSimpleExpr` | 没有计划的改写（P1 移交） | `blocked-by:D09.2`（第 2 部分） | 第 2 部分移植 `BuildOption` 和 `ParseSimpleExpr` 时一起做 |

## 5. 与 Go 的其他差异

1. **`_utf8mb4'x'` 这类字符集引导符。** Go 的 parser 给带引导符的字面量（`_charset'x'`、`N'x'`）加 `UnderScoreCharsetFlag`，改写时据此决定类型的标志和排序规则（utf8mb4 时换成会话的 `default_collation_for_utf8mb4`）。Rust AST 的字面量原来没有这个标记，本项在 `ValueExpr` 上加了 `underscore_charset`，由 parser 的三条引导符产生式设置（D02.2 的 AST，改动很小），改写器照 Go 的产生式补上字符集、排序规则和标志。govec 中有 `N'abc'`、`_binary'abc'`、`_latin1'abc'`，结果与 Go 相同。
2. **内置函数使用统计。** `BuiltinFunctionUsageInc` 是遥测计数（D14 系列），不移植，不影响结果。
3. **同一列的多次引用（Go 的别名，不模拟）。** Go 每次引用一列都把 schema 中的同一个 `*Column` 压栈，某次引用惰性算出的 coercibility 会缓存在这个共享对象上，之后的引用都看到它。Rust 每次引用复制一份列，coercibility 在各自的副本上算。只有一种情况结果不同：ENUM 列先在字符串比较中被引用（算出 implicit），再被 `WrapWithCastAsInt` 复制并加上"按整数"标志——Go 的副本带着缓存的 implicit，Rust 按整数类型算出 numeric。coercibility 只在推导字符串排序规则时有用，整数上下文不会用到，所以不影响结果；govec 中这样的记录有 1 条，测试中逐条登记（`KNOWN_ALIASING_DIFFS`）。
4. **`collectPrivsForCount`。** Go 遍历 map 生成权限检查项，顺序随机；Rust 按（库，表）排序（`M-MAPORDER`，顺序不影响权限检查）。视图名需要 InfoSchema（视图还没有移植），按表名处理。

## 6. 测试

- **govec 领域 `rewrite`**（tracker `govec/domains/rewrite.go`，2,392 条，`crates/planner/tests/govec_rewrite.rs`）：输入是一张表的定义、一个表达式的 SQL 文本、`?` 的参数值和会话开关（new collation、计划缓存、类型标志、SQL mode）；Go 用 `rewriteAstExprWithPlanCtx`（以表的全部列为 schema 的假计划）改写，记录整棵结果树（每个节点的类型、字符集、排序规则、coercibility、repertoire，常量的参数序号和延迟函数）、字符串形式、权限检查项（visit info）和 warning，或错误。Rust 解析同一条 SQL，用同样的 schema 调 `rewrite_ast_expr_with_plan_ctx` 比较。用例：sysbench 只读语句的条件（`id = ?`、`id BETWEEN ? AND ?` 等，参数值覆盖整数、无符号、浮点、字符串、十进制、NULL，计划缓存开 / 关）；§2 的每种节点；各种字面量；错误（未知列、行表达式列数不符、CAST 精度过大、参数个数）；桩；1,800 条随机组合。
  - 结果：1,467 条与 Go 相同，1 条是 §5 第 3 条的已知差异，2 条 Go 解析失败跳过；922 条停在边界桩，按块固定在测试中：D08.6 407、D08.7 20、D08.9 473（函数族未移植，见 §4 最后几行）、D09.6 12、D09.15 4、D09.2 4、D09.4 2。
  - tracker 中 D09.2 块 109 个函数：41 个由本领域覆盖，68 个豁免（子查询那一半和标量子查询表达式，以及桩）。
- **converge**：`scripts/converge.py --block D09.2 --part specs/VS1-vertical-slice/functions.tsv --item VS1-22 crates/planner/src/core/expression_rewriter.rs`：44/53 移植 + 9 个桩，OK。

## 7. 可能的上游问题

暂无。
