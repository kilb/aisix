import { useId, useState } from "react";
import type { Detail, RateCard } from "../lib/api";
import { problemAt } from "../lib/errors";
import { Button } from "../ui/Button";
import { Field } from "../ui/Field";
import { JsonEditor } from "./JsonEditor";

/** The rates the form edits: the `default` tier's base. Other tiers and bands go through the JSON view. */
const RATES: { dimension: string; label: string }[] = [
  { dimension: "input", label: "输入" },
  { dimension: "output", label: "输出" },
  { dimension: "cache_read", label: "缓存读" },
  { dimension: "cache_write", label: "缓存写" },
  { dimension: "cache_write_1h", label: "1 小时缓存写" },
  { dimension: "reasoning", label: "推理" },
  { dimension: "web_search", label: "联网搜索" },
];

/** The path a field's refusal is at; the backend refuses a card as a whole at `/card`. */
function fieldPath(dimension: string): string {
  return `/card/default/base/${dimension}`;
}

/**
 * A price card as a form: the `default` tier's base rates, each in USD per
 * million units, with the rest of the card kept as it is. `编辑完整价格卡（JSON）`
 * opens the whole card in the JSON editor instead. A rate left empty is left
 * out of the card, and the server decides whether the card is complete.
 *
 * Problems whose path starts `/card` show at their field, or else above the
 * card. `onValidity` reports whether the JSON view holds a document that
 * parses; while it does not, `value` is the last card that did.
 */
export function RateCardEditor({
  value,
  onChange,
  details,
  onValidity,
}: {
  value: RateCard | null;
  onChange(c: RateCard): void;
  details: Detail[];
  onValidity?(ok: boolean): void;
}) {
  const [json, setJson] = useState<string | null>(null);
  const [jsonError, setJsonError] = useState<string | null>(null);
  const aboveId = useId();
  const mine = details.filter((d) => d.path.startsWith("/card"));
  const atField = new Set(RATES.map((r) => fieldPath(r.dimension)));
  const above = mine.filter((d) => !atField.has(d.path));

  const base = value?.default?.base ?? {};

  function setRate(dimension: string, text: string) {
    const next = { ...base };
    const rate = text.trim();
    if (rate === "") delete next[dimension];
    else next[dimension] = rate;
    onChange({ ...(value ?? {}), default: { ...(value?.default ?? {}), base: next } });
  }

  function setJsonText(text: string) {
    setJson(text);
    try {
      const parsed: unknown = JSON.parse(text);
      if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("价格卡应是一个对象");
      setJsonError(null);
      onValidity?.(true);
      onChange(parsed as RateCard);
    } catch (e) {
      setJsonError(`不是合法的价格卡 JSON，修改尚未生效：${e instanceof Error ? e.message : String(e)}`);
      onValidity?.(false);
    }
  }

  function toggleJson() {
    if (json === null) {
      setJson(JSON.stringify(value ?? {}, null, 2));
      setJsonError(null);
    } else {
      setJson(null);
      setJsonError(null);
      // The form shows the last card that parsed.
      onValidity?.(true);
    }
  }

  return (
    <fieldset className="rate-card" aria-invalid={mine.length > 0 ? true : undefined} aria-describedby={above.length > 0 ? aboveId : undefined}>
      <legend>价格卡（美元/百万单位）</legend>
      {above.length > 0 && (
        <div className="note crit" role="alert" id={aboveId}>
          {above.map((d) => d.message).join("；")}
        </div>
      )}
      {json === null ? (
        <div className="r">
          {RATES.map((r) => (
            <Field key={r.dimension} label={r.label} error={problemAt(mine, (p) => p === fieldPath(r.dimension))}>
              {(aria) => (
                <input
                  {...aria}
                  inputMode="decimal"
                  autoComplete="off"
                  placeholder="美元/百万单位"
                  value={base[r.dimension] ?? ""}
                  onChange={(e) => setRate(r.dimension, e.target.value)}
                />
              )}
            </Field>
          ))}
        </div>
      ) : (
        <>
          <JsonEditor value={json} onChange={setJsonText} details={mine.map((d) => ({ ...d, path: d.path.slice("/card".length) }))} />
          {jsonError && (
            <div className="note crit" role="alert">
              {jsonError}
            </div>
          )}
        </>
      )}
      <p>
        <Button type="button" size="sm" aria-pressed={json !== null} onClick={toggleJson}>
          编辑完整价格卡（JSON）
        </Button>
      </p>
    </fieldset>
  );
}
