# tidb-rs 移植方法论与阶段计划

状态：方向已由项目负责人确认（2026-11-01），团队评审中。规则以 [CONSTITUTION.md](../../CONSTITUTION.md) 为准；本文说明背景、分层模型、流程和阶段计划。

## 0. 结论先行

- **把 Go TiDB 当作规格本身**：Rust 代码按 Go 的包、文件、函数结构对应移植，每个 Rust 函数都能追溯到一个 Go 函数和一个固定的 Go 版本。不为让测试通过而另写逻辑。
- **在 feature 之上加两层**：领域（约 15 个，按 Go 包划分）→ 功能块（约 60–100 个，每块 2k–10k 行 Go，一个块走一轮 spec → plan → tasks → 实现 → 收敛检查）。计划、进度、测试都挂在功能块上。
- **按计划移植整块，不按测试移植单个函数**：一个块排进计划时整块移植（或显式列出不移植的函数和原因）；依赖到还没排期的块时，用"边界桩"明确报"不支持"，不允许顺手把上游函数拉进来。这条规则就是用来终止"深度优先探索"的。
- **进度用两个独立指标衡量**：移植覆盖率（Go 函数中已有对应 Rust 实现的比例，由代码里的来源标注自动统计）和行为一致率（测试）。前者可控、可计划；后者允许阶段性不达标，但每个失败都要归因到某个未完成的块。
- **PoC 的做法换成"逐步替换"**：现在能跑的端到端链路保留作为回归基线，按块用忠实移植的代码替换 PoC 里自创的部分（主要是 `sql` 规划器和 `executor`），替换期间端到端不能回退。
- **spec-kit 用它的结构，不用它的"需求写作"**：宪章（移植规则）、roadmap（功能块计划）、每块的 spec/plan/tasks、analyze/converge 这些环节都适用；但我们的 spec 不是自然语言需求，而是"Go 范围清单 + 测试清单 + 边界"，内容主要由 tracker 生成。先在一个块上试用 specify CLI 再决定是否全面采用。

## 1. 现状诊断：PoC 证明了什么，暴露了什么

PoC 证明了技术路线可行：协议、解析器、TiKV 访问、schema 同步、指标都能和 Go TiDB 共存于同一集群，sysbench 只读性能达到 Go 的 1.0–1.2 倍。

但 PoC 的 SQL 层是"为跑通 sysbench 写的固定流水线"，和 Go 的结构没有对应关系：

| Rust crate | 行数 | 对应 Go | Go 行数 | 性质 |
|---|--:|---|--:|---|
| parser | 7.1k + 生成代码 | pkg/parser（parser.y 转换） | 68k | 忠实移植（语法表与 goyacc 同构） |
| mysql-protocol / server | 5.9k | pkg/server | 18.5k | 部分对照移植 |
| kv / distsql / meta / domain | 5.1k | kv, store/copr, distsql, meta, infoschema | ~70k | 部分对照，部分自写 |
| **sql**（规划） | 3.3k | planner/core + expression + ranger | ~210k | **自创**："固定流水线" |
| **executor** | 0.8k | pkg/executor | 116k | **自创** |

R1 的 83 条不一致、12,108 条"不支持"，大部分源于这两层。如果继续"测哪补哪"，这 4k 行自创代码会长成另一个和 Go 结构不同的优化器，之后每个 Go 的 bug 修复和新特性都无法对应移植。这就是要在此止住的情况。

## 2. 移植宪章

见 [CONSTITUTION.md](../../CONSTITUTION.md)（八条：Go 是规格、结构对应与 L1/L2/L3、例外、整块移植与边界桩、不支持就报错、来源标注、测试与验收、评审）。例外的清单和状态见 [specs/exceptions/](../../specs/exceptions/README.md)。

## 3. 分层模型：领域 → 功能块 → feature → 函数 → 测试

```
领域 (Domain, ~15)            例：D09 逻辑优化
 └ 功能块 (Block, ~60–100)    例：D09.3 列裁剪与投影消除（rule_column_pruning.go, rule_eliminate_projection.go …）
    └ feature (Go 文件, 1,499)  例：pkg/planner/core/rule_column_pruning.go   ← tracker 已有
       └ Go 函数 (symbols)     例：(*LogicalProjection).PruneColumns       ← features.tsv 已有
          └ 测试 (unit / integrationtest, 带权重)                            ← tracker 已有
```

- **领域**：按 Go 包前缀划分，稳定，决定大的阶段顺序。
- **功能块**：计划和交付的单位。按 Go 文件分组，规模 2k–10k 行 Go，一个 agent 在一个周期内能完成并评审完。块之间的依赖从 Go 的调用图算出来（不是拍脑袋）。
- feature、函数、测试三层 tracker 里已经有了，只需加上 `block` 字段。

### 领域初稿（按 TiDB master `4cee7f2084` 统计，非测试 Go 代码共 90 万行；P0 中按 v8.5.8 重新统计）

| 领域 | Go 包数 | Go 行数 | feature | 单元测试 | 说明 |
|---|--:|--:|--:|--:|---|
| D01 基础类型与编码 | 20 | 62.7k | 85 | 437 | types, codec, chunk, collate, charset, errno, tablecodec, rowcodec |
| D02 SQL 解析 | 14 | 67.9k | 39 | 328 | 已完成语法表移植（parser.go 28k 为生成代码），AST 动作待补齐 |
| D03 协议与连接 | 19 | 18.5k | 40 | 271 | pkg/server |
| D04 权限与认证 | 4 | 4.6k | 5 | 80 | |
| D05 会话、变量、事务上下文 | 24 | 39.5k | 61 | 576 | session, sessionctx/variable, sessiontxn, config |
| D06 元数据与 Schema | 30 | 52.2k | 91 | 441 | meta, infoschema, domain, table（只读部分） |
| D07 KV 访问与 DistSQL | 10 | 16.3k | 40 | 145 | kv, store/driver, store/copr, distsql |
| D08 表达式与内置函数 | 8 | 87.8k | 107 | 584 | expression（框架 + 按函数族分块） |
| D09 逻辑优化 | 26 | 86.7k | 145 | 757 | planbuilder、名字解析、逻辑算子、改写规则 |
| D10 物理优化与代价 | 18 | 33.9k | 95 | 149 | physicalop、ranger、cardinality、cost |
| D11 统计信息 | 32 | 26.1k | 74 | 418 | 只读需要的加载、缓存、估算部分 |
| D12 执行器 | 23 | 115.8k | 194 | 1,433 | executor 及 join/agg/sort/aggfuncs 等 |
| D13 Plan Cache、绑定、Hint | 2+ | 5.5k | 12 | 82 | |
| D14 可观测性 | 13 | 20.3k | 34 | 245 | metrics（已做）、stmtsummary、topsql、慢日志 |
| D15 资源控制 | 4 | 9.5k | 14 | 84 | memory tracker、resource group |
| X 不移植（Go TiDB 承担） | 132 | 188.5k | 269 | 2,353 | ddl、lightning、br、dxf、ttl、import、objstore 等 |
| 未归类（长尾工具包） | 174 | 65.1k | 204 | 1,782 | 主要是 util/*，随所属块引入 |

要移植的部分（D01–D15）约 65 万行 Go，其中写路径、事务、DDL 不在只读阶段内，只读阶段实际约 40–45 万行。这个规模决定了必须靠计划和并行推进，而不是靠测试驱动摸索。

## 4. 每个功能块的工作流（spec-kit 适配）

| 环节 | spec-kit 对应 | 产物 | 内容 / 关卡 |
|---|---|---|---|
| 0 宪章 | `/speckit.constitution` | `CONSTITUTION.md` | 第 2 节的规则，全项目一份 |
| 1 路线图 | spec of specs 的 roadmap | `specs/ROADMAP.md` | 领域、块、依赖、阶段、状态；由 tracker 生成骨架 |
| 2 规格 | `/speckit.specify` | `specs/<块>/spec.md` | **Go 范围清单**（文件、函数，含"不移植"项和理由）、对外接口（提供 / 消费）、边界桩清单、验收测试清单（Go 单测 + integrationtest 文件）、验收门槛 |
| 3 澄清 | `/speckit.clarify` | spec 内 | Go 行为里的不确定点、版本差异、需人决定的问题 |
| 4 设计 | `/speckit.plan` | `plan.md` | Rust 模块布局（对应 Go 文件）、类型映射、每个 L2/L3 偏离及理由、并发和内存模型 |
| 5 任务 | `/speckit.tasks` | `tasks.md` | 通常一个 Go 文件一个任务，标注可并行 |
| 6 一致性检查 | `/speckit.analyze` | 报告 | spec 范围 ⇔ tasks 全覆盖 |
| 7 实现 | `/speckit.implement` | 代码 + 测试 | 带来源标注 |
| 8 收敛检查 | `/speckit.converge` | 追加任务 | **机器检查**：范围内 Go 函数 100% 有标注或已登记"不移植"；块内测试结果；端到端无回退 |
| 9 评审合并 | — | PR | Go-Rust 并排评审；合并后 tracker 更新状态 |

和 spec-kit 的差别：spec.md 的主体由 tracker 生成（Go 文件和函数清单、测试清单），人写的部分只有范围取舍、边界和验收门槛。这样 spec 本身就锚定在 Go 代码上，避免用自然语言重新描述需求带来的偏差。

## 5. 一致性保障机制

1. **来源标注 + 覆盖率**：tracker 新增 Go 函数清单（基线版本上抽取）和 Rust 标注抽取，按块、按领域给出"已移植 / 已登记不移植 / 未开始"。
2. **边界桩登记**：每个桩一行记录（调用方块、被依赖块、触发的 SQL 特征），桩被触发时返回统一错误码，R1 的"不支持"统计就能按"缺哪个块"归类，直接驱动排期。
3. **对照测试**三层（无 ORDER BY 的结果按集合比较）：
   - 函数级：纯函数（类型转换、内置函数、range 计算、代价公式）用 Go 生成输入输出向量，Rust 回放比对。
   - 计划级：同一 SQL 在 Go 和 Rust 上 `EXPLAIN` 对比（规划器移植后最有效的一致性检查，比结果对比更早发现偏差）。
   - 结果级：现有 R1 混合模式 integrationtest，加 sysbench / TPC-H 读查询。
4. **上游同步**：换基线时，对两个 tag 做 Go 的函数级 diff，通过来源标注映射到受影响的 Rust 函数，生成同步任务。这是"长期跟上游"的基础，没有结构对应和标注就做不到。

## 6. 阶段计划（只读目标）

总体用"逐步替换"：现在的端到端链路一直保持可用，作为每次合并的回归基线；新移植的模块按块接入，替换 PoC 的对应部分。

| 阶段 | 内容 | 退出条件 |
|---|---|---|
| **P0 基线与工具**（约 1 周） | 在 v8.5.8 上重抽 tracker 并建 Go 函数清单；基础设计 spec（N1、N2、N3、A1）；确定领域 / 功能块划分和依赖图；来源标注规范和抽取工具；宪章；PoC 代码逐文件定性（忠实 / Rust 化 / 替换 / 自创） | ROADMAP.md 评审通过；tracker 能显示按块的移植覆盖率（PoC 现状作为起点） |
| **P1 地基** | D01 类型与编码（含 L3 决策）、D08 表达式框架（Expression、Column、Constant、ScalarFunction、类型推导、cast、比较） | 函数级对照测试通过；PoC 的表达式求值切换到新实现 |
| **P2 元数据与会话骨架** | D06 只读部分、D05 系统变量全表（按 Go 定义移植校验逻辑，未实现的一律报错）、D03/D07 补齐并补标注 | 会话变量类不一致清零 |
| **P3 优化器**（最大） | D09 planbuilder + 逻辑规则，D10 physical + ranger + cost，D11 统计信息加载与估算（按 B1–B5 排除废弃和非默认路径，按 D1 排除 TiFlash/MPP） | 新规划器替换 PoC 规划器；`EXPLAIN` 与 Go 对比在已移植范围内一致 |
| **P4 执行器** | D12：reader 系列、projection/selection、agg、sort/topn、join、window、union 等 | 替换 PoC executor；integrationtest 一致率按块提升 |
| **P5 外围** | D08 内置函数按函数族铺开（可与 P3/P4 并行）、D13、SHOW/EXPLAIN/infoschema 内存表、D04、D14 剩余 | 只读范围的 integrationtest"不支持"项只剩登记过的 |
| **QA 阶段** | 端到端（sysbench、TPC-H/CH 读查询、随机 SQL 差分）；逐个审 TiDB repo 的 bug issue（基线之后修复的，看是否需要同步）；上游同步演练 | 由项目负责人定标准 |

说明：
- 阶段内的块可以并行（按依赖图），阶段之间有依赖但不是完全串行，例如内置函数族可以从 P1 末尾一直并行做到 P5。
- 测试通过率在 P1–P4 期间会波动，这是允许的；考核的是"移植覆盖率按计划上升"和"失败全部有归因"。
- 写路径、事务、DDL 不在本计划内，之后另行规划，方法相同。

## 7. tracker 需要的扩展

- `data/domains.tsv`、`data/blocks.tsv`：Go 包/文件 → 块 → 领域，以及块依赖。
- `data/go_funcs.tsv`：基线版本的 Go 函数清单（features.tsv 的 symbols 展开即可）。
- `data/rs_provenance.tsv`：从 tidb-rs 代码抽取的标注，带 L1/L2/L3。
- `data/stubs.tsv`：边界桩登记。
- 报表：按领域 / 块的移植覆盖率、测试归因（`blocked-by` 分布）、阶段燃尽图。

## 8. spec-kit 的采用方式

- **采用**：宪章、roadmap（spec of specs）、每块的 spec/plan/tasks 三件套、analyze 和 converge 两个检查环节、用模板 preset 定制成"移植型"spec。
- **不采用**：用自然语言从零撰写需求、让 agent 根据 spec 自由设计实现——这两点和"Go 是规格"相冲突。
- **试点**：P0 中挑一个中等块（建议 D09 的"列裁剪"或 D01 的 `types/datum`）完整走一遍 specify CLI，评估模板是否顺手、converge 能否接上 tracker 的机器检查，再决定全面使用 CLI 还是只保留文件约定。

## 9. 已确认的决定（2026-11-01）

| # | 问题 | 决定 |
|---|---|---|
| Q1 | 移植基线 | **Go TiDB v8.5.8**（commit `8b857efa2`），与运行时对照版本一致；master 的差量按"上游同步"流程跟进 |
| Q2 | 优化器移植范围 | 除例外清单中的项（B1–B5、D1）外完整按结构移植 |
| Q3 | 类型与编码 | 拆分处理：编码格式（行格式、key 编码、chunk 格式）和 collation 数据表复用 TiKV 的 `tidb_query_datatype`（L3，需 ADR 和逐字节对照测试）；值的语义（Decimal 运算、时间、类型转换、比较、warning/sql_mode）从 Go 移植；JSON 作为例外 A4 |
| Q4 | PoC 自创代码 | 逐步替换：现有链路作为回归基线，对应块移植完成后删除；期间冻结，只修崩溃和数据错误 |
| Q5 | spec-kit | 先在一个块上试点（第 8 节） |
| Q6 | 分层粒度 | 两层：领域 + 功能块（2k–10k 行 Go） |
| Q7 | R1 中已报的结果错误 | 不单独修，在对应块移植时解决；修复前在 R1 报告中标注 |
| — | 例外 | 默认用 Go 的特性定义 tidb-rs；例外单独成 spec 集合，开发前逐项确认。见 [specs/exceptions/](../../specs/exceptions/README.md) |
