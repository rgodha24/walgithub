import { createHash } from "node:crypto";
import { promises as fs } from "node:fs";
import path, { posix } from "node:path";
import type { Plugin } from "vite";

/**
 * Emit an import map and make chunks import each other through stable bare
 * specifiers (`walgit/<chunk-name>`) instead of relative hashed paths.
 *
 * Why: without this a change to a leaf chunk (say a `react` upgrade) changes
 * the hashed file name of *every* importer up to the entry, so users
 * re-download the whole graph. With bare specifiers a chunk's bytes — and
 * therefore its content hash and its `immutable` cache entry — depend only on
 * its own source; the tiny import map in `index.html` (`no-cache` + ETag) is
 * the only thing that changes. Import maps are supported since Chrome 89,
 * Safari 16.4 and Firefox 108.
 *
 * Rolldown does not allow mutating `bundle` in `generateBundle`, so this runs
 * in `writeBundle` on the emitted files: it rewrites import specifiers,
 * re-hashes and renames chunks (dependencies first), updates
 * `__vite__mapDeps` preload lists and `index.html`, and injects
 * `<script type="importmap">` as the first element in `<head>` (import maps
 * must precede every module script and modulepreload link).
 */
export function importMap(opts: { prefix?: string } = {}): Plugin {
  const prefix = opts.prefix ?? "walgit/";
  let base = "/";
  return {
    name: "walgit:importmap",
    apply: "build",
    enforce: "post",
    configResolved(config) {
      base = config.base;
    },
    async writeBundle(options, bundle) {
      const outDir = options.dir ?? path.dirname(options.file ?? "dist");
      type Chunk = Extract<(typeof bundle)[string], { type: "chunk" }>;
      const chunks = new Map<string, Chunk>();
      for (const out of Object.values(bundle)) if (out.type === "chunk") chunks.set(out.fileName, out);
      if (chunks.size === 0) return;

      // Stable bare specifier per chunk (chunk.name is stable; the hash is not).
      const bare = new Map<string, string>();
      const seen = new Map<string, number>();
      for (const [file, c] of chunks) {
        const n = seen.get(c.name) ?? 0;
        seen.set(c.name, n + 1);
        bare.set(file, `${prefix}${c.name}${n ? `~${n}` : ""}`);
      }

      // After rewriting, a chunk's bytes reference other chunks' *hashed* names
      // only in `__vite__mapDeps` preload lists, i.e. for its dynamic imports.
      // Process strongly connected components of that graph in dependency
      // order so importers can embed their dependencies' final names; members
      // of a cycle (rare: mutual dynamic imports) are hashed together.
      const deps = (c: Chunk) => [...new Set([...c.imports, ...c.dynamicImports])].filter((f) => chunks.has(f));
      // A preload list names the dynamic target *and its static import closure*
      // (everything the browser should fetch in parallel), so all of those are
      // hash dependencies of the importer.
      const closureMemo = new Map<string, Set<string>>();
      const staticClosure = (f: string, acc = new Set<string>()): Set<string> => {
        const memo = closureMemo.get(f);
        if (memo) {
          for (const m of memo) acc.add(m);
          return acc;
        }
        for (const d of chunks.get(f)?.imports ?? []) {
          if (chunks.has(d) && !acc.has(d)) {
            acc.add(d);
            staticClosure(d, acc);
          }
        }
        return acc;
      };
      for (const f of chunks.keys()) closureMemo.set(f, staticClosure(f));
      const hashDeps = (f: string) => {
        const out = new Set<string>();
        for (const d of chunks.get(f)?.dynamicImports ?? []) {
          if (!chunks.has(d)) continue;
          out.add(d);
          for (const m of closureMemo.get(d) ?? []) out.add(m);
        }
        out.delete(f);
        return [...out];
      };
      const sccs = tarjan([...chunks.keys()], hashDeps); // dependencies first

      const sources = new Map(
        await Promise.all([...chunks.keys()].map(async (f) => [f, await fs.readFile(path.join(outDir, f), "utf8")] as const)),
      );
      const writes: Promise<unknown>[] = [];
      const renamed = new Map<string, string>(); // old fileName → new fileName
      const imports: Record<string, string> = {};
      for (const scc of sccs) {
        const inScc = new Set(scc);
        const codes = new Map<string, string>();
        for (const oldFile of scc) {
          let code = sources.get(oldFile) ?? "";
          const dir = posix.dirname(oldFile);
          for (const dep of deps(chunks.get(oldFile)!)) {
            let rel = posix.relative(dir, dep);
            if (!rel.startsWith(".")) rel = `./${rel}`;
            const spec = bare.get(dep)!;
            // Static + dynamic import specifiers → bare specifier (rolldown
            // emits dynamic imports as template literals: import(`./x.js`)).
            for (const q of ['"', "'", "`"]) code = code.split(`${q}${rel}${q}`).join(`${q}${spec}${q}`);
          }
          // `__vite__mapDeps([...])` preload lists hold base-relative paths of
          // every hash dependency; point them at the final (re-hashed) names.
          // For members of this SCC the final name is not known yet: use a
          // placeholder resolved after the SCC hash is computed.
          for (const dep of hashDeps(oldFile)) {
            const target = inScc.has(dep) ? `\u0000${bare.get(dep)}\u0000` : renamed.get(dep);
            if (target && target !== dep) code = code.split(`"${dep}"`).join(`"${target}"`);
          }
          codes.set(oldFile, code);
        }
        // One hash for the whole SCC: every member's name changes iff any member changes.
        const hash = hashOf(scc.map((f) => `${bare.get(f)}\n${codes.get(f)}`).join("\n"));
        for (const oldFile of scc) renamed.set(oldFile, oldFile.replace(/-[\w-]{8}(\.m?js)$/, `-${hash}$1`));
        for (const oldFile of scc) {
          const newFile = renamed.get(oldFile)!;
          let code = codes.get(oldFile) ?? "";
          for (const m of scc) code = code.split(`\u0000${bare.get(m)}\u0000`).join(renamed.get(m)!);
          if (chunks.get(oldFile)?.map) {
            code = code.replace(/(\/\/# sourceMappingURL=)\S+/, `$1${posix.basename(newFile)}.map`);
            writes.push(fs.rename(path.join(outDir, `${oldFile}.map`), path.join(outDir, `${newFile}.map`)).catch(() => {}));
          }
          const write = fs.writeFile(path.join(outDir, newFile), code);
          writes.push(newFile === oldFile ? write : write.then(() => fs.unlink(path.join(outDir, oldFile))));
          imports[bare.get(oldFile)!] = base + newFile;
        }
      }

      // Patch emitted HTML: renamed files + the import map itself.
      // `<` escaped so a (hostile) chunk name can never close the script element.
      const mapTag = `<script type="importmap">${JSON.stringify({ imports }).replace(/</g, "\\u003c")}</script>`;
      const htmlFiles = Object.values(bundle).filter((o) => o.type === "asset" && o.fileName.endsWith(".html"));
      const patchHtml = (html: string) => {
        for (const [o, n] of renamed) if (o !== n) html = html.split(o).join(n);
        return /<meta charset[^>]*>/i.test(html)
          ? html.replace(/(<meta charset[^>]*>)/i, `$1\n    ${mapTag}`)
          : html.replace(/<head>/i, `<head>\n    ${mapTag}`);
      };
      const rewriteHtml = (file: string) =>
        fs.readFile(file, "utf8").then((html) => fs.writeFile(file, patchHtml(html)));
      writes.push(...htmlFiles.map((out) => rewriteHtml(path.join(outDir, out.fileName))));
      await Promise.all(writes);
      this.info(`import map: ${Object.keys(imports).length} chunks in ${sccs.length} groups`);
    },
  };
}

const hashOf = (s: string) => createHash("sha256").update(s).digest("base64url").slice(0, 8).replace(/[-_]/g, "0");

/** Tarjan's SCC algorithm; returns components in reverse topological order (dependencies first). */
function tarjan(nodes: string[], edges: (n: string) => string[]): string[][] {
  let index = 0;
  const idx = new Map<string, number>();
  const low = new Map<string, number>();
  const onStack = new Set<string>();
  const stack: string[] = [];
  const out: string[][] = [];
  const visit = (v: string) => {
    idx.set(v, index);
    low.set(v, index);
    index++;
    stack.push(v);
    onStack.add(v);
    for (const w of edges(v)) {
      if (!idx.has(w)) {
        visit(w);
        low.set(v, Math.min(low.get(v)!, low.get(w)!));
      } else if (onStack.has(w)) {
        low.set(v, Math.min(low.get(v)!, idx.get(w)!));
      }
    }
    if (low.get(v) === idx.get(v)) {
      const scc: string[] = [];
      let w: string;
      do {
        w = stack.pop()!;
        onStack.delete(w);
        scc.push(w);
      } while (w !== v);
      out.push(scc.toSorted());
    }
  };
  for (const n of nodes) if (!idx.has(n)) visit(n);
  return out;
}
