import { useState } from "react";

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
 */
export function SecretInput({
  label,
  stored,
  required,
  readOnly = false,
  hint,
  onChange,
}: {
  label: string;
  stored: boolean;
  required: boolean;
  readOnly?: boolean;
  hint?: string;
  onChange: (value: string | undefined) => void;
}) {
  const [mode, setMode] = useState<"keep" | "replace" | "clear">(stored ? "keep" : "replace");
  const [typed, setTyped] = useState("");

  const keep = () => {
    setMode("keep");
    setTyped("");
    onChange(REDACTED);
  };

  if (mode === "keep" || mode === "clear") {
    return (
      <div className="f" role="group" aria-label={label}>
        <span>{label}</span>
        <div className="secret">
          {mode === "keep" ? (
            <span className="pill ok">已设置</span>
          ) : (
            <span className="pill crit">保存后清除</span>
          )}
          {!readOnly && mode === "keep" && (
            <button
              type="button"
              className="ghost"
              onClick={() => {
                setMode("replace");
                onChange("");
              }}
            >
              替换
            </button>
          )}
          {!readOnly && mode === "keep" && !required && (
            <button
              type="button"
              className="ghost"
              onClick={() => {
                setMode("clear");
                onChange(undefined);
              }}
            >
              清除
            </button>
          )}
          {!readOnly && mode === "clear" && (
            <button type="button" className="ghost" onClick={keep}>
              撤销
            </button>
          )}
        </div>
        {hint && <p className="hint">{hint}</p>}
      </div>
    );
  }

  return (
    <div className="f">
      <label>
        <span>{label}</span>
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
        />
      </label>
      {stored && !readOnly && (
        <button type="button" className="ghost secret-undo" onClick={keep}>
          保留原密钥
        </button>
      )}
      {hint && <p className="hint">{hint}</p>}
    </div>
  );
}
