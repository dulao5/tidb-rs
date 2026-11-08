# D01.1 Datum（含 D01.4）：第 1 部分规格

状态：第 1 部分已提 PR（2026-11-08）。第 2 部分（datum.go、field_type*.go、field_name.go、datum_eval.go）见 plan.md。

## 1. 范围与拆分

- **D01.4 并入 D01.1 一起移植**：convert.go 与 datum.go 双向引用，datum.go 几乎用到整个 convert.go。tracker 的块编号**保持不变**（D01.1、D01.4 两个 id），只是共用一组 PR；这样不改 blocks.tsv / func_blocks.tsv / layers，收敛检查仍按块统计。
- **第 1 部分**（本 PR）：
  - D01.4：convert.go（27/28，缺 `ToString`）、overflow.go 15/15、truncate.go 1/1；
  - D01.1 的叶子文件：binary_literal.go 13/13、enum.go 6/6、set.go 7/7、compare.go 6/6、helper.go 12/12、etc.go 18/20（缺 `EOFAsNil`、`InvOp2`），context.go 26/26（D01.3 已提前）；
  - errors.go 的全部错误原型，MyDecimal 迁移到 `tidb_base::Error`（D01.3 tasks 承诺不晚于 D01.4）；
  - charset 的 Encoding 框架（D02.2 的 encoding*.go，提前移植到 tidb-parser）。
- 收敛检查：D01.4 43/44；D01.1 在上述文件中 88/300（其余 212 个属于第 2 部分）。

## 2. 映射与设计

- Go `string` 参数为 `&[u8]`，可能含任意字节的结果为 `Vec<u8>`；`(T, error)` 中 T 与错误一起有意义时返回 `(T, Result<(), Error>)`（M-ERRVAL）。
- `BinaryLiteral`/`BitLiteral`/`HexLiteral` 是 `Vec<u8>` 的 newtype；`Enum`/`Set` 的名字是 `Vec<u8>`。
- Datum 的 Kind 常量先放在 `datum.rs`（无 go-file 头，第 2 部分补全）。
- `overflow(v any, tp)` 的 `v` 以 `gofmt::Arg` 传入（`%v` 格式化）。

### 2.1 MyDecimal 的错误类型

- `types::Error` 的桩类型删除，`tidb_types::Error` 即 `tidb_base::Error`。
- Go 的 MyDecimal 按指针返回哨兵错误（无分配）。为此 `tidb_base::Error` 新增共享表示：`Repr::Shared(&'static Inner)`，`Error::share(&'static self)` 返回原型本身，首次修改时才复制（`DerefMut`）；`equal` 先比较实例地址。MyDecimal 与 helper.go 的 `strToInt` 用 `share()`；哨兵比较用 `is_err(&r, &ErrX)`。
- **基准**（criterion，arm64，与静态桩相比）：错误路径 `to_int` 每个错误 +1.1ns（1000 次 1.23→2.29µs）；其余 MyDecimal 基准 −8%～+4%。若用 `clone()`：每个错误 +17ns，FromFloat64RandomBits +29%。
- `Result<T, Error>` 从 8 字节变为 16 字节（两种表示的枚举）。

## 3. 偏离（Go 行为保持，写法不同）

1. `errors.Wrap(ErrTruncated, msg)`（Enum 解析）返回 `ErrTruncated` 本身（M-ERR：包装器不移植，Go 的调用方只看 cause）。govec 记录的是 `errors.Cause(err)`。
2. `ToString(any)`、`InvOp2(any)`：依赖 Go 动态类型，随第 2 部分的 `GoValue` 一起移植。
3. `EOFAsNil`：tidb_base 中没有 `io.EOF` 错误值（M-ERR），第 2 部分如有调用方再处理。
4. `set.go` 的 `init` 建表改为 const 表；`setIndexInvertValue` 用 `!` 计算。

## 4. 上游问题（upstream pitfall 候选）

1. **`SubInt64`/`SubDuration` 的溢出检查漏判**：`SubInt64(1, MinInt64)` 中 `-b` 回绕成 MinInt64，检查不成立，`a - b` 回绕得到 `-9223372036854775807`，**不报错**（正确结果应溢出）。Rust 照 Go 用 `wrapping_sub` 复刻。`SubDuration` 同样。
2. **`ConvertFloatToInt(NaN)` 随平台不同**（M-F2I）：NaN 两个比较都不成立，落到 `int64(NaN)`：amd64 为 MinInt64，arm64 为 0，且不报错。`ConvertFloatToUint` 的负数分支 `uint64(int64(val))` 对小于 MinInt64 的值同样依赖平台。
3. **`ConvertFloatToUint(NaN)` panic**：`big.Float.SetFloat64(NaN)` panic；Rust 同样 panic。
4. **`ParseSetValue` 元素多于 64 个时越界 panic**（`setIndexValue[i]`）；Rust 同样。

## 5. 验证

- govec `datum` 领域：74,274 条（arm64 本地生成，amd64 取自 tracker CI），回放在 debug 与 release-checked 下都通过。
- Go 测试字面量：`cmd/minelits` 抽取 binary_literal/compare/convert/enum/etc/helper/overflow/set 的测试文件，784 条，登记进 `minedGoTests`（`DatumMinedTests`）。
- 每条记录都开启新排序规则（`SetNewCollationEnabledForTest(true)`），因为其他领域会切换这个全局开关。
- **反证**：`round_ties_even` 改为 `round` → 487 条不一致；NUL 截断判断改坏 → 753 条；去掉 ParseUint 的下划线检查 → 312 条。
