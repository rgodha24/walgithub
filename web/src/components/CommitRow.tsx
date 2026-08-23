import { Link } from "react-router-dom";
import type { Commit } from "../api";
import { relTime } from "../format";
import { Linkified, Trailers } from "./CommitMessage";

export function Avatar({ name }: { name: string }) {
  let h = 0;
  for (const ch of name) h = (h * 31 + ch.charCodeAt(0)) >>> 0;
  return (
    <span className="avatar" style={{ background: `hsl(${h % 360} 45% 55%)` }} title={name}>
      {name.trim().slice(0, 1).toUpperCase()}
    </span>
  );
}

export function CommitRow({ repo, commit, compact }: { repo: string; commit: Commit; compact?: boolean }) {
  return (
    <div className={`commit-row ${compact ? "compact" : ""}`}>
      <Avatar name={commit.author} />
      <div className="commit-main">
        <Link to={`/${repo}/commit/${commit.sha}`} className="commit-subject">
          {commit.subject}
        </Link>
        {!compact && commit.body && (
          <details className="commit-body">
            <summary aria-label="Show full commit message">…</summary>
            <pre>
              <Linkified text={commit.body} />
            </pre>
          </details>
        )}
        <div className="muted small row wrap gap">
          <span>
            <strong>{commit.author}</strong> committed {relTime(commit.commit_date)}
          </span>
          {!compact && <Trailers repo={repo} trailers={commit.trailers ?? []} />}
        </div>
      </div>
      <div className="commit-meta">
        <Link to={`/${repo}/commit/${commit.sha}`} className="sha">
          {commit.sha.slice(0, 7)}
        </Link>
        <Link to={`/${repo}/tree/${commit.sha}`} className="btn small" title="Browse the repository at this point in the history">
          {"<>"}
        </Link>
      </div>
    </div>
  );
}
