# D01.2 MyDecimal：块 spec（流程试点）

状态：草案（2026-11-04）。阶段：P1（ROADMAP 中排在 L3；作为 P0 的流程试点提前做）。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[mappings.md](../../docs/porting/mappings.md)（若这两份尚未合入 main，以 PR #1 为准）。

试点的目的不只是移植这个块，还要验证整条流程：spec → plan → tasks → 实现（带来源标注）→ govec 回放 → converge 检查 → 并排评审。流程上的发现记在 [pilot-notes.md](pilot-notes.md)。

## 1. Go 范围

文件：`pkg/types/mydecimal.go`（2,442 行，58 个函数，函数体 2,004 行）。函数清单由 tracker `data/func_blocks.tsv`（block = D01.2）生成：

| Go 函数 | 行 | 函数体行数 |
|---|---|--:|
| `zeroMyDecimalWithFrac` | 117–122 | 6 |
| `add` | 125–134 | 10 |
| `add2` | 139–153 | 15 |
| `sub` | 156–165 | 10 |
| `sub2` | 169–182 | 14 |
| `fixWordCntError` | 185–193 | 9 |
| `countLeadingZeroes` | 201–208 | 8 |
| `countTrailingZeroes` | 216–223 | 8 |
| `digitsToWords` | 225–230 | 6 |
| `MyDecimal.IsNegative` | 251–253 | 3 |
| `MyDecimal.GetDigitsFrac` | 256–258 | 3 |
| `MyDecimal.GetDigitsInt` | 261–263 | 3 |
| `MyDecimal.String` | 266–271 | 6 |
| `MyDecimal.stringSize` | 273–276 | 4 |
| `MyDecimal.removeLeadingZeros` | 278–292 | 15 |
| `MyDecimal.removeTrailingZeros` | 294–309 | 16 |
| `MyDecimal.ToString` | 317–392 | 76 |
| `MyDecimal.FromString` | 395–534 | 140 |
| `MyDecimal.Shift` | 544–700 | 157 |
| `MyDecimal.digitBounds` | 708–749 | 42 |
| `MyDecimal.doMiniLeftShift` | 762–774 | 13 |
| `MyDecimal.doMiniRightShift` | 787–799 | 13 |
| `MyDecimal.Round` | 817–989 | 173 |
| `MyDecimal.FromInt` | 992–1001 | 10 |
| `MyDecimal.FromUint` | 1004–1021 | 18 |
| `MyDecimal.ToInt` | 1024–1062 | 39 |
| `MyDecimal.ToUint` | 1065–1086 | 22 |
| `MyDecimal.FromFloat64` | 1089–1092 | 4 |
| `MyDecimal.ToFloat64` | 1095–1134 | 40 |
| `MyDecimal.ToBin` | 1214–1216 | 3 |
| `MyDecimal.WriteBin` | 1219–1336 | 118 |
| `MyDecimal.ToHashKey` | 1340–1357 | 18 |
| `MyDecimal.PrecisionAndFrac` | 1360–1368 | 9 |
| `MyDecimal.IsZero` | 1371–1380 | 10 |
| `MyDecimal.FromBin` | 1383–1486 | 104 |
| `DecimalBinSize` | 1489–1499 | 11 |
| `readWord` | 1501–1518 | 18 |
| `writeWord` | 1520–1538 | 19 |
| `MyDecimal.Compare` | 1541–1551 | 11 |
| `MyDecimal.MarshalJSON` | 1564–1572 | 9 |
| `MyDecimal.UnmarshalJSON` | 1575–1586 | 12 |
| `DecimalNeg` | 1589–1596 | 8 |
| `DecimalAdd` | 1601–1609 | 9 |
| `DecimalSub` | 1612–1620 | 9 |
| `validateArgs` | 1622–1642 | 21 |
| `doSub` | 1644–1806 | 163 |
| `doAdd` | 1808–1912 | 105 |
| `maxDecimal` | 1914–1941 | 28 |
| `DecimalMul` | 1962–2083 | 122 |
| `DecimalDiv` | 2091–2095 | 5 |
| `DecimalMod` | 2121–2125 | 5 |
| `doDivMod` | 2127–2380 | 254 |
| `DecimalPeak` | 2383–2394 | 12 |
| `NewDecFromInt` | 2397–2399 | 3 |
| `NewDecFromUint` | 2402–2404 | 3 |
| `NewDecFromFloatForTest` | 2407–2414 | 8 |
| `NewDecFromStringForTest` | 2417–2424 | 8 |
| `NewMaxOrMinDec` | 2427–2442 | 16 |

**不移植**：无。`NewDecFromFloatForTest`、`NewDecFromStringForTest` 只给测试用，仍按 L1 移植并放在 `#[cfg(test)]` 之外（其他块的测试会用到），标注照常。

## 2. 对外接口

- **提供**：`MyDecimal` 类型及上表的公开函数和方法，`RoundMode` 常量，`DivFracIncr`、`MaxDecimalWidth` 等包级常量。`MyDecimal` 的内存布局必须与 Go 相同（`#[repr(C)]`，字段顺序 `digitsInt, digitsFrac, resultFrac, negative, wordBuf[9]`，共 40 字节），因为 chunk 的线上格式就是这个布局（N5a）。
- **消费方**（引用数，来自 `block_edges.tsv`）：D08.5 算术（114）、D01.1 Datum（67）、D08.8 CAST（54）、D08.7 时间函数（38）、D01.8 编码（22）、D01.7 Chunk（17）、D08.4 比较（16）等。

## 3. 依赖与边界桩

`mydecimal.go` 只引用了块外的这些符号（来自 tracker 引用图 `go_refs.tsv`）：

| Go 符号 | 所在文件（所属块） | 引用数 | 处理 |
|---|---|--:|---|
| `ErrOverflow`、`ErrTruncated`、`ErrBadNumber`、`ErrTruncatedWrongVal`、`ErrDivByZero` | `types/errors.go`（D01.1） | 26 | 临时边界桩（blocked-by D01.1、D00.2）：在 `types/errors.rs` 中只定义这 5 个错误，带 Go 的错误码和文案模板；D00.2（terror/errno）和 D01.1 完成时替换 |
| `terror.Log` | `parser/terror/terror.go`（D00.2） | 3 | 临时边界桩：记一条日志 |
| `mysql.MaxDecimalScale` | `parser/mysql/const.go`（D00.2） | 3 | 常量，直接定义在对应模块位置（`parser/mysql` 的常量 PoC 中已有则复用） |
| `strToInt`、`isSpace`、`isDigit` | `types/helper.go`（D01.1） | 3 | 叶子函数（没有进一步依赖）：按 L1 移植到 Rust 中**对应的文件** `types/helper.rs`，D01.1 开工时直接计为已完成。这是宪章第四条没有覆盖的情况，见 pilot-notes |

## 4. 验收

1. **函数级**：govec 向量 `vectors/v8.5.8/decimal.jsonl`（tracker PR #1，31,620 条：26 个基本操作 27,641 条，别名调用 `*_alias` 1,795 条，链式调用 `*_then_*` 2,184 条）全部回放通过，比对规则见 tracker `govec/README.md`：`s`、`str`、`err.code` 与未填充的 `err.msg`、内部状态 `st` 一致；别名调用按 `M-ALIAS` 先复制被别名的输入；Go 会 panic 的记录 Rust 也 panic（由 task 边界转换成错误，`M-PANIC`）。
2. **单测**：`TestReset`（没有用例表）翻译为 Rust 单测；其余 29 个测试已被向量覆盖，不再逐个翻译。
3. **来源标注**：58 个函数 100% 有 `// go:` 标注；converge 检查（标注 ⇔ 函数清单）通过。
4. **端到端**（方法论 §6，P1 退出条件之一）：PoC 中 `crates/executor/src/agg.rs`、`crates/distsql/src/{dag,decode}.rs` 对 TiKV `Decimal` 的使用改为本块的 `MyDecimal`，sysbench oltp_read_only 与 R1 的 decimal 相关结果不回退。这一步作为单独的任务，可以放在块合入之后。
5. **性能**：Go `mydecimal_benchmark_test.go` 的 5 个基准在 Rust 中对应实现，结果不低于 Go（初拟，先测 Go 基线）。
