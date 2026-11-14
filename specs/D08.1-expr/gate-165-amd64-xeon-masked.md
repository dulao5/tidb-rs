```
date: 2026-11-14T01:43:50Z
uname: Linux ip-10-0-132-231 7.0.0-1014-aws #14~24.04.1-Ubuntu SMP PREEMPT Fri Sep 18 21:24:59 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux
cpu: Intel(R) Xeon(R) 6975P-C, 4 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load1 1.84 (< 2, waited 60s); load average 1.84, 4.69, 3.13
round 2/5: load1 1.20 (< 2, waited 0s); load average 1.20, 2.13, 2.45
round 3/5: load1 1.32 (< 2, waited 0s); load average 1.32, 1.43, 2.00
round 4/5: load1 1.11 (< 2, waited 0s); load average 1.11, 1.29, 1.72
round 5/5: load1 1.00 (< 2, waited 0s); load average 1.00, 1.08, 1.47
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 1,364 ns | 0 | 726 ns | 0 | 1.88× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 957 ns | 0 | 854 ns | 0 | 1.12× | ≥ 1.0× | no | pass |
| PlusInt-SU | 1,030 ns | 0 | 1,055 ns | 0 | 0.98× | ≥ 1.0× | no | **FAIL** |
| PlusInt-US | 1,032 ns | 0 | 1,060 ns | 0 | 0.97× | ≥ 1.0× | no | **FAIL** |
| LTInt | 2,141 ns | 0 | 529 ns | 0 | 4.05× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,611 ns | 0 | 335 ns | 0 | 4.81× | ≥ 1.0× | yes | pass |
| Concat | 61.15 µs | 1821 | 11.95 µs | 3 | 5.12× | ≥ 0.95× (allocates) | yes | pass |
| Like | 84.24 µs | 1255 | 51.02 µs | 1328 | 1.65× | ≥ 0.95× (allocates) | yes | pass |
| PlusInt-SS-port | 1,364 ns | 0 | 727 ns | 0 | 1.88× | ≥ 1.0× | yes | pass |
| PlusInt-UU-port | 957 ns | 0 | 859 ns | 0 | 1.11× | ≥ 1.0× | no | pass |
| PlusInt-SU-port | 1,030 ns | 0 | 1,061 ns | 0 | 0.97× | ≥ 1.0× | no | **FAIL** |
| PlusInt-US-port | 1,032 ns | 0 | 1,062 ns | 0 | 0.97× | ≥ 1.0× | no | **FAIL** |
| LTInt-port | 2,141 ns | 0 | 519 ns | 0 | 4.13× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,611 ns | 0 | 335 ns | 0 | 4.81× | ≥ 1.0× | yes | pass |
| VecL1Distance-d3 | 26.45 µs | 0 | 10.30 µs | 0 | 2.57× | ≥ 1.0× | yes | pass |
| VecL2Distance-d3 | 16.06 µs | 0 | 10.54 µs | 0 | 1.52× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d3 | 17.00 µs | 0 | 11.49 µs | 0 | 1.48× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d3 | 15.84 µs | 0 | 10.45 µs | 0 | 1.52× | ≥ 1.0× | yes | pass |
| VecL1Distance-d768 | 4,623.16 µs | 0 | 423.79 µs | 0 | 10.91× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 1,173.78 µs | 0 | 409.86 µs | 0 | 2.86× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d768 | 1,321.98 µs | 0 | 483.02 µs | 0 | 2.74× | ≥ 1.0× | yes | pass |
| VecNegativeInnerProduct-d768 | 1,178.96 µs | 0 | 401.88 µs | 0 | 2.93× | ≥ 1.0× | yes | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 21.10 µs | 0 | 16.20 µs | 0 | 1.30× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU | 16.15 µs | 0 | 16.13 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU | 16.68 µs | 0 | 16.29 µs | 0 | 1.02× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US | 16.75 µs | 0 | 16.02 µs | 0 | 1.05× | ≥ 1.0× | no | ≥ Go |
| LTInt | 18.99 µs | 0 | 18.70 µs | 0 | 1.02× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal | 11.99 µs | 0 | 11.43 µs | 0 | 1.05× | ≥ 1.0× | no | ≥ Go |
| Concat | 75.00 µs | 2325 | 83.82 µs | 3980 | 0.89× | ≥ 0.95× (allocates) | no | below Go |
| Like | 117.17 µs | 1881 | 102.03 µs | 5442 | 1.15× | ≥ 0.95× (allocates) | no | ≥ Go |
| PlusInt-SS-port | 21.10 µs | 0 | 14.37 µs | 0 | 1.47× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU-port | 16.15 µs | 0 | 14.36 µs | 0 | 1.13× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 16.68 µs | 0 | 14.61 µs | 0 | 1.14× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 16.75 µs | 0 | 14.46 µs | 0 | 1.16× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 18.99 µs | 0 | 16.30 µs | 0 | 1.17× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 11.99 µs | 0 | 11.89 µs | 0 | 1.01× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 33.62 µs | 0 | 21.79 µs | 0 | 1.54× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d3 | 23.89 µs | 0 | 22.35 µs | 0 | 1.07× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d3 | 24.87 µs | 0 | 23.57 µs | 0 | 1.06× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d3 | 23.84 µs | 0 | 22.61 µs | 0 | 1.05× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d768 | 3,870.94 µs | 0 | 434.99 µs | 0 | 8.90× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d768 | 436.53 µs | 0 | 427.53 µs | 0 | 1.02× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d768 | 554.91 µs | 0 | 494.95 µs | 0 | 1.12× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d768 | 431.30 µs | 0 | 418.69 µs | 0 | 1.03× | ≥ 1.0× | no | ≥ Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 15.5 ns | pass |

**Gate failed:** PlusInt-SU-Vec: 0.98× < 1.00×; PlusInt-US-Vec: 0.97× < 1.00×; PlusInt-SU-port-Vec: 0.97× < 1.00×; PlusInt-US-port-Vec: 0.97× < 1.00×
