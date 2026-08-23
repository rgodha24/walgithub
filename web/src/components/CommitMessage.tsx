import { Link } from "react-router-dom";
import type { CommitTrailer } from "../api";

/* ---------- body: text with every http(s) URL linkified (safe) ---------- */

const URL_RE = /https?:\/\/[^\s<>()"'`]+[^\s<>()"'`.,;:!?]/g;

export function Linkified({ text }: { text: string }) {
  const parts: (string | { url: string })[] = [];
  let last = 0;
  for (const m of text.matchAll(URL_RE)) {
    const i = m.index ?? 0;
    if (i > last) parts.push(text.slice(last, i));
    parts.push({ url: m[0] });
    last = i + m[0].length;
  }
  if (last < text.length) parts.push(text.slice(last));
  return (
    <>
      {parts.map((p, i) =>
        typeof p === "string" ? (
          <span key={i}>{p}</span>
        ) : (
          <a key={i} href={p.url} target="_blank" rel="noopener noreferrer">
            {p.url}
          </a>
        ),
      )}
    </>
  );
}

/* ---------- trailers: a small table of known keys, repo-agnostic ---------- */

type Group = "Merge queue" | "People" | "Other";

function groupOf(key: string): Group {
  const k = key.toLowerCase();
  if (k === "co-authored-by" || k === "assisted-by" || k === "signed-off-by" || k === "reviewed-by" || k === "acked-by" || k === "tested-by")
    return "People";
  if (k.startsWith("merge-queue-") || k.includes("ci-sha") || k.includes("ci-boundary")) return "Merge queue";
  return "Other";
}

const SHA_RE = /^[0-9a-f]{40}$/;
const MAIL_RE = /^(.*?)\s*<([^>]+@[^>]+)>\s*$/;

function TrailerValue({ repo, t }: { repo: string; t: CommitTrailer }) {
  const v = t.value.trim();
  if (SHA_RE.test(v))
    return (
      <Link to={`/${repo}/commit/${v}`} className="sha" title="May not exist here (a CI boundary commit)">
        {v.slice(0, 12)}
      </Link>
    );
  const m = v.match(MAIL_RE);
  if (m)
    return (
      <>
        {m[1] && <span>{m[1]} </span>}
        <a href={`mailto:${m[2]}`}>&lt;{m[2]}&gt;</a>
      </>
    );
  return <Linkified text={v} />;
}

/** `12 trailers` pill with the table under it. */
export function Trailers({ repo, trailers, open }: { repo: string; trailers: CommitTrailer[]; open?: boolean }) {
  if (trailers.length === 0) return null;
  const groups: Group[] = ["Merge queue", "People", "Other"];
  const by = new Map<Group, CommitTrailer[]>();
  for (const t of trailers) {
    const g = groupOf(t.key);
    by.set(g, [...(by.get(g) ?? []), t]);
  }
  const bits = [`${trailers.length} trailer${trailers.length === 1 ? "" : "s"}`];
  return (
    <details className="trailers" open={open}>
      <summary className="pill" title="Commit trailers (machine-readable footer lines)">
        {bits.join(" · ")}
      </summary>
      <table className="grid trailers-table">
        <tbody>
          {groups
            .filter((g) => by.has(g))
            .map((g) => (
              <>
                <tr key={`h-${g}`} className="trailers-group">
                  <th colSpan={2}>{g}</th>
                </tr>
                {by.get(g)!.map((t, i) => (
                  <tr key={`${g}-${i}`}>
                    <td className="trailer-key">{t.key}</td>
                    <td className="trailer-value">
                      <TrailerValue repo={repo} t={t} />
                    </td>
                  </tr>
                ))}
              </>
            ))}
        </tbody>
      </table>
    </details>
  );
}
