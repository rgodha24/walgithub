# The GitHub Enterprise Server facade

**Audience:** anyone changing `crates/walgit-server/src/github/*`, and anyone pointing a GitHub-integrated
application at walgit for local development. Decision of record: **D42** (AGENTS.md §4).

walgit answers a subset of the GitHub Enterprise Server REST API over its own repositories. An application
that already supports GitHub Enterprise Server — one that stores a `baseUrl` per install and constructs its
octokit with it — needs no new code to develop against walgit: point that `baseUrl` at
`http://<walgit-host>/api/v3` and it reads real refs, commits and objects out of the bucket, and its writes
go through walgit's publish path.

It exists for exactly one workflow: a developer running walgit and their application on one machine. It is
**never deployed**. There are no rate limits, no GitHub App to register, no webhook secrets and — the point
below — no authentication at all.

## 1. Trust boundary: there isn't one

Every route under `/api/v3`, `/api/graphql` and `/login/oauth` **bypasses `server.auth`**:

- any bearer, JWT, installation token or nothing at all is accepted, and resolves to one hardcoded user
  (`login = mintlify-dev`, `id = 1`);
- `…/collaborators/{u}/permission` always answers `admin`, for every user and every repository;
- `POST /api/v3/app/installations/{id}/access_tokens` hands out the constant `ghs_dev`;
- `GET /login/oauth/authorize` redirects straight back with `code=dev` — there is no consent form because
  there is nothing to consent to;
- the facade is admin on every repository in the bucket, including delete-a-ref.

So the facade is only as safe as the network it is on. `Config::validate` fails closed the way `auth.mode =
none` does: `github.enabled = true` is refused unless `server.auth.mode = "none"` (itself loopback-only) or
`server.listen` is a loopback address.

## 2. Configuration

```toml
[github]
enabled = true          # default false; docs/GITHUB.md

[server]
listen = "127.0.0.1:8080"

[server.auth]
mode = "none"

[server.tls]
mode = "off"            # see below
```

**TLS must be off.** `walgit.standalone.toml` uses `mode = "self_signed"` (D39), and Node will not trust a
self-signed CA without `NODE_EXTRA_CA_CERTS` pointing at `/services/public/ca.pem` — octokit's fetch fails
with `SELF_SIGNED_CERT_IN_CHAIN` before any of this code runs. `mode = "off"` (the default) serves plain
HTTP/1.1 + h2c, which is what the facade is documented and tested against. If you want TLS anyway, fetch
`http(s)://<host>/services/public/ca.pem` and export `NODE_EXTRA_CA_CERTS`.

`server.public_url` is what every URL in a response is built from (`html_url`, `clone_url`, `url`). Set it
to the origin the application will actually use, or leave it unset and the request's `Host` is used.

## 3. Pointing an application at it

```
1. walgit-server --config walgit.standalone.toml     # with [github] enabled and tls off
2. git remote add walgit http://127.0.0.1:8080/acme/docs.git
3. git push walgit main                              # this is how repositories are created
4. in the application's database, set the install's baseUrl to http://127.0.0.1:8080/api/v3
```

Repositories are seeded by a plain `git push` to walgit's existing smart HTTP and by nothing else — there is
no `POST /user/repos` here. `server.auto_create_on_push = true` makes the first push create the repository.

## 4. URL conventions

These are dictated by the clients, not chosen here.

| Client | Emits | Mounted at |
|---|---|---|
| `@octokit/rest` with `baseUrl = <origin>/api/v3` | `<origin>/api/v3/…` | `/api/v3/*` |
| `@octokit/graphql` (rewrites a `/api/v3` base) | `<origin>/api/graphql` | `/api/graphql` |
| a hand-rolled client doing `${baseUrl}/graphql` | `<origin>/api/v3/graphql` | `/api/v3/graphql` |
| `@octokit/oauth-methods` (strips `/api/v3`) | `<origin>/login/oauth/{authorize,access_token}` | `/login/oauth/*` |

Both GraphQL paths route to one handler. Anything under `/api/v3` that is not routed answers a
GitHub-shaped 404 rather than falling through to walgit's repo-prefix dispatcher (D26), which would
otherwise read `api/v3` as `owner = api`, `repo = v3`.

Every response carries `x-ratelimit-{limit,remaining,used,reset,resource}` and `x-github-request-id`;
clients read those headers
rather than calling `/rate_limit`, though `GET /api/v3/rate_limit` answers too.

## 5. What is implemented

**Identity and installation** — `GET /api/v3/user`, `GET /api/v3/users/{login}`,
`GET /api/v3/user/installations`, `GET /api/v3/app`, `GET /api/v3/app/installations`,
`GET|DELETE /api/v3/app/installations/{id}`, `POST /api/v3/app/installations/{id}/access_tokens`,
`GET /api/v3/installation/repositories`, `GET /api/v3/rate_limit`,
`DELETE /api/v3/applications/{client_id}/{grant,token}`,
`GET /api/v3/repos/{o}/{r}/collaborators/{u}/permission`, `GET /login/oauth/authorize`,
`POST /login/oauth/access_token`.

**Reads** — `GET /api/v3/repos/{o}/{r}`, `…/commits`, `…/commits/{ref}` (branch, tag or sha),
`…/git/commits/{sha}`, `…/branches`, `…/branches/{branch}`, `…/git/ref/{ref}`, `…/git/refs[/{ref}]`,
`…/git/matching-refs/{ref}`. Listings carry a `Link` header (`next`/`prev`/`first`; no `last`, which would
mean walking a whole history).

**Writes** — `POST /api/v3/repos/{o}/{r}/git/refs`, `PATCH` and `DELETE` on `…/git/refs/{ref}`, all on the
write primitive below.

**Pull requests** (§10) — `POST|GET /repos/{o}/{r}/pulls`, `GET|PATCH …/pulls/{n}`,
`…/pulls/{n}/{files,commits,merge,reviews,comments,requested_reviewers}`,
`…/pulls/{n}/reviews/{id}[/events]`, `GET|POST /repos/{o}/{r}/issues/{n}/comments`,
`GET|PATCH|DELETE …/issues/comments/{id}`, reactions on both comment kinds,
`GET …/issues/{n}`, `GET …/commits/{sha}/pulls`, `POST …/merges`,
`POST /repos/{t}/{t}/generate`, `GET /search/issues`.

**Accept-and-forget** — `POST …/check-runs`, `GET|PATCH …/check-runs/{id}`,
`GET …/commits/{ref}/check-runs`, `POST|GET …/deployments`,
`POST|GET …/deployments/{id}/statuses`, `POST …/statuses/{sha}`,
`GET …/commits/{ref}/{status,statuses}`, `GET …/rules/branches/{branch}`. These keep a bounded
in-memory map and nothing else: every one of them is a write whose response is read once, for an
`id`, and then only written back to.

**GraphQL** — parsed, not dispatched. The operation's name and first top-level field are extracted with
`graphql-parser` and returned as `{"errors":[{"message":"not implemented: <op>.<field>"}]}`, so a client's
failure names the gap instead of a transport error. The next phase fills in arms; the parse is already there.

## 6. Module map (`crates/walgit-server/src/github/`)

| File | Holds |
|---|---|
| `mod.rs` | The module doc and the re-export of `router`. |
| `router.rs` | Every route, the rate-limit header layer, the `/api/v3` catch-all 404, the ref-write handlers, the GraphQL stub. |
| `auth.rs` | The hardcoded user, the installation/token stubs, the OAuth web flow. |
| `models.rs` | GitHub JSON shapes; `Urls` (origin → `api`/`html`); stable 48-bit `id`s and `node_id`s derived from names. |
| `repo.rs` | `{owner}/{repo}` → a synced `RepoHandle`, the ref index, ref resolution, and the read handlers. |
| `write.rs` | The write primitive. |
| `prs.rs` | Pull requests, reviews, comments, reactions, issues and search. |
| `pr_store.rs` | PR state as JSON in the bucket, written with CAS. |
| `merge.rs` | `git merge-tree` merges, the PR diff plumbing, `generate`. |
| `stubs.rs` | Check runs, deployments and statuses (accept-and-forget). |
| `error.rs` | `{message, documentation_url}` (+ `errors[]` on a 422) with GitHub's statuses. |
| `events.rs` | The hook point where a webhook would be produced. |

Later phases slot in beside these: a `diff.rs` (compare/contents/trees) and dispatch arms inside
`router::graphql`.

## 7. The write primitive

`github::write` is the only place the facade mutates anything, and it goes through walgit's real publish
path — the bucket is the truth and another instance sees the result on its next revalidation. The mechanism
is receive-pack's, minus the wire:

1. `RepoHandle::sync()` — Serve level, so the base commit's objects are readable and no pack is removed
   while we build on them.
2. Build the objects with `git` plumbing in a **scratch object directory**: `GIT_OBJECT_DIRECTORY` in a
   tempdir, `GIT_ALTERNATE_OBJECT_DIRECTORIES` at the repository's own `objects/`, `GIT_INDEX_FILE` beside
   it. `read-tree` → `hash-object -w` / `update-index` per change → `write-tree` → `commit-tree`. Nothing
   touches the serving copy, so a write refused later leaves nothing behind — the same property
   receive-pack gets from its per-ingest scratch git dir.
3. `git pack-objects --revs --stdout` over `<new> ^<base>`: a self-contained pack of exactly the new
   objects.
4. `LocalRepo::ingest_pack` indexes it into the serving copy, `check_connectivity_async` proves the tip.
5. `RepoHandle::publish_push_synced` — pack PUT ∥ log PUT → manifest CAS.

Ref create, fast-forward update, force update and delete are the same call with no pack.

Semantics worth knowing:

- **Expected head.** `CommitOnRef::expected_head` (defaulting to `base`) is checked before the build and
  again by the WAL's `verify_txn` at CAS time. A ref that moved is a **409**.
- **Fast-forward.** The WAL never enforces fast-forward — that is receive-pack's job — so `update_ref` does
  the ancestry check itself with `git merge-base --is-ancestor` and answers **422 Validation Failed**
  unless `force`.
- **Zero oids.** Every `old_oid` handed to the WAL is hex or `""`; forty zeros would be read as an oid.
  This matches the normalisation `smart.rs` does on a pushed transaction.
- **No blocking on a tokio worker.** Every `git` call is `tokio::process`; the tempdir is created under
  `spawn_blocking`.

## 8. Known limits (read this before the next phase)

- **Object reads need the packs locally.** `repo::objects_view` refuses with **503** when
  `sync_objects()` hands back `ObjectAccess::Remote` — the facade renders through stock `git` against the
  local copy and has no remote-reader path (`web/objects.rs` has one for the web API; wiring it in is a
  later phase). Fine for a developer's repositories, wrong for a monorepo.
- **No worktree, and none needed.** `git merge-tree --write-tree` merges two trees without one, and
  `git diff <a> <b>` runs fine against a bare repository, so `merge.rs` works out of the same scratch
  object directory `write.rs` uses rather than a checkout (§9.2).
- **`files` and `stats` are omitted from a commit response.** A PR's diff is served by
  `…/pulls/{n}/files`; the per-commit one waits for `diff.rs`.
- **Ref names.** `write::validate_ref_name` accepts `refs/…` only, with no `..`, `//`, space, control byte
  or `~^:?*[\`. The path segments a URL wildcard delivers are already decoded by axum, so a branch with a
  `/` works; a branch with a `%` in it has never been tried.
- **Ids are derived, not stored.** A repository's numeric `id` is the first 48 bits of the sha1 of
  `owner/name`. Stable across restarts and instances, but it is not GitHub's id, and two facades over
  different buckets will agree on it.
- **`created_at` is a constant.** The WAL has no repository creation time; `pushed_at`/`updated_at` come
  from the manifest's `updated_at`, which is real.
- **The facade produces no events.** `events.rs` is a hook point only. Principle III: a webhook must be a
  reader of the WAL from a durable cursor (`crate::bridge`, `docs/EVENTS.md`), never a step of a write.

## 9. Pull requests

### 9.1 Storage layout

PR state is the one thing the facade owns that git does not, and it lives in the same bucket as
everything else — **S3 is the only dependency the PR flow adds**. Two key shapes, both relative to
the repository's own prefix (`repos/{owner}/{repo}/`, the prefix `RepoHandle::store()` is already
scoped to):

| Key | Holds |
|---|---|
| `github/prs/index.json` | `{next_number, prs: [row]}` — the listing. |
| `github/prs/<n>.json` | One PR: title, body, state, draft, base/head, comments, reviews, review comments, review threads. |

A row is `{number, state, draft, merged, head_ref, base_ref, head_sha, merge_commit_sha,
created_at, updated_at}` — everything `GET /pulls` filters or sorts on, and everything
`GET /commits/{sha}/pulls` matches on, so **no request path ever issues a `LIST`**: a listing is one
GET of the index plus one GET per PR actually rendered.

Both objects are written with compare-and-swap — `PutMode::Create` for a new object,
`PutMode::Update(version)` on the version last read — and a `PreconditionFailed` re-reads and retries
a bounded number of times. Creating a PR bumps `next_number` in the index *first*: the number is the
thing two writers race for, and the loser reads the bumped counter rather than overwriting a PR.
Editing one writes the object and then refreshes its row, which is two puts and therefore not atomic
— a crash between them leaves a row one edit stale, and every read that needs precision reads the
object. The object is the truth; the index is a cache of it.

`node_id` is base64 of `PR_<owner>/<repo>#<n>` (so `PR_acme/docs#412` →
`UFJfYWNtZS9kb2NzIzQxMg==`), stable across restarts and instances the way every other id here is.
Comment, review and reaction ids come from a counter in the PR JSON, offset by the PR number
(`number * 1_000_000 + k`), so an id names the PR it belongs to and
`PATCH /issues/comments/{id}` needs no scan.

`head.sha` is **not** authoritative while a PR is open: it is re-resolved from `refs/heads/<head>`
on every read, so a PR always reports the branch's current tip. It freezes at the value the branch
had when the PR was merged or closed.

### 9.2 Merging

`PUT /pulls/{n}/merge` and `POST /merges` share one path in `merge.rs`, and it is a real merge:

1. `git merge-tree --write-tree --messages <base> <head>` in a scratch object directory
   (`GIT_OBJECT_DIRECTORY` in a tempdir, `GIT_ALTERNATE_OBJECT_DIRECTORIES` at the serving copy's
   `objects/`) — no worktree, and nothing written near the serving copy. Exit 1 is a conflict.
2. `commit-tree` on the merged tree: two parents for `merge`, one for `squash`. `rebase` replays
   `base..head` one commit at a time, each step a `merge-tree --merge-base=<c^> <onto> <c>`, which
   is what `git rebase` does.
3. `pack-objects --revs --stdout` over `<new> ^<base> ^<head>` → `LocalRepo::ingest_pack` →
   `check_connectivity_async` → `RepoHandle::publish_push_synced`. Exactly `write.rs`'s sequence;
   `write.rs` builds its tree from file changes, this one from a tree that already exists.

Statuses a client branches on: **405 `Pull Request is not mergeable`** on a conflict (GitHub uses
405, not 409, for this), **409 `Head branch was modified. Review and try the merge again.`** when
the request's `sha` is not the head's current tip, **204** from `POST /merges` when the head is
already an ancestor of the base, and **422** with `A pull request already exists` / `No commits
between ` inside `errors[].message` from `POST /pulls`.

The base ref's expected old value travels into the WAL transaction, so a base that moves between the
merge-tree and the publish is a conflict rather than a lost update.

### 9.3 Known limits

- `POST /generate` copies the template's **default branch only**; `include_all_branches` is accepted
  and ignored.
- There are no forks, so `head.repo` is never `null` and `owner:branch` filters ignore the owner.
- `GET /pulls/{n}` computes `mergeable` with a real `merge-tree` on every read. Correct, and one
  subprocess per read.
- `commits/{sha}/pulls`, `commits/{ref}/check-runs` and `commits/{ref}/status` are dispatched off
  the tail of the `commits/{*ref}` route rather than being routes of their own: matchit refuses to
  register a path parameter beside a catch-all.

## 10. Testing

`crates/walgit-server/tests/github_prs.rs` covers the PR flow: open/duplicate/no-commits-between,
list and filter, files with a rename, merge/squash/rebase each verified by a real `git fetch` of the
base, a conflict (405) and a stale head (409), comments/reviews/reactions, `commits/{sha}/pulls`,
`POST /merges`, template generation, concurrent check-run PATCHes, and a second instance over the
same bucket reading a PR the first one wrote.

`crates/walgit-server/tests/github.rs` boots a server on the in-memory store, pushes a real repository with
real `git`, then exercises the auth stubs, the OAuth flow, every read shape, ref create/update/delete, a
write through `github::write` verified by a real `git fetch`, the GraphQL stub, and the two fail-closed
paths (facade absent when disabled, `validate` refusing a public bind). It is in `just test`.
