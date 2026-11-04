# D01.2 MyDecimal：tasks

[P] = 可以并行。

| # | 任务 | 依赖 | 完成条件 |
|---|---|---|---|
| T1 | crate 骨架 `crates/types`；`errors.rs` 桩（5 个错误，错误码与 Go 相同）；`helper.rs` 的 3 个叶子函数；`MyDecimal` 结构体与常量 | — | `cargo build`；布局静态断言 |
| T2 [P] | 解析与输出：`FromString`、`ToString`、`String`、`stringSize`、`removeLeadingZeros`、`removeTrailingZeros`、`digitBounds`、`MarshalJSON`、`UnmarshalJSON` | T1 | 对应向量通过 |
| T3 [P] | 整数和浮点转换：`FromInt`、`FromUint`、`ToInt`、`ToUint`、`FromFloat64`、`ToFloat64`、`NewDecFrom*` | T1 | 对应向量通过 |
| T4 [P] | 二进制编码：`DecimalBinSize`、`ToBin`、`WriteBin`、`FromBin`、`ToHashKey`、`readWord`、`writeWord`、`PrecisionAndFrac`、`GetDigits*`、`IsZero`、`IsNegative` | T1 | 对应向量通过 |
| T5 [P] | 舍入与移位：`Round`、`Shift`、`doMiniLeftShift`、`doMiniRightShift`、`DecimalPeak`、`DecimalNeg`、`NewMaxOrMinDec`、`maxDecimal`、`zeroMyDecimalWithFrac` | T1 | 对应向量通过 |
| T6 [P] | 四则运算与比较：`DecimalAdd/Sub/Mul/Div/Mod`、`doAdd`、`doSub`、`doDivMod`、`add`、`add2`、`sub`、`sub2`、`Compare`、`validateArgs`、`fixWordCntError`、`digitsToWords`、`countLeadingZeroes`、`countTrailingZeroes` | T1 | 对应向量通过 |
| T7 | 向量回放测试与 `TestReset` 单测 | T1（可先写，随 T2–T6 逐步通过） | 27,641 条全部通过 |
| T8 | 收敛检查脚本；58 个函数标注齐全 | T2–T6 | 脚本报告 0 缺失 |
| T9 | 基准（5 个） | T2–T6 | 与 Go 基线对比表 |
| T10 | PoC 切换：`executor/agg.rs`、`distsql/{dag,decode}.rs` 改用本块的 `MyDecimal` | T2–T7 | sysbench、R1 不回退（可以在块合入后单独提 PR） |
