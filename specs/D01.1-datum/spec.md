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

- datum.go 144/144、field_type.go 22/22、field_type_builder.go 20/20、field_name.go 4/4、datum_eval.go 1/1、etc.go `InvOp2`；D01.4 的 convert.go `ToString`（D01.4 达到 44/44）。
- converge：D01.1 **280/280**（第 2 部分评审后调整了块归属，tracker `rules.py`）：
  - `parser_driver/value_expr.go` 全部 16 个和 `NameSlice.FindAstColName` 移到 D02.2：它们实现的是 parser AST 节点（`ast.Node`、`format.RestoreCtx`、`*ast.ColumnName`），留在 D01.1 只会让本块永远完成不了。
  - `ChangeReverseResultByUpperLowerBound` 及其辅助函数 `getDatumBound` 改为 X：v8.5.8 中没有非测试的调用方。Rust 端删除，govec 的 `d_reverse` op 也删除。
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
- `ConvertTo` 是 Go 的指针接收者方法，`convertToMysqlBit` 会把接收者的 `k` 从 KindInt64 改成 KindUint64，调用方之后看到的是改过的接收者。Rust 的 `convert_to` 因此取 `&mut self`，忠实复刻（L1）。这个副作用在 SQL 层可见：index join 用 BIT 列的索引探测外表的 `-1` 时报 ERROR 1406 Data Too Long，hash join / merge join 正常返回（upstream-pitfall tidb-rs#34）。其余目标类型都不写接收者，`convert_to_non_bit(&self)` 供只持有 `&self`、目标固定为非 BIT 的调用方使用（`compareMysqlDecimal`）。
- `b` 的构造集中在 `set_b` / `set_b_string` 两个私有函数里（所有 setter 和构造函数都经过它们）。是否把 `b` 改成 `Cow<[u8]>` / `Bytes` 等实测数据再定（决定性的场景是 chunk 中 varchar 列的 `get_datum`），届时只需改这两个函数。
- `MarshalJSON` / `UnmarshalJSON`：按 jsonDatum 的字段顺序手写，解码用 `gostd::json` 的 Go 规则；嵌套的 Time / MyDecimal `UnmarshalJSON` 错误按 Go 的 `addErrorContext` 补上结构名和字段路径。

## 8. 偏离

5. `errors.Wrap(ErrTruncated, "convert to MySQL enum/set failed: "+err)`（`convertToMysqlEnum`、`convertToMysqlSet`）返回 cause（M-ERR）；govec 记录 `errors.Cause(err)`。评审在 v8.5.8 上验证过：INSERT、UPDATE、INSERT…SELECT 写入非法 enum/set 时，SHOW WARNINGS 和严格模式的报错都只显示 cause 的文字，前缀不会到达客户端，所以只影响日志文字。
   - **必须原样保留**：UPDATE 和 INSERT…SELECT 产生的 warning 是**未格式化**的 `Data truncated for column '%s' at row %d`（不带参数的 `ErrTruncated`）。tidb-rs 要逐字节保持这段文字，不能"修好"占位符。
6. `InvOp2` 只返回错误（Go 的 `any` 结果恒为 nil）。
7. `FieldTypeBuilder` 的方法按值传递构建器（Go 返回 `*FieldTypeBuilder` 链式调用）。
8. `NewFieldTypeWithCollation` 遇到未知排序规则时 panic，与 Go 解引用 nil `*Collation` 相同。

## 9. 上游问题（upstream pitfall 候选）

5. `Datum.convertToMysqlBit` 修改接收者（`d.k = KindUint64`），SQL 层可见：已开 upstream-pitfall tidb-rs#34（评审用 v8.5.8 复现，上游 master 未修）。Rust 照 Go 复刻（见第 7 节）。
6. `ChangeReverseResultByUpperLowerBound` 的 Ceiling 分支对 `TypeTiny`、`TypeInt24`、`TypeYear` 不加 1：函数在 v8.5.8 中没有非测试的调用方，改为 X，不开 issue。

## 10. 验证

- govec `datum` 领域共 **170,188** 条（第 1 部分 74,274 条 + 第 2 部分 + 零日期 flag + 评审后的 d_convert_recv，去掉 d_reverse），arm64 本地生成，amd64 取自 tracker CI。回放在 debug 和 release-checked 下都是 0 条不一致。评审用的种子 11、12、13（arm64 本地生成）回放也都通过。
- 第 2 部分的 op：d_convert_recv 35,360（评审后新增：ConvertTo 的结果和之后的接收者，pool 中每个 datum × 每个字段类型，加上有符号整数到 BIT(1/8/32/63/64)）、d_convert 20,820、d_to 13,160、d_compare 7,470、ft 5,616、d_list 2,000、d_plus 1,530、d_new 916、d_unmarshal 496、context 147、d_bound 75、d_ctor 60、ft_builder 60、field_name 56。
- datum_test.go、field_type_test.go 的字面量（minelits，284 条，`testdata/datum2_literals.tsv`）登记进 `minedGoTests`，作为 d_convert / d_new 的输入。这两个测试文件中没有 govec 表达不了的断言（Hash64 依赖 codec，属于 D01.8），因此没有移植成 Rust 单测。
- **反证**：
  - SortDatums 改用 Rust 标准库排序 → 318 条不一致；
  - 浮点比较中 NaN 改为相等 → 56 条；
  - convertToMysqlBit 去掉 Int64 按 Uint64 处理 → 11 条；
  - ProduceStrWithSpecifiedTp 去掉 binary 判断 → 3 条；
  - `convert_to` 改回在副本上转换（不修改接收者）→ 300 条（全部是 d_convert_recv）；
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
