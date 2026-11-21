# 结果比较规格（全项目唯一）

来源：#261（负责人 2026-11-20 采纳 pgrust 经验报告第 ② 条）。

## 1. 适用范围与唯一实现

凡是"拿 tidb-rs（B）的回答和 Go TiDB（A）的回答比"的 SQL 级测试层，都只能经由本规格的唯一实现
`tools/ab/src/compare.rs` 判定是否一致：

| 测试层 | 入口 | 调用方式 |
|---|---|---|
| integrationtest A/B（含 weekly 的 integrationtest 部分） | `ab integrationtest`（`tools/ab/src/integration.rs`） | `classify` + `compare`（T4 时 `compare_status_only`） |
| sysbench 语句 / 工作负载文件（含 weekly 的 ab、tpch 部分） | `ab sysbench`、`ab file`（`tools/ab/src/main.rs`） | `classify` + `compare` |
| 新栈 strict 轮（weekly ab 部分对 `--new-stack=strict` 的 tidb-rs） | 同上，B 换成 strict 实例 | 同上 |

任何测试层都不得自己写"差不多就算一样"的判断。要放宽，只能在 `docs/testing/tolerances.toml`
加一条有编号的容差条目（§6），并在比较器中实现。govec 向量回放比较的是函数级输出，不属于本规格。

## 2. 语句分类

只看 SQL 文本（不依赖规划器），加上 A 返回的列名，把语句分为四类：

### 2.1 exact（逐行）

行按列表比较，顺序必须相同。包括：

- 首个词为 `EXPLAIN` / `DESC` / `DESCRIBE` / `SHOW` / `TRACE` / `ADMIN` / `HELP` / `PLAN` 的语句；
- 不是查询（`SELECT` / `WITH` / `TABLE` / `VALUES` / 以括号开头）的任何其它返回结果集的语句；
- 顶层 `ORDER BY` 中有某一项解析不到结果列（"更严格的读法"：宁可误报，不可漏报）。

### 2.2 unordered（多重集）

没有顶层 `ORDER BY` 的查询。行按多重集比较：重复行计数，`NULL` 与空串不同。子查询、窗口函数、
`GROUP_CONCAT(... ORDER BY ...)` 中的 `ORDER BY` 以及字符串、注释里的文字都不算顶层。
`ORDER BY NULL`（MySQL：不要求顺序）也是 unordered。

### 2.3 ordered K（按键分段）

顶层 `ORDER BY` 的每一项都能解析到结果列 K（可以是多列）。解析规则：

1. 序号：`ORDER BY 2` 即第 2 列；
2. 列名或别名（大小写不敏感），可带限定名（`t.a`、`` `t`.`a` ``，限定名须等于该列的表别名）；
3. 列名不在结果中时，取原始列名（`org_name`）唯一匹配的列（`SELECT a AS k ... ORDER BY a`）；
4. 表达式：与某列的列名文本相同（Go 用选择项原文命名未加别名的表达式列）；
5. 匹配到多列（有歧义）或匹配不到 → 整条语句按 exact。

比较：按 A 的行把键值相同的最大连续区段切开；B 在同一位置区段内的行必须与 A 的该区段构成同一多重集，
区段之间的顺序必须一致。键值相等按文本逐字节判定。`ASC` / `DESC` 不影响分段。

注意：在不区分大小写的排序规则下，`'a'` 与 `'A'` 是相等的键，两边在这类并列行上的顺序可以不同，
而按文本切段会把它们当成不同的键，报不一致。这是已知的误报，方向是安全的（宁可误报），**不要为它加容差**；
遇到时修正测试或在偏差台账（#262）里逐条裁定。

整条语句只是一个带括号的查询（`(SELECT a FROM t ORDER BY a LIMIT 3)`）时，先去掉最外层括号再分类，
括号里的 ORDER BY / LIMIT 就是这条语句自己的。

### 2.4 limited（LIMIT 截断）

有顶层 `LIMIT` 的查询，内层类别为去掉 LIMIT 后的类别（`limited unordered`、`limited ordered K`、
`limited exact`）。先按内层类别比较，一致即一致。不一致时，若能拿到 A 去掉 LIMIT 后的完整结果
（`a_full`），则：

- 行数必须与 A 相同；
- B 的每一行都必须属于 `a_full`（多重集包含：A 只有一行的值，B 不能出现两次）；
- `limited ordered K`：B 的键序列必须与 A 逐行相同（截断落在并列区段内时，可以是该区段的任意成员）。

拿不到 `a_full` 时保持严格（按内层类别）。`limited exact` 永远严格。runner 只在 LIMIT 子句是
结尾处的 `LIMIT n [, m]` / `LIMIT n OFFSET m`（字面数字）时去掉它重跑 A（`strip_limit`）；
预处理语句的 LIMIT 参数为 `?`、integrationtest 中的 `EXECUTE` 不重跑，保持严格。语句有副作用时
（`NEXTVAL` / `SETVAL`、`@v :=` 赋值、`GET_LOCK` / `RELEASE_LOCK` / `RELEASE_ALL_LOCKS`、`SLEEP`、
`LAST_INSERT_ID(expr)`、`FOR UPDATE` / `FOR SHARE` / `LOCK IN SHARE MODE`）也不重跑：在 A 上多跑一次
会改变 A 的状态（`has_side_effects`）。integrationtest
重跑 A 之后，A 的"上一条语句"状态已变，下一条依赖历史的读语句不再比较（记为 skipped）。

### 2.5 `--sorted_result`

mysqltest 指令 `--sorted_result` 使下一条语句按 unordered 比较（带 LIMIT 时按 limited unordered），
见容差 T2。

## 3. 结果集与 OK 包

- 结果集：先比列数与每列的元数据（name、org_name、schema、table、org_table、类型、长度、小数位、
  flags、字符集），再按 §2 的类别比行。
- 值：按线上协议渲染的文本逐字节比较（带类型标记，`1` 与 `'1'` 不同；二进制协议下 `1.5f64` 与
  `1.5f32` 不同）。
- OK 包：比较 affected rows 与 warning 数。
- 形状不同（结果集 vs OK vs 错误）即不一致。

## 4. 错误与 warning

- 两边都报错：错误码、SQLSTATE、错误消息三项都必须相同，才算一致（`both_error`）。只有错误码相同
  不算一致。
- 一边报错一边不报错：不一致（integrationtest 中 A 成功 B 报错记为 `b_error`，它同样不是一致）。
- B 返回 1235：只有边界桩的回答（消息带 `blocked-by:`）或原型自己的固定文案（`tools/ab/prototype-not-supported.txt`）
  才判为 `unsupported`（容差 T1），既不算一致也不算不一致。A 也返回 1235 时按上一条完整比较；其他
  1235（例如移植来的 Go 代码走到 `ErrNotSupportedYet` 分支）是普通错误，A 成功即不一致。
- warning：先比 warning 数；任一边大于 0 时，runner 必须在语句之后立即在两边各执行一次
  `SHOW WARNINGS`，比较器按 exact 逐行比较（Level、Code、Message）。warning 数大于 0 而 runner
  没有取回内容时，比较器判为不一致（"contents not fetched"），防止 runner 漏取而静默变绿。

## 5. 浮点数

不设浮点容差。数值按协议文本逐字节比较；Go 与 Rust 的浮点格式化、聚合顺序如有差异，是待修的缺陷，
不是比较规则的问题。确有需要时，按 §6 申请编号条目。

## 6. 容差条目

所有放宽写在 `docs/testing/tolerances.toml`，每条有编号（T1、T2……）、名称、适用范围、规则、理由与
日期，格式与偏差台账（#262）共用。比较器中的 `TOLERANCES` 与该文件逐条一致（单元测试
`tolerances_match_the_ledger` 校验）。当前条目：

| 编号 | 名称 | 规则摘要 |
|---|---|---|
| T1 | b-not-supported | B 返回带 `blocked-by:` 的 1235 或原型固定文案的 1235（且 A 不是 1235）判为 unsupported，不算一致，不计入新栈覆盖 |
| T2 | sorted-result | `--sorted_result` 后按多重集比较 |
| T3 | replace-directives | `--replace_regex` / `--replace_column` 同时作用于两边 |
| T4 | result-log-off | `--disable_result_log` 期间只比形状，错误仍完整比较 |
| T5 | go-execute-metadata-drift | Go 自己的 EXECUTE/PREPARE 列定义漂移，B 漂移到同一值不算差异 |

## 7. fixture

`tools/ab/fixtures/compare/*.case`（格式见同目录 README）。每个类别（exact、unordered、ordered、
limited、错误、warning）至少有一个判为一致的用例和一个**必须判为不一致**的用例（文件名以 `neg-`
开头）；缺任何一类，`cargo test -p tidb-ab` 失败。修改比较器时，fixture 必须全部通过；故意放宽
比较器（例如不比较错误消息、把 exact 当成多重集），对应的负 fixture 会变红。

## 8. 判定结果

| 结果 | 含义 | 是否算"一致" |
|---|---|---|
| `match` | 一致 | 是 |
| `both_error` | 两边同一错误（码 + SQLSTATE + 消息） | 是 |
| `unsupported` | B 返回"尚未实现"的 1235（T1） | 否 |
| `mismatch` | 不一致；integrationtest 的 kind 为 rows / order / columns / metadata / error / warnings / affected_rows / shape | 否 |
