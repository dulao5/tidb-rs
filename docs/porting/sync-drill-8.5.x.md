# 上游同步演练：v8.5.8 → release-8.5（2026-11-13）

状态：只读演练（#128 第 4 项的一部分），没有修改 Rust 代码。目的：估算 tidb-rs 跟随上游补丁版本的成本，并据此提出同步节奏。规则依据 [CONSTITUTION.md](../../CONSTITUTION.md) 第一条（基线 v8.5.8；v8.5.8 之后上游的安全和数据正确性修复按同步流程移植，标 `go-fix`，见 [provenance.md](provenance.md)）。

## 0. 结论

- 上游还没有 v8.5.9。本次对比的是 **`release-8.5` 分支头**：`507927a8e87b`（2026-11-13）。基线 `v8.5.8` = `8b857efa2036`（2026-08-27）。窗口约 11 周，first-parent 路径上 115 个提交。
- 已移植的 4,607 个 Go 函数/符号中，有 **39 个函数**被改动（约 0.8%），改动行数 **319 行**；另有 `defaultSysVars` 的 6 个条目被改动（1 个修改、5 个新增，48 行）。这些改动来自 **15 个提交**。
- 按性质分（全部 15 个提交都已人工分类，不是抽样）：
  - **现在就该移植（宪章 1.2 的例外）**：2 组，15 个函数，237 行。分别是 `RedactURL` 的凭据泄露修复（安全）和 JSON 输出与路径解析的修复（结果错误）。
  - **升级基线时再移植**：行为变化或新功能，5 组，23 个单元。
  - **可选**：1 组小修复（SQLKiller），2 个函数。
  - **不需要移植**：表面改动或不适用，5 组，5 个单元。`NewSessionVars` 同时被常量传播那组改动，算在那一组里。
- 估算：本窗口全量同步约 **5–6 人日**，其中移植约 4 人日（现在就该移植的约 2–2.5 天），基线切换的固定开销 1–2 天。建议：**每周自动跑本演练**，安全和正确性修复随出随移；**每两个上游补丁版本（约每半年）升级一次基线**。

## 1. 版本与范围

| 项 | 值 |
|---|---|
| 基线 | tag `v8.5.8`，commit `8b857efa20363d50a8fa2ea7dd9809a85a61b115`（2026-08-27） |
| 目标 | `origin/release-8.5` 分支头，commit `507927a8e87b16ba26fcab1ec59a46fcd4c57d9f`（2026-11-13） |
| 最新 v8.5.x tag | `v8.5.8`，没有更新的 tag。`v8.5.8-2026MMDD-<sha>` 这 8 个 tag 是 hotfix 分支（如 `release-8.5-20261015-v8.5.8`）上的，不在 `release-8.5` 上，不计入 |
| 上游补丁版本间隔 | v8.5.4 2025-11-26、v8.5.5 2026-01-14、v8.5.6 04-13、v8.5.7 07-07、v8.5.8 08-27：6–13 周一次 |
| 已移植集合 | tidb-rs `main`（`849e748`）中 Rust 代码的 `// go:` 和 `// go-part:` 标注，按 `scripts/converge.py` 的规则解析到 tracker 的 `func_blocks.tsv`：4,607 个符号，分布在 212 个 Go 文件中。spike、poc crate 和 tests、benches 不计入；`go-std` 和 `go-dep` 不属于 TiDB 同步 |
| 单元 | 顶层 func，符号写法与 tracker 相同（`Func`、`Type.Method`）。`defaultSysVars` 这类已移植闭包的包级 var 按字面量元素计（`defaultSysVars{TiDBMaxKeysRead}`） |

## 2. 数字

### 2.1 总量

| 指标 | 数值 |
|---|--:|
| first-parent 提交（v8.5.8..release-8.5） | 115 |
| 全仓 diff | 793 个文件，+61,007 / −30,200 |
| 改动了已移植文件的提交 | 29 |
| 改动了已移植单元的提交 | 15 |
| 已移植文件的 diff（不含测试） | 29 个文件，+908 / −207 |
| 其中已移植函数的改动 | 39 个函数，319 行（diff 中 + 和 − 的行数之和） |
| 其中 `defaultSysVars` 条目的改动 | 6 个（1 个修改、5 个新增），48 行 |
| 同文件中未移植函数的改动（不计入成本） | 58 个单元 |
| 同文件中函数以外的声明改动（结构体字段、常量） | −23 / +147 行，随对应函数一起处理 |
| 已移植包中的测试文件 | 25 个，+1,783 / −37 |
| `tests/integrationtest/r/*.result` | 37 个文件，+2,350 / −2,629（影响 A/B 和集成测试基线） |

39 个函数按改动方式分：语义改动 26 个、只改名 8 个、改名并改语义 2 个、删除 2 个、只改注释 1 个。工具能自动认出只改注释和只改名的情况；`gofmt` 打印结果不同但语义等价的改动（例如 Go 1.27 vet 要求的 `WriteString(fmt.Sprintf(...))` → `fmt.Fprintf(...)`），靠下表的人工分类认出。

### 2.2 按块

| 块 | 已移植 | 改动的单元 | 改动行 | 主要来源 |
|---|--:|--:|--:|---|
| D01.5 JSON | 128 | 14 | 141 | JSON 修复 |
| D08.2 函数框架（常量传播） | 107 | 10 | 38 | 常量传播（10 个中有 8 个只是改名） |
| D02.1 parser AST | 1 | 1 | 96 | RedactURL |
| D05.2 会话变量 | 205 | 5 | 17 | 移除 Cascades、stmt summary、join key |
| sysvar 条目（D05.1 / D05.2） | — | 6 | 48 | 新增 5 个 sysvar，修改 1 个 |
| D00.3 util（sqlkiller） | 84 | 2 | 9 | SQLKiller 竞态 |
| D00.2、D01.3、D05.1、D07.0、D08.8、D13.0、D15.1 | — | 各 1 | 共 18 | 见 §3 |

### 2.3 按提交分类

| 提交 | 内容 | 单元 | 行 | 分类 | 处理 | 估时 |
|---|---|--:|--:|---|---|--:|
| `4f61995df8`、`84040e1f0a` | `RedactURL`：去掉 userinfo，查询参数改为白名单，增加 oss/gcs，azure 的 `endpoint` 打码 | 1 | 96 | **安全**（凭据泄露） | 现在移植，`go-fix`。Rust 侧用在 `tidb_cloud_storage_uri` 的 GetGlobal | 4h |
| `02ca651b03` | JSON：浮点输出（`4` → `4.0`、科学计数法阈值、指数去掉 `+`），`\b` `\f` 转义，路径按 rune 解析，非 ASCII 标识符，非法 UTF-8 报错 | 14 | 141 | **正确性**（结果与 MySQL 不同，关闭 9 个 issue） | 现在移植，`go-fix`；会和 v8.5.8 的 A/B 结果不一致，需要登记 A/B 例外（见 §4） | 12–16h |
| `05c7dd7b96` | `SQLKiller.Reset` 改用 swap，避免与 `SendKillSignal` 竞争 | 2 | 9 | bug 修复（并发） | Rust 版不会 panic（nil 错误已处理），但 load/store 之间仍会丢失 kill 信号；可以顺手移植 | 1h |
| `a249b02201` | 移除 Cascades 规划器：`tidb_enable_cascades_planner` 变为只告警的空操作，删除 `USE_CASCADES` 的处理和两个 getter/setter | 4 + 1 sysvar | 31 | 行为变化 | 升级基线时 | 3h |
| `dc73a29396`、`55ba9a1a04` | 常量传播：新 sysvar `tidb_opt_always_keep_join_key`（默认关），`solve` 增加 keepJoinKey 参数；`propOuterJoinConstSolver` → `propSpecialJoinConstSolver`，覆盖 anti semi join；新增 3 个函数（54 行） | 12 | 44 | 行为变化（计划变化） | 升级基线时，与规划器调用方一起移植 | 6h |
| `1551c197c4` | 新 sysvar `tidb_max_keys_read`、status `tidb_keys_examined` | 2 | 15 | 新功能 | 升级基线时（限制的执行部分不在已移植范围） | 2h |
| `3a2e67d7be` | 两个 stmt summary sysvar；`DecodeSessionStates` 改写 `PrevAffectedRows` | 3 | 10 | 新功能 + 小修复 | 升级基线时 | 1.5h |
| `1c2b91e1c5` | 新 sysvar `tidb_enable_connection_event_log` | 1 | 6 | 新功能 | 升级基线时 | 1h |
| `4b0c85337b` | 升级到 Go 1.27.3：`fmt.Fprintf` 写法 | 3 | 8 | 表面（等价改写） | 不移植，只核对 | 0.5h |
| `33c7657d08` | `Time.Sub`：宽松 SQL mode 下不再打日志 | 1 | 4 | 只影响日志 | 不移植（或随基线升级） | 0.5h |
| `9e58a801fe` | `Tracker.Consume` 的函数体搬进 `consume(bs, enforce)`；新增 `ReplaceBytesUsedForCleanup` | 1 | 2 | 重构（`Consume` 行为不变） | 等移植会话清理路径时再做 | 0.5h |
| `9890f3b630` | `SetTxnResourceGroup` 的 client-go 拦截器加 1 行 | 1 | 1 | bug 修复，但不适用 | Rust 版没有这个拦截器 | 0 |
| `e152661cf1` | `NewSessionVars` 把 SQLKiller 接到 client-go | 1 | 1 | 不适用 | 无 | 0 |

注：

- 行数取自 `git diff -U0 v8.5.8 <target>` 落在单元范围内的行，包括 doc 注释。代码被搬到新函数时会少算：`Tracker.Consume` 记为 2 行，实际是 77 行的函数体搬进了新的 `consume`。改名用相似度配对，按旧名统计。
- 已移植的 3 个 `go-fix`（`dfda1fd3e8`、`811a10e115`、`bc0d97a44d`）都来自 master，不在这次的 release-8.5 路径上，同步时不会冲突。

## 3. 成本模型与估算

| 改动类型 | 每个函数单元 | 说明 |
|---|--:|---|
| 只改注释、只改名、等价改写 | 0.1–0.25h | 更新标注或改名，不需要新的测试向量 |
| 小改动（≤10 行，单个函数） | 0.5–1h | 对照 diff 改写，补 1–2 个用例 |
| 新增 sysvar 或删除 sysvar | 1h / 个 | sysvar 表、hook、`SHOW VARIABLES` 的 A/B |
| 中等行为变化（多个函数联动） | 2–3h / 组 | 加上 govec 重新生成测试向量 |
| 大的正确性修复（如 JSON） | 约 1h / 函数 | 主要花在上游新增的约 200 行测试的向量化上 |
| 基线切换的固定开销 | 1–2 天 / 次 | tracker 按新 tag 重新生成（`go_funcs.tsv` 的行号会变）、govec、Go TiDB A/B 基线换版本、37 个 `.result` 文件 |

| 本窗口合计 | 估时 |
|---|--:|
| 现在就该移植（安全 + 正确性） | 16–20h |
| 可选的小修复（SQLKiller） | 1h |
| 升级基线时移植（行为变化 + 新功能） | 约 14h |
| 表面改动的核对 | 约 2h |
| 基线切换的固定开销 | 8–16h |
| **合计** | **约 41–53h（5–6 人日）** |

## 4. 建议的节奏

1. **每周**：在 weekly 工作流中运行 `tools/sync-drill/run.sh`（首次 clone 之后约 10 秒），把新增的已移植单元改动列进周报。人工判断是否属于宪章 1.2 的例外（安全、结果错误、数据损坏）。按本窗口的数据，平均每周约 1.4 个相关提交，判断约需 15 分钟。
2. **随出随移**：安全和正确性修复在一周内以 `go-fix` 移植。会改变用户可见结果的修复（如本次 JSON 的 `4` → `4.0`）同时登记 A/B 例外，否则每周和 Go v8.5.8 的对比会一直报不一致。如果不想维护例外，也可以把这类修复留到升级基线时一起移植；RedactURL 不受影响，应立即移植。
3. **升级基线**：每两个上游补丁版本（约每半年）升级一次。下一次候选是 v8.5.9 或 v8.5.10。按本窗口约 11 周、5–6 人日的量，半年一次约 2 人周。升级时把行为变化和新功能一起移植，宪章第一条的 tag 和 commit 同步更新。
4. 块越多，成本越高：本次只覆盖已移植的 4.6k 个函数。D08/D09 的规划器块移植之后，常量传播、hint、`session.go` 这类改动多的文件都会进入范围（这次同文件中未移植的改动有 58 个单元）。

## 5. 复现

```sh
git clone --filter=blob:none --no-checkout https://github.com/pingcap/tidb /tmp/tidb-up
TIDB_UP=/tmp/tidb-up TRACKER=../tidb-rs-tracker GO=~/sdk/go1.25.12/bin/go \
  tools/sync-drill/run.sh origin/release-8.5 /tmp/sync-drill
# 或指定 tag：tools/sync-drill/run.sh v8.5.9 /tmp/sync-drill-8.5.9
```

- `tools/sync-drill/ported.py`：从 Rust 标注列出已移植的 (Go 文件, 符号)，复用 `scripts/converge.py` 的解析，输出 `ported.tsv`；未能解析的标注写到 `ported.err`（本次 16 条：接口方法、`StmtHints` 结构体和 `crates/systable` 写路径中的 go-part，不计入）。
- `tools/sync-drill/funcdiff`：只依赖 Go 标准库，用 `go/ast` 切分单元。输出：
  - `units.tsv`：改动的单元，包括已移植和未移植的，含状态（changed / cosmetic / renamed / removed / added）、增删行数、相关提交；
  - `commits.tsv`：每个提交改动了哪些已移植单元；
  - `files.tsv`：每个文件中函数以外的改动行数。
  "cosmetic" 的判断方法：去掉注释后用 gofmt 打印，结果相同。
- 局限：
  - 只看 first-parent 路径和 `pkg/` 下的 TiDB 代码；
  - 匿名函数算在外层函数里；
  - `defaultSysVars` 和 `noopSysVars` 按条目统计，其他包级 var 整体统计；
  - 函数以外的声明改动只统计了行数，没有分类。
