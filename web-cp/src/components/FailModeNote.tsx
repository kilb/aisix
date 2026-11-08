import { useEffect, useState } from "react";
import { resources } from "../lib/api";
import { errorText } from "../lib/fmt";

const MODE_LABEL: Record<string, string> = { sticky: "保持最后状态", closed: "关闭" };

export const FAIL_MODE_TEXT =
  "这个设置对整个环境的所有租户生效，不能按租户单独设置。控制面不可达时：sticky 继续执行最后收到的拦截标记，其余请求照常放行；closed 在宽限时间过后拒绝所有受预算约束的请求。每个客户租户都有余额标记，所以 closed 在宽限时间过后会拒绝所有客户的请求。";

/** The mode a stored `budget_policy` document names; `sticky` when there is none, as the gateway behaves. */
export function failModeOf(docs: readonly { fail_mode?: unknown }[]): string {
  const mode = docs[0]?.fail_mode;
  return typeof mode === "string" ? mode : "sticky";
}

/** The note for a known mode. `onEdit` adds the link to the stored document. */
export function FailModeView({ mode, onEdit }: { mode: string; onEdit?: () => void }) {
  return (
    <div className="note" role="note" aria-label="预算失败模式">
      <p>
        预算失败模式：<strong>{`${mode}: ${MODE_LABEL[mode] ?? mode}`}</strong>
      </p>
      <p>{FAIL_MODE_TEXT}</p>
      {onEdit && (
        <button type="button" className="link-button" onClick={onEdit}>
          去修改
        </button>
      )}
    </div>
  );
}

/** Reads the `budget_policy` singleton and explains what its mode does. */
export function FailModeNote({ onEdit }: { onEdit?: () => void }) {
  const [mode, setMode] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    resources
      .list<{ fail_mode?: unknown }>("budget_policy")
      .then((views) => live && setMode(failModeOf(views.map((v) => v.doc))))
      .catch((e: unknown) => live && setError(errorText(e)));
    return () => {
      live = false;
    };
  }, []);
  if (error) {
    return (
      <div className="note crit" role="alert">
        {error}
      </div>
    );
  }
  return mode === null ? null : <FailModeView mode={mode} onEdit={onEdit} />;
}
