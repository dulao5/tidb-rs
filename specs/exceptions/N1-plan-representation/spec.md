# N1：计划树和表达式树的表示与所有权（基础设计 spec）

状态：**已确认**（2026-11-14，项目负责人在 [#149](https://github.com/dulao5/tidb-rs/issues/149) 批准 D1、D2）。类型：实现例外（基础设计）。

修订记录：

- 2026-11-04：按 PR #2 评审修订（物理计划的冻结时机、plan cache 改写路径、plan id、借用写法）。
- 2026-11-14：按设计试点 D09.7 的结论修订（[试点报告](../../D09.7-column-pruning/pilot-report.md) §6，#149 D1/D2）：
  1. 固定写法改为"摘出节点再分派"，`PlanCtx::split()` 降为备用（§3.8、§4）；
  2. 物理计划从一开始就是 `Arc` 树，用 `make_mut_plan` / `map_children` 原地改或写时复制，取代"先 `Box`、再冻结"；plan cache 路径复制用同一套机制（§3.4、§3.5）；
  3. 共享修改的审计范围扩大到 `FieldType` 和 `Schema` 的别名，新增映射规则 `M-FT`（§1、§3.3、§5）；
  4. 写明 `M-SELF` 的细节（§3.7）；
  5. 登记交给 N4 / A3 的待定项，以及未验证的 §3.2（§7）。

## 1. 要解决的问题

Go 优化器的数据结构依赖 GC 和随意共享的指针，不能原样搬进安全 Rust。如果每个块各自想办法，会出现多套不兼容的写法，规则代码也就没法再逐行对应 Go。所以需要在 P1 之前统一定下来。

v8.5.8 中与表示相关的事实：

| 事实 | 位置 | 对 Rust 的影响 |
|---|---|---|
| 逻辑算子都实现 `base.LogicalPlan`（约 30 个方法）；公共部分 `BaseLogicalPlan` 通过 `self base.LogicalPlan` 字段回调具体算子，模拟虚函数 | `planner/core/base/plan_base.go`、`operator/logicalop/base_logical_plan.go` | Rust 的 trait 默认方法天然就能动态分派，`self` 字段可以去掉（L2） |
| 规则的典型写法是"处理子节点，返回新的子树根，由调用方 `SetChild` 替换"，同时也会原地修改自身字段（如 `p.Conditions = ...`） | 如 `LogicalSelection.PredicatePushDown` | 需要"可原地修改的节点 + 可替换的子节点引用" |
| `FindBestTask` 在每个逻辑节点上缓存结果（`taskMap`，以物理属性为键），并支持按时间戳回滚 | `BaseLogicalPlan.taskMap/taskMapBak` | 记忆化数据需要挂在节点身份上 |
| 列的身份是 `Column.UniqueID`，比较列用 ID 而不是指针（`EqualColumn`） | `expression/column.go` | 表达式不必依赖指针身份，可以安全地共享不可变表达式 |
| 表达式在多个计划节点之间共享；需要改时，惯例是先 `Clone()` 再改（如 `ResolveIndices` 设置 `Index`）。planner/core 非测试代码中有 44 处 `.Index =`/`.RetType =` 赋值（含 `AccessPath.Index` 等非表达式字段） | planner/core | 对应"写时复制"。D09.7 已逐处审计（试点报告附录 A）：只有 `rule_partition_processor.go:157` 依赖"共享修改对他处可见" |
| **`FieldType` 别名**：`Column.Clone()` 是 `newCol := *col`，`RetType` 指针仍然共享；`Schema.Clone`、`MergeSchema`、`buildProjectionField`（`RetType: expr.GetType()`）、`firstrow(col)` 都只复制列，不复制 `FieldType`。所以同一源列的 `FieldType` 在整条查询里实际上是一个共享的可变对象，`GetType().SetFlag(...)` 之类的方法调用会对所有持有者生效。memory table 的列直接指向进程全局的 infoschema 定义（`logical_plan_builder.go:5510`），修改会污染之后的所有查询（上游 bug [#148](https://github.com/dulao5/tidb-rs/issues/148)） | `expression/column.go`、planner/core、expression | Rust 的 `Column.ret_type` 按值保存（D08.1），这种别名不存在。需要一条全局规则（`M-FT`，§3.3）。试点审计：方法调用式的原地修改 planner 101 处、expression 55 处，"共享且可见性有影响"的按机制合并为 10 组（试点报告附录 B） |
| **`Schema` 别名**：planner/core 中约 33 处 `SetSchema(x.Schema())` 不做 Clone，直接共享指针；`BaseLogicalPlan.Schema()` 返回子节点的 schema | planner/core | 逻辑侧大多是所有权转移（move 或 clone 即可）；唯一的例外是 INTERSECT/EXCEPT 的半连接（`logical_plan_builder.go:2204`），见试点报告 §3.4。按 §3.3 审计 |
| 物理计划进 plan cache 后跨语句复用，靠生成的 `Clone` 复制；实例级 plan cache 每次命中都深拷贝整棵计划（`CloneForInstancePlanCache`） | `plan_clone_generated.go`、plan cache | 物理计划要能脱离单条语句的生命周期 |

## 2. 方案比较

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| a. 模仿 Go 指针 | `Rc<RefCell<dyn LogicalPlan>>` | 代码形状最像 Go | 运行时借用冲突会 panic；有循环引用和泄漏风险；不能跨线程；性能差。**否决** |
| b. 拥有型树 | `Box<dyn LogicalPlan>`，子节点由父节点拥有 | 简单，所有权清楚 | 节点没有稳定身份，`taskMap` 记忆化和回滚不好挂；CTE 等共享子计划要另想办法 |
| c. 语句级 arena + 下标 | 逻辑节点放在每次优化独占的 arena 里，用 `PlanId(u32)` 互相引用 | 节点身份稳定（对应 Go 的 `p.ID()`）；"返回新子树根、由调用方替换"的写法可以原样对应；记忆化数据放在以 `PlanId` 为键的旁表里；优化结束整体释放 | 调用 arena 中节点的方法时要先摘出节点（§3.8） |

## 3. 方案

1. **逻辑计划用方案 c**。`LogicalPlan` 是 trait，方法签名与 Go 一一对应，只是用 `PlanId` 代替接口指针、用 `&mut PlanCtx`（内含 arena）代替隐式的全局可达性。Go 中由 `self` 字段完成的回调，改用 trait 默认方法（`M-SELF`，§3.7）。
2. **`taskMap` 等记忆化数据放旁表**：`HashMap<(PlanId, PropKey), Task>`，回滚逻辑按 Go 的时间戳语义移植。**本条没有被 D09.7 验证**，见 §7 第 3 条。
3. **表达式用 `Arc<Expr>`，不可变共享，写时复制**：Go 的"先 Clone 再改"对应 `Arc::make_mut`。列身份继续用 `UniqueID`。审计范围包括三类共享对象：
   - **表达式节点**（`.Index =` / `.RetType =` 赋值）：44 处已在 D09.7 审计完（试点报告附录 A）。唯一依赖可见性的 `rule_partition_processor.go:157`（改 `partCols[i].Index`，期望在 `hashExpr` 中生效）在 PartitionProcessor 块（D09.12）的 plan 中写明改写方式：按 UniqueID 映射到位置，重写出一个新的 `hashExpr`。
   - **`FieldType`**（`GetType()` 返回的对象上的赋值和方法调用）：按映射规则 **`M-FT`** 处理。默认写时复制：`Column.ret_type` 保持按值保存，**不改成 `Arc<FieldType>`**（保留 D08.1 的表示）。只有当 Go 的别名会改变 golden test 或 EXPLAIN 对照能看到的计划时，才在那一处显式模拟 Go 的效果：除了改本处的副本，还按 UniqueID 同步改同一查询中来源列（以及引用它的列）的类型。例如 E3（`rule_eliminate_projection.go:201`）遇上 MaxMinEliminator 时，Go 会多出一个 `not(isnull(a))` 的 Selection，Rust 要在这一处模拟。每处模拟都：
     - 加注释 `// Go aliasing (M-FT): <file>.go:<line>, <effect>`；
     - 配一个回归测试；
     - 在该块 plan 的"已知差异 / 模拟"表里登记，由项目负责人逐处接受或否决。
   - **不模拟跨查询的全局效果**：memory table 上的全局污染（附录 B 第 1、2 组）是上游 bug [#148](https://github.com/dulao5/tidb-rs/issues/148)，按 upstream-pitfall 处理，tidb-rs 不复现，只在需要 EXPLAIN 对照时模拟单个查询内的效果。
   - **`Schema`**：每个节点拥有自己的 `Schema`；不拥有 schema 的节点（Go 中 `Schema()` 返回子节点 schema 的 Selection、Sort 等）写到拥有者那里（`PlanArena::schema_mut`）。共享 schema 的写法按与 `FieldType` 相同的 S/V0/V1 分类审计，V1 同样按 `M-FT` 的模拟写法处理。
   - 每块 plan 都必须包含这三类审计的结果，模板要求见 §5。
4. **物理计划从一开始就是 `Arc` 树**（取代 2026-11-04 版的"`Box` 阶段 + `DoOptimize` 末尾冻结"）。`PhysicalPlanRef = Arc<dyn PhysicalPlan>`，`PhysicalPlan: Send + Sync`：
   - **语句独占时原地改**：`FindBestTask` 选出方案之后，v8.5.8 还会原地修改物理树：`postOptimize`（`optimizer.go:413`，由 L296 调用）依次执行 `eliminatePhysicalProjection`、`InjectExtraProjection`、`mergeContinuousSelections`、`eliminateUnionScanAndLock`、`enableParallelApply`、`handleFineGrainedShuffle`、`propagateProbeParents`、`countStarRewrite`、`disableReuseChunkIfNeeded`、`tryEnableLateMaterialization`、`generateRuntimeFilter` 等 pass，之后还有 `ResolveIndices`。这些 pass 通过 `make_mut_plan` 写节点：引用计数为 1 时就是 `Arc::get_mut`，与 Go 的原地修改逐行对应。
   - **共享后写时复制**：被 plan cache 或 `taskMap` 中的多个父候选共享的节点，`make_mut_plan` 先浅拷贝一个节点（`clone_node`：字段复制，子节点和表达式仍共享），再改。
   - **改子节点用 `map_children`**：Go 的 `p.Children()[i] = f(child)`。节点独占时把子节点移出再放回，`f` 看到的是子节点自己的引用计数；共享时只在子节点确实变化时复制父节点。不要直接 `child.clone()` 再写回：这会让每个子节点看起来都被共享，从而被复制（试点 B5）。
   - 理由：`FindBestTask` 的记忆化会把同一个子计划挂到多个父候选下。用 `Box` 意味着每挂一次就深拷贝；用 `Arc`，落选的候选被丢弃后子计划的引用计数回到 1，`postOptimize` 可以原地改。也不再需要单独的冻结步骤，`Arc<dyn PhysicalPlan>` 本身就可以进 plan cache、交给执行器 worker 共享，`Clone` 是引用计数加一。
   - 这两个函数写进映射规则 `M-PLAN`，签名见 [foundation-interfaces.md](../foundation-interfaces.md)。
5. **plan cache 命中后的改写：路径复制**，用与 §3.4 相同的机制。命中时 `RebuildPlan4CachedPlan` / `rebuildRange`（`plan_cache_rebuild.go:31`、`:77`）会按本次参数改写缓存计划中的 range。Rust 不复制整棵计划：对缓存中的 `Arc` 树，用 `map_children` 走到每个被改写的 reader / point get，用 `make_mut_plan` 改写，于是只有根到这些节点路径上的节点被浅拷贝，其余子树、表达式和 schema 都共享。缓存中的原计划不变，并发执行的语句互不影响（试点测试 `plan_cache_rebuild_by_path_copy`，4 个线程并发重建）。
   - 范围：**只考虑实例级 plan cache**（项目负责人 2026-11-14）。Go 在 Put 时和每次命中时都深拷贝（含 `util.CloneExpressions`），Rust 只浅拷贝路径。
   - range 每次都会重建，所以根到每个 reader 的路径都会被复制，通常是 root task 的大部分节点；但都是浅拷贝，仍远低于 Go 的深拷贝。
   - PointGet / BatchPointGet 的 `Handle`、`IndexValues` 与 `PhysicalTableScan.Ranges` 同形，试点只演示了 TableScan。
   - 具体写法和风险（计划节点不能保存会话上下文、Go 执行期修改计划的位置、表达式求值缓存）在 A3（plan cache）spec 中定稿，见 §7。
6. **`PlanId`（arena 下标）不兼任 Go 的 plan id**。Go 的 id 来自 `PlanID.Add(1)`（`operator/baseimpl/plan.go:42` 等 7 处），按构造顺序分配，包括后来被丢弃的节点；EXPLAIN 输出的 `TableReader_5` 就是它。计划级对照（EXPLAIN 与 Go 对比）要求编号相同，所以每个节点另存一个 `id` 字段，从语句级计数器在 Go 分配的同一位置分配（`PlanCtx::alloc_plan_id`）。arena 下标只用于内部引用。试点按此分配，金标准中的 plan id 都能对上。
7. **`M-SELF` 的细节**（Go 的 struct 嵌入与 `self` 回调）：
   - **嵌入结构用访问器**：Go 的嵌入链（`LogicalProjection` 嵌 `LogicalSchemaProducer`，后者嵌 `BaseLogicalPlan`）写成 `base()` / `base_mut()`、`schema_producer()` / `schema_producer_mut()` 访问器，由宏为每个算子生成；嵌入结构的方法写成 `LogicalPlan` 的 trait 默认方法，经访问器读写嵌入字段。没有嵌入 `LogicalSchemaProducer` 的算子，`schema_producer()` 返回 `None`。
   - **有副作用的 getter 用 `OnceCell`**：如 `LogicalSchemaProducer.Schema()` 在 schema 为空时从子节点克隆一份并保存下来。Rust 中 schema 字段是 `OnceCell<Schema>`，读路径仍然是 `&self`；`SetSchema(nil)` 对应把 `OnceCell` 重置为空。
   - **`this: PlanId` 代替 Go 的 `self` / `p` 指针**：trait 方法多接收一个 `this: PlanId`，即节点自己在 arena 中的下标。Go 的 `return p`（返回自己作为新子树根）写成 `Ok(this)`；`s.Self()` 回调具体算子、需要把自己交给别的函数（trace 等）时，传 `this`。
   - **指针比较**：Go 的 `p.Schema() == child.Schema()` 写成 `std::ptr::eq`，结果相同。
8. **AST** 维持 parser crate 现有的结构。注意：N2 要求 `Session: Send`，prepared statement 若在会话中保存 AST，AST 中的 `Rc` 要换成 `Arc` 或拥有型结构，或者会话只保存降级后的 IR（D02 中处理）。

## 4. 固定写法：摘出节点再分派（对 L1 移植的影响）

**标准写法**：Go 的接口方法调用 `x.Method(...)`（x 是 arena 中的节点）写成一个同名自由函数加一个 `&mut self` 的 trait 方法。自由函数用 `PlanArena::take` 把节点从 arena 中暂时摘出，以 `&mut self` 调用 trait 方法（同时传 `&mut PlanCtx`），再用 `put` 放回（统一由 `with_detached` 完成）。方法体内，`self`（节点）和 `ctx`（含 arena）是两个互不重叠的借用，所以 Go 的 `p.Field = ...`、`p.Children()[0] = child.Method(...)` 都能原样写。

Go：

```go
retConditions, child = p.Children()[0].PredicatePushDown(append(canBePushDown, predicates...), opt)
...
p.Conditions = expression.PropagateConstant(exprCtx, retConditions)
```

Rust（示意）：

```rust
// 分派函数：每个 Go 接口方法一个
// glue: N1 (interface dispatch)
pub fn predicate_push_down(ctx: &mut PlanCtx<'_>, p: PlanId, predicates: Vec<Expr>, opt: &mut LogicalOptimizeOp)
    -> Result<(Vec<Expr>, PlanId), Error> {
    with_detached(ctx, p, |node, ctx| node.predicate_push_down(ctx, p, predicates, opt))
}

// trait 方法：方法体与 Go 逐行对应
// go: (*LogicalSelection).PredicatePushDown L1 [M-PLAN]
fn predicate_push_down(&mut self, ctx: &mut PlanCtx<'_>, this: PlanId, predicates: Vec<Expr>, opt: &mut LogicalOptimizeOp)
    -> Result<(Vec<Expr>, PlanId), Error> {
    let (ret_conditions, child) = predicate_push_down(ctx, self.children()[0], [can_be_push_down, predicates].concat(), opt)?;
    ...
    self.conditions = propagate_constant(&*ctx.expr_ctx, ret_conditions);
    Ok((ret, this))
}
```

逻辑步骤、分支和返回值与 Go 相同。区别只在"如何取到节点"，由映射规则 `M-PLAN` 覆盖，所以仍是 L1，标注写 `L1 [M-PLAN]`。

试点中借用检查器迫使的其他偏离也都是机械的（试点报告 §4 的 B2–B6）：

- 遍历 `self.schema()` 时又写 `self` 的另一个字段：通过嵌入字段路径取 schema（`self.sp.schema(&self.base.children, ...)`）。
- `f(&mut x, x.len())`：先把 `x.len()` 取到局部变量。
- 自由函数同时操作父子两个节点：摘出父节点；或先用 `&arena` 计算、再用 `&mut arena` 写回；按下标遍历子节点。
- 物理计划上的 `p.Children()[i] = f(child)`：`map_children`（§3.4）。
- Go 往参数 slice 上 append（与调用方共享底层数组）：参数按值传 `Vec`，调用方之后还要用的先 clone，并在块 plan 中逐处核对等价性。

**`PlanCtx` 的字段是公开的**：在同一个函数里，`ctx.arena` 和 `ctx.expr_ctx`、`ctx.vars` 是不同字段的借用，编译器允许同时借用。表达式上下文是 `ExprContextRef = Arc<dyn ExprContext>`（N4 已移植的代码），warning、列 ID 分配器都是内部可变的，从来不需要 `&mut`。

**`PlanCtx::split()` 只作备用**：只在需要跨函数边界、经一个 `&mut PlanCtx` 一次传出多个互不重叠的借用时使用（签名见 foundation-interfaces.md）。D09.7 中一次都没有用到。新代码先用摘出分派和字段借用，用 `split()` 时在块 plan 中说明原因。

**摘出分派的约束**：节点被摘出期间再访问它会 panic（`take` 两次、`put` 一个没有摘出的节点都会 panic）。v8.5.8 的树规则不会在一次分派中两次访问同一节点；CTE 的子计划由 `LogicalCTE` 独立优化，不在同一次遍历里。D09.6（去关联）和 join reorder 移植时，要在块 plan 中核对这一点。

## 5. 验收与块 plan 模板

**已完成**（D09.7 设计试点，2026-11-13，[试点报告](../../D09.7-column-pruning/pilot-report.md)）：

- 列裁剪（9 个算子）、投影消除和 plan cache 范围重建（路径复制）在 N1 上移植，对照 Go 自己的金标准输出。规则代码与 Go 逐行对照（16 个函数，Go 400 行 → Rust 457 行），借用检查器带来的偏离都是机械的。对 P3 工作量的估算：比纯逐行翻译多 10%–20%。
- 共享修改的审计：44 处赋值逐处结论（附录 A）；156 处 FieldType 方法调用式修改的初筛（附录 B，V1 合并为 10 组，其中 2 组已在 Go v8.5.8 上实测）。

**每块 plan 模板必须包含**（凡是改动计划节点、表达式、`FieldType` 或 `Schema` 的块；P3 的 planner 块和 expression 块都适用）：

1. **两个审计 grep**，对本块的 Go 文件运行，逐处给出 S / V0 / V1 / A 结论（分类定义见试点报告附录 B）：
   - 赋值：`grep -rn '\.Index = \|\.RetType = ' <本块文件> | grep -v _test.go`
   - 方法调用：`grep -rnE '\.(SetFlag|AddFlag|DelFlag|SetFlen|SetDecimal|SetCharset|SetCollate|SetType|SetFlenUnderLimit|SetDecimalUnderLimit|SetElems|SetArray|ToggleFlag|AndFlag)\(' <本块文件> | grep -v _test.go`
   - 另外，抓不到的 helper（改的是"不是自己新建的参数或表达式"的类型，如 `constant_fold`、`PropagateType`、`WrapWithCastAsInt`）和 `SetSchema(x.Schema())` 这类不 Clone 的 schema 共享，要读代码补充。
2. **"已知差异 / 模拟"表**：每个 V1 一行，写明是按 `M-FT` 显式模拟（注释位置、回归测试名），还是登记为不模拟（理由）。
3. **只改元数据的 V1 要单独核对**：附录 B 中有 4 组只改 charset、collation 或 decimal（第 3、4、8、9 组）。它们不改变计划，但如果改动后的类型会进入**发给客户端的列定义**（结果集元数据），就是用户可见的差异，不能当作无害。每块要核对本块的这类修改是否到达列定义，到达的按 V1 处理（Lucky，#149）。
4. 用到 `split()` 的地方，以及摘出分派可能两次访问同一节点的地方（§4）。

**仍未完成**：

- 指标：sysbench 只读语句和 TPC-H 22 条查询的优化耗时不高于 Go；每条语句优化结束后没有残留分配（arena 整体释放）。要等 planbuilder（D09.1）可用后测。
- §3.2 `taskMap` 旁表，见 §7 第 3 条。

## 6. 原待定项的结论

1. **arena 按语句划分，不按 optimize 调用划分**。v8.5.8 在建计划的过程中会递归调用优化器：不相关子查询在表达式改写时就先优化并执行（`expression_rewriter.go` L1100、L1394 调 `DoOptimize`），`INSERT ... SELECT`、`planbuilder.go` L769/L4679/L5030 同样如此，而且都复用同一个 `builder.ctx`。所以一条语句只有一个 `PlanArena`，嵌套的 `DoOptimize` 共用它；物理计划是 `Arc` 树，不引用 arena。arena 随语句的规划阶段结束而释放。
2. **trait 对象，不用枚举**。arena 中存 `Box<dyn LogicalPlan>`，用 `as_any()` 向下转型对应 Go 的类型断言（`p.(*LogicalSelection)`）。另外给 `LogicalPlan` / `PhysicalPlan` 一个 `kind()` 方法，返回算子种类的枚举，Go 中大量的 `switch x := p.(type)` 写成 `match p.kind()`，不写成一串向下转型（`M-IFACE`）。理由：可以按块逐个算子增量移植；Go 代码中大量的类型断言和类型 switch 能逐行对应。性能敏感的地方（规则遍历）如果测得虚调用成为瓶颈，再收敛为枚举，届时只影响 arena 内部。
3. **与 N2 的接口**：`PlanArena` 放在 N2 定义的 `PlanCtx` 里。分派函数的签名统一为 `fn(ctx: &mut PlanCtx, p: PlanId, ...)`，trait 方法为 `fn(&mut self, ctx: &mut PlanCtx, this: PlanId, ...)`（§4）。
4. **与 N3 的接口**：物理计划是 `Send + Sync` 的 `Arc` 树，执行器 worker 可以直接共享。Go 的部分并发执行器会为每个 worker 复制表达式（原因待逐个核实，可能与求值时的可变缓存有关）；Rust 中是否还需复制，取决于表达式求值是否带可变状态，在 N4 中定。

## 7. 待定项（交给其他 spec）

1. **N4：`Arc<Column>` 与内嵌 `Column` 两种表示之间的分配开销**。schema 里是 `Arc<Column>`，表达式里是 `Expr::Column(Column)`（列直接内嵌）。`ExtractColumns`、`Column2Exprs`、消除投影时往 replace map 里放列，每次转换都要分配一个新的 `Arc`，Go 在这些地方不分配。试点没有测绝对开销（与规则涉及的列引用数成正比）。规划器全面移植（P3）前在 N4 中测一下，必要时改成 `Expr::Column(Arc<Column>)`。
2. **N4 / A3：跨会话共享计划时的函数缓存**。Go 的 `BuiltinFuncCache` 按 `ctx_id` 缓存、用 Mutex 保护。实例级 plan cache 让多个会话共享同一个表达式后，会争用这把锁，并因 `ctx_id` 不同而互相冲掉缓存。实例级 plan cache 打开前，要在 N4 或 A3 中决定：改为按执行状态缓存（N4 的 `RpnState`），或命中时复制带缓存的函数节点。A3 还要处理试点报告 §5 的其他风险：计划节点不能保存会话上下文（`p.SCtx()`、`PointGetPlan.ctx`），Go 在执行期修改计划的位置（`generateNewPlan` 注释 "execution (might modify the plan)"）要在 D12 移植时逐处找出。
3. **§3.2 `taskMap` / `FindBestTask` 没有验证**。D09.7 用不到 FindBestTask，`taskMap` 旁表、按时间戳回滚、`Arc` 子计划在候选之间的共享都没有被试点覆盖（`memo_shared_child_is_copied_only_while_shared` 只是演示，属推断）。D10.2（约 5.9k 行）开工时补一个小试点，只验证这三件事；结论回写本 spec。
