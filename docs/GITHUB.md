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

So the facade is only as safe as the network it is on, and the configuration says so out loud:
`github.enabled = true` requires `server.auth.mode = "none"`, and with the facade on that mode may bind
beyond loopback (a compose network, say) — the network the process sits on is the trust boundary.

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

**Reads** — `GET /api/v3/repos/{o}/{r}`, `…/commits` (`?sha=`, `?path=`), `…/commits/{ref}` (branch, tag
or sha), `…/git/commits/{sha}`, `…/branches`, `…/branches/{branch}`, `…/git/ref/{ref}`,
`…/git/refs[/{ref}]`, `…/git/matching-refs/{ref}`. Listings carry a `Link` header
(`next`/`prev`/`first`; no `last`, which would mean walking a whole history).

**Object reads** — `…/git/trees/{sha}` (`?recursive=`), `…/git/blobs/{sha}`, `…/contents[/{path}]`
(`?ref=`), `…/compare/{base}...{head}`, `…/readme` (`?ref=`), `…/zipball[/{ref}]`, `…/tarball[/{ref}]`.
Everything renders through stock `git` on the **bare** serving copy — `ls-tree`, `cat-file`, `diff-tree`
and `archive` all work without a work tree, so the scratch checkout §8 predicted was never needed.

**Branch protection** — `…/rules/branches/{branch}` and `…/branches/{branch}/protection`, both driven by
one object per repository in the bucket (below).

**Writes** — `POST /api/v3/repos/{o}/{r}/git/refs`, `PATCH` and `DELETE` on `…/git/refs/{ref}`, all on the
write primitive below.

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
| `error.rs` | `{message, documentation_url}` (+ `errors[]` on a 422) with GitHub's statuses. |
| `events.rs` | The hook point where a webhook would be produced. |
| `reads.rs` | Trees, blobs, the README, archives, branch protection — and the git plumbing (`git`, `ls_tree`, `commit_facts`, `parse_commits`, `base64_github`, `wants_raw`, `resolve_ref`) the other read modules share. |
| `contents.rs` | `contents/{path}` in its four representations, plus `entry_type`/`entry_json`/`file_json`/`file_response`. |
| `compare.rs` | Three-dot compare. |
| `diff.rs` | `FileChange` and `changed_files`/`file_json` — `files[]` rendered by `git diff-tree`. |

Later phases slot in beside these: a `prs.rs` (PR state as JSON under the repository's prefix in the
bucket) and dispatch arms inside `router::graphql`.

## 6a. The read surface in detail

- **Trees.** `{sha}` is a tree sha, a commit sha *or* a ref name, all peeled with `rev-parse ^{tree}`.
  `recursive` is any truthy string. `truncated` is always `false` — there is no cap, and every caller
  reads `true` as either "start a BFS" or "give up". Recursive listings carry repo-relative paths and keep
  the `tree` entries (`ls-tree -r -t`); non-recursive ones carry bare names.
- **Blobs and raw media.** Any `Accept` containing `raw` (`application/vnd.github.raw`, `…raw+json`,
  octokit's `mediaType: {format: "raw"}`) returns the bytes with `Content-Type: application/vnd.github.raw`
  — the bypass client destroys the stream otherwise. `contents` additionally honours
  `application/vnd.github.object+json`. JSON `content` is GitHub's base64: a newline every 60 characters
  and a trailing one.
- **Contents.** A file is an object, a directory is an **array** — `getFileBufferByPath` branches on
  `Array.isArray` and `getContentDirectorySha` refuses a non-array. A blob over 1 MiB answers
  `content: ""`, `encoding: "none"`, which is what drives the client's fallback to `git/blobs/{sha}`.
  An empty repository, a missing path and a missing `ref` are all 404.
- **Compare.** Three-dot: `ahead_by`, `behind_by`, `commits[]` and `files[]` are all measured from the
  merge base, and `merge_base_commit` is a full commit object because its `.sha` is dereferenced with no
  null guard. `base..head` is accepted as well as `base...head`, and an `owner:branch` side has its owner
  stripped (there are no forks here). Unrelated histories fall back to `base` as the merge base rather
  than answering without one. Pagination follows GitHub's caps rather than its `per_page` default:
  ≤ 250 commits and ≤ 300 files per page, and *no* `per_page` means both caps, because `compareRef`
  sends no pagination and still expects the whole file list. A binary file carries no `patch`.
- **Archives.** GitHub 302s to codeload; walgit streams `git archive` on the 200 instead, under the same
  `<repo>-<shortsha>/` prefix the template extractor strips. Both callers read `response.data` as an
  `ArrayBuffer` and the tarball caller sets `redirect: "follow"`, so a body on the first response is what
  they end up with either way — and it saves inventing a second origin that has to be reachable.
- **Branch protection.** GitHub's rulesets API is an order of magnitude more surface than anything reads,
  so the facade stores the answer and renders the rule objects from it. The object is
  `github/protection.json` under the repository's prefix:

  ```json
  { "protected_branches": ["main"], "required_approving_review_count": 0 }
  ```

  Set it with `PUT /api/v3/_dev/repos/{o}/{r}/protection` (a CAS on the object's version; the `_dev`
  prefix is there so no client mistakes it for a GitHub route). A protected branch answers
  `rules/branches/{b}` with `pull_request`, `non_fast_forward` and `deletion` rules and
  `branches/{b}/protection` with the legacy object; an unprotected one answers `[]` and a 404 whose
  message is `Branch not protected`. `branches/{b}` reports `protected` from the same object — a
  hardcoded `false` there would short-circuit `getBranchProtections` before it ever asked for the rules.
  `branches/{*branch}` is a catch-all (branch names contain `/`), so `/protection` is dispatched inside
  that handler rather than by a route of its own; a branch literally named `<x>/protection` is
  unreachable.

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
- **No scratch checkout, and none needed for reads.** `diff-tree`, `ls-tree`, `cat-file` and `archive`
  all take tree-ishes and never look at a work tree, so the whole read surface runs on the bare serving
  copy. `git merge` still needs one; when the merge phase arrives it should follow the base-rebuild
  precedent (`crates/walgit-server/src/rebuild.rs`) — a scratch copy under `<cache.dir>/` that never
  rewrites the serving copy — with `write.rs`'s `Scratch` as the place to grow it.
- **`files` and `stats` are still omitted from a commit response.** `compare` has them
  (`diff::changed_files`); wiring the same call into `commits/{ref}` is a one-liner nobody has needed yet.
- **An archive is not range-requestable and carries no `Content-Length`.** It is streamed straight off
  `git archive`, so a client that retries a partial download starts over.
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

## 9. Testing

`crates/walgit-server/tests/github_reads.rs` covers the read surface: trees in both modes, blobs as JSON
and as raw bytes, `contents` as a directory / a file / raw / `object+json` / 404, compare (ahead with a
rename and a binary file, diverged, identical, `owner:branch`), the README in both representations, a
zipball whose listing is checked with `unzip -l` and a gzip-magic tarball, and the protection toggle
end to end.

`crates/walgit-server/tests/github.rs` boots a server on the in-memory store, pushes a real repository with
real `git`, then exercises the auth stubs, the OAuth flow, every read shape, ref create/update/delete, a
write through `github::write` verified by a real `git fetch`, the GraphQL stub, and the two fail-closed
paths (facade absent when disabled, `validate` refusing a public bind). It is in `just test`.
