import { EmptyState } from "./ui/EmptyState";

/** 「预算」 page, not built yet: its task replaces this body. */
export function Budgets() {
  return (
    <div className="panel" role="region" aria-label="预算">
      <h2>预算</h2>
      <EmptyState text="即将上线" />
    </div>
  );
}
