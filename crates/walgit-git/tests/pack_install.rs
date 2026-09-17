mod common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;
use walgit_git::{LocalRepo, ObjectFormat, RepoId, gix_hash};

/// Compare identical cold installs; excludes fixture creation and copying.
/// Run with WALGIT_BENCH_PACKS=3801 cargo test -p walgit-git --test pack_install -- --ignored --nocapture.
#[tokio::test]
#[ignore = "cold pack install benchmark"]
async fn cold_pack_install_benchmark() {
    let count: usize = std::env::var("WALGIT_BENCH_PACKS")
        .unwrap_or_else(|_| "1000".into())
        .parse()
        .unwrap();
    assert!(count > 0);
    let source = common::SourceRepo::new();
    let fixture = tempfile::tempdir().unwrap();
    let mut packs = Vec::new();
    let mut commits = Vec::new();
    let mut previous = String::new();
    for i in 0..count {
        let commit = source.commit_file("current.txt", &format!("content {i}\n"), "update");
        let mut child = Command::new("git")
            .current_dir(&source.dir)
            .args(["pack-objects", "--revs"])
            .arg(fixture.path().join("pack"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut revisions = format!("{commit}\n");
        if !previous.is_empty() {
            revisions.push_str(&format!("^{previous}\n"));
        }
        child
            .stdin
            .take()
            .unwrap()
            .write_all(revisions.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        packs.push(String::from_utf8(out.stdout).unwrap().trim().to_owned());
        commits.push(gix_hash::ObjectId::from_hex(commit.as_bytes()).unwrap());
        previous = commit;
    }
    for batched in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let repo = LocalRepo::init(
            root.path(),
            &RepoId::new("test", "batch").unwrap(),
            ObjectFormat::Sha1,
        )
        .unwrap();
        for checksum in &packs {
            for ext in ["pack", "idx"] {
                let filename = format!("pack-{checksum}.{ext}");
                std::fs::copy(
                    fixture.path().join(&filename),
                    staging.path().join(filename),
                )
                .unwrap();
            }
        }
        let started = Instant::now();
        for checksum in &packs {
            let pack = staging.path().join(format!("pack-{checksum}.pack"));
            let idx = staging.path().join(format!("pack-{checksum}.idx"));
            if batched {
                repo.install_pack_files(&pack, &idx, &[]).unwrap();
            } else {
                repo.install_pack(&pack, &idx, &[]).await.unwrap();
            }
        }
        // The old materialization path also refreshed after the batch.
        repo.refresh_async().await.unwrap();
        eprintln!(
            "packs={count} batched={batched} elapsed={:?}",
            started.elapsed()
        );
        for oid in &commits {
            assert!(repo.has_object(oid), "missing commit {oid}");
        }
        assert_eq!(repo.packs().unwrap().len(), count);
    }
}
