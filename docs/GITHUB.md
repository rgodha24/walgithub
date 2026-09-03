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

**GraphQL** — `POST /api/graphql` and `POST /api/v3/graphql`, §10 below.

## 6. Module map (`crates/walgit-server/src/github/`)

| File | Holds |
|---|---|
| `mod.rs` | The module doc and the re-export of `router`. |
| `router.rs` | Every route, the rate-limit header layer, the `/api/v3` catch-all 404, the ref-write handlers. |
| `auth.rs` | The hardcoded user, the installation/token stubs, the OAuth web flow. |
| `models.rs` | GitHub JSON shapes; `Urls` (origin → `api`/`html`); stable 48-bit `id`s and `node_id`s derived from names. |
| `repo.rs` | `{owner}/{repo}` → a synced `RepoHandle`, the ref index, ref resolution, and the read handlers. |
| `write.rs` | The write primitive. |
| `error.rs` | `{message, documentation_url}` (+ `errors[]` on a 422) with GitHub's statuses. |
| `events.rs` | The hook point where a webhook would be produced. |
| `graphql/` | `POST /graphql`: `parse.rs` (document → field tree), `ops.rs` (queries), `mutate.rs` (mutations), `blame.rs`, `prs.rs` (PR JSON in the bucket), `error.rs` (GitHub's error `type`s). |

Later phases slot in beside these: a `prs.rs` (the REST pulls endpoints over the JSON `graphql/prs.rs`
already writes) and a `diff.rs` (compare/merge on a scratch checkout).

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
- **No scratch checkout yet.** `git diff` and `git merge` need a worktree, and the serving copy is bare.
  The compare/merge phase should follow the base-rebuild precedent (`crates/walgit-server/src/rebuild.rs`):
  a scratch copy under `<cache.dir>/` that never rewrites the serving copy. `write.rs`'s `Scratch` is the
  smaller version of the same idea and is the place to grow it.
- **`files` and `stats` are omitted from a commit response.** They are a diff, so they wait for the same
  phase.
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

`crates/walgit-server/tests/github.rs` boots a server on the in-memory store, pushes a real repository with
real `git`, then exercises the auth stubs, the OAuth flow, every read shape, ref create/update/delete, a
write through `github::write` verified by a real `git fetch`, and the two fail-closed paths (facade absent
when disabled, `validate` refusing a public bind). `tests/github_graphql.rs` runs every document in
`docs/GITHUB_SHAPES.md` §GraphQL verbatim against a pushed repository — including `createCommitOnBranch`,
whose commit is fetched back with `git` and read out of the tree. Both are in `just test`.

## 10. GraphQL

`POST /api/graphql` and `POST /api/v3/graphql` are one handler, and there is **no GraphQL engine behind
it**. Every document a client sends is a string literal in its own source — `docs/GITHUB_SHAPES.md`
("POST /graphql") lists all of them, their variables and the exact fields destructured off the answer — so
the handler parses the document with `graphql-parser`, takes its single operation, resolves each argument
against the JSON `variables`, and dispatches on field names. A schema, a resolver graph and an executor
would be a large amount of machinery to answer eleven known questions.

Served today:

| Document | Answers with |
|---|---|
| `repository { ref(qualifiedName) { target { … history(first) } } }` | the branch tip, `git log` from it |
| `repository { object(expression: "<rev>:<path>") { … on Blob } }` | `oid`, `byteSize`, `isBinary`, `text` |
| `repository { refs(refPrefix, first, after, query) }` | the branch listing, one page per call |
| `repository { ref { target { blame(path) { ranges } } } }` | `git blame --porcelain` |
| `repositoryOwner(login) { repositories(first, after, orderBy) }` | every repository of that owner in the bucket |
| `search(query, type: REPOSITORY, first)` | the same listing, filtered by the search string |
| `mutation createCommitOnBranch(input)` | a commit through `write::commit_on_ref` |
| `mutation markPullRequestReadyForReview(input)` / `convertPullRequestToDraft(input)` | `draft` on the PR JSON |
| `mutation addPullRequestReviewThread(input)` | a thread appended to the PR JSON |

Conventions, all of them the call sites':

- **Every answer is HTTP 200**, including every error — GitHub's is too, and the client turns a non-empty
  `errors[]` into a `GraphqlResponseError` reading only `errors[0].type` and `errors[0].message`.
- **Error `type`s are GitHub's**, because `createCommitOnBranch` branches on them: `NOT_FOUND` → 404
  (a message containing `Could not resolve to a Repository` gets its own summary), `STALE_DATA` → 412,
  everything else → 500. An `expectedHeadOid` that is not where the branch is answers `UNPROCESSABLE` with
  `Expected branch to point to "<oid>" but it did not.`; a ref that moves between that check and the CAS
  answers `STALE_DATA`. **Neither is an HTTP 409** — over GraphQL a status is not how a client is told.
- **A field that is not served** is `{"data":null,"errors":[{"type":"NOT_IMPLEMENTED","message":"not
  implemented: <operation>.<field path>"}]}`, so a gap names itself.
- **Missing things follow the call site**: a repository that does not resolve is a `NOT_FOUND` error, a
  `ref` that is not there is `null` (the caller falls back to REST), and `object` is `null` for every miss
  (the caller swallows errors to `null`, so a miss must not look like an outage).
- Each arm answers the **full** shape of its node, not only the selected fields — the documents are
  literals that get edited — but selection still decides work: `history` and `blame` are computed only when
  they are asked for.
- `x-ratelimit-remaining` is never `0`: a zero makes the client sleep `retry-after + 1` seconds (61 by
  default) and retry once.

Known limits: a cursor is an offset, not a snapshot, so a page taken across a concurrent push can skip or
repeat a row; `search` matches the free terms against the repository *name* only; every addition is
committed at mode `100644` (an executable file rewritten through the editor loses its bit); and
`repositoryOwner` resolves the owner's whole listing before paging it (a developer's bucket, not a
monorepo host's).

## 11. Pull request state in the bucket

The GraphQL mutations that only flip `draft` or append a review thread need somewhere durable to write
before the REST pulls endpoints exist, and it must be the place those endpoints will read. The layout is
therefore fixed here, under the repository's own prefix (`RepoId::store_prefix()` =
`repos/<owner>/<repo>/`):

- `github/prs/<n>.json` — one pull request. **The unit of CAS**: every mutation is read → modify →
  `PutMode::Update(version)`, retried on a lost race.
- `github/prs/index.json` — `{next_number, numbers: [...]}`, so allocating a number is one CAS and a
  listing is not a LIST.

```json
{
  "number": 412,
  "node_id": "UFJfYWNtZS9kb2NzIzQxMg==",
  "title": "Docs: add quickstart",
  "body": "",
  "state": "open",
  "draft": false,
  "base": { "ref": "main", "sha": "1111111111111111111111111111111111111111" },
  "head": { "ref": "editor/quickstart", "sha": "2222222222222222222222222222222222222222" },
  "user": "mintlify-dev",
  "created_at": "2026-08-30T09:00:00Z",
  "updated_at": "2026-08-30T10:16:00Z",
  "merged": false,
  "merged_at": null,
  "merge_commit_sha": null,
  "html_url": "http://127.0.0.1:8080/acme/docs/pull/412",
  "review_threads": []
}
```

Unknown keys survive a round trip (they are kept in a flattened `extra`), so a REST handler may add
fields — `commits`, `additions`, `changed_files` — without this module dropping them.

**Node ids.** GitHub's are opaque base64 and no client parses one, so the facade's are base64 of a
readable body and round-trip exactly:

| Kind | Body | Example |
|---|---|---|
| pull request | `PR_<owner>/<repo>#<number>` | `PR_acme/docs#412` |
| pending review | `PRR_<owner>/<repo>#<number>#<review id>` | `PRR_acme/docs#412#7` |
| review thread | `PRRT_<owner>/<repo>#<number>#<ordinal>` | `PRRT_acme/docs#412#1` |

`markPullRequestReadyForReview` and `convertPullRequestToDraft` are handed the first;
`addPullRequestReviewThread` is handed the second (the `node_id` of the pending review the client opened
over REST) and answers the third. `POST /pulls/{n}/reviews` must therefore mint its `node_id` this way, or
the mutation cannot find the pull request the review belongs to.
