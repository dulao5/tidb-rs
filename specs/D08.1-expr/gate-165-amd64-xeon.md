```
date: 2026-11-14T02:33:34Z
uname: Linux ip-10-0-143-76 7.0.0-1014-aws #14~24.04.1-Ubuntu SMP PREEMPT Fri Sep 18 21:24:59 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux
cpu: Intel(R) Xeon(R) 6975P-C, 4 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load1 1.92 (< 2, waited 60s); load average 1.92, 4.85, 3.22
round 2/5: load1 1.20 (< 2, waited 0s); load average 1.20, 2.17, 2.50
round 3/5: load1 1.09 (< 2, waited 0s); load average 1.09, 1.50, 2.07
round 4/5: load1 1.07 (< 2, waited 0s); load average 1.07, 1.21, 1.74
round 5/5: load1 1.15 (< 2, waited 0s); load average 1.15, 1.18, 1.53
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 1,374 ns | 0 | 527 ns | 0 | 2.61× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 953 ns | 0 | 615 ns | 0 | 1.55× | ≥ 1.0× | yes | pass |
| PlusInt-SU | 1,027 ns | 0 | 744 ns | 0 | 1.38× | ≥ 1.0× | no | pass |
| PlusInt-US | 1,028 ns | 0 | 741 ns | 0 | 1.39× | ≥ 1.0× | no | pass |
| LTInt | 2,144 ns | 0 | 564 ns | 0 | 3.80× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,610 ns | 0 | 258 ns | 0 | 6.24× | ≥ 1.0× | yes | pass |
| Concat | 63.39 µs | 1907 | 11.33 µs | 3 | 5.59× | ≥ 0.95× (allocates) | yes | pass |
| Like | 86.59 µs | 1317 | 49.24 µs | 1328 | 1.76× | ≥ 0.95× (allocates) | yes | pass |
| PlusInt-SS-port | 1,374 ns | 0 | 527 ns | 0 | 2.61× | ≥ 1.0× | yes | pass |
| PlusInt-UU-port | 953 ns | 0 | 616 ns | 0 | 1.55× | ≥ 1.0× | yes | pass |
| PlusInt-SU-port | 1,027 ns | 0 | 749 ns | 0 | 1.37× | ≥ 1.0× | no | pass |
| PlusInt-US-port | 1,028 ns | 0 | 744 ns | 0 | 1.38× | ≥ 1.0× | no | pass |
| LTInt-port | 2,144 ns | 0 | 567 ns | 0 | 3.78× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,610 ns | 0 | 259 ns | 0 | 6.21× | ≥ 1.0× | yes | pass |
| VecL1Distance-d3 | 26.39 µs | 0 | 10.34 µs | 0 | 2.55× | ≥ 1.0× | yes | pass |
| VecL2Distance-d3 | 16.07 µs | 0 | 10.64 µs | 0 | 1.51× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d3 | 17.03 µs | 0 | 11.41 µs | 0 | 1.49× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d3 | 15.81 µs | 0 | 10.48 µs | 0 | 1.51× | ≥ 1.0× | yes | pass |
| VecL1Distance-d768 | 4,699.54 µs | 0 | 428.04 µs | 0 | 10.98× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 1,221.66 µs | 0 | 414.97 µs | 0 | 2.94× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d768 | 1,362.27 µs | 0 | 483.71 µs | 0 | 2.82× | ≥ 1.0× | yes | pass |
| VecNegativeInnerProduct-d768 | 1,210.79 µs | 0 | 403.17 µs | 0 | 3.00× | ≥ 1.0× | yes | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 21.20 µs | 0 | 16.01 µs | 0 | 1.32× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU | 16.10 µs | 0 | 16.09 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU | 16.60 µs | 0 | 16.23 µs | 0 | 1.02× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US | 16.70 µs | 0 | 16.05 µs | 0 | 1.04× | ≥ 1.0× | no | ≥ Go |
| LTInt | 19.05 µs | 0 | 17.93 µs | 0 | 1.06× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal | 11.75 µs | 0 | 11.18 µs | 0 | 1.05× | ≥ 1.0× | no | ≥ Go |
| Concat | 78.27 µs | 2455 | 85.11 µs | 3980 | 0.92× | ≥ 0.95× (allocates) | no | below Go |
| Like | 119.80 µs | 1974 | 102.41 µs | 5442 | 1.17× | ≥ 0.95× (allocates) | no | ≥ Go |
| PlusInt-SS-port | 21.20 µs | 0 | 14.45 µs | 0 | 1.47× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU-port | 16.10 µs | 0 | 14.44 µs | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 16.60 µs | 0 | 14.72 µs | 0 | 1.13× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 16.70 µs | 0 | 14.47 µs | 0 | 1.15× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 19.05 µs | 0 | 15.75 µs | 0 | 1.21× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 11.75 µs | 0 | 11.14 µs | 0 | 1.05× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 33.65 µs | 0 | 21.80 µs | 0 | 1.54× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d3 | 23.89 µs | 0 | 22.14 µs | 0 | 1.08× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d3 | 24.91 µs | 0 | 23.63 µs | 0 | 1.05× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d3 | 23.86 µs | 0 | 21.86 µs | 0 | 1.09× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d768 | 3,899.64 µs | 0 | 441.58 µs | 0 | 8.83× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d768 | 439.64 µs | 0 | 429.53 µs | 0 | 1.02× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d768 | 558.65 µs | 0 | 494.74 µs | 0 | 1.13× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d768 | 436.34 µs | 0 | 420.04 µs | 0 | 1.04× | ≥ 1.0× | no | ≥ Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 15.7 ns | pass |

**Gate passed.**
