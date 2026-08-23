import { useEffect, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { refListStream, type RefInfo } from "../api";
import { reportError } from "../data";
import { useRepo } from "../pages/RepoLayout";

const PAGE = 50;

/** Branch/tag dropdown plus path breadcrumbs. `page` is the current page
 * kind: the dropdown keeps the page and path, swapping only the ref; crumbs
 * link to tree listings (commits pages link to commit listings).
 *
 * The picker never loads the whole ref list: it asks the server for one
 * name-sorted page (optionally substring-filtered) per keystroke, so it is
 * as cheap on a monorepo as on a toy repo. The page arrives as an SSE stream
 * (`event: ref` per match) and is painted progressively — on a repo with
 * hundreds of thousands of refs the first matches show up while the server
 * is still scanning; a new keystroke aborts the previous stream. */
export function RefBar({
  refname,
  refKind,
  path,
  page,
}: {
  refname: string;
  refKind: "branch" | "tag" | "commit";
  path: string;
  page: "tree" | "blob" | "commits";
}) {
  const { full, refs } = useRepo();
  const nav = useNavigate();
  const [open, setOpen] = useState(false);
  const [tab, setTab] = useState<"branches" | "tags">(refKind === "tag" ? "tags" : "branches");
  const [q, setQ] = useState("");
  const [list, setList] = useState<{ key: string; refs: RefInfo[]; more: boolean; loading: boolean; error?: string }>({
    key: "",
    refs: [],
    more: false,
    loading: false,
  });
  const key = open ? `${full}:${tab}:${q}` : "";
  useEffect(() => {
    if (!key) return;
    const ctl = new AbortController();
    const t = setTimeout(() => {
      setList({ key, refs: [], more: false, loading: true });
      let batch: RefInfo[] = [];
      let flush = 0;
      const paint = () => {
        flush = 0;
        if (ctl.signal.aborted || batch.length === 0) return;
        const add = batch;
        batch = [];
        setList((s) => (s.key === key ? { ...s, refs: [...s.refs, ...add] } : s));
      };
      refListStream(
        full,
        tab,
        { q, n: PAGE },
        (r) => {
          batch.push(r);
          // Coalesce events into one render per frame.
          if (!flush) flush = requestAnimationFrame(paint);
        },
        ctl.signal,
      ).then(
        ({ more }) => {
          cancelAnimationFrame(flush);
          paint();
          if (!ctl.signal.aborted) setList((s) => ({ ...s, key, more, loading: false }));
        },
        (err: unknown) => {
          if (ctl.signal.aborted) return;
          const msg = err instanceof Error ? err.message : String(err);
          reportError(err, `${tab} list`);
          setList({ key, refs: [], more: false, loading: false, error: msg });
        },
      );
    }, q ? 150 : 0);
    return () => {
      ctl.abort();
      clearTimeout(t);
    };
  }, [key, full, tab, q]);

  const segs = path ? path.split("/") : [];
  const crumbKind = page === "commits" ? "commits" : "tree";
  const go = (name: string) => {
    setOpen(false);
    nav(`/${full}/${page}/${name}${path ? "/" + path : ""}`);
  };
  const shown = list.key === key ? list.refs : [];
  return (
    <div className="refbar">
      <div className="dropdown">
        <button className="btn" onClick={() => setOpen((o) => !o)} title={refname}>
          <span className="muted">{refKind}:</span>{" "}
          <strong>{refKind === "commit" ? refname.slice(0, 7) : refname.length > 28 ? refname.slice(0, 25) + "…" : refname}</strong>{" "}
          <span className="caret">▾</span>
        </button>
        {open && (
          <div className="menu">
            <input autoFocus placeholder={`Find a ${tab === "tags" ? "tag" : "branch"}…`} value={q} onChange={(e) => setQ(e.target.value)} />
            <div className="menu-tabs">
              <button className={tab === "branches" ? "active" : ""} onClick={() => setTab("branches")}>
                Branches
              </button>
              <button className={tab === "tags" ? "active" : ""} onClick={() => setTab("tags")}>
                Tags
              </button>
            </div>
            <ul>
              {shown.map((r) => (
                <li key={r.name} className={r.name === refname ? "current" : ""}>
                  <button type="button" className="ref-option" onClick={() => go(r.name)} aria-current={r.name === refname ? "true" : undefined}>
                    {r.name}
                    {tab === "branches" && r.name === refs.head?.name && <span className="pill default">default</span>}
                  </button>
                </li>
              ))}
              {list.key === key && list.error && (
                <li className="flash error small" role="alert">
                  Could not load {tab}: {list.error}
                </li>
              )}
              {shown.length === 0 && !list.loading && !list.error && <li className="muted">No matches</li>}
              {list.loading && shown.length === 0 && <li className="muted">Loading…</li>}
              {list.more && <li className="muted small">Showing first {shown.length}; type to narrow</li>}
            </ul>
          </div>
        )}
      </div>
      <div className="crumbs">
        <Link to={`/${full}/${crumbKind}/${refname}`} className="strong">
          {full.split("/")[1]}
        </Link>
        {segs.map((s, i) => {
          const sub = segs.slice(0, i + 1).join("/");
          const last = i === segs.length - 1;
          return (
            <span key={sub}>
              <span className="muted"> / </span>
              {last ? <strong>{s}</strong> : <Link to={`/${full}/${crumbKind}/${refname}/${sub}`}>{s}</Link>}
            </span>
          );
        })}
      </div>
      <span className="spacer" />
      {page === "tree" && (
        <Link className="btn" to={`/${full}/commits/${refname}${path ? "/" + path : ""}`}>
          History
        </Link>
      )}
    </div>
  );
}
