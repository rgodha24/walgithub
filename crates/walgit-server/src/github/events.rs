//! The one call site every facade write makes once it is committed.
//!
//! Still not a producer. Principle III: side effects are readers of the WAL,
//! never steps of a write — the `push` / `create` / `delete` deliveries are
//! rendered by [`super::webhook::GithubSink`] from log entries the bridge
//! reads from a durable cursor (`crate::bridge`, `docs/EVENTS.md`), so a
//! failed delivery cannot fail a write and a replay from the cursor is exact.
//! What happens here is a *wake-up*: the bridge is told there is something to
//! read, which only saves it a poll interval and is spawned, never awaited.

use std::sync::Arc;

use walgit_git::RepoId;

use crate::AppState;

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
/// convention rather than GitHub's forty zeros; the sink maps them to the
/// forty zeros the payload carries.
pub fn ref_written(
    st: &Arc<AppState>,
    repo: &RepoId,
    ref_name: &str,
    old: &str,
    new: &str,
    seq: u64,
) {
    tracing::debug!(
        repo = %repo,
        event = Kind::Push.as_str(),
        ref_name,
        old,
        new,
        seq,
        "github facade write committed"
    );
    if let Some(bridge) = &st.bridge {
        bridge.wake(repo);
    }
}
