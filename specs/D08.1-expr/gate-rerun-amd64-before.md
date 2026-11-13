```
date: 2026-11-13T19:33:38Z
uname: Linux runnervm8axu0 7.0.0-1017-azure #17~24.04.1-Ubuntu SMP PREEMPT Thu Sep 24 18:46:57 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux
cpu: AMD EPYC 7763 64-Core Processor, 4 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load 5.17, 6.36, 4.31
round 2/5: load 1.50, 2.72, 3.28
round 3/5: load 1.05, 1.54, 2.52
round 4/5: load 1.66, 1.59, 2.19
round 5/5: load 1.48, 1.40, 1.87
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 2,107 ns | 0 | 1,172 ns | 0 | 1.80× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 1,551 ns | 0 | 1,316 ns | 0 | 1.18× | ≥ 1.0× | no | pass |
| PlusInt-SU | 2,007 ns | 0 | 1,676 ns | 0 | 1.20× | ≥ 1.0× | no | pass |
| PlusInt-US | 1,866 ns | 0 | 1,683 ns | 0 | 1.11× | ≥ 1.0× | no | pass |
| LTInt | 3,020 ns | 0 | 755 ns | 0 | 4.00× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,842 ns | 0 | 516 ns | 0 | 3.57× | ≥ 1.0× | yes | pass |
| Concat | 88.44 µs | 1844 | 18.40 µs | 3 | 4.81× | ≥ 0.95× (allocates) | yes | pass |
| Like | 100.98 µs | 1303 | 62.23 µs | 1328 | 1.62× | ≥ 0.95× (allocates) | yes | pass |
| PlusInt-SS-port | 2,107 ns | 0 | 1,737 ns | 0 | 1.21× | ≥ 1.0× | no | pass |
| PlusInt-UU-port | 1,551 ns | 0 | 1,387 ns | 0 | 1.12× | ≥ 1.0× | no | pass |
| PlusInt-SU-port | 2,007 ns | 0 | 1,527 ns | 0 | 1.31× | ≥ 1.0× | no | pass |
| PlusInt-US-port | 1,866 ns | 0 | 1,521 ns | 0 | 1.23× | ≥ 1.0× | no | pass |
| LTInt-port | 3,020 ns | 0 | 749 ns | 0 | 4.03× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,842 ns | 0 | 1,566 ns | 0 | 1.18× | ≥ 1.0× | no | pass |
| VecL1Distance-d3 | 28.20 µs | 0 | 54.16 µs | 0 | 0.52× | ≥ 1.0× | no | **FAIL** |
| VecL2Distance-d3 | 25.77 µs | 0 | 61.24 µs | 0 | 0.42× | ≥ 1.0× | no | **FAIL** |
| VecCosineDistance-d3 | 32.77 µs | 0 | 67.44 µs | 0 | 0.49× | ≥ 1.0× | no | **FAIL** |
| VecNegativeInnerProduct-d3 | 24.96 µs | 0 | 53.89 µs | 0 | 0.46× | ≥ 1.0× | no | **FAIL** |
| VecL1Distance-d768 | 5,608.72 µs | 0 | 792.37 µs | 0 | 7.08× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 1,021.95 µs | 0 | 797.88 µs | 0 | 1.28× | ≥ 1.0× | no | pass |
| VecCosineDistance-d768 | 1,034.96 µs | 0 | 805.33 µs | 0 | 1.29× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d768 | 1,019.82 µs | 0 | 790.91 µs | 0 | 1.29× | ≥ 1.0× | no | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 26.99 µs | 0 | 24.39 µs | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU | 23.04 µs | 0 | 24.34 µs | 0 | 0.95× | ≥ 1.0× | no | below Go |
| PlusInt-SU | 23.36 µs | 0 | 24.66 µs | 0 | 0.95× | ≥ 1.0× | no | below Go |
| PlusInt-US | 23.60 µs | 0 | 24.36 µs | 0 | 0.97× | ≥ 1.0× | no | below Go |
| LTInt | 28.62 µs | 0 | 24.74 µs | 0 | 1.16× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal | 17.68 µs | 0 | 15.89 µs | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| Concat | 107.81 µs | 2357 | 122.08 µs | 3980 | 0.88× | ≥ 0.95× (allocates) | no | below Go |
| Like | 153.40 µs | 1953 | 129.00 µs | 5442 | 1.19× | ≥ 0.95× (allocates) | no | ≥ Go |
| PlusInt-SS-port | 26.99 µs | 0 | 22.56 µs | 0 | 1.20× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU-port | 23.04 µs | 0 | 20.61 µs | 0 | 1.12× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 23.36 µs | 0 | 21.23 µs | 0 | 1.10× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 23.60 µs | 0 | 21.22 µs | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 28.62 µs | 0 | 22.16 µs | 0 | 1.29× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 17.68 µs | 0 | 14.28 µs | 0 | 1.24× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 45.88 µs | 0 | 85.03 µs | 2048 | 0.54× | ≥ 0.95× (allocates) | no | below Go |
| VecL2Distance-d3 | 37.75 µs | 0 | 90.98 µs | 2048 | 0.41× | ≥ 0.95× (allocates) | no | below Go |
| VecCosineDistance-d3 | 43.08 µs | 0 | 96.89 µs | 2048 | 0.44× | ≥ 0.95× (allocates) | no | below Go |
| VecNegativeInnerProduct-d3 | 37.60 µs | 0 | 84.19 µs | 2048 | 0.45× | ≥ 0.95× (allocates) | no | below Go |
| VecL1Distance-d768 | 5,354.44 µs | 0 | 998.69 µs | 2048 | 5.36× | ≥ 0.95× (allocates) | yes | ≥ Go |
| VecL2Distance-d768 | 778.52 µs | 0 | 1,008.30 µs | 2048 | 0.77× | ≥ 0.95× (allocates) | no | below Go |
| VecCosineDistance-d768 | 789.89 µs | 0 | 1,017.50 µs | 2048 | 0.78× | ≥ 0.95× (allocates) | no | below Go |
| VecNegativeInnerProduct-d768 | 773.94 µs | 0 | 999.55 µs | 2048 | 0.77× | ≥ 0.95× (allocates) | no | below Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 18.5 ns | pass |

**Gate failed:** VecL1Distance-d3-Vec: 0.52× < 1.00×; VecL2Distance-d3-Vec: 0.42× < 1.00×; VecCosineDistance-d3-Vec: 0.49× < 1.00×; VecNegativeInnerProduct-d3-Vec: 0.46× < 1.00×
