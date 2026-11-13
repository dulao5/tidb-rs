```
date: 2026-11-13T20:18:09Z
uname: Darwin Mac-mini-duzhigang.local 27.0.0 Darwin Kernel Version 27.0.0: Tue Aug 11 21:05:48 PDT 2026; root:xnu-13432.1.9~1/RELEASE_ARM64_T6041 arm64 arm Darwin
cpu: Apple M4 Pro, 12 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load 14.88, 7.69, 4.40
round 2/5: load 2.36, 3.90, 3.70
round 3/5: load 2.60, 2.81, 3.19
round 4/5: load 1.81, 2.37, 2.83
round 5/5: load 2.01, 2.16, 2.57
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 1,377 ns | 0 | 477 ns | 0 | 2.89× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 736 ns | 0 | 458 ns | 0 | 1.61× | ≥ 1.0× | yes | pass |
| PlusInt-SU | 975 ns | 0 | 666 ns | 0 | 1.46× | ≥ 1.0× | no | pass |
| PlusInt-US | 976 ns | 0 | 666 ns | 0 | 1.47× | ≥ 1.0× | no | pass |
| LTInt | 1,782 ns | 0 | 370 ns | 0 | 4.82× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,065 ns | 0 | 264 ns | 0 | 4.03× | ≥ 1.0× | yes | pass |
| Concat | 35.46 µs | 1911 | 13.26 µs | 3 | 2.67× | ≥ 0.95× (allocates) | yes | pass |
| Like | 55.25 µs | 1325 | 40.86 µs | 1328 | 1.35× | ≥ 0.95× (allocates) | no | pass |
| PlusInt-SS-port | 1,377 ns | 0 | 1,079 ns | 0 | 1.28× | ≥ 1.0× | no | pass |
| PlusInt-UU-port | 736 ns | 0 | 454 ns | 0 | 1.62× | ≥ 1.0× | yes | pass |
| PlusInt-SU-port | 975 ns | 0 | 658 ns | 0 | 1.48× | ≥ 1.0× | no | pass |
| PlusInt-US-port | 976 ns | 0 | 674 ns | 0 | 1.45× | ≥ 1.0× | no | pass |
| LTInt-port | 1,782 ns | 0 | 369 ns | 0 | 4.83× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,065 ns | 0 | 265 ns | 0 | 4.02× | ≥ 1.0× | yes | pass |
| VecL1Distance-d3 | 23.27 µs | 0 | 6,974 ns | 0 | 3.34× | ≥ 1.0× | yes | pass |
| VecL2Distance-d3 | 10.34 µs | 0 | 6,850 ns | 0 | 1.51× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d3 | 11.42 µs | 0 | 7,171 ns | 0 | 1.59× | ≥ 1.0× | yes | pass |
| VecNegativeInnerProduct-d3 | 10.14 µs | 0 | 6,804 ns | 0 | 1.49× | ≥ 1.0× | no | pass |
| VecL1Distance-d768 | 3,244.25 µs | 0 | 338.41 µs | 0 | 9.59× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 736.96 µs | 0 | 555.85 µs | 0 | 1.33× | ≥ 1.0× | no | pass |
| VecCosineDistance-d768 | 758.82 µs | 0 | 580.85 µs | 0 | 1.31× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d768 | 707.57 µs | 0 | 538.60 µs | 0 | 1.31× | ≥ 1.0× | no | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 12.77 µs | 0 | 8,823 ns | 0 | 1.45× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU | 10.34 µs | 0 | 8,821 ns | 0 | 1.17× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU | 10.59 µs | 0 | 8,815 ns | 0 | 1.20× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US | 10.53 µs | 0 | 8,815 ns | 0 | 1.19× | ≥ 1.0× | no | ≥ Go |
| LTInt | 13.47 µs | 0 | 10.41 µs | 0 | 1.29× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal | 8,733 ns | 0 | 5,903 ns | 0 | 1.48× | ≥ 1.0× | no | ≥ Go |
| Concat | 47.67 µs | 2454 | 82.82 µs | 3980 | 0.58× | ≥ 0.95× (allocates) | no | below Go |
| Like | 72.80 µs | 1986 | 92.14 µs | 5442 | 0.79× | ≥ 0.95× (allocates) | no | below Go |
| PlusInt-SS-port | 12.77 µs | 0 | 10.14 µs | 0 | 1.26× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU-port | 10.34 µs | 0 | 7,898 ns | 0 | 1.31× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 10.59 µs | 0 | 7,935 ns | 0 | 1.34× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 10.53 µs | 0 | 8,013 ns | 0 | 1.31× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 13.47 µs | 0 | 10.41 µs | 0 | 1.29× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 8,733 ns | 0 | 6,560 ns | 0 | 1.33× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 30.20 µs | 0 | 14.76 µs | 0 | 2.05× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d3 | 16.48 µs | 0 | 14.72 µs | 0 | 1.12× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d3 | 16.92 µs | 0 | 15.20 µs | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d3 | 16.39 µs | 0 | 14.42 µs | 0 | 1.14× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d768 | 3,139.61 µs | 0 | 343.54 µs | 0 | 9.14× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d768 | 654.07 µs | 0 | 574.38 µs | 0 | 1.14× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d768 | 671.89 µs | 0 | 597.47 µs | 0 | 1.12× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d768 | 612.34 µs | 0 | 554.85 µs | 0 | 1.10× | ≥ 1.0× | no | ≥ Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 9.5 ns | pass |

**Gate passed.**
