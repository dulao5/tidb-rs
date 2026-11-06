# N1：计划树和表达式树的表示与所有权（基础设计 spec，草案）

状态：草案（2026-11-04 按 PR #2 评审修订：物理计划的冻结时机、plan cache 改写路径、plan id、借用写法）。类型：实现例外（基础设计）。计划定稿：P1 之前。

## 1. 要解决的问题

Go 优化器的数据结构依赖 GC 和随意共享的指针，不能原样搬进安全 Rust。如果每个块各自想办法，会出现多套不兼容的写法，规则代码也就没法再逐行对应 Go。所以需要在 P1 之前统一定下来。

v8.5.8 中与表示相关的事实：

| 事实 | 位置 | 对 Rust 的影响 |
|---|---|---|
| 逻辑算子都实现 `base.LogicalPlan`（约 30 个方法）；公共部分 `BaseLogicalPlan` 通过 `self base.LogicalPlan` 字段回调具体算子，模拟虚函数 | `planner/core/base/plan_base.go`、`operator/logicalop/base_logical_plan.go` | Rust 的 trait 默认方法天然就能动态分派，`self` 字段可以去掉（L2） |
| 规则的典型写法是"处理子节点，返回新的子树根，由调用方 `SetChild` 替换"，同时也会原地修改自身字段（如 `p.Conditions = ...`） | 如 `LogicalSelection.PredicatePushDown` | 需要"可原地修改的节点 + 可替换的子节点引用" |
| `FindBestTask` 在每个逻辑节点上缓存结果（`taskMap`，以物理属性为键），并支持按时间戳回滚 | `BaseLogicalPlan.taskMap/taskMapBak` | 记忆化数据需要挂在节点身份上 |
| 列的身份是 `Column.UniqueID`，比较列用 ID 而不是指针（`EqualColumn`） | `expression/column.go` | 表达式不必依赖指针身份，可以安全地共享不可变表达式 |
| 表达式在多个计划节点之间共享；需要改时，惯例是先 `Clone()` 再改（如 `ResolveIndices` 设置 `Index`），planner/core 非测试代码中有 44 处 `.Index =`/`.RetType =` 赋值（含 `AccessPath.Index` 等非表达式字段）。抽查的 `resolve_indices.go`、`logical_plan_builder.go` 中，都是先 `Clone()` 再改，或者改的是刚新建的对象 | planner/core | 对应"写时复制"；需要审计 Go 是否有依赖"共享修改对他处可见"的写法 |
| 物理计划进 plan cache 后跨语句复用，靠生成的 `Clone` 复制 | `plan_clone_generated.go`、plan cache | 物理计划要能脱离单条语句的生命周期 |

## 2. 方案比较

| 方案 | 做法 | 优点 | 缺点 |
|---|---|---|---|
| a. 模仿 Go 指针 | `Rc<RefCell<dyn LogicalPlan>>` | 代码形状最像 Go | 运行时借用冲突会 panic；有循环引用和泄漏风险；不能跨线程；性能差。**否决** |
| b. 拥有型树 | `Box<dyn LogicalPlan>`，子节点由父节点拥有 | 简单，所有权清楚 | 节点没有稳定身份，`taskMap` 记忆化和回滚不好挂；CTE 等共享子计划要另想办法 |
| c. 语句级 arena + 下标 | 逻辑节点放在每次优化独占的 arena 里，用 `PlanId(u32)` 互相引用 | 节点身份稳定（对应 Go 的 `p.ID()`）；"返回新子树根、由调用方替换"的写法可以原样对应；记忆化数据放在以 `PlanId` 为键的旁表里；优化结束整体释放 | 同时修改父节点、读取子节点时要先取出 ID，写法比 Go 多一步 |

## 3. 建议

1. **逻辑计划用方案 c**。`LogicalPlan` 是 trait，方法签名与 Go 一一对应，只是用 `PlanId` 代替接口指针、用一个 `&mut PlanArena`（连同优化上下文）代替隐式的全局可达性。Go 中由 `self` 字段完成的回调，改用 trait 默认方法（L2）。
2. **`taskMap` 等记忆化数据放旁表**：`HashMap<(PlanId, PropKey), Task>`，回滚逻辑按 Go 的时间戳语义移植。
3. **表达式用 `Arc<Expr>`，不可变共享，写时复制**：Go 的"先 Clone 再改"对应 `Arc::make_mut`。列身份继续用 `UniqueID`。对这 44 处逐个审计（在试点块中完成）：如果 Go 依赖"修改对其他持有者可见"，就在该块的 plan 里写明处理方式，并补一个回归测试。
4. **物理计划在 `DoOptimize` 返回前是可变的，返回时冻结为不可变的 `Arc` 树**。`FindBestTask` 选出方案之后，v8.5.8 还会原地修改物理树：`postOptimize`（`optimizer.go:413`，由 L296 调用）依次执行 `eliminatePhysicalProjection`、`InjectExtraProjection`、`mergeContinuousSelections`、`eliminateUnionScanAndLock`、`enableParallelApply`、`handleFineGrainedShuffle`、`propagateProbeParents`、`countStarRewrite`、`disableReuseChunkIfNeeded`、`tryEnableLateMaterialization`、`generateRuntimeFilter` 等 pass，之后还有 `ResolveIndices`。所以：
   - 优化期间，物理节点是拥有型的可变树（`Box<dyn PhysicalPlan>`，子节点由父节点拥有），这些 pass 可以逐行对应 Go 的原地修改；
   - `DoOptimize` 末尾整树冻结为 `Arc` 节点（`M-PLAN`）。冻结后可以脱离语句的生命周期，进 plan cache，交给执行器 worker 共享，`Clone` 变成引用计数加一。
5. **plan cache 命中后的改写**：命中时 `RebuildPlan4CachedPlan` / `rebuildRange`（`plan_cache_rebuild.go:31`、`:77`）会按本次参数改写缓存计划中的 range。这与"不可变"直接冲突，采用**路径复制**：只复制被改写的 reader / point get 节点及其到根的路径（`Arc::make_mut`），其余子树共享。缓存中的原计划不变，并发执行的语句互不影响。具体写法在 A3（plan cache）spec 中定稿，本 spec 只规定原则。
6. **`PlanId`（arena 下标）不兼任 Go 的 plan id**。Go 的 id 来自 `PlanID.Add(1)`（`operator/baseimpl/plan.go:42` 等 7 处），按构造顺序分配，包括后来被丢弃的节点；EXPLAIN 输出的 `TableReader_5` 就是它。计划级对照（EXPLAIN 与 Go 对比）要求编号相同，所以每个节点另存一个 `id` 字段，从语句级计数器在 Go 分配的同一位置分配。arena 下标只用于内部引用。
7. **AST** 维持 parser crate 现有的结构。注意：N2 要求 `Session: Send`，prepared statement 若在会话中保存 AST，AST 中的 `Rc` 要换成 `Arc` 或拥有型结构，或者会话只保存降级后的 IR（D02 中处理）。
5. **AST** 维持 parser crate 现有的拥有型结构，不变。

## 4. 对 L1 移植的影响（示例）

Go：

```go
retConditions, child = p.Children()[0].PredicatePushDown(append(canBePushDown, predicates...), opt)
...
p.Conditions = expression.PropagateConstant(exprCtx, retConditions)
```

Rust（示意）：

```rust
// go: LogicalSelection.PredicatePushDown L2 (arena ids instead of interface pointers)
let child_id = ctx.arena[p].children()[0];
let (ret_conditions, child) = predicate_push_down(ctx, child_id, [can_be_push_down, predicates].concat(), opt);
...
let (arena, ectx) = ctx.split();            // 固定写法：一次拆出 arena 和表达式上下文，避免两个可变借用冲突
arena.sel_mut(p).conditions = propagate_constant(ectx, ret_conditions);
```

逻辑步骤、分支和返回值与 Go 相同。区别只在"如何取到节点"，由映射规则 `M-PLAN` 覆盖，所以仍是 L1，标注写 `L1 [M-PLAN]`。

`PlanCtx` 同时持有 arena、`&mut SessionVars`、`&StmtCtx`（N2 §3.3），`ctx.arena.sel_mut(p).conditions = propagate_constant(ctx.expr_ctx(), …)` 这种写法在 Rust 中编译不过（两个可变借用）。这是 arena 方案最常见的摩擦点。`PlanCtx::split()` 一次返回互不重叠的借用（arena 可变、表达式上下文只读），是唯一允许的写法；设计试点（D09.7 spike）要验证它覆盖所有规则的写法，并写进 plan 模板。

## 5. 验收

- 在设计试点 D09.7（列裁剪 + 投影消除，spike，允许桩算子，不合入）上实际使用，正好覆盖原地修改、子节点替换、schema 修改三种写法。产出：本 spec 的修订、`split()` 等固定写法、plan 模板。
- 指标：sysbench 只读语句和 TPC-H 22 条查询的优化耗时不高于 Go；每条语句优化结束后没有残留分配（arena 整体释放）。
- 共享表达式的审计清单完成，每处都有结论。

## 6. 原待定项的结论

1. **arena 按语句划分，不按 optimize 调用划分**。v8.5.8 在建计划的过程中会递归调用优化器：不相关子查询在表达式改写时就先优化并执行（`expression_rewriter.go` L1100、L1394 调 `DoOptimize`），`INSERT ... SELECT`、`planbuilder.go` L769/L4679/L5030 同样如此，而且都复用同一个 `builder.ctx`。所以一条语句只有一个 `PlanArena`，嵌套的 `DoOptimize` 共用它；物理计划在每次 `DoOptimize` 结束时提取成 `Arc` 树，不再引用 arena。arena 随语句的规划阶段结束而释放。
2. **trait 对象，不用枚举**。arena 中存 `Box<dyn LogicalPlan>`，用 `as_any()` 向下转型对应 Go 的类型断言（`p.(*LogicalSelection)`）。另外给 `LogicalPlan` / `PhysicalPlan` 一个 `kind()` 方法，返回算子种类的枚举，Go 中大量的 `switch x := p.(type)` 写成 `match p.kind()`，不写成一串向下转型（`M-IFACE`）。理由：可以按块逐个算子增量移植；Go 代码中大量的类型断言和类型 switch 能逐行对应。性能敏感的地方（规则遍历）如果测得虚调用成为瓶颈，再收敛为枚举，届时只影响 arena 内部。
3. **与 N2 的接口**：`PlanArena` 放在 N2 定义的 `PlanCtx` 里，规则函数统一签名为 `fn(ctx: &mut PlanCtx, p: PlanId, ...)`。
4. **与 N3 的接口**：提取出的物理计划是 `Send + Sync` 的不可变 `Arc` 树，执行器 worker 可以直接共享，Go 的部分并发执行器会为每个 worker 复制表达式（原因待逐个核实，可能与求值时的可变缓存有关）；Rust 中是否还需复制，取决于表达式求值是否带可变状态，在 N4 中定。
