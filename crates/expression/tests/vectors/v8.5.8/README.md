# Go-generated vectors (v8.5.8), domain "expr" (block D08.1 parts 1 and 2)

`<target>/expr.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/expr.jsonl.gz` (branch `govec/expr-D08.1-2`) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261112), 8,867 records: `col` 690, `colmisc` 300, `const` 2,500, `corr` 460, `evalbool` 300, `new` 60, `schema` 600, `tbl` 856; part 2: `sf` 2,000, `sfpair` 700, `vcol` 400, `sizes` 1 |
| `arm64/expr.jsonl.gz` sha256 | `4adfac04b9969bad51765278120f293819a4abd5d94c47857d1bd1fe080f0acd` (GOARCH=arm64, generated locally) |
| `amd64/expr.jsonl.gz` sha256 | `c83dcbecfc499271093d32431ace0a2d206727cbce6ebbfd94871993171517cf` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact) |

Column and constant values reuse the chunk domain's column specs and the datum domain's datum
specs. Three `const` records differ between the targets (M-F2I: a NaN float datum with a
hybrid `RetType` reaches `int64(NaN)`); the Rust conversion follows the target, as in the
datum domain. `tbl` records are the 856 TableInfo texts exported from a v8.5.8 cluster (model domain)
without virtual generated columns. A `{"panic": true}` output must panic in Rust too.

Pinned deviation (spec §4): a correlated column's value lives in the evaluation state, not in
the node, so `CorrelatedColumn.MemoryUsage` counts the zero Datum; the `corr` records carry
`dmem` (the value's `MemUsage`) and the replay compares `mem - zero + dmem` with Go's `mem`.

Part 2 records give a recipe (leaves and function names); Go builds it with
`NewFunctionBase`, so Go picks the real signatures. The output carries the built tree
(function name, Go signature type, result type, collation, the signature's own `vectorized()`
where observable), which the replay rebuilds with descriptor-only signatures
(`SigDrivers::None`), and the plan-side results (`String`, `HashCode`, `CanonicalHashCode`,
`ConstLevel`, `MemoryUsage` before `Vectorized()`, `ResolveIndices*`, `RemapColumn`,
`Decorrelate`, `Equal`, `ExpressionsSemanticEqual`, `GetUsedList`, `EqualByExprAndID`).
39 recipes Go cannot build (a function rejects the argument types) carry `{"build": err}` and
are skipped.

# Domain "funcs" (block D08.2 part 1)

`<target>/funcs.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/funcs.jsonl.gz` (branch `govec/funcs-D08.2`) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261113), 7,448 records: `derive` 4,000, `newbase` 1,300, `retft` 828, `deferred` 618, `verify` 400, `setflen` 300, `registry` 1, `baseerr` 1 |
| `arm64/funcs.jsonl.gz` sha256 | `9a52d47d3f7832b46c9c16bdce46ccb0aca3198feef719497746f50cf98b205d` (GOARCH=arm64, generated locally) |
| `amd64/funcs.jsonl.gz` sha256 | `c212a2059ab8c3bdd970f9a982e8ab282ded11995c850ae8995975b6d9c8927c` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact; same records as arm64) |

`derive` and `newbase` records carry the arguments Go built (columns, constants, small function
trees built with `NewFunctionBase`) as dumps in the output; the replay rebuilds them with
descriptor-only signatures, applies the record's coercibility/repertoire overrides, and compares
`deriveCollation` / `newBaseBuiltinFunc*` (result type, collation info, the collator's key of a
probe string). A `{"panic": true}` output (Go indexes `args` or `argTps` out of range) must
panic in Rust too.

# Domain "cast" (block D08.8 parts 1, 2a, 2b-1, 2b-2a, 2b-2b-i, 2b-2b-ii, 2b-3a and 2b-3c = D08.1 part 2b; block D08.4 batches 1–3; block D08.5 batches 1–5; block D08.2 constant propagation; block D08.12 batch 1)

`<target>/cast.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/cast.jsonl.gz` (branch `govec/conv-D08.5`, commit `89f1bd9`; amd64 follows from the tracker CI) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261114), 75,764 records: `cast` 3,860 (60 of them UNSIGNED UNION negative strings; 800 `conv` records — part 2a — with FLOAT, BIT(n), ENUM and SET columns, which Go's `Column.VecEvalInt/Real/String` convert), `wrap` 700; part 2b-1 (appended, the 4,560 records above unchanged): `str` 1,600 (CAST to STRING), `dbl` 120 (FLOAT → DOUBLE, bit for bit), `binlit` 500 (`HandleBinaryLiteral`, to_binary/from_binary), `collcast` 300 (`BuildCastCollationFunction`), `wrapstr` 300 (`WrapWithCastAsString`); part 2b-2a (appended, the 7,380 records above unchanged): `from` 2,400 (DATE/DATETIME/TIMESTAMP, TIME, JSON and VECTOR columns to INT/REAL/DECIMAL/STRING/YEAR), `from-fsp` 100 (TIME columns with an unspecified fsp); part 2b-2b-i (appended, the 9,880 records above unchanged): `to` 3,000 (every argument type to DATETIME/DATE/TIMESTAMP/TIME, VECTOR also to INT/REAL/DECIMAL), `to-const` 400 (constant arguments, folded), `to-fsp` 300 (an inner `CAST(json AS TIME(n))` whose values keep the JSON value's fsp), `wrap-time` 300 (`WrapWithCastAsTime/Duration`); part 2b-2b-ii (appended, the 13,880 records above unchanged): `to-json` 1,500 (every argument type to JSON, including BOOLEAN, UNSIGNED, YEAR, binary strings and JSON texts), `to-json-const` 200, `to-array` 600 (`CAST(json AS type ARRAY)`, a seventh `cast` argument `isArray`), `to-vec` 600 (CAST to VECTOR(n)), `wrap-json` 300 (`WrapWithCastAsJSON/VectorFloat32`), `json2tp` 300 (`ConvertJSON2Tp`); part 2b-3a (appended, the 17,380 records above unchanged): `withtps` 1,300 (`newBaseBuiltinFuncWithTp`/`WithFieldTypes`: the wrapped arguments and the `baseBuiltinFunc` fields), `newfunc` 700 (`NewFunction*` for cast/to_binary/from_binary/unknown names), `nullinfer` 300 (`typeInferForNull`); part 2b-3c (appended, the 19,680 records above unchanged): `evsuite` 1,500 (`NewEvaluatorSuite` + `Run`), `evfilter` 1,500 (`VectorizedFilter[ConsiderNull]`), `evbool` 700 (`VecEvalBool`), `evexpr` 700 (`EvalExpr`), `evexec` 300 (`VectorizedExecute`) over chunks of 1–3 columns with vectorization on/off, `Sel()` and NULLs; the argument of the "Truncated incorrect DOUBLE value" warning on the `VecEvalBool` paths is recorded as `'<pooled>'` (Go formats it from a pooled column's reused bytes, D08.1 spec §4 item 15); D08.4 batch 1 (appended, the 24,380 records above unchanged): `cmpbuild` 4,584 — `sys` 3,000, `same` 500 (two columns of one type), `sign` 400 (signed/unsigned BIGINT), `refine` 600 (the refining branches before Ceil/Floor), `json-parse` 84 (`CAST(s AS JSON) <op> s`) — building `name(lhs, rhs)` through `compareFunctionClass` (`NewFunctionBase` or `NewFunction`; the refined arguments and the signature), `GetAccurateCmpType`, the `GetCmpFunction` result per row, and `EvalExpr` with vectorization off/on; `newfunc` 900 with the comparison names (`cmp` 700, `tryfold` 200: constant folding, D08.1 T6c); D08.5 batch 1 (appended, the 29,864 records above unchanged): `arbuild` 5,100 (`sys` 2,400, `int` 900, `real` 400, `dec` 700, `ceil` 700: `+ - * / DIV %` and CEIL/CEILING/FLOOR built through `NewFunctionBase`/`NewFunction`/`NewFunctionTryFold`, then `EvalExpr` with vectorization off/on), `cmpbuild` `ceilcmp` 900, `newfunc` `arfold`/`artryfold` 600; D08.5 batch 2 (appended, the 36,464 records above unchanged): `arbuild` `logic` 2,000 (and/or/xor/not/unaryminus/istrue/istrue_with_null/isfalse), `logicfb` 600 (AND/OR/XOR with a second argument that fails or warns on some rows: the vector AND/OR fall back to the row version, the row XOR skips it after a NULL), `uminus` 500 (BIGINT boundaries, overflowing constants, DECIMAL/DOUBLE/time/strings); D08.2 constant propagation (appended, the 39,564 records above unchanged): `cprop` 3,200 — `sys` 1,500 (`PropagateConstant` over random CNF items: column = constant / column = column / comparisons / arithmetic / AND / OR / NOT / IS NULL / <=>), `year` 300 (YEAR columns against DOUBLE / DECIMAL / string constants, #99), `coll` 400 (string columns of several collations), `outer` 1,000 (`PropConstOverOuterJoin`, with outerCol = innerCol join conditions and null-sensitive joins); records where Go changed a node of the input conditions in place carry `"known_deviation": "in-place"`; the replay compares them and counts a difference as a deviation, at most 2 % of the cprop records (D08.2 spec §4 item 7, #104); `arbuild` `isnull` 300 (IS NULL over every argument type); `cprop` `ccast` 300 (appended, the 43,064 records above unchanged: `s1 = 'a' AND CAST(s1 AS CHAR) <op> s2` over string columns and CAST targets of several collations — ColumnSubstitute restores the CAST's coercibility); D08.5 batch 3 (appended, the 43,364 records above unchanged): `arbuild` `bitop` 1,500 (`& | ^ << >> ~`), `bitfb` 400 (a binary bit operator whose second argument fails on rows where the first is NULL); D08.5 batch 4 (appended, the 45,264 records above unchanged; `castCtx` index 6 is the seed of the ExprContext's MysqlRng, so `RAND()` never reads the clock): `mathbuild` 6,000 (`math` 3,000, `mathreal` 2,000, `mathround` 1,000: the math functions except CONV/CRC32, as `arbuild`), `mathclone` 800 (build, `Clone`, evaluate the original and the clone in turn: RAND shares its generator with the clone), `gomath` 6,000 (Go `math` functions on edge inputs, bit patterns); D08.12 batch 1 (appended, the 58,064 records above unchanged): `util` 3,000 — `x` 1,000 (the column/filter extractors, `FindUpperBound`, `ContainOuterNot`, `Contains`, side effects over random expression trees with correlated and virtual columns), `pnot` 900 (`PushDownNot`, `EliminateNoPrecisionLossCast`), `dnf` 700 (`ExtractFiltersFromDNFs`, `DeriveRelaxedFiltersFromDNF`, `FlattenDNF/CNFConditions`; extracted items compared sorted, Go iterates a map), `const` 300 (`DatumToConstant`, `Get*FromConstant`), `fmt` 100 (`GetFormatBytes/NanoTime`); records where Go changed a node or a column's field type in place carry `"known_deviation": "in-place"` and share the cprop 2 % cap (D08.12 spec §4); D08.4 batch 2 (appended, the 61,064 records above unchanged): `arbuild` `coal` 1,500 (COALESCE over 1–5 arguments of every comparison type, constants incl. NULL, CASTs), `coalfb` 600 (arguments that fail or warn on some rows at any position, behind NULL rows: the vector version falls back to the row version); D08.4 batch 3 (appended, the 63,164 records above unchanged): `arbuild` `gl` 2,000 (GREATEST/LEAST over 2–5 arguments of every type mix), `gltime` 1,200 (strings and temporals mixed: `CmpStringAsDate/Time`, DATE/DATETIME results), `glstr` 400 (`_ci` strings: collation ties, the row version keeps the earlier value, the vector version the later one), `glfb` 600 (arguments failing or warning behind NULL rows, INTERVAL included), `intv` 1,200 (INTERVAL: signed/unsigned, NULL / NOT NULL arguments, linear and binary search), `mathclone` `glclone` 400 (`builtinIntervalIntSig.Clone` drops `hasNullable`), `between` 600 (`ResolveType4Between`), `arbuild` `coalrand` 300 (`coalesce(col, rand(seed))`, #83) and `coalfsp` 2,000 (`CAST(COALESCE(d0, d1) AS CHAR)`); `arbuild` `gltnull` 400 (appended, the 71,864 records above unchanged: DATE/DATETIME columns with invalid dates and a later argument NULL on some rows, mostly strict — Go's vector version converts the NULL rows too); D08.5 batch 5 (appended, the 72,264 records above unchanged): `convbuild` 3,500 — `conv` 2,000 (CONV over random strings, binary literals and bases including negative and invalid ones), `crc32` 800, `convhybrid` 400 (CONV of a CAST of a BIT/ENUM/SET column: the hybrid branch), `convlimit` 300 (signed bases at the BIGINT limits) |
| `arm64/cast.jsonl.gz` sha256 | `3084cd0a1ba86945ef75ad3372ba1fe851d3bfd915339cd479b1279e4b469dd6` (GOARCH=arm64, generated locally) |
| `amd64/cast.jsonl.gz` sha256 | `594f8d23f4f822d15a03457aab6b13e2b459995dfa9968d69f289ba6d928320e` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact, run 43305552080; same records as arm64 except 557: 13 `arbuild` (5 hardware default NaN of an invalid DOUBLE operation, 8 `bitop` NaN DOUBLE cast to INT, M-F2I), 228 `mathbuild`, 10 `mathclone`, 306 `gomath` — Go's math package is target dependent: Exp / Log assembly, arm64 FMA fusing, the hardware default NaN of Sqrt / Pow) |

A `cast` record builds `CAST(arg AS tp)` with `BuildCastFunctionWithCheck` (the argument is a
column of the record's rows or a constant, which is folded) under random type flags and error
levels, and compares the built node (`sfDump`), the warnings of building, the `inUnion` metadata,
and the evaluation in both of Go's modes: row by row (value/error and warnings per row) and over
the chunk (the vector driver: values, or the first error, and warnings). Go's row and vector
versions differ in four places (spec D08.8 §3.2) and in `from_binary` (§3.4); each Rust driver
follows its own Go version. Part 2b-1 records may carry a sixth `cast` argument
(`isExplicitCharset`) and `castCtx` entries 3–4 (`max_allowed_packet`, SQL mode); part 2b-2b-i
records a fifth `castCtx` entry (the statement time, unix nanoseconds UTC, `WithCurrentTime`) and
an argument `["cast", tpIn, colArg]` (the argument is itself a CAST of the column).
