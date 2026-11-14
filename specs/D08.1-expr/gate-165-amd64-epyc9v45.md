```
date: 2026-11-14T02:32:29Z
uname: Linux runnervm8axu0 7.0.0-1017-azure #17~24.04.1-Ubuntu SMP PREEMPT Thu Sep 24 18:46:57 UTC 2026 x86_64 x86_64 x86_64 GNU/Linux
cpu: AMD EPYC 9V45 96-Core Processor, 4 cores
go: go1.25.12
rustc: rustc 1.99.1 (58cf8f9c3 2026-10-13)
round 1/5: load1 1.89 (< 2, waited 60s); load average 1.89, 4.93, 3.22
round 2/5: load1 1.23 (< 2, waited 0s); load average 1.23, 2.20, 2.52
round 3/5: load1 1.20 (< 2, waited 0s); load average 1.20, 1.48, 2.05
round 4/5: load1 1.20 (< 2, waited 0s); load average 1.20, 1.33, 1.78
round 5/5: load1 1.40 (< 2, waited 0s); load average 1.40, 1.35, 1.63
```

Minimum over 5 Go processes (3 samples each) and 5 Rust processes; ns per 1024-row batch.

### Vector driver (gate)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 904 ns | 0 | 490 ns | 0 | 1.84× | ≥ 1.0× | yes | pass |
| PlusInt-UU | 696 ns | 0 | 460 ns | 0 | 1.51× | ≥ 1.0× | yes | pass |
| PlusInt-SU | 785 ns | 0 | 549 ns | 0 | 1.43× | ≥ 1.0× | no | pass |
| PlusInt-US | 887 ns | 0 | 541 ns | 0 | 1.64× | ≥ 1.0× | yes | pass |
| LTInt | 1,057 ns | 0 | 430 ns | 0 | 2.46× | ≥ 1.0× | yes | pass |
| CastIntAsReal | 908 ns | 0 | 333 ns | 0 | 2.72× | ≥ 1.0× | yes | pass |
| Concat | 44.34 µs | 1930 | 8,903 ns | 3 | 4.98× | ≥ 0.95× (allocates) | yes | pass |
| Like | 62.38 µs | 1351 | 30.13 µs | 1328 | 2.07× | ≥ 0.95× (allocates) | yes | pass |
| PlusInt-SS-port | 904 ns | 0 | 494 ns | 0 | 1.83× | ≥ 1.0× | yes | pass |
| PlusInt-UU-port | 696 ns | 0 | 464 ns | 0 | 1.50× | ≥ 1.0× | no | pass |
| PlusInt-SU-port | 785 ns | 0 | 547 ns | 0 | 1.44× | ≥ 1.0× | no | pass |
| PlusInt-US-port | 887 ns | 0 | 539 ns | 0 | 1.64× | ≥ 1.0× | yes | pass |
| LTInt-port | 1,057 ns | 0 | 428 ns | 0 | 2.47× | ≥ 1.0× | yes | pass |
| CastIntAsReal-port | 908 ns | 0 | 250 ns | 0 | 3.64× | ≥ 1.0× | yes | pass |
| VecL1Distance-d3 | 15.09 µs | 0 | 7,071 ns | 0 | 2.13× | ≥ 1.0× | yes | pass |
| VecL2Distance-d3 | 13.13 µs | 0 | 7,806 ns | 0 | 1.68× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d3 | 14.09 µs | 0 | 9,099 ns | 0 | 1.55× | ≥ 1.0× | yes | pass |
| VecNegativeInnerProduct-d3 | 12.89 µs | 0 | 7,567 ns | 0 | 1.70× | ≥ 1.0× | yes | pass |
| VecL1Distance-d768 | 3,947.09 µs | 0 | 373.77 µs | 0 | 10.56× | ≥ 1.0× | yes | pass |
| VecL2Distance-d768 | 509.87 µs | 0 | 338.70 µs | 0 | 1.51× | ≥ 1.0× | yes | pass |
| VecCosineDistance-d768 | 529.33 µs | 0 | 391.48 µs | 0 | 1.35× | ≥ 1.0× | no | pass |
| VecNegativeInnerProduct-d768 | 510.12 µs | 0 | 329.37 µs | 0 | 1.55× | ≥ 1.0× | yes | pass |

### Row driver (not gated, #75)

| case | Go | Go allocs/op | Rust | Rust allocs/batch | Go/Rust | threshold | ≥ 1.5× | result |
|---|--:|--:|--:|--:|--:|--:|:-:|:-:|
| PlusInt-SS | 15.82 µs | 0 | 13.67 µs | 0 | 1.16× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU | 12.23 µs | 0 | 13.68 µs | 0 | 0.89× | ≥ 1.0× | no | below Go |
| PlusInt-SU | 12.36 µs | 0 | 13.91 µs | 0 | 0.89× | ≥ 1.0× | no | below Go |
| PlusInt-US | 12.46 µs | 0 | 13.92 µs | 0 | 0.90× | ≥ 1.0× | no | below Go |
| LTInt | 17.66 µs | 0 | 16.14 µs | 0 | 1.09× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal | 10.48 µs | 0 | 9,790 ns | 0 | 1.07× | ≥ 1.0× | no | ≥ Go |
| Concat | 60.39 µs | 2478 | 67.39 µs | 3980 | 0.90× | ≥ 0.95× (allocates) | no | below Go |
| Like | 86.20 µs | 2025 | 71.87 µs | 5442 | 1.20× | ≥ 0.95× (allocates) | no | ≥ Go |
| PlusInt-SS-port | 15.82 µs | 0 | 11.64 µs | 0 | 1.36× | ≥ 1.0× | no | ≥ Go |
| PlusInt-UU-port | 12.23 µs | 0 | 11.64 µs | 0 | 1.05× | ≥ 1.0× | no | ≥ Go |
| PlusInt-SU-port | 12.36 µs | 0 | 11.63 µs | 0 | 1.06× | ≥ 1.0× | no | ≥ Go |
| PlusInt-US-port | 12.46 µs | 0 | 11.64 µs | 0 | 1.07× | ≥ 1.0× | no | ≥ Go |
| LTInt-port | 17.66 µs | 0 | 14.68 µs | 0 | 1.20× | ≥ 1.0× | no | ≥ Go |
| CastIntAsReal-port | 10.48 µs | 0 | 9,466 ns | 0 | 1.11× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d3 | 33.48 µs | 0 | 18.16 µs | 0 | 1.84× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d3 | 20.33 µs | 0 | 19.04 µs | 0 | 1.07× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d3 | 22.48 µs | 0 | 22.66 µs | 0 | 0.99× | ≥ 1.0× | no | below Go |
| VecNegativeInnerProduct-d3 | 20.21 µs | 0 | 18.41 µs | 0 | 1.10× | ≥ 1.0× | no | ≥ Go |
| VecL1Distance-d768 | 3,768.55 µs | 0 | 393.33 µs | 0 | 9.58× | ≥ 1.0× | yes | ≥ Go |
| VecL2Distance-d768 | 376.79 µs | 0 | 370.99 µs | 0 | 1.02× | ≥ 1.0× | no | ≥ Go |
| VecCosineDistance-d768 | 392.39 µs | 0 | 406.26 µs | 0 | 0.97× | ≥ 1.0× | no | below Go |
| VecNegativeInnerProduct-d768 | 374.41 µs | 0 | 365.27 µs | 0 | 1.03× | ≥ 1.0× | no | ≥ Go |

### Other N4 §5 metrics

| metric | threshold | result | verdict |
|---|---|---|:-:|
| heap allocations per chunk, fixed-length results (20 cases, vector driver) | 0 | 0 in every case | pass |
| `RpnProgram::compile` per node (66 nodes) | ≤ 1 µs | 12.6 ns | pass |

**Gate passed.**
