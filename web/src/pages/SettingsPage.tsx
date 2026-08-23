import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { api, type Overview, type PolicyDryRun, type PolicyValidation, type SettingsDescribe, type SettingsHistory, type SettingsValidation } from "../api";
import { invalidate, useData } from "../data";
import { Box } from "../components/Layout";
import { Maintainers } from "../components/Maintainers";
import { Link } from "react-router-dom";
import { useRepo } from "./RepoLayout";

const fmtBytes = (n: number) => {
  if (!n) return "unlimited";
  if (n < 1024) return `${n} B`;
  const u = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < u.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v.toFixed(1)} ${u[i]}`;
};
const fmtTime = (s?: string | null) => (s && !s.startsWith("1970") ? new Date(s).toLocaleString() : "—");
const show = (v: unknown): string => (v === null || v === undefined ? "—" : typeof v === "string" ? v : JSON.stringify(v));

function useDebounced<T>(value: T, ms: number): T {
  const [v, setV] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setV(value), ms);
    return () => clearTimeout(t);
  }, [value, ms]);
  return v;
}

/** Settings tab: scheduled tasks + placement + live plan; push policy; effective config + history. */
export function SettingsPage() {
  const { full } = useRepo();
  const d: SettingsDescribe = useData(`settings:${full}`, () => api.settings(full).describe(), 2_000);
  const o: Overview = useData(`overview:${full}`, () => api.overview(full), 2_000);
  const [section, setSection] = useState<"tasks" | "policy" | "config">("tasks");
  return (
    <div className="settings">
      <nav className="subtabs" aria-label="Settings sections">
        {(
          [
            ["tasks", "Scheduled tasks"],
            ["policy", "Push policy"],
            ["config", "Effective config & history"],
          ] as const
        ).map(([k, label]) => (
          <button key={k} type="button" className={section === k ? "subtab active" : "subtab"} onClick={() => setSection(k)}>
            {label}
          </button>
        ))}
      </nav>
      {section === "tasks" && <Tasks d={d} o={o} full={full} />}
      {section === "policy" && <PolicyEditor full={full} />}
      {section === "config" && <EffectiveConfig d={d} full={full} />}
    </div>
  );
}

// ---- 1. scheduled tasks ---------------------------------------------------------

function Tasks({ d, o, full }: { d: SettingsDescribe; o: Overview; full: string }) {
  const host = d.maintenance.this_host;
  return (
    <>
      <Box title={`Bundle strategies (${d.strategies.length}) — ${d.bundles.enabled ? "enabled" : "disabled"}`}>
        {d.strategies.length === 0 ? (
          <div className="muted pad">No strategies in the effective config.</div>
        ) : (
          <div className="scroll-x">
            <table className="grid">
              <thead>
                <tr>
                  <th>name</th>
                  <th>kind</th>
                  <th>base</th>
                  <th>schedule</th>
                  <th>next (local time)</th>
                  <th>keep</th>
                  <th>backfill</th>
                  <th>min commits</th>
                  <th>refs</th>
                </tr>
              </thead>
              <tbody>
                {d.strategies.map((s) => (
                  <tr key={s.name}>
                    <td>
                      <strong>{s.name}</strong>
                    </td>
                    <td>
                      <span className={`pill ${s.kind}`}>{s.kind}</span>
                    </td>
                    <td>{s.base ?? "—"}</td>
                    <td>
                      <code>{s.schedule}</code>
                      <div className="muted small">{s.schedule_human}</div>
                    </td>
                    <td>{fmtTime(s.next)}</td>
                    <td>
                      {s.kind === "full" ? (
                        s.keep
                      ) : s.chain ? (
                        <span title="chained: each slot is cut on this strategy's previous bundle; every link since the newest kept base stays listed">
                          chain since {s.base}
                        </span>
                      ) : (
                        <span className="muted" title="cut on the base's newest bundle; the 2 newest are listed (D21)">2 newest</span>
                      )}
                    </td>
                    <td>{s.backfill_max || "∞"}</td>
                    <td>{s.kind === "full" ? <span className="muted">never gated</span> : s.min_commits}</td>
                    <td className="small">{s.refs.join(", ")}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
        <div className="pad small muted">
          Calendar slots, backfilled oldest-first; content = WAL state as of the slot; creationToken = slot epoch. Fulls are never gated by
          min_commits; an incremental below the floor is <span className="pill too-small">too-small</span> and the next slot catches up.
          Refs {d.bundles.main_only ? "main-only" : "all"} for this repository.
        </div>
      </Box>

      <Box title="Maintenance placement (host facts, read-only)">
        <KV
          rows={[
            ["checkpoints", d.maintenance.checkpoints ? `on · every ${d.maintenance.interval_secs}s pass` : "off"],
            ["compaction", d.compaction.enabled ? `on · trigger ${d.compaction.trigger_packs} packs / ${fmtBytes(d.compaction.trigger_bytes)}` : "off"],
            [
              "this instance",
              <span key="this-instance">
                <code>{host.name}</code> · roles {host.roles.join(", ")} ·{" "}
                {host.serves ? <span className="pill built">serves</span> : <span className="pill">not served here</span>}{" "}{host.maintains ? <span className="pill built">maintains</span> : <span className="pill">not maintained here</span>}
              </span>,
            ],
            ["capacity here", `${host.disk} · pack cap ${fmtBytes(host.max_pack_bytes || host.cache_budget_bytes)} · cache budget ${fmtBytes(host.cache_budget_bytes)}`],
            ["upstream follow", <UpstreamFollow key="upstream-follow" u={d.upstream} />],
            ["maintainers", <Maintainers key="maintainers" list={o.bundle_plan.maintainers} orphaned={o.bundle_plan.orphaned} label={false} />],
          ]}
        />
        <div className="pad small muted">
          Who maintains a repository is a host rule (<code>[placement] maintain / maintain_exclude</code> + declared capacity), not a repository
          setting. What those hosts will cut next, and the chain they have published, is on the <Link to={`/${full}/wal`}>WAL page</Link>; change
          strategies, min_commits or compaction triggers for <code>{full}</code> under “Effective config & history”.
        </div>
      </Box>
    </>
  );
}

/** `[upstream] follow`: which refs follow which host, and what the last round here did (D33). */
function UpstreamFollow({ u }: { u: SettingsDescribe["upstream"] }) {
  if (!u || u.follow.length === 0) {
    return (
      <span>
        <span className="pill">off</span>{" "}
        <span className="muted small">
          set <code>[upstream] git</code> + <code>follow = ["refs/heads/main"]</code> under “Effective config & history” — the maintaining host then
          publishes the upstream's moves as pushes
        </span>
      </span>
    );
  }
  const r = u.last_round;
  const pill = r ? (r.outcome === "in-sync" || r.outcome === "published" ? "pill built" : "pill stale") : "pill";
  return (
    <span>
      <code>{u.follow.join(", ")}</code> ← <code>{u.git}</code> · every {u.follow_interval_secs}s on the maintaining host
      {u.token_env ? "" : " · no token"}
      <div className="small">
        {r ? (
          <>
            <span className={pill}>{r.outcome}</span> {r.detail} · {fmtTime(r.at)}
            {r.outcome === "in-sync" && Object.entries(r.upstream).length > 0 && (
              <span className="muted"> · {Object.entries(r.upstream).map(([k, v]) => `${k} @ ${v.slice(0, 10)}`).join(", ")}</span>
            )}
          </>
        ) : (
          <span className="muted">no round on this instance yet (rounds run where the repository is maintained)</span>
        )}
      </div>
    </span>
  );
}

function KV({ rows }: { rows: [string, ReactNode][] }) {
  return (
    <dl className="kv">
      {rows.map(([k, v]) => (
        <div key={k}>
          <dt>{k}</dt>
          <dd>{v}</dd>
        </div>
      ))}
    </dl>
  );
}

// ---- 2. push policy ------------------------------------------------------------------

function PolicyEditor({ full }: { full: string }) {
  const saved = useData(`policy:${full}`, () => api.policy(full).get(), 10_000);
  const [text, setText] = useState(() => JSON.stringify(saved, null, 2));
  const [dirty, setDirty] = useState(false);
  const debounced = useDebounced(text, 400);
  const [validation, setValidation] = useState<PolicyValidation | null>(null);
  const [dry, setDry] = useState<PolicyDryRun | null>(null);
  const [busy, setBusy] = useState<"" | "dry" | "save">("");
  const [err, setErr] = useState("");
  const [last, setLast] = useState(20);
  const seq = useRef(0);

  useEffect(() => {
    if (!dirty) return;
    const n = ++seq.current;
    api
      .policy(full)
      .validate(debounced)
      .then((v) => {
        if (seq.current === n) setValidation(v);
      })
      .catch((e: Error) => {
        if (seq.current === n) setValidation({ ok: false, errors: [e.message] });
      });
  }, [debounced, dirty, full]);

  const dryRun = async () => {
    setBusy("dry");
    setErr("");
    try {
      setDry(await api.policy(full).dryRun(dirty ? text : "", last));
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy("");
    }
  };
  const save = async () => {
    setBusy("save");
    setErr("");
    try {
      await api.policy(full).put(JSON.parse(text));
      setDirty(false);
      invalidate(`policy:${full}`);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy("");
    }
  };
  const valid = !dirty || (validation?.ok ?? false);
  return (
    <>
      <Box title="Push policy (policy.json — docs/POLICY.md)">
        <div className="editor">
          <textarea
            className="code-input"
            spellCheck={false}
            rows={Math.min(30, Math.max(10, text.split("\n").length + 1))}
            value={text}
            onChange={(e) => {
              setText(e.target.value);
              setDirty(true);
            }}
            aria-label="policy.json"
          />
          <div className="editor-status" aria-live="polite">
            {!dirty && <span className="muted">saved policy{Object.keys(saved).length === 0 ? " (empty = allow-all)" : ""}</span>}
            {dirty && validation === null && <span className="muted">validating…</span>}
            {dirty && validation?.ok && (
              <span className="ok">
                valid · {validation.rules} rule(s), {validation.groups} group(s){validation.protect ? ", protect rules present" : ""}
              </span>
            )}
            {dirty && validation && !validation.ok && (
              <ul className="errors">
                {validation.errors.map((e) => (
                  <li key={e}>{e}</li>
                ))}
              </ul>
            )}
          </div>
          <div className="editor-actions">
            <label className="small">
              dry-run against the last{" "}
              <input type="number" min={1} max={200} value={last} onChange={(e) => setLast(Number(e.target.value) || 20)} className="num" /> pushes
            </label>
            <button type="button" className="btn small" disabled={busy !== "" || !valid} onClick={dryRun}>
              {busy === "dry" ? "running…" : "Dry-run"}
            </button>
            <button type="button" className="btn small primary" disabled={busy !== "" || !dirty || !valid} onClick={save}>
              {busy === "save" ? "saving…" : "Save policy"}
            </button>
            <button
              type="button"
              className="btn small"
              disabled={!dirty}
              onClick={() => {
                setText(JSON.stringify(saved, null, 2));
                setDirty(false);
                setValidation(null);
              }}
            >
              Discard
            </button>
          </div>
          {err && (
            <div className="flash error" role="alert">
              {err}
            </div>
          )}
        </div>
      </Box>
      {dry && (
        <Box title={`Dry-run: ${dry.pushes} push(es) · ${dry.allowed} ref update(s) allowed · ${dry.denied} denied`}>
          {dry.results.length === 0 ? (
            <div className="muted pad">No pushes in the live log to replay.</div>
          ) : (
            <table className="grid">
              <thead>
                <tr>
                  <th>seq</th>
                  <th>when</th>
                  <th>who</th>
                  <th>refs</th>
                </tr>
              </thead>
              <tbody>
                {dry.results.map((r) => (
                  <tr key={r.seq}>
                    <td>{r.seq}</td>
                    <td>{fmtTime(r.at)}</td>
                    <td>{r.principal}</td>
                    <td>
                      {r.refs.map((x) => (
                        <div key={x.name} className="small">
                          <span className={x.ok ? "pill built" : "pill missing"}>{x.ok ? "ok" : "ng"}</span> <code>{x.name}</code>
                          {x.force && <span className="pill">force</span>} {x.reason && <span className="muted">— {x.reason}</span>}
                        </div>
                      ))}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </Box>
      )}
    </>
  );
}

// ---- 3. effective config + history -----------------------------------------------

function EffectiveConfig({ d, full }: { d: SettingsDescribe; full: string }) {
  const history: SettingsHistory = useData(`settings-history:${full}`, () => api.settings(full).history(), 5_000);
  const [text, setText] = useState(d.settings.toml);
  const [dirty, setDirty] = useState(false);
  const [message, setMessage] = useState("");
  const debounced = useDebounced(text, 400);
  const [validation, setValidation] = useState<SettingsValidation | null>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState("");
  const [filter, setFilter] = useState("");
  const seq = useRef(0);

  useEffect(() => {
    if (!dirty) return;
    const n = ++seq.current;
    api
      .settings(full)
      .validate(debounced)
      .then((v) => {
        if (seq.current === n) setValidation(v);
      })
      .catch((e: Error) => {
        if (seq.current === n) setValidation({ ok: false, errors: [e.message] });
      });
  }, [debounced, dirty, full]);

  // Fields shown: the preview when the draft validates, else the live describe.
  const view: SettingsDescribe = dirty && validation?.ok ? validation : d;
  const fields = useMemo(() => view.fields.filter((f) => !filter || f.key.includes(filter)), [view, filter]);
  const publish = async (toml: string, msg: string) => {
    setBusy(true);
    setErr("");
    try {
      await api.settings(full).put(toml, msg);
      setDirty(false);
      setMessage("");
      invalidate(`settings:${full}`);
      invalidate(`settings-history:${full}`);
      invalidate(`overview:${full}`);
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const rev = d.settings.revision;
  const valid = !dirty || (validation?.ok ?? false);
  return (
    <>
      <Box title={rev ? `Repository settings — revision ${rev} by ${"author" in d.settings ? d.settings.author : ""}` : "Repository settings — none (host config)"}>
        <div className="editor">
          <textarea
            className="code-input"
            spellCheck={false}
            rows={Math.min(24, Math.max(8, text.split("\n").length + 1))}
            value={text}
            placeholder={"# TOML overrides of [bundles], [maintenance], [compaction], [upstream]\n[bundles]\nmin_commits = 25\n"}
            onChange={(e) => {
              setText(e.target.value);
              setDirty(true);
            }}
            aria-label="settings TOML"
          />
          <div className="editor-status" aria-live="polite">
            {!dirty && rev > 0 && "message" in d.settings && d.settings.message && <span className="muted">“{d.settings.message}”</span>}
            {dirty && validation === null && <span className="muted">validating…</span>}
            {dirty && validation?.ok && <span className="ok">valid — the table below previews the effective config</span>}
            {dirty && validation && !validation.ok && (
              <ul className="errors">
                {validation.errors.map((e) => (
                  <li key={e}>{e}</li>
                ))}
              </ul>
            )}
          </div>
          <div className="editor-actions">
            <input className="text" placeholder="why (shown in history)" value={message} onChange={(e) => setMessage(e.target.value)} />
            <button type="button" className="btn small primary" disabled={busy || !dirty || !valid} onClick={() => publish(text, message)}>
              {busy ? "publishing…" : `Publish as revision ${rev + 1}`}
            </button>
            <button
              type="button"
              className="btn small"
              disabled={!dirty}
              onClick={() => {
                setText(d.settings.toml);
                setDirty(false);
                setValidation(null);
              }}
            >
              Discard
            </button>
            {rev > 0 && (
              <button type="button" className="btn small danger" disabled={busy} onClick={() => publish("", "clear")}>
                Clear (back to host config)
              </button>
            )}
          </div>
          {err && (
            <div className="flash error" role="alert">
              {err}
            </div>
          )}
        </div>
      </Box>

      <Box
        title={
          <>
            Effective config ({fields.length} fields){" "}
            <input className="text small" placeholder="filter keys…" value={filter} onChange={(e) => setFilter(e.target.value)} aria-label="filter keys" />
          </>
        }
      >
        <div className="scroll-x">
          <table className="grid">
            <thead>
              <tr>
                <th>key</th>
                <th>value</th>
                <th>source</th>
              </tr>
            </thead>
            <tbody>
              {fields.map((f) => (
                <tr key={f.key} className={f.source === "setting" ? "setting-row" : ""}>
                  <td>
                    <code>{f.key}</code>
                  </td>
                  <td className="small">
                    <code>{show(f.value)}</code>
                    {f.source === "setting" && f.host_value !== undefined && f.host_value !== null && show(f.host_value) !== show(f.value) && (
                      <span className="muted"> (host: {show(f.host_value)})</span>
                    )}
                  </td>
                  <td>
                    {f.source === "setting" ? (
                      <span className="pill built" title={`set by the repository settings${rev ? ` @rev ${dirty ? rev + 1 : rev}` : ""}`}>
                        repo setting{rev ? ` @${dirty && validation?.ok ? rev + 1 : rev}` : ""}
                        {"author" in d.settings && d.settings.author && !dirty ? ` · ${d.settings.author}` : ""}
                      </span>
                    ) : (
                      <span className="pill" title="walgit.toml ⊕ WALGIT__ env on the answering host">
                        host config
                      </span>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </Box>

      <Box title={`History (${history.entries.length} change(s) in the live log)`}>
        {history.entries.length === 0 ? (
          <div className="muted pad">No settings changes since seq {history.min_seq} (older ones are folded into checkpoints).</div>
        ) : (
          <ol className="history">
            {history.entries.toReversed().map((e, i, arr) => {
              const prev = arr[i + 1]?.toml ?? "";
              return (
                <li key={e.seq}>
                  <div className="history-head">
                    <strong>revision {e.revision}</strong> <span className="muted">· seq {e.seq} · {fmtTime(e.at)} · {e.author}</span>
                    {e.message && <span> — {e.message}</span>}
                    {e.revision !== rev && (
                      <button
                        type="button"
                        className="btn small"
                        disabled={busy}
                        onClick={() => publish(e.toml, `revert to revision ${e.revision}`)}
                        title="Publish this document again as a new revision"
                      >
                        Revert to this
                      </button>
                    )}
                  </div>
                  <Diff before={prev} after={e.toml} />
                </li>
              );
            })}
          </ol>
        )}
      </Box>
    </>
  );
}

/** Minimal line diff (LCS-free: mark lines removed/added by set difference, in order). */
function Diff({ before, after }: { before: string; after: string }) {
  const a = before.split("\n").filter((l) => l.length);
  const b = after.split("\n").filter((l) => l.length);
  const aSet = new Set(a);
  const bSet = new Set(b);
  const lines: { t: "-" | "+" | " "; s: string }[] = [];
  for (const l of a) if (!bSet.has(l)) lines.push({ t: "-", s: l });
  for (const l of b) lines.push(aSet.has(l) ? { t: " ", s: l } : { t: "+", s: l });
  if (lines.length === 0) return <pre className="diff muted">(empty document)</pre>;
  return (
    <pre className="diff">
      {lines.map((l, i) => (
        <div key={i} className={l.t === "+" ? "add" : l.t === "-" ? "del" : ""}>
          {l.t} {l.s}
        </div>
      ))}
    </pre>
  );
}
