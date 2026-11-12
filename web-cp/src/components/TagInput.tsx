import { useId, useState } from "react";
import { Button } from "../ui/Button";

/**
 * A list of short names, such as provider groups: one chip per name with
 * its own remove button, and a box that adds the typed name on Enter or
 * `添加`. Names are trimmed; an empty or repeated name is not added.
 * `readOnly` (a viewer, or a form awaiting confirmation) shows the chips
 * only. `problem` is the control plane's refusal of the list, shown under it.
 */
export function TagInput({
  label,
  value,
  onChange,
  readOnly = false,
  hint,
  problem = null,
}: {
  label: string;
  value: string[];
  onChange: (next: string[]) => void;
  readOnly?: boolean;
  hint?: string;
  problem?: string | null;
}) {
  const [draft, setDraft] = useState("");
  const problemId = useId();

  const add = () => {
    const name = draft.trim();
    if (name !== "" && !value.includes(name)) onChange([...value, name]);
    setDraft("");
  };

  return (
    <div
      className="f text-body"
      role="group"
      aria-label={label}
      aria-invalid={problem ? true : undefined}
      aria-describedby={problem ? problemId : undefined}
    >
      <span>{label}</span>
      {value.length === 0 ? (
        <p className="hint text-caption text-ink-soft">未设置</p>
      ) : (
        <ul className="tags">
          {value.map((name) => (
            <li key={name} className="h-auto min-h-11 max-w-full gap-1 rounded-full border border-line bg-well py-0 pr-0 pl-3 font-mono text-meta text-ink-soft [overflow-wrap:anywhere]">
              {name}
              {!readOnly && (
                <button
                  type="button"
                  aria-label={`移除 ${name}`}
                  title={`移除 ${name}`}
                  className="size-11 shrink-0 rounded-full text-ink-soft hover:bg-stone hover:text-ink"
                  onClick={() => onChange(value.filter((v) => v !== name))}
                />
              )}
            </li>
          ))}
        </ul>
      )}
      {!readOnly && (
        <div className="tag-add max-w-full">
          <input
            className="min-w-0 flex-1"
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
          <Button type="button" onClick={add}>
            添加
          </Button>
        </div>
      )}
      {hint && <p className="hint text-caption text-ink-soft">{hint}</p>}
      {problem && (
        <p id={problemId} className="field-problem">
          {problem}
        </p>
      )}
    </div>
  );
}
