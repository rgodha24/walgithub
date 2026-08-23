# walgit web UI

Context: **frontend implementation and build guide** for engineers changing the bundled React SPA, its Vite
pipeline, static assets, or loading states. The HTTP contract belongs in `web/API.md`; public SDK usage
belongs in `web/sdk/README.md`.

React 19 + Vite 8 (rolldown) SPA, embedded in the walgit binary (`rust-embed`)
and served under `/_ui/`. Build from the repository root with `just web-build`
(`pnpm run build` = `oxlint --deny-warnings && tsc --noEmit && vite build`).

For local development, run the server and Vite together:

```sh
WALGIT_URL=http://127.0.0.1:8080 pnpm run dev
```

Checks: `pnpm run lint` (oxlint, config in `.oxlintrc.json`), `pnpm run typecheck`,
`pnpx react-doctor@latest .` (kept at 100/100).

## SDK (`sdk/repos.ts` → `/repos.js`, `/repos.mjs`)

The public SDK (D20, `sdk/README.md`) lives next to the SPA and is built into
`dist/` by the second step of `pnpm run build` (`vite.sdk.config.ts`: IIFE that
registers `window.repos`, plus an ESM build). `src/api.ts` is an
adapter over it — repository requests use `/{owner}/{repo}/api/*`, cross-origin pages
use `/{owner}/{repo}/api-browser/*`, and only non-repository discovery/authentication
uses `/api/v1/*` (D26/D27). Changing the API means changing `sdk/repos.ts` and
`API.md` in the same commit.

## Production build

- **Code splitting**: vendor groups (`vendor-react`, `vendor-diffs`,
  `vendor-markdown`) + per-route lazy chunks (`BlobPage`, `CommitPage`,
  `OverviewPage`, `MarkdownRenderer`); shiki grammars/themes stay lazy
  (`@pierre/diffs` loads them on demand). Entry ≈ 15 kB, react ≈ 73 kB gz.
- **Import map** (`plugins/importmap.ts`): chunks import each other through
  bare specifiers (`walgit/<chunk>`) resolved by a `<script type="importmap">`
  in `index.html`; each chunk is re-hashed over its own bytes only, so
  changing one chunk does not cascade new hashes through its importers — users
  re-download only what changed. The import map lives in `index.html`
  (`no-cache` + ETag → usually a 304).
- **Precompression** (`plugins/precompress.ts`): `.br` (q11) and `.gz`
  siblings for every asset; the server negotiates `Accept-Encoding` and never
  compresses static assets at request time.
- **Serving** (`crates/walgit-server/src/web/ui.rs`): `/_ui/assets/*` →
  `Cache-Control: public, max-age=31536000, immutable`, strong `ETag`
  (build-time sha256), `If-None-Match` → 304, `Vary: Accept-Encoding`,
  `Content-Length`, `HEAD`. `index.html` → `no-cache` + ETag. Dynamic JSON is
  brotli/gzip-compressed on the fly (`CompressionLayer`), SSE never.

## Loading states

- Data loading is Suspense-based (`src/data.ts`: `useData(key, fn, ttl)` — a
  promise cache with background revalidation wrapped in `startTransition`;
  sha-addressed data uses `ttl: Infinity`).
- `index.html` paints a skeleton before any JS; the shell (top bar, repo
  header, tabs) stays put while a page suspends (`RouteBoundary` =
  ErrorBoundary + Suspense with skeletons); navigations are transitions, so
  the previous page stays visible with the top progress bar running
  (`TopProgress`, fed by every in-flight fetch and lazy chunk import).

## SSE

`src/data.ts#readSse` reads `text/event-stream` over `fetch` (GET/POST, with
`Accept`/auth headers). Used for the branch/tag picker (`refs/{branches,tags}`
streams `event: ref` per match; painted progressively, a keystroke aborts the
previous stream) and for maintenance ops on the WAL page (`POST …/ops/{op}`
streams `started`/`log`/`done`/`error` into a live console).
