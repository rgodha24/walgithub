import { startTransition, use, useEffect, useReducer, useSyncExternalStore } from "react";

/**
 * Suspense data layer.
 *
 * `useData(key, fn, ttl)` suspends until `fn()` resolves the first time and
 * returns the value from then on. Entries older than `ttl` are revalidated in
 * the background; the re-render is wrapped in `startTransition`, so stale
 * content stays on screen (no fallback flash) until the fresh value is ready.
 * Sha-addressed data never changes, so callers pass `ttl: Infinity` for it.
 *
 * Every in-flight request (and lazy chunk load, see `track`) bumps a global
 * pending counter that drives the top progress bar.
 */

type Entry<T> = {
  key: string;
  promise: Promise<T>;
  status: "pending" | "ok" | "error";
  value?: T;
  error?: unknown;
  at: number; // when the current value/promise was created
};

const cache = new Map<string, Entry<unknown>>();
const listeners = new Map<string, Set<() => void>>();
const MAX_ENTRIES = 400;

function notify(key: string) {
  for (const l of listeners.get(key) ?? []) l();
}

function evict() {
  if (cache.size <= MAX_ENTRIES) return;
  // Map iterates in insertion order: drop the oldest settled entries.
  for (const [k, e] of cache) {
    if (e.status !== "pending") cache.delete(k);
    if (cache.size <= MAX_ENTRIES * 0.8) break;
  }
}

function start<T>(key: string, fn: () => Promise<T>, prev?: Entry<T>): Entry<T> {
  const entry: Entry<T> = {
    key,
    status: prev?.value !== undefined ? "ok" : "pending",
    value: prev?.value,
    at: Date.now(),
    promise: undefined as unknown as Promise<T>,
  };
  entry.promise = track(fn()).then(
    (value) => {
      // Replace the entry immutably so `useSyncExternalStore` sees a new snapshot.
      if (cache.get(key) === entry) {
        cache.set(key, { ...entry, status: "ok", value, error: undefined, at: Date.now() });
        notify(key);
      }
      return value;
    },
    (error: unknown) => {
      if (cache.get(key) === entry) {
        // Keep stale data if we had some; only fresh loads surface the error
        // through Suspense — a failed background refresh goes to the tray.
        if (entry.value !== undefined) reportError(error, `refresh ${key.split(":")[0]}`);
        cache.set(key, { ...entry, status: entry.value === undefined ? "error" : "ok", error });
        notify(key);
      }
      throw error;
    },
  );
  cache.delete(key); // re-insert at the end (LRU-ish order)
  cache.set(key, entry);
  evict();
  return entry;
}

function ensure<T>(key: string, fn: () => Promise<T>, ttl: number): Entry<T> {
  const cur = cache.get(key) as Entry<T> | undefined;
  if (!cur) return start(key, fn);
  if (cur.status !== "pending" && Date.now() - cur.at > ttl) return start(key, fn, cur);
  return cur;
}

/** Suspend on `fn()` keyed by `key`; revalidate in the background after `ttl` ms. */
export function useData<T>(key: string, fn: () => Promise<T>, ttl = 5_000): T {
  const [, force] = useReducer((n: number) => n + 1, 0);
  useEffect(() => {
    const set = listeners.get(key) ?? listeners.set(key, new Set()).get(key)!;
    const l = () => startTransition(() => force());
    set.add(l);
    return () => {
      set.delete(l);
      if (set.size === 0) listeners.delete(key);
    };
  }, [key]);
  const entry = ensure(key, fn, ttl);
  if (entry.status === "ok") return entry.value as T;
  if (entry.status === "error") throw entry.error;
  return use(entry.promise);
}

/** Mark entries stale (e.g. after a mutation): mounted readers refetch in the
 * background right away (keeping what they show), others on their next read. */
export function invalidate(prefix: string) {
  for (const [k, e] of cache) {
    if (!k.startsWith(prefix)) continue;
    if (e.status === "pending") continue;
    cache.set(k, { ...e, at: 0 });
    notify(k);
  }
}

// ---- pending counter → top progress bar -------------------------------------

let pending = 0;
const pendingListeners = new Set<() => void>();
function setPending(d: number) {
  pending += d;
  for (const l of pendingListeners) l();
}
/** Count a promise (fetch, lazy chunk import…) towards the global loading state. */
export function track<T>(p: Promise<T>): Promise<T> {
  setPending(1);
  return p.finally(() => setPending(-1));
}
export function usePending(): boolean {
  return useSyncExternalStore(
    (l) => {
      pendingListeners.add(l);
      return () => pendingListeners.delete(l);
    },
    () => pending > 0,
    () => false,
  );
}

// ---- live activity (SSE envelope notices/progress) ----------------------------

export interface Activity {
  text: string;
  done?: number;
  total?: number;
  percent?: number;
}
const activities = new Map<string, Activity>();
const activityListeners = new Set<() => void>();
let activitySnapshot: Activity | null = null;
/** Record what a long request is doing right now (`null` = finished). */
export function setActivity(key: string, a: Activity | null) {
  if (a) activities.set(key, a);
  else activities.delete(key);
  // Newest activity wins; `null` when idle.
  activitySnapshot = [...activities.values()].at(-1) ?? null;
  for (const l of activityListeners) l();
}
export function useActivity(): Activity | null {
  return useSyncExternalStore(
    (l) => {
      activityListeners.add(l);
      return () => activityListeners.delete(l);
    },
    () => activitySnapshot,
    () => null,
  );
}

// ---- SSE ----------------------------------------------------------------------

export interface SseEvent {
  event: string;
  data: string;
}

/**
 * Read a `text/event-stream` response (GET or POST — `EventSource` is GET-only
 * and cannot send `Accept`/auth headers) and invoke `onEvent` per event as
 * soon as it arrives. Resolves when the stream ends; rejects on HTTP errors.
 */
export async function readSse(
  input: string | Response,
  init: RequestInit,
  onEvent: (ev: SseEvent) => void,
  onError: (status: number, text: string) => Error,
): Promise<void> {
  const r =
    input instanceof Response
      ? input
      : await fetch(input, {
          ...init,
          headers: { ...(init.headers as Record<string, string>), Accept: "text/event-stream" },
          credentials: "same-origin",
        });
  if (!r.ok || !r.body) throw onError(r.status, (await r.text()).trim() || r.statusText);
  const reader = r.body.getReader();
  const dec = new TextDecoder();
  let buf = "";
  const dispatch = (chunk: string) => {
    let event = "message";
    const data: string[] = [];
    for (const line of chunk.split("\n")) {
      if (line.startsWith("event:")) event = line.slice(6).trim();
      else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
      // `id:`, `retry:` and comments (`:`) are ignored.
    }
    if (data.length) onEvent({ event, data: data.join("\n") });
  };
  // oxlint-disable-next-line no-await-in-loop -- streaming read is inherently sequential
  for (let chunk = await reader.read(); !chunk.done; chunk = await reader.read()) {
    buf += dec.decode(chunk.value, { stream: true });
    let i: number;
    while ((i = buf.indexOf("\n\n")) >= 0) {
      dispatch(buf.slice(0, i));
      buf = buf.slice(i + 2);
    }
  }
  buf += dec.decode();
  if (buf.trim()) dispatch(buf);
}

// ---- error tray -----------------------------------------------------------------
//
// Every error that would otherwise only reach the console (unhandled promise
// rejections, window errors, failed background revalidations, aborted
// streams) is recorded here and shown by `<ErrorTray>` in the layout, so a
// user never sees "nothing happened" without a reason on screen.

export interface TrayError {
  id: number;
  at: number;
  text: string;
  detail?: string;
  status?: number;
}
const trayErrors: TrayError[] = [];
const trayListeners = new Set<() => void>();
let traySnapshot: TrayError[] = [];
let trayId = 0;
const TRAY_MAX = 6;

export function reportError(err: unknown, context?: string) {
  const e = err instanceof Error ? err : new Error(String(err));
  // Aborted fetches are user intent (navigation, new keystroke), not errors.
  if (e.name === "AbortError") return;
  const status = (e as { status?: number }).status;
  const text = context ? `${context}: ${e.message}` : e.message;
  // Collapse exact repeats.
  const last = trayErrors.at(-1);
  if (last && last.text === text && Date.now() - last.at < 2000) return;
  trayErrors.push({ id: ++trayId, at: Date.now(), text, detail: e.stack?.split("\n").slice(1, 3).join(" "), status });
  while (trayErrors.length > TRAY_MAX) trayErrors.shift();
  traySnapshot = [...trayErrors];
  for (const l of trayListeners) l();
}
export function dismissError(id?: number) {
  if (id === undefined) trayErrors.length = 0;
  else {
    const i = trayErrors.findIndex((e) => e.id === id);
    if (i >= 0) trayErrors.splice(i, 1);
  }
  traySnapshot = [...trayErrors];
  for (const l of trayListeners) l();
}
export function useErrors(): TrayError[] {
  return useSyncExternalStore(
    (l) => {
      trayListeners.add(l);
      return () => trayListeners.delete(l);
    },
    () => traySnapshot,
    () => traySnapshot,
  );
}
if (typeof window !== "undefined") {
  window.addEventListener("unhandledrejection", (ev) => reportError(ev.reason, "unhandled"));
  window.addEventListener("error", (ev) => reportError(ev.error ?? ev.message));
}
