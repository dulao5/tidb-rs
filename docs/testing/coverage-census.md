# 防假绿：新栈覆盖率普查与负对照

来源：#260（负责人 2026-11-20 采纳 pgrust 经验报告第 ① 条，"门禁的门禁"）。结果是否一致由唯一的
比较器判定（`docs/testing/result-comparison.md`，#261）；本文规定"每条语句到底走了哪条路"以及
门禁本身何时算失败。

## 1. 问题

`--new-stack=fallback` 下新栈走不通会回退到原型，结果照样正确；只看"结果一致"分不出是新栈真的
执行了，还是原型接手了。分母变小（语句被跳过、被 runner 弄丢）也能让比例好看。

## 2. 四类普查

`ab` 的每次运行（`ab integrationtest`、`ab sysbench`、`ab file`）给它**应当执行的每一条语句**
恰好一个普查类别：

| 类别 | 含义 | 原因（reason） |
|---|---|---|
| `new_stack` | B 在新栈上执行（路由 engine = `new`） | `ok`，或 `error`（新栈返回了 Go 移植的错误） |
| `fallback` | B 由原型执行，或 strict 模式下因只能由原型执行而返回 1235（engine = `poc` / `none`） | 路由原因：`kind`、`ast`、`session`、`sql_mode`、`schema_lease`、`schema_version`、某个 `blocked-by` 的块名、`new_stack_off` 等 |
| `error` | 发给了 B，但无法归属到某条路径 | 连接错误、超时、在路由前被拒（如 PREPARE 失败）、路由跟踪读不到 |
| `skipped` | 根本没有发给 B | runner 的原因：写语句只在 A 上执行、事务中、会话已分叉、文件其余部分无法运行等 |

- **分母**在运行之前从输入算出：integrationtest 为所选 `.test` 文件中的语句数，workload 为
  （语句 × 协议）数。四类之和必须等于分母，否则输出 `INCOMPLETE CENSUS`，`ab` 以退出码 3 结束。
  空运行（分母为 0）同样失败。
- runner 中途放弃的语句（连接失败后"文件其余部分跳过"、整个文件建库失败）逐条记为 `skipped` 并
  写明原因，不允许静默消失。
- **只有 `new_stack` 且比较器判为一致（`match` / `both_error`）才计入新栈覆盖**（`new_stack_equal`）。
  `unsupported`（1235，容差 T1）永远不计入。

## 3. 负对照

每次运行在最后执行一个负对照：在 A 和 B 上各执行 `SELECT 'negative control' AS c`，把 B 的回答
**故意篡改**后交给同一个比较器。它必须被判为 `mismatch`；被判为一致，或者没有执行，门禁本身失败
（退出码 3，`NEGATIVE CONTROL PASSED` / `NOT RUN`）。负对照不计入分母。

各门禁层的负对照：

| 门禁层 | 负对照 | 不执行或通过时 |
|---|---|---|
| 比较器 fixture（`cargo test -p tidb-ab`） | 每个类别至少一个 `neg-*` fixture | 测试失败（缺 `neg-*` 也失败） |
| `ab integrationtest`（含 weekly integrationtest 部分） | 上述篡改语句 | `ab` 退出码 3 → 部分 fail |
| `ab sysbench` / `ab file`（weekly ab 部分、strict 轮、tpch） | 同上 | 同上 |
| weekly 报告（`tools/weekly/report.py`） | 每个应有普查的部分必须有 `census-summary.tsv`，且其负对照为 `failed as required` | RED |

## 4. 名称校验

- 普查层名只能是 `integrationtest`、`workload`、`workload-strict`、`tpch`（`tools/ab/src/census.rs`
  的 `LAYERS`，`tools/weekly/report.py` 的 `CENSUS_LAYERS`）。`ab --census-layer` 遇到未知层名直接
  报错退出；报告遇到未知层名、未知列名或格式错误的行判 RED。
- 普查类别只有上表四个；报告按固定列读取，不认识的列即失败。

## 5. 输出

每次 `ab` 运行在 `--out-dir` 下写：

- `census.tsv`：每条语句一行（layer、unit 文件或模板、line、protocol、path、reason、verdict、covered）；
- `census-summary.tsv`：layer、expected、total、new_stack、fallback、error、skipped、
  new_stack_equal、negative_control、status（`ok` 或失败原因）；
- `census-reasons.tsv`：layer、path、reason、count（回退停在哪个 `blocked-by`、跳过的原因）。

weekly 报告把所有 `census-summary.tsv` 汇总成"Coverage census"一节（写入 CI 的 step summary 和
#54 的评论），并在 `report.md` 旁写出合并后的 `census.tsv`（随结果 artifact 上传，tracker 可导入）。
与上一次运行相比，某层的语句总数（分母）变小或 `new_stack_equal` 变小 → REGRESSED（棘轮：只升不降）。
tracker 的 `import-integration` 同时导入 integrationtest 的普查（见 tracker 仓库）。

## 6. 服务端的最小可观测信号：路由跟踪

`tidbrs_route_total{engine, reason}` 只有全局计数，分不出单条语句。为此 tidb-rs 增加一个默认关闭的
路由跟踪：

- 开启：`tidb-server --route-trace`，或环境变量 `TIDB_RS_ROUTE_TRACE=1`。
- 每个连接每次路由决策（路由器的每个 `tidbrs_route_total` 计数点；`--new-stack=off` 时原型执行的
  每个 COM_QUERY / COM_STMT_EXECUTE，reason 为 `new_stack_off`）把该连接的序号加一，并记录 engine
  与 reason。
- 读取：状态端口 `GET /debug/route?conn=<连接 id>`，返回 `<序号>\t<engine>\t<reason>`；跟踪关闭时
  返回 404。
- `ab` 在 B 上执行每条语句之前、之后各读一次本连接的记录：序号增加才归属为该语句的路径，否则记为
  `error`（未经路由）。按连接读取，其它连接的语句不会混入。
- 关闭时开销只是一次 relaxed 原子读；打开时每条语句要拿一次进程级的锁（`parking_lot::Mutex`）。
  连接关闭时删除其记录。不改变任何语句的执行与结果。
- **测性能的实例不开**：weekly 的 sysbench 部分（tidb-rs 与 Go 的 QPS 比值）以及以后的性能门禁（#263）
  都不加 `--route-trace`；只有要做普查的部分（ab、tpch、integrationtest）打开。

`scripts/integrationtest.sh` 与 `tools/weekly/run.sh` 中做普查的部分启动的 tidb-rs 带 `--route-trace`，并把
状态地址交给 `ab --b-status`。integrationtest 默认以 `--new-stack=fallback` 运行（`-n` 可改），
weekly integrationtest 部分同样使用 fallback，测试文件取自参照树（`scripts/tidb-reference.sh`）。

## 7. 证明门禁会变红

隐藏参数 `ab --gate-fault <名称>` 用于演示（未知名称直接报错）：

| 故障 | 效果 | 结果 |
|---|---|---|
| `drop-census-record` | 丢掉一条普查记录（模拟 runner 弄丢一条语句） | `INCOMPLETE CENSUS`，退出码 3 |
| `skip-negative-control` | 不执行负对照 | `NEGATIVE CONTROL NOT RUN`，退出码 3 |
| `untampered-negative-control` | 不篡改负对照的回答（模拟比较器/管线吞掉错误结果） | `NEGATIVE CONTROL PASSED`，退出码 3 |

报告侧由 `tools/weekly/test_report.py` 的 `CensusTest` 覆盖（CI 每次运行）：普查不完整、负对照通过或
未执行、缺普查、未知层名或列名均为 RED；分母变小或覆盖下降为 REGRESSED。
