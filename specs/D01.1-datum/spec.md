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

---

# 第 2 部分（datum.go、field_type*.go、field_name.go、datum_eval.go）

状态：已提 PR（2026-11-08）。第 1 部分已合并（tidb-rs #29、tracker #18）。

## 6. 范围

- datum.go 146/146、field_type.go 22/22、field_type_builder.go 20/20、field_name.go 4/5、datum_eval.go 1/1、etc.go `InvOp2`；D01.4 的 convert.go `ToString`（D01.4 达到 44/44）。
- converge：D01.1 **282/299**。缺的 17 个登记为豁免，原因相同：依赖尚未忠实移植的 parser AST（blocked by D02.2）：
  - `parser_driver/value_expr.go` 全部 16 个（`ValueExpr`、`ParamMarkerExpr` 是 `ast.Node`，用到 `format.RestoreCtx`）；
  - `NameSlice.FindAstColName`（参数是 `*ast.ColumnName`，PoC AST 只是替身）。
- `types.EOFAsNil` 改为 X（v8.5.8 中没有调用方，第 1 部分评审）；tracker `rules.py` 中记录了原因。
- 新增 `tidb_base::gostd::sort`：Go `sort.Sort`（pdqsort，不稳定）的逐行移植。`SortDatums` 的 `Less` 有副作用（记录第一个比较错误），排序规则也不是全序（`utf8mb4_general_ci` 下的等值），所以必须和 Go 的比较序列一致。
- 第 1 部分评审的两项后续：`tidb-base` 中 `size_of::<Result<i64, Error>>() == 16` 的编译期断言；govec `str_to_time` 覆盖零日期相关的 flag 位。

## 7. 设计（datum.go）

- `Datum { k: u8, decimal: u16, length: u32, i: i64, collation: Cow<'static, str>, b: Option<Vec<u8>>, x: DatumX }`，`DatumX = Nil | Decimal(Box<MyDecimal>) | Time(Time) | Interface(Box<Arc<dyn Any>>)`。
  - `size_of::<Datum>() == 80`，Go 为 72。`Interface` 装箱是为了让枚举只占 16 字节。
  - `EmptyDatumSize` 等内存统计常量仍报告 Go 的数值（72、`MyDecimalStructSize`、`Time` 为 8），因为统计信息和内存配额以此为准。
- Go 的 `hack.Slice` / `hack.String` 零拷贝改为拥有所有权的复制（M-POOL 的思路）。`d2 := d` 共享 `b` 和 decimal 指针在 Rust 中是深拷贝；没有调用方依赖别名修改。
- `hack.Slice` 对空字符串返回 nil 切片：`SetString`、`SetMysqlEnum`、`SetMysqlSet` 遇到空串时 `b = None`，与常见情况一致。Go 中 `b` 是否为 nil 还取决于字符串的数据指针（`s[:0]` 不为 nil），这只能通过 `GetRaw` 观察到；govec 只对 KindRaw 比较 nil。
- `GetValue() any` / `SetValue(any)` 等使用 `GoValue` 枚举；`%v` / `%T` 由 `GoValue::arg` 生成（`BitLiteral`、`HexLiteral` 没有 `String` 方法，`%v` 是 `[1 2 3]`）。
- `Hash64ForDatum` 是 codec 包设置的函数变量：`OnceLock<fn(&mut dyn IHasher, &Datum)>`；未设置时 `Hash64` panic（Go 是 nil 函数调用）。
- `convertToMysqlBit` 在 Go 中会把接收者的 `k` 从 KindInt64 改成 KindUint64；Rust 改在副本上进行（`&self`），只影响 Go 调用之后接收者的状态。
- `MarshalJSON` / `UnmarshalJSON`：按 jsonDatum 的字段顺序手写，解码用 `gostd::json` 的 Go 规则；嵌套的 Time / MyDecimal `UnmarshalJSON` 错误按 Go 的 `addErrorContext` 补上结构名和字段路径。

## 8. 偏离

5. `errors.Wrap(ErrTruncated, "convert to MySQL enum/set failed: "+err)`（`convertToMysqlEnum`、`convertToMysqlSet`，以及经过它们的 `ChangeReverseResultByUpperLowerBound`）返回 cause（M-ERR）；govec 记录 `errors.Cause(err)`。**注意**：Go 中这条消息带前缀，若经 `HandleTruncate` 成为 warning，用户在 SHOW WARNINGS 中看到的文字会不同。请评审判断是否要在 tidb-base 中支持 `errors.Wrap` 的消息前缀。
6. `InvOp2` 只返回错误（Go 的 `any` 结果恒为 nil）。
7. `FieldTypeBuilder` 的方法按值传递构建器（Go 返回 `*FieldTypeBuilder` 链式调用）。
8. `NewFieldTypeWithCollation` 遇到未知排序规则时 panic，与 Go 解引用 nil `*Collation` 相同。

## 9. 上游问题（upstream pitfall 候选）

5. `Datum.convertToMysqlBit` 修改接收者（`d.k = KindUint64`）：同一个 Datum 再次使用时种类已变（Go 的数据竞争隐患）。Rust 不复刻对接收者的修改。
6. `ChangeReverseResultByUpperLowerBound` 的 Ceiling 分支对 `TypeTiny`、`TypeInt24`、`TypeYear` 不加 1（只处理 Short/Long/Longlong）。是否为有意行为未知，照 Go 移植。

## 10. 验证

- govec `datum` 领域共 **141,968** 条（第 1 部分 74,274 条 + 第 2 部分 + 零日期 flag），arm64 本地生成，amd64 取自 tracker CI。回放在 debug 和 release-checked 下都是 0 条不一致。
- 第 2 部分的 op：d_convert 20,820、d_to 13,160、d_compare 7,470、d_reverse 7,140、ft 5,616、d_list 2,000、d_plus 1,530、d_new 916、d_unmarshal 496、context 147、d_bound 75、d_ctor 60、ft_builder 60、field_name 56。
- datum_test.go、field_type_test.go 的字面量（minelits，284 条，`testdata/datum2_literals.tsv`）登记进 `minedGoTests`，作为 d_convert / d_new 的输入。这两个测试文件中没有 govec 表达不了的断言（Hash64 依赖 codec，属于 D01.8），因此没有移植成 Rust 单测。
- **反证**：
  - SortDatums 改用 Rust 标准库排序 → 318 条不一致；
  - 浮点比较中 NaN 改为相等 → 56 条；
  - convertToMysqlBit 去掉 Int64 按 Uint64 处理 → 11 条；
  - ProduceStrWithSpecifiedTp 去掉 binary 判断 → 3 条；
  - ChangeReverse 的 Long Ceiling 不加 1 → 199 条；
  - ConvertToMysqlYear 去掉长度 4 的条件 → 4 条。
- **性能**（arm64，Go `go test -bench`、Rust criterion，同样的输入）：

  | 操作 | Go | Rust |
  |---|--:|--:|
  | compare int | 7.9 ns | 2.2 ns |
  | compare string utf8mb4_bin | 10.7 ns | 4.9 ns |
  | compare string general_ci | 103 ns | 35.8 ns |
  | convert int → varchar(32) | 41.3 ns | 29.1 ns |
  | convert string → bigint | 52.7 ns | 46.9 ns |
  | convert float → decimal(10,2) | 161.8 ns | 140.0 ns |
  | decimal → int64 | 17.4 ns | 10.7 ns |
  | clone string datum | 28.5 ns | 13.5 ns |
  | **set_string** | **0.99 ns** | **7.6 ns** |

  `set_string` 较慢：Go 用 `hack.Slice` 零拷贝引用字符串，Rust 拥有字节（一次分配和一次释放）。chunk 读取路径（D01.7）不经过 Datum，影响有限；若后续热路径需要，可考虑 `Cow<[u8]>` 或 `Bytes`。
