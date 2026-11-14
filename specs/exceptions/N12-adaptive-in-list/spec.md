# N12：自适应 IN 列表（列表参数槽、与列表长度无关的计划缓存、超大列表改走 join）

状态：**调研草案**（2026-11-14，[#169](https://github.com/dulao5/tidb-rs/issues/169)）。类型：特性例外（digest、plan digest、EXPLAIN 与 Go 不同）+ 实现例外（plan cache key、range 和下推推迟到执行期）。所在阶段：与 D13.1（实例级 plan cache，例外 A3）同期定稿和实现；`INSERT … VALUES` 部分（§7）推迟到写路径阶段。

负责人在 #169 中已经决定：

- 方案 (1)：计划里放一个"列表参数槽"，执行时按实际元素个数展开（point get、BatchPointGet、range 构造），实例级 plan cache（D13.1）用规范化后的形式做 key。**同意。**
- 方案 (3)：**digest 可以和 Go 不同**（已批准的例外；实现后在 `docs/compatibility.md` 的 "Deliberate deviations" 中登记）。
- 方案 (4)：用 `statements_summary` 和 plan cache 条目数验收。
- 问题 3（超大 IN 列表何时改成临时表 join）没有答案，需要测试摸底：本 spec §3 给出 v8.5.8 上的实测，§4.6 给出建议的判定规则。

引用：TiDB `v8.5.8`（`8b857efa2`），路径相对于 `pkg/`；parser 是单独的 Go module，记为 `parser/…`；client-go `c3b5079`，记为 CG。凡是从代码推出、没有直接读到或测到的结论，标"推断"。

## 0. 总结

1. **Go 的 SQL digest 已经与列表长度无关（≥ 2 个元素时）**。`IN (1,2,3)`、`IN (?,?,?,?)` 都规范化成 `in ( ... )`，`?` 和字面量同样处理（`parser/digester.go:364-371`、`:617-619`），prepared 语句的 digest 在 PREPARE 时由带 `?` 的文本算出（`planner/core/plan_cache_utils.go:135`，EXECUTE 时沿用，`executor/select.go:1055`）。只有 1 个元素的 `IN (?)` 是另一个 digest。plan digest 在 `tidb_ignore_inlist_plan_digest = ON`（新集群默认 ON，升级上来的集群为 OFF）时也忽略 IN 列表。§2.7 实测确认：长度 2…1000 的 IN 在 `statements_summary` 中是同一行。**所以问题 1 的根源不在 digest，在 plan cache。**
2. **Go 的 plan cache 按长度分裂，并且大列表根本不缓存**：cache key 包含语句文本（`plan_cache_utils.go:312`），不同长度就是不同的 PREPARE / 不同的参数化 SQL，各占一个条目；IN 列表元素合计超过 200（`plan_cacheable_checker.go:136-140`、`:637-649`）或非 prepared 语句常量超过 200（`:473-476`）时不缓存；range 超过 `tidb_opt_range_max_size`（默认 64 MiB）退化为全范围时也不缓存（`util/context/plancache.go:179-186`）。§2.7 实测：每个长度一个实例级 cache 条目，长度 > 200 不进缓存。
3. **问题 3 的实测结论**（§3；2,000,000 行、约 80 字节/行、表和索引各约 32–38 个 region，单 TiKV，coprocessor cache 关闭）。60 万个元素时：
   - 整数主键：Go 走 BatchPointGet（快速路径，不比较代价），3.96 s，**TiKV CPU 24.3 s**，TiKV 返回 74 次 `server_is_busy`；同样的值放进临时表做 hash join（全表扫描）0.86 s + 装载 1.10 s，TiKV CPU 0.54 s。
   - 二级索引列 / 无索引列：range 超过 `tidb_opt_range_max_size` 退化为全表扫描 + 下推的 `in(…60 万个常量…)`，**42.0 s / 39.4 s，TiKV CPU 78 s / 79 s，TiDB CPU 17 s / 14 s**；hash join 0.86 s + 装载 1.1–1.2 s，TiKV CPU 0.60 s。
   - 原因有两个，**都与"IN 还是 join"无关**：(a) 按点访问每个 key 约 37 µs（主键）/ 50 µs（非覆盖二级索引）TiKV CPU，而全表扫描约 0.28 µs/行，所以元素数超过表行数的约 1/130（主键）/ 1/180（二级索引）后，扫描 + hash 比点访问便宜；(b) 下推的 `in` 表达式随**每个** cop RPC（含 paging 的每一页）发送和解码一次，额外 TiKV CPU ≈ 0.4 µs × 元素数 × RPC 数，而 RPC 数本身随返回行数增加（32 → 289），所以是超线性的。
   - **这部分 CPU 在 Go 的语句级指标里看不到**：上面 79 s 的语句（`v`，60 万），慢日志 `Process_time` 只有 0.66 s（`Tikv_cpu_time` 也来自 TiKV 的 ProcessTime，同样看不到）；BatchPointGet 的 `Process_time` 是 0。
   - Go 的代价模型对 (a) 只在非快速路径上考虑（主键 IN 走快速路径，从不比较），对 (b) 完全不计：`filterCostVer2` 把一个 `in` 记为 1 个函数，与元素个数无关（`planner/core/plan_cost_ver2.go:935-940`、`:956-966`）。
4. **建议的判定规则**（§4.6）：
   - 规则 A（点访问还是扫描）：按代价比较 `N × c_point` 与 `R_scan × c_scan(row_size) + N × c_hash`，选便宜的；本机测得的分界点约为 N ≈ R / 130（主键）、R / 180（非覆盖二级索引），即 200 万行的表约 1.1–1.5 万个元素。整数主键也要比较（不再有不比较代价的快速路径）。
   - 规则 B（`in` 能否下推）：只有 `N × 预计 RPC 数 ≤ K_push`（建议初值 10⁶，约 0.4 s TiKV CPU）时才把 `in` 下推到 TiKV；超过时 IN 留在 TiDB 做 hash 过滤（或 hash semi join），TiKV 只扫描。
   - 规则 C：range 超过内存上限时**不**退化为"全范围 + 下推 `in`"（Go 最差的情况），而是按规则 A/B 走扫描 + TiDB 侧 hash。
   - **不需要真的建临时表**：临时表的装载（60 万行 1.1 s）和 join 本身一样贵；tidb-rs 直接用内存中的列表做 hash semi join 的 build 侧。
5. **设计**（§4）：IN 列表在 AST 规范化时变成一个"列表参数槽"，plan cache key 用规范化文本（与长度无关）+ 元素类型 + 一个"规模档位"（点访问档 / 扫描档），所以一条语句最多 2 个缓存条目。计划里只记"range / handle / 过滤值来自槽 k"，执行时按实际值构造 range、BatchGet 的 key 和下推的 DAG。计划是 N1 的不可变 `Arc` 树，命中时不需要复制或路径复制。
6. **兼容性**（§5）：查询结果与 Go 相同。SQL digest 可以**保持与 Go 相同**（Go 已经与长度无关）；不同的是 plan digest（档位决定的计划形状、新算子）、EXPLAIN、`last_plan_from_cache`、`statements_summary` 的 plan cache 命中数和若干 warning。

## 1. 问题与范围

| 问题（#169） | 本 spec 的处理 |
|---|---|
| 1. `WHERE id IN (?, ?, …)`：plan cache 条目随列表长度增加 | §4.1–§4.5：列表参数槽 + 与长度无关的 cache key。只读路径，随 D13.1 实现 |
| 2. `INSERT … VALUES (…), (…)`：行数不同就重新生成计划，比 PG array bind 慢 | §7：设计草图，写路径阶段实现 |
| 3. 超大 IN 列表（60 万）下推到 TiKV 代价很大，不知道何时改成临时表 join | §3 实测，§4.6 判定规则和代价模型需要的输入 |

不在范围内：`NOT IN` 的列表（只作为过滤条件处理，见 §4.1 规则 5）；`(a, b) IN ((…), (…))` 行构造器列表（§4.7 后续）；`IN (SELECT …)`（已经是 join，Go 的改写照搬）；TiFlash / MPP（D1）。

## 2. Go v8.5.8 的行为

### 2.1 SQL digest 怎样规范化 IN 列表和 VALUES 行

| 事实 | 位置 |
|---|---|
| `isLit` 把数字、字符串、bit 字面量和 `paramMarker`（`?`）都当作字面量，所以 `?` 与常量一样先被替换成 `?` | `parser/digester.go:617-619`，`reduceLit` `:323` |
| `"?, ?"` → `...`：`isGenericList` 把连续的 `?` 列表折叠成一个 `...` | `parser/digester.go:364-371`（判定 `:545`） |
| `"(...) , ("` → `...`：`isGenericLists` 把多个括号组折叠，所以 `VALUES (…), (…), …` 规范化成 `values ( ... )` | `parser/digester.go:382-388`（判定 `:413`） |
| `IN (?)`（1 个元素）→ `IN (...)` 只在 binding 的规范化中做（`forBinding`），普通 digest 不做 | `parser/digester.go:243-247`、`:483-495` |
| 没有变量控制这种折叠；`tidb_redact_log` 只影响 `Normalize()` 的输出，不影响 `NormalizeDigest` | `parser/digester.go:90-93`、`:125`、`:194` |
| PREPARE 时 digest 由带 `?` 的语句文本计算，存入 `NormalizedSQL` / `SQLDigest` | `planner/core/plan_cache_utils.go:135`、`:214-215` |
| EXECUTE 时语句的 digest 换成 PREPARE 的 digest | `executor/select.go:1055` |
| 其他语句的 digest 由 `NormalizeDigest(sc.OriginalSQL)` 计算 | `sessionctx/stmtctx/stmtctx.go:813-817` |

结论：长度 ≥ 2 的 IN 列表（字面量或 `?`）以及任意行数的 `VALUES` 已经共用一个 SQL digest；**prepared 语句也已经共用 digest**（§2.7 实测：prepared 和非 prepared 的执行落在 `statements_summary` 的同一行）。`IN (?)` 与 `IN (?, ?)` 的 digest 不同。

plan digest：`PhysicalSelection` / `PhysicalProjection` 的规范化信息在 `tidb_ignore_inlist_plan_digest = ON` 时把 `in(...)` 的参数写成 `...`（`planner/core/explain.go:405-410`、`:450-455`，`expression/explain.go:84-97`），否则每个常量写一个 `?`，plan digest 随长度变化。该变量默认 ON（`sessionctx/variable/tidb_vars.go:1519`），GLOBAL 作用域（`sysvar.go:2962`），从旧版本升级上来的集群初始化为 OFF（`session/bootstrap.go:3471`）。BatchPointGet 的规范化信息是 `handle:?`（`planner/core/point_get_plan.go:563-567`），range 是 `range:[?,?]`（`planner/core/explain.go:107-108`、`:221-222`），都与长度无关。但计划形状随长度变化时 plan digest 也变：走快速路径时 `IN (x)` 也是 `Batch_Point_Get`（§2.7 实测），不走快速路径时（如带 ORDER BY）单元素改写成 `=`（§2.4）得到 `Point_Get`；二级索引按代价在 IndexLookUp 和全表扫描之间切换（§3.2 中 20k → 40k）。

### 2.2 prepared plan cache 的 key

- `NewPlanCacheKey`（`planner/core/plan_cache_utils.go:257`）把用户、主机、当前库、**语句原文 `stmt.StmtText`**（`:309-312`）、schema 版本和表版本（`:313-314`）、sql_mode、时区、隔离读引擎、`sql_select_limit`、binding、字符集、是否有子查询、`foreign_key_checks`、`LIMIT ?` 的值（`:356-379`）、统计信息版本、脏表等拼成一个 hash。
- **参数个数和类型不在 key 里**：同一个 key 下可以有多个计划，查找时用 `checkTypesCompatibility4PC` 比较参数类型列表，长度不同就不匹配（`:661-688`，长度比较在 `:667`）。
- 不同长度的 IN 列表是不同的语句文本，所以各自是一个 key、一个条目（推断自 `:312`，§2.7 实测）。
- 实例级 plan cache（`tidb_enable_instance_plan_cache`）用同一个 key（`plan_cache.go:243-248`），`instancePlanCache.Get` 遍历 key 下的条目并调用 `checkTypesCompatibility4PC`（`plan_cache_instance.go:81-91`）；Put 时深拷贝（`plan_cache.go:320-329`），**每次命中再深拷贝一次**（`:255`）。
- PREPARE 的参数超过 65535 个时报 `ErrPsManyParam`（`plan_cache_utils.go:107-111`）。

### 2.3 non-prepared plan cache

- 流程（`planner/optimize.go:85-124`）：`NonPreparedPlanCacheableWithCtx` 判定 → `GetParamSQLFromAST` 把常量换成参数 → 按参数化后的 SQL 文本取缓存的语句 → `GetPlanFromPlanCache`，最终的 key 仍由 `NewPlanCacheKey` 用参数化 SQL 计算。
- `paramReplacer.Enter` 把每个 `driver.ValueExpr` 换成一个 `ParamMarker`，包括 IN 列表的每个元素（`planner/core/plan_cache_param.go:57-83`）；所以参数化 SQL 中 `?` 的个数等于列表长度，长度不同就是不同的 key。
- 常量个数超过 `getMaxParamLimit`（默认 200，`plan_cacheable_checker.go:637-649`；fix control 44823 可改，0 表示不限）时不缓存，原因 "query has too many constants"（`:473-476`）。
- `Optimize` 先试快速路径 `TryFastPlan`（`planner/optimize.go:250`），之后才查 non-prepared plan cache（`:298`）。所以整数主键 / 唯一索引上的 `IN` 字面量查询**从不进入** non-prepared plan cache（§2.7 实测），每次都重新走快速路径（计划很便宜，但 60 万个元素时 Compile_time 仍有 0.08 s）。

### 2.4 IN 列表怎样变成计划

| 步骤 | 事实 | 位置 |
|---|---|---|
| 表达式改写 | 左边是单列、所有元素比较类型相同且元素 ≥ 2 时，构造一个 `in` 函数；否则（包括 1 个元素）改写成 `eq` 的 OR（`a IN (x)` → `a = x`）。这里**不去重**，也没有长度上限 | `planner/core/expression_rewriter.go:2027-2045`（函数 `inToExpression` `:1981-2057`） |
| 参数可能被转换成 INT 时不缓存 | `'%v' may be converted to INT` | `expression_rewriter.go:2006` |
| 快速路径 | `TryFastPlan` → `tryWhereIn2BatchPointGet`：WHERE 只有一个 `PatternInExpr`、不是 NOT、没有 ORDER BY / GROUP BY / LIMIT / HAVING / DISTINCT / 窗口时，直接生成 BatchPointGet，**不走代价模型，没有元素个数上限** | `planner/core/point_get_plan.go:941`、`:971`、`:1321-1332` |
| 物理优化 | 唯一索引或整数主键、所有 range 都是非 NULL 的点时，1 个点生成 PointGet，多个点 `convertToBatchPointGet`，再用 `compareTaskCost` 与其他候选比代价 | `planner/core/find_best_task.go:1972-2003`，`convertToBatchPointGet` `:3290` |
| 点查计划默认不缓存 | 除非每列一个 `=`（场景 1），或打开 fix control 44830 且是单个无重复的 IN（场景 2），否则 "Batch/PointGet plans may be over-optimized" | `find_best_task.go:1993-1995`，`plan_cache_utils.go:690-733` |
| 二级索引的 range | `buildFromIn` 为每个常量生成一对点，跳过 NULL，按排序规则排序并去重 | `util/ranger/points.go:620-716` |
| range 内存上限 | 估算内存 = `(EmptyRangeSize+16) × 点数 / 2 + datum 大小`，超过 `tidb_opt_range_max_size`（默认 64 MiB，0 不限）时**退化为全范围**，IN 留在过滤条件里 | `util/ranger/ranger.go:120-142`，`detacher.go:496-498`、`:873-875`、`:914-927`；默认值 `sessionctx/variable/tidb_vars.go:1601` |
| 退化的后果 | `SetSkipPlanCache("in-list is too long")`，并报 warning `Memory capacity of … bytes for 'tidb_opt_range_max_size' exceeded …`；注释说这种计划"probably suboptimal" | `util/context/plancache.go:179-186` |
| 下推 | `in` 在 TiKV 可下推的函数列表中；protobuf 信号 `InInt` / `InString` 等 | `expression/infer_pushdown.go:160`、`:167`；`expression/builtin_other.go:114`、`:122` |
| TiDB 侧求值 | 常量参数建一个 hash set 加速 | `expression/builtin_other.go:191-230` |
| 代价模型 | 过滤代价 = 行数 × 函数个数 × CPU 因子，`in(col, c1…cN)` 计为 1 个函数，与 N 无关；表达式随请求发送和解码的代价没有对应项 | `planner/core/plan_cost_ver2.go:935-940`、`:956-966` |
| paging 的 RPC 数 | 代价模型用 `paging.CalculateSeekCnt` 估算 paging 请求次数（页大小 128 起翻倍到 50000） | `planner/core/task.go:578`，`util/paging/paging.go:25-60` |
| `IN (SELECT …)` | 满足 `tidb_opt_insubq_to_join_and_agg`（默认 ON）等条件时改写成"去重聚合 + inner join"，否则 semi join / apply | `expression_rewriter.go:1171`、`:1268-1309`；`tidb_vars.go:325`、`:1386` |

### 2.5 执行

- **BatchPointGet**：唯一索引的 key 去重（`executor/batch_point_get.go:259-279`）并排序（`:287`），一次 `BatchGet` 取 handle（`:310`），再用一次 `BatchGet` 取行（`:436`）。client-go 把请求按 region 拆分，每批最多 `batchGetSize = 5120` 个 key（CG `txnkv/txnsnapshot/snapshot.go:74`；按 region 拆分是推断）。
- **coprocessor**：同一个 DAG 请求（`worker.req.Data`）原样发给每个 cop task，只有 `Ranges` 是按 task 切分的（`store/copr/coprocessor.go:1313-1314`）。所以**下推的 `in(col, c1, …, cN)` 表达式随每个 cop RPC 发送一次、在 TiKV 解码一次**，代价约为 N × RPC 数；而按 range 下推时每个 task 只带自己 region 内的点。paging（`tidb_enable_paging` 默认 ON，`sessionctx/variable/tidb_vars.go:1565`）下，一个 task 的每一页都是一次新的 RPC，带同一份 `Data`（`store/copr/coprocessor.go:1509-1530` 计算剩余 range 后重发），所以 RPC 数 ≈ region 数 × 页数，页数随该 region 返回的行数增加（§3 实测 32 → 289）。
- **plan cache 命中后的 range 重建**：`RebuildPlan4CachedPlan` → `rebuildRange`（`planner/core/plan_cache_rebuild.go:31-49`、`:77`）按新参数重算 range；BatchPointGet 要求重建后的点数等于原来的点数（`:205`、`:227`、`:241-242`、`:259-260`），`isSafeRange` 拒绝"原来不是全范围、重建后变成全范围"等情况（`:435-449`），失败则 "skip plan-cache: plan rebuild failed"（`:40`）并重新生成计划。

### 2.6 现有的阈值

| 阈值 | 默认 | 作用 | 位置 |
|---|---|---|---|
| plan cache 参数上限（fix control 44823） | 200 | IN 元素合计 / 非 prepared 常量 / INSERT 行×列超过时不缓存 | `plan_cacheable_checker.go:117-126`、`:136-140`、`:473-476`、`:637-649` |
| `tidb_opt_range_max_size` | 64 MiB | range 内存超过时退化为全范围 + 过滤，且不缓存 | 见 §2.4 |
| PREPARE 参数个数 | 65535 | 超过报错 | `plan_cache_utils.go:107-111` |
| `MaxCacheableLimitCount` | 10000 | `LIMIT ?` 超过时不缓存（与 IN 无关） | `plan_cache_utils.go:61` |
| client-go `batchGetSize` | 5120 | BatchGet 每批 key 数 | CG `snapshot.go:74` |

没有 `MaxInList` 一类的常量，也**没有任何"IN 列表太长时改走 join"的规则**：快速路径的 BatchPointGet 没有上限；二级索引和无索引列只有代价模型（它不计入 IN 表达式随每个 RPC 发送和解码的代价，§3.3）和 64 MiB 的 range 退化。


### 2.7 在 v8.5.8 上实测：plan cache 和 digest

`tools/bench/in-list/plancache_probe.py`，打开实例级 plan cache 和 non-prepared plan cache，对长度 n ∈ {1, 2, 3, 4, 5, 10, 50, 100, 200, 201, 1000} 各执行两次（值不同），看第二次是否命中。完整输出：`tools/bench/in-list/results/plancache.md`。

| 形式 | 列 | n ≤ 200 | n = 201、1000 | 11 个长度新增的实例级缓存条目 |
|---|---|---|---|---:|
| prepared（`PREPARE … IN (?,…,?)`） | `id`（整数主键，BatchPointGet，n = 1 也是） | 第二次命中 | 不命中，warning `skip prepared plan-cache: too many values in in-list` | **9**（每个长度一个） |
| prepared | `k`（二级索引，IndexLookUp） | 第二次命中 | 同上 | **9** |
| 非 prepared（字面量） | `id` | **从不命中**，没有 warning（快速路径在 plan cache 之前，§2.3） | 不命中 | 0 |
| 非 prepared | `k` | 第二次命中 | 不命中，没有 warning（"too many constants" 只在 `EXPLAIN FORMAT='plan_cache'` 中可见） | **9** |

`statements_summary`（同一批执行）：

| digest 文本 | 执行次数 | 不同的 plan digest |
|---|---:|---:|
| `select pad from t where id in ( ... )`（n ≥ 2，prepared 与字面量合在一起） | 40 | 1 |
| `select pad from t where id in ( ? )`（n = 1） | 4 | 1 |
| `select pad from t where k in ( ... )` | 40 | 1 |
| `select pad from t where k in ( ? )` | 4 | 1 |

结论：问题 1 在 Go 中的表现是"**每个长度一个缓存条目，长度超过 200 不缓存**"，而 `statements_summary` 已经按语句聚合。对 tidb-rs 的验收（§6）因此用 plan cache 条目数和命中率，`statements_summary` 的行数只作为"不退化"的检查。

## 3. 基准测试：超大 IN 列表在 TiKV 上的代价（问题 3）

### 3.1 方法

- 环境：tiup playground v8.5.8（1 PD、1 TiKV、1 Go TiDB，tag `maya-in169`，port offset 31690），Apple Silicon 笔记本；**coprocessor cache 关闭**（`tools/bench/in-list/tidb.toml`，否则重复执行同一语句会命中缓存）。数据全部在 TiKV block cache 中。测完已 `scripts/playground.sh clean`。
- 表：`t(id BIGINT 聚簇主键, k BIGINT 非唯一索引 idx_k, v BIGINT 无索引, pad VARCHAR(32))`，2,000,000 行，`k = v = id`，约 80 字节/行，表和索引用 `SPLIT TABLE` 预切成 32 个 region（测试过程中自动分裂到约 38 个）。查找值是固定种子的随机样本，每个值命中一行。
- 变体（X 为 `id` / `k` / `v`）：
  - **X_in**：`SELECT id, pad FROM t WHERE X IN (…)`，Go 的默认计划；
  - **X_tmp_default / _inl / _hash**：值先装进 `CREATE TEMPORARY TABLE tmp(id BIGINT PRIMARY KEY)`（每批 2 万行的多行 INSERT），再 `tmp JOIN t ON t.X = tmp.id`，分别用默认计划、`INL_JOIN(t)`、`HASH_JOIN(tmp, t)`；
  - **X_tmp_subq**：`WHERE X IN (SELECT id FROM tmp)`；
  - **X_in_root**：同 X_in，但把 `in` 加进 `mysql.expr_pushdown_blacklist`，IN 留在 TiDB 求值（近似"内存列表做 hash 过滤"，不需要临时表）。
  - `VALUES` 派生表：v8.5.8 不支持 `VALUES` 表构造器，没有测。
- 指标：客户端墙钟时间；**TiKV / TiDB 进程 CPU**（语句前后 `ps` 的差值，含少量后台工作，±0.05 s）；慢日志 `Process_time`、`Compile_time`；EXPLAIN ANALYZE 中各算子 `num_rpc` 之和（RPC 数）。每个组合跑 3 次取中位数，另跑一次 EXPLAIN ANALYZE 记录计划。
- 复现：`tools/bench/in-list/run.sh`（启动、装载、三轮测量、plan cache 探测、汇总、清理）。全部结果：`tools/bench/in-list/results/summary.md`。

### 3.2 结果

墙钟时间 / TiKV CPU，单位秒；临时表变体的"+x"是装载临时表的时间（另计）。**粗体**为该行最快（含装载）。

**整数主键 `id`**

| N | Go 默认：BatchPointGet | 临时表 + IndexHashJoin（默认计划） | 临时表 + HashJoin（全表扫描） |
|---:|---|---|---|
| 1,000 | **0.007 / 0.05** | 0.007+0.004 / 0.05 | 0.079+0.004 / 0.54 |
| 3,000 | **0.020 / 0.12** | — | 0.077+0.007 / 0.54 |
| 10,000 | **0.063 / 0.37** | 0.055+0.018 / 0.40 | 0.082+0.018 / 0.53 |
| 20,000 | 0.127 / 0.77 | — | **0.084+0.035 / 0.54** |
| 40,000 | 0.254 / 1.53 | — | **0.091+0.071 / 0.54** |
| 100,000 | 0.628 / 3.70 | 0.418+0.181 / 3.59 | **0.156+0.177 / 0.55** |
| 600,000 | 3.959 / 24.29（74 次 `server_is_busy`） | 1.165+1.143 / 8.09 | **0.863+1.102 / 0.54** |

**二级索引 `k`（非唯一、非覆盖）**

| N | Go 默认 | 临时表 + IndexHashJoin（默认计划） | 临时表 + HashJoin | IN 留在 TiDB（`_root`） |
|---:|---|---|---|---|
| 1,000 | IndexLookUp **0.011 / 0.06** | 0.010+0.003 / 0.06 | 0.086+0.004 / 0.59 | — |
| 3,000 | IndexLookUp 0.030 / 0.16 | **0.023+0.007 / 0.16** | 0.086+0.007 / 0.58 | — |
| 10,000 | IndexLookUp 0.094 / 0.51 | **0.063+0.018 / 0.45** | 0.094+0.018 / 0.59 | IndexLookUp 0.103 / 0.51 |
| 20,000 | IndexLookUp 0.190 / 1.00 | 0.117+0.038 / 0.85 | **0.093+0.035 / 0.59** | — |
| 40,000 | 全表扫描 + 下推 `in`：0.752 / 2.45 | 0.206+0.072 / 1.59 | **0.098+0.072 / 0.60** | — |
| 100,000 | 全表扫描 + 下推 `in`：3.281 / 6.80 | 0.461+0.186 / 3.66 | **0.156+0.181 / 0.60** | 0.464 / 0.62 |
| 600,000 | range 退化 → 全表扫描 + 下推 `in`：**42.03 / 77.61**（TiDB CPU 17.1） | 1.449+1.269 / 10.69 | **0.856+1.102 / 0.60** | 4.632 / 0.58（Compile 3.78 s） |

**无索引列 `v`**

| N | Go 默认：全表扫描 + 下推 `in`（RPC 数） | 临时表 + HashJoin | IN 留在 TiDB（`_root`） |
|---:|---|---|---|
| 1,000 | **0.067 / 0.51**（32） | 0.088+0.004 / 0.61 | — |
| 3,000 | **0.072 / 0.54**（38） | 0.086+0.007 / 0.61 | — |
| 10,000 | 0.108 / 0.73（64） | **0.093+0.018 / 0.59** | 0.106 / 0.60 |
| 20,000 | 0.251 / 1.18（102） | **0.092+0.036 / 0.60** | — |
| 40,000 | 0.770 / 2.41（140） | **0.102+0.073 / 0.60** | — |
| 100,000 | 3.060 / 6.88（160） | **0.152+0.192 / 0.60** | 0.398 / 0.62 |
| 600,000 | **39.36 / 79.40**（289，TiDB CPU 14.3） | **0.865+1.218 / 0.60** | 2.586 / 0.56（Compile 1.72 s） |

临时表 + `IN (SELECT …)` 与"临时表 + 默认计划"的计划和数字相同（Go 改写成 join，§2.4），见 `results/summary.md`。

### 3.3 解读

1. **点访问的代价是线性的，而且比扫描贵两个数量级**。BatchPointGet 每个 key 约 37–40 µs TiKV CPU（10k：0.37 s，100k：3.70 s，600k：24.3 s）；二级索引回表约 50 µs/key；全表扫描 200 万行约 0.54–0.60 s，即约 0.28 µs/行。TiKV CPU 的分界点：主键约 N ≈ 15,000（表的 0.75%，≈ R/130），二级索引约 N ≈ 12,000（≈ R/180）。墙钟时间的分界点相近（主键 20k 时 0.127 s 对 0.084+0.035 s）。60 万个主键时 BatchGet 把单个 TiKV 压到 `server_is_busy`，会影响同一集群上的其他负载。
2. **下推 `in` 的代价是超线性的**。额外 TiKV CPU（减去扫描本身约 0.5 s）与"元素数 × RPC 数"成正比，系数稳定在约 0.4 µs：10k × 64 = 64 万 → 0.23 s；100k × 160 = 1,600 万 → 6.4 s；600k × 289 = 1.73 亿 → 79 s。RPC 数随返回行数增加（paging 一页一个 RPC，每页都带整个表达式，§2.5），所以 N 增大 10 倍，代价增大 30 倍以上。TiKV 在哪一步花掉这些 CPU（解码 protobuf、构造常量、建 hash set）没有拆开测（推断：都是每个请求一次、与 N 成正比的工作）。
3. **这部分代价在语句级指标中看不到**：60 万 `v_in` 的慢日志 `Process_time` 是 0.66 s，与 1,000 个元素时（0.48 s）几乎一样（TiKV CPU 0.51 s 对 79 s）；`Tikv_cpu_time` 取自同一个 ProcessTime（N9 §1.3）；RU 也只按扫描计（约 2,790 RU，与全表 hash join 相当）。BatchPointGet 的 `Process_time` 为 0。用 `statements_summary` 只能看到墙钟时间变长，看不到 TiKV 被压满。
4. **Go 的优化器两种情况都选错**：主键 IN 走快速路径，不比较代价；二级索引在 40k、100k 时代价模型正确地放弃了 IndexLookUp，却选了"全表扫描 + 下推 `in`"，因为 `in` 的过滤代价与 N 无关（§2.4），比真正最便宜的 hash join（0.156 s）慢 21 倍；60 万时 range 内存超限，退化成同样的计划。
5. **"临时表 join"有效，是因为它让值不再下推、并让优化器能选扫描**，临时表本身没有贡献，反而带来装载成本（60 万行 1.1 s，和 join 本身一样）。`_root` 变体说明：把 IN 留在 TiDB 就消除了 TiKV 侧的代价（600k：TiKV CPU 0.56 s），剩下的是 Go 在 TiDB 侧为 60 万个常量建表达式的成本（Compile_time 1.7–3.8 s，TiDB CPU 2.7–4.7 s）。列表参数槽（§4）把值保存为一个扁平数组，不为每个元素建表达式，这部分在 tidb-rs 中预计降到 hash set 构造（推断：60 万个 int64 约 10 ms 量级）。
6. 局限：单 TiKV、数据全在内存、行很窄。真实集群上点访问可能要读盘（更贵），宽行让扫描更贵，多 TiKV 让扫描并行度更高。所以**分界点不能写死成常数**，只能用代价模型按表的行数、行宽、region 数计算（§4.6）；本节的数字用来校准代价因子和验收。

## 4. 设计

### 4.1 列表参数槽（规范化）

在计划缓存的参数化阶段（prepared：PREPARE 时；非 prepared：对应 Go `ParameterizeAST` 的位置），满足以下条件的 `expr [NOT] IN (e1, …, eN)` 整体换成**一个**列表参数槽 `ListParam{slot}`：

1. 右边每个元素都是字面量或 `?`，N ≥ 1。
2. 左边是单个表达式（不是行构造器；行构造器见 §4.7）。
3. 字面量形式下，所有元素属于同一个字面量类别（都是整数 / 都是字符串 / 都是 decimal …）；`?` 形式在绑定时检查，类别不一致的执行退回逐元素（Go）路径，并且不进缓存。元素中的 `NULL`：绑定时记下"含 NULL"标志，求值按 Go 的三值逻辑（没有匹配且含 NULL 时结果为 NULL，所以 `NOT IN` 不返回行、选择列表中的 `x IN (…)` 返回 NULL）；构造 range / handle 时跳过 NULL（同 Go `buildFromIn`，`util/ranger/points.go:646-648`）。由验收 A3 的结果对照覆盖。
4. 规范化后的文本写作 `… IN (?…)`，一条语句可以有多个槽。每个槽的元素类型（统一后的比较类型）进入缓存匹配，元素个数不进入。
5. `NOT IN` 也用槽，但只作为过滤条件（不构造 range），与 Go 相同。

槽替换的是 Go 中"每个元素一个 `ParamMarker`"的做法（`plan_cache_param.go:57-83`），因此 Go 的 200 个参数上限（`plan_cacheable_checker.go:136-140`）对槽内元素**不再适用**：上限的理由是"to save memory"（`:138`），而槽内的值不存进缓存的计划。INSERT 的上限见 §7。

### 4.2 形状稳定的计划与规模档位

优化器看到的是 `in(col, ListParam{slot, elem_type, n_est})`，`n_est` 取本次（生成计划时）的实际元素个数。物理计划只有两个**档位**，每个档位内的形状与 N 无关：

| 档位 | 适用 | 计划形状 | 值在执行时的用法 |
|---|---|---|---|
| 点访问档 | §4.6 规则 A 判定点访问更便宜 | 整数主键 / 唯一索引：`BatchPointGet`（N = 1 也用它，Go 的快速路径对 `IN (x)` 也生成 BatchPointGet，§2.7）；其他索引：`IndexRangeScan` / `IndexLookUp` / `TableRangeScan`，range 来源记为"槽 k" | 去重、按排序规则排序，生成 handle / key / range |
| 扫描档 | 规则 A 判定扫描更便宜，或规则 C | 读算子按其他条件扫描（无其他条件则全表），上面接 `InListSemiJoin`（hash semi join，build 侧是槽里的值）；或 `in` 作为过滤下推（只在规则 B 允许时） | 建 hash set（记入语句内存，A1）；下推时把值序列化进 DAG |

档位进入缓存匹配，所以一条语句最多 2 个条目（每个槽的档位独立时为 2^槽数，实际中槽数为 1–2）。执行时按实际 N 重新判定档位：落在同一档位就复用，否则查（或生成）另一档位的计划。判定只用缓存在计划上的数据（表行数估计、行宽、region 数、代价因子），O(1)，不重新优化。

`InListSemiJoin` 是新算子（EXPLAIN 中可见），语义等于 `col IN (值集合)` 的过滤；实现上是 Go `HashJoin`（semi）的 build 侧换成内存数组，probe 侧照 Go。不建临时表（§3.3 第 5 点）。

### 4.3 range、BatchGet 和下推推迟到执行期

- 计划节点保存 `RangeSource::List(slot)` / `HandleSource::List(slot)`，不保存具体的 range 或 handle。执行器构建时（D12）从执行参数中取出槽的值，调用移植自 Go 的 `buildFromIn` 逻辑（`util/ranger/points.go:620-716`：转换类型、跳过 NULL 和非法值、排序、去重；L1）生成 range；BatchPointGet 照 Go 去重排序（`executor/batch_point_get.go:259-287`）后按 region 分批 BatchGet（每批 ≤ 5120 个 key，与 client-go 相同）。
- 下推的 `in` 过滤：DAG 在执行时序列化，值来自槽。规则 B 在这里再检查一次（按实际 N 和本次的 task 数）：超过 `K_push` 时不下推，改在 TiDB 侧过滤。
- Go 的"range 超过 `tidb_opt_range_max_size` 时退化为全范围"（`util/ranger/ranger.go:134-142`）在槽上按规则 C 处理：转到扫描档，而不是"全范围 + 下推 `in`"。变量本身的语义（range 内存上限）不变。
- Go 禁止缓存点查计划的理由是"over-optimized"（如 `a>=? AND a<=?` 被折叠成点，换参数后不成立；`find_best_task.go:1993-1995`）。槽从构造上只表示"等于其中一个值"，计划不依赖具体值，所以这个顾虑不适用；值的类型转换、溢出、截断在执行时按 Go 的 ranger 规则处理。

### 4.4 plan cache key（D13.1）

- key = Go `NewPlanCacheKey` 的全部字段，只把 `stmt.StmtText`（`plan_cache_utils.go:312`）换成**规范化后的文本**（槽写作 `(?…)`）。用户、库、schema 版本、统计信息版本、sql_mode 等照 Go。
- 同一 key 下的条目按（非槽参数的类型列表，各槽的元素类型，各槽的档位）匹配，对应 Go `checkTypesCompatibility4PC`（`:661-688`）的位置。
- 不同个数的 `?` 的 PREPARE（`IN (?,?,?)` 与 `IN (?,?,?,?,?)`）规范化后是同一文本，所以**在实例级缓存中共用计划**。会话中的 prepared 对象（语句 ID、参数个数、参数元数据）仍按协议各自独立。
- 只做实例级 plan cache（#128 第 5 项，负责人已定）。缓存的值是 N1 的 `Arc` 物理计划，不含槽的值，内存与 N 无关。

### 4.5 与 N1、N2、N4 的关系

- **N1**：物理计划是不可变的 `Arc` 树（D09.7 试点建议 P2）。槽相关的节点在命中时**不需要改写**：range 和 handle 在执行器构建时生成，放在执行状态里，不写回计划。所以 D09.7 §5 的"路径复制"只剩非槽参数（如 `a > ?` 的 range）需要。
- **N2**：槽的值是执行参数的一部分，按 D13.1 P1-2 的约定是共享的 `Arc<[Datum]>`，放在语句的执行上下文中，不放在计划里。
- **N4**：表达式 `in(col, ListParam)` 编译成 RPN 时只记槽号；hash set 在执行状态中按语句构建一次。不使用计划节点上的可变缓存（D09.7 §5 风险 3：共享表达式上的 `BuiltinFuncCache` 会在会话间争用）。
- **N9**：执行记录中记下每个槽的 N 和所选档位，慢日志和 EXPLAIN ANALYZE 可以显示（具体字段名随 N9 定）。§3.3 第 3 点的观测缺口（请求解码的 TiKV CPU 不在 `Process_time` 中）是 TiKV 一侧的问题，tidb-rs 不能补；验收改用进程 CPU（§6）。

### 4.6 判定规则与代价模型需要的输入

规则（在优化时按 `n_est` 选初始档位，执行时按实际 N 复核）：

- **规则 A（点访问 vs 扫描）**：`cost_point = N_distinct × c_point(访问路径)`；`cost_scan = R_scan × c_scan(row_size) + R_scan × c_probe + N × c_build`。`cost_point ≤ cost_scan` 时点访问档，否则扫描档。整数主键和唯一索引也参与比较（取代 Go 不比较代价的快速路径）。本机校准值：`c_point` ≈ 37 µs（主键 / 唯一索引的点读）、≈ 50 µs（非覆盖二级索引：索引 + 回表）；`c_scan` ≈ 0.28 µs/行（约 80 字节/行）；分界点约 R/130、R/180。
- **规则 B（下推 `in`）**：`N × RPC_est ≤ K_push` 才下推，`RPC_est = 涉及的 region 数 × paging 页数`（页数用 Go 已有的 `paging.CalculateSeekCnt`，按每个 region 预计返回的行数估算）。`K_push` 建议初值 10⁶（本机 ≈ 0.4 s TiKV CPU）。不满足时扫描档 + TiDB 侧 hash 过滤。
- **规则 C（range 内存超限）**：转扫描档，不退化为"全范围 + 下推 `in`"。
- **兜底**：N 小于某个下限（建议 1,000）时不做判定，直接点访问档（本机所有变体在 N ≤ 3,000 时点访问都最快或持平）。

代价模型需要的输入（Go 的模型 v2 现在缺的用 ✗ 标出）：

| 输入 | 来源 | Go v8.5.8 |
|---|---|---|
| N（去重后的元素个数） | 执行参数；优化时用 `n_est` | 有（通过 range 个数），但快速路径不用 |
| 表 / 范围的行数 `R_scan`、行宽 | 统计信息 | 有 |
| 访问路径是否覆盖、是否需要回表 | 索引元数据 | 有 |
| 每次点读的代价 `c_point` | 新代价因子（或由现有的 seek / request 因子组合），需要校准 | ✗ 快速路径不计代价 |
| 下推表达式的大小 × 请求数（`N × RPC_est × c_decode`） | 新代价项；`RPC_est` 来自 region 数 × `CalculateSeekCnt` | ✗ `in` 计为 1 个函数（`plan_cost_ver2.go:935-940`） |
| 表 / 索引范围的 region 数 | region cache 或统计信息 | ✗ 代价模型不用 region 数 |
| TiDB 侧 hash build / probe 的 CPU 因子 | 现有的 hash join 代价项 | 有 |
| hash set 的内存（N × 元素大小） | 语句内存预算（A1） | 不涉及 |

校准：`tools/bench/in-list` 的数字来自单 TiKV 笔记本，只能作为初值；需要在 3 TiKV 的集群（磁盘读）上重测（§8 第 4 点）。

### 4.7 以后的扩展（不在本 spec 的实现范围内）

- 行构造器 `(a, b) IN ((?, ?), …)`：同样的槽，元素是元组；Go 的 `tryWhereIn2BatchPointGet` 已支持复合唯一索引上的这种形式。
- 多个 IN 的组合（`a IN (…) AND b IN (…)`）：range 是笛卡尔积，规则 C 更容易触发；按槽各自计算后组合。
- 客户端直接发数组（PG 风格的 `= ANY($1)`，C3）：天然就是一个槽。

## 5. 兼容性说明（与 Go 的差异）

查询结果（行的集合、NULL 语义、重复值、类型转换）与 Go 相同。无 ORDER BY 时的行顺序本来就不保证，可能不同。

| 方面 | Go v8.5.8 | tidb-rs | 用户可见的影响 |
|---|---|---|---|
| SQL digest | 长度 ≥ 2 折叠为 `in ( ... )`，`IN (?)` 单独一个（§2.1） | **建议与 Go 相同**（槽只用于内部的缓存 key，不改 digest） | 无。负责人已批准 digest 可以不同；若选择把 `IN (?)` 也并入 `in ( ... )`，`statements_summary` 中长度 1 的语句会并入同一行（§8 第 1 点） |
| plan digest | 随计划形状变化；`tidb_ignore_inlist_plan_digest` 影响是否计入 IN 常量 | 按档位：同一语句最多 2 个 plan digest；扫描档出现新算子 `InListSemiJoin` | 与 Go 节点对照时 plan digest 不同；SQL binding 的计划匹配要用 tidb-rs 自己的 plan digest |
| EXPLAIN | 主键 IN 总是 `Batch_Point_Get`；二级索引按代价，可能是全表扫描 + `in(…)`；超大列表时全范围 + `in(…)` | 按规则 A/B/C：大列表时 `InListSemiJoin`；`in` 下推只在规则 B 允许时出现；operator info 中显示本次的值（与 Go 一样列出 handle / range） | 计划不同，结果相同 |
| `@@last_plan_from_cache` / `PLAN_CACHE_HITS` / `PLAN_IN_CACHE` | 不同长度各自缓存；> 200 个元素不缓存；主键 IN 字面量不走 non-prepared 缓存 | 与长度无关地命中 | 命中率更高 |
| warning | `skip prepared plan-cache: too many values in in-list`、`Batch/PointGet plans may be over-optimized`、`Memory capacity of … for 'tidb_opt_range_max_size' exceeded …` | 槽适用时前两个不再出现；第三个出现时含义变为"转扫描档"（文案保留，§8 第 3 点） | `SHOW WARNINGS` 的内容不同 |
| `tidb_opt_range_max_size` | 超限时退化为全范围 | 超限时转扫描档（规则 C） | 计划不同 |
| 混合集群（#153 D1，ADR 0009 模式 1/4） | — | 结果一致，计划级字段（plan digest、EXPLAIN、warning、plan cache 命中）不同 | 影子读对照要忽略这些字段 |

实现后在 `docs/compatibility.md` 的 "Deliberate deviations from v8.5.8" 中登记（负责人决定 (3)），本 spec 合并时不改 `docs/compatibility.md`（尚未实现）。

## 6. 验收

| # | 验收项 | 方法 | 目标 |
|---|---|---|---|
| A1 | plan cache 条目与长度无关 | `plancache_probe.py` 对 tidb-rs 运行（prepared + 字面量，`id` / `k`，n = 1…1000 及 10,000） | 每条语句的实例级缓存条目 ≤ 档位数（2），Go 为每个长度 1 个（§2.7）；n > 200 也命中 |
| A2 | `statements_summary` 不退化 | 同上，查 `statements_summary` | 每条语句的行数与 Go 相同（建议的 digest 方案下）；`PLAN_CACHE_HITS / EXEC_COUNT` 预热后 ≥ 99%；不同的 plan digest ≤ 2 |
| A3 | 结果等价 | tools/ab 对照：随机列表（含重复、NULL、越界值、有符号/无符号混合、字符串与排序规则、`NOT IN`），长度 1…10⁵，Go 与 tidb-rs 比较结果集（不计顺序） | 全部一致 |
| A4 | 大列表性能（问题 3） | `tools/bench/in-list` 对 tidb-rs 运行（`_in` 变体） | N = 600,000 时 `k` / `v` 的墙钟 ≤ 2 s、TiKV CPU ≤ 1 s（Go：42 s / 78 s）；`id` 的 TiKV CPU ≤ 1 s（Go：24 s） |
| A5 | 小列表不变慢 | 同上，N ≤ 10,000 | 墙钟与 TiKV CPU 不高于 Go 的 110% |
| A6 | 规划成本 | 慢日志 `Compile_time` | 命中缓存时与 N 无关；N = 600,000 未命中时 ≤ 0.2 s（Go：1.1–3.8 s） |
| A7 | TiKV CPU 的测量 | 不用 `Process_time` / `Tikv_cpu_time`（§3.3 第 3 点），用 TiKV 进程 CPU 或 TiKV 的 thread CPU 指标 | 写进测试脚本（已是 `bench.py` 的做法） |

## 7. `INSERT … VALUES` 设计草图（推迟到写路径阶段）

Go v8.5.8 的事实：

- digest 已经把 `VALUES (…), (…), …` 折叠成 `values ( ... )`（`parser/digester.go:382-388`），所以 `statements_summary` 已按语句聚合。
- plan cache：prepared INSERT 的行 × 列超过 200 时不缓存（`plan_cacheable_checker.go:117-126`），非 prepared 同理（`:252-253`）；行数不同是不同的语句文本，各占一个条目（同 §2.2）。
- MySQL 协议没有数组绑定，客户端驱动把 R 行展开成 R × C 个 `?`；PREPARE 最多 65535 个参数（`plan_cache_utils.go:107-111`），所以单条语句最多 65535 / C 行。

草图：

1. **行批参数槽**：`VALUES (?, …, ?), …` 规范化为一个"行批槽"`VALUES (?…)×`，元素是 C 列的行；cache key 只含列数和各列类型，与行数无关。
2. 计划中的 `Insert` 节点保存"行来自槽 k"，执行时逐列对整个批做类型转换、默认值、生成列（按 Go 的逐行语义；能向量化的列按 A5 处理）。
3. 唯一性检查、自增 ID 分配、`ON DUPLICATE KEY UPDATE`、`INSERT IGNORE` 的语义照 Go（`batchCheckAndInsert` 一类的批量 BatchGet 已经是按批的）。
4. 内存：批按语句内存预算（A1）切分；单批过大时分段处理，事务语义不变。
5. 与 PG 风格数组绑定的关系：C3（PG 协议）中的数组参数直接映射为行批槽；MySQL 协议下的收益只来自"共用计划"，不减少网络上的参数个数（推断：主要收益是省掉每种行数一次的优化和 Go 的 200 上限）。
6. 验收（届时）：同一 INSERT 形状只有 1 个缓存条目；R = 1…1000 的写入吞吐不低于 Go。

## 8. 需要负责人决定

1. **SQL digest**：建议**保持与 Go 相同**（Go 已经与长度无关，§2.1），把批准的例外只用在 plan digest / EXPLAIN 上；另一选项是把 `IN (?)` 也并入 `in ( ... )`（与 binding 的规范化一致，`digester.go:243-247`），代价是与 Go 节点的 digest 在长度 1 时不同。
2. **开关**：是否加一个 tidb-rs 自己的系统变量（例如 `tidb_rs_adaptive_in_list`，默认 ON）在混合集群中关掉这套行为、回到 Go 的计划。建议加（#153 D1："用户可见行为跟随集群的 Go 基线"；有开关时运维可以选择计划一致）。
3. **warning 文案**：规则 C 触发时，是保留 Go 的 `tidb_opt_range_max_size` warning 文案（建议），还是换成说明"转扫描档"的新 warning。
4. **校准环境**：§3 的数字来自单 TiKV 笔记本。是否在 3 TiKV、数据不全在内存的集群上重测，再定 `c_point`、`c_scan`、`K_push` 的初值（建议在 D13.1 开工前做一次）。
5. **`K_push` 和兜底下限**是做成代价因子（随代价模型校准）还是系统变量。建议代价因子，不加新变量。

## 9. 替换的 Go 范围

以下函数在 tidb-rs 中按本 spec 改写（tracker 标为"例外 N12"）；其余（`buildFromIn` 的转换和排序、BatchPointGet 执行器、hash join 的 probe 侧、代价模型 v2 的已有项）照 L1 移植。

| Go 位置 | 改写 |
|---|---|
| `planner/core/plan_cache_param.go` `paramReplacer.Enter`、`ParameterizeAST` | 满足 §4.1 的 IN 列表换成一个槽 |
| `planner/core/plan_cacheable_checker.go` `PatternInExpr` 分支（`:136-140`）、`getMaxParamLimit` 对槽的适用 | 槽内元素不计入 200 上限 |
| `planner/core/plan_cache_utils.go` `NewPlanCacheKey`（`StmtText` → 规范化文本）、`checkTypesCompatibility4PC`（加槽元素类型和档位）、`isSafePointGetPath4PlanCache` | §4.4、§4.3 |
| `planner/core/plan_cache_rebuild.go` `rebuildRange` 中 BatchPointGet / range 的槽部分 | 不再改写计划，执行时生成 |
| `planner/core/point_get_plan.go` `TryFastPlan` / `tryWhereIn2BatchPointGet` | 槽适用时不走快速路径，按规则 A 比较代价 |
| `planner/core/find_best_task.go` 点查转换（`:1972-2003`） | handle 来自槽 |
| `planner/core/expression_rewriter.go` `inToExpression` | 槽 → `in(col, ListParam)`；单元素不改写成 `eq`（槽适用时） |
| `planner/core/plan_cost_ver2.go` `filterCostVer2` 对 `in`、新代价项 | §4.6 |
| `util/ranger` 的 range 内存退化（`ranger.go:134-142` 等）对槽的处理 | 规则 C |
| `planner/core/explain.go`、`point_get_plan.go` 的 operator info | 档位与新算子的显示 |
| 新增 | `InListSemiJoin` 算子及其执行器 |
