import { useMemo, useState } from "react";
import { Link, useParams } from "react-router-dom";
import { parsePatchFiles } from "@pierre/diffs";
import { FileDiff } from "@pierre/diffs/react";
import { api } from "../api";
import { useData } from "../data";
import { useRepo } from "./RepoLayout";
import { Box } from "../components/Layout";
import { relTime } from "../format";
import { Avatar } from "../components/CommitRow";
import { Linkified, Trailers } from "../components/CommitMessage";

export function CommitPage() {
  const { full } = useRepo();
  const { sha = "" } = useParams();
  const data = useData(`commit:${full}:${sha}`, () => api.commit(full, sha), Infinity);
  const [split, setSplit] = useState(false);
  const files = useMemo(() => {
    if (!data.patch) return [];
    try {
      return parsePatchFiles(data.patch, sha).flatMap((p) => p.files);
    } catch (e) {
      console.error(e);
      return [];
    }
  }, [data, sha]);
  const { commit: c, stats } = data;
  const add = stats.reduce((n, s) => n + Math.max(0, s.additions), 0);
  const del = stats.reduce((n, s) => n + Math.max(0, s.deletions), 0);
  return (
    <>
      <Box className="commit-box">
        <div className="commit-title pad">
          <h2>{c.subject}</h2>
          {c.body && (
            <pre className="commit-body-full">
              <Linkified text={c.body} />
            </pre>
          )}
          <div className="row wrap gap">
            <Trailers repo={full} trailers={c.trailers ?? []} open />
            <span className="spacer" />
            <Link className="btn small" to={`/${full}/tree/${c.sha}`}>
              Browse files
            </Link>
          </div>
        </div>
        <div className="commit-foot pad row wrap gap">
          <Avatar name={c.author} />
          <strong>{c.author}</strong>
          <span className="muted">
            authored {relTime(c.author_date)}
            {(c.committer !== c.author || c.commit_date !== c.author_date) && <> · committed by {c.committer} {relTime(c.commit_date)}</>}
          </span>
          <span className="spacer" />
          <span className="muted small">
            {c.parents.length === 0 && "root commit"}
            {c.parents.length > 0 && (
              <>
                {c.parents.length === 1 ? "parent " : "parents "}
                {c.parents.map((p, i) => (
                  <span key={p}>
                    {i > 0 && " + "}
                    <Link to={`/${full}/commit/${p}`} className="sha">
                      {p.slice(0, 7)}
                    </Link>
                  </span>
                ))}
              </>
            )}
            {" · commit "}
            <span className="sha">{c.sha.slice(0, 7)}</span>
          </span>
        </div>
      </Box>

      <div className="diffstat row wrap gap">
        <span>
          Showing <strong>{stats.length}</strong> changed file{stats.length === 1 ? "" : "s"} with{" "}
          <strong className="add">{add} additions</strong> and <strong className="del">{del} deletions</strong>.
        </span>
        <span className="spacer" />
        <span className="seg">
          <button className={split ? "" : "active"} onClick={() => setSplit(false)}>
            Unified
          </button>
          <button className={split ? "active" : ""} onClick={() => setSplit(true)}>
            Split
          </button>
        </span>
      </div>
      <details className="box filelist">
        <summary className="box-header">Files changed</summary>
        <ul className="list compact">
          {stats.map((s) => (
            <li key={s.path} className="row">
              <a href={`#d-${encodeURIComponent(s.path)}`}>{s.path}</a>
              <span className="spacer" />
              {s.additions < 0 ? (
                <span className="muted small">binary</span>
              ) : (
                <span className="small">
                  <span className="add">+{s.additions}</span> <span className="del">−{s.deletions}</span>
                </span>
              )}
            </li>
          ))}
        </ul>
      </details>

      {files.map((f, i) => (
        <div key={f.name + i} id={`d-${encodeURIComponent(f.name)}`} className="diff-file">
          <FileDiff fileDiff={f} options={{ diffStyle: split ? "split" : "unified", themeType: "light", overflow: "scroll" }} />
        </div>
      ))}
      {files.length === 0 && stats.length > 0 && (
        <Box>
          <pre className="pad code-block">{data.patch}</pre>
        </Box>
      )}
    </>
  );
}
