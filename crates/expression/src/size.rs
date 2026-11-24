//! go-file: pkg/util/size/size.go
//!
//! The sizes `MemoryUsage` adds up, as Go computes them with `unsafe.Sizeof` on 64-bit
//! targets (spec §4). Only constants; the package has no functions.

#![allow(non_upper_case_globals)]

/// SizeOfSlice is the memory itself used, excludes the elements' memory
pub const SizeOfSlice: i64 = 24;
/// SizeOfPointer is the memory each pointer occupied
pub const SizeOfPointer: i64 = 8;
/// SizeOfString is the memory string itself occupied
pub const SizeOfString: i64 = 16;
/// SizeOfBool is the memory each bool occupied
pub const SizeOfBool: i64 = 1;
/// SizeOfInt is the memory each int occupied
pub const SizeOfInt: i64 = 8;
