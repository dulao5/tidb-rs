# D00.2 常量、错误码与配置：块 spec

状态：草案（2026-11-07；同日按 PR #6 Lucky 评审修订：Go 字符串改为字节串、最短浮点表示按 L1 移植、大宽度、命名规则 M-GONAME、`go-dep` 标注）。阶段：P1 第一个块（ROADMAP §4 L1；P0 决定 D38）。基线：Go TiDB v8.5.8（`8b857efa2`）。
依据：[CONSTITUTION.md](../../CONSTITUTION.md)、[mappings.md](../../docs/porting/mappings.md)、[N0 crate 分层](../exceptions/N0-crate-layering/spec.md)（本块落在 `tidb-base`，rank 0）。

本块分两次提交：**第一部分**（本 PR）是 `pkg/parser/mysql`、`pkg/parser/terror`、`pkg/errno`、`pkg/util/dbterror`、`kv.StoreType`，即 tidb-rs 的错误体系和常量；**第二部分**是 `pkg/config`（44 个函数，见 §1.2），单独提交。两部分都在 crate `tidb-base` 中。

## 1. Go 范围

函数清单来自 tracker `data/func_blocks.tsv`（block = D00.2，137 个函数，函数体 1,291 行）。除函数外，本块的主体是**数据**：错误码常量（`parser/mysql` 954 个、`errno` 1,151 个）、错误信息表（952 + 1,149 条）、SQLSTATE 表（244 条）、字符集与排序规则表、类型与协议常量、权限表、`dbterror` 的 395 个错误原型。

### 1.1 第一部分（本 PR）：93 个函数

| Go 文件 | 函数数 | 函数体行数 | Rust 文件 |
|---|--:|--:|---|
| `pkg/parser/mysql/const.go` | 24 | 130 | `mysql/const.rs`（数据：`const_gen.rs`） |
| `pkg/parser/mysql/type.go` | 16 | 48 | `mysql/type.rs`（`type_gen.rs`） |
| `pkg/parser/mysql/privs.go` | 6 | 34 | `mysql/privs.rs`（`privs_gen.rs`） |
| `pkg/parser/mysql/util.go` | 4 | 26 | `mysql/util.rs`（`util_gen.rs`） |
| `pkg/parser/mysql/locale_format.go` | 3 | 84 | `mysql/locale_format.rs` |
| `pkg/parser/mysql/error.go` | 3 | 35 | `mysql/error.rs` |
| `pkg/parser/mysql/charset.go` | 2 | 19 | `mysql/charset.rs`（`charset_gen.rs`） |
| `pkg/parser/mysql/errname.go` | 1 | 3 | `mysql/errname.rs`（`errname_gen.rs`） |
| `pkg/parser/mysql/errcode.go`、`state.go` | 0 | — | `mysql/errcode.rs`、`mysql/state.rs`（全部生成） |
| `pkg/parser/terror/terror.go` | 23 | 172 | `terror.rs`（注册表：`terror_registry.rs`） |
| `pkg/errno/infoschema.go` | 9 | 100 | `errno/infoschema.rs` |
| `pkg/errno/errcode.go`、`errname.go` | 0 | — | `errno/errcode.rs`、`errno/errname.rs`（全部生成） |
| `pkg/util/dbterror/terror.go` | 1 | 3 | `dbterror/mod.rs` |
| `pkg/util/dbterror/ddl_terror.go`、`exeerrors/errors.go`、`plannererrors/planner_terror.go` | 0 | — | `dbterror/{ddl_terror,exeerrors,plannererrors}.rs`（395 个原型，全部生成） |
| `pkg/kv/kv.go`（只有 `StoreType`，N0 下沉） | 1 | 10 | `kv.rs` |

**块外但必须移植的依赖**（不是 tidb 仓库的代码，按 ON-DEMAND 工具代码处理，标注 `go-std:`）：

| 来源 | 内容 | Rust |
|---|---|---|
| `github.com/pingcap/errors` `normalize.go`、`errors.go`（标注 `go-dep:`，见 provenance.md） | `Error` 类型（错误码、RFC 码、消息模板、参数、cause）、`Normalize` 及选项、`GenWithStack*`、`FastGen*`、`Equal`、`RedactErrorArg`、`RedactLogEnabled`、`redactFormatter`、`New`/`Errorf` | `errors.rs` |
| Go `fmt`（`print.go`、`format.go`）、`strconv`（`ftoa.go`、`ftoaryu.go` 的最短表示、`quote.go`、`IsPrint`）、`unicode/utf8` | `Sprintf`/`Sprint` 对动态类型参数的格式化：所有动词（含 `%U`）、标志、宽度与精度（到 Go 的上限 1e6）、参数下标、错误输出；最短浮点表示按 Go 的 Ryū 移植（半数取偶）；`%q` 的可打印判断用从 Go 生成的 `IsPrint` 表 | `gofmt.rs`、`gostd/ftoaryu.rs` |
| Go `unicode`（`letter.go`、`digit.go`、`tables.go`） | `Is`、`IsOneOf`、`IsDigit` 和 22 个类别表（`RangeGraph` 用） | `gostd/` |

### 1.2 第二部分（单独 PR）：`pkg/config`，44 个函数

`config.go` 38 个（`Config` 结构体及其 TOML/JSON 编解码、`NewConfig`、`Load`、`Valid`、`InitializeConfig`、`RemovedVariableCheck`、全局配置的存取、`nullableBool`、`AtomicBool` 等），`config_util.go` 6 个（`CloneConf`、`MergeConfigItems`、`FlattenConfigItems` 等）。它依赖 client-go 的 `TiKVClient`/`PDClient`/`Security` 配置结构、`logutil` 的日志配置、jaeger 的 tracing 配置和 BurntSushi/toml 的元数据（未解码项检查），工作量与第一部分相当，所以拆开。拆分期间 converge 报告 `pkg/config/*.go` 缺失（预期）。

### 1.3 不移植

| Go | 理由 |
|---|---|
| `dbterror.ReorgRetryableErrCodes`、`ReorgRetryableErrMsgs`（`ddl_terror.go` 的两个变量） | 只用于 DDL reorg 的重试判断；DDL 由 Go TiDB 承担（ROADMAP X） |

`mysql.RangeGraph`（规划器 D09.1 检查列名用）和它需要的 `unicode` 类别表**已移植**（表由 rsgen 从 Go 1.25.12 生成）。

## 2. 对外接口

- **提供**：错误类型 `errors::Error` 和 `M-ERR`（§4）；`terror::ErrClass` 和 27 个错误类、`NewStd`/`NewStdErr`/`Synthesize`、`ToSQLError`、`ErrorEqual`；`dbterror::ErrClass` 和 395 个错误原型（`LazyLock<Error>`，名字与 Go 相同，如 `dbterror::plannererrors::ErrUnknownColumn`）；`mysql::*`、`errno::*` 的全部常量和表；`mysql::SQLError`、`NewErr`；`SQLMode`、`PrivilegeType`、`PriorityEnum`；`gofmt::{sprintf, sprint, Arg}`、`go_args!`。
- **消费方**：几乎所有块（错误码与错误原型）。引用数前列：D08.\*（内置函数）、D09/D10（规划器）、D12（执行器）、D03.1（协议层，`SQLError`、协议常量）。

## 3. 依赖与边界桩

`tidb-base` 是 rank 0，不依赖任何 tidb-rs crate。本块的 Go 代码只引用块外的 `pingcap/errors`、`pingcap/log`（zap）、`fmt`、`strconv`、`unicode`、`parser/format`。前三者见 §1.1 的 go-std；日志用 `log` crate；`parser/format` 见 plan §5（接口反转）。**没有边界桩。**

## 4. 映射规则 `M-ERR`（本块定义）

mappings.md 中 `M-ERR` 的依据是本 spec。规则：

1. **一个具体错误类型**：tidb-rs 的 Go `error` 一律映射为 `tidb_base::Error`（`Result<T, Error>`）。它是 `*errors.Error` 的移植：错误码、RFC 码文本、消息模板、`Arg` 参数、脱敏位置、可选的 cause、创建点（`#[track_caller]` 的文件和行）、可选的调用栈（只有设置了 `RUST_BACKTRACE` 才采集）。字段放在 `Box` 里，`Result<T, Error>` 只有一个指针宽。
2. **非 terror 的 Go 错误**（`errors.New`、`fmt.Errorf`、标准库错误）也是 `Error`，没有错误码（`Error::plain`），`Error()` 只有消息本身，`Equal` 永远为 false。
3. **不移植包装**：`errors.Trace`、`Annotate`、`AddStack`、`WithStack` 不移植，直接返回错误；`errors.Cause(err)` 就是 `err`。Go 中这些包装对 `Error()`、`Cause`、`Equal`、`ErrorEqual` 都是透明的，所以行为不变。`%+v` 打印的 Go 调用栈没有对应物。
4. **原型是 `LazyLock<Error>` 静态量**，名字与 Go 变量相同。Go 在包初始化时执行 `ClassX.NewStd(code)`，Rust 在第一次使用时执行，结果相同。
5. **注册表是静态表**：Go 的 `ErrClassToMySQLCodes` 由所有包的初始化填充，tidb-server 启动后冻结（`RegisterFinish`）。Rust 用从 tidb-server 的包集合生成的静态表（656 个错误码），`initError` 只检查不插入（debug 断言）。`ToSQLError` 对未注册的码返回 1105，与 Go 相同。
6. **Go 的 string 是字节串**：消息模板、`Error()`、`GetMsg()`、`SQLError` 的消息和 `fmt` 的 Go `string` 参数都是字节（`Vec<u8>`、`Cow<'static, [u8]>`、`Arg::Str`），非 UTF-8 的字节按 Go 的规则处理（宽度和精度中每个非法字节算一个 rune，`%s` 原样输出，`%q` 输出 `\xNN`）。例如 `CAST(0xFF41 AS SIGNED)` 的 warning 中原始字节 `ff 41` 原样保留。`Display` 只用于日志，有损转换。Go 的 `[]byte` 参数是 `Arg::Bytes`；字节没有到 `Arg` 的 `From` 转换，必须显式选 `Arg::go_string` 或 `Arg::Bytes`，避免混淆。
7. **错误码常量是 `u16`**（MySQL 线上格式），`ErrClass::new_std` 等接受 `impl IntoErrCode`（Go 的无类型常量隐式转换）。

## 5. 验收

1. **函数级（govec，tracker 领域 `base`）**：`vectors/v8.5.8/<目标>/base.jsonl.gz`，28,047 条记录全部回放通过（其中 20,049 条是随机格式化 `sprintf_rand`；非 UTF-8 的字符串在向量中以 hex 编码）：
   - 26 张整表（两张错误信息表、SQLSTATE、字符集与排序规则、SQL mode、权限、错误注册表、395 个原型等）与 Go 运行时一致，覆盖全部生成的数据；
   - 1,795 条 `fmt.Sprintf`（两张表和全部原型的每个消息模板，以及 42 种动词/标志组合 × 12 个随机类型参数）、30 条 `fmt.Sprint`；
   - 600 + 60 条 `NewErr`/`NewErrf`，1,043 条 `terror`/`dbterror` 的 `NewStd`+`GenWithStackByArgs`（注册表中的每个 (类, 码)，三种脱敏模式随机），395 条原型，150 条 `ToSQLError`，以及 SQL mode、权限、字符集、类型默认值、`FORMAT`、`errno` 统计、2,756 条 unicode 表查询等。
   - 目标：两个目标的记录相同（本领域没有 FMA 相关的浮点运算）。arm64 的文件在本地生成，amd64 的文件取自 tracker PR 的 CI 产物（作者机器不能运行 amd64）。
   - 另外用评审中的差分 fuzz 复核（不入库）：30 万组随机格式与参数（含非 UTF-8）、100 万个 float32/float64 的 `%v %g %e %G %E` 与带精度格式，与 Go 逐字节一致，无 panic。
2. **来源标注**：第一部分 93 个函数 100% 有 `// go:` 标注；`go-std`（Go 标准库）、`go-dep`（pingcap/errors）、`glue` 覆盖其余函数；converge 对第一部分 `matched 93/93, missing 0`，只报告第二部分的两个文件缺失。
3. **分层**：`check_layers.py` 通过（`tidb-base` rank 0，无 tidb-rs 依赖）。
4. **单测**：Go 的 `errname_test.go`、`terror_test.go`、`const_test.go`、`privs_test.go`、`type_test.go`、`error_test.go`、`infoschema_test.go` 没有逐个翻译。govec `base` 的用例是生成的（每个模板、每个注册码、每张表），没有抄这些测试的用例表；这些测试检查的性质（表无重复且完整、格式化、类与 RFC 码、`ErrorEqual`、SQL mode、权限、统计计数）都在向量中。Rust 侧另有几个单测（gofmt 基本格式、注册表有序、`ErrCritical`）。
