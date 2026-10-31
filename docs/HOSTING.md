# Hosting walgit

**Audience:** operators running walgit as a service, not developers running it on a laptop.
`README.md` is the introduction, `walgit.standalone.toml` is the one-box developer shape, and
`docs/GITHUB.md` describes the GitHub Enterprise Server facade — which is auth-free and stays on a
loopback or private network.

A deployment is two services in front of one object store:

| Service | What it is | Default |
|---|---|---|
| **WAL-Git** | git smart HTTP, the JSON API, the web UI, bundles and LFS. Token or OIDC auth. | `127.0.0.1:8080` |
| **WAL-Github** | the GHES facade (`/api/v3`, `/api/graphql`, `/login/oauth`) over the same repositories. Auth-free by construction; loopback only. | `127.0.0.1:8097` |

The bucket is the repository; both services' disks are caches. There is no database, no shared
filesystem, no node identity and no stable IP. You provide the object store; this deployment starts
no S3 service.

The artefacts this page describes:

| File | What it is |
|---|---|
| `Dockerfile` | one image for both services: web UI embedded, git + git-lfs + tini, uid 1000, health-checked |
| `deploy/host.toml` | WAL-Git's config (`/etc/walgit/walgit.toml`); env-overridable |
| `deploy/walgithub.toml` | WAL-Github's config; the same image built with `WALGIT_CONFIG_FILE` + `WALGIT_HTTP_PORT` |
| `deploy/compose.yaml` | the WAL deployment: WAL-Git + WAL-Github, two cache volumes, JSON logs |
| `deploy/walgit.env.example` | the shared environment; copy it to `deploy/.env` |
| `deploy/compose.rustfs.yaml` | an **optional, separate** S3-compatible dev store (`just dev-store`) |
| `deploy/nginx.conf.example` | an optional edge: public TLS, `auth_request`, `X-Accel-Redirect` byte offload |

## 1. Prerequisites

- **An object store bucket.** S3 or S3-compatible (AWS, MinIO, rustfs, R2, Ceph) or GCS. The bucket
  holds every durable byte; size it for all repositories, their bundles and LFS objects. It must exist
  before the first request (`walgit repo create` and the manifest CAS create keys, not buckets).
- **Credentials** that can read, write, delete and list objects under the configured prefix.
  Prefer an instance role / ECS task role / workload identity: the S3 backend uses the AWS SDK chain
  when neither static-key variable is set, and refreshes role credentials automatically (D43). If you
  must use a static pair, set **both** `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`; one alone fails
  closed. For GCS the credentials are Application Default Credentials.
- **Docker Engine 24+ with Compose v2**, or any orchestrator that can run the image with an
  environment and a writable `/var/lib/walgit`.
- **A TLS terminator** unless you deliberately run private HTTP. See §4.

Recommended, not required: bucket versioning (the few objects rewritten in place — `manifest.pb`,
leases, bundle lists, the events cursor — are recoverable from a previous version) and a lifecycle rule
that does not expire `repos/`, `bundles/`, `lfs/` or `events/` objects.

## 2. Build the images

`deploy/compose.yaml` builds both services from one `Dockerfile`:

```sh
docker compose build
# or, to build and start in one step
docker compose up -d --build
```

The build is multi-stage: the Vite SPA and `repos.js` SDK are embedded into the binary, then
`walgit` and `walgit-server` are compiled, and the runtime layer is Debian with git, git-lfs, CA
certificates, tini and curl. The web artifacts are required — the build fails without them.

WAL-Git uses the default build (`deploy/host.toml`, port 8080). WAL-Github is the same build with two
arguments, which Compose passes for you:

```sh
docker build --build-arg WALGIT_CONFIG_FILE=deploy/walgithub.toml \
             --build-arg WALGIT_HTTP_PORT=8097 -t walgithub:latest .
```

`WALGIT_BUILD_SHA` is embedded and shown on `/readyz` and in the UI footer; pass your commit id in CI.

## 3. Start it with Compose

```sh
cp deploy/walgit.env.example deploy/.env
"${EDITOR:-vi}" deploy/.env          # WALGIT_TOKEN, WALGIT__STORE__BUCKET, the two public origins
docker compose up -d --build
docker compose ps
curl -fsS http://127.0.0.1:8080/readyz
curl -fsS http://127.0.0.1:8097/healthz
```

`deploy/.env` is loaded for Compose interpolation and passed to both containers. The services publish on
loopback by default — keep them there and put a proxy in front, or set `WALGIT_BIND` for WAL-Git once
you have a firewall in place. `WALGITHUB_BIND` is separate on purpose: the facade is auth-free. Each service has its own cache volume; remove either at any time:

```sh
docker compose down          # stop; the bucket keeps everything
docker compose down -v       # also lose warmth (both caches only)
```

Use WAL-Git (token auth):

```sh
# the token in deploy/.env is the admin principal's bearer
git -c http.extraHeader="Authorization: Bearer $WALGIT_TOKEN" \
    push http://127.0.0.1:8080/acme/app.git main     # auto_create_on_push creates the repository
```

Use WAL-Github (no auth, local only):

```sh
git clone http://127.0.0.1:8097/acme/app.git
curl http://127.0.0.1:8097/api/v3/repos/acme/app
```

A developer sets her machine up against WAL-Git with the installer (open, no credential needed):

```sh
sh -c "$(curl -fsSL 'https://git.example.com/services/public/install.sh')"
```

It pins the host's CA in self-signed mode, stores the token in `~/.config/git/<host>-token`, installs
the git credential helper and turns on `transfer.bundleURI`.

### The optional S3-compatible dev store

`deploy/compose.rustfs.yaml` is a **separate Compose project** for a local rustfs endpoint. It is not
part of the WAL deployment and starts no service implicitly:

```sh
docker compose -f deploy/compose.rustfs.yaml up -d --wait rustfs
docker compose -f deploy/compose.rustfs.yaml run --rm create-bucket
# or simply: just dev-store
```

Point both WAL services at it in `deploy/.env`:

```dotenv
WALGIT__STORE__S3__ENDPOINT=http://host.docker.internal:9000
WALGIT__STORE__S3__FORCE_PATH_STYLE=true
AWS_ACCESS_KEY_ID=walgit-dev
AWS_SECRET_ACCESS_KEY=walgit-dev-secret
```

Its data is a local volume, not a durable bucket. Do not host repositories you care about on it.
For GCS instead of S3, set `WALGIT__STORE__BACKEND=gcs` and provide Application Default Credentials
(mount a service-account JSON and set `GOOGLE_APPLICATION_CREDENTIALS`); the `[store.gcs]` keys are in
`walgit.example.toml`.

## 4. Configuration

`deploy/host.toml` is WAL-Git's starting point and `deploy/walgithub.toml` is WAL-Github's;
`walgit.example.toml` documents every key with its default. Precedence is **baked config <
`WALGIT__SECTION__KEY` environment** (values are parsed as TOML, falling back to strings). The Compose
file passes `deploy/.env` through and then sets the per-service facts (roles, listener, public URL) in its
`environment:` block.

The two required settings are enforced by the stack:

| Variable | Meaning |
|---|---|
| `WALGIT_TOKEN` | WAL-Git bearer for the `admin` principal (generate with `openssl rand -hex 32`) |
| `WALGIT__STORE__BUCKET` | the bucket that is the repository (shared by both services) |

Important overrides in `deploy/.env`:

| Variable | Default | Notes |
|---|---|---|
| `WALGIT_PUBLIC_URL` | `http://127.0.0.1:8080` | WAL-Git origin; **set it behind a proxy**. bundle-uri, LFS, clone recipes and OAuth callbacks use absolute URLs |
| `WALGITHUB_PUBLIC_URL` | `http://127.0.0.1:8097` | WAL-Github origin; the facade's response URLs are built from it |
| `WALGIT_PORT` / `WALGIT_CONTAINER_PORT` | `8080` | host port / container port for WAL-Git |
| `WALGITHUB_PORT` / `WALGITHUB_CONTAINER_PORT` | `8097` | host port / container port for WAL-Github |
| `WALGIT_LISTEN` / `WALGITHUB_LISTEN` | `0.0.0.0:8080` / `0.0.0.0:8097` | the in-container listeners |
| `WALGIT_BIND` | `127.0.0.1` | host interface WAL-Git publishes on |
| `WALGITHUB_BIND` | `127.0.0.1` | host interface WAL-Github publishes on; keep it loopback (the facade is auth-free) |
| `WALGIT_AUTH_MODE` | `token` | WAL-Git only (`token` or `oidc`); WAL-Github is always auth-free |
| `WALGIT__STORE__S3__REGION` | `us-east-1` | |
| `WALGIT__STORE__S3__ENDPOINT` | empty | AWS endpoint for the region; set for MinIO/rustfs/R2/Ceph |
| `WALGIT__STORE__PREFIX` | empty | share one bucket between deployments |
| `WALGIT__CACHE__MAX_BYTES` | `20GiB` | budget for every materialized pack, index and bundle; leave the volume room |
| `WALGIT__TELEMETRY__LOG_FORMAT` | `json` | container-native; `pretty` for an interactive host |

The service roles are part of the deployment shape, not `deploy/.env`: WAL-Git runs `["serve","maintain"]`,
WAL-Github runs `["serve","events"]`. To change them, edit `deploy/compose.yaml` (or a Compose override); see §6.

`deploy/walgit.env.example` is deliberately small. Add any other key from `walgit.example.toml` as
`WALGIT__<SECTION>__<KEY>`. Unknown keys and unparsable values are ignored with a WARN rather than
crash-looping an older image, so read the startup log when an override seems not to apply; values that
reach the config are still validated and fail closed.

### Authentication

WAL-Git uses `mode = "token"` with one admin bearer. For a team, use OIDC instead, which keeps git on
walgit-issued access tokens and needs no shared secret on developer machines:

```dotenv
WALGIT_AUTH_MODE=oidc
WALGIT__SERVER__AUTH__ISSUER=https://id.example.com
WALGIT__SERVER__AUTH__ALLOWED_DOMAINS=["example.com"]
WALGIT__SERVER__AUTH__OAUTH_CLIENT_ID=…
WALGIT__SERVER__AUTH__OAUTH_CLIENT_SECRET=…      # keep it in deploy/.env, not the image
WALGIT__SERVER__AUTH__SESSION_SECRET=…           # >= 32 bytes; every host must share it
WALGIT__SERVER__AUTH__ANONYMOUS_READ=false
```

`session_secret` signs session cookies and walgit access tokens: share it across hosts, and rotate it to
revoke every issued token (it signs everyone out). Static `tokens` work in OIDC mode too, for robots.
`mode = "none"` is loopback-only and exists for experiments; `validate` refuses it on a public bind.

**WAL-Github has no authentication by construction.** It accepts any bearer, resolves one hardcoded
user, and is admin on every repository (`docs/GITHUB.md` §1). Its Compose service forces
`server.auth.mode = "none"` and `server.tls.mode = "off"`; keep it on loopback or a private network.
Never publish `:8097` to the internet.

### TLS

TLS applies to **WAL-Git**. WAL-Github is forced to plain HTTP because the facade is a local
development surface (Node clients reject the self-signed cert otherwise). Three shapes, pick one:

1. **Edge termination (usual).** Run nginx (or any proxy) on the host, terminate TLS there, and keep
   the container on `127.0.0.1:8080`. Set `WALGIT_PUBLIC_URL` to the public origin and forward
   `Host`. `deploy/nginx.conf.example` is the reference: it also documents one `auth_request` per
   credential and `X-Accel-Redirect`, which lets the edge stream bundle/LFS bytes from the bucket
   itself. Only enable `server.accel_redirect` when that edge announces
   `X-Walgit-Capabilities: accel-redirect`; the response carries a store credential.
2. **walgit terminates TLS.** Mount a PEM chain and key, switch the listener and public URL:

   ```dotenv
   WALGIT__SERVER__TLS__MODE=files
   WALGIT__SERVER__TLS__CERT=/etc/walgit/cert.pem
   WALGIT__SERVER__TLS__KEY=/etc/walgit/key.pem
   WALGIT_LISTEN=0.0.0.0:8443
   WALGIT_CONTAINER_PORT=8443
   WALGIT_PORT=8443
   WALGIT_PUBLIC_URL=https://git.example.com
   ```

   Mount the certificate and key read-only (the image runs as uid 1000). The image healthcheck reads
   the port from `WALGIT__SERVER__LISTEN` and falls back to HTTPS, so it follows the change.
3. **Self-signed, private networks only.** `mode = "self_signed"` generates a certificate under
   `<cache.dir>/tls/` and publishes the CA at `/services/public/ca.pem`; the installer pins it for git.
   Browsers and GitHub-integration clients need `NODE_EXTRA_CA_CERTS` or an exception; do not use this
   for a public host.

## 5. The cache volume and the root filesystem

`/var/lib/walgit` holds materialized repositories, pack indexes, a self-signed certificate, task logs and
scratch space for `git index-pack`. It is a cache in the strict sense: the bucket is revalidated on every
read, and losing the volume costs warmth, not data.

- Each service has its own named volume (`walgit-cache`, `walgithub-cache`).
- Both services run with `read_only: true`: the image's root filesystem is only the OS, git and the
  binaries, and the process never writes to it. `/tmp` is a 256 MiB tmpfs; `/var/lib/walgit` is the one
  writable mount that holds state.
- Size the filesystem above `cache.max_bytes` (default 20 GiB) plus headroom for scratch; budget mode
  refuses to pull a pack set that cannot fit and serves it remotely instead.
- `cache.mode = "disk"` removes the budget and fully materializes repositories (`auto`, the default,
  chooses disk when `maintenance.disk = "ssd"`); it is for the host with a real SSD, not for a tmpfs box.
- A bind mount must be writable by uid 1000 (`chown -R 1000:1000 /srv/walgit-cache`); the named volumes
  inherit ownership from the image.

## 6. Two services, several hosts

Every service may point at one bucket. They coordinate only through the manifest CAS and leases, so there
is no leader and no gossip; "which host serves this repository" is decided by configuration and the URL
prefix, never by a registry.

- **This Compose stack:** WAL-Git is `serve + maintain`; WAL-Github is `serve + events`. That is one
  maintainer and one events host for the deployment, with the facade as the GitHub webhook sink. If you
  want the walgit-native events bridge on WAL-Git instead, change the roles and keep exactly one events
  host.
- **Placement:** `[placement] serve`/`serve_exclude` and `maintain`/`maintain_exclude` are globs. A host
  that does not serve a repository answers its object work with `503` + `Retry-After` before any sync;
  refs-level reads (`info/refs`, `ls-refs`, the API, bundle lists, the UI) stay available everywhere.
  Route by `/<owner>/<repo>` in the edge.
- **Scale-out** is "start another container with the same environment". The second host is useful as soon
  as `/readyz` is 200; it does not need a warm cache.
- **Push broker (optional):** a host that maintains nothing can forward receive-pack to a warm writer
  with `[wal] push_broker_url` and a broker token. Publish is CAS-safe, so this is an optimisation, not a
  correctness requirement.
- **The SSD host:** a base rebuild (`compact --base`, the maintainer's weekly `BaseRebuild` unit) needs a
  real disk. Keep those on one maintainer with `maintenance.disk = "ssd"`; the weekly bundle is then a
  bucket-side compose, and everything newer than the base stays in small packs that every tmpfs host can
  hold (`docs/BUNDLE_URI_DESIGN.md`).

Example: two stateless servers and one maintainer, all from the same image. These roles are config, not
the Compose defaults, so set them where the service's config lives:

```toml
# server-a / server-b
[server]
roles = ["serve"]

# maintainer-1
[server]
roles = ["serve", "maintain"]
[maintenance]
disk = "ssd"
```

## 7. Operations

**Health and metrics.** `/healthz` is liveness (the process is up). `/readyz` is readiness: 503 while a
configured prewarm runs, while the instance is draining (D31 phase 2); it carries the instance id,
build revision, roles and placement rules. `/metrics` is Prometheus when `telemetry.metrics = true` (the
default) and sits behind the same auth as the API — scrape WAL-Git with a bearer token. The TLS
fingerprint is logged at startup. Logs are JSON on stdout; the Compose stack rotates them with the
`json-file` driver.

**Deploying a new image.** Build (or pull) the new tag, then `docker compose up -d`. SIGTERM starts the
drain: the maintenance loop starts no new unit, the running unit is interrupted at once, and serving
continues (phase 1, bounded at 30s); then `/readyz` turns 503, new fetch/push/LFS are refused with 503 +
`Retry-After`, in-flight requests get `server.drain_timeout` (20s), and the process exits.
`stop_grace_period: 60s` in the Compose file leaves room for both phases. On several hosts, one at a time.

**Backups** are bucket backups. There is nothing on either host worth copying. Versioning the bucket is
the cheap insurance for the objects rewritten in place (`manifest.pb`, leases, bundle lists, the events
cursor); everything else is immutable and content-addressed. Restoring a bucket restores every
repository, including the WAL needed to reproduce checkpoints and bundles.

**Secrets.** `deploy/.env` is gitignored; keep credentials out of the image. Rotating `WALGIT_TOKEN` requires
recreating the container (static tokens are resolved at startup); rotating `session_secret` revokes every
walgit-issued access token immediately. The facade integration registry (`github/integrations.json`)
contains webhook secrets and lives in the bucket — scope the IAM policy and the bucket's readers
accordingly (`docs/GITHUB.md` §11).

**Moving work.** If a host dies, start another with the same environment; if you move a repository's
maintainer, change the placement globs on both hosts. There is no data to migrate: the new maintainer
reads the WAL and recomputes the desired state (`AGENTS.md` §2.5b).

## 8. Troubleshooting

| Symptom | Likely cause |
|---|---|
| `/readyz` 503 at startup | prewarm still running, or the instance is draining; read the JSON body |
| WAL-Git returns 401 | `WALGIT_TOKEN` was not set (an empty token is dropped at startup); check `docker compose config` |
| WAL-Github answers everything | expected: it is auth-free by construction; keep `:8097` on loopback/private networks |
| `503` + `Retry-After` on clone/fetch | this host is not placed to serve that repository (`[placement]`), or is draining; route to a serving host |
| push rejected with a policy error | per-repository `policy.json` (`docs/POLICY.md`), not the host config |
| `connectivity: missing object` on push | see `docs/INTEGRITY.md`; the maintainer's `fsck`/`repair` units address history damage |
| clone too large / 503 `too large` | the repository does not fit this host and bundle-uri was bypassed; use the installer's recipes, or check `bundles.require` and the bundle list |
| S3 `AccessDenied` only on some operations | the policy lacks delete or list under the prefix; object work needs both |
| container exits with a config error | `Config::validate` failed closed; the message names the key |
