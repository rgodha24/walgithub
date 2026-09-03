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
//! - [`repo`] — repository resolution, the repository/commit/branch/ref
//!   endpoints, and the git plumbing every other module shares.
//! - [`reads`] — trees, blobs, the README, archives, branch protection.
//! - [`contents`] — `contents/{path}` in its four representations.
//! - [`compare`] — three-dot compare.
//! - [`diff`] — `files[]` and its totals, rendered by `git diff-tree` on the
//!   bare repository. Compare and `pulls/{n}/files` are the same call.
//! - [`write`] — the write primitive (commit, ref create/update/delete), and
//!   the scratch object directory [`merge`] builds on too.
//! - [`prs`] — pull requests, reviews, comments and search.
//! - [`pr_store`] — PR state as JSON in the bucket, written with CAS; the one
//!   store, read and written by both the REST and the GraphQL surface.
//! - [`merge`] — real merges (`git merge-tree`) and `generate`.
//! - [`stubs`] — check runs, deployments and statuses (accept-and-forget).
//! - [`graphql`] — `POST /api/graphql`, dispatched on field names.
//! - [`error`] — `{message, documentation_url}` with GitHub's statuses.
//! - [`events`] — where a webhook would be produced (a WAL reader, later).

pub mod auth;
pub mod compare;
pub mod contents;
pub mod diff;
pub mod error;
pub mod events;
pub mod graphql;
pub mod merge;
pub mod models;
pub mod pr_store;
pub mod prs;
pub mod reads;
pub mod repo;
pub mod router;
pub mod stubs;
pub mod write;

pub use router::router;
