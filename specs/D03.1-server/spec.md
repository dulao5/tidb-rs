# D03.1 连接与协议（pkg/server）：块 spec

状态：第 1 部分待评审（2026-11-15）。工作项是 VS1 的"结果编码与预处理语句协议"（VS1-16，[VS1 plan](../VS1-vertical-slice/plan.md) §6 第 1 阶段），含本块 [tasks](tasks.md) P1-1（二进制参数，从表达式块 D08.12 移交）。第 2 部分（连接循环、握手、认证）还没有排期。

## 1. 为什么重要

新栈（移植的会话，VS1-14）已经能执行语句，但它的结果要变成 MySQL 协议的包才能发给客户端。在本项之前，`newstack.rs` 在语句执行**之后**才发现结果没法编码，返回 `blocked-by:D03.1` 让路由器回退到 PoC——这违反 plan §4.3（回退只能发生在执行器打开之前），路由器把它记为 bug（#235）。

客户端看到的每个字节都由本块决定：列定义（名字、类型、长度、字符集、标志）、文本协议和二进制协议的行、预处理语句的参数解码。和 Go 不同的字节就是客户端可见的不兼容，所以验收的核心是：**同样的输入，Go 与 Rust 写出的包逐字节相同，解出的参数相同，出错和 panic 的情况也相同**。

## 2. 范围

新 crate `tidb-server`（`crates/server`，N0 第 16 层，`layers.toml` 中原来是 planned）。

| Go 文件 | Rust | 函数 |
|---|---|--:|
| `internal/dump/dump.go`：长度编码整数 / 字符串、二进制 TIME / DATETIME | `dump.rs` | 7 |
| `internal/column/column.go`：列定义 `Info.Dump`、`DumpTextRow`、`DumpBinaryRow` | `column.rs` | 9 |
| `internal/column/convert.go`：`ConvertColumnInfo` | `convert.rs` | 1 |
| `internal/column/result_encoder.go`：按 `@@character_set_results` 转码 | `result_encoder.rs` | 8 |
| `internal/util/util.go`：`AppendFormatFloat`、长度编码解析、输入解码器 | `util.rs` | 7（另 3 个永久不移植） |
| `internal/resultset/resultset.go`：结果集 | `resultset.rs` | 9（另 2 个见 §3） |
| `conn.go`：`writeResultSet`、`writeChunks`、`writeColumnInfo`（另有本部分清单外的 `initResultEncoder`、`initInputEncoder`） | `conn.rs` | 3 |
| `conn_stmt_params.go`：`parseBinaryParams` | `params.rs` | 1 |
| `pkg/expression/util.go`：`ExecBinaryParam` 与 6 个 `binary*` 辅助函数（tasks P1-1） | `tidb-expression` 的 `util.rs` | 7 |
| `pkg/param`：`BinaryParam` | `tidb-expression` 的 `param.rs` | （类型） |

另外，#505（驱动连接时的 SET 与读取）提前移植了第 2 部分的 `server.go` `setSystemTimeZoneVariable`（`server.rs`）：服务器启动时把 `mysql.tidb` 的 `system_tz` 写进实例变量 `system_time_zone`，值为空或 `System` 时保留默认值 `CST` 并按 Go 的消息记错误日志。Go 用 `sync.Once` 保证只执行一次，这里由唯一的调用点（原型服务器启动，`poc_session::TidbTable::load_system_tz`，与 `new_collation_enabled` 共用一次 `mysql.tidb` 读取）保证。时区值本身由 `pkg/util/timeutil` 的 `SetSystemTZ` / `GetSystemTZ`（日志与工具块 D00.3，`tidb_base::timeutil`）保存。

converge（`--part ... --item VS1-16`）：本部分列出的 96 个函数中移植 45 个，全部匹配，没有桩；其余 51 个在 `functions.tsv` 的 `not_ported` 列写明原因（见 §3）。块内另外 205 个函数属于第 2 部分。

## 3. 怎么接到 PoC 的连接上

连接本身（读包、握手、命令分派、EOF / OK 包）仍然是 PoC 的 `tidb-mysql-protocol`。因此：

- `writeChunks` 把列定义和每一行编码成**包的内容**（`ResultPackets`），由 PoC 的连接按顺序发出：列数、列定义、（老客户端的）EOF、各行、最后的 EOF 或带 EOF 头的 OK。Go 在 `writeChunks` 里自己写 EOF；这里 EOF 由 PoC 写，因为只有它知道客户端的能力位。字节与 Go 相同。
- 协议层新增 `Response::Encoded`（已编码的结果集）、`PreparedInfo::column_packets`（PREPARE 的列定义包），以及 `SessionHandler::execute_raw_params`：`COM_STMT_EXECUTE` 的参数段原样（NULL 位图、类型、值、长数据）交给新栈，由 Go 的 `parseBinaryParams` 解码。PoC 自己的解码照旧。
- `newstack.rs`：`COM_QUERY` = `ExecuteStmt` → `writeResultSet(text)` → 关闭结果集；`COM_STMT_PREPARE` = `PrepareStmt` → `ConvertColumnInfo` → 用连接当前的结果编码器（Go 的 `cc.rsEncoder`：上一个结果集的，第一次之前是默认的）写列定义；`COM_STMT_EXECUTE` = `parseBinaryParams` → `ExecBinaryParam`（会话当前语句上下文的类型上下文）→ `ExecutePreparedStmt` → `writeResultSet(binary)`。语句执行之后不再返回 `Blocked`。
- 错误包：新栈的错误由路由器胶水（`router/mod.rs` 的 `sql_error`）按 `writeError` 选出错误码、SQLSTATE 和消息（#486）：TiDB 错误走 `terror.ToSQLError`（类登记过的码原样，否则 1105），其他错误是 1105 加错误文本；SQLSTATE 由码查 Go 的 `MySQLState` 表（`tidb-mysql-protocol` 的 `sql_state`，PoC 的错误也用它），没有的是 `HY000`。边界桩的错误（`blocked`）是 tidb-rs 自己的，保留 1235。写包本身仍是 PoC 的（老客户端不带 `#` 和 SQLSTATE，与 Go 相同）。
- 不移植的部分（`functions.tsv` 的 `not_ported`）：`handleQuery` / `handleStmt` / `writeOK` / `writeEOF` / `writeError`、`conn_stmt.go` 的执行与关闭、`driver_tidb.go` 的 `TiDBContext` / `TiDBStatement`、`BufferedReadConn`——它们属于连接循环（第 2 部分），本部分的胶水代码做了其中结果与参数的步骤；游标（`writeChunksWithFetchSize`、`cursor.go`、`OnFetchReturned`）——路由器把游标留在 PoC（plan §4.3 第 0 步），`COM_STMT_FETCH` 是 D12.12；`SetPreparedStmt`（预处理语句的列定义缓存）——随预处理语句规划（D13.1）；`CorsHandler`、`NewTestConfig`——HTTP 状态服务和 Go 测试用，永久不移植。

## 4. 与 Go 的差异

1. **结果集先在内存中编完再发**（PoC 的 `Response` 是物化的）。字节相同；差别只在出错时：Go 已经发出的列定义和行，这里一起丢掉，只发错误包。客户端对两者都按"语句失败"处理。
2. `writeChunks` 出错时 Go 还返回"是不是第一次 `Next` 就失败"，供 TiFlash 失败后改读 TiKV 的重试用；新栈没有这个重试，这个标志不保留。
3. `ConvertColumnInfo` 不填 `DefaultValue`：Rust 的 `ResultField` 只有列的 `FieldType`，没有 `ColumnInfo`。只有 `COM_FIELD_LIST` 发默认值，新栈不处理这个命令。
4. **Go 会 panic 的输入，Rust 也 panic**（由连接循环恢复，见 §6）：
   - 二进制参数中用长度编码的 NULL（`0xfb`，不是 NULL 位图）表示的 DECIMAL：Go 得到一个没有值的 decimal datum，推断参数类型时解引用空指针。Rust 的 datum 存不下"没有值的 decimal"，在同一个位置按 Go 的消息 panic。**可能的上游 bug**（只有手工构造的包会触发）。
   - 长度编码整数被截断（例如值段只有 `0xfc` 一个字节）：Go 的 `ParseLengthEncodedInt` 越界。长度大于 `MaxInt64` 时 Go 的长度检查溢出后通过、切片越界。**可能的上游 bug**（畸形包让连接 panic，而不是返回 malformed packet）。
   - 小数位是 31（`NotFixedDec`）的 TIME 列走文本协议：`Duration.String` 越界。`ConvertColumnInfo` 不会产生这样的列，实际到不了。

## 5. 验证

- govec 领域 `protocol`（tracker `govec/domains/protocol.go`，3830 条记录）：列定义（各类型 × 7 种 `character_set_results`，带 / 不带默认值）、`ConvertColumnInfo`、文本 / 二进制行（26 种列类型含 JSON、向量、ENUM / SET、非法类型，随机多列行）、`AppendFormatFloat`、二进制参数（每种类型、随机多参数、长数据、NULL 位图、截断的值、未知类型，三种类型标志，然后 `ExecBinaryParam`）。Go 的 panic 记为 `panic`，Rust 必须同样 panic。Rust 重放：`crates/server/tests/govec_protocol.rs`，全部一致。
  - Go 的 `pkg/server/internal/...` 是 internal 包，govec 模块不能导入。tracker 新增一个**导出垫片**：`govec/fixes/pkg/server/govec_export.go.fixed`（`scripts/govec-fixes.sh` 把它加进 govec 链接的模块副本），只调用原函数、不改行为。
  - P1-1 的 7 个函数原来在 tracker 的豁免表里写着"owned by D03.1"，现在由本领域覆盖，豁免删除。
- 单测：`util.rs` 中 govec 走不到的长度编码辅助函数。
- 路由器 / 会话 / 连接见 §6。
- `tools/protocol-smoke`：本机没有 PD / TiKV，没有跑（需要真实集群）。

## 6. 路由器与连接循环的三处修改（VS1-16 的附带要求）

1. **执行之后不再回退**：`newstack.rs` 编码结果，不再在执行后返回 `Blocked`。路由器如果仍然在执行器打开之后收到 `Blocked`（新栈的 bug），**不回退**：语句在新栈结束，客户端收到该桩的 1235（宪章第五条），`tidbrs_route_total{engine="new",reason="bug_blocked_after_open"}` 计数（测试 `blocked_after_open_is_an_error_not_a_fallback`）。原来的做法是记 bug 但仍回退，PoC 会把可能已经读过数据、改过会话状态的语句再执行一遍。
2. **"执行器已打开"的计数挪到打开之后**：`ExecStmt` 新增 `executor_opened`（不在 Go 中），只有执行器构建并打开成功才置位，会话的 `executed_stmts` 只在置位时加一（普通路径和点查短路径都一样）。构建器返回 `Blocked`（TableDual、FOR UPDATE、缓存表、事务范围这些桩）是正常回退，不再被当成"打开之后"（测试 `executed_stmts_counts_only_opened_executors`：TableDual 计划的构建器桩不计数，正常计划计一次）。
3. **连接循环的 panic 与 Go 的 `clientConn.Run` 相同**：执行命令时 panic，记日志，把 panic 的消息作为错误（1105）发给客户端，`tidb_server_panic_total{type="session"}` 加一（Go 的 `PanicCounter`，名字和帮助文字相同），然后关闭连接。会话随连接一起丢弃，所以不会留下执行了一半的状态。测试 `crates/mysql-protocol/tests/panic_recover.rs`：表里有 `_tidb_row_checksum` 时读一行旧格式（v1）的行，Go 和 Rust 都在 `decodeOldRowValToChunk` 里 panic（找不到校验和列的列信息）；客户端收到错误、计数加一、连接关闭、服务器继续接受新连接。

## 7. 不做

- MDL、schema lease、读时间戳的准入（N11）：项目负责人还没有确认 N11，本项不实现，也不加桩；新栈的元数据锁仍然停在会话的已有桩（D05.4）。
- 连接循环、握手、认证插件、`COM_FIELD_LIST`、`COM_CHANGE_USER`、游标：第 2 部分或其他块。
