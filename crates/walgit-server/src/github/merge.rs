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
//! The diffs and the `rev-list` plumbing a PR needs are not here: `files[]`
//! and its totals come from [`super::diff`] and the revision walks from
//! [`super::repo`], both shared with the compare surface.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use walgit_git::{LocalRepo, RepoId};
use walgit_proto::v1::{RefTransaction, RefUpdate};

use super::error::{GhError, GhResult};
use super::write::Scratch;
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

/// The merge-specific half of [`super::write::Scratch`]: the scratch object
/// directory writes land in while the bare serving copy stays untouched.
impl Scratch {
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
        let commits = super::repo::commits_between(local, base, head).await?;
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
