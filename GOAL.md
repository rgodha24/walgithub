# GOAL — what walgit is for

Context: for everyone (humans and agents) working on this repository. Read this before `AGENTS.md`. This page
is **what we want**; `AGENTS.md` is **how we build it**. When a choice is not obviously right, come back here
and ask: which option serves this goal better?

## The one sentence

**A share-nothing git host, fast for monorepos, with an object store as the *only* source of truth — one binary
anyone can run against a bucket, and predictable enough that tooling can build on it.**

## What that means, unpacked

1. **The object store is the only source of truth.** The bucket *is* the repository. Every push is an
   immutable object + one CAS'd manifest write; every instance is a disposable cache that revalidates with one
   conditional GET. No database, no Redis, no gossip, no leader, no node identity. Wipe every instance and lose
   nothing but warmth. (Cursor's *Git at any scale* / Continuity is the design we follow —
   `docs/reference/cursor-git-at-any-scale.md`.)
2. **Share-nothing, elastic.** Any host pointed at the bucket can serve a repository from durable state;
   refs-level reads work on every host; object work goes where placement says. Coordination is only through
   object-store primitives (CAS, leases, content-addressed immutable objects). Consistency is never "eventual":
   push acknowledged ⇒ the next request anywhere sees it.
3. **Fast for the monorepo, from machines smaller than it.** A repository of tens of gigabytes, tens of
   millions of objects and hundreds of thousands of refs must be *fast* from a host with a few GiB of tmpfs:
   refs in < 1 s cold, web pages in ~100 ms, CI's `clone --filter=blob:none --depth=1 --sparse --single-branch`
   in seconds, a developer's `git fetch` in the time it takes to read the output, fresh clones as static
   bundles (weekly full + daily/hourly chain) so bytes move bucket → laptop and never through a server.
   **Fast clone + fast catch-up through bundles is the north star** (`docs/BUNDLE_URI_DESIGN.md`).
4. **All the features a git host needs, and only those**: smart HTTP v0/v2 (ls-refs, fetch with
   filter/shallow/deepen, receive-pack atomic/delete/tags/push-options/report-status-v2), bundle-uri, LFS,
   `<owner>/<repo>` namespaces, per-repo push policy and settings, ref events, a browsing web UI + one JSON API +
   one SDK (`repos.js`), tasks/narration so nothing ever waits silently. Not in scope: code review, merge
   queues, CI, issues — those live elsewhere and build on this.
5. **Works great for developers and their laptops.** One auth story (browser sign-in through your identity
   provider, a token for git), one install script, `git` does the rest; errors tell you the fix; every long
   wait is narrated. The developer on a rebased branch must get *cheaper*, never slower.
6. **Predictable for the systems that build on it**: stable, immutable, cacheable, CDN-able artefacts
   (bundles, packs, sha-addressed API answers); O(1) ref lookups; latency that does not depend on which
   instance you hit or how many refs exist; a provenance log you can rewind (`walgit wal materialize --at-seq`).
7. **Use the tools; don't reinvent them.** Upstream `git` where it is right (repack, bitmaps, bundle create,
   upload-pack), `gix` where it is faster and measured, Rust + tokio + axum for the server, the object store
   as it is (range reads, compose / multipart copy, CAS), a plain nginx or CDN in front of static bytes,
   content addressing everywhere, the WAL's ergonomics (`walgit wal ls|show|materialize`) as a first-class
   operator surface. Anything that needs a real disk or hours of CPU runs with the same binary on a host that
   has them.

## How we know we are there (acceptance)

| Claim | Bar |
|---|---|
| Cold instance is useful in seconds | `ls-remote` of the largest repository < 1 s on a fresh instance, even while it installs that repository's packs |
| CI clone of the monorepo | `clone --filter=blob:none --depth=1 --sparse --single-branch` in seconds, not minutes (reference: 2075 s → 8 s on a 57 GiB / 73 M-object repository) |
| Developer catch-up | a days-stale `fetch` on main = exactly the bundle slots missed + < 1 h of objects from upload-pack |
| Fresh clone of the monorepo | bytes through the server ≈ one hour of pushes; the rest is static bundles (reference: 32.7 GB static, 2.8 MB through upload-pack) |
| Web UI on the monorepo | tree/blob/commits without packs on disk, ~100–200 ms warm |
| Push | acknowledged only after the bucket ACKs; one CAS per batch; the host that maintains a repository writes it |
| Consistency | push then fetch anywhere sees it; concurrent pushers: exactly one winner (the simulation suite) |
| Security | every route authenticated in `token`/`oidc` mode; fail-closed config; a real 401 for a dead credential |
| Data completeness | every object reachable from an advertised ref is in the pack set; weekly `fsck` + `repair` keep it so |

## What we deliberately do **not** optimise for

- Millions of tiny repositories (the long tail is served, not tuned for).
- Running a 30 GB base repack on a tmpfs host (that is a job for the host with the SSD, weekly).
- Forking git or inventing an object format: weird stuff happens *around* git, never inside it.
