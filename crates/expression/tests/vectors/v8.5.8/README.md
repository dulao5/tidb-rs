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

# Domain "cast" (block D08.8 parts 1, 2a, 2b-1, 2b-2a, 2b-2b-i, 2b-2b-ii, 2b-3a and 2b-3c = D08.1 part 2b)

`<target>/cast.jsonl.gz` are copied unmodified from the tidb-rs-tracker repository:

| | |
|---|---|
| source | `tidb-rs-tracker` `vectors/v8.5.8/<target>/cast.jsonl.gz` (branch `govec/expr-D08.1-2b`, commit `b6ba795`) |
| generator | `govec` (Go TiDB v8.5.8 `8b857efa2`, go1.25.12, seed 20261114), 24,380 records: `cast` 3,860 (60 of them UNSIGNED UNION negative strings; 800 `conv` records — part 2a — with FLOAT, BIT(n), ENUM and SET columns, which Go's `Column.VecEvalInt/Real/String` convert), `wrap` 700; part 2b-1 (appended, the 4,560 records above unchanged): `str` 1,600 (CAST to STRING), `dbl` 120 (FLOAT → DOUBLE, bit for bit), `binlit` 500 (`HandleBinaryLiteral`, to_binary/from_binary), `collcast` 300 (`BuildCastCollationFunction`), `wrapstr` 300 (`WrapWithCastAsString`); part 2b-2a (appended, the 7,380 records above unchanged): `from` 2,400 (DATE/DATETIME/TIMESTAMP, TIME, JSON and VECTOR columns to INT/REAL/DECIMAL/STRING/YEAR), `from-fsp` 100 (TIME columns with an unspecified fsp); part 2b-2b-i (appended, the 9,880 records above unchanged): `to` 3,000 (every argument type to DATETIME/DATE/TIMESTAMP/TIME, VECTOR also to INT/REAL/DECIMAL), `to-const` 400 (constant arguments, folded), `to-fsp` 300 (an inner `CAST(json AS TIME(n))` whose values keep the JSON value's fsp), `wrap-time` 300 (`WrapWithCastAsTime/Duration`); part 2b-2b-ii (appended, the 13,880 records above unchanged): `to-json` 1,500 (every argument type to JSON, including BOOLEAN, UNSIGNED, YEAR, binary strings and JSON texts), `to-json-const` 200, `to-array` 600 (`CAST(json AS type ARRAY)`, a seventh `cast` argument `isArray`), `to-vec` 600 (CAST to VECTOR(n)), `wrap-json` 300 (`WrapWithCastAsJSON/VectorFloat32`), `json2tp` 300 (`ConvertJSON2Tp`); part 2b-3a (appended, the 17,380 records above unchanged): `withtps` 1,300 (`newBaseBuiltinFuncWithTp`/`WithFieldTypes`: the wrapped arguments and the `baseBuiltinFunc` fields), `newfunc` 700 (`NewFunction*` for cast/to_binary/from_binary/unknown names), `nullinfer` 300 (`typeInferForNull`); part 2b-3c (appended, the 19,680 records above unchanged): `evsuite` 1,500 (`NewEvaluatorSuite` + `Run`), `evfilter` 1,500 (`VectorizedFilter[ConsiderNull]`), `evbool` 700 (`VecEvalBool`), `evexpr` 700 (`EvalExpr`), `evexec` 300 (`VectorizedExecute`) over chunks of 1–3 columns with vectorization on/off, `Sel()` and NULLs; the argument of the "Truncated incorrect DOUBLE value" warning on the `VecEvalBool` paths is recorded as `'<pooled>'` (Go formats it from a pooled column's reused bytes, D08.1 spec §4 item 15) |
| `arm64/cast.jsonl.gz` sha256 | `3df711108edbb5e2a89f5902f5e5153075ec7a4b25d1fb5db6bc6722bf3dff5a` (GOARCH=arm64, generated locally) |
| `amd64/cast.jsonl.gz` sha256 | `c20c34325e2aafa0634a5c09a0ef1d363655a2fbe0f5e514f05b1b3ef7e91c97` (GOARCH=amd64, GOAMD64=v1; tracker CI artifact; same records as arm64) |

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
