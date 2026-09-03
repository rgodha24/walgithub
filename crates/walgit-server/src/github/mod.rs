//! A fake GitHub Enterprise Server over walgit's repositories (`docs/GITHUB.md`).
//!
//! An octokit constructed with `baseUrl = http://<host>/api/v3` talks to this
//! and reads real refs, commits and objects out of the bucket; a write goes
//! through walgit's publish path, so another instance sees it on its next
//! revalidation. It exists so a client that already supports GitHub Enterprise
//! Server needs no code to develop against walgit.
//!
//! **Local development only, and its own trust boundary.** Every route here
//! bypasses `server.auth`: any bearer is accepted, there is one hardcoded user
//! and every permission answer is `admin`. `Config::validate` therefore
//! refuses `github.enabled` unless `server.auth.mode = "none"` (already
//! loopback-only) or `server.listen` is loopback.
//!
//! Module map:
//! - [`router`] — the mount (`/api/v3/*`, `/api/graphql`, `/login/oauth/*`).
//! - [`auth`] — the identity stubs and the OAuth web flow.
//! - [`models`] — GitHub JSON shapes.
//! - [`repo`] — repository resolution and the read endpoints.
//! - [`write`] — the write primitive (commit, ref create/update/delete).
//! - [`error`] — `{message, documentation_url}` with GitHub's statuses.
//! - [`prs`] — pull requests, reviews, comments and search.
//! - [`pr_store`] — PR state as JSON in the bucket, written with CAS.
//! - [`merge`] — real merges (`git merge-tree`) and the PR diff plumbing.
//! - [`stubs`] — check runs, deployments and statuses (accept-and-forget).
//! - [`events`] — where a webhook would be produced (a WAL reader, later).
//! - [`reads`] — trees, blobs, the README, archives, branch protection, and
//!   the git plumbing the other read modules share.
//! - [`contents`] — `contents/{path}` in its four representations.
//! - [`compare`] — three-dot compare.
//! - [`diff`] — `files[]`, rendered by `git diff-tree` on the bare repository.
//! - [`graphql`] — `POST /api/graphql`, dispatched on field names.

pub mod auth;
pub mod error;
pub mod events;
pub mod merge;
pub mod models;
pub mod pr_store;
pub mod prs;
pub mod repo;
pub mod router;
pub mod stubs;
pub mod write;
pub mod reads;
pub mod contents;
pub mod compare;
pub mod diff;
pub mod graphql;

pub use router::router;
