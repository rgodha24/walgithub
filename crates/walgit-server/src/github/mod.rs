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
//! - [`events`] — where a webhook would be produced (a WAL reader, later).

pub mod auth;
pub mod error;
pub mod events;
pub mod models;
pub mod repo;
pub mod router;
pub mod write;

pub use router::router;
