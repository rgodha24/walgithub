//! Real merges, and the diff plumbing the PR endpoints need.
//!
//! The serving copy is bare and must stay untouched, so every object this
//! module creates is built in a scratch object directory that reads the
//! repository through `GIT_ALTERNATE_OBJECT_DIRECTORIES` — the same shape
//! [`super::write`] uses, and for the same reason: a merge that is refused
//! afterwards leaves nothing behind.
//!
//! The merge itself is `git merge-tree --write-tree`, which needs no worktree
//! (git 2.38+). A conflict is a non-zero exit and is answered with GitHub's
//! **405**; a clean merge yields a tree that is committed with `commit-tree`
//! and published exactly the way `write.rs` publishes: `pack-objects --revs`
//! → [`walgit_git::LocalRepo::ingest_pack`] → connectivity →
//! [`walgit_wal::RepoHandle::publish_push_synced`].
//!
//! The diff helpers here (`changed_files`, `stats`, `commit_count`) are the
//! minimum the PR endpoints need. They overlap with the compare/diff work in
//! `github/diff.rs`; when the two land together, keep one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tokio::process::Command;
use walgit_git::{LocalRepo, RepoId};
use walgit_proto::v1::{RefTransaction, RefUpdate};

use super::error::{GhError, GhResult};
use crate::AppState;

/// GitHub answers an unmergeable PR with 405, not 409 — the Mintlify server
/// branches on exactly that status (`docs/GITHUB_SHAPES.md`, Tier 3).
pub const NOT_MERGEABLE: &str = "Pull Request is not mergeable";
/// The 409 a client retries after re-reading the head.
pub const HEAD_MODIFIED: &str = "Head branch was modified. Review and try the merge again.";

/// A GitHub-shaped error body under a status [`GhError`] cannot express.
pub fn status_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({
            "message": message,
            "documentation_url": "https://docs.github.com/rest",
        })),
    )
        .into_response()
}

/// How a pull request is merged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Merge,
    Squash,
    Rebase,
}

impl Method {
    pub fn parse(s: &str) -> GhResult<Self> {
        match s {
            "merge" | "" => Ok(Method::Merge),
            "squash" => Ok(Method::Squash),
            "rebase" => Ok(Method::Rebase),
            other => Err(GhError::validation(
                "Validation Failed",
                super::error::FieldError::invalid(
                    "PullRequest",
                    "merge_method",
                    format!("{other} is not a merge method"),
                ),
            )),
        }
    }
}

/// What a merge did to the base ref.
#[derive(Debug, Clone)]
pub enum Outcome {
    /// The base ref now points at this commit.
    Merged(String),
    /// `head` was already contained in `base`; GitHub answers 204.
    UpToDate,
    /// The trees conflict; GitHub answers 405.
    Conflict,
}

// ---- read-only plumbing against the serving copy -----------------------------

async fn git(local: &LocalRepo, args: &[&str]) -> GhResult<std::process::Output> {
    local
        .git(args)
        .await
        .map_err(|e| GhError::Internal(format!("git {}: {e}", args.join(" "))))
}

async fn git_text(local: &LocalRepo, args: &[&str]) -> GhResult<String> {
    let out = git(local, args).await?;
    if !out.status.success() {
        return Err(GhError::Internal(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git merge-base a b`. `None` when the histories are unrelated.
pub async fn merge_base(local: &LocalRepo, a: &str, b: &str) -> GhResult<Option<String>> {
    let out = git(local, &["merge-base", a, b]).await?;
    if !out.status.success() {
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((!sha.is_empty()).then_some(sha))
}

/// `git rev-list --count base..head` — GitHub's "commits between".
pub async fn commit_count(local: &LocalRepo, base: &str, head: &str) -> GhResult<u64> {
    let range = format!("{base}..{head}");
    let out = git_text(local, &["rev-list", "--count", &range]).await?;
    Ok(out.parse().unwrap_or(0))
}

/// The shas of `base..head`, oldest first.
pub async fn commits_between(local: &LocalRepo, base: &str, head: &str) -> GhResult<Vec<String>> {
    let range = format!("{base}..{head}");
    let out = git_text(local, &["rev-list", "--reverse", &range]).await?;
    Ok(out.lines().map(str::to_string).collect())
}

/// One entry of `GET /pulls/{n}/files`.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub sha: String,
    pub filename: String,
    pub status: &'static str,
    pub additions: u64,
    pub deletions: u64,
    pub patch: Option<String>,
    pub previous_filename: Option<String>,
}

/// Totals for a PR's `additions` / `deletions` / `changed_files`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
}

/// Every file that differs between two trees, with rename detection, counts
/// and unified patches — GitHub's per-file shape minus the URLs, which the
/// caller builds because only it knows the request's origin.
pub async fn changed_files(local: &LocalRepo, base: &str, head: &str) -> GhResult<Vec<FileChange>> {
    let raw = git(
        local,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--raw",
            "--no-abbrev",
            "-z",
            "-M",
            "--no-color",
            base,
            head,
        ],
    )
    .await?;
    if !raw.status.success() {
        return Err(GhError::not_found(format!("{base}...{head}")));
    }
    let counts = numstat(local, base, head).await?;
    let patches = patches(local, base, head).await?;
    let mut out = Vec::new();
    for entry in parse_raw(&String::from_utf8_lossy(&raw.stdout)) {
        let (additions, deletions) = counts.get(&entry.path).copied().unwrap_or((0, 0));
        out.push(FileChange {
            sha: entry.sha,
            status: entry.status,
            additions,
            deletions,
            patch: patches.get(&entry.path).cloned(),
            previous_filename: entry.previous,
            filename: entry.path,
        });
    }
    Ok(out)
}

/// Totals only — no patches, so a PR read does not pay for a full diff body.
pub async fn stats(local: &LocalRepo, base: &str, head: &str) -> GhResult<Stats> {
    let counts = numstat(local, base, head).await?;
    let mut s = Stats {
        changed_files: counts.len() as u64,
        ..Stats::default()
    };
    for (add, del) in counts.values() {
        s.additions = s.additions.saturating_add(*add);
        s.deletions = s.deletions.saturating_add(*del);
    }
    Ok(s)
}

struct RawEntry {
    path: String,
    previous: Option<String>,
    sha: String,
    status: &'static str,
}

/// `git diff --raw -z` records: `:<m> <m> <src> <dst> <status>\0<path>\0`,
/// with a second path field for a rename or a copy.
fn parse_raw(text: &str) -> Vec<RawEntry> {
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    let mut out = Vec::new();
    while let Some(meta) = fields.next() {
        let Some(meta) = meta.strip_prefix(':') else {
            continue;
        };
        let cols: Vec<&str> = meta.split_whitespace().collect();
        let (Some(dst), Some(code)) = (cols.get(3), cols.get(4)) else {
            continue;
        };
        let letter = code.chars().next().unwrap_or('M');
        let renamed = matches!(letter, 'R' | 'C');
        let Some(first) = fields.next() else { break };
        let (path, previous) = if renamed {
            let Some(second) = fields.next() else { break };
            (second.to_string(), Some(first.to_string()))
        } else {
            (first.to_string(), None)
        };
        out.push(RawEntry {
            path,
            previous,
            sha: (*dst).to_string(),
            status: status_word(letter),
        });
    }
    out
}

fn status_word(letter: char) -> &'static str {
    match letter {
        'A' => "added",
        'D' => "removed",
        'R' => "renamed",
        'C' => "copied",
        'T' => "changed",
        _ => "modified",
    }
}

/// `git diff --numstat -z` keyed by the file's name after the change.
async fn numstat(
    local: &LocalRepo,
    base: &str,
    head: &str,
) -> GhResult<HashMap<String, (u64, u64)>> {
    let out = git(
        local,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--numstat",
            "-z",
            "-M",
            "--no-color",
            base,
            head,
        ],
    )
    .await?;
    if !out.status.success() {
        return Err(GhError::not_found(format!("{base}...{head}")));
    }
    Ok(parse_numstat(&String::from_utf8_lossy(&out.stdout)))
}

/// A `-z` numstat record is `adds\tdels\t<path>\0`, and for a rename
/// `adds\tdels\t\0<old>\0<new>\0` — the path moves out into its own fields.
fn parse_numstat(text: &str) -> HashMap<String, (u64, u64)> {
    let mut fields = text.split('\0').filter(|f| !f.is_empty()).peekable();
    let mut out = HashMap::new();
    while let Some(record) = fields.next() {
        let mut cols = record.splitn(3, '\t');
        let adds = cols.next().unwrap_or("0").trim().parse().unwrap_or(0);
        let dels = cols.next().unwrap_or("0").trim().parse().unwrap_or(0);
        let inline = cols.next().unwrap_or("");
        let path = if inline.is_empty() {
            let _old = fields.next();
            match fields.next() {
                Some(new) => new.to_string(),
                None => break,
            }
        } else {
            inline.to_string()
        };
        out.insert(path, (adds, dels));
    }
    out
}

/// One `git diff -p` pass, split into GitHub's per-file `patch` (the hunks,
/// without the `diff --git` header). Binary files have none.
async fn patches(local: &LocalRepo, base: &str, head: &str) -> GhResult<HashMap<String, String>> {
    let out = git(
        local,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "-p",
            "-M",
            "--no-color",
            "--no-ext-diff",
            base,
            head,
        ],
    )
    .await?;
    if !out.status.success() {
        return Ok(HashMap::new());
    }
    Ok(split_patches(&String::from_utf8_lossy(&out.stdout)))
}

fn split_patches(text: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    let mut path: Option<String> = None;
    let mut hunks: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            if let Some(p) = path.take()
                && !hunks.is_empty()
            {
                out.insert(p, hunks.join("\n"));
            }
            hunks.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ b/") {
            path = Some(rest.to_string());
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("--- a/") {
            // Only a deletion leaves `+++ /dev/null`, and then the old path is
            // the file's name; a later `+++ b/` overwrites this.
            if path.is_none() {
                path = Some(rest.to_string());
            }
            continue;
        }
        if line.starts_with("@@") || !hunks.is_empty() {
            hunks.push(line);
        }
    }
    if let Some(p) = path.take()
        && !hunks.is_empty()
    {
        out.insert(p, hunks.join("\n"));
    }
    out
}

/// One commit's facts, straight out of the object database — enough to render
/// GitHub's commit shape with [`super::models::commit`].
pub async fn commit_facts(local: &LocalRepo, sha: &str) -> GhResult<super::models::CommitFacts> {
    const FORMAT: &str = "--format=%H%x00%T%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B";
    let out = git(local, &["show", "-s", "--diff-merges=off", FORMAT, sha]).await?;
    if !out.status.success() {
        return Err(GhError::not_found(sha));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut f = text.split('\0');
    let mut next = || f.next().unwrap_or("").to_string();
    let facts = super::models::CommitFacts {
        sha: next().trim().to_string(),
        tree: next(),
        parents: next().split_whitespace().map(str::to_string).collect(),
        author_name: next(),
        author_email: next(),
        author_date: next(),
        committer_name: next(),
        committer_email: next(),
        committer_date: next(),
        message: next().trim_end_matches('\n').to_string(),
    };
    if facts.sha.is_empty() {
        return Err(GhError::not_found(sha));
    }
    Ok(facts)
}

// ---- the merge itself --------------------------------------------------------

/// Merge `head` into the ref `base_ref` and publish the result.
///
/// `expected_base` is the sha the caller believes the base ref has; the WAL
/// checks it again at CAS time, so a base that moves between here and the
/// publish is a conflict rather than a lost update.
pub async fn merge_into_ref(
    st: &Arc<AppState>,
    id: &RepoId,
    base_ref: &str,
    head: &str,
    method: Method,
    title: &str,
    body: &str,
) -> GhResult<Outcome> {
    let handle = super::repo::open(st, id).await?;
    let guard = handle.sync().await?;
    let local = handle.local().clone();

    let Some(base) = super::repo::ref_oid(&local, base_ref)? else {
        drop(guard);
        return Err(GhError::not_found(base_ref));
    };
    if local
        .is_ancestor(head, &base)
        .await
        .map_err(|e| GhError::Internal(format!("merge-base: {e}")))?
    {
        drop(guard);
        return Ok(Outcome::UpToDate);
    }

    let scratch = Scratch::new(local.path()).await?;
    let author = Author::facade();
    let built = match method {
        Method::Merge => {
            let Some(tree) = scratch.merge_tree(&base, head, None).await? else {
                drop(guard);
                return Ok(Outcome::Conflict);
            };
            scratch
                .commit_tree(&tree, &[&base, head], &message(title, body), &author)
                .await?
        }
        Method::Squash => {
            let Some(tree) = scratch.merge_tree(&base, head, None).await? else {
                drop(guard);
                return Ok(Outcome::Conflict);
            };
            scratch
                .commit_tree(&tree, &[&base], &message(title, body), &author)
                .await?
        }
        Method::Rebase => {
            let Some(sha) = scratch.rebase(&local, &base, head).await? else {
                drop(guard);
                return Ok(Outcome::Conflict);
            };
            sha
        }
    };

    let pack = scratch.pack(&built, &[&base, head]).await?;
    let ingested = local
        .ingest_pack(
            std::io::Cursor::new(pack),
            walgit_git::IngestOptions {
                fsck: st.cfg.wal.fsck_objects,
                max_bytes: None,
                thin: false,
            },
        )
        .await
        .map_err(|e| GhError::Internal(format!("index the merge: {e}")))?;
    let tip = gix_hash::ObjectId::from_hex(built.as_bytes())
        .map_err(|e| GhError::Internal(format!("commit-tree returned {built:?}: {e}")))?;
    local
        .check_connectivity_async(&[tip], true)
        .await
        .map_err(|e| GhError::Internal(format!("connectivity: {e}")))?;
    drop(guard);

    publish(&handle, &[(base_ref, base.as_str(), built.as_str())], ingested).await?;
    Ok(Outcome::Merged(built))
}

fn message(title: &str, body: &str) -> String {
    if body.trim().is_empty() {
        return title.to_string();
    }
    format!("{title}\n\n{body}")
}

/// Publish one or more ref updates with an optional pack, through the same
/// path `write.rs` uses: pack PUT ∥ log PUT → manifest CAS.
pub async fn publish(
    handle: &Arc<walgit_wal::RepoHandle>,
    updates: &[(&str, &str, &str)],
    pack: Option<walgit_git::IngestedPack>,
) -> GhResult<u64> {
    let mut txn = RefTransaction {
        updates: updates
            .iter()
            .map(|(name, old, new)| RefUpdate {
                name: (*name).to_string(),
                old_oid: (*old).to_string(),
                new_oid: (*new).to_string(),
                ..Default::default()
            })
            .collect(),
        atomic: true,
        ..Default::default()
    };
    handle.local().fill_peeled(&mut txn);
    let meta = HashMap::from([
        ("principal".to_string(), super::auth::USER_LOGIN.to_string()),
        ("agent".to_string(), "walgit github facade".to_string()),
    ]);
    let res = handle
        .publish_push_synced(pack, txn, meta)
        .await
        .map_err(|e| GhError::Internal(format!("publish: {e}")))?;
    for (name, outcome) in res.per_ref {
        if let Err(e) = outcome {
            return Err(match e {
                walgit_wal::RefError::Conflict { .. } => GhError::Conflict(HEAD_MODIFIED.into()),
                other => GhError::Conflict(format!("{name}: {other}")),
            });
        }
    }
    for (name, old, new) in updates {
        super::events::ref_written(handle.id(), name, old, new, res.seq);
    }
    Ok(res.seq)
}

/// Publish a symbolic ref (`HEAD` → a branch), which a freshly generated
/// repository needs when its default branch is not the one `init` chose.
pub async fn publish_head(handle: &Arc<walgit_wal::RepoHandle>, target: &str) -> GhResult<()> {
    let txn = RefTransaction {
        updates: vec![RefUpdate {
            name: "HEAD".to_string(),
            new_symbolic_target: target.to_string(),
            ..Default::default()
        }],
        atomic: true,
        ..Default::default()
    };
    handle
        .publish_push_synced(None, txn, HashMap::new())
        .await
        .map_err(|e| GhError::Internal(format!("publish HEAD: {e}")))?;
    Ok(())
}

/// A commit author. Everything the facade writes is the facade's user.
pub struct Author {
    pub name: String,
    pub email: String,
}

impl Author {
    pub fn facade() -> Self {
        Self {
            name: super::auth::USER_LOGIN.to_string(),
            email: format!("{}@walgit.localhost", super::auth::USER_LOGIN),
        }
    }
}

/// A scratch object directory: writes land here, reads fall through to the
/// serving copy's `objects/`. Mirrors `write.rs`'s `Scratch`.
pub struct Scratch {
    dir: tempfile::TempDir,
    git_dir: PathBuf,
}

impl Scratch {
    pub async fn new(git_dir: &Path) -> GhResult<Self> {
        let git_dir = git_dir.to_path_buf();
        let dir = tokio::task::spawn_blocking(|| -> std::io::Result<tempfile::TempDir> {
            let dir = tempfile::Builder::new().prefix("walgit-gh-merge-").tempdir()?;
            std::fs::create_dir_all(dir.path().join("objects"))?;
            std::fs::create_dir_all(dir.path().join("worktree"))?;
            Ok(dir)
        })
        .await
        .map_err(|e| GhError::Internal(format!("scratch task: {e}")))?
        .map_err(|e| GhError::Internal(format!("scratch dir: {e}")))?;
        Ok(Self { dir, git_dir })
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.current_dir(self.dir.path())
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_OBJECT_DIRECTORY", self.dir.path().join("objects"))
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                self.git_dir.join("objects"),
            )
            .env("GIT_INDEX_FILE", self.dir.path().join("index"))
            .env("GIT_WORK_TREE", self.dir.path().join("worktree"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        cmd
    }

    async fn run(&self, mut cmd: Command, stdin: &[u8]) -> GhResult<std::process::Output> {
        use tokio::io::AsyncWriteExt;
        let mut child = cmd
            .spawn()
            .map_err(|e| GhError::Internal(format!("spawn git: {e}")))?;
        if let Some(mut pipe) = child.stdin.take() {
            pipe.write_all(stdin)
                .await
                .map_err(|e| GhError::Internal(format!("git stdin: {e}")))?;
            pipe.shutdown()
                .await
                .map_err(|e| GhError::Internal(format!("git stdin: {e}")))?;
        }
        child
            .wait_with_output()
            .await
            .map_err(|e| GhError::Internal(format!("git: {e}")))
    }

    async fn text(&self, cmd: Command, stdin: &[u8]) -> GhResult<String> {
        let out = self.run(cmd, stdin).await?;
        if !out.status.success() {
            return Err(GhError::Internal(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// `git merge-tree --write-tree`. `Ok(None)` is a conflict — the trees do
    /// not merge, which is GitHub's 405. `merge_base` overrides the common
    /// ancestor, which is how a rebase replays one commit at a time.
    pub async fn merge_tree(
        &self,
        ours: &str,
        theirs: &str,
        merge_base: Option<&str>,
    ) -> GhResult<Option<String>> {
        let mut c = self.command();
        c.args(["merge-tree", "--write-tree", "--messages"]);
        if let Some(base) = merge_base {
            c.arg(format!("--merge-base={base}"));
        }
        c.args([ours, theirs]);
        let out = self.run(c, &[]).await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let Some(tree) = stdout.lines().next().map(str::trim) else {
            return Err(GhError::Internal(format!(
                "merge-tree said nothing: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        };
        match out.status.code() {
            Some(0) => Ok(Some(tree.to_string())),
            Some(1) => Ok(None),
            _ => Err(GhError::Internal(format!(
                "merge-tree: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    /// `git commit-tree` with any number of parents.
    pub async fn commit_tree(
        &self,
        tree: &str,
        parents: &[&str],
        message: &str,
        author: &Author,
    ) -> GhResult<String> {
        let mut c = self.command();
        c.args(["commit-tree", tree]);
        for p in parents {
            c.args(["-p", p]);
        }
        c.args(["-m", message]);
        c.env("GIT_AUTHOR_NAME", &author.name)
            .env("GIT_AUTHOR_EMAIL", &author.email)
            .env("GIT_COMMITTER_NAME", &author.name)
            .env("GIT_COMMITTER_EMAIL", &author.email);
        self.text(c, &[]).await
    }

    /// Replay `base..head` onto `base`, one commit at a time: each step is the
    /// three-way merge of that commit against the growing tip with the
    /// commit's own parent as the base, which is what `git rebase` does.
    /// `Ok(None)` on the first conflict.
    pub async fn rebase(
        &self,
        local: &LocalRepo,
        base: &str,
        head: &str,
    ) -> GhResult<Option<String>> {
        let commits = commits_between(local, base, head).await?;
        let mut onto = base.to_string();
        for sha in &commits {
            let parent = git_text(local, &["rev-parse", "--verify", &format!("{sha}^")])
                .await
                .unwrap_or_else(|_| String::new());
            let merge_base = (!parent.is_empty()).then_some(parent.as_str());
            let Some(tree) = self.merge_tree(&onto, sha, merge_base).await? else {
                return Ok(None);
            };
            let subject = git_text(local, &["log", "-1", "--format=%B", sha]).await?;
            let author = Author {
                name: git_text(local, &["log", "-1", "--format=%an", sha]).await?,
                email: git_text(local, &["log", "-1", "--format=%ae", sha]).await?,
            };
            onto = self.commit_tree(&tree, &[&onto], &subject, &author).await?;
        }
        Ok(Some(onto))
    }

    /// A self-contained pack of everything `commit` adds over `haves`.
    pub async fn pack(&self, commit: &str, haves: &[&str]) -> GhResult<Vec<u8>> {
        let mut revs = format!("{commit}\n");
        for have in haves {
            revs.push('^');
            revs.push_str(have);
            revs.push('\n');
        }
        let mut c = self.command();
        c.args([
            "pack-objects",
            "--revs",
            "--stdout",
            "--delta-base-offset",
            "-q",
        ]);
        let out = self.run(c, revs.as_bytes()).await?;
        if !out.status.success() {
            return Err(GhError::Internal(format!(
                "pack-objects: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    }

    /// Pack a whole history, for seeding a repository from a template.
    pub async fn pack_all(&self, commit: &str) -> GhResult<Vec<u8>> {
        self.pack(commit, &[]).await
    }
}

// ---- template generation -----------------------------------------------------

#[derive(serde::Deserialize)]
pub struct GenerateRequest {
    #[serde(default)]
    pub owner: Option<String>,
    pub name: String,
    #[serde(default)]
    pub private: bool,
    /// Accepted and ignored: only the template's default branch is copied.
    #[serde(default)]
    pub include_all_branches: bool,
}

/// `POST /repos/{template_owner}/{template_repo}/generate`.
///
/// A new repository in the same bucket, seeded with the template's default
/// branch. The repository is created the way the first push creates one
/// (`Registry::open_or_create`) and the history is published as one pack
/// through the ordinary WAL path — nothing loops back through smart HTTP.
pub async fn generate(
    axum::extract::State(st): axum::extract::State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path((template_owner, template_repo)): axum::extract::Path<(String, String)>,
    axum::Json(req): axum::Json<GenerateRequest>,
) -> GhResult<Response> {
    let _ = req.include_all_branches;
    let template_id = super::repo::repo_id(&template_owner, &template_repo)?;
    let template = super::repo::objects_view(&st, &template_id).await?;
    let branch = template
        .index
        .head_target
        .strip_prefix("refs/heads/")
        .unwrap_or("main")
        .to_string();
    let tip = template
        .index
        .branch(&branch)
        .ok_or_else(|| GhError::not_found(format!("{template_id} has no {branch}")))?
        .to_string();

    let owner = req.owner.clone().unwrap_or(template_owner);
    let target = super::repo::repo_id(&owner, &req.name)?;
    if st.registry.open(&target).await.is_ok() {
        return Err(GhError::validation(
            "Validation Failed",
            super::error::FieldError::invalid(
                "Repository",
                "name",
                "name already exists on this account",
            ),
        ));
    }

    let scratch = Scratch::new(template.local.path()).await?;
    let pack = scratch.pack_all(&tip).await?;

    let format = walgit_git::ObjectFormat::from(st.cfg.git.object_format);
    let handle = st
        .registry
        .open_or_create(&target, format)
        .await
        .map_err(GhError::from)?;
    let guard = handle.sync().await?;
    let local = handle.local().clone();
    let ingested = local
        .ingest_pack(
            std::io::Cursor::new(pack),
            walgit_git::IngestOptions {
                fsck: st.cfg.wal.fsck_objects,
                max_bytes: None,
                thin: false,
            },
        )
        .await
        .map_err(|e| GhError::Internal(format!("seed the new repository: {e}")))?;
    let oid = gix_hash::ObjectId::from_hex(tip.as_bytes())
        .map_err(|e| GhError::Internal(format!("{tip} is not an object id: {e}")))?;
    local
        .check_connectivity_async(&[oid], true)
        .await
        .map_err(|e| GhError::Internal(format!("connectivity: {e}")))?;
    drop(guard);

    let full_ref = format!("refs/heads/{branch}");
    publish(&handle, &[(&full_ref, "", tip.as_str())], ingested).await?;
    publish_head(&handle, &full_ref).await?;

    let urls = super::models::Urls::from_request(&st, &headers);
    let mut body = serde_json::to_value(super::models::repository(
        &urls,
        target.owner(),
        target.name(),
        &branch,
        super::models::EPOCH,
    ))
    .unwrap_or(serde_json::Value::Null);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("private".into(), serde_json::json!(req.private));
        obj.insert("is_template".into(), serde_json::json!(false));
    }
    Ok((StatusCode::CREATED, axum::Json(body)).into_response())
}

#[cfg(test)]
mod tests {
    use super::{parse_numstat, parse_raw, split_patches, status_word};

    #[test]
    fn raw_records_carry_the_status_and_the_rename_pair() {
        let text = ":100644 100644 aaa bbb M\0docs/a.mdx\0:100644 100644 ccc ddd R100\0old.mdx\0new.mdx\0";
        let entries = parse_raw(text);
        assert_eq!(entries.len(), 2);
        let first = entries.first().expect("first");
        assert_eq!(first.path, "docs/a.mdx");
        assert_eq!(first.status, "modified");
        assert_eq!(first.sha, "bbb");
        assert!(first.previous.is_none());
        let second = entries.get(1).expect("second");
        assert_eq!(second.path, "new.mdx");
        assert_eq!(second.status, "renamed");
        assert_eq!(second.previous.as_deref(), Some("old.mdx"));
    }

    #[test]
    fn numstat_handles_the_split_rename_record() {
        let text = "3\t1\tdocs/a.mdx\x002\t0\t\0old.mdx\0new.mdx\0";
        let counts = parse_numstat(text);
        assert_eq!(counts.get("docs/a.mdx"), Some(&(3, 1)));
        assert_eq!(counts.get("new.mdx"), Some(&(2, 0)));
    }

    #[test]
    fn patches_are_split_per_file_and_start_at_the_first_hunk() {
        let text = "diff --git a/x b/x\nindex 1..2 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\ndiff --git a/y b/y\nnew file mode 100644\n--- /dev/null\n+++ b/y\n@@ -0,0 +1 @@\n+hello\n";
        let map = split_patches(text);
        assert_eq!(map.get("x").map(String::as_str), Some("@@ -1 +1 @@\n-a\n+b"));
        assert_eq!(
            map.get("y").map(String::as_str),
            Some("@@ -0,0 +1 @@\n+hello")
        );
    }

    #[test]
    fn status_letters_map_to_githubs_words() {
        assert_eq!(status_word('A'), "added");
        assert_eq!(status_word('D'), "removed");
        assert_eq!(status_word('R'), "renamed");
        assert_eq!(status_word('X'), "modified");
    }
}
