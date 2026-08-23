#!/usr/bin/env bash
# tests/git-bundle-filter.sh — the git patch `docs/patches/0001-bundle-uri-match-bundle-id-filter…patch`
# (client-side `bundle.<id>.filter` matching) against a local walgit that puts BOTH bundle
# families — the whole history and the `blob:none` history — on ONE advertised list
# (`bundles.advertise_filtered = true`, design §6b).
#
#   stock git   : a full clone swallows the blobless bundles too (promisor packs in a
#                 full clone) — the reason the families live on separate lists today.
#   patched git : a full clone takes only the unfiltered bundles (no promisor pack, fsck
#                 clean); a `--filter=blob:none` clone takes only the history bundles
#                 (promisor packs from bundles, blobs missing until checkout).
#
# Usage: PATCHED_GIT=/path/to/patched/git [STOCK_GIT=git] tests/git-bundle-filter.sh
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PATCHED_GIT="${PATCHED_GIT:?set PATCHED_GIT=/path/to/patched/git (built from the fork with the patch)}"
# A git built in-tree (`make git git-remote-http`) finds its helpers next to itself.
export GIT_EXEC_PATH="${GIT_EXEC_PATH:-$(dirname "$PATCHED_GIT")}"
STOCK_GIT="${STOCK_GIT:-git}"
PORT="${PORT:-8431}"
TMP="$(mktemp -d)"
mkdir -p "$TMP/tpl"; export GIT_TEMPLATE_DIR="${GIT_TEMPLATE_DIR:-$TMP/tpl}"
PIDS=()
cleanup() { for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done; rm -rf "$TMP"; }
trap cleanup EXIT
step() { printf '\033[1m>>> %s\033[0m\n' "$*"; }
ok()   { printf '\033[32m  ok: %s\033[0m\n' "$*"; }
fail() { printf '\033[31m  FAIL: %s\033[0m\n' "$*"; exit 1; }

WALGIT="${WALGIT:-}"
if [[ -z "$WALGIT" ]]; then
  TARGET_DIR="$(cd "$ROOT" && cargo metadata --format-version=1 --no-deps | jq -r .target_directory)"
  WALGIT="$TARGET_DIR/release/walgit"
fi
[[ -x "$WALGIT" ]] || { (cd "$ROOT" && cargo build --release -p walgit-cli >/dev/null); }
BASE="http://127.0.0.1:$PORT"

cat > "$TMP/walgit.toml" <<EOF
[server]
listen = "127.0.0.1:$PORT"
roles = ["serve", "maintain", "compact", "bundle"]
[store]
backend = "memory"
bucket = "walgit-bundle-filter"
[cache]
dir = "$TMP/cache"
mode = "disk"
[wal]
freshness_ttl = "0s"
[maintenance]
disk = "ssd"
checkpoints = false
interval = "3600s"
[compaction]
enabled = true
[bundles]
enabled = true
advertise = true
advertise_filtered = true
main_only = true
min_commits = 1
[[bundles.strategy]]
name = "weekly"
kind = "full"
schedule = "0 0 23 * * Sun"
keep = 2
[[bundles.strategy]]
name = "hourly"
kind = "incremental"
base = "weekly"
schedule = "0 0 * * * *"
keep = 3
[[bundles.strategy]]
name = "weekly-history"
kind = "full"
schedule = "0 0 23 * * Sun"
keep = 2
filter = "blob:none"
[[bundles.strategy]]
name = "hourly-history"
kind = "incremental"
base = "weekly-history"
schedule = "0 0 * * * *"
keep = 3
filter = "blob:none"
[lfs]
enabled = false
EOF

step "walgit serve (memory) on $PORT"
RUST_LOG=warn "$WALGIT" --config "$TMP/walgit.toml" serve > "$TMP/server.log" 2>&1 &
PIDS+=($!)
for _ in $(seq 50); do curl -sf "$BASE/healthz" >/dev/null 2>&1 && break; sleep 0.2; done
curl -sf "$BASE/healthz" >/dev/null || { tail -20 "$TMP/server.log"; fail "server not up"; }

REPO="o/r"
curl -sf -X PUT "$BASE/$REPO.git" -o /dev/null
SRC="$TMP/src"; mkdir -p "$SRC"
git -C "$SRC" init -q -b main
git -C "$SRC" config user.email t@t; git -C "$SRC" config user.name T
for i in 1 2 3; do echo "blob $i $(head -c 2000 /dev/urandom | base64)" > "$SRC/f$i.txt"; git -C "$SRC" add .; git -C "$SRC" commit -qm "c$i"; done
git -C "$SRC" push -q "$BASE/$REPO.git" main
ok "repo with 3 commits pushed"

op() { # $1 op, $2 query → waits, prints summary
  local out; out="$(curl -sf -X POST -H 'Accept: application/json' "$BASE/$REPO/api/ops/$1?$2" 2>&1 | tr -d '\n')"
  # The task record: `"ok":true` when it finished well; anything else is shown.
  printf '%s\n' "$out" | grep -qE '"ok": ?true' || { printf '%s\n' "$out" | tail -c 600; echo; tail -20 "$TMP/server.log"; fail "op $1 $2"; }
}
step "base rebuild (base + D18 history pack), then the two fulls (composes)"
op compact "base=1&force=1"
op bundle "strategy=weekly&slot=$(date -u -d 'last sunday 23:00' +%s)"
op bundle "strategy=weekly-history&slot=$(date -u -d 'last sunday 23:00' +%s)"
echo "blob 4 $(head -c 2000 /dev/urandom | base64)" > "$SRC/f4.txt"; git -C "$SRC" add .; git -C "$SRC" commit -qm c4; git -C "$SRC" push -q "$BASE/$REPO.git" main
op bundle "strategy=hourly"
op bundle "strategy=hourly-history"
LIST="$(curl -sf "$BASE/$REPO.git/bundles/list")"
printf '%s\n' "$LIST" | grep -q 'bundle "weekly-' && printf '%s\n' "$LIST" | grep -q 'bundle "weekly-history-' && printf '%s\n' "$LIST" | grep -q 'filter = blob:none' || { printf '%s\n' "$LIST"; fail "one list with both families"; }
ok "ONE list: $(printf '%s\n' "$LIST" | grep -c '^\[bundle "') bundles, $(printf '%s\n' "$LIST" | grep -c 'filter = blob:none') with filter = blob:none"

promisors() { find "$1/.git/objects/pack" -name '*.promisor' | wc -l; }
missing()   { git -C "$1" rev-list --objects --all --missing=print 2>/dev/null | grep -c '^?' || true; }

step "stock git ($($STOCK_GIT --version)): full clone with bundle-uri"
env -u GIT_EXEC_PATH -u GIT_TEMPLATE_DIR "$STOCK_GIT" -c transfer.bundleURI=true clone -q "$BASE/$REPO.git" "$TMP/stock-full" 2>"$TMP/stock.err" || true
echo "  promisor packs: $(promisors "$TMP/stock-full")  missing objects: $(missing "$TMP/stock-full")"
[[ "$(promisors "$TMP/stock-full")" -gt 0 ]] && ok "stock git swallowed the blobless bundles into a FULL clone (the defect; why the families are on separate lists today)" || echo "  (stock git did not take the filtered bundles — fine, nothing to prove here)"

step "patched git ($($PATCHED_GIT --version)): full clone"
"$PATCHED_GIT" -c transfer.bundleURI=true clone -q "$BASE/$REPO.git" "$TMP/p-full"
[[ "$(promisors "$TMP/p-full")" == 0 ]] || fail "full clone has promisor packs"
[[ "$(missing "$TMP/p-full")" == 0 ]] || fail "full clone is missing objects"
"$PATCHED_GIT" -C "$TMP/p-full" fsck --connectivity-only 2>&1 | { grep -v "^Checking" || true; } | sed 's/^/  fsck: /'
"$PATCHED_GIT" -C "$TMP/p-full" fsck --connectivity-only >/dev/null 2>&1 || fail "fsck"
grep -q 'bundle' "$TMP/p-full/.git/config" && ok "full clone: took the unfiltered bundles only ($(grep -c . <<<"$(ls "$TMP/p-full/.git/objects/pack/"*.pack)") packs, 0 promisor, fsck clean, fetch.bundleURI recorded)" || fail "no bundle config recorded — bundles not used?"

step "patched git: --filter=blob:none clone"
GIT_TRACE2_EVENT="$TMP/trace.json" "$PATCHED_GIT" -c transfer.bundleURI=true clone -q --filter=blob:none --no-checkout "$BASE/$REPO.git" "$TMP/p-blobless"
[[ "$(promisors "$TMP/p-blobless")" -ge 1 ]] || fail "blobless clone has no promisor pack from bundles"
[[ "$(missing "$TMP/p-blobless")" -ge 1 ]] || fail "blobless clone is not missing any blob (took the full bundles?)"
{ grep -o '"filter-skip"[^}]*' "$TMP/trace.json" || true; } | head -3 | sed 's/^/  trace2: /'
"$PATCHED_GIT" -C "$TMP/p-blobless" checkout -q main
cmp -s "$TMP/p-blobless/f4.txt" "$SRC/f4.txt" || fail "lazy blob fetch on checkout"
ok "blobless clone: history bundles only ($(promisors "$TMP/p-blobless") promisor packs, blobs lazy on checkout)"

step "patched git: stale blobless fetch takes only newer history bundles"
echo "blob 5 $(head -c 2000 /dev/urandom | base64)" > "$SRC/f5.txt"; git -C "$SRC" add .; git -C "$SRC" commit -qm c5; git -C "$SRC" push -q "$BASE/$REPO.git" main
op bundle "strategy=hourly"
op bundle "strategy=hourly-history"
before="$(promisors "$TMP/p-blobless")"
"$PATCHED_GIT" -C "$TMP/p-blobless" fetch -q origin
[[ "$(promisors "$TMP/p-blobless")" -gt "$before" ]] || fail "stale fetch did not unbundle the new history incremental"
"$PATCHED_GIT" -C "$TMP/p-blobless" fsck --connectivity-only 2>&1 | { grep -v "^Checking" || true; } | head -5 | sed 's/^/  fsck: /'
[[ "$(missing "$TMP/p-blobless")" -ge 1 ]] && echo "  missing (blobs, expected in a blobless clone): $(missing "$TMP/p-blobless")"
"$PATCHED_GIT" -C "$TMP/p-blobless" rev-parse --verify -q origin/main >/dev/null || fail "origin/main after fetch"
"$PATCHED_GIT" -C "$TMP/p-blobless" checkout -q main && "$PATCHED_GIT" -C "$TMP/p-blobless" merge -q --ff-only origin/main && cmp -s "$TMP/p-blobless/f5.txt" "$SRC/f5.txt" || fail "fast-forward + lazy blob after the stale fetch"
ok "stale fetch: +$(( $(promisors "$TMP/p-blobless") - before )) promisor pack(s) from the history family"
printf '\033[32m  git-bundle-filter: PASS\033[0m\n'
