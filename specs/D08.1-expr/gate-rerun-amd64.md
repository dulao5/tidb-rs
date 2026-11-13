```
date: 2026-11-13T20:27:37Z
uname: Linux runnervm7aw5e 7.0.0-1017-azure #17~24.04.1-Ubuntu SMP PREEMPT Thu Sep 24 18:46:57 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux
cpu: AMD EPYC 9V74 80-Core Processor, 4 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load 5.18, 6.22, 3.78
round 2/5: load 1.20, 2.72, 2.95
round 3/5: load 1.42, 1.66, 2.36
round 4/5: load 1.17, 1.32, 1.96
round 5/5: load 1.15, 1.22, 1.69
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 1,635 ns | 0 | 958 ns | 0 | 1.71× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 1,210 ns | 0 | 1,099 ns | 0 | 1.10× | ≥ 1.0× | no | pass |
| PlusInt-SU | 1,550 ns | 0 | 1,399 ns | 0 | 1.11× | ≥ 1.0× | no | pass |
| PlusInt-US | 1,639 ns | 0 | 1,349 ns | 0 | 1.21× | ≥ 1.0× | no | pass |
| LTInt | 2,421 ns | 0 | 606 ns | 0 | 3.99× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,388 ns | 0 | 420 ns | 0 | 3.30× | ≥ 1.0× | yes | pass |
| Concat | 61.95 µs | 1944 | 13.97 µs | 3 | 4.43× | ≥ 0.95× (allocates) | yes | pass |
| Like | 74.92 µs | 1323 | 41.66 µs | 1328 | 1.80× | ≥ 0.95× (allocates) | yes | pass |
| PlusInt-SS-port | 1,635 ns | 0 | 1,358 ns | 0 | 1.20× | ≥ 1.0× | no | pass |
| PlusInt-UU-port | 1,210 ns | 0 | 1,207 ns | 0 | 1.00× | ≥ 1.0× | no | pass |
| PlusInt-SU-port | 1,550 ns | 0 | 1,205 ns | 0 | 1.29× | ≥ 1.0× | no | pass |
| PlusInt-US-port | 1,639 ns | 0 | 1,210 ns | 0 | 1.36× | ≥ 1.0× | no | pass |
| LTInt-port | 2,421 ns | 0 | 609 ns | 0 | 3.97× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,388 ns | 0 | 422 ns | 0 | 3.29× | ≥ 1.0× | yes | pass |
| VecL1Distance-d3 | 20.07 µs | 0 | 12.14 µs | 0 | 1.65× | ≥ 1.0× | yes | pass |
| VecL2Distance-d3 | 20.63 µs | 0 | 11.90 µs | 0 | 1.73× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d3 | 22.07 µs | 0 | 15.06 µs | 0 | 1.47× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d3 | 20.34 µs | 0 | 11.37 µs | 0 | 1.79× | ≥ 1.0× | yes | pass |
| VecL1Distance-d768 | 4,712.30 µs | 0 | 640.16 µs | 0 | 7.36× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 884.41 µs | 0 | 636.06 µs | 0 | 1.39× | ≥ 1.0× | no | pass |
| VecCosineDistance-d768 | 893.12 µs | 0 | 659.86 µs | 0 | 1.35× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d768 | 877.60 µs | 0 | 630.04 µs | 0 | 1.39× | ≥ 1.0× | no | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 17.09 µs | 0 | 19.89 µs | 0 | 0.86× | ≥ 1.0× | no | below Go |
| PlusInt-UU | 16.89 µs | 0 | 19.89 µs | 0 | 0.85× | ≥ 1.0× | no | below Go |
| PlusInt-SU | 17.01 µs | 0 | 19.91 µs | 0 | 0.85× | ≥ 1.0× | no | below Go |
| PlusInt-US | 17.14 µs | 0 | 19.95 µs | 0 | 0.86× | ≥ 1.0× | no | below Go |
| LTInt | 17.51 µs | 0 | 18.55 µs | 0 | 0.94× | ≥ 1.0× | no | below Go |
| CastIntAsReal | 10.77 µs | 0 | 11.10 µs | 0 | 0.97× | ≥ 1.0× | no | below Go |
| Concat | 72.63 µs | 2497 | 87.41 µs | 3980 | 0.83× | ≥ 0.95× (allocates) | no | below Go |
| Like | 114.21 µs | 1983 | 86.59 µs | 5442 | 1.32× | ≥ 0.95× (allocates) | no | ≥ Go |
| PlusInt-SS-port | 17.09 µs | 0 | 16.99 µs | 0 | 1.01× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU-port | 16.89 µs | 0 | 16.60 µs | 0 | 1.02× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 17.01 µs | 0 | 17.16 µs | 0 | 0.99× | ≥ 1.0× | no | below Go |
| PlusInt-US-port | 17.14 µs | 0 | 17.25 µs | 0 | 0.99× | ≥ 1.0× | no | below Go |
| LTInt-port | 17.51 µs | 0 | 16.31 µs | 0 | 1.07× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 10.77 µs | 0 | 10.80 µs | 0 | 1.00× | ≥ 1.0× | no | below Go |
| VecL1Distance-d3 | 28.14 µs | 0 | 29.53 µs | 0 | 0.95× | ≥ 1.0× | no | below Go |
| VecL2Distance-d3 | 28.59 µs | 0 | 29.38 µs | 0 | 0.97× | ≥ 1.0× | no | below Go |
| VecCosineDistance-d3 | 30.89 µs | 0 | 33.62 µs | 0 | 0.92× | ≥ 1.0× | no | below Go |
| VecNegativeInnerProduct-d3 | 28.52 µs | 0 | 29.31 µs | 0 | 0.97× | ≥ 1.0× | no | below Go |
| VecL1Distance-d768 | 4,369.78 µs | 0 | 666.32 µs | 0 | 6.56× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d768 | 670.04 µs | 0 | 670.64 µs | 0 | 1.00× | ≥ 1.0× | no | below Go |
| VecCosineDistance-d768 | 678.28 µs | 0 | 677.56 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d768 | 665.28 µs | 0 | 664.63 µs | 0 | 1.00× | ≥ 1.0× | no | ≥ Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 14.7 ns | pass |

**Gate passed.**
