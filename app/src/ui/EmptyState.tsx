/** Chinese empty / search-empty with a simple inline SVG. */
export default function EmptyState({
  title,
  hint,
  detail,
  testId = "empty-state",
}: {
  title: string;
  hint?: string;
  detail?: string;
  testId?: string;
}) {
  const sub = hint ?? detail;
  return (
    <div className="empty-state" data-testid={testId}>
      <svg
        className="empty-svg"
        width="72"
        height="72"
        viewBox="0 0 72 72"
        fill="none"
        aria-hidden
      >
        <rect x="12" y="18" width="48" height="36" rx="8" stroke="currentColor" strokeWidth="2" opacity="0.35" />
        <circle cx="28" cy="34" r="3" fill="currentColor" opacity="0.35" />
        <path d="M12 46l12-10 8 6 14-12 14 16" stroke="currentColor" strokeWidth="2" opacity="0.35" fill="none" />
      </svg>
      <div className="empty-title">{title}</div>
      {sub && <div className="empty-hint-sm">{sub}</div>}
    </div>
  );
}
