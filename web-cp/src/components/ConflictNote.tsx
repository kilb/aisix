/**
 * A save answered `412 version_mismatch`: someone else changed the row
 * since this edit started. The edit is not retried; the operator reloads.
 */
export function ConflictNote({ onReload }: { onReload: () => void }) {
  return (
    <div className="note warn" role="alert">
      该资源已被他人修改。重新载入后再编辑。{" "}
      <button type="button" className="ghost" onClick={onReload}>
        重新载入
      </button>
    </div>
  );
}
