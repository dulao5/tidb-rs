# 偏差台账规格

来源：#262（负责人 2026-11-20 采纳 pgrust 经验报告第 ③ 条）。台账：`docs/testing/rulings.toml`。
检查：`scripts/check_rulings.py`。比较器侧：`tools/ab/src/rulings.rs`。

## 1. 什么进台账，什么不进

台账里的每一条是**裁定**：一处经过决定、用户看得见的 tidb-rs 与 Go TiDB v8.5.8 的差异，
写明差在哪里、证据、谁批准、什么情况下重新裁定。

| 情况 | 去处 |
|---|---|
| 有意的偏离（`docs/compatibility.md` 的 go-fix、Deliberate deviations、Observability） | 台账（divergence） |
| 例外 spec 中已确认、用户可见的差异 | 台账（divergence），spec 写明理由 |
| 只差外观、不影响语义的差异（例如提示文字），维护者可以批准 | 台账（cosmetic），可设过期日期 |
| 照 Go 移植的已知缺口（`go-gap` 标签），行为与 Go 相同 | 台账（go-gap），记录决定和重开条件 |
| tidb-rs 自己的缺陷 | **不是裁定**：开 issue（`bug`），integrationtest 中的语句列进 `tools/weekly/integrationtest-expected.json` |
| 尚未移植（1235 `blocked-by:`、原型固定文案） | 不是裁定：容差 T1 记为 unsupported |
| 例外与 X 类的"不支持"围栏 | 围栏清单 `specs/exceptions/fences.md`（五段式，#262 的第二部分）；围栏的 1235 回答由 T1 计为 unsupported |

**容差与裁定的关系。** 容差（`tolerances.toml`，T1…）是比较规则：对所有语句生效，写在比较器代码里。
裁定（R1…）是数据：只覆盖匹配规则选中的语句，不改变比较规则，而且被覆盖的语句仍然显示为
`ruled:<id>`，不算一致。两者共用字段习惯（id / name / 理由 / 日期），但不合并：合并后，一条
容差就能整类放宽比较，裁定的"逐条、可审计"就保不住了。

## 2. 字段

见 `rulings.toml` 文件头。要点：

- `class`：`divergence` 须负责人（`zhigang-du`）批准；`cosmetic` 维护者（`maintainer`）可批，只有它能设 `expires`；
  `go-gap` 记录"照 Go"的决定。
- `approver = "pending-owner"`：等负责人批准。**待批准的条目不放行任何东西**：被它覆盖的语句
  照常是 mismatch（kind 后注明 `pending ruling <id>`），命中照常计数。
- `plane`：比较器能报告的 rows / order / columns / metadata / error / warnings / affected_rows /
  shape / b_error / explain（EXPLAIN、DESC 语句的 rows 等），以及比较器看不到的 session /
  protocol / tool / observability / behaviour。
- `layer`：`integrationtest` / `workload` 时比较器按 `unit`（测试文件或 workload 模板名的正则）、
  `stmt`（SQL 文本的正则）、`detail`（差异描述的正则，可选）和 `plane` 匹配，第一条匹配的生效。
  `none`：没有测试层运行到这一差异，不匹配、不做命中审计；有钉住它的测试时写在 `test`。
- `example`：可匹配的条目必须给出一条它覆盖的语句（integrationtest：固定版本测试集的 `test:line`）。
- `reopen`：重开条件。id 不复用：删掉的条目的 id 不再分配。

文件只用 TOML 的一个子集（表、单行字符串、整数、注释），比较器用小解析器读，检查脚本拒绝其他写法。

## 3. 比较器怎么用

`ab`（integrationtest 与 workload 两层）在比较器判为不一致之后查台账，比较器本身（`compare.rs`）不变：

- integrationtest：kind 为 rows / order / columns / metadata / error / warnings / affected_rows / shape
  的 mismatch 和 `b_error`（plane `b_error`，detail 是 B 的错误消息）；
- workload：mismatch 的 kind 按同一规则从差异计算（`compare::mismatch_kind`）。

命中已批准的条目：判定为 `ruled`（`Verdict::Ruled`、integrationtest 的 `StmtOutcome::Ruled`），
kind 为 `ruled:<id>`；仍写进 mismatches.jsonl（带差异）、results.jsonl、census（不算覆盖）、
files.tsv 的 `ruled` 列，汇总行单独计数。`ab` 的退出码只看 mismatch，所以 ruled 不会让运行失败，
但也不会被算成一致。

每次运行在 `--out-dir` 写 `rulings-hits.tsv`：每条可匹配的条目一行（id、状态、layer、命中数，
0 也写）。台账默认编译进 `ab`（构建时的版本），`ab --rulings <file>` 可以换一份。

## 4. 检查

| 检查 | 在哪里 | 什么时候红 |
|---|---|---|
| schema | CI（check job）：`scripts/check_rulings.py`、`scripts/test_check_rulings.py`；`cargo test -p tidb-ab` 再用比较器的解析器读一遍 | 缺字段、取值不合法、非 cosmetic 带 `expires` 或由 maintainer 批准、待批准条目带日期、正则不能编译、`test` 指向的函数不存在、超出 TOML 子集 |
| 匹配规则能命中 | CI（reference tree job）：`check_rulings.py --tests <固定版本>/tests/integrationtest/t` | 条目的 `example` 不存在，或它自己的规则匹配不到 `example`（"不可能命中"的条目） |
| 命中审计 | weekly（完整运行，含 integrationtest）：`report.py --rulings-history` | 一条可匹配的条目在最近 N 次完整运行中命中都为 0（N 为 `[audit] runs`，现在 4）：报 RED，要么差异已消失（删条目），要么规则写错了 |
| 过期 | 同上 | cosmetic 条目过了 `expires` 仍命中（或已不命中）：警告（REGRESSED，黄色，不算通过） |

命中审计只数**列出了该条目**的运行：新加的条目要等 N 次完整运行之后才会被审计。只有完整运行
（weekly 模式，integrationtest 部分跑完）写 `rulings-run.tsv` 并上传为 `weekly-rulings-hits`
（保留 90 天，所以 N 最大 12）；夜间运行不审计。

## 5. 当前内容（2026-11-21）

13 条：go-fix 5 条（R1–R5）、有意偏离 5 条（R6–R10）、可观测性 2 条（R11–R12，已决定未实现）、
go-gap 1 条（R13，#252）。都是 `layer = "none"`：现有的测试层都没有运行到这些差异。已批准 3 条
（R9 infoschema v1 不移植、R11 #115、R12 #116，负责人决定有记录）；其余 10 条 `pending-owner`。

integrationtest 最近一次完整运行（#272 分支，2026-11-20，新比较器）的 76 个 mismatch 全部是
tidb-rs 缺陷，没有一个是裁定：57 个属于已有 issue（#183 #184 #185 #187 #188 #190 #191 #192
#193 #194），12 个语法错误的列号（#278），3 个列定义字符集（#279），4 个语句不在固定版本的测试集中
（那次运行用的测试集比 v8.5.8 新；其中 3 个也是 #278 的列号问题）。新的两个 issue 的语句已列进 `integrationtest-expected.json`。

尚未定稿的例外 spec（N10、N11、N12 草案）中的用户可见差异，在负责人确认时加进台账。
