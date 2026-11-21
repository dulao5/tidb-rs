# `taskMap` 小试点报告（VS1-26）

日期：2026-11-21。对应设计 spec：N1 §3.2（`taskMap` 旁表）、§3.4（物理计划的 `Arc` 树）、§7 第 3 条（未验证项）。

## 0. 结论

**通过**。N1 的表示能承载 Go `FindBestTask` 的记忆化（`taskMap` / `GetTask` / `StoreTask` / `RollBackTaskMap`），共享的结构与 Go 相同；代价计算期间 `clone_node` 调用 **0 次**。不需要在 VS1-28 开工前推翻 N1，但要补三条写法规则（§5），其中第 1 条（reader 的 `table_plans` 与 `table_plan` 双重持有）是本次新发现的问题，影响 VS1-24 的 `postOptimize` 和 VS1-31 的 `ResolveIndices` / `ToPB`。

| 检查项 | 结果 |
|---|---|
| 代价计算期间的 `clone_node` 次数（#223 评审要求） | 0（4 个实验共 9 次 `compareTaskCost`） |
| 记忆化命中时返回的 task 与旁表中存的是同一个 `Arc`（Go：同一个指针） | 2/2 |
| 每个候选挂上子 task 后，子计划在候选树中按指针出现（共享，没有复制） | 6/6 |
| 共享子节点的代价缓存写在共享节点本身（`Mutex<PlanCostCache>`，经 `&self`） | 是 |
| 按时间戳回滚（`TaskMapNeedBackUp`） | 与 Go 相同 |
| 释放旁表、丢弃落选候选后，胜出计划中每个节点独占 | 是（但见 §3 第 1 条：reader 下的节点有第二个持有者） |
| `postOptimize` 形状的原地遍历 | 按 §5 第 1 条的写法 0 次复制；直接写则每棵树复制 2 次，且 `table_plans` 失步 |

## 1. 做了什么

试验代码在分支 `spike/taskmap`（提交 `96b30c9`）：[`crates/planner/src/core/taskmap_spike_tests.rs`](https://github.com/dulao5/tidb-rs/blob/96b30c9/crates/planner/src/core/taskmap_spike_tests.rs)，另在 `make_mut_plan` 中加了一个只在测试中编译的计数器（`clone_node` 只有这一个调用方，已 grep 确认）。**本 PR 不包含这段代码**，理由见 §6。

- **模拟的 Go 函数**（`find_best_task.go`，v8.5.8）：`findBestTask`（L505–605，含 `canAddEnforcer` 两轮枚举和 hint 分支）、`enumeratePhysicalPlans4Task` + `iteratePhysicalPlan4BaseLogical`（L188–300）、`compareTaskCost`（L355）；`base_logical_plan.go` 的 `GetLogicalTS4TaskMap` / `GetTask` / `StoreTask` / `RollBackTaskMap`（L279–368）。
- **真实代码**：VS1-27（#285）已合并的 `exhaust_physical_plans`、`attach2_task`、`enforce_property`、`CopTask::convert_to_root_task`、物理算子；VS1-19（#223）的 `BasePhysicalPlan::get_plan_cost_ver2`（`Mutex<PlanCostCache>`）。
- **模拟的部分**：`DataSource` 的 `findBestTask`（单一表路径、无索引：根属性给出 TableReader，cop 单读属性给出 cop task，其余无效），同样走旁表；`getTaskPlanCost` 只取根计划的 `GetPlanCostVer2`（VS1-30 未合并，各算子用基类的默认代价，值都是 0，所以总选第一个有效候选；这不影响要验证的缓存写入路径）。
- **旁表**：`HashMap<(PlanId, prop.HashCode()), Option<Task>>`，`None` 对应 Go 回滚后的 `p.taskMap[cur] = nil`；回滚日志按节点存 `(key, ts)`；时间戳用 `StatementContext.task_map_bak_ts`（经 `stmt_ctx_mut()`，规划器在 `plan_cache.rs` 中已这样写）。

## 2. 实验与数字

| 实验 | 形状 | 数字 |
|---|---|---|
| E1 VS1 形状 | `Limit(1,2)` / `TopN(by c1, 10)` 在 DataSource 上，根属性 | Limit：存 3 次、命中 0、有效候选 1、代价计算 2 次、`clone_node`：枚举 0 / 代价 0。TopN：存 7 次（6 个候选，其中 MPP、多读、带序的 Limit 无效）、其余同上。胜出树 4 个节点（Limit → TableReader → 下推 Limit → TableScan）：旁表存活时独占 2 个，释放后 4 个（按 §3 第 1 条扣除 `table_plans` 的持有）。`postOptimize` 形状遍历：直接写复制 2 次且 `table_plans` 失步；按 §5 第 1 条写 0 次 |
| E2 共享子节点 | 一个逻辑节点的 3 个候选向同一子节点要同一属性（Go 中 `getHashJoins` 的左右 build 两个候选就是这样） | 存 1 次、命中 2 次，3 个候选的子节点与旁表中的是同一个 `Arc`；第一次代价计算后共享子节点的 `plan_cost_init` 为真；代价 3 次、`clone_node` 0。枚举结束后子节点引用计数 4（旁表 + 胜出者 + 两个测试局部变量），落选者已释放。对照：旁表存活时跑 `postOptimize` 形状遍历复制 3 次；释放后 0 次 |
| E3 回滚 | `force_nth_plan = 1`（`TaskMapNeedBackUp` 为真）；在 ts 前后各存一个属性，`RollBackTaskMap(ts)` | ts 之后存的项变成 `None`，之前的保留；重新搜索得到新的 `Arc`，旧候选手里的旧 task 仍然有效（Go 靠 GC）；时间戳序列与 Go 相同（结束时为 4） |
| E4 反例 | E1 的 TopN，但枚举循环写成"持有 `Vec` 再 `Arc::clone(pp)` 交给 `attach2_task`" | 每个有效候选在 `attachPlan2Task` 的 `SetChildren` 处被复制 1 次（本例 1 次）；代价期间仍为 0 |

`cargo test -p tidb-planner --lib taskmap_spike -- --nocapture` 输出每个实验的 `SPIKE ...` 一行，上表数字取自其中。

## 3. 与 Go 共享语义的对比

**结构共享相同**。Go 的 `GetTask` 返回旁表里同一个 `base.Task` 指针，`Attach2Task` 先 `tasks[0].Copy()`（浅拷贝），子计划指针在所有候选和旁表之间共享。Rust 的 `Task` 克隆只增加 `Arc` 计数，`attach_plan2_task` 把子计划移入父节点，指针身份与 Go 一一对应（E2：3/3 个候选指向同一节点）。代价缓存也一样：Go 把 `planCostInit` / `planCostVer2` 写在共享节点上，下一个候选直接读；Rust 经 `&self` 写在 `Mutex` 里，同一个节点，不复制（#223 评审改动的目的，E2 验证了）。

**写入的可见性不同，这是有意的**（N1 §3.3 的写时复制原则）。Go 在少数地方直接写共享节点，写入对旁表和其他候选可见；Rust 的 `make_mut_plan` 会复制，旁表里的原节点不变。`task.go` 中的这类写入：

| Go 位置 | 写什么 | VS1 中的状态 |
|---|---|---|
| `convertToRootTaskImpl` 的 `ExpandVirtualColumn` | TableScan 的 `Columns` 和 schema | 有虚拟列时返回 `blocked-by:D10.1`（D10.1 spec §5 第 1 条、待定问题 1） |
| `finishIndexPlan` 的 `ts.SetStats` | index lookup 表侧 scan 的统计 | index lookup 是边界桩 |
| `sinkIntoIndexLookUp` 的 `ts.SetStats` | 同上 | 只在 index lookup 上触发 |
| `pushTopNDownToDynamicPartition` 慢日志分支的 `KeepOrder` / `SetStats` | scan | Go 先 `tablePlan.Clone()` 再写，本来就不共享，Rust 复制后结果相同 |

所以在 VS1 范围内，没有可达的"写共享节点并依赖可见性"的路径，结构共享与 Go 等价。VS1-28 移植 `find_best_task.go` 时要对新增的写入逐处做同样的核对（写进该块 plan 的"已知差异"表）。

**新发现的问题**：

1. **reader 的双重持有**。`PhysicalTableReader` 同时保存 `table_plan`（链）和 `table_plans`（Go 的 `TablePlans`，`flattenPushDownPlan` 的结果），两者是同一批节点（Go 中也是同一批指针）。所以即使释放了旁表，reader 下面的每个节点引用计数仍然 ≥ 2，`make_mut_plan` 会复制它们。复制之后 `table_plan` 指向新节点，`table_plans` 还指向旧节点；Go 中两边看到的是同一次写入。`ResolveIndices`（VS1-31）改的是 `tablePlan`，`ToPB` 读的是 `TablePlans`，直接照写会编出索引没解析的 DAG。E1 中直接写：每棵树复制 2 次，`table_plans` 失步；先清空 `table_plans`、改完再重新展开：0 次，且一致。`PhysicalTableReader::init` 在 `setMppOrBatchCopForTableScan` 前后已经是这样写的（#285），只是没有写成通用规则。N1 §3.4 的 `debug_assert!(Arc::strong_count(..) == 1)` 也必须把这一点算进去，否则在每个 reader 下都会误报。之后的 `PhysicalIndexReader.IndexPlans`、`PhysicalIndexLookUpReader.IndexPlans/TablePlans` 也是这种结构。
2. **枚举循环必须把候选移出**（E4）。`exhaustPhysicalPlans` 返回的 `Vec<PhysicalPlanRef>` 要 `into_iter()` 逐个移给 `attach2_task`；先 `&physical_plans` 再 `Arc::clone(pp)`，每个候选都会在 `SetChildren` 处被浅拷贝一次。结果仍然正确，但这正是 §3.4 说的"悄悄复制、没有任何东西报出来"。Go 在 `Attach2Task` 之后不再用 `pp`，移出是等价的。
3. **旁表放在哪里、何时释放**。Go 只在 `find_best_task.go` 中读写 `taskMap`（grep 确认，`physicalOptimize` 之后没有读者）。旁表应放在 `physicalOptimize` 范围内的结构里（或 `PlanCtx` 中一个在 `physicalOptimize` 返回前取出并 drop 的字段），**不要**放在 arena 的逻辑节点上：arena 活到语句规划结束，旁表会在 `postOptimize` 期间一直持有每个胜出节点（E2 对照：复制 3 次）。
4. **VS1 形状几乎不产生命中**（E1 命中 0）。VS1 的算子（Sel / Proj / Sort / TopN / Limit 在单表上）的候选向子节点要的属性各不相同，记忆化命中主要来自 join 的多个候选、或同一属性被两条路径要到。VS1-28 的计划向量基本覆盖不到共享，所以共享要靠单元测试守住（§6）。

## 4. 未覆盖

- DataSource 是模拟的（真实的 `findBestTask4DataSource` 有自己的 `GetTask` / `StoreTask` 和 enforcer 分支，L1792–1844），访问路径、skyline 没有参与。
- 代价值都是 0（VS1-30 未合并），只验证了缓存的写入位置和复制次数，没有验证选优结果。VS1-30 的各算子 `GetPlanCostVer2` 也要经 `&self` 写缓存、经 `&` 访问子节点，不能再用 `make_mut_plan`，否则本报告的 0 不再成立。
- `nth_plan` 的 `rebuildChildTasks` 没有走（只验证了回滚本身）；MPP 属性全部无效。

## 5. N1 修订建议

不改方案，补三条写法规则和一处措辞（建议由 VS1-28 的 PR 一并写入 N1，或单独一个小 PR）：

1. §3.4 增加 **"扁平列表"规则**：节点中保存的、与子树同一批节点的扁平列表（`TablePlans`、`IndexPlans`），在通过 `make_mut_plan` / `map_children` 写子树之前先清空，写完再从子树重新展开；`postOptimize` 入口的 `strong_count == 1` 断言按"子树链 + 扁平列表"计算。备选：不保存扁平列表，用到时再展开（要核对 Go 中直接读 `TablePlans` 的位置）。
2. §3.4 增加：**枚举候选时按值移出**（`into_iter()`），不要持有候选列表再克隆 `Arc`。
3. §3.2 改为：旁表的生命周期限定在 `physicalOptimize` 之内，返回前 drop；不挂在 arena 节点上。回滚日志与 Go 相同（`(key, ts)` 列表，回滚项置 `None`），时间戳沿用 `StatementContext.task_map_bak_ts`。
4. §7 第 3 条改为"已由 VS1-26 验证"，链接本报告；保留"写共享节点的位置逐处核对"作为 VS1-28 的要求。

## 6. 试验代码的去留

不合并。它模拟的 `findBestTask` 会在 VS1-28 中被真正的移植取代，留下来就是同一个 Go 函数的第二份翻译。建议 VS1-28：

- 保留 `make_mut_plan` 中只在测试中编译的 `clone_node` 计数器；
- 在真正的 `FindBestTask` 上加两个单元测试：E2 形状（多个候选共享子 task：指针相同、代价缓存写在共享节点、代价期间 `clone_node` 为 0）和 E1 形状（释放旁表后 `postOptimize` 0 次复制、reader 的扁平列表一致）。

## 本文所用 ID

- VS1-19：规划器的属性、工具、`BasePhysicalPlan`（#223）
- VS1-24：优化器驱动（`DoOptimize`、`postOptimize`）
- VS1-26：本试点（`taskMap` 小试点）
- VS1-27：物理算子与 task、`exhaustPhysicalPlans`（#285）
- VS1-28：`FindBestTask`、DataSource 的访问路径
- VS1-30：代价模型 v2
- VS1-31：`ResolveIndices`、`ToPB`、access object
- D10.1：物理算子定义与 task
- N1：计划树和表达式树的表示（§3.2 `taskMap` 旁表，§3.3 共享修改，§3.4 `Arc` 树，§7 待定项）
