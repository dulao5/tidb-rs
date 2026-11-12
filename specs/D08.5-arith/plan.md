# D08.5 算术、数学与逻辑运算：plan

## 第 1 批（本 PR）

见 spec §1。govec `cast` 领域（tidb-rs-tracker `govec/domains/cast_d085.go`）：

| op / 来源 | 条数 | 内容 |
|---|--:|---|
| `arbuild` `sys` | 2,400 | 1–3 列（BIGINT 边界值、有无符号、DOUBLE 含 Inf/NaN、长短 DECIMAL、FLOAT/hybrid、YEAR、时间/JSON/VECTOR），参数为列、常量、CAST、嵌套算术或 CEIL/FLOOR；SQL mode 与 `div_precision_increment` 随机 |
| `arbuild` `int` | 900 | 两列 BIGINT 的有无符号组合；NULL 下藏着出错值；出错行不在第 0 行 |
| `arbuild` `real` | 400 | 两列 DOUBLE，含 Inf/NaN |
| `arbuild` `dec` | 700 | 两列 DECIMAL：溢出、除零、截断、DIV、MOD |
| `arbuild` `ceil` | 700 | CEIL/CEILING/FLOOR 的各种参数和返回类型 |
| `cmpbuild` `ceilcmp` | 900 | 走到 Ceil/Floor 的比较（D08.4 T6） |
| `newfunc` `arfold`/`artryfold` | 600 | 常量折叠；TryFold 的 warning 分支 |

## 第 2 批

见 spec §1b。govec `cast` 领域（tidb-rs-tracker `govec/domains/cast_d085b.go`，追加在第 1 批之后）：

| op / 来源 | 条数 | 内容 |
|---|--:|---|
| `arbuild` `logic` | 2,000 | 8 个名字（and、or、xor、not、unaryminus、istrue、istrue_with_null、isfalse），参数为列、常量、CAST、嵌套算术、比较和逻辑运算；0/1/NULL 的整数列 |
| `arbuild` `logicfb` | 600 | AND/OR/XOR 的另一个参数在部分行上出错或产生 warning（除零、BIGINT 溢出、截断的字符串）；出错行不在第 0 行；含嵌套 AND/OR |
| `arbuild` `uminus` | 500 | BIGINT 边界（有无符号列和常量：`-MinInt64`、无符号 2^63+1 溢出、无符号 2^63 为 MinInt64、溢出的常量使结果为 DECIMAL）、DECIMAL、DOUBLE、时间/duration（按 DECIMAL）、字符串 |

## 第 3 批

见 spec §1c。govec `cast` 领域（tidb-rs-tracker `govec/domains/cast_d085c.go`，追加在常量传播之后）：

| op / 来源 | 条数 | 内容 |
|---|--:|---|
| `arbuild` `bitop` | 1,500 | 6 个名字，参数为列（有无符号 BIGINT 边界值、批 1 的算术叶子：DOUBLE、DECIMAL、字符串、FLOAT、hybrid、时间等）、常量、CAST；移位数含 63、64、65、2^40、负数、MinInt64、MaxInt64 |
| `arbuild` `bitfb` | 400 | 二元位运算的第二个参数在部分行上出错或产生 warning，第一个参数在这些行上为 NULL |

## 后续批次（交接）

1. **常量传播**（第 2 批之后）（`constant_propagation.go`，D08.2 第 2b 部分），它的记录要包含 YEAR 列与非整数常量的比较（Lucky 评审 #98）。
2. ~~第 3 批：位运算~~（已完成，见上）。
3. **第 4 批：其余数学函数**（ABS、ROUND、TRUNCATE、LOG*、POW、SQRT、EXP、三角函数、RAND（带状态：Clone 对照 Go 并加"克隆后分别求值"的 govec 操作，D08.1 T6c (b)）、CONV、CRC32、SIGN、PI、DEGREES/RADIANS）。
