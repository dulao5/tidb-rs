```
date: 2026-11-14T02:37:08Z
uname: Linux runnervm8axu0 7.0.0-1017-azure #17~24.04.1-Ubuntu SMP PREEMPT Thu Sep 24 18:46:57 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux
cpu: AMD EPYC 7763 64-Core Processor, 4 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load1 1.89 (< 2, waited 70s); load average 1.89, 5.60, 4.41
round 2/5: load1 1.45 (< 2, waited 0s); load average 1.45, 2.48, 3.34
round 3/5: load1 1.45 (< 2, waited 0s); load average 1.45, 1.68, 2.66
round 4/5: load1 1.75 (< 2, waited 0s); load average 1.75, 1.51, 2.20
round 5/5: load1 1.20 (< 2, waited 0s); load average 1.20, 1.24, 1.82
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 2,122 ns | 0 | 878 ns | 0 | 2.42× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 1,642 ns | 0 | 727 ns | 0 | 2.26× | ≥ 1.0× | yes | pass |
| PlusInt-SU | 1,972 ns | 0 | 897 ns | 0 | 2.20× | ≥ 1.0× | yes | pass |
| PlusInt-US | 1,808 ns | 0 | 884 ns | 0 | 2.05× | ≥ 1.0× | yes | pass |
| LTInt | 3,028 ns | 0 | 701 ns | 0 | 4.32× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,833 ns | 0 | 440 ns | 0 | 4.17× | ≥ 1.0× | yes | pass |
| Concat | 87.12 µs | 1881 | 18.23 µs | 3 | 4.78× | ≥ 0.95× (allocates) | yes | pass |
| Like | 103.53 µs | 1269 | 63.26 µs | 1328 | 1.64× | ≥ 0.95× (allocates) | yes | pass |
| PlusInt-SS-port | 2,122 ns | 0 | 876 ns | 0 | 2.42× | ≥ 1.0× | yes | pass |
| PlusInt-UU-port | 1,642 ns | 0 | 732 ns | 0 | 2.24× | ≥ 1.0× | yes | pass |
| PlusInt-SU-port | 1,972 ns | 0 | 876 ns | 0 | 2.25× | ≥ 1.0× | yes | pass |
| PlusInt-US-port | 1,808 ns | 0 | 897 ns | 0 | 2.02× | ≥ 1.0× | yes | pass |
| LTInt-port | 3,028 ns | 0 | 694 ns | 0 | 4.36× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,833 ns | 0 | 427 ns | 0 | 4.29× | ≥ 1.0× | yes | pass |
| VecL1Distance-d3 | 28.71 µs | 0 | 14.61 µs | 0 | 1.97× | ≥ 1.0× | yes | pass |
| VecL2Distance-d3 | 25.76 µs | 0 | 16.18 µs | 0 | 1.59× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d3 | 32.76 µs | 0 | 20.68 µs | 0 | 1.58× | ≥ 1.0× | yes | pass |
| VecNegativeInnerProduct-d3 | 24.92 µs | 0 | 14.31 µs | 0 | 1.74× | ≥ 1.0× | yes | pass |
| VecL1Distance-d768 | 5,611.19 µs | 0 | 744.82 µs | 0 | 7.53× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 1,021.11 µs | 0 | 742.19 µs | 0 | 1.38× | ≥ 1.0× | no | pass |
| VecCosineDistance-d768 | 1,035.89 µs | 0 | 762.88 µs | 0 | 1.36× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d768 | 1,015.78 µs | 0 | 738.76 µs | 0 | 1.37× | ≥ 1.0× | no | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 26.93 µs | 0 | 24.34 µs | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU | 22.95 µs | 0 | 24.35 µs | 0 | 0.94× | ≥ 1.0× | no | below Go |
| PlusInt-SU | 23.18 µs | 0 | 24.46 µs | 0 | 0.95× | ≥ 1.0× | no | below Go |
| PlusInt-US | 23.45 µs | 0 | 24.45 µs | 0 | 0.96× | ≥ 1.0× | no | below Go |
| LTInt | 27.72 µs | 0 | 24.53 µs | 0 | 1.13× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal | 17.44 µs | 0 | 15.93 µs | 0 | 1.09× | ≥ 1.0× | no | ≥ Go |
| Concat | 105.79 µs | 2407 | 120.38 µs | 3980 | 0.88× | ≥ 0.95× (allocates) | no | below Go |
| Like | 154.71 µs | 1902 | 134.85 µs | 5442 | 1.15× | ≥ 0.95× (allocates) | no | ≥ Go |
| PlusInt-SS-port | 26.93 µs | 0 | 20.72 µs | 0 | 1.30× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU-port | 22.95 µs | 0 | 20.74 µs | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 23.18 µs | 0 | 20.91 µs | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 23.45 µs | 0 | 20.81 µs | 0 | 1.13× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 27.72 µs | 0 | 21.64 µs | 0 | 1.28× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 17.44 µs | 0 | 15.83 µs | 0 | 1.10× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 42.97 µs | 0 | 36.46 µs | 0 | 1.18× | ≥ 1.0× | no | ≥ Go |
| VecL2Distance-d3 | 37.74 µs | 0 | 39.32 µs | 0 | 0.96× | ≥ 1.0× | no | below Go |
| VecCosineDistance-d3 | 43.11 µs | 0 | 46.20 µs | 0 | 0.93× | ≥ 1.0× | no | below Go |
| VecNegativeInnerProduct-d3 | 37.57 µs | 0 | 36.22 µs | 0 | 1.04× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d768 | 5,344.97 µs | 0 | 771.74 µs | 0 | 6.93× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d768 | 779.15 µs | 0 | 777.49 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d768 | 792.24 µs | 0 | 786.89 µs | 0 | 1.01× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d768 | 773.20 µs | 0 | 770.01 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 18.8 ns | pass |

**Gate passed.**
