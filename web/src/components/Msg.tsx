export interface Note {
  text: string;
  kind: "ok" | "crit";
}

/** 面板内的结果提示。空则不占位。 */
export function Msg({ note }: { note: Note | null }) {
  if (!note) return null;
  return <div className={`note ${note.kind === "crit" ? "crit" : ""}`}>{note.text}</div>;
}
