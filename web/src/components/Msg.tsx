export interface Note {
  text: string;
  /** `warn`：做了，但还没生效（比如配置写了、网关没收到重载信号）。 */
  kind: "ok" | "warn" | "crit";
}

/** 面板内的结果提示。空则不占位。 */
export function Msg({ note }: { note: Note | null }) {
  if (!note) return null;
  return <div className={`note ${note.kind === "ok" ? "" : note.kind}`}>{note.text}</div>;
}
