```
date: 2026-11-14T00:44:59Z
uname: Linux runnervm8axu0 7.0.0-1017-azure #17~24.04.1-Ubuntu SMP PREEMPT Thu Sep 24 18:46:57 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux
cpu: AMD EPYC 7763 64-Core Processor, 4 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load1 1.95 (< 2, waited 70s); load average 1.95, 4.66, 2.67
round 2/5: load1 1.08 (< 2, waited 0s); load average 1.08, 2.10, 2.14
round 3/5: load1 1.25 (< 2, waited 0s); load average 1.25, 1.46, 1.81
round 4/5: load1 1.18 (< 2, waited 0s); load average 1.18, 1.26, 1.60
round 5/5: load1 1.62 (< 2, waited 0s); load average 1.62, 1.32, 1.48
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 2,114 ns | 0 | 1,207 ns | 0 | 1.75× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 1,527 ns | 0 | 1,344 ns | 0 | 1.14× | ≥ 1.0× | no | pass |
| PlusInt-SU | 1,973 ns | 0 | 1,667 ns | 0 | 1.18× | ≥ 1.0× | no | pass |
| PlusInt-US | 1,733 ns | 0 | 1,665 ns | 0 | 1.04× | ≥ 1.0× | no | pass |
| LTInt | 3,027 ns | 0 | 752 ns | 0 | 4.02× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,846 ns | 0 | 527 ns | 0 | 3.50× | ≥ 1.0× | yes | pass |
| Concat | 88.25 µs | 1876 | 18.48 µs | 3 | 4.77× | ≥ 0.95× (allocates) | yes | pass |
| Like | 103.99 µs | 1309 | 62.40 µs | 1328 | 1.67× | ≥ 0.95× (allocates) | yes | pass |
| PlusInt-SS-port | 2,114 ns | 0 | 1,183 ns | 0 | 1.79× | ≥ 1.0× | yes | pass |
| PlusInt-UU-port | 1,527 ns | 0 | 1,431 ns | 0 | 1.07× | ≥ 1.0× | no | pass |
| PlusInt-SU-port | 1,973 ns | 0 | 2,051 ns | 0 | 0.96× | ≥ 1.0× | no | **FAIL** |
| PlusInt-US-port | 1,733 ns | 0 | 2,060 ns | 0 | 0.84× | ≥ 1.0× | no | **FAIL** |
| LTInt-port | 3,027 ns | 0 | 747 ns | 0 | 4.05× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,846 ns | 0 | 529 ns | 0 | 3.49× | ≥ 1.0× | yes | pass |
| VecL1Distance-d3 | 27.16 µs | 0 | 14.92 µs | 0 | 1.82× | ≥ 1.0× | yes | pass |
| VecL2Distance-d3 | 25.74 µs | 0 | 16.21 µs | 0 | 1.59× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d3 | 32.77 µs | 0 | 20.34 µs | 0 | 1.61× | ≥ 1.0× | yes | pass |
| VecNegativeInnerProduct-d3 | 24.95 µs | 0 | 14.58 µs | 0 | 1.71× | ≥ 1.0× | yes | pass |
| VecL1Distance-d768 | 5,597.82 µs | 0 | 745.20 µs | 0 | 7.51× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 1,045.18 µs | 0 | 741.34 µs | 0 | 1.41× | ≥ 1.0× | no | pass |
| VecCosineDistance-d768 | 1,056.88 µs | 0 | 761.34 µs | 0 | 1.39× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d768 | 1,040.89 µs | 0 | 738.06 µs | 0 | 1.41× | ≥ 1.0× | no | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 26.80 µs | 0 | 24.29 µs | 0 | 1.10× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU | 22.86 µs | 0 | 24.28 µs | 0 | 0.94× | ≥ 1.0× | no | below Go |
| PlusInt-SU | 23.36 µs | 0 | 24.23 µs | 0 | 0.96× | ≥ 1.0× | no | below Go |
| PlusInt-US | 23.41 µs | 0 | 24.34 µs | 0 | 0.96× | ≥ 1.0× | no | below Go |
| LTInt | 27.76 µs | 0 | 24.78 µs | 0 | 1.12× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal | 17.38 µs | 0 | 15.93 µs | 0 | 1.09× | ≥ 1.0× | no | ≥ Go |
| Concat | 107.15 µs | 2383 | 124.56 µs | 3980 | 0.86× | ≥ 0.95× (allocates) | no | below Go |
| Like | 155.55 µs | 1962 | 130.27 µs | 5442 | 1.19× | ≥ 0.95× (allocates) | no | ≥ Go |
| PlusInt-SS-port | 26.80 µs | 0 | 20.89 µs | 0 | 1.28× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU-port | 22.86 µs | 0 | 20.88 µs | 0 | 1.09× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 23.36 µs | 0 | 21.17 µs | 0 | 1.10× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 23.41 µs | 0 | 21.18 µs | 0 | 1.10× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 27.76 µs | 0 | 21.74 µs | 0 | 1.28× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 17.38 µs | 0 | 15.53 µs | 0 | 1.12× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 46.29 µs | 0 | 38.22 µs | 0 | 1.21× | ≥ 1.0× | no | ≥ Go |
| VecL2Distance-d3 | 37.72 µs | 0 | 39.67 µs | 0 | 0.95× | ≥ 1.0× | no | below Go |
| VecCosineDistance-d3 | 43.18 µs | 0 | 45.47 µs | 0 | 0.95× | ≥ 1.0× | no | below Go |
| VecNegativeInnerProduct-d3 | 37.50 µs | 0 | 35.86 µs | 0 | 1.05× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d768 | 5,318.53 µs | 0 | 773.19 µs | 0 | 6.88× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d768 | 777.54 µs | 0 | 776.16 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d768 | 789.75 µs | 0 | 785.52 µs | 0 | 1.01× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d768 | 772.84 µs | 0 | 770.44 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 18.3 ns | pass |

**Gate failed:** PlusInt-SU-port-Vec: 0.96× < 1.00×; PlusInt-US-port-Vec: 0.84× < 1.00×
