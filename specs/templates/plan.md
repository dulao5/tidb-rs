# <块 ID>：plan（模板）

状态：模板（VS1-01，2026-11-14）。凡是改动计划节点、表达式、`FieldType` 或 `Schema` 的块都用这份模板，P3 的 planner 块和 expression 块都适用；其他块只填第 1、2、5、6 节。固定写法的出处是 [N1](../exceptions/N1-plan-representation/spec.md) §3–§5，在 `crates/planner/src/idiom_tests.rs` 中有对应的测试。

## 1. 范围（按部分）

- 本 PR 移植的函数（文件 → 函数），与 VS1 的 [functions.tsv](../VS1-vertical-slice/functions.tsv) 或块的函数清单逐个对照，多出或缺少的写明原因。
- 块内其余函数登记为 `blocked-by:<块>`，同时登记进 tracker 的 `data/stubs.tsv`。
- converge：`scripts/converge.py` 的 matched / total。

## 2. 边界桩

| Rust 位置 | Go 函数 | 类型（临时 `blocked-by:<块>` / 永久 / 委托） | 调用时的错误 |
|---|---|---|---|

临时桩返回 `tidb_base::blocked::blocked(<块>, <说明>)`：1235，并且 VS1 路由器能识别，从而回退到 PoC（VS1 plan §4.3）。不得用其他错误代替。

## 3. 固定写法（N1 §4，映射规则 M-PLAN / M-SELF / M-IFACE）

1. **接口方法 = trait 方法 + 同名分派函数**。trait 方法签名 `fn xxx(&mut self, ctx: &mut PlanCtx<'_>, this: PlanId, …)`，标注 `// go: … L1 [M-PLAN]`；分派函数用 `logical_dispatch!` 生成，上方写 `// glue: N1 (interface dispatch)`。Go 的 `return p` 写成 `Ok(this)`，返回新子树根时由调用方 `self.children_mut()[i] = …`。
2. **嵌入结构**：算子字段 `base: BaseLogicalPlan`（有自己 schema 的再加 `sp: LogicalSchemaProducer`），`impl LogicalPlan` 中写 `logical_plan_boilerplate!(<Kind>[, producer]);`。嵌入结构的方法写成 `LogicalPlan` 的默认方法。
3. **type switch**：`match p.kind()`；类型断言用 `arena.downcast_ref::<T>(id)`。
4. **借用**：`ctx.arena`、`ctx.expr_ctx`、`ctx.vars` 直接按字段借用；`PlanCtx::split()` 只在必须跨函数一次传出多个借用时用，用到的地方在第 4 节说明。
5. **摘出期间**：不按 `this` 回查 arena；需要节点本身的辅助函数接收 `&self` / `&dyn LogicalPlan`。
6. **物理计划**：`PhysicalPlanRef = Arc<dyn PhysicalPlan>`；写节点用 `make_mut_plan(&mut p)`，改子节点用 `map_children(&mut p, f)`，不要 `child.clone()` 后写回（会被当作共享而复制）。算子实现 `Clone`，`impl PhysicalPlan` 中写 `physical_plan_boilerplate!(<Kind>)`。计划树中不出现 `Weak`。
7. **plan id**：在 Go 调用 `PlanID.Add(1)` 的同一位置调用 `ctx.alloc_plan_id()`；arena 下标不兼任 plan id。
8. **有副作用的 getter**：`OnceCell`（如 `LogicalSchemaProducer.Schema()`）；`SetSchema(nil)` 对应 `set_schema(None)`。
9. **指针比较**：Go 的 `p.Schema() == child.Schema()` 写成 `std::ptr::eq`。

## 4. 共享修改的审计（N1 §5）

1. 对本块的 Go 文件运行两个 grep，逐处给出 S / V0 / V1 / A 结论（分类见 [试点报告](../D09.7-column-pruning/pilot-report.md) 附录 B）：
   - 赋值：`grep -rn '\.Index = \|\.RetType = ' <文件> | grep -v _test.go`
   - 方法调用：`grep -rnE '\.(SetFlag|AddFlag|DelFlag|SetFlen|SetDecimal|SetCharset|SetCollate|SetType|SetFlenUnderLimit|SetDecimalUnderLimit|SetElems|SetArray|ToggleFlag|AndFlag)\(' <文件> | grep -v _test.go`
   - 读代码补充 grep 抓不到的：改"不是自己新建的"参数或表达式类型的 helper（`constant_fold`、`PropagateType`、`WrapWithCastAsInt` 等），以及 `SetSchema(x.Schema())` 这类不 Clone 的 schema 共享。
2. **已知差异 / 模拟表**：

   | Go 位置 | 结论 | 处理（`// Go aliasing (M-FT): <file>.go:<line>, <effect>` 的位置与回归测试名，或不模拟的理由） | 负责人结论 |
   |---|---|---|---|

3. 只改 charset、collation、decimal 的 V1：核对改动后的类型是否进入发给客户端的列定义；进入的按 V1 处理。
4. 用到 `split()` 的地方；摘出分派可能两次访问同一节点的地方；摘出期间按 `this` 回查 arena 的辅助函数（应改为接收 `&self`）。

## 5. 测试

- Go 单测随代码移植（不依赖存储的部分）。
- govec 向量：领域、op、条数、两个目标（amd64 / arm64）。
- 规划器块：计划向量（VS1 plan §7.1，govec 领域 `plan`），去掉 estRows 和 cost 后逐行比较。
- 失败的测试逐个标注 `blocked-by:<块>` 或 `bug`。

## 6. 偏离登记

L2 / L3 偏离、Go map 顺序（M-MAPORDER）、整数溢出（M-INT）等，逐处列出。
