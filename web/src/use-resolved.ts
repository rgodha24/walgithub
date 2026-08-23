import { api, type Resolved } from "./api";
import { useData } from "./data";

/** Two-step read used by every ref-addressed page: resolve "{ref}/{path}"
 * to a sha (small, revalidated in the background), then fetch the
 * sha-addressed payload (immutable: cached forever, here and in the browser). */
export function useResolved<T>(repo: string, rest: string, fetch: (r: Resolved) => Promise<T>): { r: Resolved; data: T } {
  const r = useData(`resolve:${repo}:${rest}`, () => api.resolve(repo, rest));
  const data = useData(`sha:${repo}:${r.sha}:${r.path}:${r.kind}`, () => fetch(r), Infinity);
  return { r, data };
}
