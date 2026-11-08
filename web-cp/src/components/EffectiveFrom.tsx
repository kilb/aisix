import { useEffect, useId, useState } from "react";
import { ApiError } from "../lib/api";
import { Field } from "../ui/Field";

/** What a form asks `EffectiveFrom` for when it submits. */
export interface EffectiveResolver {
  /** True while 立即生效 is on: the instant is stamped at submit. */
  immediate: boolean;
  /** The ISO instant to send, stamped now when immediate; null when the typed time is not a UTC time. */
  stamp(): string | null;
}

const TYPED = /^(\d{4}-\d{2}-\d{2})[T ](\d{2}:\d{2})(:\d{2})?Z?$/;

/** A time typed as `YYYY-MM-DD HH:mm[:ss]` in UTC, as an ISO instant; null when it is not one. */
function parseUtc(text: string): string | null {
  const m = TYPED.exec(text.trim());
  if (!m) return null;
  const at = new Date(`${m[1]}T${m[2]}${m[3] ?? ":00"}Z`);
  return Number.isNaN(at.getTime()) ? null : at.toISOString();
}

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
      <label className="check">
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

/**
 * Sends a write with the resolver's start. A start stamped at submit that the
 * server finds in the past (`422 /effective_from`: the clock moved between
 * the stamp and the check) is stamped again and sent once more; a typed time
 * is never retried. `null` from the resolver (a time that is not valid) is
 * `invalid`.
 */
export async function sendAt<T>(
  resolver: EffectiveResolver,
  send: (effectiveFrom: string) => Promise<T>,
): Promise<T | "invalid"> {
  const first = resolver.stamp();
  if (first === null) return "invalid";
  try {
    return await send(first);
  } catch (e) {
    const past = e instanceof ApiError && e.status === 422 && e.details.some((d) => d.path === "/effective_from");
    if (!resolver.immediate || !past) throw e;
    const again = resolver.stamp();
    if (again === null) throw e;
    return await send(again);
  }
}
