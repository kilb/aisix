import { Fragment, useId } from "react";

/** The route colour a name is drawn in: one of six, by a stable hash of the name. */
export function routeColor(name: string): string {
  let h = 0;
  for (const ch of name) h = (Math.imul(h, 31) + ch.codePointAt(0)!) >>> 0;
  return `var(--route-${(h % 6) + 1})`;
}

/** A provider group's name with its short route-colour bar. Dashboard only. */
export function GroupName({ name }: { name: string }) {
  return (
    <span className="grp" style={{ "--c": routeColor(name) } as React.CSSProperties}>
      {name}
    </span>
  );
}

/**
 * Provider groups in a table cell: the first one, then a `+N` chip whose
 * full list shows on hover and is read out in full. The names keep their
 * `、` separators in the text, so the cell reads as the list it is.
 */
export function GroupList({ groups }: { groups: string[] }) {
  const tipId = useId();
  const [first, ...rest] = groups;
  if (first === undefined) return null;
  if (rest.length === 0) return <GroupName name={first} />;
  return (
    <span className="grps grps-one">
      <GroupName name={first} />
      <span className="has-tip">
        <span className="tag more" tabIndex={0} aria-describedby={tipId}>
          <span aria-hidden="true">+{rest.length}</span>
        </span>
        <span className="tip" role="tooltip" id={tipId}>
          {groups.join("、")}
        </span>
      </span>
      <span className="vh">
        {rest.map((g) => (
          <Fragment key={g}>、{g}</Fragment>
        ))}
      </span>
    </span>
  );
}
