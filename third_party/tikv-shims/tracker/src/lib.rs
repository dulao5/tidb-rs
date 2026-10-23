//! Placeholder for TiKV's `tracker` crate.
//!
//! `tidb_query_common` lists `tracker` as an optional dependency of its
//! `coprocessor` feature. Cargo resolves optional dependencies even when they
//! are not enabled, so the package has to exist. The `coprocessor` feature is
//! not supported in tidb-rs; this crate is never compiled.
