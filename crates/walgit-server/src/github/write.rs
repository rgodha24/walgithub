//! The write primitive behind every mutating endpoint of the facade.
//!
//! Writes go through walgit's real publish path, so the bucket is the truth
//! and every other instance sees the result on its next revalidation. The
//! mechanism is exactly receive-pack's, minus the wire:
//!
//! 1. [`RepoHandle::sync`] — Serve level, so the base commit's objects are
//!    readable and no pack is removed while we build on them.
//! 2. Build the new objects with `git` plumbing in a **scratch object
//!    directory** (`GIT_OBJECT_DIRECTORY` + `GIT_ALTERNATE_OBJECT_DIRECTORIES`
//!    at the repository's own `objects/`, `GIT_INDEX_FILE` in the same
//!    tempdir). Nothing touches the serving copy, so a write that is refused
//!    later leaves nothing behind — the same property receive-pack gets from
//!    its per-ingest scratch git dir.
//! 3. `git pack-objects --revs --stdout` over `<new> ^<base>` for a
//!    self-contained pack of exactly the new objects.
//! 4. [`LocalRepo::ingest_pack`] indexes it into the serving copy
//!    (`IngestedPack`), then [`LocalRepo::check_connectivity_async`] proves
//!    the tip before anything is published.
//! 5. [`RepoHandle::publish_push_synced`] — pack PUT ∥ log PUT → manifest CAS.
//!
//! Ref creates, fast-forward updates, force updates and deletes are the same
//! call with no pack. The WAL never enforces fast-forward (that is
//! receive-pack's job), so this module does the ancestry check itself with
//! `git merge-base --is-ancestor` and refuses with [`GhError::Conflict`].
//!
//! Every `old_oid` sent to the WAL is either the expected hex or `""`, never
//! forty zeros: `publish::verify_txn` reads `""` as "this ref must not exist".

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use tokio::process::Command;
use walgit_git::RepoId;
use walgit_proto::v1::{RefTransaction, RefUpdate};

use super::error::{FieldError, GhError, GhResult};
use crate::AppState;

/// One file change in a commit. `Delete` on a path the base tree does not have
/// is a no-op, not an error — GitHub's own tree API behaves the same way.
#[derive(Debug, Clone)]
pub enum Change {
    Put {
        path: String,
        /// Git file mode: `100644`, `100755` or `120000`.
        mode: &'static str,
        content: Bytes,
    },
    Delete {
        path: String,
    },
}

impl Change {
    pub fn put(path: impl Into<String>, content: impl Into<Bytes>) -> Self {
        Change::Put {
            path: path.into(),
            mode: "100644",
            content: content.into(),
        }
    }
    pub fn delete(path: impl Into<String>) -> Self {
        Change::Delete { path: path.into() }
    }
    fn path(&self) -> &str {
        match self {
            Change::Put { path, .. } | Change::Delete { path } => path,
        }
    }
}

/// Author or committer of a commit built here.
#[derive(Debug, Clone)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// RFC 3339 / git date. `None` = now.
    pub date: Option<String>,
}

impl Signature {
    pub fn new(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            email: email.into(),
            date: None,
        }
    }
}

/// Create a commit on top of `base` and move `ref_name` to it.
#[derive(Debug, Clone)]
pub struct CommitOnRef {
    /// Full ref name (`refs/heads/main`).
    pub ref_name: String,
    /// The commit to build on. `None` = an initial commit on an unborn ref.
    pub base: Option<String>,
    /// The value `ref_name` must currently have. `None` = whatever `base` is
    /// (or "must not exist" for an initial commit).
    pub expected_head: Option<String>,
    pub changes: Vec<Change>,
    pub message: String,
    pub author: Signature,
    pub committer: Signature,
}

/// What a successful write moved.
#[derive(Debug, Clone)]
pub struct RefWritten {
    pub ref_name: String,
    /// The oid the ref now has; empty after a delete.
    pub oid: String,
    /// WAL sequence of the entry that made it visible.
    pub seq: u64,
}

/// Build a commit and publish it on `req.ref_name`.
pub async fn commit_on_ref(
    st: &Arc<AppState>,
    id: &RepoId,
    req: CommitOnRef,
) -> GhResult<RefWritten> {
    validate_ref_name(&req.ref_name)?;
    for c in &req.changes {
        validate_path(c.path())?;
    }
    let handle = super::repo::open(st, id).await?;
    let guard = handle.sync().await?;
    let local = handle.local().clone();

    let current = super::repo::ref_oid(&local, &req.ref_name)?;
    let expected = req.expected_head.clone().or_else(|| req.base.clone());
    if let Some(want) = &expected
        && current.as_deref() != Some(want.as_str())
    {
        return Err(GhError::Conflict(format!(
            "{} is at {}, expected {want}",
            req.ref_name,
            current.as_deref().unwrap_or("(unborn)")
        )));
    }
    if expected.is_none() && current.is_some() {
        return Err(GhError::Conflict(format!(
            "Reference already exists: {}",
            req.ref_name
        )));
    }

    let scratch = Scratch::new(local.path()).await?;
    let commit = scratch.build_commit(&req).await?;
    let pack = scratch.pack(&commit, req.base.as_deref()).await?;

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
        .map_err(|e| GhError::Internal(format!("index the new objects: {e}")))?;

    let tip = gix_hash::ObjectId::from_hex(commit.as_bytes())
        .map_err(|e| GhError::Internal(format!("commit-tree returned {commit:?}: {e}")))?;
    local
        .check_connectivity_async(&[tip], true)
        .await
        .map_err(|e| GhError::Internal(format!("connectivity: {e}")))?;

    drop(guard);
    publish(
        &handle,
        &req.ref_name,
        expected.as_deref().unwrap_or_default(),
        &commit,
        ingested,
    )
    .await
}

/// `POST /git/refs`: create `ref_name` at `oid`. 422 when it already exists.
pub async fn create_ref(
    st: &Arc<AppState>,
    id: &RepoId,
    ref_name: &str,
    oid: &str,
) -> GhResult<RefWritten> {
    validate_ref_name(ref_name)?;
    let handle = super::repo::open(st, id).await?;
    let guard = handle.sync().await?;
    let local = handle.local().clone();
    if super::repo::ref_oid(&local, ref_name)?.is_some() {
        return Err(GhError::validation(
            "Reference already exists",
            FieldError::invalid("Reference", "ref", format!("{ref_name} already exists")),
        ));
    }
    require_object(&local, oid)?;
    drop(guard);
    publish(&handle, ref_name, "", oid, None).await
}

/// `PATCH /git/refs/{ref}`: move `ref_name` to `oid`. Fast-forward unless
/// `force`; 422 when the ref does not exist.
pub async fn update_ref(
    st: &Arc<AppState>,
    id: &RepoId,
    ref_name: &str,
    oid: &str,
    force: bool,
) -> GhResult<RefWritten> {
    validate_ref_name(ref_name)?;
    let handle = super::repo::open(st, id).await?;
    let guard = handle.sync().await?;
    let local = handle.local().clone();
    let Some(current) = super::repo::ref_oid(&local, ref_name)? else {
        return Err(GhError::not_found(ref_name));
    };
    require_object(&local, oid)?;
    if !force
        && current != oid
        && !local
            .is_ancestor(&current, oid)
            .await
            .map_err(|e| GhError::Internal(format!("merge-base: {e}")))?
    {
        return Err(GhError::validation(
            "Update is not a fast forward",
            FieldError::invalid(
                "Reference",
                "sha",
                format!("{oid} is not a descendant of {current}"),
            ),
        ));
    }
    drop(guard);
    publish(&handle, ref_name, &current, oid, None).await
}

/// `DELETE /git/refs/{ref}`.
pub async fn delete_ref(st: &Arc<AppState>, id: &RepoId, ref_name: &str) -> GhResult<RefWritten> {
    validate_ref_name(ref_name)?;
    let handle = super::repo::open(st, id).await?;
    let guard = handle.sync_refs().await?;
    let local = handle.local().clone();
    let Some(current) = super::repo::ref_oid(&local, ref_name)? else {
        return Err(GhError::not_found(ref_name));
    };
    drop(guard);
    publish(&handle, ref_name, &current, "", None).await
}

/// One ref update through the WAL: pack PUT ∥ log PUT → manifest CAS. `old`
/// and `new` are hex or `""` (create / delete).
async fn publish(
    handle: &Arc<walgit_wal::RepoHandle>,
    ref_name: &str,
    old: &str,
    new: &str,
    pack: Option<walgit_git::IngestedPack>,
) -> GhResult<RefWritten> {
    let mut txn = RefTransaction {
        updates: vec![RefUpdate {
            name: ref_name.to_string(),
            old_oid: old.to_string(),
            new_oid: new.to_string(),
            ..Default::default()
        }],
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
                walgit_wal::RefError::Conflict { expected, actual } => GhError::Conflict(format!(
                    "{name} moved: expected {expected}, found {actual}"
                )),
                other => GhError::Conflict(format!("{name}: {other}")),
            });
        }
    }
    super::events::ref_written(handle.id(), ref_name, old, new, res.seq);
    Ok(RefWritten {
        ref_name: ref_name.to_string(),
        oid: new.to_string(),
        seq: res.seq,
    })
}

/// A scratch object directory that reads the serving copy through
/// `GIT_ALTERNATE_OBJECT_DIRECTORIES` and writes nowhere near it.
///
/// `GIT_WORK_TREE` points at an empty directory in the same tempdir: the
/// serving copy is bare, and `read-tree`/`update-index` refuse to run without
/// a work tree. Nothing is ever checked out into it.
struct Scratch {
    dir: tempfile::TempDir,
    git_dir: PathBuf,
}

impl Scratch {
    async fn new(git_dir: &Path) -> GhResult<Self> {
        let git_dir = git_dir.to_path_buf();
        let dir = tokio::task::spawn_blocking(|| -> std::io::Result<tempfile::TempDir> {
            let dir = tempfile::Builder::new().prefix("walgit-gh-").tempdir()?;
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

    async fn run(&self, mut cmd: Command, stdin: &[u8]) -> GhResult<Vec<u8>> {
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
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| GhError::Internal(format!("git: {e}")))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        Err(GhError::Internal(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }

    async fn text(&self, cmd: Command, stdin: &[u8]) -> GhResult<String> {
        let out = self.run(cmd, stdin).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// Stage the base tree, apply the changes, write the tree and the commit.
    async fn build_commit(&self, req: &CommitOnRef) -> GhResult<String> {
        if let Some(base) = &req.base {
            let mut c = self.command();
            c.args(["read-tree", &format!("{base}^{{tree}}")]);
            self.run(c, &[]).await?;
        }
        for change in &req.changes {
            match change {
                Change::Put {
                    path,
                    mode,
                    content,
                } => {
                    let mut c = self.command();
                    c.args(["hash-object", "-w", "-t", "blob", "--stdin"]);
                    let oid = self.text(c, content).await?;
                    let mut c = self.command();
                    c.args([
                        "update-index",
                        "--add",
                        "--cacheinfo",
                        &format!("{mode},{oid},{path}"),
                    ]);
                    self.run(c, &[]).await?;
                }
                Change::Delete { path } => {
                    let mut c = self.command();
                    c.args(["update-index", "--force-remove", path]);
                    self.run(c, &[]).await?;
                }
            }
        }
        let mut c = self.command();
        c.arg("write-tree");
        let tree = self.text(c, &[]).await?;

        let mut c = self.command();
        c.args(["commit-tree", &tree]);
        if let Some(base) = &req.base {
            c.args(["-p", base]);
        }
        c.args(["-m", &req.message]);
        c.env("GIT_AUTHOR_NAME", &req.author.name)
            .env("GIT_AUTHOR_EMAIL", &req.author.email)
            .env("GIT_COMMITTER_NAME", &req.committer.name)
            .env("GIT_COMMITTER_EMAIL", &req.committer.email);
        if let Some(d) = &req.author.date {
            c.env("GIT_AUTHOR_DATE", d);
        }
        if let Some(d) = &req.committer.date {
            c.env("GIT_COMMITTER_DATE", d);
        }
        self.text(c, &[]).await
    }

    /// A self-contained pack of everything `commit` adds over `base`.
    async fn pack(&self, commit: &str, base: Option<&str>) -> GhResult<Vec<u8>> {
        let mut revs = format!("{commit}\n");
        if let Some(base) = base {
            revs.push('^');
            revs.push_str(base);
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
        self.run(c, revs.as_bytes()).await
    }
}

fn require_object(local: &walgit_git::LocalRepo, oid: &str) -> GhResult<()> {
    let parsed = gix_hash::ObjectId::from_hex(oid.as_bytes()).map_err(|_| {
        GhError::validation(
            "Validation Failed",
            FieldError::invalid("Reference", "sha", format!("{oid} is not an object id")),
        )
    })?;
    if local.has_object(&parsed) {
        return Ok(());
    }
    Err(GhError::validation(
        "Validation Failed",
        FieldError::invalid("Reference", "sha", format!("{oid} is not a valid object")),
    ))
}

/// `refs/heads/x` shapes only: the facade never invents a ref namespace, and a
/// name with `..`, a control byte or a trailing `/` would be rejected by
/// `apply_ref_txn` later with a much worse message.
fn validate_ref_name(name: &str) -> GhResult<()> {
    let ok = name.starts_with("refs/")
        && !name.ends_with('/')
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains(' ')
        && name.bytes().all(|b| b.is_ascii_graphic())
        && !name.contains('~')
        && !name.contains('^')
        && !name.contains(':')
        && !name.contains('?')
        && !name.contains('*')
        && !name.contains('[')
        && !name.contains('\\');
    if ok {
        return Ok(());
    }
    Err(GhError::validation(
        "Validation Failed",
        FieldError::invalid("Reference", "ref", format!("{name} is not a valid ref name")),
    ))
}

fn validate_path(path: &str) -> GhResult<()> {
    let ok = !path.is_empty()
        && !path.starts_with('/')
        && !path.ends_with('/')
        && !path.split('/').any(|s| s.is_empty() || s == "." || s == "..")
        && !path.contains('\0');
    if ok {
        return Ok(());
    }
    Err(GhError::validation(
        "Validation Failed",
        FieldError::invalid("Blob", "path", format!("{path} is not a valid path")),
    ))
}

#[cfg(test)]
mod tests {
    use super::{validate_path, validate_ref_name};

    #[test]
    fn ref_names() {
        assert!(validate_ref_name("refs/heads/main").is_ok());
        assert!(validate_ref_name("refs/tags/v1.0.0").is_ok());
        assert!(validate_ref_name("main").is_err());
        assert!(validate_ref_name("refs/heads/a..b").is_err());
        assert!(validate_ref_name("refs/heads/a b").is_err());
        assert!(validate_ref_name("refs/heads/").is_err());
    }

    #[test]
    fn paths() {
        assert!(validate_path("docs/index.mdx").is_ok());
        assert!(validate_path("/abs").is_err());
        assert!(validate_path("a//b").is_err());
        assert!(validate_path("../escape").is_err());
        assert!(validate_path("").is_err());
    }
}
