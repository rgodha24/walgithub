import { Link, useParams } from "react-router-dom";
import { api } from "../api";
import { useData } from "../data";
import { Box } from "../components/Layout";

export function Repos() {
  const { owner = "" } = useParams();
  const repos = useData(`repos:${owner}`, () => api.repos(owner));
  return (
    <>
      <h1 className="page-title">
        <Link to="/">Repositories</Link> <span className="muted">/</span> {owner}
      </h1>
      <Box>
        {repos.length === 0 && (
          <div className="muted pad">
            No repositories under <code>{owner}</code>. Push to{" "}
            <code>{location.origin}/{owner}/repository.git</code>.
          </div>
        )}
        <ul className="list">
          {repos.map((r) => (
            <li key={r}>
              <Link to={`/${owner}/${r}`} className="strong">
                {owner}/{r}
              </Link>
              <div className="muted small">
                <code>
                  git -c transfer.bundleURI=true clone {location.origin}/{owner}/{r}.git
                </code>
              </div>
            </li>
          ))}
        </ul>
      </Box>
    </>
  );
}
