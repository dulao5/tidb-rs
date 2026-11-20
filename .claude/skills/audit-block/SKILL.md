---
name: audit-block
description: Read-only audit of a tidb-rs porting PR or block against Go TiDB v8.5.8 — machine checks (one translation per Go symbol, converge, stubs registered, Go files cited) plus a side-by-side equivalence review using the recurring review rules of port-block. Use when asked to audit / review a port, before opening a porting PR, or when checking an existing block for drift. 只读审计一个移植 PR 或块时使用。
---

# audit-block：只读审计

**只读**：不改代码、不推送、不在 PR 上评论。输出是一份报告，交给调用者。

输入：一个 PR 号、一个分支，或者一个块 ID（审计 main 上已有的代码）。

## 1. 准备

```sh
# PR：在单独的 worktree 中检出，不要动别人的 worktree
git fetch origin pull/<N>/head:audit-<N> && git worktree add ../tidb-rs-audit-<N> audit-<N>
gh pr view <N> -R dulao5/tidb-rs --json body -q .body > /tmp/pr-<N>.md
go_tree=$(scripts/tidb-reference.sh --check)     # Go 参照树，必须通过
```

tracker 侧对应的 PR（PR 正文中的"tracker#N"）同样检出。

## 2. 机器检查（逐项记录结果）

| 检查 | 命令 | 通过条件 |
|---|---|---|
| 一型一译 | `python3 scripts/check_one_translation.py --list` | 没有 `[NEW]`；PR 新增的 `// go-dup:` 都写了站得住的理由 |
| PR 引用的 Go 文件、新增的桩 | `python3 scripts/check_one_translation.py --changed origin/main --pr-body /tmp/pr-<N>.md --stubs ../tidb-rs-tracker/data/stubs.tsv`（tracker PR 已改 stubs.tsv 时用 tracker PR 分支上的那份） | 改动的移植文件对应的 Go 文件都在 PR 正文中列出；每个新增的 `blocked-by` 都已登记 |
| 桩登记与代码一致 | tracker：`python3 scripts/blocks/parts.py scan <tidb-rs worktree>` 和 `parts.py check` | 无差异；每行都有 note |
| converge | `scripts/converge.py --block <块> [--part ... --item ...] --func-blocks ../tidb-rs-tracker/data/func_blocks.tsv <文件...>` | 范围内 100%（matched + accounted），没有 malformed / unannotated |
| 分层与可变性 | `python3 scripts/check_layers.py`、`python3 scripts/check_model_mutability.py` | 通过 |
| govec | tracker CI 的 govec 任务结果；`scripts/govec.sh --check <领域>` | 向量可以重新生成；amd64 文件与 CI 产物的 sha256 相同 |
| 命名 | 读 `git diff origin/main... --stat` 和新增的标识符、测试名、文件名、环境变量 | 不含项目 ID（VS1-xx、Dxx.y、Nxx、M-xx 等），按含义命名 |

CI 已经跑的检查（clippy、fmt、test、TZ 步骤）看 CI 结果即可，不用重跑。

## 3. 人工审计：与 Go 是否等价（宪章第八条）

对每个改动的移植函数，打开参照树中的 Go 原文并排读：

1. 分支、错误码、报错文案、warning、报错的先后顺序与 Go 相同；
2. `defer`、事务结束、生命周期相关的顺序与 Go 相同；
3. Go 会走、而我们走不了的分支，在同一位置是显式的 `blocked-by`，没有静默的默认值；
4. 承载方式符合 mappings.md 的规则，`L1 [M-xx]` 引用的规则确实适用；L2 写了理由；
5. 共享元数据只经 `make_mut` 修改；没有跨 `.await` 持锁；跨连接的锁不会毒化；持有 worker 的结构有 `Drop`；
6. 与 Go 的差异都在 spec 的"已知差异"一节或文档注释中登记；spec 中关于 Go 的陈述与源码一致；
7. govec 覆盖了每个移植的函数，豁免属实，`blocked` 条数有固定的断言，会读时区的回放在非 UTC 下也跑；
8. 新代码在默认模式（`--new-stack=off`）下不运行。

完整的规则清单见 `.claude/skills/port-block/SKILL.md`；对照它逐条检查。

## 4. 报告格式

```
审计：PR #N（块 / 部分），基于 <commit>
机器检查：一型一译 ✓/✗（新重复 …）；Go 文件引用 …；桩 …；converge a/b；分层 ✓；govec …；命名 …
等价性问题（必须改）：
  1. <Rust 文件:行> vs <Go 文件:行>：<差异>，<影响>
建议（可以不改）：…
未登记的差异 / 可能的上游 bug：…
```

每个问题给出 Rust 和 Go 两侧的位置。没有问题的检查只写一行"通过"。审计中发现新的一类反复出现的问题时，在报告末尾建议把它加入 port-block 的规则。
