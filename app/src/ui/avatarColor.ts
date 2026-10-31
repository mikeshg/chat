/** Stable palette for missing-avatar fallback (identity → color). */
const PALETTE = [
  "#4F46E5",
  "#7C3AED",
  "#DB2777",
  "#D97706",
  "#059669",
  "#2563EB",
  "#DC2626",
  "#0D9488",
  "#9333EA",
  "#CA8A04",
];

/** Hash user id (or name) to a palette color; same id → same color. */
export function avatarColor(id?: string | null): string {
  const s = (id ?? "").trim();
  if (!s) return PALETTE[0];
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return PALETTE[(h >>> 0) % PALETTE.length];
}
