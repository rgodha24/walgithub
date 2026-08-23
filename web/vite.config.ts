import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { importMap } from "./plugins/importmap.ts";
import { precompress } from "./plugins/precompress.ts";

// Built assets are embedded in the walgit binary (rust-embed) and served under
// /_ui/ with immutable caching; in dev, the JSON API (`/api/v1/…`, D20, plus the
// older `/services/api` and `/{owner}/{repo}/api` shapes, API.md §1) is proxied
// to a running `walgit serve`.
export default defineConfig(({ command }) => ({
  plugins: [react(), importMap(), precompress()],
  // Embedded assets live under /_ui/; the dev server serves the SPA at / so
  // BrowserRouter paths (/owner/repo/…) work unchanged.
  base: command === "build" ? "/_ui/" : "/",
  server: {
    proxy: {
      "/api/": process.env.WALGIT_URL ?? "http://127.0.0.1:8080",
      "/api-browser/": process.env.WALGIT_URL ?? "http://127.0.0.1:8080",
      "/services/api/": process.env.WALGIT_URL ?? "http://127.0.0.1:8080",
      "^/[^/]+/[^/]+/api(-browser)?(/|$)": process.env.WALGIT_URL ?? "http://127.0.0.1:8080",
      "^/[^/]+/[^/]+(\\.git)?/(info/refs|git-upload-pack|git-receive-pack|bundles/)": process.env.WALGIT_URL ?? "http://127.0.0.1:8080",
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    // Evergreen browsers only (import maps need Chrome 89 / Safari 16.4 /
    // Firefox 108 anyway): no transpilation of modern syntax, no polyfills.
    target: "es2022",
    modulePreload: { polyfill: false },
    cssMinify: "lightningcss",
    reportCompressedSize: true,
    chunkSizeWarningLimit: 600,
    rolldownOptions: {
      output: {
        // Stable vendor groups: a change in app code never invalidates the
        // (large, rarely changing) framework chunks and vice versa. Chunk names
        // double as import-map specifiers (`walgit/react`, …).
        codeSplitting: {
          groups: [
            { name: "vendor-react", test: /node_modules[\\/](react|react-dom|scheduler|react-router|react-router-dom)[\\/]/, priority: 30 },
            { name: "vendor-diffs", test: /node_modules[\\/](@pierre[\\/]diffs|shiki|@shikijs[\\/](core|engine-|types|vscode-textmate|transformers|primitive)|oniguruma-to-es|oniguruma-parser|regex|diff[\\/])/, priority: 20 },
            { name: "vendor-markdown", test: /node_modules[\\/](react-markdown|remark|micromark|mdast|unified|unist|vfile|hast|property-information|html-url-attributes|devlop|decode-named|character-|trim-lines|zwitch|bail|trough|is-plain-obj|space-separated|comma-separated|estree|longest-streak|ccount|markdown-table|escape-string-regexp|extend|style-to)/, priority: 10 },
          ],
        },
      },
    },
  },
  css: { transformer: "lightningcss" },
}));
