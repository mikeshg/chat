/** Lightweight loading skeleton rows (no animation library). */
export function SkeletonLines({
  rows = 3,
  className = "",
  testId = "skeleton",
}: {
  rows?: number;
  className?: string;
  testId?: string;
}) {
  return (
    <div className={`sk-list ${className}`.trim()} data-testid={testId} aria-busy="true" aria-label="加载中">
      {Array.from({ length: rows }, (_, i) => (
        <div className="sk-row" key={i}>
          <div className="sk-avatar" />
          <div className="sk-mid">
            <div className="sk-line short" />
            <div className="sk-line" />
          </div>
        </div>
      ))}
    </div>
  );
}

export default function Skeleton(props: {
  rows?: number;
  variant?: "list" | "contacts" | "messages";
  testId?: string;
}) {
  return <SkeletonLines rows={props.rows ?? 6} testId={props.testId ?? "skeleton"} className={`skeleton-${props.variant ?? "list"}`} />;
}
