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
//! - [`crypto`] / [`encrypting_store`] — client-side payload encryption (DEV-189).
//! - [`push`] — sync-up: build + upload a session bundle, git precondition (DEV-193).
//! - [`pull`] — sync-down: browse + pull a bundle into local state, project-gated (DEV-194).
//!
//! Public items are the surface the UI (DEV-195) builds on, so a binary-crate
//! dead-code sweep flags some until then — allow it.
#![allow(dead_code)]

pub mod config;
pub mod connect;
pub mod crypto;
pub mod encrypting_store;
pub mod identity;
pub mod ledger;
pub mod meta;
pub mod pull;
pub mod push;
pub mod rt;
pub mod s3_store;
pub mod store;

// Flat `crate::sync::…` surface; consumers land in the push/pull flows.
#[allow(unused_imports)]
pub use meta::{ProjectIdentity, SessionBundleMeta, SyncHeader};
#[allow(unused_imports)]
pub use store::{meta_key, session_id_from_key, MemStore, SyncStore, SESSIONS_PREFIX};

#[cfg(test)]
pub(crate) mod leak_check {
    //! Asserting that plaintext did not survive encryption (DEV-433).
    //!
    //! The obvious way to write this check is subtly wrong:
    //!
    //! ```ignore
    //! assert!(!at_rest.windows(2).any(|w| w == b"My"));
    //! ```
    //!
    //! Ciphertext is effectively uniform random bytes, so a short needle
    //! occurs *by chance*. For a haystack of `n` bytes and a needle of `k`
    //! bytes the probability is roughly `n / 256^k` — with a two-byte needle
    //! and a few tens of kilobytes of ciphertext that is a coin flip, and the
    //! ciphertext differs every run, so it fails intermittently forever.
    //!
    //! One such assertion cost several days of "failed once, passed on rerun,
    //! believed pre-existing" before anyone worked out it was arithmetic
    //! rather than the environment.
    //!
    //! The needle length is enforced rather than advised, so the mistake
    //! cannot be reintroduced quietly.

    /// Shortest needle whose accidental occurrence is negligible.
    ///
    /// At eight bytes the chance is about `n / 2^64` — one in millions of
    /// millions for any plausible payload, against roughly one in three for
    /// the two-byte version this replaced.
    const MIN_NEEDLE: usize = 8;

    /// Assert `needle` does not appear anywhere in `haystack`.
    ///
    /// Panics if the needle is too short to be a reliable probe, because a
    /// test that fails at random is worse than no test: it trains everyone
    /// reading it to disbelieve the suite.
    pub(crate) fn assert_not_leaked(haystack: &[u8], needle: &[u8], what: &str) {
        assert!(
            needle.len() >= MIN_NEEDLE,
            "leak probe {needle:?} is {} bytes; needs at least {MIN_NEEDLE} or it will \
             match random ciphertext by chance. Use a longer, distinctive plaintext.",
            needle.len(),
        );
        assert!(
            !haystack.windows(needle.len()).any(|w| w == needle),
            "{what} leaked into ciphertext",
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_present_needle_is_caught() {
            let caught = std::panic::catch_unwind(|| {
                assert_not_leaked(b"xxxxSECRET-VALUExxxx", b"SECRET-VALUE", "secret")
            });
            assert!(caught.is_err(), "a real leak must fail the assertion");
        }

        #[test]
        fn an_absent_needle_passes() {
            assert_not_leaked(b"xxxxxxxxxxxxxxxxxxxx", b"SECRET-VALUE", "secret");
        }

        /// The guard that stops the original bug returning.
        #[test]
        fn a_short_needle_is_rejected_rather_than_silently_flaky() {
            let rejected =
                std::panic::catch_unwind(|| assert_not_leaked(b"haystack", b"My", "label"));
            assert!(rejected.is_err(), "a 2-byte probe must be refused");
        }
    }
}
