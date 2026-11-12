import type { ReactNode } from "react";

/** 分组名保留为完整标签；隐藏分隔符让辅助技术按列表朗读。 */
export function GroupBadges({ groups, empty = "—" }: { groups: string[]; empty?: ReactNode }) {
  if (groups.length === 0) return <>{empty}</>;
  return (
    <span className="inline-flex max-w-full flex-wrap items-center gap-1 align-middle">
      {groups.map((g, i) => (
        <span key={g} className="tag gap-1.5 font-mono font-normal text-ink">
          {i > 0 && <span className="vh">、</span>}
          <span aria-hidden="true" className="size-1.5 shrink-0 rounded-full bg-graphite" />
          {g}
        </span>
      ))}
    </span>
  );
}
