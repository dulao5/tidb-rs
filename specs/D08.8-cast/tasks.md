# D08.8：tasks

- [x] T1 框架：`#[eval_fn]` Decimal、`Args::decimal`、`DecimalView/Out`、常量 DECIMAL、`SigView::vector()`、`BuiltinFunc::in_union`、`gofmt::Arg::{Struct, Array}`
- [x] T2 `baseBuiltinCastFunc`、CAST 到 INT/REAL/DECIMAL 的函数类与 12 个签名
- [x] T3 `BuildCastFunction*`、`WrapWithCastAs{Int,Real,Decimal}`、`PropagateType`、`FoldConstant`
- [x] T4 govec `cast` 领域与回放（两种模式）、反证
- [x] T5 第 2a 部分：RPN 列引用的类型转换；FLOAT/hybrid 参数
- [x] T6a 第 2b-1 部分：CAST 到 STRING、`WrapWithCastAsString`、`BuildCastCollationFunction`、`builtin_convert_charset.go`、`newBaseBuiltinFuncWithTp`/`Cast4String`；govec "str"/"dbl"/"binlit"/"collcast"/"wrapstr"；反证
- [ ] T6b 第 2b-2 部分：CAST 到 TIME/DURATION/JSON/VECTOR；从它们出发的签名
- [ ] T7 第 2b-3 部分（D08.2 第 2 部分其余与 D08.1 2b）：`newBaseBuiltinFuncWithFieldTypes`、`NewFunction*`（`derive` 记录经包 CAST 的构造函数再跑一遍）、常量传播、EvaluatorSuite、chunk_executor、`VecEvalBool`
