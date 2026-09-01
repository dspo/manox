// Usage-number formatting shared by the info card. Mirrors the host's two
// token conventions: lowercase `m` for totals and window labels, uppercase
// `M` (integral above 10M) for the compact ↑/↓/R/CH symbol lines.

/** Totals and window labels: `1.0m` / `104.2k` / verbatim below 1k. */
export function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}m`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

/** Compact symbol lines: `14M` above 10M, `1.0M` above 1M, `104.2k` above 1k. */
export function formatTokensPi(n: number): string {
  if (n >= 10_000_000) return `${Math.floor(n / 1_000_000)}M`;
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

/** Cost tiers: `$1.23` at dollars, `$0.023` at cents, `$0.0001` below. */
export function formatCost(cost: number): string {
  if (cost >= 1) return `$${cost.toFixed(2)}`;
  if (cost >= 0.01) return `$${cost.toFixed(3)}`;
  return `$${cost.toFixed(4)}`;
}
