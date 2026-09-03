//! Per-file diffs, rendered by `git diff-tree` against the bare serving copy.
//!
//! `docs/GITHUB.md` §8 assumed compare would need a scratch checkout. It does
//! not: `diff-tree` takes two tree-ishes and never looks at a work tree, so
//! three invocations on the bare repository produce everything GitHub's
//! `files[]` carries.
//!
//! | Invocation | Yields |
//! |---|---|
//! | `diff-tree -r -M -z --raw` | status, old and new blob sha, old and new path |
//! | `diff-tree -r -M -z --numstat` | additions, deletions (`-` for binary) |
//! | `diff-tree -r -M -p --no-color` | the patch hunks |
//!
//! All three apply the same rename detection to the same pair of trees, so
//! they emit the same files in the same order; the patch blocks are matched
//! back by their `+++ b/<path>` header where one exists and by position
//! otherwise.

use super::error::GhResult;
use super::models::Urls;
use super::reads;

/// One entry of a compare's or a pull request's `files[]`.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// `added`, `removed`, `modified`, `renamed` or `copied`.
    pub status: &'static str,
    pub filename: String,
    pub previous_filename: Option<String>,
    /// The new blob sha, or the old one for a deletion — the server keys a
    /// content cache on it (`uniqueId`).
    pub sha: String,
    pub additions: u64,
    pub deletions: u64,
    pub patch: Option<String>,
    pub binary: bool,
}

impl FileChange {
    pub fn changes(&self) -> u64 {
        self.additions.saturating_add(self.deletions)
    }
}

const ZERO_OID: &str = "0000000000000000000000000000000000000000";

fn status_of(code: &str) -> &'static str {
    match code.chars().next() {
        Some('A') => "added",
        Some('D') => "removed",
        Some('R') => "renamed",
        Some('C') => "copied",
        _ => "modified",
    }
}

/// Every file that differs between two tree-ishes, with stats and patches.
pub async fn changed_files(
    local: &walgit_git::LocalRepo,
    base: &str,
    head: &str,
) -> GhResult<Vec<FileChange>> {
    let raw = reads::git(
        local,
        &[
            "diff-tree",
            "-r",
            "-M",
            "-z",
            "--raw",
            "--no-commit-id",
            "--end-of-options",
            base,
            head,
        ],
    )
    .await?;
    let mut files = parse_raw(&String::from_utf8_lossy(&raw));
    if files.is_empty() {
        return Ok(files);
    }

    let numstat = reads::git(
        local,
        &[
            "diff-tree",
            "-r",
            "-M",
            "-z",
            "--numstat",
            "--no-commit-id",
            "--end-of-options",
            base,
            head,
        ],
    )
    .await?;
    apply_numstat(&mut files, &String::from_utf8_lossy(&numstat));

    let patch = reads::git(
        local,
        &[
            "diff-tree",
            "-r",
            "-M",
            "-p",
            "--no-color",
            "--no-commit-id",
            "--end-of-options",
            base,
            head,
        ],
    )
    .await?;
    apply_patches(&mut files, &String::from_utf8_lossy(&patch));
    Ok(files)
}

/// `:<oldmode> <newmode> <oldsha> <newsha> <status>\0<path>[\0<newpath>]\0`.
fn parse_raw(out: &str) -> Vec<FileChange> {
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let mut files = Vec::new();
    while let Some(meta) = fields.next() {
        let Some(meta) = meta.strip_prefix(':') else {
            continue;
        };
        let cols: Vec<&str> = meta.split_whitespace().collect();
        let (Some(old_sha), Some(new_sha), Some(code)) =
            (cols.get(2), cols.get(3), cols.get(4))
        else {
            continue;
        };
        let status = status_of(code);
        let Some(first) = fields.next() else { break };
        let (filename, previous_filename) = if matches!(status, "renamed" | "copied") {
            let Some(second) = fields.next() else { break };
            (second.to_string(), Some(first.to_string()))
        } else {
            (first.to_string(), None)
        };
        let sha = if *new_sha == ZERO_OID { old_sha } else { new_sha };
        files.push(FileChange {
            status,
            filename,
            previous_filename,
            sha: (*sha).to_string(),
            additions: 0,
            deletions: 0,
            patch: None,
            binary: false,
        });
    }
    files
}

/// `<added>\t<deleted>\t<path>\0`, or `<added>\t<deleted>\t\0<old>\0<new>\0`
/// for a rename. `-` in either count means the file is binary.
fn apply_numstat(files: &mut [FileChange], out: &str) {
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let mut i = 0;
    while let Some(record) = fields.next() {
        let mut cols = record.splitn(3, '\t');
        let (Some(add), Some(del), Some(rest)) = (cols.next(), cols.next(), cols.next()) else {
            continue;
        };
        if rest.is_empty() {
            // A rename: the two paths follow as their own NUL-terminated fields.
            fields.next();
            fields.next();
        }
        let Some(f) = files.get_mut(i) else { break };
        i += 1;
        if add == "-" || del == "-" {
            f.binary = true;
            continue;
        }
        f.additions = add.parse().unwrap_or(0);
        f.deletions = del.parse().unwrap_or(0);
    }
}

/// Split a combined patch on its `diff --git` boundaries and hand each block
/// to the file it names. GitHub's `patch` starts at the first `@@`, with none
/// at all for a binary file or a pure rename.
fn apply_patches(files: &mut [FileChange], out: &str) {
    let mut by_index = 0usize;
    for block in out.split("\ndiff --git ") {
        let block = block.strip_prefix("diff --git ").unwrap_or(block);
        if block.trim().is_empty() {
            continue;
        }
        let target = block
            .lines()
            .find_map(|l| l.strip_prefix("+++ b/"))
            .or_else(|| block.lines().find_map(|l| l.strip_prefix("--- a/")));
        let hunks: String = block
            .lines()
            .skip_while(|l| !l.starts_with("@@"))
            .collect::<Vec<_>>()
            .join("\n");
        let idx = target
            .and_then(|t| files.iter().position(|f| f.filename == t))
            .unwrap_or(by_index);
        by_index = by_index.saturating_add(1);
        if let Some(f) = files.get_mut(idx)
            && !hunks.is_empty()
        {
            f.patch = Some(hunks);
        }
    }
}

/// GitHub's `files[]` entry. `patch` is omitted for a binary file, which is
/// exactly what GitHub does and what a client rendering a diff checks for.
pub fn file_json(
    urls: &Urls,
    full_name: &str,
    head_sha: &str,
    f: &FileChange,
) -> serde_json::Value {
    let api = format!("{}/repos/{full_name}", urls.api);
    let mut v = serde_json::json!({
        "sha": f.sha,
        "filename": f.filename,
        "status": f.status,
        "additions": f.additions,
        "deletions": f.deletions,
        "changes": f.changes(),
        "blob_url": format!("{}/{full_name}/blob/{head_sha}/{}", urls.html, f.filename),
        "raw_url": format!("{}/{full_name}/raw/{head_sha}/{}", urls.html, f.filename),
        "contents_url": format!("{api}/contents/{}?ref={head_sha}", f.filename),
    });
    if let Some(map) = v.as_object_mut() {
        if let Some(prev) = &f.previous_filename {
            map.insert("previous_filename".into(), serde_json::json!(prev));
        }
        if let Some(patch) = &f.patch {
            map.insert("patch".into(), serde_json::json!(patch));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::{apply_numstat, apply_patches, parse_raw};

    const RAW: &str = ":100644 100644 9b69328 9b69328 R100\0README.md\0READ2.md\0:000000 100644 0000000000000000000000000000000000000000 d4f30d3 A\0bin.dat\0:100644 100644 de98044 a7bc997 M\0pages/index.mdx\0";

    #[test]
    fn raw_records_carry_renames_and_deletions() {
        let files = parse_raw(RAW);
        assert_eq!(files.len(), 3);
        let r = files.first().expect("rename");
        assert_eq!(r.status, "renamed");
        assert_eq!(r.filename, "READ2.md");
        assert_eq!(r.previous_filename.as_deref(), Some("README.md"));
        let a = files.get(1).expect("add");
        assert_eq!(a.status, "added");
        assert_eq!(a.sha, "d4f30d3");
        assert_eq!(files.get(2).map(|f| f.status), Some("modified"));
    }

    #[test]
    fn a_deletion_keeps_the_old_blob_sha() {
        let raw = ":100644 000000 abc123 0000000000000000000000000000000000000000 D\0gone.md\0";
        let files = parse_raw(raw);
        assert_eq!(files.first().map(|f| f.status), Some("removed"));
        assert_eq!(files.first().map(|f| f.sha.as_str()), Some("abc123"));
    }

    #[test]
    fn numstat_marks_binaries_and_counts_lines() {
        let mut files = parse_raw(RAW);
        apply_numstat(
            &mut files,
            "0\t0\t\0README.md\0READ2.md\0-\t-\tbin.dat\x002\t1\tpages/index.mdx\0",
        );
        assert!(!files.first().expect("rename").binary);
        assert!(files.get(1).expect("bin").binary);
        let m = files.get(2).expect("modified");
        assert_eq!((m.additions, m.deletions, m.changes()), (2, 1, 3));
    }

    #[test]
    fn patches_start_at_the_first_hunk_and_skip_binaries() {
        let mut files = parse_raw(RAW);
        let out = "diff --git a/README.md b/READ2.md\nsimilarity index 100%\nrename from README.md\nrename to READ2.md\ndiff --git a/bin.dat b/bin.dat\nnew file mode 100644\nBinary files /dev/null and b/bin.dat differ\ndiff --git a/pages/index.mdx b/pages/index.mdx\nindex de98044..a7bc997 100644\n--- a/pages/index.mdx\n+++ b/pages/index.mdx\n@@ -1,3 +1,4 @@\n a\n-b\n+B\n c\n+d\n";
        apply_patches(&mut files, out);
        assert!(files.first().expect("rename").patch.is_none());
        assert!(files.get(1).expect("bin").patch.is_none());
        let p = files.get(2).expect("modified").patch.clone().expect("patch");
        assert!(p.starts_with("@@ -1,3 +1,4 @@"), "patch: {p}");
        assert!(p.contains("+B"), "patch: {p}");
    }
}
