```
date: 2026-11-13T23:41:54Z
uname: Darwin Mac-mini-duzhigang.local 27.0.0 Darwin Kernel Version 27.0.0: Tue Aug 11 21:05:48 PDT 2026; root:xnu-13432.1.9~1/RELEASE_ARM64_T6041 arm64 arm Darwin
cpu: Apple M4 Pro, 12 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load1 1.74 (< 2, waited 260s); load average 1.74, 8.39, 9.10
round 2/5: load1 1.44 (< 2, waited 0s); load average 1.44, 3.57, 6.44
round 3/5: load1 1.91 (< 2, waited 260s); load average 1.91, 6.40, 7.13
round 4/5: load1 1.93 (< 2, waited 20s); load average 1.93, 4.02, 5.76
round 5/5: load1 1.91 (< 2, waited 690s); load average 1.91, 5.47, 5.68
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 1,337 ns | 0 | 477 ns | 0 | 2.81× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 719 ns | 0 | 457 ns | 0 | 1.57× | ≥ 1.0× | yes | pass |
| PlusInt-SU | 954 ns | 0 | 667 ns | 0 | 1.43× | ≥ 1.0× | no | pass |
| PlusInt-US | 960 ns | 0 | 665 ns | 0 | 1.44× | ≥ 1.0× | no | pass |
| LTInt | 1,734 ns | 0 | 370 ns | 0 | 4.68× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,054 ns | 0 | 265 ns | 0 | 3.98× | ≥ 1.0× | yes | pass |
| Concat | 34.65 µs | 1797 | 13.21 µs | 3 | 2.62× | ≥ 0.95× (allocates) | yes | pass |
| Like | 54.43 µs | 1309 | 40.20 µs | 1328 | 1.35× | ≥ 0.95× (allocates) | no | pass |
| PlusInt-SS-port | 1,337 ns | 0 | 476 ns | 0 | 2.81× | ≥ 1.0× | yes | pass |
| PlusInt-UU-port | 719 ns | 0 | 461 ns | 0 | 1.56× | ≥ 1.0× | yes | pass |
| PlusInt-SU-port | 954 ns | 0 | 667 ns | 0 | 1.43× | ≥ 1.0× | no | pass |
| PlusInt-US-port | 960 ns | 0 | 667 ns | 0 | 1.44× | ≥ 1.0× | no | pass |
| LTInt-port | 1,734 ns | 0 | 369 ns | 0 | 4.69× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,054 ns | 0 | 266 ns | 0 | 3.96× | ≥ 1.0× | yes | pass |
| VecL1Distance-d3 | 23.10 µs | 0 | 7,123 ns | 0 | 3.24× | ≥ 1.0× | yes | pass |
| VecL2Distance-d3 | 10.37 µs | 0 | 6,880 ns | 0 | 1.51× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d3 | 11.43 µs | 0 | 7,175 ns | 0 | 1.59× | ≥ 1.0× | yes | pass |
| VecNegativeInnerProduct-d3 | 10.21 µs | 0 | 6,820 ns | 0 | 1.50× | ≥ 1.0× | no | pass |
| VecL1Distance-d768 | 3,265.73 µs | 0 | 339.30 µs | 0 | 9.62× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 744.21 µs | 0 | 563.49 µs | 0 | 1.32× | ≥ 1.0× | no | pass |
| VecCosineDistance-d768 | 756.97 µs | 0 | 589.18 µs | 0 | 1.28× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d768 | 700.20 µs | 0 | 538.81 µs | 0 | 1.30× | ≥ 1.0× | no | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 12.64 µs | 0 | 8,836 ns | 0 | 1.43× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU | 10.32 µs | 0 | 8,834 ns | 0 | 1.17× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU | 10.66 µs | 0 | 8,840 ns | 0 | 1.21× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US | 10.57 µs | 0 | 8,842 ns | 0 | 1.20× | ≥ 1.0× | no | ≥ Go |
| LTInt | 13.43 µs | 0 | 10.55 µs | 0 | 1.27× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal | 8,809 ns | 0 | 6,034 ns | 0 | 1.46× | ≥ 1.0× | no | ≥ Go |
| Concat | 46.09 µs | 2294 | 81.36 µs | 3980 | 0.57× | ≥ 0.95× (allocates) | no | below Go |
| Like | 72.59 µs | 1962 | 91.77 µs | 5442 | 0.79× | ≥ 0.95× (allocates) | no | below Go |
| PlusInt-SS-port | 12.64 µs | 0 | 7,767 ns | 0 | 1.63× | ≥ 1.0× | yes | ≥ Go |
| PlusInt-UU-port | 10.32 µs | 0 | 7,784 ns | 0 | 1.33× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 10.66 µs | 0 | 7,910 ns | 0 | 1.35× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 10.57 µs | 0 | 7,827 ns | 0 | 1.35× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 13.43 µs | 0 | 10.61 µs | 0 | 1.27× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 8,809 ns | 0 | 6,644 ns | 0 | 1.33× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 29.73 µs | 0 | 14.80 µs | 0 | 2.01× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d3 | 16.48 µs | 0 | 14.56 µs | 0 | 1.13× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d3 | 16.85 µs | 0 | 15.20 µs | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d3 | 16.55 µs | 0 | 14.45 µs | 0 | 1.15× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d768 | 3,134.92 µs | 0 | 342.56 µs | 0 | 9.15× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d768 | 656.08 µs | 0 | 586.57 µs | 0 | 1.12× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d768 | 666.74 µs | 0 | 610.04 µs | 0 | 1.09× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d768 | 605.00 µs | 0 | 555.71 µs | 0 | 1.09× | ≥ 1.0× | no | ≥ Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 9.6 ns | pass |

**Gate passed.**
