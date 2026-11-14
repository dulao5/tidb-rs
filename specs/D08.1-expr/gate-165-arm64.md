```
date: 2026-11-14T02:24:19Z
uname: Darwin Mac-mini-duzhigang.local 27.0.0 Darwin Kernel Version 27.0.0: Tue Aug 11 21:05:48 PDT 2026; root:xnu-13432.1.9~1/RELEASE_ARM64_T6041 arm64 arm Darwin
cpu: Apple M4 Pro, 12 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load1 1.93 (< 2, waited 30s); load average 1.93, 2.78, 3.09
round 2/5: load1 1.55 (< 2, waited 0s); load average 1.55, 1.85, 2.52
round 3/5: load1 1.73 (< 2, waited 0s); load average 1.73, 1.99, 2.36
round 4/5: load1 1.79 (< 2, waited 0s); load average 1.79, 1.87, 2.15
round 5/5: load1 1.97 (< 2, waited 20s); load average 1.97, 2.00, 2.11
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 1,312 ns | 0 | 391 ns | 0 | 3.36× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 714 ns | 0 | 239 ns | 0 | 2.99× | ≥ 1.0× | yes | pass |
| PlusInt-SU | 943 ns | 0 | 293 ns | 0 | 3.22× | ≥ 1.0× | yes | pass |
| PlusInt-US | 939 ns | 0 | 293 ns | 0 | 3.21× | ≥ 1.0× | yes | pass |
| LTInt | 1,703 ns | 0 | 251 ns | 0 | 6.80× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 1,045 ns | 0 | 200 ns | 0 | 5.22× | ≥ 1.0× | yes | pass |
| Concat | 35.15 µs | 1867 | 13.08 µs | 3 | 2.69× | ≥ 0.95× (allocates) | yes | pass |
| Like | 51.71 µs | 1371 | 40.47 µs | 1328 | 1.28× | ≥ 0.95× (allocates) | no | pass |
| PlusInt-SS-port | 1,312 ns | 0 | 388 ns | 0 | 3.38× | ≥ 1.0× | yes | pass |
| PlusInt-UU-port | 714 ns | 0 | 239 ns | 0 | 2.99× | ≥ 1.0× | yes | pass |
| PlusInt-SU-port | 943 ns | 0 | 292 ns | 0 | 3.23× | ≥ 1.0× | yes | pass |
| PlusInt-US-port | 939 ns | 0 | 294 ns | 0 | 3.20× | ≥ 1.0× | yes | pass |
| LTInt-port | 1,703 ns | 0 | 254 ns | 0 | 6.71× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 1,045 ns | 0 | 201 ns | 0 | 5.20× | ≥ 1.0× | yes | pass |
| VecL1Distance-d3 | 22.87 µs | 0 | 7,094 ns | 0 | 3.22× | ≥ 1.0× | yes | pass |
| VecL2Distance-d3 | 10.20 µs | 0 | 6,855 ns | 0 | 1.49× | ≥ 1.0× | no | pass |
| VecCosineDistance-d3 | 11.37 µs | 0 | 7,225 ns | 0 | 1.57× | ≥ 1.0× | yes | pass |
| VecNegativeInnerProduct-d3 | 10.17 µs | 0 | 6,816 ns | 0 | 1.49× | ≥ 1.0× | no | pass |
| VecL1Distance-d768 | 3,256.03 µs | 0 | 338.43 µs | 0 | 9.62× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 737.21 µs | 0 | 556.89 µs | 0 | 1.32× | ≥ 1.0× | no | pass |
| VecCosineDistance-d768 | 745.55 µs | 0 | 575.81 µs | 0 | 1.29× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d768 | 692.39 µs | 0 | 538.60 µs | 0 | 1.29× | ≥ 1.0× | no | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 12.36 µs | 0 | 8,833 ns | 0 | 1.40× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU | 10.34 µs | 0 | 8,831 ns | 0 | 1.17× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU | 10.56 µs | 0 | 8,836 ns | 0 | 1.20× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US | 10.49 µs | 0 | 8,834 ns | 0 | 1.19× | ≥ 1.0× | no | ≥ Go |
| LTInt | 13.75 µs | 0 | 10.44 µs | 0 | 1.32× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal | 8,668 ns | 0 | 5,862 ns | 0 | 1.48× | ≥ 1.0× | no | ≥ Go |
| Concat | 46.83 µs | 2390 | 81.09 µs | 3980 | 0.58× | ≥ 0.95× (allocates) | no | below Go |
| Like | 69.51 µs | 2055 | 89.97 µs | 5442 | 0.77× | ≥ 0.95× (allocates) | no | below Go |
| PlusInt-SS-port | 12.36 µs | 0 | 7,911 ns | 0 | 1.56× | ≥ 1.0× | yes | ≥ Go |
| PlusInt-UU-port | 10.34 µs | 0 | 7,945 ns | 0 | 1.30× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 10.56 µs | 0 | 8,167 ns | 0 | 1.29× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 10.49 µs | 0 | 7,927 ns | 0 | 1.32× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 13.75 µs | 0 | 10.48 µs | 0 | 1.31× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 8,668 ns | 0 | 6,501 ns | 0 | 1.33× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 29.83 µs | 0 | 14.96 µs | 0 | 1.99× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d3 | 16.40 µs | 0 | 14.62 µs | 0 | 1.12× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d3 | 16.85 µs | 0 | 15.26 µs | 0 | 1.10× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d3 | 16.36 µs | 0 | 14.38 µs | 0 | 1.14× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d768 | 3,107.14 µs | 0 | 343.17 µs | 0 | 9.05× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d768 | 651.15 µs | 0 | 574.99 µs | 0 | 1.13× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d768 | 661.08 µs | 0 | 597.11 µs | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| VecNegativeInnerProduct-d768 | 605.53 µs | 0 | 555.33 µs | 0 | 1.09× | ≥ 1.0× | no | ≥ Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 9.8 ns | pass |

**Gate passed.**
