---
name: find-next-block
description: Choose the next block (or part of a block) of Go TiDB to port in tidb-rs whose dependencies are already satisfied, using specs/ROADMAP.md, the active task list and the tracker's block / stub data. Use when asked "what should be ported next", to plan parallel porting work, or before starting port-block without a given block. 选择下一个依赖已满足的块或部分时使用。
---

# find-next-block：选下一个可以开工的块

只读，不改任何文件。输出是一份候选清单和推荐，交给负责人或 `port-block`。

## 数据来源

| 内容 | 位置 |
|---|---|
| 块、阶段、依赖结论 | `specs/ROADMAP.md`（第 3、4 节） |
| 当前的任务清单（优先于 ROADMAP 的阶段顺序） | `specs/VS1-vertical-slice/tasks.md`、`plan.md` §5（或负责人指定的当前切片） |
| 块定义、函数 → 块 | tracker `data/blocks.tsv`、`data/func_blocks.tsv` |
| 块之间的引用（`from` 依赖 `to`，`refs` 为引用次数） | tracker `data/block_edges.tsv` |
| 已完成的块、部分的状态 | tracker `data/blocks_done.tsv`、`data/block_parts.tsv`、`reports/parts.md` |
| 现有的边界桩（谁在等哪个块） | tracker `data/stubs.tsv`（`block` 列 = 被等待的块） |
| 正在进行的工作 | `gh pr list -R dulao5/tidb-rs --state open`、tracker 同样；tasks.md 的"进行中 / 评审中" |

先 `git fetch` 两个仓库，用 `origin/main` 的数据。

## 步骤

1. **范围**：只考虑当前切片 tasks.md 中"未开始"的任务行；没有当前切片时，用 ROADMAP 中当前阶段已排期的块（宪章第四条 1：只移植已排期的块）。
2. **依赖是否满足**：对每个候选，
   - tasks.md 的"依赖"列中的任务都已完成（状态为完成，或 PR 已合并）；
   - `block_edges.tsv` 中它引用的块（`from = 候选`）要么在 `blocks_done.tsv` 中已完成，要么有对应的部分已完成，要么可以写成边界桩（不在本切片关键路径上的依赖可以先用桩）；
   - 依赖的基础设计 spec（N 系列、A1、A4）和映射规则已确认，相关的例外 spec 已确认（宪章第三条 3）。
3. **不冲突**：不选已有开放 PR 或已有人认领的块；两个并行的任务不应改同一批文件（看它们的 Go 文件对应的 Rust 文件）。同一个 Go 类型只能由一个任务翻译（一型一译，见 `port-block`）；两个候选都要定义同一个类型时，串行做，或者先单独做一个只放共享类型的小 PR。
4. **排序**：
   1. 当前切片的关键路径（tasks.md 的依赖链、plan 的阶段目标日期）；
   2. 解除的桩多：`stubs.tsv` 中 `block = 候选` 的行数，以及 integrationtest 中因它 `blocked` 的语句数（tracker `reports/integration.md`）；
   3. 规模适中：ROADMAP 中的 Go 函数体行数；太大的块按部分拆（`block_parts.tsv`，converge 用 `--part`）。
5. **输出**：表格列出 3–5 个候选：块 / 部分、对应任务、依赖状态（逐个列出）、会解除的桩数、Go 行数、建议并行的组合和理由。指出需要负责人先做的决定（未确认的 spec、需要拆分的块）。

## 注意

- 不要为了"先能跑"去移植未排期块中的单个函数；用边界桩（宪章第四条 3）。没有出边的小函数例外（第四条 5）。
- 读 Go 代码时用参照树：`$(scripts/tidb-reference.sh)/pkg/...`。
