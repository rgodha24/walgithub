import { useState } from "react";
import type { Overview } from "../api";
import { Maintainers } from "./Maintainers";

type Slot = Overview["bundle_plan"]["slots"][number];

const slotTime = (s: number) => new Date(s * 1000).toISOString().slice(0, 16).replace("T", " ") + "Z";

/** Statuses that mean "somebody has to do something" — shown one per row. */
const ACTIONABLE = new Set<Slot["status"]>(["missing", "pending", "blocked", "wrong-host"]);

/**
 * The slot table as a plan, not a log: per strategy one line of settled history (built / skipped /
 * too-small / unavailable as counts, expandable) and every slot that still needs work on its own
 * row. Who can do the work (the maintainers) and what the next slot of each strategy will be.
 */
export function BundlePlan({ plan }: { plan: Overview["bundle_plan"] }) {
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const strategies = [...new Set(plan.slots.map((r) => r.strategy))];
  return (
    <>
      <div className="pad">
        <Maintainers list={plan.maintainers} orphaned={plan.orphaned} />
      </div>
      {plan.upcoming?.length > 0 && (
        <div className="pad small">
          <strong>Next slots</strong>
          <table className="kv compact">
            <tbody>
              {plan.upcoming.map((u) => (
                <tr key={u.strategy} className={u.host ? "" : "warn"}>
                  <th>{u.strategy}</th>
                  <td>
                    <code>{slotTime(u.slot)}</code> → {u.unit}
                    {!u.host && <span className="pill stale"> no live maintainer</span>}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {plan.slots.length === 0 ? (
        <div className="muted pad">No slots planned (bundles disabled or no strategies).</div>
      ) : (
        <table className="grid">
          <thead>
            <tr>
              <th>strategy</th>
              <th>slots</th>
              <th>detail</th>
            </tr>
          </thead>
          <tbody>
            {strategies.map((name) => {
              const rows = plan.slots.filter((r) => r.strategy === name);
              const settled = rows.filter((r) => !ACTIONABLE.has(r.status));
              const work = rows.filter((r) => ACTIONABLE.has(r.status));
              const counts = new Map<string, number>();
              for (const r of settled) counts.set(r.status, (counts.get(r.status) ?? 0) + 1);
              const key = `settled:${name}`;
              return (
                <SlotGroup key={name} name={name} kind={rows[0]?.kind ?? ""} settled={settled} counts={counts} work={work} open={!!open[key]} toggle={() => setOpen({ ...open, [key]: !open[key] })} />
              );
            })}
          </tbody>
        </table>
      )}
      <div className="pad small muted">
        A slot is the schedule's fire time; its bundle holds main as of that instant (token = slot epoch). <span className="pill pending">pending</span> =
        fired in the last 2 minutes (a writer's entry may still land); <span className="pill missing">missing</span> = work for the next pass, oldest
        first; <span className="pill skipped">skipped</span> = measured closed with nothing new, recorded so nobody re-measures it.
      </div>
    </>
  );
}

function SlotGroup({
  name,
  kind,
  settled,
  counts,
  work,
  open,
  toggle,
}: {
  name: string;
  kind: string;
  settled: Slot[];
  counts: Map<string, number>;
  work: Slot[];
  open: boolean;
  toggle: () => void;
}) {
  const built = settled.filter((r) => r.status === "built");
  const newest = built.at(-1);
  return (
    <>
      <tr>
        <td>
          <strong>{name}</strong> <span className="muted small">{kind}</span>
        </td>
        <td className="small">
          {[...counts.entries()].map(([st, n]) => (
            <span key={st} className={`pill ${st}`} style={{ marginRight: 4 }}>
              {n} {st}
            </span>
          ))}
          {settled.length > 0 && (
            <button className="btn link small" onClick={toggle}>
              {open ? "hide" : "show"}
            </button>
          )}
        </td>
        <td className="small muted">
          {newest ? (
            <>
              newest built <code>{slotTime(newest.slot)}</code> ({newest.detail})
            </>
          ) : (
            "nothing built yet"
          )}
        </td>
      </tr>
      {open &&
        settled.map((r) => (
          <tr key={`${r.strategy}-${r.slot}`} className="muted">
            <td />
            <td>
              <code>{slotTime(r.slot)}</code> <span className={`pill ${r.status}`}>{r.status}</span>
            </td>
            <td className="small">
              {r.bundle_id ? <code>{r.bundle_id}</code> : null} {r.detail}
            </td>
          </tr>
        ))}
      {work.map((r) => (
        <tr key={`${r.strategy}-${r.slot}`} className={r.status === "missing" ? "warn" : ""}>
          <td />
          <td>
            <code>{slotTime(r.slot)}</code> <span className={`pill ${r.status}`}>{r.status}</span>
          </td>
          <td className="small">{r.detail || (r.status === "missing" ? "next pass builds it" : "")}</td>
        </tr>
      ))}
    </>
  );
}
