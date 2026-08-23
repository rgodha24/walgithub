import { useEffect, useRef, useState } from "react";
import { api, type TaskRecord } from "../api";
import { reportError } from "../data";

/** Poll cadence while something runs / when idle (API.md §2c). */
const BUSY_MS = 1500;
const IDLE_MS = 15000;
/** How long a finished task stays listed in the dropdown. */
const LINGER_MS = 20000;

function kindLabel(kind: string): string {
  return kind.replace(/[-_]/g, " ");
}

/**
 * What the serving instance is doing to this repository right now
 * (materializing packs, indexing remote packs, checkpoint, bundle…), as a
 * compact indicator in the repo header: spinner + the name of the job (+N
 * more) + its percent. Clicking it opens a dropdown with every running task,
 * its latest progress, and the tasks that just finished. Polls `…/tasks`
 * fast while anything runs, slowly otherwise. Errors go to the tray.
 *
 * Requests are routed to a random instance, so a task id disappearing from
 * `running` only means "finished" when the same instance answered (or the
 * task shows up in `recent` with a result) — never when we simply landed on
 * another instance.
 */
export function TasksOverlay({ repo }: { repo: string }) {
  const [running, setRunning] = useState<TaskRecord[]>([]);
  const [justDone, setJustDone] = useState<TaskRecord[]>([]);
  const [host, setHost] = useState("");
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    let alive = true;
    let timer = 0;
    let seenHost = "";
    let seen = new Map<string, TaskRecord>();
    const tick = async () => {
      try {
        const t = await api.tasks(repo);
        if (!alive) return;
        setHost(t.hostname);
        const now = new Map(t.running.map((r) => [r.id, r]));
        const done: TaskRecord[] = [];
        for (const [id, prev] of seen) {
          if (now.has(id)) continue;
          const rec = t.recent.find((r) => r.id === id);
          if (rec) done.push(rec);
          else if (t.hostname === seenHost) done.push({ ...prev, ok: true, finished: prev.finished ?? new Date().toISOString(), summary: prev.summary || "done" });
          // else: a different instance answered; keep waiting for the owner.
        }
        for (const rec of done) if (rec.ok === false) reportError(new Error(rec.summary), `${kindLabel(rec.kind)} task`);
        seenHost = t.hostname;
        seen = now;
        setRunning(t.running);
        if (done.length) {
          const ids = new Set(done.map((d) => d.id));
          setJustDone((d) => [...d.filter((x) => !ids.has(x.id)), ...done].slice(-5));
          setTimeout(() => alive && setJustDone((d) => d.filter((x) => !ids.has(x.id))), LINGER_MS);
        }
        timer = window.setTimeout(tick, t.running.length ? BUSY_MS : IDLE_MS);
      } catch (e) {
        if (!alive) return;
        // A 404 means "no such repo here"; anything else is worth a line in the tray, once.
        if ((e as { status?: number }).status !== 404) reportError(e, "tasks");
        timer = window.setTimeout(tick, IDLE_MS);
      }
    };
    void tick();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [repo]);

  useEffect(() => {
    if (!open) return;
    const onDoc = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && setOpen(false);
    document.addEventListener("mousedown", onDoc);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDoc);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  // Nothing to show: render nothing (and close a stale dropdown).
  useEffect(() => {
    if (running.length === 0 && justDone.length === 0) setOpen(false);
  }, [running.length, justDone.length]);
  if (running.length === 0 && justDone.length === 0) return null;

  // The headline task: the one with progress, else the newest running, else the latest finished.
  const head = running.find((t) => t.progress) ?? running[0] ?? justDone[justDone.length - 1];
  if (!head) return null;
  const others = running.length > 1 ? running.length - 1 : 0;
  const pct = percentOf(head);
  const failed = justDone.some((t) => t.ok === false);

  return (
    <div className="tasks-indicator" ref={ref}>
      <button
        type="button"
        className={`tasks-pill ${running.length ? "busy" : "idle"} ${failed ? "failed" : ""}`}
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        aria-haspopup="true"
        title={running.length ? `${running.length} task${running.length === 1 ? "" : "s"} running on this instance` : "Recently finished tasks"}
      >
        {running.length ? <span className="spinner" aria-hidden /> : <span className={`dot ${failed ? "failed" : "ok"}`} aria-hidden />}
        <span className="task-kind">{kindLabel(head.kind)}</span>
        {others > 0 && <span className="muted">+{others}</span>}
        {pct !== undefined && <span className="muted tabular">{pct.toFixed(0)}%</span>}
        <span className="caret" aria-hidden>
          ▾
        </span>
      </button>
      {open && (
        <output className="tasks-pop" aria-live="polite">
          {running.length > 0 && (
            <div className="tasks-section">
              <div className="tasks-title muted small">Running</div>
              {running.map((t) => (
                <TaskLine key={t.id} t={t} />
              ))}
            </div>
          )}
          {justDone.length > 0 && (
            <div className="tasks-section">
              <div className="tasks-title muted small">Finished</div>
              {justDone.toReversed().map((t) => (
                <div key={t.id} className={`task done ${t.ok === false ? "failed" : ""}`}>
                  <span className={`dot ${t.ok === false ? "failed" : "ok"}`} aria-hidden />
                  <span className="task-kind">{kindLabel(t.kind)}</span>
                  <span className="task-text">{t.summary}</span>
                  <span className="muted small tabular">{fmtSecs(t.elapsed_ms)}</span>
                </div>
              ))}
            </div>
          )}
          {host && <div className="muted small task-host">instance {host.slice(0, 8)}</div>}
        </output>
      )}
    </div>
  );
}

function percentOf(t: TaskRecord | undefined): number | undefined {
  const p = t?.progress;
  if (!p) return undefined;
  const v = p.percent ?? (p.total ? (100 * p.done) / p.total : undefined);
  return v === undefined ? undefined : Math.min(100, Math.max(0, v));
}

function fmtSecs(ms: number): string {
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(s < 10 ? 1 : 0)}s`;
  const m = Math.floor(s / 60);
  return `${m}m ${Math.round(s - m * 60)}s`;
}

function TaskLine({ t }: { t: TaskRecord }) {
  const p = t.progress;
  const pct = percentOf(t);
  const last = t.log_tail.at(-1);
  return (
    <div className="task running">
      <div className="task-row">
        <span className="spinner" aria-hidden />
        <span className="task-kind">{kindLabel(t.kind)}</span>
        <span className="task-text">{p?.label ?? last ?? t.summary ?? "working…"}</span>
        {pct !== undefined && <span className="muted small tabular">{pct.toFixed(0)}%</span>}
        <span className="muted small tabular">{fmtSecs(t.elapsed_ms)}</span>
      </div>
      {pct !== undefined && (
        <span className="activity-bar" aria-hidden>
          <span style={{ width: `${pct.toFixed(1)}%` }} />
        </span>
      )}
    </div>
  );
}
