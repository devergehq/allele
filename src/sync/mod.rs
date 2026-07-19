//! Cross-machine session sync (Allele Session Sync — Phase 1).
//!
//! A session can be pushed from one Mac and resumed on another. The design is
//! manual, per-session, project-gated, and replace-never-merge — see
//! `Plans/SESSION-SYNC-PROPOSAL.md` for the full rationale.
//!
//! Submodules:
//! - [`store`] — the [`SyncStore`] object-store abstraction + `MemStore` (DEV-187).
//! - [`s3_store`] — S3-compatible `SyncStore` via rust-s3 (DEV-187).
//! - [`meta`] — portable [`SessionBundleMeta`] schema + path normalization (DEV-190).
//! - [`ledger`] — per-session revision/base ledger (DEV-192).
//! - [`identity`] — project identity + git-remote resolver, the sync gate (DEV-191).
//!
//! Public items are the surface later Phase 1 tasks build on, so a binary-crate
//! dead-code sweep flags some until then — allow it.
#![allow(dead_code)]

pub mod identity;
pub mod ledger;
pub mod meta;
pub mod s3_store;
pub mod store;

// Flat `crate::sync::…` surface; consumers land in later Phase 1 tasks.
#[allow(unused_imports)]
pub use meta::{ProjectIdentity, SessionBundleMeta, SyncHeader};
#[allow(unused_imports)]
pub use store::{meta_key, session_id_from_key, MemStore, SyncStore, SESSIONS_PREFIX};
