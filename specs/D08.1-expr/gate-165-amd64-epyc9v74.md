```
date: 2026-11-14T02:34:28Z
uname: Linux runnervm8axu0 7.0.0-1017-azure #17~24.04.1-Ubuntu SMP PREEMPT Thu Sep 24 18:46:57 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux
cpu: AMD EPYC 9V74 80-Core Processor, 4 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load1 1.90 (< 2, waited 60s); load average 1.90, 5.03, 3.49
round 2/5: load1 1.28 (< 2, waited 0s); load average 1.28, 2.37, 2.75
round 3/5: load1 1.08 (< 2, waited 0s); load average 1.08, 1.47, 2.19
round 4/5: load1 1.07 (< 2, waited 0s); load average 1.07, 1.27, 1.86
round 5/5: load1 1.32 (< 2, waited 0s); load average 1.32, 1.21, 1.62
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 1,658 ns | 0 | 739 ns | 0 | 2.24× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 1,327 ns | 0 | 592 ns | 0 | 2.24× | ≥ 1.0× | yes | pass |
| PlusInt-SU | 1,542 ns | 0 | 737 ns | 0 | 2.09× | ≥ 1.0× | yes | pass |
| PlusInt-US | 1,695 ns | 0 | 731 ns | 0 | 2.32× | ≥ 1.0× | yes | pass |
| LTInt | 2,396 ns | 0 | 556 ns | 0 | 4.31× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,387 ns | 0 | 343 ns | 0 | 4.05× | ≥ 1.0× | yes | pass |
| Concat | 61.43 µs | 1918 | 14.51 µs | 3 | 4.23× | ≥ 0.95× (allocates) | yes | pass |
| Like | 74.94 µs | 1303 | 42.27 µs | 1328 | 1.77× | ≥ 0.95× (allocates) | yes | pass |
| PlusInt-SS-port | 1,658 ns | 0 | 740 ns | 0 | 2.24× | ≥ 1.0× | yes | pass |
| PlusInt-UU-port | 1,327 ns | 0 | 598 ns | 0 | 2.22× | ≥ 1.0× | yes | pass |
| PlusInt-SU-port | 1,542 ns | 0 | 741 ns | 0 | 2.08× | ≥ 1.0× | yes | pass |
| PlusInt-US-port | 1,695 ns | 0 | 734 ns | 0 | 2.31× | ≥ 1.0× | yes | pass |
| LTInt-port | 2,396 ns | 0 | 556 ns | 0 | 4.31× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,387 ns | 0 | 344 ns | 0 | 4.03× | ≥ 1.0× | yes | pass |
| VecL1Distance-d3 | 20.10 µs | 0 | 12.14 µs | 0 | 1.66× | ≥ 1.0× | yes | pass |
| VecL2Distance-d3 | 20.62 µs | 0 | 11.84 µs | 0 | 1.74× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d3 | 22.02 µs | 0 | 15.06 µs | 0 | 1.46× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d3 | 20.34 µs | 0 | 11.37 µs | 0 | 1.79× | ≥ 1.0× | yes | pass |
| VecL1Distance-d768 | 4,614.25 µs | 0 | 639.80 µs | 0 | 7.21× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 884.63 µs | 0 | 636.36 µs | 0 | 1.39× | ≥ 1.0× | no | pass |
| VecCosineDistance-d768 | 894.56 µs | 0 | 659.87 µs | 0 | 1.36× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d768 | 878.33 µs | 0 | 629.79 µs | 0 | 1.39× | ≥ 1.0× | no | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 17.13 µs | 0 | 19.92 µs | 0 | 0.86× | ≥ 1.0× | no | below Go |
| PlusInt-UU | 16.87 µs | 0 | 19.91 µs | 0 | 0.85× | ≥ 1.0× | no | below Go |
| PlusInt-SU | 16.99 µs | 0 | 19.89 µs | 0 | 0.85× | ≥ 1.0× | no | below Go |
| PlusInt-US | 17.12 µs | 0 | 19.91 µs | 0 | 0.86× | ≥ 1.0× | no | below Go |
| LTInt | 17.78 µs | 0 | 18.50 µs | 0 | 0.96× | ≥ 1.0× | no | below Go |
| CastIntAsReal | 10.84 µs | 0 | 11.10 µs | 0 | 0.98× | ≥ 1.0× | no | below Go |
| Concat | 72.75 µs | 2441 | 86.94 µs | 3980 | 0.84× | ≥ 0.95× (allocates) | no | below Go |
| Like | 114.45 µs | 1953 | 87.72 µs | 5442 | 1.30× | ≥ 0.95× (allocates) | no | ≥ Go |
| PlusInt-SS-port | 17.13 µs | 0 | 16.91 µs | 0 | 1.01× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU-port | 16.87 µs | 0 | 16.94 µs | 0 | 1.00× | ≥ 1.0× | no | below Go |
| PlusInt-SU-port | 16.99 µs | 0 | 16.89 µs | 0 | 1.01× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 17.12 µs | 0 | 16.95 µs | 0 | 1.01× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 17.78 µs | 0 | 16.41 µs | 0 | 1.08× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 10.84 µs | 0 | 10.79 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 28.12 µs | 0 | 29.53 µs | 0 | 0.95× | ≥ 1.0× | no | below Go |
| VecL2Distance-d3 | 28.61 µs | 0 | 29.26 µs | 0 | 0.98× | ≥ 1.0× | no | below Go |
| VecCosineDistance-d3 | 30.92 µs | 0 | 34.04 µs | 0 | 0.91× | ≥ 1.0× | no | below Go |
| VecNegativeInnerProduct-d3 | 28.52 µs | 0 | 29.27 µs | 0 | 0.97× | ≥ 1.0× | no | below Go |
| VecL1Distance-d768 | 4,433.13 µs | 0 | 666.36 µs | 0 | 6.65× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d768 | 670.36 µs | 0 | 670.34 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d768 | 678.28 µs | 0 | 677.72 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d768 | 665.98 µs | 0 | 664.79 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 14.8 ns | pass |

**Gate passed.**
