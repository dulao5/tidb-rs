# tools/weekly: the weekly and nightly builds (#128)

`.github/workflows/weekly.yml` runs Go TiDB v8.5.8 and tidb-rs side by side on tiup playgrounds
and writes one page of report.

| mode | when | parts | report |
|---|---|---|---|
| weekly | Fridays 09:00 UTC (18:00 JST), or by hand | ab, sysbench, faults, locks, tpch | artifact, job summary, comment on #54, tag `weekly-YYYY-Www` |
| nightly | 18:00 UTC (03:00 JST), or by hand | ab, locks | artifact, job summary |

The parts run in parallel jobs, each against its own cluster (`run.sh <part> <out-dir>`):

| part | what | cluster |
|---|---|---|
| `ab` | `tidb-ab` on the sysbench statements and `tools/ab/workloads/*.sql`; `classify.py` explains every text statement on Go TiDB and counts the verdicts per operator | 1 TiKV |
| `sysbench` | `oltp_point_select` and `oltp_read_only`, 1 and 8 threads, Go TiDB then tidb-rs; B/A QPS | 1 TiKV |
| `faults` | `tools/ab/faults/run.sh` (needs passwordless sudo, which GitHub's hosted runners have) | 3 TiKV |
| `locks` | `tools/ab/locks/mixed_locks.py` with `lockgen` | 1 TiKV |
| `tpch` | TPC-H queries 1-22 (`tpch/queries.sql`, from go-tpc) on `tiup bench tpch prepare --sf 1` data: `tidb-ab` result comparison and per-query time on both sides (`tpch_time.py`, median of 3). SF 1 because `tiup bench` takes an integer scale factor; loading takes about 6 minutes | 1 TiKV |

`report.py` turns the parts' JSON into the report:

- the status of every part;
- agreement per operator (Go's plan), and which operators tidb-rs executed for the first time this week;
- sysbench B/A ratios;
- the fault scenarios;
- the lock cases.

It compares with the **baseline** of the same mode: the `<mode>-baseline` artifact, uploaded only by GREEN runs (or by a manual run with `accept_baseline`, never a RED one), kept 90 days. A regression therefore stays reported until it is fixed or accepted. Every run also keeps its own `<mode>-results`.

A wrong result in the raw data (a mismatch, a wrong fault-scenario read, a lock case that differs) makes the run RED whatever the part's exit status; `test_report.py` covers the verdicts and runs in CI.

**Verdict** (first line of the report, exit status of `report.py`):

- **RED:** a wrong result, a mismatch or a new inconsistency. The run fails and no tag is pushed.
- **INCOMPLETE:** a part did not run fully: skipped, failed to set up, missing, or with too few results (an empty `ab` workload, a sysbench workload × thread count without both sides, fewer than 18 lock cases). This is not a pass.
- **REGRESSED:** compared with the last run, an operator is no longer executable on tidb-rs, `unsupported` statements increased, or a sysbench B/A ratio dropped by more than 15%.
- **GREEN:** otherwise.

INCOMPLETE and REGRESSED show as a warning (a failed `continue-on-error` step). The weekly tag gets a `-incomplete` / `-regressed` suffix.

Locally:

```sh
cargo build --release -p poc-server -p tidb-ab
tools/weekly/run.sh ab /tmp/weekly/ab
python3 tools/weekly/report.py --in /tmp/weekly --parts ab --out /tmp/weekly/report.md
```

**TPC-H and ordering.** Q3, Q10 and Q18 end with `ORDER BY … LIMIT` on keys that can tie. Once tidb-rs runs them, ties may come back in a different order, or a different row may fall at the limit, and that would show as mismatches. When that happens, append the primary key to their `ORDER BY` in `tpch/queries.sql`, on both sides, so the order is total.
