```
date: 2026-11-13T19:10:37Z
uname: Darwin Mac-mini-duzhigang.local 27.0.0 Darwin Kernel Version 27.0.0: Tue Aug 11 21:05:48 PDT 2026; root:xnu-13432.1.9~1/RELEASE_ARM64_T6041 arm64 arm Darwin
cpu: Apple M4 Pro, 12 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load 4.46, 7.61, 6.67
round 2/5: load 4.17, 4.38, 5.31
round 3/5: load 2.08, 2.70, 4.13
round 4/5: load 4.56, 3.32, 3.83
round 5/5: load 2.40, 3.10, 3.55
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 1,380 ns | 0 | 477 ns | 0 | 2.89× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 738 ns | 0 | 459 ns | 0 | 1.61× | ≥ 1.0× | yes | pass |
| PlusInt-SU | 980 ns | 0 | 669 ns | 0 | 1.47× | ≥ 1.0× | no | pass |
| PlusInt-US | 985 ns | 0 | 669 ns | 0 | 1.47× | ≥ 1.0× | no | pass |
| LTInt | 1,803 ns | 0 | 370 ns | 0 | 4.87× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,069 ns | 0 | 265 ns | 0 | 4.03× | ≥ 1.0× | yes | pass |
| Concat | 35.59 µs | 1837 | 13.30 µs | 3 | 2.67× | ≥ 0.95× (allocates) | yes | pass |
| Like | 54.72 µs | 1327 | 41.00 µs | 1328 | 1.33× | ≥ 0.95× (allocates) | no | pass |
| PlusInt-SS-port | 1,380 ns | 0 | 1,097 ns | 0 | 1.26× | ≥ 1.0× | no | pass |
| PlusInt-UU-port | 738 ns | 0 | 454 ns | 0 | 1.63× | ≥ 1.0× | yes | pass |
| PlusInt-SU-port | 980 ns | 0 | 674 ns | 0 | 1.45× | ≥ 1.0× | no | pass |
| PlusInt-US-port | 985 ns | 0 | 672 ns | 0 | 1.47× | ≥ 1.0× | no | pass |
| LTInt-port | 1,803 ns | 0 | 370 ns | 0 | 4.87× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,069 ns | 0 | 1,178 ns | 0 | 0.91× | ≥ 1.0× | no | **FAIL** |
| VecL1Distance-d3 | 23.20 µs | 0 | 17.20 µs | 0 | 1.35× | ≥ 1.0× | no | pass |
| VecL2Distance-d3 | 10.20 µs | 0 | 19.53 µs | 0 | 0.52× | ≥ 1.0× | no | **FAIL** |
| VecCosineDistance-d3 | 11.47 µs | 0 | 25.05 µs | 0 | 0.46× | ≥ 1.0× | no | **FAIL** |
| VecNegativeInnerProduct-d3 | 10.15 µs | 0 | 16.50 µs | 0 | 0.62× | ≥ 1.0× | no | **FAIL** |
| VecL1Distance-d768 | 3,320.63 µs | 0 | 408.32 µs | 0 | 8.13× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 755.18 µs | 0 | 634.23 µs | 0 | 1.19× | ≥ 1.0× | no | pass |
| VecCosineDistance-d768 | 769.42 µs | 0 | 640.15 µs | 0 | 1.20× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d768 | 705.59 µs | 0 | 613.36 µs | 0 | 1.15× | ≥ 1.0× | no | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 12.88 µs | 0 | 8,827 ns | 0 | 1.46× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU | 10.35 µs | 0 | 8,822 ns | 0 | 1.17× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU | 10.56 µs | 0 | 8,821 ns | 0 | 1.20× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US | 10.43 µs | 0 | 8,820 ns | 0 | 1.18× | ≥ 1.0× | no | ≥ Go |
| LTInt | 13.78 µs | 0 | 10.76 µs | 0 | 1.28× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal | 8,811 ns | 0 | 6,221 ns | 0 | 1.42× | ≥ 1.0× | no | ≥ Go |
| Concat | 47.09 µs | 2331 | 83.39 µs | 3980 | 0.56× | ≥ 0.95× (allocates) | no | below Go |
| Like | 72.80 µs | 1989 | 93.05 µs | 5442 | 0.78× | ≥ 0.95× (allocates) | no | below Go |
| PlusInt-SS-port | 12.88 µs | 0 | 10.73 µs | 0 | 1.20× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU-port | 10.35 µs | 0 | 8,153 ns | 0 | 1.27× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 10.56 µs | 0 | 8,210 ns | 0 | 1.29× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 10.43 µs | 0 | 8,199 ns | 0 | 1.27× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 13.78 µs | 0 | 10.79 µs | 0 | 1.28× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 8,811 ns | 0 | 6,422 ns | 0 | 1.37× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 29.64 µs | 0 | 39.89 µs | 2048 | 0.74× | ≥ 0.95× (allocates) | no | below Go |
| VecL2Distance-d3 | 16.37 µs | 0 | 40.38 µs | 2048 | 0.41× | ≥ 0.95× (allocates) | no | below Go |
| VecCosineDistance-d3 | 16.90 µs | 0 | 40.05 µs | 2048 | 0.42× | ≥ 0.95× (allocates) | no | below Go |
| VecNegativeInnerProduct-d3 | 16.44 µs | 0 | 40.10 µs | 2048 | 0.41× | ≥ 0.95× (allocates) | no | below Go |
| VecL1Distance-d768 | 3,478.28 µs | 0 | 506.37 µs | 2048 | 6.87× | ≥ 0.95× (allocates) | yes | ≥ Go |
| VecL2Distance-d768 | 662.79 µs | 0 | 752.78 µs | 2048 | 0.88× | ≥ 0.95× (allocates) | no | below Go |
| VecCosineDistance-d768 | 682.77 µs | 0 | 767.07 µs | 2048 | 0.89× | ≥ 0.95× (allocates) | no | below Go |
| VecNegativeInnerProduct-d768 | 621.68 µs | 0 | 729.25 µs | 2048 | 0.85× | ≥ 0.95× (allocates) | no | below Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 9.4 ns | pass |

**Gate failed:** CastIntAsReal-port-Vec: 0.91× < 1.00×; VecL2Distance-d3-Vec: 0.52× < 1.00×; VecCosineDistance-d3-Vec: 0.46× < 1.00×; VecNegativeInnerProduct-d3-Vec: 0.62× < 1.00×
