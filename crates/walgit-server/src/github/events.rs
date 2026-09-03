//! The hook point where a GitHub-shaped webhook would be produced.
//!
//! Deliberately not a producer. Principle III: side effects are readers of the
//! WAL, never steps of a write — a `push` or `pull_request` delivery must come
//! from something tailing the log from a durable cursor (`crate::bridge`,
//! `docs/EVENTS.md`), so that a failed delivery cannot fail a write and a
//! replay from the cursor is exact. What lives here is the *shape* and the one
//! call site every facade write already makes, so the later phase is a sink
//! registration and a converter, not a hunt through `write.rs` for the places
//! a write completes.

use walgit_git::RepoId;

/// What the facade did, in GitHub's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A ref moved: `push` in webhook terms.
    Push,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Push => "push",
        }
    }
}

/// A write became visible in the bucket at `seq`. Called after the manifest
/// CAS, never before: nothing may observe a write the WAL has not committed.
///
/// `old` / `new` are hex or `""` (create / delete), matching the WAL's own
/// convention rather than GitHub's forty zeros; a converter maps them when
/// there is a payload to convert.
pub fn ref_written(repo: &RepoId, ref_name: &str, old: &str, new: &str, seq: u64) {
    tracing::debug!(
        repo = %repo,
        event = Kind::Push.as_str(),
        ref_name,
        old,
        new,
        seq,
        "github facade write committed"
    );
}
