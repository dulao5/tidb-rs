# D01.1 Datum：交接说明（WIP）

> 2026-11-08 再更新：第 2 部分已完成并提 PR（见 spec.md 第 6–10 节）；评审后 value_expr.go 和 FindAstColName 移到 D02.2、ChangeReverseResultByUpperLowerBound 改为 X，D01.1 converge 280/280。下面是第 1 部分时的交接记录。
>
> 2026-11-08 更新：第 1 部分（D01.4 + 叶子文件 + MyDecimal 错误迁移 + Encoding）已完成，见 spec.md。分支已 rebase 到 main（D01.5 合并后），不再叠在 D01.5 上。下面"范围"表中 helper、etc（除 EOFAsNil/InvOp2）、binary_literal、enum、set、compare 已完成；第 2 部分从 datum.go 开始。

状态：进行中（2026-11-08）。分支：tidb-rs `block/D01.1-datum`（叠在 `block/D01.5-json` 上，并已合入一次 main），tracker `govec/datum-D01.1`（叠在 `govec/json-D01.5` 上）。D01.5 正在按评审重做并会 force-push；同步时用 `git rebase --onto <新的 D01.5 tip> <旧的 D01.5 tip>`，不要改 D01.5 的文件。

## 范围（func_blocks.tsv，block = D01.1，300 个函数）

| Go 文件 | 函数 | 状态 |
|---|--:|---|
| context.go | 26 | 已移植（D01.3 提前） |
| helper.go | 12 | 4 个已有；缺 RoundFloat、Round、Truncate、GetMaxFloat、TruncateFloat、TruncateFloatToString、DecimalLength2Precision、Precision2LengthNoTruncation |
| etc.go | 20 | tidb-rs #22 已提前移植 5 个（`crates/types/src/etc.rs`，分支 block/D06.1-model-b）；其余未做。合并时保留 #22 的版本、补齐其余 |
| datum.go | 146 | 未开始（已通读，设计见下） |
| field_type.go / field_type_builder.go | 22 / 20 | 未开始 |
| binary_literal.go、enum.go、set.go、compare.go、field_name.go、datum_eval.go | 13/6/7/6/5/1 | 未开始 |
| parser_driver/value_expr.go | 16 | 建议登记豁免 blocked-by D02.2（依赖 parser AST 的 ValueExpr 与 format.RestoreCtx，PoC AST 不是忠实移植） |

## 已完成

1. **charset 的 Encoding 框架**（D02.2 的 encoding*.go，提前移植到 `tidb-parser` 的 `gocharset::encoding` / `encoding_gbk`）：Datum 的字符串转换（`GetBinaryStringEncoded/Decoded`、`GetStringWithCheck`、`findEncoding`）离不开它。GBK 解码表由 rsgen 生成（`gbk_decode_data.rs`，tracker commit efe1214）。`encodingBase.Foreach` 以 atEOF=false、4 字节目标调用 x/text 的编解码器，逐字节复刻（包括 0x80 两字节块吞掉第二字节的行为）。**尚无 govec 覆盖**：应在 datum 领域加 encoding 的 op（每种 Encoding × Transform 各 Op × 随机/非法字节）。
2. `errors.go` 剩余的错误原型（go_errors）。
3. `tidb_base::gostd::math::pow10`（Go `math.Pow10` 的精确表）。
4. **encoding/json 一致性**（协调者追加的范围）：已拆成独立 PR（tidb-rs 分支 `json/go-encoding-json-errors`、tracker #17），D01.1 分支上的同样提交在该 PR 合并后 rebase 时会变成空提交。

## 设计决定（datum.go）

- `Datum { k: u8, decimal: u16, length: u32, i: i64, collation: String, b: Option<Vec<u8>>, x: X }`，`X = None | Decimal(MyDecimal) | Time(Time) | Interface(Arc<dyn Any + Send + Sync>)`。`b` 用 `Option` 区分 Go 的 nil 与空切片（`GetBytes`、`Copy`、`MarshalJSON` 的 omitempty 依赖它）。Go 的 hack.Slice 零拷贝在 Rust 中变为拥有所有权（复制），按 M-POOL 的思路接受。
- `GetValue() any` / `SetValue(any)` 用一个 `GoValue` 枚举表示 Go 的动态类型（nil、bool、int、int64、uint64、float32、float64、string、[]byte、*MyDecimal、Duration、Enum、BinaryLiteral、BitLiteral、HexLiteral、Set、BinaryJSON、Time、VectorFloat32、其他）。`String()`、`TruncatedStringify` 的 `%v` 通过 gofmt 的 `Arg::Stringer`。
- D01.4（convert.go、overflow.go、truncate.go，共 44 个函数）与 D01.1 互相引用（37/67 处）。datum.go 直接用到 22 个，传递依赖几乎是整个 convert.go。**建议本块一并移植 D01.4**，在 spec 中写明（宪章 §4.5，同 crate 提前移植），D01.4 只剩收尾。
- Go 字符串参数一律 `&[u8]`（同 D01.3 §2.2）；排序规则名、字符集名用 `&str`。
- 依赖的已有 API：time.rs（`parse_time`、`parse_datetime_str`、`parse_duration`、`Time::convert/round_frac/to_number/string/compare`、`Duration::*`、`adjust_year_checked`、`from_date`）、mydecimal.rs（`remove_leading_zeros` 需改为 `pub(crate)`）、json（`create_binary_json`、`parse_binary_json_from_string`、`compare_binary_json`、`BinaryJSON::{unquote,string,is_zero,get_*}`）、vector.rs。

## 未做 / 下一步

1. MyDecimal 的错误类型迁移到 `tidb_base::Error`（tasks 承诺不晚于 D01.4）：未做。errors.rs 的注释说需要单独的 benchmark 评审（热路径上从常量变为装箱错误）。
2. 依次移植：binary_literal、enum/set、compare、helper 剩余、etc 剩余、convert/overflow/truncate（D01.4）、field_type_builder、field_name（`FindAstColName` 依赖 ast.ColumnName，可能也要 blocked-by D02.2）、datum.go、field_type.go、datum_eval.go。
3. govec `datum` 领域（rsHeader 已按 "datum" 生成 gbk_decode_data.rs 的头部）：用 minelits 抽取 datum_test.go、convert_test.go、field_type_test.go、binary_literal_test.go、enum_test.go、set_test.go、compare_test.go、etc_test.go、helper_test.go 的字面量（登记进 `minedGoTests`）；无法用向量表达的断言移植为 Rust 单测。
4. converge、check_layers、CI 全量；PR 草稿（叠在 D01.5 上时为 draft）。
