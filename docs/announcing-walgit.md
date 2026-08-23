# Announcing walgit: a git server that is one binary in front of a bucket

*Draft announcement post. Publish alongside the repository.*

We are open-sourcing **walgit**, a git server whose only durable state is an object-store bucket.

You run one binary. You point it at S3 or GCS. You get smart HTTP fetch and push, clones served as static files
through git's `bundle-uri`, Git LFS, a browsing web UI, a JSON API with a drop-in SDK, per-repository push policy
and webhooks. Run a second copy against the same bucket and it serves the same repositories, consistently, with
nothing to coordinate. Kill every copy and you have lost warmth — nothing else.

```sh
walgit serve --config walgit.toml
git push https://git.example.com/acme/app.git main     # a push to a new name creates the repository
```

## Why we built it

We needed to host a monorepo that most git hosting treats as an edge case: tens of gigabytes, tens of millions of
objects, hundreds of thousands of refs, LFS, thousands of developers and their agents fetching all day. The usual
advice is to make the repository smaller — fewer refs, shallower history, a different workflow. We wanted the
opposite bet: the repository is what it is; the host has to get out of the way.

The design we started from is Cursor's [*Git at any scale*](https://cursor.com/blog/git-at-any-scale), the system
they call Continuity. Its insight is simple and changes the economics of hosting git: **put a write-ahead log in
object storage and make it the source of truth; every on-disk repository is a cache.** A push is stored as an
immutable pack in the bucket and becomes visible when a tiny manifest is rewritten with a compare-and-swap. That
CAS *is* the consensus — no election, no quorum, no primary, no database mapping repositories to machines. Any
instance can accept a push; two racing instances cannot both win. A replica that has never seen a repository
reads the log and has it. Every read starts with one conditional GET, so there is no "eventually": a push
acknowledged anywhere is visible everywhere on the next request.

walgit is a Rust implementation of that idea, plus what it took to run it on machines **smaller than the
repository**:

* **The remote reader.** Refs, the web UI and the API work for a repository whose packs will never fit on the
  instance: pack indexes are local, pack data is read by HTTP range request into a block cache, and the UI
  faults in exactly the objects a `git ls-tree` or `git show` will touch.
* **The history pack.** When the base pack is rebuilt, a derived pack of commits and trees is published next to
  it. Every instance keeps that one local (a few GB for a 57 GiB repository); only blob bytes cross the network.
  CI's `clone --filter=blob:none --depth=1 --sparse` of the monorepo went from 35 minutes to 8 seconds.
* **Bundles as the transport.** A fresh clone of a big repository should not touch the server at all. walgit's
  maintainer cuts bundles on calendar slots — a weekly full (for a big repo, a server-side *compose* of a header
  onto the base pack: no bytes move), chained dailies, hourlies — and the list is a pure function of the WAL: a
  missing slot is built on the next pass, a deleted bundle is rebuilt identically. A fresh clone downloads the
  newest full and the chain above it from the bucket and asks upload-pack for a few kilobytes. A days-stale fetch
  downloads exactly the days it missed.
* **A maintainer that heals itself.** Checkpoints, bundle builds, geometric compaction, base rebuilds,
  connectivity audits and repairs are one loop that computes the desired state from (config, WAL) every pass and
  does one bounded unit of the most important missing work, under a lease. There are no cron jobs and no backfill
  scripts; an outage of any length leaves no holes.
* **Nothing waits silently.** Anything slow is a task with an id, a log and a progress stream, narrated to git on
  sideband 2 (`remote: * …`) and to the browser as server-sent events.

## What it is not

walgit is a git host, not a forge. There is no code review, no issues, no CI, no merge queue — those live
elsewhere and can build on the API and the webhook. It does not fork git: upstream `git` does upload-pack,
repack, bitmaps and bundles; walgit does receive-pack, the WAL and the plumbing around them.

## Running it

The standalone shape is a single process that terminates its own TLS and streams every byte itself. Authentication
is `none` for loopback experiments, static `token`s for a small team, or `oidc` against any OpenID Connect issuer:
browsers sign in through your identity provider, then mint a walgit access token for git at `/_auth/tokens`; one
idempotent installer sets a developer machine up. S3 (and everything S3-compatible: MinIO, Ceph, R2, rustfs) and
GCS are both first class, down to the server-side compose that builds the weekly bundle.

For more, an optional nginx in front can terminate public TLS, cache one auth verdict per credential, and take the
bytes: walgit answers a bundle or LFS download with `X-Accel-Redirect` and nginx streams and caches the object
from the bucket itself (S3 presigned or GCS with walgit's bearer). The example config documents the whole contract.

```sh
nix run github:tobi/walgit -- --config walgit.toml     # or: podman build -f Containerfile .
```

The repository is at **github.com/tobi/walgit**, MIT licensed. `README.md` is the introduction; `AGENTS.md` is
the architecture and the list of design decisions with their reasoning; `docs/BUNDLE_URI_DESIGN.md` is the bundle
scheduler's design of record. The numbers we quote — 8-second CI clones, 32 GB of clone bytes with 2.8 MB through
the server — come from the monorepo it was built for; the simulation suite (crashes, partitions, stale reads,
lost responses) is how we keep the consistency story honest.

If you host a repository that your current provider wishes you would shrink, we would like to hear from you.
