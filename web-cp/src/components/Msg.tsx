// Copied from web/src/components/Msg.tsx; comments translated.

export interface Note {
  text: string;
  /** `warn`: done, but not in effect yet (for example saved, not yet applied). */
  kind: "ok" | "warn" | "crit";
}

/** A result notice inside a panel. Takes no space when empty. */
export function Msg({ note }: { note: Note | null }) {
  if (!note) return null;
  return <div className={`note ${note.kind === "ok" ? "" : note.kind}`}>{note.text}</div>;
}
