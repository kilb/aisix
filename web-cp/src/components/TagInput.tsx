import { useState } from "react";

/**
 * A list of short names, such as provider groups: one chip per name with
 * its own remove button, and a box that adds the typed name on Enter or
 * `添加`. Names are trimmed; an empty or repeated name is not added.
 * `readOnly` (a viewer, or a form awaiting confirmation) shows the chips
 * only.
 */
export function TagInput({
  label,
  value,
  onChange,
  readOnly = false,
  hint,
}: {
  label: string;
  value: string[];
  onChange: (next: string[]) => void;
  readOnly?: boolean;
  hint?: string;
}) {
  const [draft, setDraft] = useState("");

  const add = () => {
    const name = draft.trim();
    if (name !== "" && !value.includes(name)) onChange([...value, name]);
    setDraft("");
  };

  return (
    <div className="f" role="group" aria-label={label}>
      <span>{label}</span>
      {value.length === 0 ? (
        <p className="hint">未设置</p>
      ) : (
        <ul className="tags">
          {value.map((name) => (
            <li key={name}>
              {name}
              {!readOnly && (
                <button
                  type="button"
                  aria-label={`移除 ${name}`}
                  title={`移除 ${name}`}
                  onClick={() => onChange(value.filter((v) => v !== name))}
                />
              )}
            </li>
          ))}
        </ul>
      )}
      {!readOnly && (
        <div className="tag-add">
          <input
            aria-label={`添加${label}`}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              // Enter would submit the form; while an IME composes, it
              // confirms the composition instead.
              if (e.key === "Enter" && !e.nativeEvent.isComposing) {
                e.preventDefault();
                add();
              }
            }}
          />
          <button type="button" className="ghost" onClick={add}>
            添加
          </button>
        </div>
      )}
      {hint && <p className="hint">{hint}</p>}
    </div>
  );
}
