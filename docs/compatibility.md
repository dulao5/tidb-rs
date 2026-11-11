# Differences from TiDB v8.5.8 visible to users

tidb-rs is a function-by-function port of Go TiDB **v8.5.8** (`8b857efa2`). By default it behaves exactly like v8.5.8, including its bugs. This page lists the places where a user can see different behavior. Internal deviations that cannot be observed through SQL or the wire protocol are recorded in each block's `specs/<block>/spec.md` §4, not here.

## Upstream fixes ported from TiDB master (go-fix)

Following the constitution (article 1, item 2), tidb-rs ports security and data-correctness fixes that are on TiDB master but were not backported to `release-8.5`. The govec vectors for these cases are generated from a patched v8.5.8 (see `govec/fixes/README.md` in tidb-rs-tracker).

| Upstream fix | v8.5.8 behavior | tidb-rs behavior | Compatibility impact |
|---|---|---|---|
| [pingcap/tidb#62892](https://github.com/pingcap/tidb/pull/62892) (labelled `compatibility-breaker` upstream; not in any 8.5 release) | `SET [SESSION] tidb_redact_log = …` succeeds, but actually switches redaction for **every session on the instance**, and is not persisted ([tidb-rs #62](https://github.com/dulao5/tidb-rs/issues/62)). | `tidb_redact_log` has **GLOBAL scope only**. `SET tidb_redact_log = ON` without a scope (= SESSION) and `SET SESSION …` fail with error 1229; use `SET GLOBAL tidb_redact_log = …`, which is cluster-wide and persisted. Same as TiDB master for user sessions. | **Breaking.** Scripts that set it without `GLOBAL` get an error. In a mixed Go/tidb-rs cluster both sides store the value in `mysql.global_variables` in the same format and read each other's `SET GLOBAL`. |
| [pingcap/tidb#65884](https://github.com/pingcap/tidb/pull/65884) | `SET tidb_opt_fix_control = '123:'` (colon with no value) fails with `ERROR 1105 runtime error: index out of range …` (an internal panic, recovered per statement); nothing is set. | The value after the colon is empty; no error. | None (an error becomes a success). |
| [pingcap/tidb#67149](https://github.com/pingcap/tidb/pull/67149) | Comparing or sorting a string containing U+2CEA1 under `utf8mb4_0900_ai_ci` fails with `ERROR 1105 runtime error: index out of range …` (an internal panic, recovered per statement). | Works. | None (an error becomes a success). |

## Deliberate deviations from v8.5.8

| Area | v8.5.8 behavior | tidb-rs behavior | Reason |
|---|---|---|---|
| `tidb-server --redact` (log de-redaction, `DeRedact`) | A line longer than 64 KiB stops Go's `bufio.Scanner`; that line **and every line after it** are silently dropped and the command exits successfully ([tidb-rs #90](https://github.com/dulao5/tidb-rs/issues/90)). Not fixed on master. | All lines are processed. | Copying Go would silently lose data in a recovery tool; slow logs with large `INSERT` statements commonly exceed 64 KiB per line. |
| Warnings of a non-numeric string filter (`WHERE str_col`, `WHERE CAST(.. AS CHAR)`) with vectorized evaluation | `Truncated incorrect DOUBLE value: '…'` is formatted when the warning is read, from the bytes of a pooled column that may have been reused by then, so `SHOW WARNINGS` can show other bytes instead of the value (Go `toBool` over `Column.GetString`, which aliases the column; observed across statements in govec, across sessions inferred from the process-wide pool, not verified on a cluster). | The warning shows the value. | Reproducing it would show unrelated (possibly other sessions') data in warnings; tidb-rs has no global column pool (mapping rule M-POOL). |

## Transitional gaps (until the session layer is ported)

tidb-rs still uses its prototype session layer until block D05.3. Until then:

- `SET GLOBAL` is not supported in the prototype session, so `tidb_redact_log` cannot be turned on in tidb-rs itself (`SET SESSION` is rejected by the fix above). Values set with `SET GLOBAL` on a Go TiDB in the same cluster are stored but not yet reloaded by the prototype.
