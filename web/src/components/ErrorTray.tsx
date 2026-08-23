import { dismissError, useErrors } from "../data";

/** Every error the app knows about, visible (bottom-right), dismissable. */
export function ErrorTray() {
  const errors = useErrors();
  if (errors.length === 0) return null;
  return (
    <div className="error-tray" role="alert" aria-live="assertive">
      {errors.map((e) => (
        <div key={e.id} className="error-item">
          <div className="error-text">
            {e.status ? <span className="pill">HTTP {e.status}</span> : null} {e.text}
          </div>
          <button type="button" className="btn small" onClick={() => dismissError(e.id)} aria-label="Dismiss">
            ×
          </button>
        </div>
      ))}
      {errors.length > 1 && (
        <button type="button" className="btn small dismiss-all" onClick={() => dismissError()}>
          dismiss all
        </button>
      )}
    </div>
  );
}
