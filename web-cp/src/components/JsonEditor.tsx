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
    <div className="json-editor">
      <textarea
        aria-label="资源文档"
        spellCheck={false}
        readOnly={!onChange}
        value={value}
        className="min-h-[360px] text-[13px] leading-relaxed"
        onChange={(e) => {
          setFormatError(null);
          onChange?.(e.target.value);
        }}
        aria-invalid={details.length > 0 ? true : undefined}
        aria-describedby={details.length > 0 ? problemsId : undefined}
      />
      {onChange && (
        <div className="mt-2 flex justify-end">
          <Button type="button" size="sm" onClick={format}>
            格式化
          </Button>
        </div>
      )}
      {formatError && <div className="note crit">{formatError}</div>}
      {details.length > 0 && (
        <ul
          id={problemsId}
          className="json-problems mt-3 list-none rounded-lg border border-danger/30 bg-danger-soft py-2.5 pr-4 pl-4"
          aria-label="校验问题"
        >
          {details.map((d, i) => (
            <li key={i} data-path={d.path}>
              <mark className="json-path">{d.path || "（文档根）"}</mark> {d.message}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
