import { useEffect, useId, useState } from "react";
import { parseUtc, type EffectiveResolver } from "../lib/effective";
import { Field } from "../ui/Field";

export { sendAt, type EffectiveResolver } from "../lib/effective";

/**
 * When a price starts: `立即生效` (on by default), or a `生效时间（UTC）`.
 * With it on the time is not read until submit, so a form left open does not
 * send a start that has since passed. `onResolve` is given the resolver
 * whenever the choice changes; `error` is a refusal at `/effective_from`.
 */
export function EffectiveFrom({
  onResolve,
  error,
}: {
  onResolve(resolver: EffectiveResolver): void;
  error?: string | null;
}) {
  const [immediate, setImmediate] = useState(true);
  const [text, setText] = useState("");
  const noteId = useId();

  useEffect(() => {
    onResolve({
      immediate,
      stamp: () => (immediate ? new Date().toISOString() : parseUtc(text)),
    });
    // onResolve is the form's setter; the choice is what changes.
  }, [immediate, text]); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <>
      <label className="check min-h-11 gap-3 font-mono text-meta text-ink-soft">
        <input type="checkbox" checked={immediate} onChange={(e) => setImmediate(e.target.checked)} />
        <span>立即生效</span>
      </label>
      {!immediate && (
        <Field
          label="生效时间（UTC）"
          hint={<span id={noteId}>价格只能从现在或将来开始生效</span>}
          error={error}
        >
          {(aria) => (
            <input
              {...aria}
              autoComplete="off"
              placeholder="2026-11-07 13:05"
              value={text}
              onChange={(e) => setText(e.target.value)}
            />
          )}
        </Field>
      )}
    </>
  );
}
