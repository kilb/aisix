import { useEffect, useState, type ComponentType } from "react";
import type { TabProps } from "../App";
import type { OperatorRole } from "../lib/api";
import { EmptyState } from "../ui/EmptyState";

/** One section of a tab that holds several: a page of its own, shown to operators of `minRole` and above. */
export interface Section {
  id: string;
  label: string;
  Component: ComponentType<TabProps>;
  minRole: OperatorRole;
}

const RANK: Record<OperatorRole, number> = { viewer: 0, operator: 1, admin: 2 };

/**
 * A tab made of sections: a switch between the sections the operator's role
 * reaches, and the open one's page. With none, a placeholder stands in, so a
 * tab can be registered before its pages exist.
 *
 * `initialSection` is not only a start: a link that names a section while this
 * tab is already open must switch to it, so the prop is followed whenever it
 * arrives. `sectionVisit` counts those arrivals, so asking for the section
 * already named again still switches: the string alone would not change.
 */
export function SectionHost({
  label,
  sections,
  initialSection,
  sectionVisit,
  ...props
}: TabProps & { label: string; sections: readonly Section[] }) {
  const shown = sections.filter((s) => RANK[props.operator.role] >= RANK[s.minRole]);
  const [open, setOpen] = useState<string | null>(initialSection ?? null);
  useEffect(() => {
    if (initialSection) setOpen(initialSection);
  }, [initialSection, sectionVisit]);
  // A section this build does not register yet closes nothing: the first one stands in.
  const current = shown.find((s) => s.id === open) ?? shown[0];

  if (!current) {
    return (
      <div className="panel text-body" role="region" aria-label={label}>
        <h2 className="font-mono text-meta font-medium text-ink">{label}</h2>
        <EmptyState text="即将上线" />
      </div>
    );
  }
  const { Component } = current;
  return (
    <div className="sections min-w-0" role="region" aria-label={label}>
      {shown.length > 1 && (
        <div className="sections-switch mb-7 min-h-11 gap-1 rounded-full border-0 bg-well p-1" role="group" aria-label={`${label}分区`}>
          {shown.map((s) => (
            <button key={s.id} type="button" className="h-11 shrink-0 rounded-full px-4 font-mono text-meta font-medium text-ink-soft after:hidden hover:bg-surface hover:text-ink aria-pressed:bg-ink aria-pressed:text-surface" aria-pressed={s.id === current.id} onClick={() => setOpen(s.id)}>
              {s.label}
            </button>
          ))}
        </div>
      )}
      <Component {...props} />
    </div>
  );
}
