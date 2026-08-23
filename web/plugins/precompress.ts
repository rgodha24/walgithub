import { promises as fs } from "node:fs";
import path from "node:path";
import { promisify } from "node:util";
import zlib from "node:zlib";
import type { Plugin } from "vite";

const brotli = promisify(zlib.brotliCompress);
const gzip = promisify(zlib.gzip);

/**
 * Write `.br` and `.gz` siblings for every compressible asset in `outDir`.
 * The walgit binary embeds the whole directory and serves the precompressed
 * variant matching `Accept-Encoding` (`Content-Encoding` + `Vary`), so
 * production never compresses JS/CSS at request time — max-quality brotli is
 * paid once at build.
 */
export function precompress(opts: { minBytes?: number; exts?: string[] } = {}): Plugin {
  const minBytes = opts.minBytes ?? 1024;
  const exts = new Set(opts.exts ?? [".js", ".mjs", ".css", ".html", ".svg", ".json", ".txt", ".map"]);
  let outDir = "dist";
  return {
    name: "walgit:precompress",
    apply: "build",
    enforce: "post",
    configResolved(config) {
      outDir = path.resolve(config.root, config.build.outDir);
    },
    async closeBundle() {
      const files: string[] = [];
      const walk = async (dir: string): Promise<void> => {
        const entries = await fs.readdir(dir, { withFileTypes: true });
        await Promise.all(
          entries.map((e) => {
            const p = path.join(dir, e.name);
            if (e.isDirectory()) return walk(p);
            if (exts.has(path.extname(e.name))) files.push(p);
            return undefined;
          }),
        );
      };
      await walk(outDir);
      let raw = 0;
      let br = 0;
      await Promise.all(
        files.map(async (f) => {
          const data = await fs.readFile(f);
          if (data.length < minBytes) return;
          const [b, g] = await Promise.all([
            brotli(data, {
              params: {
                [zlib.constants.BROTLI_PARAM_QUALITY]: 11,
                [zlib.constants.BROTLI_PARAM_SIZE_HINT]: data.length,
                [zlib.constants.BROTLI_PARAM_MODE]: zlib.constants.BROTLI_MODE_TEXT,
              },
            }),
            gzip(data, { level: 9 }),
          ]);
          raw += data.length;
          br += b.length;
          await Promise.all([fs.writeFile(`${f}.br`, b), fs.writeFile(`${f}.gz`, g)]);
        }),
      );
      this.info(`precompressed ${files.length} files: ${(raw / 1024).toFixed(0)} kB → ${(br / 1024).toFixed(0)} kB brotli`);
    },
  };
}
