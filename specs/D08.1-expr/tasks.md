# D08.1：tasks

- [x] T1 crate 骨架、layers.toml、errors.go、context.go
- [x] T2 collationInfo（D08.2 的 11 个方法）、Column、CorrelatedColumn
- [x] T3 Constant、ParamMarker
- [x] T4 Schema、KeyInfo；expression.go 第 1 部分
- [x] T5 govec `expr` 领域与回放；反证
- [x] T5a 评审 #73 后续：关联列的槽按外层列 `UniqueID` 取值，去掉全局计数器（spec §3.2、§4 第 9 条）；`memory_usage` 不带 state 的理由、`Column.Clone` 共享 `*FieldType` 的移植注意写入 spec §4
- [x] T6a 第 2 部分 2a：N4 §5 门禁（`checked` 形式，通过）、`#[eval_fn]`、两种驱动、RPN、ScalarFunction 规划侧方法、baseBuiltinFunc 字段
  - 验收：`scalar_function.rs` 中不再有临时桩（`part2_stub!` 全部删除，Lucky 评审 #73）——已完成
- [x] T6b 第 2 部分 2b：EvaluatorSuite、chunk_executor、VecEvalBool/EvalExpr（D08.8 2b-3c）；NewFunction*（D08.8 2b-3a，#95）
- [ ] T6c 随第一个 `funcs` 函数族（D08.3–D08.9）完成 T6b 留下的验收项（第一个函数族是 D08.4 比较，结果见 specs/D08.4-compare/spec.md §3.4：(a) 比较没有出错行；(b) 比较签名无状态，留给 D08.6 LIKE/REGEXP；(c) fold 1 已覆盖，TryFold 的 warning 分支由 D08.5 第 1 批的 `artryfold` 记录覆盖；(a) 由 D08.5 第 1 批的 checked 算术（PlusInt 等）覆盖；(d) DisableParseJSONFlag4Expr 已覆盖）：
  - 验收：采用 `checked` 的函数族满足 spec §3.4 三条，govec 含"NULL 下藏着出错值"和"第一个出错行不在第 0 行"的记录
  - 验收：第一个带状态的签名按 spec §3.5 对照 Go 的 `Clone`，govec 有"克隆后分别求值"的操作——已完成（D08.5 第 4 批：RAND，`SigExtra::Rand` 与 Go 一样让克隆共用生成器；govec `mathclone` 800 条，反证 199 条不一致）
  - 验收：门禁按 plan.md 的测量规则（多进程取最小值、amd64 CI 确认）
- [x] T6d（D08.4 第 2 批完成，spec §3.6）移植 CASE WHEN、COALESCE 之前：把 `RpnNode::RowFallback` 泛化（Lucky 评审 #102）。Go 中这两个函数的向量版也是"自己求参数、出问题就改用逐行版"，但规则不同：参数个数不固定、类型各异；任何一个参数（包括第一个）出错或新增 warning 都会回退；只有新增 warning 时才截断；COALESCE 每求一个参数就合并一次。要求：参数个数不固定，输出用签名自己的求值类型，回退规则写在 `SigDesc` 上，不要在 `falls_back_to_rows` 中用 `ptr::eq` 逐个列出签名
- [ ] （可选）IS TRUE/FALSE 的 `...WithNull` pb code 不依赖 `+= 20` 的编号规律：直接写进签名表，或加单元测试核对这 6 个编号（Lucky 评审 #102）
- [ ] T7 第 3 部分：向量函数（随 D08.2）
