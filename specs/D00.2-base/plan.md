# D00.2 常量、错误码与配置：plan

§1–§6 是第一部分（PR #6），§7 是第二部分（`pkg/config`）。

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
| gofmt：`%p`（`%P`）用于 `[]byte` 参数 | Go 打印切片底层数组的地址（`0xc000…`，每次运行不同），port 没有地址可打印，按其他非法动词的方式逐元素输出（`[%!p(uint8=1) …]`） | 地址无意义且不可复现；TiDB 的错误参数中没有 `%p`；向量的随机格式不生成 `%p` + `[]byte` |


## 5. 反向引用与处理（N0 §4）

`check_layers.py --block-edges` 中涉及 `tidb-base` 的两处，以及 Lucky 在 N0 评审中指出的 P1 内的另两处：

| 引用 | 实际引用 | 处理 |
|---|---|---|
| `tidb-base` → `tidb-parser`（2） | `PriorityEnum.Restore` → `format.RestoreCtx`、`RestoreCtx.WriteKeyWord` | **接口反转**（N0 §4 第 2 条）：`tidb-base` 定义 trait `mysql::RestoreKeyWord { fn write_key_word(&mut self, &str) }`，`PriorityEnum::restore(&mut dyn RestoreKeyWord)`；parser 的 `RestoreCtx` 实现它（parser 移植到用到时） |
| `tidb-metrics` → `tidb-base` 同层（1） | `metrics.ExecuteErrorToLabel`（`pkg/metrics/server.go`）用到 `terror.Error` 类型 | **第二部分已处理**：tracker 把 `ExecuteErrorToLabel` 改归 D00.2，移植在 `metrics.rs`（只读 `RFCCode`，不依赖 prometheus）。tidb-metrics 中按错误码近似的 `error_label(code)` 在 metrics 下次修改时改为调用它 |
| `tidb-types` → `tidb-sessionctx`（3） | `types.Context.AppendWarning`、`NewContext` → `util/context.WarnAppender`（接口） | **第二部分已处理**：接口 `WarnAppender`（及其两个方法声明）改归 D00.2，`tidb-base` 的 `contextutil::WarnAppender` trait。选 base 而不是 types：使用方是 `types.Context`（D01.1）和 `errctx`（D00.1）两侧，接口只依赖 `Error`，放在 rank 0 两侧都能用，也不必让 `errctx` 为一个 trait 依赖 `tidb-types`。`funcWarnAppender` 等实现留在 D00.1 |
| `ColumnNeedRestoredData`（`pkg/types/etc.go`，原归 D01.1） | 参数是 `*model.IndexColumn`、`[]*model.ColumnInfo` | **第二部分已处理**：改归 D06.1。留下一处 `tidb-model` → `tidb-types` 同层引用（它调用 `types.NeedRestoredData`，后者依赖 collate，不能一起下移）；需要 D06.1 / D01.1 开工时决定 model 与 types 的相对 rank，或把这个函数放到两者之上的调用方 |

tracker 的 `scripts/blocks/rules.py` 增加三条规则（PR 见 tasks T11），重新生成 `func_blocks.tsv`、`block_edges.tsv` 后，`check_layers.py --block-edges` 的向上/同层引用从 130 降到 124：

```text
- cross-crate Go references: 58821, downward 58691 (99.78%), upward/same-rank 130
+ cross-crate Go references: 58844, downward 58720 (99.79%), upward/same-rank 124
-    3  tidb-types -> tidb-sessionctx  D01.1->D00.1(3)
-    3  tidb-types -> tidb-model (same rank)  D01.1->D06.1(3)
+    1  tidb-model -> tidb-types (same rank)  D06.1->D01.1(1)
-    1  tidb-metrics -> tidb-base (same rank)  D14.1->D00.2(1)
```

## 6. 测试

- `tests/govec_base.rs` 回放 `tests/vectors/v8.5.8/<目标>/base.jsonl.gz`（从 tracker 复制，见 tasks T9 的 sha256）。比对规则：`out` 完全相等；Go 返回错误时比对 `Error()` 文本；Go panic 的记录（`FORMAT` 对过短输入的越界，32 条）Rust 也必须 panic。
- 单测：`gofmt` 基本格式、注册表有序且无重复、`ErrCritical`。
- 发布检查（`release-checked`）：`cargo test -p tidb-base --profile release-checked` 与 debug 同样通过（溢出检查打开）；CI 的 release-checked 作业不单列 `tidb-base`。

## 7. 第二部分：`pkg/config`

### 7.1 模块布局

| Go | Rust | 说明 |
|---|---|---|
| `pkg/config/config.go`、`const.go` | `config/mod.rs` | 常量、包级变量、`Load`、`Valid`、全局配置、`InitializeConfig`、JSON 等 |
| 同上（类型与默认值） | `config/types_gen.rs`、`config/defaults_gen.rs` | rsgen 生成（§7.2） |
| `pkg/config/config_util.go` | `config/config_util.rs` | |
| Go `reflect` 的用法（TOML 解码、JSON、`mergeConfigItems`） | `config/reflect.rs` | glue：`GoStruct` trait（字段描述 + 按下标取字段）和 `ValRef` / `ValMut` |
| BurntSushi/toml v1.5.0 | `gotoml/{lex,parse,decode,datetime,error}.rs` | go-dep，L1（`datetime` 为 L2） |
| `pkg/metrics/server.go` 的 `ExecuteErrorToLabel` | `metrics.rs` | 归属调整并入 |
| `pkg/util/context/warn.go` 的 `WarnAppender` | `contextutil.rs` | 归属调整并入（trait） |
| Go `time`、`strconv`、`encoding/json`、`path/filepath`、`encoding/base64` | `gostd/{time,strconv,json,path}.rs` | go-std |

### 7.2 生成的数据

rsgen（`govec/cmd/rsgen/config.go`）用反射遍历 `config.Config{}` 和 `tikvcfg.Config{}`：每个结构体生成 Rust 结构体（字段名为 Go 名的 snake_case，少数缩写按表修正，如 `SSLCA` → `ssl_ca`）、`FIELDS`（Go 名、`toml` / `json` tag、是否嵌入）和 `GoStruct` 实现；类型名：config 包保持原名（`nullableBool` → `NullableBool`），client-go 加前缀 `Tikv`，logutil 加 `Logutil`，pingcap/log 加 `Zaplog`。默认值是 `config.NewConfig()` 与 `tikvcfg.DefaultConfig()` 的运行时值，写成 Rust 字面量；`TempStoragePath` 调用 `temp_storage_dir_name()`（与 Go 一样启动时按 `$TMPDIR` 和 uid 计算）。govec `config` 的 `default_conf` 记录逐字段核对。

### 7.3 写法映射与已知差异

| Go | Rust | 说明 |
|---|---|---|
| `toml.DecodeFile(path, c)` 的反射合一 | `gotoml::decode` 遍历 `GoStruct` | L1；`typeFields`（嵌入提升、tag 优先、支配规则）照移植 |
| Go map 的随机迭代（`unifyStruct`、`ErrConfigInstanceSection.Error`、`Valid` 的存储列表、`NameMappings`） | 按键序 | 有两个以上解码错误的文件，Go 报其中随机一个，Rust 报键序第一个；解码出错后 Go 的部分配置取决于顺序（`InitializeConfig` 随即 panic，无影响）；同一节有两项要移到 `[instance]` 时 Go 的消息顺序随机。这些情况不进向量（生成器按结构判断：单叶子解码错误数 ≤ 1、无大小写折叠重复键、每节最多一项迁移、`Valid` 不在存储名上失败） |
| `NewConfig()` 浅拷贝 `defaultConf` | 值语义 | Go 中 `Load` 写入默认切片（容量足够时）或默认 `Labels` map 会改到 `defaultConf` 本身（别名）；Rust 没有这个别名。向量从去别名的副本开始 |
| `*ErrConfigValidationFailed`、`*ErrConfigInstanceSection`（调用方做类型断言） | `Load` 返回 `LoadError` 枚举；其余错误是 `Error`（M-ERR） | `ErrConfigInstanceSection` 在 Go 中持有全局切片的指针，`error()` 同样读取全局量 |
| `ConflictOptions`、`DeprecatedOptions`、`TikvConfigLock` | `parking_lot::Mutex` 静态量 | M-LOCK；与 Go 一样跨 `Load` 累积 |
| `globalConf atomic.Pointer[Config]` | `RwLock<Arc<Config>>`，首次访问时执行 `init` | `GetGlobalConfig` 返回 `Arc<Config>` |
| `AtomicBool`（嵌入 `atomic.Bool`）的 JSON | JSON bool | 嵌入类型提升的 `MarshalJSON` 优先于 `AtomicBool.MarshalText`（向量确认） |
| `CloneConf`（JSON 往返） | 克隆后应用往返的效果：`json:"-"` 字段清零、非法 `nullableBool` 变为未设置；`NaN`/`Inf` 时返回 `json: unsupported value` | 向量逐字段核对 |
| TOML 本地日期时间（无时区） | 按 UTC | Go 用进程所在时区；只影响写入 `nullableBool`/`AtomicBool` 时错误消息里的时间文本，向量只用带时区的时间 |
| `encoding/json` 解码错误的消息 | 不移植文本 | 只用于 `nullableBool.UnmarshalJSON` 的出错判断，向量只比较是否出错 |
| `strings.EqualFold`、`ToLower` | 单字符的 Rust 大小写映射（`İ` → `i` 特判） | 与 Go 的简单折叠在 BMP 常用字符上一致 |
| `TiKVClient.Valid` 的 `time.Duration(f * 1e9)` | `go_float_to_i64`：x86_64 上越界和 NaN 为 `MinInt64`，aarch64 上饱和 | 与两种目标上的 Go 一致（向量各自生成） |
| `InitializeConfig` 的 `os.Exit` / `terror.MustNil` | 返回 `InitializeExit::Exit(code)` / panic | L2；单测覆盖 |
| `TableLockDelayClean`（可替换的函数变量） | 普通函数 | 测试替换不移植 |

### 7.4 边界桩（spec §3 的类别）

| Rust | Go | 类别 | 何时替换 |
|---|---|---|---|
| `tiflashcompute_is_valid_auto_scaler_config` | `tiflashcompute.IsValidAutoScalerConfig`（DEF.MPP） | 块外纯函数的副本 | 不替换（rank 0 不能依赖 DEF.MPP） |
| `tikvutil_CommitterConcurrency` | `tikvutil.CommitterConcurrency` | 块外包级变量 | `pkg/util/tikvutil` 归属确定后（只有这一个变量，可直接并入本块） |
| `versioninfo_TiDBEdition` | `versioninfo.TiDBEdition` | 链接期变量 | 版本信息块 |
| `LogConfig`、`Log::to_log_config` | `logutil.LogConfig`（`zaplog.Config`） | 运行时结构的形状 | 日志块构建 logger 时使用 |
| `TracingConfiguration` | jaeger `config.Configuration` | 运行时结构的形状 | 不移植 tracing |
| `tikv_get_global_config`、`store_global_config` 中的 client-go 全局 | `tikvcfg.GetGlobalConfig` / `StoreGlobalConfig` | 第三方全局状态 | client-go 移植（L3 复用）时改为调用其接口 |
