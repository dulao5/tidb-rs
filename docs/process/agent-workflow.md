# 开发流程：负责人、Maya、Lucky、Duet 的分工

状态：2026-11-24 起生效（负责人在 Raft 上确认）。规则以 [CONSTITUTION.md](../../CONSTITUTION.md) 为准；
agent 的日常工作规则在 [CLAUDE.md](../../CLAUDE.md)。本文件只记录"谁做什么、按什么顺序"。

## 1. 角色

| 角色 | GitHub 身份 | 负责 |
|---|---|---|
| 负责人（zhigang-du） | `dulao5` | 方向和范围；确认 spec；裁定与 Go 的差异（`docs/testing/rulings.toml`）；预算 |
| Maya（主 agent） | `du-maya[bot]` | 规划和拆任务；开 `duet-ready` issue；用子 agent 做集成、性能、跨模块的工作；**合并 PR**；合并后跑端到端对比（`tools/ab`） |
| Lucky（评审 agent） | `du-lucky-agent[bot]` | 评审所有 PR（包括 Maya 和 Duet 的），在 GitHub 上 Approve 或 Request changes |
| Duet（自主 agent） | `du-duet` | 认领 `duet-ready` issue，按 CLAUDE.md §5 实现、开 PR、按评审意见修改 |

Maya 的子 agent 用 Maya 的身份提交。模型分工（2026-11-24 试点）：调查、设计、评审用 Opus；按已确认的 spec 和清单实现可以用 Sonnet；等 CI、汇总日志用 Haiku。

## 2. 一个任务的流程

1. **开任务**：Maya 按 [ROADMAP](../../specs/ROADMAP.md) 和依赖关系开 issue。适合 Duet 的（边界清楚、靠 govec / CI 就能验收、不需要本地集群）用 `duet-ready` 模板；其余由 Maya 的子 agent 做。
2. **实现**：实现者开 tracker PR 和 tidb-rs PR，正文写 `Closes #<issue>` 和"Not checked"一节，等 CI 通过。
3. **评审**：Lucky 在 GitHub 上评审。要改的用 Request changes；实现者在同一分支上改，Lucky 复审。
4. **批准**：Lucky 在最新的 commit 上 Approve。批准时附带"小改动"意见的，改完 push 后**由 Lucky 在新 head 上再批准一次**（只看改动部分）；`gh-events` 在已批准的 PR 又有新 push 时会 @Lucky。
5. **合并**：**由 Maya 合并**，实现者（包括 Duet）和 Lucky 都不合并。合并条件全部满足才合：
   - Lucky 对**当前 head** 的 review 是 Approved（之后又有新 push 就要复审）；成对的 tracker PR 也要在**它自己的**当前 head 上被批准；
   - CI 全部通过（push 和 pull_request 两次运行都算）；
   - **基于最新的 main**：上次 CI 之后 main 前进了，并且动到同一个 crate、桩棘轮基线（`scripts/stub_ratchet.tsv`）或 tracker 的桩登记，就要 rebase、重跑 CI，tracker 那边在最新 main 上重新扫桩（`parts.py scan`）。`merge_pair.sh` 只核对两边一致，替代不了这一步；
   - 成对的 PR 按 "tracker#N → #M" 的顺序，用 tracker 仓库的 `scripts/merge_pair.sh` 依次合并；
   - **没有待决定的事**：没有 `approver = "pending-owner"` 的裁定；依赖的 spec 没有标"待确认"；没有未关闭的 block issue；没有未解决的 Request changes；PR 的"Not checked"里每一项都有评审的回应；
   - **client-rust**：先合 client-rust 的 PR（用 merge commit，不 squash），再把 tidb-rs 固定的 client-rust 版本更新到合并后 main 上可达的 commit，重跑 CI，最后合 tidb-rs。
6. **合并后**：Maya 跑端到端对比（`tools/ab`）。发现与 Go 不一致的，开新的 `duet-ready`（或子 agent）issue，引用原 PR。

## 3. 通知

- GitHub 事件由 Maya 机器上的后台程序 `gh-events` 每 3 分钟轮询一次：新 PR、新 push、review、评论、CI 结果、`duet-blocked`。
  - 需要评审时在 Raft 私有频道 `#tidb-rs` 里 @Lucky；
  - 其他事件（包括 Lucky 的 Approve）提醒 Maya，Maya 据此合并。
- Duet 每小时看一次 issue 和自己的 PR；在 issue 里等回复时会更频繁地查看。

## 4. 预算

- Maya 每 3 小时检查一次 Claude 用量（`scripts/usage-log.sh`），按"每天 ≈ (90% − 本周已用) ÷ 剩余天数"控制子 agent。
- Duet 每周有独立的 500 美元额度，用完后会消耗主账号的周额度。Maya 没在跑子 agent 时周额度仍明显上涨（3 小时 2% 以上），就把所有 `duet-ready` 改成 `duet-paused`，等周五 22:00（JST）重置后再改回来。正在做的 `duet-claimed` 任务不受影响。

## 5. 标签

| 标签 | 含义 |
|---|---|
| `duet-ready` | 可以由 Duet 认领的任务 |
| `duet-claimed` | Duet 正在做 |
| `duet-blocked` | Duet 需要回答，问题写在 issue 里 |
| `duet-paused` | 额度原因暂停，不要认领 |
| `upstream-pitfall` / `go-gap` | Go 本身的问题（行为有错 / 缺功能），见 port-block skill |
