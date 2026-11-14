# ADR 0010：协议与方言分层

- 状态：proposed（2026-11-14）。方向由项目负责人在 #166、#167、#168、#169 和私信中给出；本 ADR 把它落成对当前代码的约束，**现在不实现任何方言或协议**。
- 背景：#166（PG / Oracle 迁移）、#167（语法超集和功能超集只有一份）、#168（多租户下一个租户一种协议）、#169（IN 列表 / VALUES 行数自适应，digest 与 Go 不同）。候选例外 C3（[candidates.md](../../specs/exceptions/candidates.md)）。
- 引用记法：`pkg/...` 指 TiDB v8.5.8。标"推断"的是没有代码或实测支撑的判断。

## 1. 背景：项目负责人的立场

| 问题 | 立场 | 来源 |
|---|---|---|
| 支持什么 | 将来支持 PostgreSQL 和 Oracle 的 **SQL 语法和线协议**。它们的功能（存储过程、触发器、系统目录、特有类型、事务语义）和迁移工具**暂不考虑** | #166 |
| 超集的形态 | **一个语义核心**（TiDB / MySQL 语义的超集），多个语法前端；超集特性在逻辑上只有一份，不是三份 | #167 |
| 租户与协议 | 将来多租户（tidb-x-rs / keyspace）时，**租户与协议 1 对 1**；元数据（含 digest、plan cache、统计信息、权限）**按租户隔离** | #167 第 1、4 条；#168 第 1–3 条 |
| 语义冲突以谁为准 | **未定**（如 Oracle 空串等于 NULL、PG 标识符大小写、除法与整数溢出）。现在只保留设计余地 | #167 第 2 条 |
| 与 Go TiDB 共存 | 超集特性与 Go 节点的共存问题，**等 tidb-go 退役后**再考虑 | #167 第 3 条 |
| Oracle 协议 | Oracle 的 TNS / Net8 没有公开规范。如果做不了，优先在 MySQL / PG 协议上支持 Oracle 风格的语法 | #166（私信 2026-11-14） |
| 现在做什么 | "当前只需要考虑为**协议层**和 **SQL 解析层**留出设计余地"；会话层与协议无关、解析器到 planbuilder 之间留边界（#166 的建议，已采纳） | #168、#166 |
| digest 与 plan cache 键 | 在归一化后的 AST / IR 上计算，不在 SQL 文本上计算；数组 / IN 列表参数归一化为一个占位。**digest 允许与 Go 不同**（项目负责人批准的例外） | #169 |

## 2. 决定

一条语句从连接进来到返回结果，分成四层。每层只认识相邻层的接口：

```
客户端
  │  线协议（MySQL；将来 PG；Oracle 见 §5）
  ▼
① 协议适配器（每种协议一个）    握手与认证、分帧、命令分派、prepared statement 模型、
                                 列类型的线编码、错误码映射、会话状态标志
  │  协议无关的会话接口：语句（文本或 AST）+ 参数  →  带类型的结果流 + warnings / error
  ▼
② 会话（只有一份）              SessionVars、事务、prepared statement 存储、语句执行流程（Go pkg/session）
  │  dialect（每个连接 / 租户一个，§4.4）选择语法前端
  ▼
③ 语法前端（每种方言一个）      词法、语法、标识符大小写与引号规则 → 同一种 AST
  │  AST（= 语义核心的输入，Go pkg/parser/ast 的形状）
  ▼
④ 语义核心（只有一份）          preprocess、planbuilder、优化器、执行器、表达式、类型系统、元数据
```

- **语义核心只有一份**，按 Go TiDB v8.5.8 逐函数移植，类型系统、错误码、collation 都是 MySQL / TiDB 的。其他方言只决定"怎么写"，"是什么意思"由核心定义。方言需要的语义差异将来以"核心语义 + 方言开关"的形式加进核心，边界未定（§5）。
- **协议适配器和语法前端是两条独立的轴**。现在只有 MySQL 协议 + MySQL 语法这一种组合；租户与协议 1 对 1，所以实际部署中协议决定了方言，但代码中两者分开（Oracle 风格的语法可能跑在 MySQL 或 PG 协议上，§5）。
- **AST 是语法前端和语义核心之间的唯一边界**。现在不另设一层新的 IR：planbuilder 要按 L1 移植，就必须吃 Go 的 AST 形状。其他前端产出同样的 AST。
- **digest 和 plan cache 键在归一化后的 AST 上计算**，不在 SQL 文本上计算，所以不同前端、不同写法的同一条语句可以得到同一个 digest 和同一个计划。

## 3. 现状：各边界在今天的代码中的位置

| 层 | 今天的 crate | 状态（`layers.toml`） | 与本 ADR 的关系 |
|---|---|---|---|
| ① MySQL 协议适配器 | `crates/mysql-protocol`（`tidb-mysql-protocol`） | legacy | 已经独占线协议：分帧、握手、认证、命令分派、prepared statement 状态、结果编码。执行边界是 `SessionHandler` / `SessionFactory` trait（`src/handler.rs`） |
| ① 将来的位置 | `tidb-server`（rank 16，planned，D03.1/D03.2 = Go `pkg/server`） | planned | Go 的 `pkg/server` 本身就是 MySQL 适配器：`clientConn.dispatch`（`pkg/server/conn.go:1311`）、`writeColumnInfo`（`:2386`）、`writeError`（`:1580`）、`column.DumpTextRow` / `DumpBinaryRow`（`pkg/server/internal/column/column.go:151`、`:228`）、`handleStmtExecute`（`conn_stmt.go:137`）。D03.x 移植后，MySQL 适配器就在这里 |
| ② 会话 | `crates/poc/session`（`poc-session`，实现 `SessionHandler`） | legacy | 将来是 `tidb-session`（rank 15，planned，D05.3 = Go `pkg/session`） |
| ③ MySQL 语法前端 | `crates/parser`（`tidb-parser`） | layered，rank 1 | `parse(sql, &ParseOptions) -> ParseOutput { stmts: Vec<ast::Stmt>, warnings }`。`ParseOptions` 目前只有 `window_functions` |
| ③→④ PoC 的 IR | `crates/sql`（`tidb-sql`，`parse_and_lower`） | legacy | PoC 的降级 IR（ADR 0004），**不是**本 ADR 的边界；会被 `tidb-planner` 取代 |
| ④ 语义核心 | `tidb-planner`（rank 13，planned，含 D09.1 planbuilder、D13.1 plan cache）、`tidb-executor`、`tidb-expression` 等 | layered / planned | planbuilder 的输入是 `tidb_parser::ast` |

Go 自己的边界已经接近本 ADR 的形状：`pkg/server` 解析后调用 `Session.ExecuteStmt(ctx, ast.StmtNode) (sqlexec.RecordSet, error)`（`pkg/session/types/sesson_interface.go:45`，经 `pkg/server/driver_tidb.go:352`），结果是 `RecordSet`（列信息 + chunk 流），线编码在 `pkg/server` 里做。偏离之处在于：

1. **PoC 的协议类型渗进了 SQL 层和执行器**。`tidb-sql` 用 `tidb_mysql_protocol::{SqlError, Value, ColumnDef, ColumnType}` 作为内部的错误、值和结果元数据类型（`crates/sql/src/{ir,plan,ranges,types}.rs`），`poc-executor` 同样（`crates/poc/executor/src/{lib,convert,agg,compare}.rs`）。它们都是 legacy，会被移植代码取代；移植 crate 不能这样做（§4.1）。
2. **`SessionHandler` 带有 MySQL 的形状**：`prepare(stmt_id: u32, …)`、`status_flags() -> u16`（MySQL 的 `SERVER_STATUS_*` 位）、`use_db`（COM_INIT_DB）、`lookup_user` 返回 `mysql_native_password` 的 `Credential`；`Response` 的 `Value` 是协议导向的。作为 MySQL 适配器内部的接口没有问题，但它不是协议无关的会话接口（§4.1）。
3. **Go 的 SQL digest 是在文本上算的**：`parser.NormalizeDigest(sql)`（`pkg/parser/digester.go:125`）对词法 token 归一化，`pkg/session/session.go:1653`、`pkg/server/conn.go:2037`、`pkg/planner/core/plan_cache_utils.go:135` 都这样用；prepared plan cache 的键里放的是语句文本（`NewPlanCacheKey` 用 `stmt.StmtText`，`plan_cache_utils.go:257`）。这依赖 MySQL 的词法，换一种语法前端就不成立（§4.3）。
4. **planner 有 18 处直接读原始 SQL 文本**（`pkg/planner/core` 非测试代码中 `.Text()` / `OriginalText()` / `OriginTextPosition()` 的调用）。最典型的是结果集列名：`SELECT a+1 FROM t` 的列名就是 `field.Text()`（`logical_plan_builder.go:1462`、`:1484`、`:1497`），另有错误消息中的表达式文本（`:3940`）等。这是 AST 边界上必须由前端提供的东西（§4.2）。

## 4. 对当前代码的约束（现在就守住，不实现）

### 4.1 协议无关的会话接口

**会话（`tidb-session`，D05.3）及其以下的所有 crate 不认识任何线协议。** 接口的形状：

- **输入**：语句（SQL 文本交给本会话的语法前端解析，或已经解析好的 AST）+ 参数（核心的 `Datum`，已由适配器从线格式解码）。
- **输出**：带类型的结果流（列元数据用核心的类型，即 Go 的 `ResultField` / `FieldType`；数据是 chunk 流，对应 Go 的 `RecordSet`）+ warnings + 错误（`tidb_base::Error`，内部错误码就是 MySQL errno，`M-ERR`）+ 影响行数、last insert id 等语句结果。
- **会话状态以查询的方式给出**（是否在事务中、autocommit、当前库），由适配器换算成自己的表示（MySQL 的 `SERVER_STATUS_*` 位、PG 的 `ReadyForQuery` 事务状态字节）。

**每个协议适配器独占**：

| 职责 | MySQL（今天 / D03.x） | 适配器之外不能出现的东西 |
|---|---|---|
| 握手与认证 | HandshakeV10、auth switch、`mysql_native_password` 等插件、TLS 升级 | 认证插件名、scramble、salt |
| 分帧 | 4 字节包头、序号、16 MiB 拆包 | 包、序号 |
| 命令分派 | COM_QUERY、COM_INIT_DB、COM_FIELD_LIST、COM_STMT_*、COM_CHANGE_USER 等，翻译成会话调用 | 命令字节 |
| prepared statement 模型 | 语句 ID、参数类型、long data、COM_STMT_FETCH 游标 | 协议的语句 ID / 名称。会话按自己的 ID 保存 prepared statement（Go `SessionVars.PreparedStmts`），适配器维护"协议名 → 会话 ID"的映射（PG 的具名语句与 portal 也这样映射） |
| 列类型的线编码 | `writeColumnInfo`、`DumpTextRow` / `DumpBinaryRow`、二进制参数解码（D03.1 的 `parseBinaryParams`） | 线上的类型码、文本 / 二进制编码 |
| 错误码映射 | MySQL errno + SQLSTATE 原样输出 | 其他协议的错误码。PG 适配器将来持有 errno → PG SQLSTATE 的映射表 |

具体约束：

1. **依赖方向**：协议适配器的 crate 排在 `tidb-session` 之上（`tidb-server` rank 16 > `tidb-session` rank 15）。layered crate 本来就不能依赖 legacy crate，所以现在的 `tidb-mysql-protocol` 进不了移植代码；将来它被吸收进 `tidb-server`（或作为 rank ≥ 16 的 layered crate）时，`layers.toml` 保证 rank ≤ 15 的 crate 依赖不到它。
2. **D03.1 / D03.2 照 Go 的 `pkg/server` 移植**，它就是 MySQL 适配器，不需要额外抽象层；D05.3 照 `pkg/session` 移植。块 spec 中写明：凡是 Go 在 `pkg/session` 及以下读写 MySQL 协议概念的地方（如 `PrepareStmt` 返回的 `stmtID`、`SessionVars.PreparedStmts map[uint32]any`、`SessionVars.status` 中的 `mysql.ServerStatus*` 标志位，`pkg/sessionctx/variable/session.go:798`、`:2631`），逐处登记，保持在会话中按 L1 移植，但不让线格式（包、类型码）进到会话以下。
3. **`SessionHandler` 不升格为协议无关接口**。它可以继续作为 MySQL 适配器与会话之间的胶水，PG 适配器将来写自己的胶水，两者都调用同一个会话接口。

### 4.2 AST 边界：其他语法前端产出同样的 AST

1. **planbuilder 和 preprocess（`tidb-planner`）只读 `tidb_parser::ast`，不调用词法器或语法分析器**，也不重新解析 SQL 文本。需要解析子片段的地方（视图定义、binding、`PREPARE` 的语句文本等）调用会话提供的"按本会话方言解析"的入口，而不是直接调用 `tidb_parser::parse`。Go 中这类位置在移植块的 plan 中逐处登记。
2. **AST 中的原始文本是前端的输出，不是 MySQL 词法的副产品**。§3 第 4 条的 18 处依赖原始文本（列名、错误消息），其他前端必须为这些节点填上等价的文本跨度。所以 AST 保留 Go 的 `Text()` / `OriginTextPosition()` 语义，移植这些调用的块在 plan 中登记，标明"前端须提供"。
3. **标识符和字面量在 AST 中已经规范化**：大小写折叠、引号（反引号 / 双引号）、字符集引导符（`_utf8mb4'x'`）在前端处理完，AST 里是 `CIStr` 和带类型的值，与 Go 相同。语义核心不判断"这个标识符原来是怎么写的"。
4. **不分叉 MySQL 语法**：方言不做成 MySQL 语法内部的开关，也不在 `grammar/tidb_typed.y` 里加别的方言的产生式。将来 PG / Oracle 语法是独立的前端（可以共享 LR 驱动和词法框架），产出同一种 AST；AST 不够表达的超集语法再加节点，加在核心里，所有前端可用。
5. **`ParseOptions` 是前端的配置入口**。现在不加字段；将来选择前端的是会话的 dialect（§4.4），不是 `ParseOptions` 里的一个标志。

### 4.3 digest 和 plan cache 键在归一化 AST 上计算

1. **SQL digest、归一化文本、plan cache 键都从归一化后的 AST 计算**：字面量和参数变成占位，`IN (…)` 列表和 `INSERT … VALUES` 的多行（以及将来的数组参数）归一化为**一个**占位（#169），然后对归一化 AST 求哈希。不对原始文本做词法归一化。
2. **与 Go 不同，项目负责人已批准**（#169 第 3 条）。用户可见的影响：`statements_summary` / 慢日志中的 `DIGEST`、`DIGEST_TEXT` 与 Go 节点不同；按 digest 关联 tidb-go 与 tidb-rs 的诊断（ADR 0009 的影子读比对）要按语句文本或 tidb-rs 自己的 digest 关联。正式的例外条目在 D14.2（statement summary）和 D13.1（plan cache）的块 spec 中登记，`DIGEST_TEXT` 用哪种方言打印也在那里定。
3. **plan cache 键带上 dialect**（以及将来的租户 / keyspace）。租户之间元数据隔离（#167、#168），所以同一 digest 不跨租户共享计划。
4. **plan digest 不变**：它本来就在物理计划上计算（N9），与前端无关。

### 4.4 会话中预留 dialect 属性

1. **会话持有一个 `dialect`**（枚举，现在只有 `MySQL` 一个值），在连接建立时由协议适配器按租户配置设置，会话存续期间不变。它不是系统变量，不能 `SET`（租户与协议 1 对 1，一个连接不切换方言）。
2. dialect 决定：用哪个语法前端解析（§4.2 第 1 条）、plan cache 键（§4.3 第 3 条）、`DIGEST_TEXT` 的打印方言（待 D14.2 定）。将来"核心语义 + 方言开关"的开关也从这里读，但开关本身未定（§5）。
3. **现在不在元数据中加方言标记**（#168 第 4 条）。元数据按租户隔离，每个租户只有一种协议。
4. 落地方式：建立 `tidb-session`（D05.3）时在 N2 的 `Session` 结构里加这个字段，并在 [foundation-interfaces.md](../../specs/exceptions/foundation-interfaces.md) 中登记；在那之前不写代码。

### 4.5 检查方法

| 约束 | 怎么检查 |
|---|---|
| 会话及以下不依赖协议 crate | `layers.toml` + `scripts/check_layers.py`（已有）；协议 crate 登记在 rank ≥ 16 |
| planner 不调用解析器 | `tidb-planner` 中 grep `tidb_parser::parse` / `Parser::`，只允许经会话提供的入口 |
| 依赖原始文本的位置 | 块 plan 中登记 `.Text()` / `OriginTextPosition()` 的移植位置 |
| digest 从 AST 计算 | D14.2 / D13.1 的测试：同一语句不同 IN 列表长度、不同空白和注释，得到同一 digest 和同一 plan cache 项 |

## 5. 明确不决定的事

1. **语义冲突以谁为准**，以及"核心语义 + 方言开关"的边界（Oracle 空串等于 NULL、PG 标识符大小写、除法与整数溢出、PG 出错后事务进入 aborted 状态等）。#167 第 2 条：未定。
2. **PG / Oracle 的功能**：存储过程（PL/pgSQL、PL/SQL）、触发器、序列、同义词、包、系统目录（`pg_catalog`、Oracle 数据字典）、特有数据类型（`timestamptz`、`NUMBER`）、迁移工具。#166：暂不考虑。
3. **Oracle 的线协议**。TNS / Net8 没有公开规范，实现它需要逆向，有法律和兼容性风险，也可能根本做不到稳定兼容。如果不可行，就在 MySQL 或 PG 协议上提供 Oracle 风格的语法（§2 中协议与前端是两条轴，正是为此）。是否做、做到什么程度，现在不定。
4. **超集特性的清单和优先级**（PG 风格的向量索引语法、`INDEX … INCLUDE (…)` 覆盖索引、原生 ARRAY、跨协议可用的 `JSON_TABLE` 等），以及这些需要改表元数据和编码的特性与 Go TiDB 的共存方式（等 tidb-go 退役后再考虑，#167 第 3 条）。
5. **多租户的实现**（tidb-x-rs / keyspace）。#128 中"目标平台是否包括 keyspace"仍未决定，ADR 0003、0005 目前不支持 keyspace。本 ADR 只假定"租户与协议 1 对 1、元数据按租户隔离"。
6. **其他前端的实现方式**：独立语法文件还是共享部分产生式、AST 是否需要扩展节点。等真正开始做时再定。
7. **#169 的计划层设计**（IN 列表参数槽位、超长 IN 何时改走临时表 join / hash semi join）：在 D13.1 和相关物理计划块中定，不在本 ADR。

## 6. 后果

- 对移植工作量几乎没有影响：D03.x、D05.3、D09.1 本来就按 Go 的 `pkg/server` / `pkg/session` / `pkg/parser/ast` 边界移植。新增的工作是三项登记（§4.1 第 2 条、§4.2 第 1–2 条）和 digest 的计算方式（§4.3，D14.2 / D13.1）。
- digest 与 Go 不同，影响按 digest 跨节点关联的诊断和比对（§4.3 第 2 条）。
- PoC crate（`tidb-sql`、`poc-executor`、`poc-session`）中的协议类型渗透不修，随 PoC 退役消失。
