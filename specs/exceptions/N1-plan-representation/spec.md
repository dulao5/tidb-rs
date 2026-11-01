# N1：计划树和表达式树的表示与所有权（基础设计 spec，草案）

状态：草案。类型：实现例外（基础设计）。计划定稿：P1 之前。

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
4. **物理计划用拥有型的不可变树**（`Arc` 节点），在逻辑优化结束、`FindBestTask` 选出最优方案时从 arena 中"提取"出来。这样它可以脱离语句的生命周期，直接进 plan cache，`Clone` 变成引用计数加一（对应 Go 的生成代码，按 L2 登记）。
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
ctx.arena.sel_mut(p).conditions = propagate_constant(expr_ctx, ret_conditions);
```

逻辑步骤、分支和返回值与 Go 相同。区别只在"如何取到节点"，所以整体登记为 L2，并把这一条映射规则写进 plan 模板，供每个块直接引用。

## 5. 验收

- 在 spec-kit 试点块上实际使用（建议 D09 的列裁剪 + 投影消除，正好覆盖原地修改、子节点替换、schema 修改三种写法）。
- 指标：sysbench 只读语句和 TPC-H 22 条查询的优化耗时不高于 Go；每条语句优化结束后没有残留分配（arena 整体释放）。
- 共享表达式的审计清单完成，每处都有结论。

## 6. 原待定项的结论

1. **arena 按语句划分，不按 optimize 调用划分**。v8.5.8 在建计划的过程中会递归调用优化器：不相关子查询在表达式改写时就先优化并执行（`expression_rewriter.go` L1100、L1394 调 `DoOptimize`），`INSERT ... SELECT`、`planbuilder.go` L769/L4679/L5030 同样如此，而且都复用同一个 `builder.ctx`。所以一条语句只有一个 `PlanArena`，嵌套的 `DoOptimize` 共用它；物理计划在每次 `DoOptimize` 结束时提取成 `Arc` 树，不再引用 arena。arena 随语句的规划阶段结束而释放。
2. **trait 对象，不用枚举**。arena 中存 `Box<dyn LogicalPlan>`，用 `as_any()` 向下转型对应 Go 的类型断言（`p.(*LogicalSelection)`）。理由：可以按块逐个算子增量移植；Go 代码中大量的类型断言和类型 switch 能逐行对应。性能敏感的地方（规则遍历）如果测得虚调用成为瓶颈，再收敛为枚举，届时只影响 arena 内部。
3. **与 N2 的接口**：`PlanArena` 放在 N2 定义的 `PlanCtx` 里，规则函数统一签名为 `fn(ctx: &mut PlanCtx, p: PlanId, ...)`。
4. **与 N3 的接口**：提取出的物理计划是 `Send + Sync` 的不可变 `Arc` 树，执行器 worker 可以直接共享，Go 的部分并发执行器会为每个 worker 复制表达式（原因待逐个核实，可能与求值时的可变缓存有关）；Rust 中是否还需复制，取决于表达式求值是否带可变状态，在 N4 中定。
