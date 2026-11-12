import { useId, useState } from "react";
import { Button } from "../ui/Button";
import { Lamp } from "../ui/Lamp";

/** What the control plane reads every stored credential back as. */
export const REDACTED = "[REDACTED]";

/**
 * A credential field that never shows a stored value.
 *
 * - With `stored`, it shows "已设置" and `替换` (plus `清除` when the field is
 *   not `required`); there is never a text box holding the placeholder.
 * - `onChange` is given what to send: `REDACTED` while the stored value is
 *   kept (the control plane then keeps it), the typed value once replaced,
 *   or `undefined` once cleared (the field is then left out).
 * - A replacement is required to be non-empty: an empty string is never
 *   sent in place of a stored secret.
 * - `readOnly` (a viewer) shows the state without any control.
 * - `problem` is the control plane's refusal of the field, shown under it.
 */
export function SecretInput({
  label,
  stored,
  required,
  readOnly = false,
  hint,
  problem = null,
  onChange,
}: {
  label: string;
  stored: boolean;
  required: boolean;
  readOnly?: boolean;
  hint?: string;
  problem?: string | null;
  onChange: (value: string | undefined) => void;
}) {
  const [mode, setMode] = useState<"keep" | "replace" | "clear">(stored ? "keep" : "replace");
  const [typed, setTyped] = useState("");
  const problemId = useId();
  const aria = problem ? { "aria-invalid": true as const, "aria-describedby": problemId } : {};
  const why = problem && (
    <p id={problemId} className="field-problem">
      {problem}
    </p>
  );

  const keep = () => {
    setMode("keep");
    setTyped("");
    onChange(REDACTED);
  };

  if (mode === "keep" || mode === "clear") {
    return (
      <div className="f" role="group" aria-label={label} {...aria}>
        <span className="f-label">{label}</span>
        {/* Drawn as the field it stands for: the stored state where the text would be, the actions at its end. */}
        <div className="secret min-h-11 flex-wrap gap-2 rounded-xl border border-line bg-well p-1 pl-3 text-ink-soft">
          {mode === "keep" ? (
            <Lamp state="applied">已设置</Lamp>
          ) : (
            <Lamp state="pending">保存后清除</Lamp>
          )}
          <span aria-hidden="true" className="min-w-0 flex-1 truncate font-mono text-meta tracking-widest text-ink-soft">
            {mode === "keep" ? "••••••••" : ""}
          </span>
          {!readOnly && mode === "keep" && (
            <Button
              type="button"
              size="sm"
              kind="quiet"
              className="h-11"
              onClick={() => {
                setMode("replace");
                onChange("");
              }}
            >
              替换
            </Button>
          )}
          {!readOnly && mode === "keep" && !required && (
            <Button
              type="button"
              size="sm"
              kind="quiet"
              className="h-11"
              onClick={() => {
                setMode("clear");
                onChange(undefined);
              }}
            >
              清除
            </Button>
          )}
          {!readOnly && mode === "clear" && (
            <Button type="button" size="sm" kind="quiet" className="h-11" onClick={keep}>
              撤销
            </Button>
          )}
        </div>
        {hint && <p className="hint">{hint}</p>}
        {why}
      </div>
    );
  }

  return (
    <div className="f">
      <label>
        <span className="f-label">{label}</span>
        <input
          type="password"
          autoComplete="new-password"
          // A stored secret is replaced by a value, never by nothing.
          required={required || stored}
          disabled={readOnly}
          value={typed}
          onChange={(e) => {
            setTyped(e.target.value);
            onChange(e.target.value === "" && !stored && !required ? undefined : e.target.value);
          }}
          {...aria}
        />
      </label>
      {stored && !readOnly && (
        <Button type="button" size="sm" kind="quiet" className="secret-undo -ml-2 self-start" onClick={keep}>
          保留原密钥
        </Button>
      )}
      {hint && <p className="hint">{hint}</p>}
      {why}
    </div>
  );
}
