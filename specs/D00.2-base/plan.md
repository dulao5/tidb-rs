# D00.2 常量、错误码与配置：plan（第一部分）

## 1. Rust 模块布局

crate `crates/base`（包名 `tidb-base`，N0 rank 0）。N0 把几个 Go 包放进同一个 crate，每个 Go 包对应一个模块，Go 文件对应 Rust 文件。

| Go | Rust | 说明 |
|---|---|---|
| `pkg/parser/mysql/*.go` | `mysql/*.rs` | `const.go` → `const.rs`（`r#const` 不便，用 `#[path]` 挂成 `mod const_`），`type.go` 同理。数据部分在 `*_gen.rs`（生成），手写文件 `pub use` 它们 |
| `pkg/parser/terror/terror.go` | `terror.rs` + `terror_registry.rs`（生成） | |
| `pkg/errno/*.go` | `errno/` | `errcode.rs`、`errname.rs` 全部生成 |
| `pkg/util/dbterror/terror.go` | `dbterror/mod.rs` | |
| `pkg/util/dbterror/ddl_terror.go`、`exeerrors/`、`plannererrors/` | `dbterror/ddl_terror.rs`（`pub use` 到 `dbterror::`）、`dbterror::exeerrors`、`dbterror::plannererrors` | 原型全部生成；每个文件另有 `ALL`（按名字列出原型，测试和工具用） |
| `pkg/kv/kv.go`（`StoreType`） | `kv.rs` | N0 下沉；`pkg/kv` 其余部分是 D07.0 |
| pingcap/errors | `errors.rs` | go-dep（§1.1 of spec） |
| Go fmt、strconv 格式化 | `gofmt.rs`、`gostd/ftoaryu.rs`（最短浮点）、`gostd/powers.rs`（10 的幂表） | go-std |
| Go unicode | `gostd/` | go-std；类别表生成 |
| — | `gomap.rs` | glue：Go 只读 map 字面量 → 排好序的静态表 |

## 2. 生成的数据（tracker `govec/cmd/rsgen`）

数据不手抄。rsgen 读 v8.5.8 源码（`go list -m` 定位模块缓存，与 govec 的 go.mod 一致）：

- **常量**：名字、顺序、注释取自 Go AST，值取自 go/types 对源码的求值（`iota`、`1<<24 - 1` 等表达式都由 Go 自己算）。类型按 `M-INT` 映射；无类型整数常量按所在文件的用途给定类型：错误码 `u16`，`type.go` 的 `MaxInt24` 等 `i64`，其余 `isize`（Go 的默认类型 `int`）。具名类型（`SQLMode`、`PrivilegeType`、`PriorityEnum`）是 newtype。
- **map 字面量**：键、值按 AST 原样（键保持常量名，如 `(ErrDupEntry, ErrMessage::new(...))`），按键值排序成 `GoMap` 静态表；切片字面量照源码顺序。
- **运行时才有的表**：错误注册表（govec 进程链接 tidb-server 的包集合）、395 个原型（`NewStd` 生成 `ClassX.new_std(errno::ErrY)`，与 Go 一样从表里取消息；`NewStdErr` 的消息在 Go 中常由 `fmt.Sprintf` 在初始化时拼出，生成时写入拼好的字面量）、unicode 类别表。
- 生成后 `cargo fmt -p tidb-base`。每张生成的表都由 govec 的整表记录核对（govec `base` 的 `table` op），生成器的错会表现为向量不一致。

## 3. 类型与写法映射

| Go | Rust | 等级 |
|---|---|---|
| `error`（`*errors.Error` 与其他） | `tidb_base::Error`（§4 of spec，`M-ERR`） | L1 [M-ERR] |
| Go `string`（消息、错误文本、`fmt` 参数与结果） | 字节：`Vec<u8>` / `&[u8]` / `Cow<'static, [u8]>`；构造接口用 `IntoGoString`（字面量、`String`、字节） | L1 [M-ERR] |
| `fmt.Sprintf(format, args ...interface{})` | `gofmt::sprintf(format: impl AsRef<[u8]>, &[Arg]) -> Vec<u8>`，`Arg` 带 Go 动态类型；`go_args![...]` 构造；Go string 字节用 `Arg::go_string`，`[]byte` 用 `Arg::Bytes` | go-std |
| 包级 `map` 字面量（只读） | `GoMap<K, V>` 静态表，`get` 二分查找；迭代按键序（Go 随机序，`M-MAPORDER`） | glue |
| 包级 `var ErrX = ClassY.NewStd(...)` | `pub static ErrX: LazyLock<Error>` | L1 [M-ERR] |
| `type SQLMode int64` 等具名整数类型 | newtype + 需要的位运算 trait | L1 |
| 包级可变变量 `TiDBReleaseVersion`、`ServerVersion` | `LazyLock<parking_lot::RwLock<String>>` | L1 [M-LOCK] |
| `sync.Mutex` 保护的 `errno` 统计 | `parking_lot::Mutex<Option<…>>`，首次访问初始化（Go 在 `init` 中初始化） | L1 [M-LOCK]，`init` 为 L2 |
| `time.Time`（统计的时间） | `SystemTime`；零值 `LastSeen` 为 `None` | L1 |
| `log.Error/Warn/Fatal`（pingcap/log） | `log` crate；`Fatal` 记日志后 `process::exit(1)` | L1 |
| Go 方法名与 Rust trait 方法同名（`String`、`Error`） | 固有方法 `string()`、`error()`，`Display` 委托 | L1 [M-NAME] |

**命名约定（映射规则 `M-GONAME`，评审已确认）**：常量、静态量（含 395 个原型）和数据表保持 Go 原名（`mysql::TypeLonglong`、`mysql::ErrDupEntry`、`terror::ClassDDL`、`dbterror::plannererrors::ErrUnknownColumn`），便于与 Go 逐个 grep 对照。函数和方法仍按 `M-NAME` 用 snake_case（`formatENUS` → `format_en_us`，来源标注写 Go 名），类型用 CamelCase（Go 的 `lengthAndDecimal` → `LengthAndDecimal`、`code2ErrClassMap` → `Code2ErrClassMap`）。workspace 的 `style-lint-allow` 只增加 `non_upper_case_globals`，并且只在声明或在 `match` 中使用 Go 名常量的文件里模块级放开（生成的文件自带）；crate 级不放开。

## 4. L2 偏离与已知差异

| 位置 | 偏离 | 影响 |
|---|---|---|
| `terror.RegisterErrorClass`、`code2ErrClassMap.Get/Put`、`newCode2ErrClassMap`、`ErrClass.initError` | 静态注册表（spec §4 第 5 条） | 无：tidb-server 在启动后冻结注册表 |
| `terror.init`、`errno.init` | 常量初始化 / 首次访问初始化 | 无 |
| `PriorityEnum.Restore` | `format.RestoreCtx` 换成 trait `RestoreKeyWord`（§5） | 无 |
| `Privileges.Has` | 切片类型别名上的方法 → 自由函数 `privileges_has` | 无 |
| `errors.Error.fillLineAndFile` | `#[track_caller]` 代替 `runtime.Caller` | 文件路径是 Rust 源文件 |
| gofmt：非 `Format` 的 Stringer 在错误输出中 | Go 用反射打印值，port 打印方法结果 | TiDB 的错误参数中未见；向量中无此情况 |
| gofmt：`%+v` 的 pingcap 错误 | Go 打印调用栈，port 只打印消息 | 只用于日志 |
| `strings.ToUpper`（`FormatSQLModeStr`、`Str2Priority`） | 多字符大写映射（如 `ß`）保持原字符，其余用 Rust 映射 | 与 Go 的简单映射一致；向量覆盖 `ß` |


## 5. 反向引用与处理（N0 §4）

`check_layers.py --block-edges` 中涉及 `tidb-base` 的两处，以及 Lucky 在 N0 评审中指出的 P1 内的另两处：

| 引用 | 实际引用 | 处理 |
|---|---|---|
| `tidb-base` → `tidb-parser`（2） | `PriorityEnum.Restore` → `format.RestoreCtx`、`RestoreCtx.WriteKeyWord` | **接口反转**（N0 §4 第 2 条）：`tidb-base` 定义 trait `mysql::RestoreKeyWord { fn write_key_word(&mut self, &str) }`，`PriorityEnum::restore(&mut dyn RestoreKeyWord)`；parser 的 `RestoreCtx` 实现它（parser 移植到用到时）。本 PR 不改 parser |
| `tidb-metrics` → `tidb-base` 同层（1） | `metrics.ExecuteErrorToLabel`（`pkg/metrics/server.go`）用到 `terror.Error` 类型 | 建议 tracker 把 `ExecuteErrorToLabel` 改归 D00.2（它是"错误 → 标签字符串"的纯函数，不依赖 prometheus）。另一个办法是 metrics 升到 rank 1（但 memory → metrics 有 3 处，会变成同层）。tidb-metrics 已是正式 crate，本 PR 不动，留给 metrics 的下一次修改 |
| `tidb-types` → `tidb-sessionctx`（3） | `types.Context.AppendWarning`、`NewContext` → `util/context.WarnAppender`（接口） | **接口下沉**：`WarnAppender` 只有 `AppendWarning`/`AppendNote` 两个方法，建议 tracker 把 `pkg/util/context/warn.go` 的接口类型（及 `funcWarnAppender`）改归 D00.2 或 D01.1，`tidb-base`/`tidb-types` 定义 trait，D00.1 实现。D01.1 开工时处理 |
| `ColumnNeedRestoredData`（`pkg/types/etc.go`，现归 D01.1） | 被 meta/model 一侧使用 | 按 Lucky 的建议，tracker 改归 D06.1（`tidb-model`）。D01.1 / D06.1 开工时处理 |

后两项和 metrics 一项都要改 tracker 的 `rules.py`，不在本 PR 内；记在这里，作为对应块 plan 的输入。

## 6. 测试

- `tests/govec_base.rs` 回放 `tests/vectors/v8.5.8/<目标>/base.jsonl.gz`（从 tracker 复制，见 tasks T9 的 sha256）。比对规则：`out` 完全相等；Go 返回错误时比对 `Error()` 文本；Go panic 的记录（`FORMAT` 对过短输入的越界，32 条）Rust 也必须 panic。
- 单测：`gofmt` 基本格式、注册表有序且无重复、`ErrCritical`。
- 发布检查（`release-checked`）不加 `tidb-base`：本块没有依赖回绕的整数运算（`M-INT` 的 `as` 截断只在错误码转换处，向量已覆盖）。
