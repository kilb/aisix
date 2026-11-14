import "./OperationalFields.css";
import { useId, useState } from "react";
import type { Detail } from "../lib/api";
import { Button } from "../ui/Button";

/**
 * A JSON document in a monospace textarea, with a `格式化` control and,
 * under it, each problem the server found (`details`) at its JSON path,
 * the path highlighted. Without `onChange` the document is read-only and
 * nothing edits it.
 */
export function JsonEditor({
  value,
  onChange,
  details,
}: {
  value: string;
  onChange?: (next: string) => void;
  details: Detail[];
}) {
  const problemsId = useId();
  const formatErrorId = `${problemsId}-format`;
  const [formatError, setFormatError] = useState<string | null>(null);

  function format() {
    try {
      onChange?.(JSON.stringify(JSON.parse(value), null, 2));
      setFormatError(null);
    } catch (e) {
      setFormatError(`不是合法的 JSON，无法格式化：${e instanceof Error ? e.message : String(e)}`);
    }
  }

  return (
    <div className="json-editor min-w-0 text-body">
      <textarea
        aria-label="资源文档"
        spellCheck={false}
        readOnly={!onChange}
        value={value}
        className="min-h-90 w-full rounded-cell border border-line bg-surface font-mono text-caption leading-relaxed text-ink"
        onChange={(e) => {
          setFormatError(null);
          onChange?.(e.target.value);
        }}
        aria-invalid={details.length > 0 || formatError ? true : undefined}
        aria-describedby={[details.length > 0 ? problemsId : null, formatError ? formatErrorId : null].filter(Boolean).join(" ") || undefined}
      />
      {onChange && (
        <div className="mt-2 flex justify-end">
          <Button type="button" size="sm" onClick={format}>
            格式化
          </Button>
        </div>
      )}
      {formatError && <div id={formatErrorId} className="note crit" role="alert">{formatError}</div>}
      {details.length > 0 && (
        <ul
          id={problemsId}
          className="json-problems mt-3 list-none rounded-cell border border-line bg-well px-4 py-3 text-caption text-ink-soft"
          aria-label="校验问题"
        >
          {details.map((d, i) => (
            <li key={i} data-path={d.path}>
              <mark className="json-path rounded bg-stone px-1 py-0.5 font-mono text-meta text-ink">{d.path || "（文档根）"}</mark> {d.message}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
