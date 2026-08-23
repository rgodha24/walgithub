# Patches to git that walgit's design leans on

Context: **client-side Git patch catalog and validation instructions** for engineers changing bundle selection
or preparing the patched git. Read with `docs/BUNDLE_URI_DESIGN.md §6b` before enabling
`bundles.advertise_filtered` or distributing a patched client.

## `0001-bundle-uri-use-a-bundle-only-when-its-filter-matches.patch`
**Candidate (not written, 2026-08-22)**: make `git fetch` honour `transfer.bundleURI` against the server's v2
`bundle-uri` advertisement the way `git clone` does (today only `fetch.bundleURI=<list URI>` makes a fetch look at
bundles; an advertised clone records merely `fetch.bundleCreationToken`, `builtin/clone.c` sets `fetch.bundleuri` only
in the `--bundle-uri=` branch). Until then every walgit clone recipe passes `-c fetch.bundleURI=<list>`.

Client-side matching of `bundle.<id>.filter` against the clone's object filter — the key the
bundle-uri list format documents but git (2.47 … master) never read (see
`docs/BUNDLE_URI_DESIGN.md` §6b and measurements for what that costs on
acme/monorepo). With it, ONE advertised list can carry both the whole-history family and the
`blob:none` history family (`bundles.advertise_filtered = true`), and each clone takes its own:

- `git clone` → the unfiltered bundles only (no promisor packs, fsck clean);
- `git clone --filter=blob:none` → the history bundles only (promisor packs from bundles,
  blobs lazy on checkout); skipped bundles show up as trace2 data `bundle-uri/filter-skip`;
- a stale blobless `git fetch` → the newer history incrementals only.

Written against git `v2.55.0-602-g0c4de8e9a9`; applies to 2.54+ with `git am`. Test: `PATCHED_GIT=/path/to/built/git tests/git-bundle-filter.sh`
(starts a local walgit with both families on one list; also shows stock git's defect: 3 promisor
packs + 3 missing objects in a full clone). Until your clients carry it, the blobless family stays at `bundles/list?filter=blob:none`.
