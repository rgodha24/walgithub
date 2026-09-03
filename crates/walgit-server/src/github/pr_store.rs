//! Pull-request state, stored as JSON in the bucket under the repository's
//! own prefix — S3 is the only dependency the PR flow adds.
//!
//! Layout, relative to `repos/{owner}/{repo}/` (the prefix
//! [`walgit_wal::RepoHandle::store`] is already scoped to):
//!
//! - `github/prs/index.json` — `next_number` plus one summary row per PR.
//! - `github/prs/<n>.json` — the whole PR: body, comments, reviews, threads.
//!
//! Both are written with compare-and-swap ([`PutMode::Create`] for a new
//! object, [`PutMode::Update`] on the version last read); a
//! `PreconditionFailed` re-reads and retries a bounded number of times. No
//! `LIST` is ever issued on a request path — the index is the listing, which
//! is the whole reason it exists.
//!
//! The index is a cache of fields the PR JSON already holds. A row and its
//! object are written in two puts, so a crash between them leaves a row that
//! is one edit stale; every read that needs precision reads the object.

use std::str::FromStr;

use base64::Engine;
use serde::{Deserialize, Serialize};
use walgit_git::RepoId;
use walgit_store::{ObjectStoreExt, Prefixed, PutMode, Version};

use super::error::{GhError, GhResult};

/// Key of the PR index, relative to the repository's store prefix.
pub const INDEX_KEY: &str = "github/prs/index.json";

/// Key of one PR object, relative to the repository's store prefix.
pub fn pr_key(number: u64) -> String {
    format!("github/prs/{number}.json")
}

/// How many times a CAS loop re-reads before giving up. A facade serving one
/// developer never contends; a retry here is a bug report, not a hot path.
const CAS_TRIES: usize = 8;

/// An RFC 3339 timestamp at second resolution, which is what GitHub emits.
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// `<owner>/<repo>#<number>#<tail>`, the body of every id but a PR's.
fn three(rest: &str) -> Option<(RepoId, u64, String)> {
    let mut parts = rest.splitn(3, '#');
    let id = RepoId::from_str(parts.next()?).ok()?;
    let number = parts.next()?.parse().ok()?;
    Some((id, number, parts.next()?.to_string()))
}

fn b64(body: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(body)
}

/// GitHub's opaque PR node id: base64 of `PR_<owner>/<repo>#<n>`.
pub fn node_id(full_name: &str, number: u64) -> String {
    b64(&format!("PR_{full_name}#{number}"))
}

/// The node id of one review: base64 of `PRR_<owner>/<repo>#<n>#<review id>`.
///
/// This is what `POST /pulls/{n}/reviews` mints and what GraphQL's
/// `addPullRequestReviewThread` is handed back — the two surfaces address the
/// same review, so the encoding is one function.
pub fn review_node_id(full_name: &str, number: u64, review: u64) -> String {
    b64(&format!("PRR_{full_name}#{number}#{review}"))
}

/// The node id of one review thread: base64 of
/// `PRRT_<owner>/<repo>#<n>#<ordinal>`.
pub fn thread_node_id(full_name: &str, number: u64, ordinal: usize) -> String {
    b64(&format!("PRRT_{full_name}#{number}#{ordinal}"))
}

/// What a node id addresses. GitHub's are opaque base64 and no client parses
/// them, so the facade's are base64 of a readable body and round-trip exactly
/// (`docs/GITHUB.md` §node ids).
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
    ReviewThread {
        id: RepoId,
        number: u64,
        thread: String,
    },
}

impl NodeId {
    pub fn pull_request(id: &RepoId, number: u64) -> String {
        node_id(&id.to_string(), number)
    }

    pub fn review(id: &RepoId, number: u64, review: u64) -> String {
        review_node_id(&id.to_string(), number, review)
    }

    pub fn review_thread(id: &RepoId, number: u64, ordinal: usize) -> String {
        thread_node_id(&id.to_string(), number, ordinal)
    }

    /// The repository and pull-request number a node id names, whichever kind
    /// it is.
    pub fn target(&self) -> (&RepoId, u64) {
        match self {
            NodeId::PullRequest { id, number }
            | NodeId::Review { id, number, .. }
            | NodeId::ReviewThread { id, number, .. } => (id, *number),
        }
    }

    pub fn parse(node_id: &str) -> Option<Self> {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(node_id)
            .ok()?;
        let body = String::from_utf8(decoded).ok()?;
        if let Some(rest) = body.strip_prefix("PRRT_") {
            let (id, number, thread) = three(rest)?;
            return Some(NodeId::ReviewThread { id, number, thread });
        }
        if let Some(rest) = body.strip_prefix("PRR_") {
            let (id, number, review) = three(rest)?;
            return Some(NodeId::Review { id, number, review });
        }
        let rest = body.strip_prefix("PR_")?;
        let (repo, number) = rest.rsplit_once('#')?;
        Some(NodeId::PullRequest {
            id: RepoId::from_str(repo).ok()?,
            number: number.parse().ok()?,
        })
    }
}

/// One review thread, appended by GraphQL's `addPullRequestReviewThread`.
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

/// One end of a pull request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Side {
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// The tip as of the last write. For an open PR the head is re-resolved
    /// from the ref on every read; this is what it froze to at merge or close.
    pub sha: String,
}

/// An issue comment or a review comment — one shape, because every field a
/// review comment adds is optional and no client tells them apart by type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    pub body: String,
    pub user: String,
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub commit_id: Option<String>,
    #[serde(default)]
    pub reactions: Vec<Reaction>,
}

/// A reaction on a comment. Only `id` is ever read back (as `reaction_id`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reaction {
    pub id: u64,
    pub content: String,
    pub user: String,
    pub created_at: String,
}

/// A review. `state` is GitHub's vocabulary: `PENDING`, `APPROVED`,
/// `CHANGES_REQUESTED`, `COMMENTED`, `DISMISSED`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub id: u64,
    pub node_id: String,
    pub state: String,
    #[serde(default)]
    pub body: String,
    pub user: String,
    #[serde(default)]
    pub submitted_at: Option<String>,
    #[serde(default)]
    pub commit_id: String,
}

/// A pull request as it lives in the bucket.
///
/// The GraphQL arms read and write this same object (draft toggles, review
/// threads), so the field set is the contract between the two — add, never
/// rename, and give every addition a `#[serde(default)]` so an object written
/// by an older build still parses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub node_id: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// `open` or `closed`. A merged PR is also `closed`.
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    pub base: Side,
    pub head: Side,
    /// The author's login.
    pub user: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub closed_at: Option<String>,
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub merged_at: Option<String>,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub labels: Vec<serde_json::Value>,
    #[serde(default)]
    pub comments: Vec<Comment>,
    #[serde(default)]
    pub reviews: Vec<Review>,
    #[serde(default)]
    pub review_comments: Vec<Comment>,
    #[serde(default)]
    pub review_threads: Vec<ReviewThread>,
    #[serde(default = "yes")]
    pub maintainer_can_modify: bool,
    /// Monotonic id source for comments, reviews and reactions on this PR.
    #[serde(default)]
    pub next_id: u64,
}

fn yes() -> bool {
    true
}

impl PullRequest {
    /// The next comment/review/reaction id. GitHub's ids are global; these are
    /// per-PR and offset by the PR number so two PRs never collide, which is
    /// all a client keying a cache on an id needs.
    pub fn take_id(&mut self) -> u64 {
        if self.next_id == 0 {
            self.next_id = self.number.saturating_mul(1_000_000).saturating_add(1);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// GitHub's own precedence, which every client reimplements: merged wins
    /// over closed, closed wins over draft.
    pub fn status(&self) -> &'static str {
        if self.merged_at.is_some() {
            "merged"
        } else if self.state != "open" {
            "closed"
        } else if self.draft {
            "draft"
        } else {
            "open"
        }
    }

    pub fn is_open(&self) -> bool {
        self.state == "open"
    }
}

/// One row of the index: everything a listing filters or sorts on, so a list
/// of PRs costs one GET plus one GET per PR actually rendered.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Row {
    pub number: u64,
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged: bool,
    pub head_ref: String,
    pub base_ref: String,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    #[serde(default)]
    pub created_at: String,
    pub updated_at: String,
}

impl Row {
    pub fn of(pr: &PullRequest) -> Self {
        Self {
            number: pr.number,
            state: pr.state.clone(),
            draft: pr.draft,
            merged: pr.merged,
            head_ref: pr.head.ref_name.clone(),
            base_ref: pr.base.ref_name.clone(),
            head_sha: pr.head.sha.clone(),
            merge_commit_sha: pr.merge_commit_sha.clone(),
            created_at: pr.created_at.clone(),
            updated_at: pr.updated_at.clone(),
        }
    }
}

/// `github/prs/index.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub next_number: u64,
    #[serde(default)]
    pub prs: Vec<Row>,
}

impl Index {
    fn upsert(&mut self, pr: &PullRequest) {
        let row = Row::of(pr);
        match self.prs.iter_mut().find(|r| r.number == pr.number) {
            Some(existing) => *existing = row,
            None => self.prs.push(row),
        }
    }
}

fn store_err(e: &walgit_store::StoreError) -> GhError {
    GhError::Internal(format!("pr store: {e}"))
}

fn parse<T: serde::de::DeserializeOwned>(key: &str, bytes: &[u8]) -> GhResult<T> {
    serde_json::from_slice(bytes).map_err(|e| GhError::Internal(format!("{key} is corrupt: {e}")))
}

fn encode<T: Serialize>(value: &T) -> GhResult<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| GhError::Internal(format!("encode pr json: {e}")))
}

/// The index and the version it was read at; `None` when it does not exist yet.
pub async fn read_index(store: &Prefixed) -> GhResult<(Index, Option<Version>)> {
    match store.get_bytes(INDEX_KEY).await.map_err(|e| store_err(&e))? {
        Some((meta, bytes)) => Ok((parse(INDEX_KEY, &bytes)?, Some(meta.version))),
        None => Ok((Index::default(), None)),
    }
}

async fn put_index(store: &Prefixed, index: &Index, at: Option<Version>) -> GhResult<bool> {
    let mode = at.map_or(PutMode::Create, PutMode::Update);
    match store.put_bytes(INDEX_KEY, encode(index)?, mode).await {
        Ok(_) => Ok(true),
        Err(e) if e.is_precondition_failed() => Ok(false),
        Err(e) => Err(store_err(&e)),
    }
}

/// One PR and the version it was read at. 404 when there is no such PR.
pub async fn read(store: &Prefixed, number: u64) -> GhResult<(PullRequest, Version)> {
    let key = pr_key(number);
    let Some((meta, bytes)) = store.get_bytes(&key).await.map_err(|e| store_err(&e))? else {
        return Err(GhError::not_found(format!("pull request #{number}")));
    };
    Ok((parse(&key, &bytes)?, meta.version))
}

/// One PR, or `None` when the object is missing — for an index row that names
/// a PR whose object never landed.
pub async fn try_read(store: &Prefixed, number: u64) -> GhResult<Option<PullRequest>> {
    let key = pr_key(number);
    let Some((_, bytes)) = store.get_bytes(&key).await.map_err(|e| store_err(&e))? else {
        return Ok(None);
    };
    Ok(Some(parse(&key, &bytes)?))
}

/// Read-modify-write one PR under CAS, then refresh its index row.
///
/// `edit` runs on every attempt and must be a pure function of the PR it is
/// handed: a `PreconditionFailed` re-reads the object and calls it again.
pub async fn update<F>(store: &Prefixed, number: u64, mut edit: F) -> GhResult<PullRequest>
where
    F: FnMut(&mut PullRequest) -> GhResult<()>,
{
    for _ in 0..CAS_TRIES {
        let (mut pr, version) = read(store, number).await?;
        edit(&mut pr)?;
        pr.updated_at = now();
        let body = encode(&pr)?;
        match store
            .put_bytes(&pr_key(number), body, PutMode::Update(version))
            .await
        {
            Ok(_) => {
                refresh_row(store, &pr).await?;
                return Ok(pr);
            }
            Err(e) if e.is_precondition_failed() => {}
            Err(e) => return Err(store_err(&e)),
        }
    }
    Err(GhError::Conflict(format!(
        "pull request #{number} is being written concurrently"
    )))
}

/// Bring the index row for `pr` up to date. Cheap and idempotent; a failure to
/// CAS the index after the object landed is retried, then given up on — the
/// object is the truth and the next write repairs the row.
async fn refresh_row(store: &Prefixed, pr: &PullRequest) -> GhResult<()> {
    for _ in 0..CAS_TRIES {
        let (mut index, version) = read_index(store).await?;
        index.upsert(pr);
        if index.next_number <= pr.number {
            index.next_number = pr.number.saturating_add(1);
        }
        if put_index(store, &index, version).await? {
            return Ok(());
        }
    }
    Err(GhError::Internal(
        "pr index is being written concurrently".to_string(),
    ))
}

/// Allocate the next PR number and write the object.
///
/// `check` sees the index on every attempt and is where "a pull request
/// already exists for this head/base" lives — it must be re-run after a
/// contended CAS, because the PR it would refuse may have just been created.
pub async fn create<C, M>(store: &Prefixed, check: C, make: M) -> GhResult<PullRequest>
where
    C: Fn(&Index) -> GhResult<()>,
    M: Fn(u64) -> PullRequest,
{
    for _ in 0..CAS_TRIES {
        let (mut index, version) = read_index(store).await?;
        check(&index)?;
        let number = index.next_number.max(1);
        let pr = make(number);
        index.next_number = number.saturating_add(1);
        index.upsert(&pr);
        if !put_index(store, &index, version).await? {
            continue;
        }
        match store
            .put_bytes(&pr_key(number), encode(&pr)?, PutMode::Create)
            .await
        {
            Ok(_) => return Ok(pr),
            // The number was handed out by an index write we lost a race with;
            // the next attempt reads the bumped counter.
            Err(e) if e.is_precondition_failed() => {}
            Err(e) => return Err(store_err(&e)),
        }
    }
    Err(GhError::Conflict(
        "pull requests are being created concurrently".to_string(),
    ))
}

/// `markPullRequestReadyForReview` / `convertPullRequestToDraft`.
pub async fn set_draft(store: &Prefixed, number: u64, draft: bool) -> GhResult<PullRequest> {
    update(store, number, |pr| {
        pr.draft = draft;
        Ok(())
    })
    .await
}

/// `addPullRequestReviewThread`, appending to the PR's `review_threads`. The
/// thread's own node id is minted here, so it is one ordinal per PR.
pub async fn add_review_thread(
    store: &Prefixed,
    full_name: &str,
    number: u64,
    thread: ReviewThread,
) -> GhResult<ReviewThread> {
    let mut stored = None;
    update(store, number, |pr| {
        let mut t = thread.clone();
        t.created_at = now();
        t.id = thread_node_id(full_name, number, pr.review_threads.len().saturating_add(1));
        pr.review_threads.push(t.clone());
        stored = Some(t);
        Ok(())
    })
    .await?;
    stored.ok_or_else(|| GhError::Internal("review thread vanished".to_string()))
}

#[cfg(test)]
mod tests {
    use super::{Index, NodeId, PullRequest, Row, Side, node_id};
    use walgit_git::RepoId;

    fn pr() -> PullRequest {
        PullRequest {
            number: 3,
            node_id: node_id("acme/docs", 3),
            title: "t".into(),
            body: String::new(),
            state: "open".into(),
            draft: false,
            base: Side {
                ref_name: "main".into(),
                sha: "b".into(),
            },
            head: Side {
                ref_name: "topic".into(),
                sha: "h".into(),
            },
            user: "dev".into(),
            created_at: "t0".into(),
            updated_at: "t0".into(),
            closed_at: None,
            merged: false,
            merged_at: None,
            merge_commit_sha: None,
            html_url: "u".into(),
            labels: Vec::new(),
            comments: Vec::new(),
            reviews: Vec::new(),
            review_comments: Vec::new(),
            review_threads: Vec::new(),
            maintainer_can_modify: true,
            next_id: 0,
        }
    }

    #[test]
    fn node_ids_decode_to_the_documented_string() {
        use base64::Engine;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(node_id("acme/docs", 412))
            .expect("base64");
        assert_eq!(String::from_utf8_lossy(&raw), "PR_acme/docs#412");
    }

    #[test]
    fn every_node_id_form_round_trips() {
        let id = RepoId::new("acme", "docs").expect("repo id");
        assert_eq!(
            NodeId::parse(&NodeId::pull_request(&id, 412)),
            Some(NodeId::PullRequest {
                id: id.clone(),
                number: 412
            })
        );
        assert_eq!(
            NodeId::parse(&NodeId::review(&id, 412, 412_000_001)),
            Some(NodeId::Review {
                id: id.clone(),
                number: 412,
                review: "412000001".to_string()
            })
        );
        assert_eq!(
            NodeId::parse(&NodeId::review_thread(&id, 412, 1)),
            Some(NodeId::ReviewThread {
                id,
                number: 412,
                thread: "1".to_string()
            })
        );
        assert!(NodeId::parse("not base64 at all!!").is_none());
        assert!(NodeId::parse("dW5rbm93bg==").is_none());
    }

    #[test]
    fn ids_are_monotonic_and_scoped_to_the_pr() {
        let mut a = pr();
        let first = a.take_id();
        assert_eq!(a.take_id(), first + 1);
        let mut b = pr();
        b.number = 4;
        assert_ne!(b.take_id(), first);
    }

    #[test]
    fn status_precedence_matches_github() {
        let mut p = pr();
        assert_eq!(p.status(), "open");
        p.draft = true;
        assert_eq!(p.status(), "draft");
        p.state = "closed".into();
        assert_eq!(p.status(), "closed");
        p.merged_at = Some("t".into());
        assert_eq!(p.status(), "merged");
    }

    #[test]
    fn upsert_replaces_a_row_rather_than_appending() {
        let mut index = Index::default();
        let mut p = pr();
        index.upsert(&p);
        p.state = "closed".into();
        index.upsert(&p);
        assert_eq!(index.prs.len(), 1);
        assert_eq!(index.prs.first().map(|r| r.state.as_str()), Some("closed"));
    }

    #[test]
    fn a_pr_written_without_the_newer_fields_still_parses() {
        let json = serde_json::json!({
            "number": 1, "node_id": "n", "title": "t", "state": "open",
            "base": {"ref": "main", "sha": "b"}, "head": {"ref": "topic", "sha": "h"},
            "user": "dev", "created_at": "t0", "updated_at": "t0", "html_url": "u",
        });
        let parsed: PullRequest = serde_json::from_value(json).expect("parse");
        assert!(parsed.maintainer_can_modify);
        assert!(parsed.review_threads.is_empty());
        assert_eq!(Row::of(&parsed).head_ref, "topic");
    }
}
