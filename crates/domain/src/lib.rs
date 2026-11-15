//! The domain crate (N0 rank 12): block D06.5, `pkg/domain`.
//!
//! Part 1 (specs/D06.5-domain, VS1-17): schema loading. [`domain`] has the `Domain` with its
//! `InfoCache` (D06.3) and `loadInfoSchema` / `tryLoadSchemaDiffs` / the full-load fetches /
//! `Reload`. Until the rest of the domain is ported (the schema syncer, the reload loop,
//! the schema validator, infosync, the background jobs), the PoC domain's sync loop
//! (`poc-domain`) calls [`domain::Domain::reload_at`] at the timestamp of each of its own
//! reloads, so the PoC and the new stack load the same schema versions (VS1 plan §4.1).
// N13 rule 5 (M-SHARE): a clone of an `Arc` is written `Arc::clone(&x)`.
#![warn(clippy::clone_on_ref_ptr)]

pub mod domain;

pub use domain::{Domain, RelatedSchemaChange, new_domain};
