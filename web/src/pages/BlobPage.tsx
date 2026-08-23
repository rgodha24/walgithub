import { useState } from "react";
import { Link, useParams } from "react-router-dom";
import { File } from "@pierre/diffs/react";
import { api, client } from "../api";
import { useResolved } from "../use-resolved";
import { useRepo } from "./RepoLayout";
import { Box } from "../components/Layout";
import { fmtSize } from "../format";
import { RefBar } from "../components/RefBar";
import { Markdown } from "../components/Markdown";

export function BlobPage() {
  const { full } = useRepo();
  const rest = useParams()["*"] ?? "";
  const { r, data: b } = useResolved(full, rest, (res) => api.blob(full, res.sha, res.path));
  const isMd = /\.(md|markdown)$/i.test(rest);
  const [mode, setMode] = useState<"preview" | "code">("preview");
  const lines = b.contents ? b.contents.split("\n").length - (b.contents.endsWith("\n") ? 1 : 0) : 0;
  const rawURL = client.repo(full).urls.raw(b.sha, b.path);
  return (
    <>
      <RefBar refname={r.ref} refKind={r.kind} path={r.path} page="blob" />
      <Box
        className="blob"
        title={
          <div className="blob-head">
            {isMd && b.contents !== undefined && (
              <span className="seg">
                <button className={mode === "preview" ? "active" : ""} onClick={() => setMode("preview")}>
                  Preview
                </button>
                <button className={mode === "code" ? "active" : ""} onClick={() => setMode("code")}>
                  Code
                </button>
              </span>
            )}
            <span className="muted small">
              {b.contents !== undefined && `${lines} lines · `}
              {fmtSize(b.size)}
            </span>
            <span className="spacer" />
            <a className="btn small" href={rawURL} target="_blank" rel="noreferrer">
              Raw
            </a>
            <Link className="btn small" to={`/${full}/commits/${b.ref}/${b.path}`}>
              History
            </Link>
          </div>
        }
      >
        {b.too_large && <div className="pad muted">File is too large to display ({fmtSize(b.size)}).</div>}
        {b.binary && <div className="pad muted">Binary file not shown.</div>}
        {b.contents !== undefined &&
          (isMd && mode === "preview" ? (
            <div className="pad">
              <Markdown source={b.contents} />
            </div>
          ) : (
            <File
              file={{ name: b.name, contents: b.contents.replace(/\n$/, "") }}
              options={{ disableFileHeader: true, themeType: "light", overflow: "scroll" }}
            />
          ))}
      </Box>
    </>
  );
}
