import { useState } from "react";

async function copyText(text: string) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
}

const ClipboardIcon = () => (
  <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden>
    <path
      fill="currentColor"
      d="M0 6.75C0 5.784.784 5 1.75 5h1.5a.75.75 0 0 1 0 1.5h-1.5a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-1.5a.75.75 0 0 1 1.5 0v1.5A1.75 1.75 0 0 1 9.25 16h-7.5A1.75 1.75 0 0 1 0 14.25Z"
    />
    <path
      fill="currentColor"
      d="M5 1.75C5 .784 5.784 0 6.75 0h7.5C15.216 0 16 .784 16 1.75v7.5A1.75 1.75 0 0 1 14.25 11h-7.5A1.75 1.75 0 0 1 5 9.25Zm1.75-.25a.25.25 0 0 0-.25.25v7.5c0 .138.112.25.25.25h7.5a.25.25 0 0 0 .25-.25v-7.5a.25.25 0 0 0-.25-.25Z"
    />
  </svg>
);

const CheckIcon = () => (
  <svg viewBox="0 0 16 16" width="14" height="14" aria-hidden>
    <path
      fill="currentColor"
      d="M13.78 4.22a.75.75 0 0 1 0 1.06l-7.25 7.25a.75.75 0 0 1-1.06 0L2.22 9.28a.751.751 0 0 1 .018-1.042.751.751 0 0 1 1.042-.018L6 10.94l6.72-6.72a.75.75 0 0 1 1.06 0Z"
    />
  </svg>
);

/**
 * Copy-to-clipboard button. `text` may be a string or a function returning a
 * (possibly async) string — used when the clipboard content is fetched lazily
 * (e.g. the installer script). Shows a check mark for a moment after copying.
 */
export function CopyButton({
  text,
  label,
  className = "",
}: {
  text: string | (() => string | Promise<string>);
  label?: string;
  className?: string;
}) {
  const [state, setState] = useState<"idle" | "done" | "error">("idle");
  return (
    <button
      type="button"
      className={`copy-btn ${label ? "copy-btn-labelled" : ""} ${className}`}
      title="Copy to clipboard"
      aria-label={label ?? "Copy to clipboard"}
      onClick={async () => {
        try {
          const t = typeof text === "function" ? await text() : text;
          await copyText(t);
          setState("done");
        } catch {
          setState("error");
        }
        setTimeout(() => setState("idle"), 1500);
      }}
    >
      {state === "done" ? <CheckIcon /> : <ClipboardIcon />}
      {label && <span>{state === "done" ? "Copied" : state === "error" ? "Failed" : label}</span>}
    </button>
  );
}

/** A code sample with a copy icon in the corner. */
export function CodeSample({ code, copy }: { code: string; copy?: string | (() => string | Promise<string>) }) {
  return (
    <div className="code-sample">
      <pre className="code-block">{code}</pre>
      <CopyButton text={copy ?? code} />
    </div>
  );
}
