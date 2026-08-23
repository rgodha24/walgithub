import { Component, Suspense, type ErrorInfo, type ReactNode } from "react";
import { useLocation } from "react-router-dom";
import { ApiError } from "../api";
import { useActivity, usePending } from "../data";

/** Thin progress bar under the top bar while any fetch / chunk load is in flight. */
export function useBusy(): boolean {
  return usePending();
}

export function TopProgress() {
  const busy = usePending();
  const activity = useActivity();
  const pct = activity?.percent ?? (activity?.total ? (100 * (activity.done ?? 0)) / activity.total : undefined);
  return (
    <>
      {/* Decorative bar; the loading semantics live on <main aria-busy> (see Layout). */}
      <div className={busy ? "progress on" : "progress"} aria-hidden />
      {activity && (
        <output className="activity" aria-live="polite">
          <span className="activity-text">{activity.text}</span>
          {pct !== undefined && (
            <span className="activity-bar" aria-hidden>
              <span style={{ width: `${Math.min(100, Math.max(0, pct)).toFixed(1)}%` }} />
            </span>
          )}
          {pct !== undefined && <span className="muted small">{pct.toFixed(0)}%</span>}
        </output>
      )}
    </>
  );
}


/** Grey placeholder blocks shaped like the page that is about to render. */
export function Skeleton({ rows = 6, title = true }: { rows?: number; title?: boolean }) {
  return (
    <div className="skeleton" aria-busy="true" aria-live="polite" aria-label="Loading">
      {title && <div className="sk sk-title" />}
      <div className="box">
        {Array.from({ length: rows }, (_, i) => (
          <div key={i} className="sk sk-row" style={{ width: `${55 + ((i * 37) % 40)}%` }} />
        ))}
      </div>
    </div>
  );
}

function ErrorBox({ error }: { error: unknown }) {
  const e = error instanceof Error ? error : new Error(String(error));
  const status = e instanceof ApiError ? e.status : undefined;
  return (
    <div className="flash error" role="alert">
      <strong>{status === 404 ? "Not found" : status ? `Error ${status}` : "Error"}:</strong> {e.message}
    </div>
  );
}

type EBProps = { children: ReactNode; resetKey: string };
type EBState = { error?: unknown; key: string };

class ErrorBoundary extends Component<EBProps, EBState> {
  state: EBState = { key: this.props.resetKey };
  static getDerivedStateFromError(error: unknown): Partial<EBState> {
    return { error };
  }
  static getDerivedStateFromProps(props: EBProps, state: EBState): Partial<EBState> | null {
    // Navigating away clears the error.
    return props.resetKey !== state.key ? { error: undefined, key: props.resetKey } : null;
  }
  componentDidCatch(error: unknown, info: ErrorInfo) {
    if (!(error instanceof ApiError)) console.error(error, info.componentStack);
  }
  render() {
    return this.state.error !== undefined ? <ErrorBox error={this.state.error} /> : this.props.children;
  }
}

/**
 * Route content boundary: keeps the shell (top bar, repo header, tabs) stable
 * while a page suspends. Navigations are transitions (React Router 7 default),
 * so the previous page stays visible — with the top progress bar running —
 * until the next one has its data; brand-new boundaries show the skeleton.
 */
export function RouteBoundary({ children, fallback }: { children: ReactNode; fallback?: ReactNode }) {
  const { pathname } = useLocation();
  return (
    <ErrorBoundary resetKey={pathname}>
      <Suspense fallback={fallback ?? <Skeleton />}>{children}</Suspense>
    </ErrorBoundary>
  );
}
