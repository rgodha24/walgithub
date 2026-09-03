//! Pull-request state in the bucket, and the node ids that address it.
//!
//! **This is a seam, not a home.** The REST pulls endpoints own this state;
//! until they land, the two GraphQL mutations that only flip `draft` and the
//! one that appends a review thread need somewhere to write, and it must be
//! the same place the REST side will read. So the layout, the JSON shape and
//! the node-id encoding are fixed here and documented in `docs/GITHUB.md`
//! (§GraphQL and §Pull requests); a `prs.rs` beside this module takes them
//! over unchanged and this file keeps only its callers' entry points.
//!
//! Layout, under the repository's own prefix in the bucket
//! (`RepoId::store_prefix()` = `repos/<owner>/<repo>/`):
//!
//! - `github/prs/<n>.json` — one [`PullRequest`], the unit of CAS.
//! - `github/prs/index.json` — `{next_number, numbers: [..]}`, so allocating a
//!   number is one CAS and listing does not need a LIST.
//!
//! Every mutation here is read → modify → `PutMode::Update(version)`, retried
//! a few times on a losing CAS: two writers to one PR are two editors, not a
//! conflict a client can do anything with.

use std::str::FromStr;
use std::sync::Arc;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use walgit_git::RepoId;
use walgit_store::{GetOptions, PutMode, PutOptions, Version};

use super::error::GqlError;
use crate::AppState;

/// How many times a losing CAS is re-read and re-applied before giving up.
const CAS_TRIES: u32 = 5;

/// One pull request. Only the fields the client reads
/// (`docs/GITHUB_SHAPES.md`, "GET /repos/{o}/{r}/pulls/{n}") plus what this
/// module needs to answer a mutation; everything else a REST handler can
/// derive from the repository at read time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub node_id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// `open` or `closed`.
    pub state: String,
    pub draft: bool,
    pub base: RefSide,
    pub head: RefSide,
    pub user: String,
    pub created_at: String,
    pub updated_at: String,
    pub merged: bool,
    pub merged_at: Option<String>,
    pub merge_commit_sha: Option<String>,
    pub html_url: String,
    /// Threads appended by `addPullRequestReviewThread`.
    #[serde(default)]
    pub review_threads: Vec<ReviewThread>,
    /// Anything a later REST handler adds and this one must not drop.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefSide {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewThread {
    pub id: String,
    /// The review this thread belongs to, when the client opened one first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_type: Option<String>,
    pub body: String,
    pub created_at: String,
}

/// What a node id addresses. GitHub's are opaque base64 and no client parses
/// them, so the facade's are base64 of a readable body and round-trip exactly:
///
/// - a pull request: `PR_<owner>/<repo>#<number>`
/// - a pending review: `PRR_<owner>/<repo>#<number>#<review id>`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeId {
    PullRequest {
        id: RepoId,
        number: u64,
    },
    Review {
        id: RepoId,
        number: u64,
        review: String,
    },
}

impl NodeId {
    pub fn pull_request(id: &RepoId, number: u64) -> String {
        encode(&format!("PR_{id}#{number}"))
    }

    pub fn review(id: &RepoId, number: u64, review: &str) -> String {
        encode(&format!("PRR_{id}#{number}#{review}"))
    }

    /// The repository and number a node id names, whichever kind it is.
    pub fn target(&self) -> (&RepoId, u64) {
        match self {
            NodeId::PullRequest { id, number } | NodeId::Review { id, number, .. } => (id, *number),
        }
    }

    pub fn parse(node_id: &str) -> Option<Self> {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(node_id)
            .ok()?;
        let body = String::from_utf8(decoded).ok()?;
        if let Some(rest) = body.strip_prefix("PRR_") {
            let mut parts = rest.splitn(3, '#');
            let id = RepoId::from_str(parts.next()?).ok()?;
            let number = parts.next()?.parse().ok()?;
            return Some(NodeId::Review {
                id,
                number,
                review: parts.next()?.to_string(),
            });
        }
        let rest = body.strip_prefix("PR_")?;
        let (repo, number) = rest.rsplit_once('#')?;
        Some(NodeId::PullRequest {
            id: RepoId::from_str(repo).ok()?,
            number: number.parse().ok()?,
        })
    }
}

fn encode(body: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(body)
}

fn key(id: &RepoId, number: u64) -> String {
    format!("{}github/prs/{number}.json", id.store_prefix())
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn missing(id: &RepoId, number: u64) -> GqlError {
    GqlError::not_found(format!(
        "Could not resolve to a PullRequest with the number {number} in {id}."
    ))
}

/// Read one pull request and the version it was read at (for the CAS).
async fn load(
    st: &Arc<AppState>,
    id: &RepoId,
    number: u64,
) -> Result<(PullRequest, Version), GqlError> {
    let got = st
        .store
        .get(&key(id, number), GetOptions::default())
        .await
        .map_err(|e| {
            if e.is_not_found() {
                missing(id, number)
            } else {
                GqlError::internal(format!("read pull request {number}: {e}"))
            }
        })?;
    let (meta, bytes) = got
        .bytes()
        .await
        .map_err(|e| GqlError::internal(format!("read pull request {number}: {e}")))?
        .ok_or_else(|| missing(id, number))?;
    let pr = serde_json::from_slice(&bytes)
        .map_err(|e| GqlError::internal(format!("pull request {number} is not valid JSON: {e}")))?;
    Ok((pr, meta.version))
}

async fn store(
    st: &Arc<AppState>,
    id: &RepoId,
    pr: &PullRequest,
    mode: PutMode,
) -> Result<(), walgit_store::StoreError> {
    let body = serde_json::to_vec(pr).map_err(walgit_store::StoreError::other)?;
    st.store
        .put(
            &key(id, pr.number),
            body.into(),
            PutOptions {
                mode,
                content_type: Some("application/json"),
                immutable: false,
            },
        )
        .await
        .map(|_| ())
}

/// Read, apply `edit`, write back under CAS; retried on a lost race.
async fn update<T>(
    st: &Arc<AppState>,
    id: &RepoId,
    number: u64,
    mut edit: impl FnMut(&mut PullRequest) -> Result<T, GqlError>,
) -> Result<T, GqlError> {
    for _ in 0..CAS_TRIES {
        let (mut pr, version) = load(st, id, number).await?;
        let out = edit(&mut pr)?;
        pr.updated_at = now();
        match store(st, id, &pr, PutMode::Update(version)).await {
            Ok(()) => return Ok(out),
            Err(e) if e.is_precondition_failed() => {}
            Err(e) => return Err(GqlError::internal(format!("write pull request: {e}"))),
        }
    }
    Err(GqlError::internal(format!(
        "pull request {number} kept moving under the write"
    )))
}

/// Write a pull request that is not there yet — the seam the tests and the
/// coming REST endpoints seed state through.
pub async fn create(st: &Arc<AppState>, id: &RepoId, pr: &PullRequest) -> Result<(), GqlError> {
    store(st, id, pr, PutMode::Overwrite)
        .await
        .map_err(|e| GqlError::internal(format!("write pull request: {e}")))
}

/// `markPullRequestReadyForReview` / `convertPullRequestToDraft`.
pub async fn set_draft(
    st: &Arc<AppState>,
    id: &RepoId,
    number: u64,
    draft: bool,
) -> Result<PullRequest, GqlError> {
    update(st, id, number, |pr| {
        pr.draft = draft;
        Ok(pr.clone())
    })
    .await
}

/// `addPullRequestReviewThread`, appending to the PR's `review_threads`.
pub async fn add_review_thread(
    st: &Arc<AppState>,
    id: &RepoId,
    number: u64,
    mut thread: ReviewThread,
) -> Result<ReviewThread, GqlError> {
    thread.created_at = now();
    update(st, id, number, move |pr| {
        let mut t = thread.clone();
        t.id = encode(&format!("PRRT_{id}#{number}#{}", pr.review_threads.len() + 1));
        pr.review_threads.push(t.clone());
        Ok(t)
    })
    .await
}

/// The pull request a node id names, without changing it.
pub async fn get(st: &Arc<AppState>, node_id: &str) -> Result<PullRequest, GqlError> {
    let parsed = NodeId::parse(node_id)
        .ok_or_else(|| GqlError::not_found(format!("Could not resolve to a node with id {node_id}.")))?;
    let (id, number) = parsed.target();
    load(st, id, number).await.map(|(pr, _)| pr)
}

#[cfg(test)]
mod tests {
    use super::NodeId;
    use walgit_git::RepoId;

    #[test]
    fn node_ids_round_trip() {
        let id = RepoId::new("acme", "docs").expect("repo id");
        let pr = NodeId::pull_request(&id, 412);
        assert_eq!(
            NodeId::parse(&pr),
            Some(NodeId::PullRequest {
                id: id.clone(),
                number: 412
            })
        );
        let review = NodeId::review(&id, 412, "7");
        assert_eq!(
            NodeId::parse(&review),
            Some(NodeId::Review {
                id,
                number: 412,
                review: "7".to_string()
            })
        );
        assert!(NodeId::parse("not base64 at all!!").is_none());
        assert!(NodeId::parse("dW5rbm93bg==").is_none());
    }
}
