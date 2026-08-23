import type { Overview } from "../api";

type M = Overview["bundle_plan"]["maintainers"][number];

/** Alive within 10 min (the heartbeat is one per pass, a pass is ≤ 60 s). */
const ALIVE_SECS = 600;
/** A host nobody has heard from for this long is hidden behind a count; the store purges it at 24 h. */
const GONE_SECS = 12 * 3600;

const fmtBytes = (n: number) => {
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
const ago = (s: number) => (s < 90 ? `${s}s` : s < 5400 ? `${Math.round(s / 60)}m` : `${(s / 3600).toFixed(1)}h`);

/**
 * The hosts whose maintainer loop covers this repository. Alive = solid; silent hosts fade with
 * their silence (a restarted dev server, a rolled serverless revision) and after 12 h are only a
 * count — the maintainer purges their heartbeat at 24 h. What matters here is: is anyone alive?
 */
const age = (m: M) => m.last_pass_age_secs ?? Number.POSITIVE_INFINITY;

export function Maintainers({ list, orphaned, label = true }: { list: M[]; orphaned: boolean; label?: boolean }) {
  const alive = list.filter((m) => age(m) < ALIVE_SECS);
  const fading = list.filter((m) => age(m) >= ALIVE_SECS && age(m) < GONE_SECS).toSorted((a, b) => age(a) - age(b));
  const gone = list.filter((m) => age(m) >= GONE_SECS);
  return (
    <div className="maintainers small">
      {orphaned && (
        <div className="flash error" role="alert">
          Nobody maintains this repository right now: no maintainer has passed in the last {ALIVE_SECS / 60} minutes. Checkpoints, bundles and
          compaction wait until one is back.
        </div>
      )}
      {label && <span className="muted">Maintained by </span>}
      {alive.length === 0 && <span className="pill stale">nobody alive</span>}
      {alive.map((m) => (
        <span key={m.host} className="pill built" title={`${m.host}\n${m.disk} · ${fmtBytes(m.max_pack_bytes)} capacity · ${m.passes} passes\nlast unit: ${m.last_unit || "—"}`}>
          {m.host.slice(0, 8)} · {m.disk} · {ago(m.last_pass_age_secs ?? 0)} ago
        </span>
      ))}
      {fading.map((m) => {
        const a = age(m);
        const opacity = Math.max(0.15, 1 - (a - ALIVE_SECS) / (GONE_SECS - ALIVE_SECS));
        return (
          <span
            key={m.host}
            className="pill stale"
            style={{ opacity }}
            title={`${m.host}\nsilent for ${ago(a)} — a restarted or rolled host; hidden after 12 h, purged from the store after 24 h\nlast unit: ${m.last_unit || "—"}`}
          >
            {m.host.slice(0, 8)} · silent {ago(a)}
          </span>
        );
      })}
      {gone.length > 0 && (
        <span className="muted" title={gone.map((m) => `${m.host} — silent ${ago(age(m))}`).join("\n")}>
          {" "}
          +{gone.length} gone (purged after 24 h)
        </span>
      )}
    </div>
  );
}
