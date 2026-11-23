# D13.1 Plan Cache（例外 A3）（tasks）

VS1-15（预处理语句的规划）移植了非缓存部分，见 [spec](spec.md)；缓存本身（例外 A3）第 1 阶段由 VS1-39 实现，见 [A3 spec](../exceptions/A3-plan-cache/spec.md) §7"第 1 阶段实现记录"。

## 给 VS1-15（预处理语句的规划）的注

- [x] **`cleanRetryInfo` 删 plan cache 的 Go 行为要照搬**（VS1-15，spec §4.3，`clean_retry_info_deletes_the_first_closed_stmt_plan_each_time`）（VS1-14b 评审，D05.3 spec §6.1）：Go 只在第一条被关闭的语句上取 `preparedObj`（`PlanCacheStmt`），之后每次循环都用这同一个对象调用 `NewPlanCacheKey`，所以一个事务里关闭多条语句时，每次删除的都是第一条语句的计划，其他语句的计划留在 cache 里。这可能是上游的 bug，但 Go v8.5.8 就是这样，移植 `session.cleanRetryInfo` 的 plan cache 部分（`crates/session/src/session.rs` 的 `SessCtx::clean_retry_info`，现在只有注释）时照 Go 写，并加一个"关闭两条语句只删掉第一条的计划"的用例。

## 从 P1 继承

P1 收尾（#48）时，各块把剩下的函数移交给真正使用它们的块。每个函数在 tracker 的豁免表中写的是"owned by <本块>"，本块完成前要移植并由 govec 覆盖（或改为正式豁免），然后从 tracker 豁免表中删除。

- [ ] P1-1 （D08.2 T8e）参数标记：`ConstExprConsiderPlanCache`、`MaybeOverOptimized4PlanCache` + `SetSkipPlanCache` 在常量传播中的路径（跳过 plan cache 是正确性保护）；需要 `ParamMarker` 常量与 plan cache 上下文后用 govec 覆盖
- [ ] P1-2 （D00.1 T12，#68 评审）`exprstatic` 的 `with_param_list` 现在深拷贝参数列表，Go 共享底层的字节；`PlanCacheParamList` 改为 `Arc<[Datum]>` 共享
