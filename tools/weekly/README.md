# tools/weekly: the weekly and nightly builds (#128)

`.github/workflows/weekly.yml` runs Go TiDB v8.5.8 and tidb-rs side by side on tiup playgrounds
and writes one page of report.

| mode | when | parts | report |
|---|---|---|---|
| weekly | Fridays 09:00 UTC (18:00 JST), or by hand | ab, sysbench, faults, locks, tpch, integrationtest | artifact, job summary, comment on #54, tag `weekly-YYYY-Www` |
| nightly | 18:00 UTC (03:00 JST), or by hand | ab, locks | artifact, job summary |

The parts run in parallel jobs, each against its own cluster (`run.sh <part> <out-dir>`):

| part | what | cluster |
|---|---|---|
| `ab` | `tidb-ab` on the sysbench statements and `tools/ab/workloads/*.sql`; `classify.py` explains every text statement on Go TiDB and counts the verdicts per operator | 1 TiKV |
| `sysbench` | `oltp_point_select` and `oltp_read_only`, 1 and 8 threads, Go TiDB then tidb-rs; B/A QPS | 1 TiKV |
| `faults` | `tools/ab/faults/run.sh` (needs passwordless sudo, which GitHub's hosted runners have) | 3 TiKV |
| `locks` | `tools/ab/locks/mixed_locks.py` with `lockgen` | 1 TiKV |
| `tpch` | TPC-H queries 1-22 (`tpch/queries.sql`, from go-tpc) on `tiup bench tpch prepare --sf 1` data: `tidb-ab` result comparison and per-query time on both sides (`tpch_time.py`, median of 3). SF 1 because `tiup bench` takes an integer scale factor; loading takes about 6 minutes | 1 TiKV |
| `integrationtest` | TiDB v8.5.8's `tests/integrationtest` (shallow, sparse clone of pingcap/tidb) through `ab integrationtest` in hybrid mode: every statement on Go TiDB, the reads also on tidb-rs and compared; the playground gets `tools/ab/integrationtest.toml`, `TZ=Asia/Shanghai`. A subset of the tests, see below | 1 TiKV |

`report.py` turns the parts' JSON into the report:

- the status of every part;
- agreement per operator (Go's plan), and which operators tidb-rs executed for the first time this week;
- sysbench B/A ratios;
- the fault scenarios;
- the lock cases;
- the integration tests: reads sent to tidb-rs and their verdicts, agreement on what tidb-rs
  executed, the mismatches, the top `unsupported` reasons and tidb-rs errors, and the tests
  with the most mismatches / unsupported statements.

It compares with the **baseline** of the same mode: the `<mode>-baseline` artifact, uploaded only by GREEN runs (or by a manual run with `accept_baseline`, never a RED one), kept 90 days. A regression therefore stays reported until it is fixed or accepted. Every run also keeps its own `<mode>-results`.

A wrong result in the raw data (a mismatch, a wrong fault-scenario read, a lock case that differs) makes the run RED whatever the part's exit status; `test_report.py` covers the verdicts and runs in CI.

**Verdict** (first line of the report, exit status of `report.py`):

- **RED:** a wrong result, a mismatch (also of an integration test, unless accepted in `integrationtest-expected.json`) or a new inconsistency. The run fails and no tag is pushed.
- **INCOMPLETE:** a part did not run fully: skipped, failed to set up, missing, or with too few results (an empty `ab` workload, a sysbench workload × thread count without both sides, fewer than 18 lock cases). This is not a pass.
- **REGRESSED:** compared with the last run, an operator is no longer executable on tidb-rs, `unsupported` statements increased (for the integration tests: over the tests both runs ran), or a sysbench B/A ratio dropped by more than 15%.
- **GREEN:** otherwise.

INCOMPLETE and REGRESSED show as a warning (a failed `continue-on-error` step). The weekly tag gets a `-incomplete` / `-regressed` suffix.

Locally:

```sh
cargo build --release -p poc-server -p tidb-ab
tools/weekly/run.sh ab /tmp/weekly/ab
python3 tools/weekly/report.py --in /tmp/weekly --parts ab --out /tmp/weekly/report.md
```

**TPC-H and ordering.** Q3, Q10 and Q18 end with `ORDER BY … LIMIT` on keys that can tie. Once tidb-rs runs them, ties may come back in a different order, or a different row may fall at the limit, and that would show as mismatches. When that happens, append the primary key to their `ORDER BY` in `tpch/queries.sql`, on both sides, so the order is total.

**Integration tests.** Only autocommit reads run on tidb-rs; a statement tidb-rs answers with
1235 (`ER_NOT_SUPPORTED_YET`) is `unsupported`, counted and shown but not a failure, and
`b_error` (tidb-rs failed otherwise) is shown too. The verdicts come from
`ab-integrationtest/results.jsonl` (`tools/ab/README.md`, "Integration tests").

A `mismatch` makes the run RED unless it is listed in `integrationtest-expected.json`: accepted
mismatches keyed by `test:line` (the tests are pinned to v8.5.8, so lines are stable), each with
a `class` (`bug`: a tidb-rs bug, with the `issue` that tracks it; `nondeterministic`: an answer
whose row order the test does not fix) and a one-line `reason`. Listed mismatches are counted as
known; a listed statement that ran on tidb-rs and no longer mismatches is reported as "fixed,
remove from integrationtest-expected.json" (not a failure): delete the entry. Mismatches the
harness causes are fixed in `ab`, not listed: for example `ab` skips the reads of a connection
whose session diverged (tidb-rs rejected a result-affecting `SET` / `USE`; `tools/ab/README.md`).

`ddl/index_modify` turns `tidb_ddl_enable_fast_reorg` on, and Go TiDB then refuses `ADD INDEX`
(8256, "no enough space") when the disk under its `temp-dir` has less than 10% free; the reads
after such an `ALTER` would differ (listed entries would come out as "fixed"). So `run.sh` gives
Go TiDB a `temp-dir` on `/dev/shm` when there is one.

Which tests: every file except those with a statement that makes Go TiDB call the status port of
every TiDB server in the cluster (`ANALYZE`, `cluster_*` tables, `tidb_decode_sql_digests`,
`tidb_servers_info`): tidb-rs's status port does not answer that, so Go retries for about 40 s
per statement and the 67 such files alone would take about 3 hours (`ab integrationtest`'s
README section). That leaves 179 of the 246 files (about 31k statements, 10k reads sent to
tidb-rs); the whole part took about 12 minutes locally (11.5 of them in `ab`); `IT_BUDGET` (default 40
minutes) stops `ab` early and reports the part as incomplete with what it ran. `IT_TESTS`
selects other tests (names or `prefix/`), `IT_TIDB_DIR` uses an existing checkout instead of
cloning.
