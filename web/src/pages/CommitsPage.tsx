import { useState, useTransition } from "react";
import { useParams } from "react-router-dom";
import { api, type Commit, type Resolved } from "../api";
import { useRepo } from "./RepoLayout";
import { useData } from "../data";
import { Box } from "../components/Layout";
import { RefBar } from "../components/RefBar";
import { CommitRow } from "../components/CommitRow";

export function CommitsPage() {
  const { full } = useRepo();
  const rest = useParams()["*"] ?? "";
  const r = useData(`resolve:${full}:${rest}`, () => api.resolve(full, rest));
  return <CommitList key={`${r.sha}:${r.path}`} full={full} r={r} />;
}

/** History from a resolved sha: every page request is sha-addressed and
 * therefore immutable/cacheable. The first page suspends (route skeleton);
 * further pages are appended inside a transition, so the list never flashes. */
function CommitList({ full, r }: { full: string; r: Resolved }) {
  const [isPending, startTransition] = useTransition();
  const first = useData(`commits:${full}:${r.sha}:${r.path}:0`, () => api.commits(full, r.sha, r.path, 0), Infinity);
  const [extra, setExtra] = useState<{ commits: Commit[]; more: boolean }>({ commits: [], more: first.more });
  const more = extra.more;
  const loadMore = () =>
    startTransition(async () => {
      const skip = first.commits.length + extra.commits.length;
      const page = await api.commits(full, r.sha, r.path, skip);
      startTransition(() => setExtra((e) => ({ commits: [...e.commits, ...page.commits], more: page.more })));
    });
  const commits = [...first.commits, ...extra.commits];

  // Group by committer day, GitHub style.
  const groups: { day: string; commits: Commit[] }[] = [];
  for (const c of commits) {
    const day = new Date(c.commit_date).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
    const g = groups[groups.length - 1];
    if (g && g.day === day) g.commits.push(c);
    else groups.push({ day, commits: [c] });
  }
  return (
    <>
      <RefBar refname={r.ref} refKind={r.kind} path={r.path} page="commits" />
      {groups.map((g) => (
        <div key={g.day} className="commit-group">
          <div className="commit-day muted">Commits on {g.day}</div>
          <Box>
            {g.commits.map((c) => (
              <CommitRow key={c.sha} repo={full} commit={c} />
            ))}
          </Box>
        </div>
      ))}
      {commits.length === 0 && (
        <Box>
          <div className="pad muted">No commits.</div>
        </Box>
      )}
      {more && (
        <div className="center pad">
          <button className="btn" disabled={isPending} aria-busy={isPending} onClick={loadMore}>
            {isPending ? "Loading…" : "Older"}
          </button>
        </div>
      )}
    </>
  );
}
