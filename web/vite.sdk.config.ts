import { defineConfig } from "vite";
import { precompress } from "./plugins/precompress.ts";

// The SDK (`sdk/repos.ts`) is built twice into `dist/` next to the SPA:
//   repos.js   — IIFE for `<script src>`: registers `window.repos`
//   repos.mjs  — ESM for `import { createClient } from ".../repos.mjs"`
// Both are served by walgit at `/repos.js` and `/repos.mjs` (permanent URLs,
// `no-cache` + ETag); Runs after
// the SPA build (`pnpm run build`), so `emptyOutDir` must stay false here.
export default defineConfig({
  plugins: [precompress()],
  build: {
    outDir: "dist",
    emptyOutDir: false,
    sourcemap: false,
    target: "es2022",
    minify: true,
    lib: {
      entry: "sdk/repos.ts",
      // The module body registers `window.repos` itself (the default client);
      // the IIFE namespace global is only a fallback handle.
      name: "ReposSDK",
      formats: ["iife", "es"],
      fileName: (format) => (format === "es" ? "repos.mjs" : "repos.js"),
    },
    rolldownOptions: {
      output: {
        exports: "named",
      },
    },
  },
});
