# N0：crate 分层与依赖方向（基础设计 spec，草案）

状态：草案（2026-11-07）。类型：实现例外（基础设计）。计划定稿：P1 第一个块 D00.2 开工前。
所属块：全部。机器可读的分层表是仓库根目录的 [`layers.toml`](../../../layers.toml)，检查脚本是 [`scripts/check_layers.py`](../../../scripts/check_layers.py)（CI 中运行）。
相关：ROADMAP §3（Go 的块之间不存在无环顺序）、宪章第四条（整块移植与边界桩）、N1/N2/N3 的接口汇总（[foundation-interfaces.md](../foundation-interfaces.md)）、ADR 0007（TiKV crate 的使用范围）。

## 1. 要解决的问题

宪章、来源标注、映射规则和收敛检查约束的是**函数级**的写法：一个 Go 函数移植成什么样。目前没有规则约束**模块级**的结构，也就是哪个块放进哪个 crate、crate 之间谁可以依赖谁。缺了这一层，会出现三种问题：

1. **新代码没有确定的位置**。ROADMAP 只写到功能块。现有的 12 个 crate 是 PoC 时临时划分的，其中 `sql`、`executor` 是自创的执行管线（方法论 §1）。每个块开工时各自决定放在哪里，结构会越写越乱。
2. **Go 的包之间有环，Rust 的 crate 之间不能有环**。ROADMAP §3 已经算出：P1–P5 的 84 个块在函数级引用上构成一个强连通分量。Go 能编译，是因为大量循环引用发生在同一个包内（`planner/core`、`expression`），或者经过接口间接实现（`sessionctx.Context`）。Rust 的 crate 依赖图必须无环。所以每一处由下层指向上层的引用都要有明确的处理办法。不提前定，这些问题会在某个块写到一半时才暴露，最常见的结局是把两个 crate 合并，或者随手加一个 `dyn Any`。
3. **编译时间**。整个只读路径约 22 万行 Go 函数体，移植后的 Rust 代码量相近。如果都放在一两个 crate 里，增量编译会很慢。N4 已经决定每个函数族一个 crate。

## 2. 原则

1. **按 Go 的包和功能块分 crate，分层由数据决定**。层次（rank）按 tracker 的块级引用图（`data/block_edges.tsv`）排定，使绝大多数引用由上层指向下层。不按"概念上应该怎样"拍脑袋分。
2. **依赖只能指向 rank 严格更小的 crate**。同层的 crate 之间不能互相依赖。这样不需要逐对审批依赖关系，加一个 crate 只要定它的 rank。
3. **反向引用要有显式的处理办法**，只有三种（§4）：把被引用的函数或类型挪到下层；在下层定义 trait，上层实现，启动时注入；或者是边界桩。不允许用合并 crate 来"解决"环。
4. **PoC crate 是 legacy**。新代码不得依赖它们。PoC 可以依赖新 crate，以便逐步换上移植代码。每个 PoC crate 在被替换完后删除。
5. **由机器检查**。分层表是 `layers.toml`，CI 用 `cargo metadata` 检查每条依赖边。人只需要评审 `layers.toml` 的改动。

## 3. 分层表

数字是 rank（越小越底层）。"Go 包"只列主要的，精确的函数归属以 tracker 的 `func_blocks.tsv` 为准。

| rank | crate | 块 | Go 包（主要） | 状态 |
|--:|---|---|---|---|
| 0 | `tidb-base` | D00.2 | parser/mysql 常量、errno、terror、dbterror、config | 规划（P1 第一个） |
| 0 | `tidb-metrics` | D14.1 | metrics | 已有 |
| 1 | `tidb-parser` | D02.1、D02.2 | parser | 已有 |
| 1 | `tidb-memory` | D15.1 | util/memory（A1） | 规划 |
| 2 | `tidb-types` | D01.1–D01.6、D01.9 | types、util/collate、util/charset | 已有（D01.2） |
| 2 | `tidb-model` | D06.1 | meta/model | 规划 |
| 3 | `tidb-chunk` | D01.7、D01.10 | util/chunk、util/disk | 规划 |
| 3 | `tidb-kv` | D07.0 | kv | 规划（与 PoC 同名，见 §5） |
| 3 | `tidb-execdetails` | D14.0 | util/execdetails | 规划 |
| 4 | `tidb-codec` | D01.8 | util/codec、tablecodec、util/rowcodec | 规划 |
| 5 | `tidb-sessionctx` | D00.1、D05.1、D05.2 | sessionctx、stmtctx、exprctx、planctx、errctx、variable | 规划 |
| 6 | `tidb-expression` | D08.1、D08.2、D08.12、D08.11 | expression 框架、函数注册、aggregation 描述 | 规划 |
| 6 | `tidb-privilege` | D04.1 | privilege | 规划 |
| 7 | `tidb-expr-{compare,arith,string,time,cast,misc,json}` | D08.4–D08.10 | expression/builtin_* | 规划（N4：每个函数族一个 crate） |
| 7 | `tidb-infoschema` | D06.2、D06.3、D06.4、D06.6 | meta、infoschema、table/tables | 规划 |
| 8 | `tidb-observe` | D14.2、D15.2 | slowlog、stmtsummary、topsql、resourcegroup | 规划 |
| 8 | `tidb-statistics` | D11.1、D11.2 | statistics | 规划 |
| 9 | `tidb-store` | D07.1–D07.4 | store/driver、store/copr、distsql | 规划 |
| 9 | `tidb-sessiontxn` | D05.4 | sessiontxn | 规划 |
| 10 | `tidb-expr-pushdown` | D08.3 | expression 的 infer_pushdown、expr_to_pb | 规划 |
| 10 | `tidb-domain` | D06.5 | domain | 规划（与 PoC 同名） |
| 11 | `tidb-planner` | D09.*、D10.*、D13.1–D13.3 | planner、bindinfo | 规划 |
| 12 | `tidb-executor` | D12.0–D12.13 | executor | 规划（与 PoC 同名） |
| 13 | `tidb-session` | D05.3 | session | 规划（与 PoC 同名） |
| 14 | `tidb-server` | D03.1、D03.2 | server | 规划（与 PoC 同名） |

几点说明：

- **D08.3（下推判定）单独放到 rank 10**。它按函数签名列出全部下推表，引用了所有函数族（495 处）。放在表达式框架里，会让框架反过来依赖全部函数族。
- **D04.1（权限）、D14.2/D15.2（可观测、resource group）、D06.5（domain）从会话层拆出来，放到规划器之下**。执行器和规划器大量引用它们（SHOW、内存表读取 D12.12/D12.13 引用 domain 115 处、权限 199 处），放在会话层会产生约 600 处反向引用。
- **规划器暂时是一个 crate**。Go 的 `planner/core` 本身就是一个包，内部循环引用很多。将来如果编译时间超标，再按 Go 子包（cardinality、cost、rule 等）拆分；拆分规则相同，改 `layers.toml` 即可。
- **D00.3（`util/*`）不单独成 crate**。宪章 4.4 规定按需整文件移植：每个 util 文件放进**用到它的最底层 crate**。如果被两个同层的 crate 同时用到，就把它下沉到两者共同依赖的更低一层（通常是 `tidb-base` 或 `tidb-types`），不建 `util` 这种大杂烩 crate。
- 不移植的块（X、D12.14、D09.17、EXC.B 等）不出现在表里，对它们的引用按宪章第四条做成边界桩。

## 4. 反向引用的处理

用上表计算 tracker 的块级引用（`scripts/check_layers.py --block-edges ../tidb-rs-tracker/data/block_edges.tsv`）：两端都已排定的跨 crate 引用共 60,375 处。其中 **60,063 处（99.48%）由上层指向下层**，剩下 312 处是反向或同层引用。按引用数排列的前几位，以及抽查函数级引用后得出的处理办法：

| 引用数 | 方向 | 实际引用的是 | 处理 |
|--:|---|---|---|
| 46 | statistics → planner | `planner/util/debugtrace`（优化器调试跟踪） | **下沉**：debugtrace 是工具代码，归到 `tidb-sessionctx`（它挂在 StmtCtx 上） |
| 30 | expression → expr-cast | `WrapWithCastAs*`、`BuildCastFunction` | **下沉**：这些是框架用的构造函数，归 D08.1；CAST 的求值函数体留在 `tidb-expr-cast` |
| 24 | store → planner | `ranger.Range` 类型 | **下沉**：ROADMAP 已定，类型定义随 D07.3 移植 |
| 18 | expression → expr-pushdown | aggregation 描述的 `ToPB` | **下沉或上移**：把 `AggFuncToPBExpr` 等函数归 D08.3 |
| 17 | sessiontxn → planner | 对计划类型做判断（`Execute`、`PhysicalLock`、点查） | **接口反转**：在 `tidb-sessiontxn` 定义它需要的那几个问题（如"是否点查""是否加锁"），作为 trait，由规划器的计划类型实现 |
| 17 | domain → planner | `bindinfo.GlobalBindingHandle` | **接口反转**：domain 持有 `Arc<dyn BindingHandle>`，trait 定义在 `tidb-domain`，实现在 `tidb-planner`，由 `tidb-server` 启动时注入 |
| 16 | sessionctx → expression | 上下文里出现 `Schema`、`CorrelatedColumn` 等表达式类型 | **接口反转**：按 N2，上下文是结构体；这些字段只有规划器用，移到规划器侧的上下文（`PlanCtx`）里 |
| 14 | executor → session | `session/txninfo`（事务信息结构） | **下沉**：纯数据结构，归 `tidb-sessiontxn` |
| 10 | privilege → planner | `resolve.ResultField` | **下沉**：归 `tidb-parser` 旁的名字解析类型，或 `tidb-model` |
| 8 | expression → expr-misc | `grouping`、`values` 的函数类 | **下沉**：框架直接引用的函数类归 D08.1 |

规则：

1. **下沉**（首选）：被引用的函数或类型其实是基础设施，只是 tracker 的块划分把它归到了上层。处理是在 tracker 里改这个函数的归属（`rules.py`），重新生成 `func_blocks.tsv`。这样它跟着下层块移植，不需要任何额外代码。
2. **接口反转**：被引用的确实是上层的业务逻辑。在**引用方**所在的 crate 定义 trait，只包含它真正用到的方法；上层实现；由上层通过已有的上下文对象注入（`Session`、`Domain`、`ExecCtx`，或启动时的 `tidb-server`）。Go 里本来就是接口的（如 `sessionctx.Context`），按原接口拆分；Go 里是直接调用的，trait 方法与 Go 函数一一对应，来源标注照常写在实现处。
3. **全局注册表**（Go 的包级 `map` 变量，如 `expression.funcs`、执行器构建分发）：用 `OnceLock<&'static dyn Trait>` 或 `OnceLock<Registry>`，由 `tidb-server` 的启动代码（以及测试的公共初始化函数）**显式**调用每个函数族 crate 的 `register(&mut Registry)` 填充，填完后不可变。**不使用** `inventory`、`linkme`、`ctor` 这类依赖链接器的自动注册：它们会让"哪些函数被注册了"取决于链接了哪些 crate，出错时没有编译期提示。
4. **边界桩**：引用的是还没移植或不移植的块，按宪章第四条处理，不属于本 spec。
5. **不允许**：为了消除环而合并两个 crate；用 `dyn Any` 加向下转型绕过依赖；用 feature flag 让下层在某些配置下依赖上层。

每个块的 plan 中列出本块涉及的反向引用和各自的处理办法（plan 模板新增一节）。`--block-edges` 的输出就是清单的来源。

## 5. PoC crate 的处理

1. `layers.toml` 的 `[legacy]` 列出全部 PoC crate：`tidb-server`、`tidb-mysql-protocol`、`tidb-kv`、`tidb-meta`、`tidb-distsql`、`tidb-sql`、`tidb-executor`、`tidb-session`、`tidb-domain`、`tidb-query-smoke`、`tidb-ab`。`tidb-parser`、`tidb-metrics`、`tidb-types` 是正式 crate。
2. **改名**：PoC 中有 5 个 crate 与规划的正式 crate 同名（`tidb-kv`、`tidb-domain`、`tidb-executor`、`tidb-session`、`tidb-server`）。第一个同名 crate 是 P1 的 `tidb-kv`（D07.0）。在它开工之前，用一个只做机械改名的 PR，把 PoC crate 移到 `crates/poc/` 下，包名加前缀 `poc-`（如 `poc-kv`）。不改逻辑，二进制名保持 `tidb-server`，部署和脚本不受影响。
3. **替换**：某个块移植完成后，PoC 中对应的部分改为调用新 crate（legacy 可以依赖正式 crate），再删掉 PoC 中的旧代码。PoC crate 变空后删除，同时从 `[legacy]` 中去掉。端到端测试在替换过程中不能回退（方法论 §0）。
4. **third_party**：vendored 的 TiKV crate 按路径识别。正式 crate 使用它们，必须在 `[third_party.users]` 中登记，并符合 ADR 0007（只复用字节格式）。目前只登记了 `tidb-codec` 使用 `codec`。

## 6. 检查（CI）

`scripts/check_layers.py` 读取 `layers.toml` 和 `cargo metadata --no-deps`，以下情况失败：

- 正式 crate 依赖了 rank 不小于自己的正式 crate（包括 dev 和 build 依赖）；
- 正式 crate 依赖了 legacy crate，或者依赖了未登记的 third_party crate；
- workspace 成员没有在 `layers.toml` 中登记，或者标了 `planned` 的 crate 实际已经存在；
- 同一个块被分到两个 crate。加 `--blocks <tracker>/data/blocks.tsv` 时，还检查块 ID 是否存在。

dev 依赖也受约束。测试需要上层的东西时（比如要用 SQL 做端到端测试），写在上层 crate 的测试里，或者放进 legacy 的集成测试 crate。

当前结果：3 个正式 crate、11 个 legacy crate，没有违规。验证过反例：把一个 PoC crate 改登记为 rank 1 的正式 crate 后，它对 6 个 legacy crate 的依赖都被报错。

## 7. 维护

1. **新建 crate**：在 `layers.toml` 中去掉 `planned`；如果是表外的新 crate，先在本表登记 rank 和块，和代码放在同一个 PR 里。
2. **调整 rank 或块的归属**：改 `layers.toml`，PR 中附上 `--block-edges` 前后的输出，说明反向引用的变化。
3. **tracker 的块划分调整后**（比如为了下沉而改了函数归属），重跑 `--block-edges`。
4. **ROADMAP**：每个块所在的 crate 以 `layers.toml` 为准。ROADMAP 的表格由 tracker 生成，以后生成时加一列"crate"，读取本仓库的 `layers.toml`（tracker 的后续工作）。

## 8. 验收

- [x] `layers.toml` 覆盖 tracker 中 P1–P5 和 EXC 的全部已排期块，每个块恰好属于一个 crate（脚本检查）。
- [x] CI 运行 `check_layers.py`，对现有 workspace 通过。
- [ ] PoC crate 改名（§5 第 2 条）在 D07.0 开工前完成。
- [ ] 块 plan 模板增加"反向引用与处理"一节。
- [ ] tracker 的 ROADMAP 生成增加 crate 列。

## 9. 待定项（需要项目负责人决定）

| # | 事项 | 建议 |
|---|---|---|
| 1 | 分层表（§3）作为 P1–P5 的初始方案 | 采纳。之后按 §7 调整，由评审把关 |
| 2 | 全局注册表显式注册，不用链接器自动注册（§4 第 3 条） | 采纳 |
| 3 | PoC crate 改名为 `poc-*` 并移到 `crates/poc/`（§5 第 2 条） | 采纳，在 D07.0 之前单独提 PR |

## 附：我不确定的地方

- 块级引用是按 tracker 现有的块划分统计的。§4 表里的处理办法是抽查函数级引用后得出的，没有逐条核对全部 312 处。每个块开工时在 plan 里逐条处理。
- `tidb-expression` 与函数族 crate 的边界（哪些函数是"框架用的构造函数"）要在 D08.1 开工时，按 N4 的宏和注册方式再确认一次。
- `tidb-planner` 作为一个 crate 的编译时间，到 P3 中期再测；超过阈值就按 §3 的说明拆分。
