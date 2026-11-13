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
| Warnings of a non-numeric string filter (`WHERE str_col`, `WHERE CAST(.. AS CHAR)`) with vectorized evaluation | `Truncated incorrect DOUBLE value: '…'` is formatted when the warning is read, from the bytes of a pooled column that may have been reused by then, so `SHOW WARNINGS` can show other bytes instead of the value (Go `toBool` over `Column.GetString`, which aliases the column; confirmed on v8.5.8 within one statement — 976 of 2,000 warnings showed another row's value — also through `c + 0` and a vectorized `CAST(c AS DOUBLE)`, and not with vectorization off; across sessions not reproduced; [tidb-rs #97](https://github.com/dulao5/tidb-rs/issues/97)). | The warning shows the value. | Reproducing it would show unrelated (possibly other sessions') data in warnings; tidb-rs has no global column pool (mapping rule M-POOL). |
| Constant propagation over YEAR columns compared with a fractional constant (`WHERE y1 = y2 AND y2 < 1.0`) | Propagation rewrites the shared constant in place (`AdjustYear` in `refineArgs`), so the original condition changes too: `y2 < 1.0` alone returns `0000`, but with `y1 = y2` it also returns `1990` and `2000` (the plan shows `lt(y2, 2001)`); `y1 = y2 AND y2 >= 0.5` loses `(1990,1990)` and `(2000,2000)` ([tidb-rs #104](https://github.com/dulao5/tidb-rs/issues/104)). | Propagation works on copies; adding `y1 = y2` does not change the rows `y2 < 1.0` returns. The `>= 0.5` case matches v8.5.8 (the `Ceil` rewrite, #99, is ported as is). | Copying Go's in-place mutation would reproduce an upstream bug that returns wrong rows; tidb-rs expressions are immutable shared values (D08.2 spec §4 item 7). |

## Observability (decided, not yet implemented)

| Area | v8.5.8 behavior | tidb-rs behavior | Reason |
|---|---|---|---|
| `/debug/pprof/goroutine`, `/debug/pprof/mutex` | Go runtime profiles in pprof format | **404 / not supported** ([#115](https://github.com/dulao5/tidb-rs/issues/115)) | tidb-rs has no goroutines or Go mutex profiler; no Go-format equivalent. CPU and heap profiles keep Go's paths. |
| Top SQL / TiDB-side statement CPU (`Tidb_cpu_time`, `AVG_TIDB_CPU_TIME`, PROCESSLIST TiDB CPU) | Sampling-based attribution, present when `tidb_enable_top_sql = ON` | Window-apportioned CPU accounting plus a dynamic **per-execution sampling rate** sysvar (default 100%); effective rate = `tidb_enable_top_sql` ? rate : 0, so turning Top SQL off from TiDB Dashboard always works. TiDB CPU fields appear only on sampled executions. (`Tikv_cpu_time` is unaffected: TiKV process time, always present as in Go.) ([#116](https://github.com/dulao5/tidb-rs/issues/116)) | Owner decision: lower and tunable overhead; numbers more accurate than Go's, not identical. |

## TiKV client gaps (client-rust fork, ADR 0008)

Gaps of the TiKV client against client-go, tracked in [`specs/D07.1-store/client-go-parity.md`](../specs/D07.1-store/client-go-parity.md); listed here only when a user can see them.

| Area | v8.5.8 behavior | tidb-rs behavior | Plan |
|---|---|---|---|
| `tidb_replica_read` (`follower`, `leader-and-follower`, `closest-replicas`, …) | Reads go to followers / the closest replica as set. | **Accepted but ignored**: every read goes to the leader, silently. Results are the same; the read load and cross-AZ traffic are not. | Fix item in the parity table (§3, replica read). Until then, `SET` of a non-`leader` value should return a warning (also a fix item). |
| `tikv_client_read_timeout` | The first attempt of a read gets this short timeout; if the leader does not answer in time, the read moves to a follower (replica read, still linearizable): "if the leader is slow, read from a follower". | **Not wired yet.** Once wired, without replica read it **only shortens the first attempt**: the retry goes back to the same leader with the default 30 s / 60 s timeout (client-rust#14, #15). | Parity table §3 (the row after replica read): done together with replica read. |

## Transitional gaps (until the session layer is ported)

tidb-rs still uses its prototype session layer until block D05.3. Until then:

- `SET GLOBAL` is not supported in the prototype session, so `tidb_redact_log` cannot be turned on in tidb-rs itself (`SET SESSION` is rejected by the fix above). Values set with `SET GLOBAL` on a Go TiDB in the same cluster are stored but not yet reloaded by the prototype.
