# 例外清单

依据 [宪章](../../CONSTITUTION.md) 第三条：默认用 Go TiDB v8.5.8 的特性定义 tidb-rs；符合条件的领域可以申请例外，用适合 Rust 的方式重新定义特性或实现。各项的理由和建议见 [candidates.md](candidates.md)。

## 流程

| 状态 | 含义 |
|---|---|
| 候选 | 已提出，未决定 |
| 原则同意 | 项目负责人同意方向；还没有正式 spec，**不能开始开发** |
| spec 待确认 | 正式 spec 已写好（`specs/exceptions/<ID>-<name>/spec.md`），等待项目负责人确认 |
| 已确认 | spec 已确认，可以按所在块的排期开发 |
| 已完成 | 已实现并通过验收（行为对照 + spec 中的指标） |
| 否决 | 不作为例外，按 Go 移植 |

每个正式 spec 至少写明：替换的 Go 范围（包、文件、函数）、与 Go 的差异及用户可见影响（特性例外）、行为契约与验收方法、可测量的目标（实现例外）、所在阶段。

## 索引

2026-11-01：以下各项项目负责人均"原则同意"，团队评审中。2026-11-02：基础设计 spec N1、N2、N3+A1（含 A2）和 A4 已写好，等待确认。2026-11-04：N4、N8 和 N5a（列格式）提前到 P1 之前定稿（PR #1 评审），spec 已写好，等待确认。2026-11-14：N1 按 D09.7 设计试点修订后确认（#149）。

| ID | 名称 | 类型 | 状态 | 计划定稿时间 |
|---|---|---|---|---|
| [N0](N0-crate-layering/spec.md) | crate 分层与依赖方向（`layers.toml` + CI 检查） | 实现（基础设计） | spec 待确认 | D00.2 开工前 |
| [N1](N1-plan-representation/spec.md) | 计划树、表达式树的表示与所有权（语句级 arena；摘出分派；物理计划 `Arc` 树；`M-FT`） | 实现（基础设计） | 已确认（2026-11-14，[#149](https://github.com/dulao5/tidb-rs/issues/149)；依据 [D09.7 试点报告](../D09.7-column-pruning/pilot-report.md)） | P1 之前 |
| [N2](N2-session-context/spec.md) | 会话、语句上下文的共享状态拆分 | 实现（基础设计） | spec 待确认 | P1 之前 |
| [N3](N3-A1-concurrency-memory/spec.md) | 并发、取消与错误边界（含 A2） | 实现（基础设计） | spec 待确认 | P1 之前 |
| [A1](N3-A1-concurrency-memory/spec.md) | 统一内存模型（缓冲区、语句级内存、可计算预算） | 实现（基础设计） | spec 待确认 | P1 之前 |
| A2 | 执行模型：算子结构不变，worker 映射到共享线程池 | 实现 | spec 待确认（并入 N3） | P1 之前 |
| A3 | 缓存：coprocessor cache、plan cache、infoschema v2、统计信息缓存（4 个 spec） | 实现 | 原则同意 | 各自所属块排期时 |
| [A4](A4-json/spec.md) | JSON：二进制格式兼容，实现重写 | 实现 | spec 待确认 | P1 |
| A5 | SIMD / 向量化求值（按函数族启用） | 实现 | 原则同意 | P1 之后按函数族 |
| A6 | 读取路径：零拷贝解码、批量请求 | 实现 | 原则同意 | P2 |
| [N4](N4-expr-eval/spec.md) | 表达式求值框架（规划侧表达式树按 Go；执行侧编译为 RPN；函数体只按 Go 逐行版写一份） | 实现（基础设计） | spec 待确认 | P1 之前 |
| [N5a](N5a-chunk/spec.md) | 根节点内存中的列格式（移植 Go Chunk） | 实现（基础设计） | spec 待确认 | P1 之前 |
| N5 | 值的表示与零拷贝（与 A1、A6 一起设计；列格式部分见 N5a） | 实现 | 原则同意 | P2 |
| N6 | 哈希聚合、哈希 join、排序的数据结构 | 实现 | 原则同意 | P4 之前 |
| N7 | 结果集编码与网络层 | 实现 | 原则同意 | P2 |
| [N8](N8-sysvar/spec.md) | 系统变量静态注册表 | 实现（基础设计） | spec 待确认 | P1 之前 |
| [N9](N9-observability/spec.md) | 可观测性：统一的语句执行记录与任务级观测上下文（profile、慢日志与语句摘要、EXPLAIN ANALYZE、Top SQL、TRACE） | 实现（基础设计）+ 待决定的特性问题 | 调研草案 | P2 之前（D12.0、D05.3、D07.2 开工前） |
| [N10](N10-resource-control/spec.md) | 资源管控：资源组 RU 计量、按组加权公平的限流、runaway 在 checkpoint 上判定（兼容面不变） | 实现 + 待决定的特性问题 | 调研草案 | D05.3 之前（读路径接入 TiKV 后） |
| [N11](N11-cluster-readts/spec.md) | 集群成员身份、schema 同步与 MDL（lease 按 TSO、准入窗口 ≤ 注册窗口、升级时拒绝不认识的元数据）、读时间戳（TSO、低精度、stale、replica read）、显式只读事务；流量接入见 [ADR 0009](../../docs/adr/0009-traffic-entry.md) | 实现 + 待决定的特性问题 | 调研草案 | lease 修复（#136）立即；其余 D05.4 开工前 |
| B1 | 静态分区裁剪：不移植，设置时报错 | 特性 | 原则同意 | P3 |
| B2 | v1 统计信息：只读 v2，v1 表按无统计处理 | 特性 | 原则同意 | P3 |
| B3 | 旧 cascades 优化器：不移植 | 特性 | 原则同意 | P3 |
| B4 | 新 cascades 优化器：暂不移植，上游同步时再评估 | 特性 | 原则同意 | P3 |
| B5 | 代价模型 v1：不移植，设置时报错 | 特性 | 原则同意 | P3 |
| C1 | JSON_TABLE | 特性（新增） | 原则同意 | 只读移植完成后 |
| C2 | cached table 新做法 | 特性（新增） | 原则同意 | C1 之后 |
| C3 | PostgreSQL / DuckDB 风格的语法和特性 | 特性（新增） | 原则同意 | 移植阶段结束后 |
| D1 | TiFlash / MPP：只读阶段不做，边界桩隔开 | 范围 | 原则同意 | — |

不作为例外、按 Go 照搬的判断示例（B6、B7 及"第三类"）见 [candidates.md](candidates.md)。
