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

## 5. Endpoint coverage

Every route the facade answers, and what stands behind it. Anything under `/api/v3` that is not in this
table is a GitHub-shaped 404, never a fall-through to walgit's repo-prefix dispatcher.

| Surface | Endpoints | Behind it |
|---|---|---|
| Identity | `GET /api/v3/user`, `…/users/{login}`, `…/user/installations`, `…/app`, `…/app/installations`, `GET\|DELETE …/app/installations/{id}`, `POST …/app/installations/{id}/access_tokens`, `…/installation/repositories`, `…/rate_limit`, `DELETE …/applications/{client_id}/{grant,token}`, `…/repos/{o}/{r}/collaborators/{u}/permission` | `auth.rs`: one hardcoded user, every permission `admin` |
| OAuth | `GET /login/oauth/authorize`, `POST /login/oauth/access_token` | `auth.rs`: the web flow agrees immediately |
| Ref reads | `GET …/repos/{o}/{r}`, `…/commits` (`?sha=`, `?path=`), `…/commits/{ref}`, `…/git/commits/{sha}`, `…/branches`, `…/branches/{branch}`, `…/git/ref/{ref}`, `…/git/refs[/{ref}]`, `…/git/matching-refs/{ref}` | `repo.rs` on the ref snapshot; listings carry `Link` (`next`/`prev`/`first`, never `last`) |
| Object reads | `GET …/git/trees/{sha}` (`?recursive=`), `…/git/blobs/{sha}`, `…/contents[/{path}]` (`?ref=`), `…/compare/{base}...{head}`, `…/readme` (`?ref=`), `…/zipball[/{ref}]`, `…/tarball[/{ref}]` | `reads.rs`, `contents.rs`, `compare.rs`, `diff.rs` — stock `git` on the **bare** serving copy |
| Branch protection | `GET …/rules/branches/{branch}`, `…/branches/{branch}/protection`, `PUT …/_dev/repos/{o}/{r}/protection` | `reads.rs` over one `github/protection.json` per repository (§6a) |
| Ref writes | `POST …/git/refs`, `PATCH\|DELETE …/git/refs/{ref}` | `write.rs`, walgit's publish path (§7) |
| Pull requests | `POST\|GET …/pulls`, `GET\|PATCH …/pulls/{n}`, `…/pulls/{n}/{files,commits,merge,reviews,comments,requested_reviewers}`, `…/pulls/{n}/reviews/{id}[/events]`, `GET\|POST …/issues/{n}/comments`, `GET\|PATCH\|DELETE …/issues/comments/{id}`, reactions on both comment kinds, `GET …/issues/{n}`, `GET …/commits/{sha}/pulls`, `POST …/merges`, `POST /repos/{t}/{t}/generate`, `GET /search/issues` | `prs.rs` + `pr_store.rs` + `merge.rs` (§9) |
| Accept-and-forget | `POST …/check-runs`, `GET\|PATCH …/check-runs/{id}`, `GET …/commits/{ref}/check-runs`, `POST\|GET …/deployments`, `POST\|GET …/deployments/{id}/statuses`, `POST …/statuses/{sha}`, `GET …/commits/{ref}/{status,statuses}` | `stubs.rs`: a bounded in-memory map, forgotten on restart — every one is a write whose response is read once, for an `id` |
| GraphQL | `POST /api/graphql`, `POST /api/v3/graphql` | `graphql/`, dispatched on field names (§10) |

Two routes are catch-alls carrying a dispatcher, because matchit refuses a path parameter beside a
catch-all and both a branch name and a ref may contain `/`:

- `commits/{*ref}` → `prs::commit_or_subroute`, which peels `/pulls`, `/check-runs`, `/status` and
  `/statuses` off the tail and otherwise answers the commit shape.
- `branches/{*branch}` → `reads::get_branch`, which peels `/protection` off the tail.

## 6. Module map (`crates/walgit-server/src/github/`)

| File | Holds |
|---|---|
| `mod.rs` | The module doc and the re-export of `router`. |
| `router.rs` | Every route, the rate-limit header layer, the `/api/v3` catch-all 404, the ref-write handlers. |
| `auth.rs` | The hardcoded user, the installation/token stubs, the OAuth web flow. |
| `models.rs` | GitHub JSON shapes; `Urls` (origin → `api`/`html`); stable 48-bit `id`s and `node_id`s derived from names. |
| `repo.rs` | `{owner}/{repo}` → a synced `RepoHandle`, the ref index, ref resolution, the repository/commit/branch/ref handlers — and the git plumbing every other module shares: `git`, `LOG_FORMAT`, `parse_commits`, `commit_facts`, `merge_base`, `commit_count`, `commits_between`. |
| `write.rs` | The write primitive, and `Scratch` — the scratch object directory `merge.rs` builds on too. |
| `prs.rs` | Pull requests, reviews, comments, reactions, issues and search. |
| `pr_store.rs` | PR state as JSON in the bucket, written with CAS. |
| `merge.rs` | `git merge-tree` merges (merge, squash, rebase), publishing, and `generate`. |
| `stubs.rs` | Check runs, deployments and statuses (accept-and-forget). |
| `error.rs` | `{message, documentation_url}` (+ `errors[]` on a 422) with GitHub's statuses. |
| `events.rs` | The hook point where a webhook would be produced. |
| `reads.rs` | Trees, blobs, the README, archives, branch protection, and `ls_tree`/`base64_github`/`wants_raw`/`resolve_ref`. |
| `contents.rs` | `contents/{path}` in its four representations, plus `entry_type`/`entry_json`/`file_json`/`file_response`. |
| `compare.rs` | Three-dot compare. |
| `diff.rs` | The only `files[]` renderer: `FileChange`, `changed_files`, `stats`, `file_json`, all `git diff-tree` on the bare copy. Compare and `pulls/{n}/files` are the same three passes. |
| `graphql/` | `POST /graphql`: `parse.rs` (document → field tree), `ops.rs` (queries), `mutate.rs` (mutations), `blame.rs`, `error.rs` (GitHub's error `type`s). PR state is `pr_store.rs`'s, not a second copy. |

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

## 8. Known limits (one list, all three phases)

- **Object reads need the packs locally.** `repo::objects_view` refuses with **503** when
  `sync_objects()` hands back `ObjectAccess::Remote` — the facade renders through stock `git` against the
  local copy and has no remote-reader path (`web/objects.rs` has one for the web API; wiring it in is a
  later phase). Fine for a developer's repositories, wrong for a monorepo.
- **No worktree anywhere, and none needed.** `diff-tree`, `ls-tree`, `cat-file` and `archive` all take
  tree-ishes, and `git merge-tree --write-tree` merges two trees without a checkout, so the whole read
  surface and every merge run on the bare serving copy out of `write::Scratch`'s scratch object
  directory (§9.2).
- **`files` and `stats` are omitted from a commit response.** `compare` and `…/pulls/{n}/files` have
  them (`diff::changed_files`); wiring the same call into `commits/{ref}` is a one-liner nobody has
  needed yet.
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

Reads:

- **An archive is a 200, not a 302.** GitHub redirects `zipball`/`tarball` to codeload; walgit streams
  `git archive` on the first response instead. Both callers read `response.data` as an `ArrayBuffer` and
  the tarball caller follows redirects, so a body on the first response is what they end up with either
  way — and it saves inventing a second origin that has to be reachable.
- **A branch literally named `<x>/protection` is unreachable.** `branches/{*branch}` is a catch-all
  (branch names contain `/`), so `/protection` is dispatched off the tail inside the handler; a branch
  whose own name ends that way is read as the protection sub-route.
- **A compare cursor is a page of an offset, not a snapshot.** Pagination follows GitHub's caps rather
  than its `per_page` default: ≤ 250 commits and ≤ 300 files, and *no* `per_page` means both caps.

Pull requests:

- **`POST /generate` copies the template's default branch only.** `include_all_branches` is accepted and
  ignored.
- **A squash commit's body is the PR body**, not the concatenation of the squashed commits' messages,
  which is what GitHub composes.
- **A PR's index row and its object are two puts.** The object lands first and the row is refreshed
  after, so a crash between them leaves a row one edit stale. The object is the truth; every read that
  needs precision reads it, and the next write repairs the row.
- **There are no forks**, so `head.repo` is never `null` and an `owner:branch` filter ignores the owner.
- **`GET /pulls/{n}` computes `mergeable` with a real `merge-tree` on every read.** Correct, and one
  subprocess per read.

GraphQL:

- **Every addition is committed at mode `100644`** — an executable file rewritten through the editor
  loses its bit. `createCommitOnBranch` carries no mode and the facade invents none.
- **A `repositories`/`refs`/`search` cursor is an offset, not a snapshot**, so a page taken across a
  concurrent push can skip or repeat a row.
- **`search` matches the free terms against the repository *name* only.**
- **`repositoryOwner` resolves the owner's whole listing before paging it** — a developer's bucket, not
  a monorepo host's.

## 9. Pull requests

### 9.1 Storage layout

PR state is the one thing the facade owns that git does not, and it lives in the same bucket as
everything else — **S3 is the only dependency the PR flow adds**. Two key shapes, both relative to
the repository's own prefix (`repos/{owner}/{repo}/`, the prefix `RepoHandle::store()` is already
scoped to):

| Key | Holds |
|---|---|
| `github/prs/index.json` | `{next_number, prs: [row]}` — the listing, and the PR-number allocator. |
| `github/prs/<n>.json` | One PR, and the unit of CAS: title, body, state, draft, base/head, comments, reviews, review comments, review threads. |

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

Comment, review and reaction ids come from a counter in the PR JSON, offset by the PR number
(`number * 1_000_000 + k`), so an id names the PR it belongs to and
`PATCH /issues/comments/{id}` needs no scan.

**Node ids.** GitHub's are opaque base64 and no client parses one, so the facade's are base64 of a
readable body and round-trip exactly. `pr_store::NodeId` mints and parses all three:

| Kind | Body | Example |
|---|---|---|
| pull request | `PR_<owner>/<repo>#<number>` | `PR_acme/docs#412` → `UFJfYWNtZS9kb2NzIzQxMg==` |
| review | `PRR_<owner>/<repo>#<number>#<review id>` | `PRR_acme/docs#412#412000001` |
| review thread | `PRRT_<owner>/<repo>#<number>#<ordinal>` | `PRRT_acme/docs#412#1` |

The second is the seam between the two surfaces: `POST /pulls/{n}/reviews` mints it, and GraphQL's
`addPullRequestReviewThread` is handed exactly that id and resolves the pull request out of it. It
carries the PR number for that reason — an id naming only the review would be unresolvable. The
mutation answers the third; `markPullRequestReadyForReview` and `convertPullRequestToDraft` take the
first. `github_graphql.rs` walks the whole seam in one test.

One PR object, in full:

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
  "closed_at": null,
  "merged": false,
  "merged_at": null,
  "merge_commit_sha": null,
  "html_url": "http://127.0.0.1:8080/acme/docs/pull/412",
  "labels": [],
  "comments": [],
  "reviews": [],
  "review_comments": [],
  "review_threads": [],
  "maintainer_can_modify": true,
  "next_id": 412000001
}
```

Every field added since is `#[serde(default)]`, so an object written by an older build still parses.
`pr_store::PullRequest` is the one definition of this shape — the GraphQL arms read and write the same
struct, so a field is added once or not at all.

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

Known limits are in §8 with everything else's.

## 11. Testing

`crates/walgit-server/tests/github_prs.rs` covers the PR flow: open/duplicate/no-commits-between,
list and filter, files with a rename, merge/squash/rebase each verified by a real `git fetch` of the
base, a conflict (405) and a stale head (409), comments/reviews/reactions, `commits/{sha}/pulls`,
`POST /merges`, template generation, concurrent check-run PATCHes, and a second instance over the
same bucket reading a PR the first one wrote.

`crates/walgit-server/tests/github_reads.rs` covers the read surface: trees in both modes, blobs as JSON
and as raw bytes, `contents` as a directory / a file / raw / `object+json` / 404, compare (ahead with a
rename and a binary file, diverged, identical, `owner:branch`), the README in both representations, a
zipball whose listing is checked with `unzip -l` and a gzip-magic tarball, and the protection toggle
end to end.

`crates/walgit-server/tests/github.rs` boots a server on the in-memory store, pushes a real repository with
real `git`, then exercises the auth stubs, the OAuth flow, every read shape, ref create/update/delete, a
write through `github::write` verified by a real `git fetch`, and the two fail-closed paths (facade absent
when disabled, `validate` refusing a public bind). `tests/github_graphql.rs` runs every document in
`docs/GITHUB_SHAPES.md` §GraphQL verbatim against a pushed repository — including `createCommitOnBranch`,
whose commit is fetched back with `git` and read out of the tree. Both are in `just test`.

`scripts/github-smoke.mjs` is the client-side check: the real `octokit` package (App JWT →
installation token, REST, GraphQL) against a running facade. Point it at a server with
`mintlify/editor-e2e` pushed in (edit `owner`/`repo`/`baseUrl` at the top otherwise), run it from a
directory whose `node_modules` has `octokit` (`ln -s <app>/node_modules`), and it walks 28 steps:
reads, `createCommitOnBranch` including the stale-head error, compare, a PR opened, reviewed, commented
on and merged, `commits/{sha}/pulls`, and a check-run created then patched.
