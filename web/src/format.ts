/** Human-readable formatting helpers (shared by pages and components). */

export function relTime(iso: string): string {
  let v = Math.round((Date.now() - new Date(iso).getTime()) / 1000);
  const steps: [number, string][] = [
    [60, "second"],
    [60, "minute"],
    [24, "hour"],
    [30, "day"],
    [12, "month"],
    [Infinity, "year"],
  ];
  for (const [div, name] of steps) {
    if (Math.abs(v) < div) return `${v} ${name}${Math.abs(v) === 1 ? "" : "s"} ago`;
    v = Math.round(v / div);
  }
  return iso;
}

export function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}
