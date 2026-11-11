//! go-file: pkg/util/size/size.go
//!
//! The sizes `MemoryUsage` adds up, as Go computes them with `unsafe.Sizeof` on 64-bit
//! targets (spec §4). Only constants; the package has no functions.

#![allow(non_upper_case_globals)]

/// SizeOfSlice is the memory itself used, excludes the elements' memory
pub const SizeOfSlice: i64 = 24;
/// SizeOfPointer is the memory each pointer occupied
pub const SizeOfPointer: i64 = 8;
