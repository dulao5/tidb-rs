# D09.7 设计试点报告：验证 N1 计划表示

状态：试点完成（2026-11-13），供 N1 定稿评审；同日按 #147 评审意见（Lucky）修订：E3 在真实 Go v8.5.8 上复现，FieldType 方法调用式修改补审计（附录 B），M-FT 给出推荐方案。类型：spike，**不合入主线，不计入覆盖率**（方法论 §8、ROADMAP P0 第 3 项）。回应 #128 第一节第 4 条。

代码位置：`crates/spike/planner-pilot`（crate `tidb-planner-pilot`）。在 `layers.toml` 中登记为 rank 13（与规划中的 `tidb-planner` 同层），`blocks = []`，D09.7 的函数仍属于 `tidb-planner`，不计为已移植。`tools/first-party.sh` 增加了 `crates/spike/*`，所以 fmt 和 clippy 会检查它。删除试点时，连同这两处登记一起删。

## 0. 结论先行

**N1 的方向可以确认，但需要修订后定稿，修订项见第 6 节。** 按 spec 写，L1 规则代码能与 Go 逐行对照，借用检查器带来的偏离都可以用固定写法机械处理。试点发现了 N1 没有覆盖的三件事：

1. **固定写法应改为"摘出节点再分派"，不是 `PlanCtx::split()`**（§2、§4）。规则方法在 `&mut self` 上运行，节点先从 arena 中暂时摘出，Go 的 `p.Conditions = ...`、`p.Children()[0] = ...` 都能原样写。整个试点没有一处用到 `split()`。
2. **共享修改的审计范围要扩大到 `*FieldType` 和 `*Schema` 的别名**（§3）。Go 的 `Column.Clone()` 是浅拷贝，`RetType` 指针仍然共享。所以 `GetType().SetFlag(...)` 这类修改在 Go 中对整个查询可见，在 Rust 中不可见。
   - **已在真实的 Go v8.5.8 上复现，而且比预想严重**：memory table 的列直接指向进程全局的 infoschema 定义，所以对 `information_schema` 表执行一条 E3 形状的查询，或者一条普通的 `FIRST_VALUE(col) OVER ()`，之后**该 tidb-server 上所有查询**的计划都会变（§3.3，已登记为 upstream-pitfall #148）。
   - **审计结果**：44 处 `.Index =`/`.RetType =` 中，只有 `rule_partition_processor.go:157` 依赖修改的可见性（附录 A）。FieldType 方法调用式的原地修改有 156 处，其中"共享且可见性有影响"的按机制合并为 10 组（附录 B），已实测确认 2 组。
   - **M-FT 推荐方案 (a)**：默认写时复制。只在 Go 的别名会改变 EXPLAIN 能看到的计划时，在该处显式模拟（§6 第 3 条）。
3. **物理计划建议从一开始就用 `Arc`，不分 `Box` 阶段和冻结阶段**（§5）。语句独占时用 `Arc::get_mut` 原地改；共享后（plan cache、记忆化的子计划）写时复制。plan cache 命中后的重建用路径复制，比 Go 每次深拷贝整棵计划便宜。只有实例级 plan cache 时，这个设计可以支持。

## 1. 范围与交付物

| 内容 | 位置 |
|---|---|
| 逻辑计划 arena、`PlanId`、摘出分派 | `src/arena.rs` |
| `LogicalPlan` trait：Go 接口方法，`BaseLogicalPlan`、`LogicalSchemaProducer` 的方法写成默认方法 | `src/base.rs`、`src/logicalop/mod.rs` |
| `PlanCtx`：arena、`&mut SessionVars`、`ExprContextRef`；`split()` 只保留作对照 | `src/planctx.rs` |
| `PruneColumns`（L1）：DataSource、Selection、Projection、Limit、Sort、TopN、Aggregation、Join（含 `ExtractUsedCols`、`MergeSchema`、`BuildLogicalJoinSchema`）、TableDual，以及 `InlineProjection`、`pruneByItems`、`ResetNotNullFlag` | `src/logicalop/*.rs`、`src/util.rs` |
| `ColumnPruner`、`noZeroColumnLayOut` | `src/rule_column_pruning.rs` |
| 投影消除：逻辑侧 `eliminate`、`ReplaceColumnOfExpr`、`ResolveExprAndReplace`；物理侧 `eliminatePhysicalProjection` | `src/rule_eliminate_projection.rs`、`src/rule_util.rs` |
| 物理计划 `Arc` 树、`make_mut_plan`、`map_children` | `src/physicalop.rs` |
| plan cache 范围重建（路径复制） | `src/plan_cache_rebuild.rs` |
| 测试：11 + 2（另 1 个忽略）+ 3 | `tests/*.rs` |

**边界桩**（`blocked-by`）：

| 桩 | 依赖块 |
|---|---|
| `AggFuncDesc`（只实现 count、firstrow） | D08.11 |
| `HandleCols` | D09.x |
| 优化器 trace，只记录字符串 | D09.13 |
| ranger，作为参数传入 | D10.4 |
| `IF` 函数类 | D08.9，导致 E3 的测试暂时忽略 |

**测试依据**：Go 的金标准输出 `planner/core/testdata/plan_suite_unexported_out.json`。

- `TestColumnPruning` 取了 7 条，`TestSortByItemsPruning` 取了 1 条；
- `TestProjectionEliminator` 取了第 1 条；
- 另有 5 个按 Go 代码推导期望值的测试：外连接 NotNull、dual 上的投影、trace、COW 共享、物理计划与 plan cache。

没有 planbuilder，计划由测试按 Go `PlanBuilder` 的形状和 plan id 顺序手工构造。金标准里的 plan id（如 `"4": ["test.t.b"]`）因此能对上。

**没有做的**：

- Apply、UnionAll、Window、CTE、Expand、Lock、MemTable、Sequence、UnionScan 的 `PruneColumns`。它们和已移植的几种是同一个形状。
- MPP 和 TiFlash 分支。
- FindBestTask 与 `taskMap` 旁表：D09.7 用不到，N1 §3.2 **没有被本试点验证**。
- N1 §5 的性能指标：没有 planbuilder，无法测端到端的优化耗时。
- govec 向量。

## 2. 问题 1：arena + PlanId + trait 默认方法，能否保持逐行对照

**能。** 关键在分派方式。Go 的 `child.PruneColumns(cols, opt)` 写成自由函数 `prune_columns(ctx, child, cols, opt)`：先用 `PlanArena::take` 把节点从 arena 中暂时摘出，以 `&mut self` 调用 trait 方法，再用 `put` 放回。

方法体内，`self`（节点）和 `ctx`（含 arena）是两个互不重叠的借用，所以：

- Go 的 `p.Field = ...` 写成 `self.field = ...`；
- `p.Children()[0], err = child.PruneColumns(...)` 写成 `self.children_mut()[0] = prune_columns(ctx, child, ...)?;`。Rust 的赋值表达式先求值右侧，所以这一行能编译。

### 2.1 对照一：`LogicalSelection.PruneColumns`（最常见的形状）

```go
func (p *LogicalSelection) PruneColumns(parentUsedCols []*expression.Column, opt *optimizetrace.LogicalOptimizeOp) (base.LogicalPlan, error) {
	child := p.Children()[0]
	parentUsedCols = expression.ExtractColumnsFromExpressions(parentUsedCols, p.Conditions, nil)
	var err error
	p.Children()[0], err = child.PruneColumns(parentUsedCols, opt)
	if err != nil {
		return nil, err
	}
	return p, nil
}
```

```rust
// go: pkg/planner/core/operator/logicalop/logical_selection.go (*LogicalSelection).PruneColumns L1 [M-PLAN]
fn prune_columns(&mut self, ctx: &mut PlanCtx<'_>, this: PlanId,
                 mut parent_used_cols: Vec<Arc<Column>>, opt: &mut LogicalOptimizeOp) -> Result<PlanId, Error> {
    let child = self.children()[0];
    parent_used_cols = extract_columns_from_expressions(parent_used_cols, &self.conditions, None);
    self.children_mut()[0] = prune_columns(ctx, child, parent_used_cols, opt)?;
    Ok(this)
}
```

与 Go 的差别只有两处：`return p` 写成 `Ok(this)`，方法要多接收一个参数 `this`，即自己的 `PlanId`；`if err != nil` 写成 `?`。

### 2.2 对照二：`LogicalLimit.PruneColumns`（带副作用的 getter，以及 slice 别名）

```go
	savedUsedCols := make([]*expression.Column, len(parentUsedCols))
	copy(savedUsedCols, parentUsedCols)
	var err error
	if p.Children()[0], err = p.Children()[0].PruneColumns(parentUsedCols, opt); err != nil {
		return nil, err
	}
	p.SetSchema(nil)
	p.InlineProjection(savedUsedCols, opt)
	return p, nil
```

```rust
    let saved_used_cols = parent_used_cols.clone();
    self.children_mut()[0] = prune_columns(ctx, self.children()[0], parent_used_cols, opt)?;
    self.sp.set_schema(None);
    self.inline_projection(ctx, saved_used_cols, opt);
    Ok(this)
```

这段代码同时涉及三个 Go 惯用法：

- **getter 有副作用**：`SetSchema(nil)` 之后，`InlineProjection` 里的 `s.Schema()` 会从子节点克隆一份 schema 并保存下来。Rust 用 `OnceCell<Schema>` 承载，读路径仍然是 `&self`。
- **继承链**：`InlineProjection` 是 `LogicalSchemaProducer` 的方法，又经 `s.Self()` 回调具体算子（trace 用），所以写成 trait 默认方法（`M-SELF`）。
- **slice 别名**：Go 先复制 `savedUsedCols`，是因为子节点可能往 `parentUsedCols` 后面 append，改到同一底层数组。Rust 按值传 `Vec`，复制是显式的，语义相同。

### 2.3 对照三：`ProjectionEliminator.eliminate` 合并相邻投影（最差的情况）

```go
	if isProj {
		if child, ok := p.Children()[0].(*logicalop.LogicalProjection); ok && !expression.ExprsHasSideEffects(child.Exprs) {
			ctx := p.SCtx()
			for i := range proj.Exprs {
				proj.Exprs[i] = ReplaceColumnOfExpr(proj.Exprs[i], child, child.Schema())
				foldedExpr := expression.FoldConstant(ctx.GetExprCtx(), proj.Exprs[i])
				foldedExpr.GetType(...).SetFlag((foldedExpr.GetType(...).GetFlag() & ^mysql.NotNullFlag) | (proj.Exprs[i].GetType(...).GetFlag() & mysql.NotNullFlag))
				proj.Exprs[i] = foldedExpr
			}
			p.Children()[0] = child.Children()[0]
```

```rust
        if is_proj {
            let child = ctx.arena.get(p).children()[0];
            if let Some(child_proj) = ctx.arena.downcast_ref::<LogicalProjection>(child)
                && !exprs_has_side_effects(&child_proj.exprs)
            {
                with_detached(ctx, p, |node, ctx| {          // N1：同时要可变的 proj 和只读的 child
                    let proj = node.as_any_mut().downcast_mut::<LogicalProjection>().expect("proj");
                    let child = ctx.arena.downcast_ref::<LogicalProjection>(child).expect("child proj");
                    let ectx = ctx.eval_ctx();
                    for i in 0..proj.exprs.len() {
                        replace_column_of_expr(&mut proj.exprs[i], child, child.schema(&ctx.arena));
                        let mut folded_expr = fold_constant(&*ctx.expr_ctx, proj.exprs[i].clone());
                        let flag = (folded_expr.get_type(&*ectx).get_flag() & !NotNullFlag)
                            | (proj.exprs[i].get_type(&*ectx).get_flag() & NotNullFlag);
                        set_ret_type_flag(&mut folded_expr, flag);   // 审计项 E3：写时复制
                        proj.exprs[i] = folded_expr;
                    }
                    proj.children_mut()[0] = child.children()[0];
```

`eliminate` 不是算子的方法，而是一个遍历整棵树、并同时操作父子两个节点的自由函数，这是 arena 写法最啰嗦的情况：

- 需要一次 `with_detached`，加两次 downcast；
- Join 的 schema 要先用 `&arena` 算出来，再用 `&mut arena` 写回；
- 读子节点列表时只能按下标循环，因为递归调用要借用整个 `ctx`。

**每处多出的行都是固定写法，Go 原来的分支和步骤一处没少。**

### 2.4 量化

行数统计：非空、非注释行，rustfmt 之后。

| 函数 | Go | Rust | 说明 |
|---|--:|--:|---|
| BaseLogicalPlan.PruneColumns | 9 | 5 | |
| LogicalSelection.PruneColumns | 8 | 5 | |
| LogicalLimit / LogicalSort.PruneColumns | 9 / 9 | 5 / 5 | |
| LogicalTableDual.PruneColumns | 10 | 10 | |
| LogicalJoin.PruneColumns | 17 | 17 | |
| LogicalJoin.ExtractUsedCols | 36 | 34 | |
| InlineProjection | 23 | 24 | |
| pruneByItems | 28 | 27 | |
| LogicalProjection.PruneColumns | 42 | 45 | |
| LogicalAggregation.PruneColumns | 69 | 83 | 主要是 rustfmt 折行 |
| DataSource.PruneColumns | 46 | 70 | rustfmt 折行，以及 MPP 分支新建投影 |
| ProjectionEliminator.eliminate | 48 | 80 | 自由函数，见 2.3 |
| ReplaceColumnOfExpr | 12 | 21 | 写时复制，只在参数变化时复制 |
| doPhysicalProjectionElimination | 25 | 18 | |
| **合计（16 个函数）** | **400** | **457** | **1.14 倍** |

作为算子方法的规则，Rust 行数约为 Go 的 1.0 倍。遍历树的自由函数规则约为 1.5 至 1.7 倍。

### 2.5 用到的映射（建议写进 plan 模板）

| Go 写法 | Rust 写法 | 规则 |
|---|---|---|
| `x.Method(...)`，x 是 arena 中的节点 | 同名自由函数，内部摘出节点后调用 trait 方法 | M-PLAN |
| `return p` | 返回 `Ok(this)`，方法多一个参数 `this: PlanId` | M-PLAN |
| `p.(*T)` / `switch p.(type)` | `arena.downcast_ref::<T>(id)` / `arena.get(id).kind()` | M-IFACE |
| struct 嵌入（`LogicalProjection` 嵌 `LogicalSchemaProducer`，后者嵌 `BaseLogicalPlan`） | `base()`、`schema_producer()` 两个访问器，由宏生成；嵌入结构的方法写成 trait 默认方法 | M-SELF |
| `BaseLogicalPlan.Schema()` 返回子节点的 schema | 默认方法返回子节点 schema 的引用。Go 用指针比较 `p.Schema() == child.Schema()`，Rust 用 `std::ptr::eq` 判断，结果相同 | M-PLAN |
| 有副作用的 getter（惰性初始化 schema） | `OnceCell` | M-PLAN |
| 往参数 slice 上 append（与调用方共享底层数组） | 参数按值传 `Vec`；调用方之后还要用的，先 clone。试点逐处核对过等价性，见下 | — |

**`DataSource.PruneColumns` 的 slice 别名，已核对等价**：`originSchemaColumns` 和原 schema 共用底层数组，原地删除会挪动数组里的元素。但 `preferKeyColumnFromTable` 只在所有列都被删光时才读它；从尾部依次删光时，Go 不挪动任何元素。所以复制一份是等价的。

## 3. 问题 2：共享表达式的原地修改

### 3.1 本规则涉及的修改

| 编号 | Go 中的修改 | 对象是否共享 | Rust 的处理 | 结论 |
|---|---|---|---|---|
| E1 | `p.Schema().Columns = append(...[:i], ...[i+1:]...)`：各算子删列 | 改的是 `*Schema`，不是表达式。Go 中 Selection、Sort 的 `Schema()` 就是子节点的 schema | 每个节点拥有自己的 `Schema`；不拥有 schema 的节点写到拥有者那里（`PlanArena::schema_mut`） | 等价 |
| E2 | `ResolveExprAndReplace` 和 `ReplaceColumnOfExpr` 原地替换 `ScalarFunction` 的参数（`GetArgs()[i] = ...`） | 推断不共享：函数节点由 rewriter 为本节点新建 | 把表达式当作槽位（`&mut ExprRef`），写时复制：只在参数确实变化时 `Arc::make_mut`；槽位独占时原地改（与 Go 相同），共享时复制 | 等价。Go 和 Rust 都保留旧的 hashcode 缓存（Go 不清缓存，Rust 的 `make_mut` 连缓存一起复制），行为一致。测试 `eliminates_column_projection_under_aggregation` |
| E3 | `FoldConstant` 之后，`foldedExpr.GetType().SetFlag(...)`（`rule_eliminate_projection.go:201`） | **共享**：折叠结果可能就是数据源的那个 `*Column`，且 `FieldType` 指针在全查询共享 | 写时复制，只改投影里的那一份 | **与 Go 不同，见 3.3**。测试 `fold_then_set_flag_is_copy_on_write`，等 D08.9 的 `IF` 函数后启用 |
| E4 | `ResetNotNullFlag` | Go 已经先复制再改 | 相同 | 等价 |
| E5 | 投影下面是 dual 时：`p.Exprs[0] = NewZero()`，并新建输出列 | 新对象 | 替换槽位 | 等价 |
| E6 | `resolveColumnAndReplace`：Clone 列后改 `RetType`、`InOperand` | Clone 后的新列，但 `RetType` 指针仍与原列共享 | 新列，`ret_type` 按值复制 | 共享的 `FieldType` 之后不再被修改时等价，见 3.3 |

**代价**：

- 写时复制只在节点被共享、且确实要改时才发生。复制一个 `ScalarFunction` 要复制它的 `BuiltinFunc`（`clone_from_base`），这是本规则最贵的一步，只出现在 E2 的共享分支。
- 更常见的开销来自表示不一致。schema 里是 `Arc<Column>`，表达式里是 `Expr::Column(Column)`（列直接内嵌在节点里）。所以 `ExtractColumns`、`Column2Exprs`，以及消除投影时往 replace map 里放列，每次都要分配一个新的 `Arc`。Go 在这些地方不分配内存。

  试点没有测这部分的绝对开销；它与规则涉及的列引用数成正比。

### 3.2 全部 44 处 `.Index =` / `.RetType =` 的审计（N1 §5 验收项）

逐处结论（文件:行号、来源、结论、所属块）见附录 A。汇总如下：

| 结论 | 数量 | 说明 |
|---|--:|---|
| 新建或刚 Clone 的对象（安全） | 34 | 写时复制，或者本来就是新值 |
| 非表达式字段（`AccessPath.Index`、`IndexInfo`） | 5 | 与 N1 无关 |
| 共享但不依赖可见性 | 4 | 例如 `rule_inject_extra_projection.go:93` 的赋值是幂等的；`resolve_indices.go:837` 之后，父节点会按 UniqueID 重新计算 |
| **共享且依赖可见性** | **1** | `rule_partition_processor.go:157`：改 `partCols[i].Index`，期望改动在 `hashExpr` 中生效（L190 的 `hashExpr.EvalInt(row)`）。Rust 要按 UniqueID 映射到位置，重写出一个新的 `hashExpr` |

`resolve_indices.go` 的结论：Go **不靠**指针共享来修正父节点表达式的 `Index`。父节点总是先 Clone，再按 UniqueID 重新计算。Join 和 Limit 甚至会先浅拷贝 slice。物理节点构造时复制一份 schema，`ResolveIndices` 只改自己那份即可。

### 3.3 N1 没覆盖的：`*FieldType` 别名

Go 的 `Column.Clone()` 是 `newCol := *col`，`RetType` 指针仍然共享。以下操作都只复制列，不复制 `FieldType`：

- `Schema.Clone`、`MergeSchema`；
- `buildProjectionField`：直接用 `RetType: expr.GetType()`；
- `firstrow(col)`：`RetTp` 就是 `col.RetType`。

于是同一源列的 `FieldType`，在整条查询里实际上是**一个共享的可变对象**。Rust 的 `Column.ret_type` 是按值保存的（D08.1 已经这样定了），这种别名不存在。

**E3 的具体例子**（代码链条已由 #147 评审逐步核实；普通表上的单查询影响未能用 EXPLAIN 观察到，memory table 上的全局影响已实测，见 3.3.1）：

建表 `t(a INT NOT NULL, b INT)`，查询 `SELECT IF(x,a,b) FROM (SELECT 1 x, a, b FROM t) s`。

1. 合并投影后得到 `IF(1, a_ds, b_ds)`。
2. 常量折叠返回数据源的列 `a_ds`。
3. 第 201 行把 IF 结果的可空性写到 `a_ds.RetType` 上。

于是 Go 中数据源、TableScan 和所有引用 a 的列都失去了 NOT NULL；Rust 中只有投影里的那份改了。

再往上加一层 `max(a)`，之后的 MaxMinEliminator（`rule_max_min_eliminate.go:190`）在 Go 中会多出一个 `not(isnull(a))` 的 Selection，Rust 不会。EXPLAIN 会不同。

ProjectionEliminator 之后读取 NotNull 的规则还有：PPD（`logical_join.go:1320`）、PartitionProcessor、CorrelateSolver。

**已有先例**：expression crate 对同一类问题已经分别做过两种选择。

- `PropagateType` 选择了复制（D08.1 spec §4，"a shared argument node is copied"）。
- `FoldConstant` 里的 `SetCoercibility` 选择了保持共享（collationInfo 的 coercibility 是原子量，"a shared node is changed in place, as in Go"）。

这说明需要一条**全局规则**，而不是每个块各自决定。

#### 3.3.1 在 Go v8.5.8 上实测（2026-11-13）

**环境**：`tiup playground v8.5.8 --tag d097-e3 --port-offset 29000 --without-monitor`，`tidb_version()` 显示 commit `8b857efa2`。实测后已清理。

**为什么 memory table 是全局的**：`buildMemTable` 用 `RetType: &col.FieldType` 建列（`logical_plan_builder.go:5510`），指向进程全局的 infoschema `TableInfo`。普通表则按查询 `col.FieldType.Clone()`（`:5346`）。

测试用的表是 `information_schema.KEY_COLUMN_USAGE`：`ORDINAL_POSITION`、`TABLE_NAME` 为 NOT NULL，`POSITION_IN_UNIQUE_CONSTRAINT` 可空（`infoschema/tables.go:563-576`）。

**第 1 步，触发前**（每条都是新连接）：

```
EXPLAIN SELECT ORDINAL_POSITION FROM information_schema.KEY_COLUMN_USAGE WHERE ORDINAL_POSITION IS NULL;
TableDual_6	0.00	root		rows:0

EXPLAIN SELECT MAX(ORDINAL_POSITION) FROM information_schema.KEY_COLUMN_USAGE;
StreamAgg_9	1.00	root		funcs:max(Column#8)->Column#13
└─TopN_10	1.00	root		Column#8:desc, offset:0, count:1
  └─MemTableScan_17	10000.00	root	table:KEY_COLUMN_USAGE
```

**第 2 步，执行一次 E3 形状的查询**（任意会话）：

```sql
SELECT IF(x, ORDINAL_POSITION, POSITION_IN_UNIQUE_CONSTRAINT) AS y
FROM (SELECT 1 AS x, ORDINAL_POSITION, POSITION_IN_UNIQUE_CONSTRAINT
      FROM information_schema.KEY_COLUMN_USAGE) s LIMIT 1;
```

它的计划是 `Limit_8 → MemTableScan_11`：两层投影已被合并消除，第 201 行确实执行到了。

**第 3 步，新连接再查**，计划变了：

```
EXPLAIN SELECT ORDINAL_POSITION FROM information_schema.KEY_COLUMN_USAGE WHERE ORDINAL_POSITION IS NULL;
Projection_4	8000.00	root		Column#8
└─Selection_5	8000.00	root		isnull(Column#8)
  └─MemTableScan_6	10000.00	root	table:KEY_COLUMN_USAGE

EXPLAIN SELECT MAX(ORDINAL_POSITION) FROM information_schema.KEY_COLUMN_USAGE;
StreamAgg_10	1.00	root		funcs:max(Column#8)->Column#13
└─TopN_13	1.00	root		Column#8:desc, offset:0, count:1
  └─Selection_18	8000.00	root		not(isnull(Column#8))
    └─MemTableScan_19	10000.00	root	table:KEY_COLUMN_USAGE
```

**第 4 步，确认影响不会自行消失**：

- 执行一次 DDL（`CREATE DATABASE d097; CREATE TABLE d097.t(a INT NOT NULL PRIMARY KEY, b INT)`），schema version 变了，第 3 步的计划仍然保持不变。
- 用普通表 `d097.t` 执行同样形状的查询作对照，之后 `EXPLAIN SELECT a FROM d097.t WHERE a IS NULL` 前后都是 `TableDual`：对普通表，影响只限于当次查询。

**第 5 步，另一条 V1 也能复现**（附录 B 第 5 组，`window_func.go:76`），而且触发它只需要一条普通的窗口函数查询：

```
EXPLAIN SELECT TABLE_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_NAME IS NULL;   -- TableDual_6
SELECT FIRST_VALUE(TABLE_NAME) OVER () FROM information_schema.KEY_COLUMN_USAGE LIMIT 1;
EXPLAIN SELECT TABLE_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_NAME IS NULL;   -- Projection_4 → Selection_5 isnull(Column#6) → MemTableScan_6
```

**没有复现成功的**：

- **E3 在普通表上的单查询影响**：`EXPLAIN SELECT MAX(a) FROM (SELECT IF(x,a,b) AS y, a FROM (SELECT 1 AS x, a, b FROM d097.t) s) s2 WHERE y > 0`，与把 `IF` 换成 `a` 的对照查询，计划相同，都是主键倒序 RangeScan 加 Limit，没有 Selection。a 是主键，range 扫描已经隐含非空，所以看不出差别。第 255 行的推断仍然是推断。
- **`IF(1, CONSTRAINT_NAME, NULL)` 走的 `constant_fold.go:46/47`**：它改的是 charset/collation，不是 NotNull，`IS NULL` 观察不到，所以既没有确认也没有否定。

**结论**：这是上游 bug。它只影响计划质量，没有观察到错误结果；但一台 tidb-server 的计划会依赖它此前执行过哪些查询，不可复现。已登记为 #148（upstream-pitfall），没有向 pingcap 仓库提交。

**对 tidb-rs 的含义**：Rust 不可能、也不应该复现这种全局污染。这是推荐 M-FT 方案 (a) 的直接论据，见 §6。

### 3.4 `*Schema` 别名

planner/core 中约 33 处 `SetSchema(x.Schema())` 不做 Clone，直接共享指针。逐处结论：

- **逻辑侧**：大多是"新节点取代旧节点、旧节点被丢弃"，实际是所有权转移，Rust 中用 move 或 clone 即可。
- **唯一的例外**：`logical_plan_builder.go:2204` 中 INTERSECT/EXCEPT 的半连接，与左侧的 distinct 聚合共享 schema。默认规则顺序下，ColumnPruner 的 `MergeSchema` 会重建 join 的 schema，别名在别的规则运行前就断开了。只有用 `opt_rule_blacklist` 禁用 `column_prune` 时，BuildKeySolver 清空 Keys 才会在 Go 中影响到聚合（推断）。
- **物理侧**：`ResolveIndices` 之前会先复制，安全。

## 4. 问题 3：`PlanCtx::split()` 与借用写法

**结论：`split()` 在试点中一次都没用上；所有偏离都是机械的。** 原因有三：

1. **摘出分派**让方法体中的 `self` 和 `ctx` 互不重叠。
2. `PlanCtx` 的字段是公开的。在同一个函数里，`ctx.arena` 和 `ctx.expr_ctx` 是不同字段的借用，编译器允许同时借用。`split()` 只在需要跨函数边界一次传出多个借用时才有用。
3. 已移植的 N4 表达式上下文是 `Arc<dyn ExprContext>`：warning、列 ID 分配器都是内部可变的，`SessionVars::alloc_plan_column_id` 用的是原子量。所以表达式上下文从来不需要 `&mut`。

`foundation-interfaces.md` 里的签名 `split() -> (&mut PlanArena, &mut EvalCtx)` 与实际的 N4 代码（`&dyn EvalContext`）不一致，需要修正。

试点中借用检查器迫使的全部偏离：

| 编号 | 场景 | 写法 | 次数 |
|---|---|---|--:|
| B1 | 调用 arena 中节点的方法，同时要 `&mut` 节点和 `&mut` arena | 摘出分派：`take` → 调用方法 → `put`。每个 Go 接口方法写一个分派函数 | 每个接口方法 1 个 |
| B2 | `self.schema()` 借用了整个 `self`，遍历 schema 的同时又往 `self` 的另一个字段 push | 通过嵌入字段路径取 schema（`self.sp.schema(&self.base.children, ...)`） | 1（DataSource） |
| B3 | `f(&mut x, x.len())` | 先把 `x.len()` 取到局部变量 | 1（BuildLogicalJoinSchema） |
| B4 | 自由函数同时操作父子两个节点 | 摘出父节点；或先用 `&arena` 计算、再用 `&mut arena` 写回；按下标遍历子节点 | 3（eliminate） |
| B5 | 物理计划 `Arc` 树上的 `p.Children()[i] = f(child)` | `map_children`：节点独占时把子节点移出再放回，共享时只在子节点变化时复制。直接 `child.clone()` 会让每个子节点看起来都被共享，从而被复制（试点第一版就犯了这个错，测试发现的） | 2 |
| B6 | Go 往参数 slice 上 append | 按值传 `Vec`，需要时 clone | 2（Limit、Join） |

**摘出分派的约束**：节点被摘出期间，再访问它会 panic。v8.5.8 的树规则不会在一次分派中两次访问同一节点。CTE 的子计划由 `LogicalCTE` 独立优化，不在同一次遍历里。但 D09.6（去关联）和 join reorder 移植时，需要按块核对这一点。

## 5. 问题 4：物理计划冻结与实例级 plan cache

**结论：能支持，而且比 Go 便宜。**

Go 的实例级 plan cache：

1. Put 时深拷贝一次（`CloneForPlanCache`）；
2. **每次命中**再深拷贝一次，包括 `util.CloneExpressions` 克隆所有表达式（`PlanCacheValue.CloneForInstancePlanCache`）；
3. 然后在这份私有副本上原地 `rebuildRange`。

Rust 的做法：

1. 缓存里保存一棵 `Arc` 树，各会话只读共享；
2. 命中时按本次参数重建 range，只复制从根到被改写的 scan 之间路径上的节点（浅拷贝），表达式和 schema 仍然共享。

测试 `plan_cache_rebuild_by_path_copy` 用 4 个线程并发重建同一份缓存计划，验证了三点：各线程结果正确；缓存中的原计划不变；投影的表达式和 schema 列在新旧计划之间是同一个 `Arc`。

试点还得出两个设计建议：

- **建议 P2：物理计划从一开始就用 `Arc`**，取代 N1 §3.4 的"先 `Box`，`DoOptimize` 结束时再冻结"。
  - 理由：FindBestTask 用 `taskMap` 记忆化每个逻辑节点的最优 task，同一个子计划会挂到多个父候选下。如果用 `Box`，独占所有权意味着每挂一次就要深拷贝。用 `Arc`，落选的候选被丢弃后，子计划的引用计数回到 1，`postOptimize` 可以原地修改。
  - 测试 `memo_shared_child_is_copied_only_while_shared` 和 `post_optimize_in_place_on_owned_tree` 演示了这一点（推断：试点没有移植 FindBestTask）。
  - 这样也不需要单独的冻结步骤：`Arc<dyn PhysicalPlan>` 本身就是 `Send + Sync`，试点中有静态断言。
- **路径复制的实际范围**：range 每次都会重建，所以根到每个 reader 的路径都会被复制，通常就是 root task 中的大部分节点。但这些都是浅拷贝（`Vec<ExprRef>` 的 clone 只是引用计数加一），仍然远低于 Go 的深拷贝。

**风险与待办**（交给 A3 spec）：

1. **计划节点不能保存会话上下文**。Go 的 `p.SCtx()`、`PointGetPlan.ctx` 在 Clone 时会换成新会话的上下文。Rust 中凡是要用上下文的方法，都必须由调用方传入（N2）。
2. **Go 有执行期修改计划的情况**：`generateNewPlan` 的注释写着 "execution (might modify the plan)"。冻结设计禁止这种修改，D12 移植时要逐处找出来。
3. **表达式求值的缓存状态（N4）**：`BuiltinFuncCache` 是按 `ctx_id` 缓存、用 Mutex 保护的。多个会话共享同一个表达式时，会争用这把锁，并且因为 `ctx_id` 不同而互相冲掉缓存。实例级 plan cache 打开前，需要在 N4 或 A3 中决定这件事（改为按执行状态缓存，或者命中时复制带缓存的函数节点）。
4. PointGet 和 BatchPointGet 的 `Handle` / `IndexValues` 也会被改写，形状与 `PhysicalTableScan.Ranges` 相同。试点只演示了 TableScan。

## 6. 结论与 N1 修订建议

**确认 N1 的以下部分**：

- 逻辑计划用语句级 arena 加 `PlanId`；
- Go 的 self 字段与嵌入写成 trait 默认方法；
- 表达式用 `Arc<Expr>`，写时复制；
- 物理计划不可变、可以共享，以支持 plan cache；
- `PlanId` 与 Go 的 plan id 分开（试点按 Go 的分配顺序分配 id，金标准里的 id 都能对上）。

**修订建议**：

1. **§4 的固定写法改为"摘出分派"**：每个 Go 接口方法对应一个同名自由函数加一个 `&mut self` trait 方法，方法多接收一个 `this: PlanId`。`split()` 降为备用写法。同时修正 `foundation-interfaces.md` 中 `split()` 和 `EvalCtx` 的签名，与已移植的 N4 一致：表达式上下文是共享引用（`&dyn`）。
2. **§3.4 改为"物理计划从一开始就是 `Arc` 树"**：`make_mut_plan`（独占时原地改，共享时浅拷贝）和 `map_children` 写进映射规则 `M-PLAN`，删掉"`Box` 阶段加冻结"。§3.5 的路径复制沿用同一套机制。
3. **§1 和 §3.3 的审计范围扩大到 `*FieldType` 和 `*Schema` 的别名**，并新增一条映射规则（建议编号 `M-FT`），规定"对 `GetType()` 返回的 `FieldType` 原地修改"怎么处理。两个候选：
   - (a) 默认写时复制，凡是 Go 改变了共享对象的地方，逐处登记为已知差异，并配回归测试；
   - (b) `Column.ret_type` 改为 `Arc<FieldType>`，保留指针身份，同时为"对所有共享者生效的修改"提供一个显式操作。

   **推荐 (a)，并做一处细化**（试点作者、Lucky 与协调人一致；待项目负责人确认）：
   - **默认写时复制**。Go 的这种别名是偶然形成的，不是设计：对普通表它只是降低计划质量，对 memory table 它会污染全局状态（§3.3.1，#148）。`Column.ret_type` 保持按值保存，**不改成 `Arc<FieldType>`**，保留 D08.1 已定的表示。
   - **逐处显式模拟**：只有当 Go 的别名会改变 golden test 或 EXPLAIN 对照能看到的计划时，才在那一处模拟 Go 的效果。例如 E3 遇上 MaxMinEliminator：除了改投影里的副本，还按 UniqueID 同步改同一查询里来源列的类型。写法固定为：
     - 加注释 `// Go aliasing (M-FT): <site>, <effect>`；
     - 配一个回归测试；
     - 在该块 plan 的"已知差异/模拟"表里登记。
   - **不模拟跨查询的全局效果**（memory table 的情况）。它属于上游 bug，按 upstream-pitfall 处理。
   - 这样每一处差异都能 grep 到（`M-FT`），由项目负责人逐处接受或否决。
   - **工作量**：附录 B 中 V1 的数量是 planner 4 处、expression 9 处，按机制合并为 10 组。逐处模拟的成本是有限的，而 (b) 要改动所有表达式类型的表示。
   - **每块 plan 模板加一项**：运行附录 B 的方法调用 grep，以及附录 A 的赋值 grep，逐处给出 S/V0/V1 结论。
   - 另外，`rule_partition_processor.go:157` 要在 PartitionProcessor 块（D09.12）的 plan 里写明怎么改写。
4. **§3 补充 `M-SELF` 的细节**：嵌入链用 `base()` / `schema_producer()` 两个访问器加宏；有副作用的 getter 用 `OnceCell`；Go 的指针比较 `p.Schema() == child.Schema()` 对应引用比较。
5. **N4（另一份 spec）的成本项**：schema 里是 `Arc<Column>`，表达式里是内嵌的 `Column`，两种表示之间每转换一次都要分配。规划器全面移植前，应该测一下这个开销，必要时改成 `Expr::Column(Arc<Column>)`。另外，表达式缓存在跨会话共享时的语义（§5 风险 3）也要在 N4 中定。
6. **§5 验收指标仍未测**：要等 planbuilder（D09.1）可用，才能测优化耗时与 Go 对比。§3.2 的 `taskMap` 旁表也没有被本试点覆盖，留给 D10.2。

### 对 P3 工作量的影响（估算）

依据只有 16 个函数、约 400 行 Go，误差较大。

- **按算子方法组织的规则**（PruneColumns、PredicatePushDown、DeriveStats、BuildKeyInfo 等）：Rust 行数约为 Go 的 1.0 至 1.15 倍，翻译是机械的。工作量与原计划基本相同。
- **遍历树的自由函数规则**（投影消除、去关联、join reorder、聚合下推等）：约 1.5 至 1.7 倍，每个函数多花 30% 至 60% 的时间。
- **新增的一次性工作**：每个块的 plan 都要列出本块对共享 `FieldType` 和 `Schema` 的原地修改，并逐处给出结论。审计方法见附录 A（赋值）和附录 B（方法调用）。全仓库的初筛已经完成：planner 101 处、expression 55 处，绝大多数是 S 类，所以分摊到每个块的审计只是复核加上处理 V1。按本试点的经验，大的块约多 1 至 2 天，V1 每处另加约半天（模拟加回归测试）。

合计：P3 比"纯逐行翻译"的基线多 **10% 至 20%**。N1 没有带来数量级的额外成本，**不需要换方案**。最大的不确定因素是 FindBestTask 和 `taskMap`（D10.2，5.9k 行），本试点没有覆盖。建议在 D10.2 开工时补一个小试点，只验证 `taskMap` 的回滚和 `Arc` 子计划共享。

## 附录 A：44 处 `.Index =` / `.RetType =` 逐处结论

块号取自 tracker `func_blocks.tsv` 中该文件函数最多的块（大文件按函数归属，个别行可能属于同文件的其他块）。来源：`grep -rn '\.Index = \|\.RetType = ' pkg/planner/core --include='*.go' | grep -v _test.go`（v8.5.8）。"推断"表示没有完整追到底。

| # | 位置 | 被改对象与来源 | 结论 | 块 |
|---|---|---|---|---|
| 1 | task.go:1485 | Column.RetType，`gbyCol.Clone()` | 安全 | D10.1 |
| 2–3 | task.go:1583、1588 | partial.Schema 的新列（L1385 新建） | 安全 | D10.1 |
| 4 | task.go:1672 | 新建的 ScalarFunction.RetType | 安全 | D10.1 |
| 5 | rule_aggregation_push_down.go:243 | 值拷贝 `arg := *col` | 安全 | D09.9 |
| 6 | rule_aggregation_push_down.go:355 | Column，Clone | 安全 | D09.9 |
| 7–8 | planbuilder.go:1177、1329 | AccessPath.Index | 非表达式字段 | planbuilder |
| 9–10 | planbuilder.go:4699、4700 | 新 Column、新 FieldType | 安全 | planbuilder（INSERT） |
| 11 | planbuilder.go:6213 | 新 Column | 安全 | planbuilder |
| 12 | pb_to_plan.go:94 | `Schema().Clone()` 的列 | 安全 | D10.9 |
| 13 | rule_inject_extra_projection.go:93 | 子节点 schema 的列，原地改，未 Clone | 共享但不依赖：ResolveIndices 之后 Index 已等于位置，赋值幂等（推断）。Rust 在新投影中放一个 Index=i 的新列 | D09.13 |
| 14、17 | rule_inject_extra_projection.go:239、332 | Clone | 安全 | D09.13 |
| 15–16 | rule_inject_extra_projection.go:253、303 | Clone，同一指针同时进 schema 和 Exprs | 安全（schema 列的 Index 不被读，推断） | D09.13 |
| 18 | physical_plans.go:863 | IndexInfo | 非表达式字段 | 计划 Clone（A3） |
| 19–20 | expression_rewriter.go:1118、1414 | 新建的 ScalarSubQueryExpr | 安全 | D09.2 |
| 21–22 | indexmerge_path.go:473、503 | AccessPath.Index | 非表达式字段 | D10.1 |
| 23 | indexmerge_path.go:1422 | Clone，`ArrayType()` 返回新类型（推断） | 安全 | D10.1 |
| 24 | resolve_indices.go:80 | 刚由 ResolveIndices 克隆的 Exprs[i] | 安全（Exprs slice 与逻辑投影共享，但最终每个逻辑节点只对应一个物理实例，推断） | D10.9 |
| 25–28 | resolve_indices.go:166、251、347、764 | 先浅拷贝 slice 再 Clone 列 | 安全 | D10.9 |
| 29 | resolve_indices.go:837 | TableScan schema 列，原地改。通常是 `ds.Schema().Clone()`；例外 find_best_task.go:2312 直接用 HandleCols 的列指针 | 共享但不依赖：父节点按 UniqueID 重算，多个 partial scan 写入同值 | D10.9 |
| 30 | fragment.go:475 | `Schema().Clone()` | 安全 | DEF.MPP |
| 31 | rule_decorrelate.go:269 | Clone | 安全 | D09.6 |
| 32 | rule_decorrelate.go:285 | 聚合参数里的 ScalarFunction，换成 Clone 后的 RetType | 共享但不依赖（循环处理全部 AggFuncs） | D09.6 |
| 33 | rule_decorrelate.go:346 | `eqCond.GetArgs()[1]` 的指针（变量名叫 clonedCol，实际没 Clone） | 共享但不依赖：firstrow 的 RetTp 就是同一 FieldType，等于没改 | D09.6 |
| 34 | scalar_subq_expression.go:162 | Clone 之后的对象 | 安全 | D09.2 |
| 35–36 | logical_plan_builder.go:374、392 | Clone | 安全 | D09.1 |
| 37 | logical_plan_builder.go:403 | schema4Agg 的新列 | 安全 | D09.1 |
| 38 | logical_plan_builder.go:1988 | `child.Schema().Clone()` | 安全 | D09.1 |
| 39 | logical_plan_builder.go:2094 | proj schema Clone | 安全 | D09.1 |
| 40–41 | logical_plan_builder.go:6174、6181 | `resolveIndicesForTblID2Handle` 已克隆 | 安全 | D09.1（DELETE） |
| 42 | logical_plan_builder.go:8215 | `seedSchema.Clone()` 加 RetType Clone | 安全 | D09.1（CTE） |
| 43 | **rule_partition_processor.go:157** | `ExtractColumns(hashExpr)` 返回 hashExpr 内部的列指针 | **共享且依赖可见性**：L190/195 的 `hashExpr.EvalInt` 依赖改过的 Index。Rust 要按 UniqueID→位置重写一个新的 hashExpr | D09.12 |
| 44 | joinorder/conflict_detector.go:833 | Clone 加 DeepCopy | 安全 | D09.10 |

grep 抓不到、但同类的原地修改：

- `rule_eliminate_projection.go:201`：E3，见 §3.3；
- `expression/constant_fold.go:43-48`：FoldConstant 对返回的表达式 SetCoercibility / SetCharset / SetCollate。D08.2 已部分处理，coercibility 原地改，collation 复制；
- `rule/util/misc.go:37`：E2；
- `resolve_indices.go` 中对 `p.Exprs[i]`、`aggFun.Args[i]`、`col.VirtualExpr` 的槽位写入：与逻辑节点共享 slice，但不依赖（推断）；
- `expression/grouping_sets.go:506`：`schema.Columns[idx].RetType.SetFlag`，**未核查**。

## 附录 B：FieldType 方法调用式的原地修改（#147 评审补充）

附录 A 只 grep 了 `.Index =` 和 `.RetType =` 两种赋值，抓不到通过方法调用原地修改 FieldType 的写法。E3 就是读代码时发现的，grep 没有命中。本附录补上这一类。

**范围**：

- `pkg/planner`：`grep -rnE '\.(SetFlag|AddFlag|DelFlag|SetFlen|SetDecimal|SetCharset|SetCollate|SetType|SetFlenUnderLimit|SetDecimalUnderLimit|SetElems|SetArray|ToggleFlag|AndFlag)\(' pkg/planner --include='*.go' | grep -v _test.go`，共 101 处；
- `pkg/expression`：改动"不是自己新建的参数或表达式"的类型的 helper，包括 grouping_sets、constant_fold、PropagateType、WrapWithCastAsInt、SetBinFlagOrBinStr 的各处调用等。

**分类**：

| 类 | 含义 |
|---|---|
| S | 新建的或已 Clone 的对象（安全） |
| V0 | 可能共享，但其他持有者是否看到修改无关紧要 |
| V1 | 共享，且其他持有者看到修改后，计划、结果、EXPLAIN 或协议元数据会变；需要显式模拟或登记 |
| A | 改的是 AST，不是计划里的 FieldType |

除非注明"已实测"，结论都来自读代码，标"推断"的需要在 Go 上实测确认。

### B.1 计数

| 部分 | S | V0 | V1 | A | 合计 |
|---|--:|--:|--:|--:|--:|
| pkg/planner | 92 | 2 | 4 | 3 | 101 |
| pkg/expression，逐条核对的 | 7 | 4 | 9 | 0 | 20 |
| pkg/expression helper 写自己新建的 `bf.tp` / `a.RetTp`（SetBinFlagOrBinStr 16、addBinFlag 7、SetBinChsClnFlag 12） | 35 | 0 | 0 | 0 | 35 |

V1 按机制合并为 **10 组**，见 B.3。另有 3 处 A 类。

### B.2 pkg/planner 中的非 S 项，以及成批的 S 项

| file:line | 被改对象 | 类 | 说明 |
|---|---|---|---|
| rule_eliminate_projection.go:201 | FoldConstant 的结果，可能就是共享列本身 | **V1** | E3。**已实测**：对 memory table 是全局影响（§3.3.1） |
| expression_rewriter.go:1701 | `COLLATE` 作用在非 `*Column` 的参数上，包括 `*CorrelatedColumn`；它的 RetType 与外层 schema 列共用（`:2630` 构造 `CorrelatedColumn{Column: *column}`） | **V1** | 推断 |
| logical_plan_builder.go:2065、2066 | UNION 一侧是 NULL 类型时，`unionJoinFieldType` 直接返回另一侧子列的 RetType，随后原地改 charset/collate | **V1**（2） | 推断 |
| logical_plan_builder.go:2043 | 同一个 `resultTp` | V0 | 共享时算出的 flen 不变（推断） |
| point_get_plan.go:2268 | point plan schema 列，`:2409` 按语句 Clone 过 | V0 | 加 EnumSetAsIntFlag 是幂等的（推断） |
| preprocess.go:1520、1521、2046 | DDL AST 中 ColumnDef 的类型 | A（3） | 有意改写 AST（DECIMAL(0) 补默认 flen；非严格模式下 VARCHAR 转 BLOB），DDL 侧要读到改写结果 |
| 其余 92 处 | `NewFieldType`、`&FieldType{}`、`.Clone()`、值拷贝、`ArrayType()`、`AggFieldType` 返回的局部值、`InferType4ControlFuncs` 新建的结果 | S | 集中在 planbuilder.go 的元数据 schema（12）、task.go（13）、logical_plan_builder.go（union 6 处等）、expression_rewriter.go（15）、point_get_plan.go（4）、mock.go（17，测试辅助代码），以及 Lucky 已核实的 rule_decorrelate.go:286、logical_plan_builder.go:8217、logical_projection.go:668、indexmerge_path.go:1422-1424 |

### B.3 V1 全表（按机制合并，10 组）

| # | 位置 | 机制 | 场景 | 状态 |
|---|---|---|---|---|
| 1 | rule_eliminate_projection.go:201 | 合并投影后 FoldConstant，再 SetFlag | `SELECT IF(x,a,b) FROM (SELECT 1 x,a,b ...) s` | **已实测**（memory table，全局） |
| 2 | expression/aggregation/window_func.go:76 | FIRST_VALUE/LAST_VALUE/NTH_VALUE 的 `RetTp = Args[0].GetType()` 不克隆，随后 DelFlag(NotNull) | `SELECT FIRST_VALUE(col) OVER () FROM t`，会使 `col IS NULL` 不再被折叠 | **已实测**（memory table，全局） |
| 3 | expression/constant_fold.go:46、47 | IF/IFNULL/CASE 的特殊折叠返回原参数，再 SetCharset/SetCollate | `SELECT IF(1, int_col, 'abc') FROM t` 改变该列的 charset 元数据；`ORDER BY IF(1, a, 'x' COLLATE utf8mb4_general_ci)` 改变 a 的 collation | 推断（实测 `IF(1, CONSTRAINT_NAME, NULL)` 时，用 IS NULL 观察不到） |
| 4 | planner/core/expression_rewriter.go:1701 | 对关联列的 COLLATE 改到外层列的类型 | `... WHERE EXISTS(SELECT 1 FROM t2 WHERE t2.b = t1.a COLLATE utf8mb4_general_ci) ORDER BY a` | 推断 |
| 5 | planner/core/logical_plan_builder.go:2065、2066 | UNION 的 NULL 侧借用另一侧子列的 RetType 后原地改 | `SELECT j FROM t UNION ALL SELECT NULL`（j 是 JSON）：Go 不加 cast，Rust 会多出 `cast(j as json)` | 推断 |
| 6 | expression/grouping_sets.go:506 | `expandSchema` 是浅拷贝，DelFlag 会改到 Agg 的分组项 | `GROUP BY a WITH ROLLUP`（MPP） | 推断 |
| 7 | expression/builtin_cast.go:2525 WrapWithCastAsInt | 只有 `*Column` 先克隆；enum 类型的关联列、ScalarFunction、Constant 原地加 EnumSetAsIntFlag | 关联子查询中的 `t1.e & 1`（e 是 enum） | 推断 |
| 8 | expression/builtin_time.go:2632 EXTRACT | 字符串参数不包 cast 时，直接改调用方传进来的列的 decimal | `SELECT s, EXTRACT(DAY_MICROSECOND FROM s) FROM t` | 推断 |
| 9 | expression/expression.go:1252、1253 PropagateType | Column 和 CorrelatedColumn 先克隆，但 ScalarFunction 和 Constant 原地改。D08.1 的 Rust 移植已选择复制，是一处已登记的差异 | 谓词下推时复用子 Projection 的 `p.Exprs`，`d + 0e0 > ...` 会改到共享的除法表达式的小数位 | 推断 |
| 10 | expression/util.go:1302 DisableParseJSONFlag4Expr | ScalarFunction 原地清 ParseToJSONFlag | 谓词下推后重建的 JSON_UNQUOTE 改到子 Projection 中共享的 `cast(s as json)` | 推断 |

**对 M-FT 的含义**：

- V1 一共 10 组，其中 4 组只影响元数据（charset、collate、decimal）。
- 真正可能改变计划的是第 1、2、5、6 组，加上附录 A 的 `rule_partition_processor.go:157`。
- 按方案 (a) 逐处显式模拟（或明确登记为不模拟）的工作量是有限的，不需要把 `ret_type` 全部改成 `Arc`。
- 第 1、2 组在 memory table 上的全局效果属于上游 bug（#148），**不模拟**。只在需要 EXPLAIN 对照时，模拟它们在单个查询内的效果。
