# govec vectors of tidb-memory (Go TiDB v8.5.8)

Copies of the tracker's `vectors/v8.5.8/<target>/<domain>.jsonl.gz`
([tidb-rs-tracker](https://github.com/dulao5/tidb-rs-tracker), `govec/`).

| file | domain | block | replayed by |
|---|---|---|---|
| `<target>/memory.jsonl.gz` | memory | D15.1 | `tests/govec_memory.rs` |
| `<target>/cgroup.jsonl.gz` | cgroup | D15.3 | `tests/govec_cgroup.rs` |

`cgroup` (seed 1503, 7,040 records): arm64 was generated locally, amd64 is the tracker CI
artifact; the two targets hold identical records (no floating point). sha256:

- `amd64/cgroup.jsonl.gz` 8d8c0c2ad132f32e80cc00b79651225e5340e6ba49d1e9a84ac60e76c278fc3f
- `arm64/cgroup.jsonl.gz` e8106826493b983afcb388b2350130448ae5bb56199fe1f696f1aa2e6c3e677b

Extra seeds (tracker CI artifact `govec-extra-seeds-*`, never committed):
`GOVEC_CGROUP_FILES=a.jsonl.gz,b.jsonl.gz cargo test -p tidb-memory --test govec_cgroup`.
