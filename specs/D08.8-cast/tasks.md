# D08.8：tasks

- [x] T1 框架：`#[eval_fn]` Decimal、`Args::decimal`、`DecimalView/Out`、常量 DECIMAL、`SigView::vector()`、`BuiltinFunc::in_union`、`gofmt::Arg::{Struct, Array}`
- [x] T2 `baseBuiltinCastFunc`、CAST 到 INT/REAL/DECIMAL 的函数类与 12 个签名
- [x] T3 `BuildCastFunction*`、`WrapWithCastAs{Int,Real,Decimal}`、`PropagateType`、`FoldConstant`
- [x] T4 govec `cast` 领域与回放（两种模式）、反证
- [ ] T5 第 2 部分：RPN 列引用的类型转换；FLOAT/hybrid 参数
- [ ] T6 第 2 部分：CAST 到 STRING/TIME/DURATION/JSON/VECTOR；其余签名
- [ ] T7 随 D08.2 第 2 部分与 D08.1 2b：`newBaseBuiltinFuncWithTp`/`WithFieldTypes`、`NewFunction*`、EvaluatorSuite
