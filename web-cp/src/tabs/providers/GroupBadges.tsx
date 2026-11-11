import type { ReactNode } from "react";
import { routeColor } from "../../ui/GroupName";

/**
 * Provider groups as badges, one per group, each with its route colour. A
 * visually hidden `、` sits between the names, so the badges read as the
 * list they are. Shared by the 上游账号 and 模型与路由 pages.
 */
export function GroupBadges({ groups, empty = "—" }: { groups: string[]; empty?: ReactNode }) {
  if (groups.length === 0) return <>{empty}</>;
  return (
    <span className="inline-flex max-w-full flex-wrap items-center gap-1 align-middle">
      {groups.map((g, i) => (
        <span key={g} className="tag gap-1.5 font-mono font-normal text-foreground">
          {i > 0 && <span className="vh">、</span>}
          <span aria-hidden="true" className="size-1.5 shrink-0 rounded-[2px]" style={{ background: routeColor(g) }} />
          {g}
        </span>
      ))}
    </span>
  );
}
