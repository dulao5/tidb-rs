# 来源标注规范

状态：草案，在 spec-kit 试点块上验证后定稿。规则依据 [宪章](../../CONSTITUTION.md) 第六条。

目的：让每段 Rust 代码都能追溯到 v8.5.8 的 Go 代码，从而 (1) 计算移植覆盖率，(2) 评审时并排对照，(3) 上游同步时定位受影响的 Rust 代码。

## 语法

注释形式，一行一条，`grep` 和脚本都能直接处理。不用过程宏，避免给每个 crate 增加编译依赖。

```rust
//! go-file: pkg/planner/core/rule_column_pruning.go
//! go-file: pkg/planner/core/operator/logicalop/logical_projection.go   (一个 Rust 文件可以对应多个 Go 文件)

// go: LogicalProjection.PruneColumns L1
fn prune_columns(...) { ... }

// go: (*ProjectionExec).Next L1 [M-GO, M-CH]          (按全局映射规则替换了载体，仍是 L1)
fn ...

// go: LogicalSelection.PruneColumns L2 (closure → iterator; no semantic change)
fn ...

// go: pkg/expression/util.go ExtractColumns L1          (Go 文件不在 go-file 列表里时，写全路径)
fn ...

// go-part: LogicalProjection.PruneColumns               (Go 函数被拆成多个 Rust 函数时，辅助函数这样标)
fn ...

// go-l3: ADR-0007 tidb_query_datatype::codec::row::v2   (L3 同源替换，指向 ADR)
// go-exception: EXC-A4 types.BinaryJSON.Unquote           (被例外 spec 取代的 Go 函数)
// go-fix: pingcap/tidb@<commit> <issue>                   (移植了 v8.5.8 之后上游对安全或数据正确性问题的修复，宪章第一条 2)
// glue: <理由>                                             (Go 中没有对应物的胶水代码)
```

- **符号写法**与 tracker 的 `go_funcs.tsv` 一致：方法写 `Type.Method`（不区分指针接收者），函数写函数名。匿名函数归入外层函数。
- **等级**：`L1` 可省略；`L1` 后可以跟方括号里的映射规则 ID（见 [mappings.md](mappings.md)），只写 ID，不写理由。`L2` 必须在括号里写一句理由。
- 多个 Go 函数合并成一个 Rust 函数时，写多行 `// go:`。

## 校验（tidb-rs CI 中的 lint）

1. 语法合法。
2. 每个符号在 `go_funcs.tsv`（v8.5.8）中存在。
3. 方括号里的映射规则 ID 在 mappings.md 中存在。
4. `go-l3` 引用的 ADR 存在且状态为 Accepted；`go-exception` 引用的例外 spec 存在且状态为"已确认"。
5. 非测试、非生成的 Rust 函数必须有上述某一种标注；首阶段只报告，不阻塞 CI。PoC 代码在被替换之前统一标为 `glue: poc`。

## tracker 侧

- 抽取结果写入 `data/rs_provenance.tsv`：`rs_file, rs_line, rs_symbol, kind (go|go-part|go-l3|go-exception|glue), go_file, go_symbol, level, note`。
- 与 `go_funcs.tsv` 和 `blocks.tsv` 关联，给出每个函数、文件、功能块、领域的状态：已移植（L1/L2/L3）/ 例外 / 已登记不移植 / 未开始。覆盖率按 Go 代码**行数**加权，函数个数作为辅助指标。
- 上游同步：对两个 Go tag 做函数级 diff，标出"Go 侧变化、Rust 侧有对应实现"的函数，生成同步任务。标注以"Go 文件 + 符号"为键，上游会改名和挪文件（如 master 上 `pkg/parser/model` 已移到 `pkg/parser/ast`），所以 tracker 维护一张 `go_renames.tsv`（旧文件/符号 → 新文件/符号），同步时先按它换算。
- 并排评审：工具根据标注取出 Go 原文和 Rust 实现，生成并排对照，供 PR 评审使用（宪章第八条，P0 交付）。

## 待试点验证

- 标注的维护成本是否可接受（目标：评审时一眼能看出漏标）。
- Go 方法在 Rust 里变成 trait 实现时，符号对应是否足够清楚。
- 泛型 / 代码生成文件（如 `*_generated.go`）是对应生成器还是对应生成结果。初步倾向：对应生成器，生成结果整体标一个 `go-file`。
