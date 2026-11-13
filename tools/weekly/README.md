# tools/weekly: the weekly and nightly builds (#128)

`.github/workflows/weekly.yml` runs Go TiDB v8.5.8 and tidb-rs side by side on tiup playgrounds
and writes one page of report.

| mode | when | parts | report |
|---|---|---|---|
| weekly | Fridays 09:00 UTC (18:00 JST), or by hand | ab, sysbench, faults, locks | artifact, job summary, comment on #54, tag `weekly-YYYY-Www` |
| nightly | 18:00 UTC (03:00 JST), or by hand | ab, locks | artifact, job summary |

The parts run in parallel jobs, each against its own cluster (`run.sh <part> <out-dir>`):

| part | what | cluster |
|---|---|---|
| `ab` | `tidb-ab` on the sysbench statements and `tools/ab/workloads/*.sql`; `classify.py` explains every text statement on Go TiDB and counts the verdicts per operator | 1 TiKV |
| `sysbench` | `oltp_point_select` and `oltp_read_only`, 1 and 8 threads, Go TiDB then tidb-rs; B/A QPS | 1 TiKV |
| `faults` | `tools/ab/faults/run.sh` (needs passwordless sudo, which GitHub's hosted runners have) | 3 TiKV |
| `locks` | `tools/ab/locks/mixed_locks.py` with `lockgen` | 1 TiKV |

`report.py` turns the parts' JSON into the report:

- the status of every part;
- agreement per operator (Go's plan), and which operators tidb-rs executed for the first time this week;
- sysbench B/A ratios;
- the fault scenarios;
- the lock cases.

It compares with the last successful run of the same mode, whose results are kept as an artifact for 90 days.

**Outcome:**

- **Red:** a wrong result, a mismatch, or a new inconsistency.
- **Warning:** a part is incomplete (skipped, failed to set up, or missing). This is not a pass.
- **Green:** otherwise.

Locally:

```sh
cargo build --release -p poc-server -p tidb-ab
tools/weekly/run.sh ab /tmp/weekly/ab
python3 tools/weekly/report.py --in /tmp/weekly --parts ab --out /tmp/weekly/report.md
```
