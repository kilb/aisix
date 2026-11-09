import { useEffect, useId, useRef, useState } from "react";
import { ApiError, upstreamModels, type UpstreamModelList } from "../lib/api";
import { errorText } from "../lib/fmt";
import { messageFor } from "../lib/errors";

/**
 * How long the field stays disabled while the account's list is read. The
 * route's own call is capped at 5 s, so this is that cap and no more: past
 * it the operator types, and a late answer only fills the list behind them.
 */
const LOAD_BUDGET_MS = 5_000;

/** The most options rendered at once: a list of thousands is filtered by typing, not by scrolling. */
const MAX_OPTIONS = 200;

/** What the field says when the list is unavailable, whatever the reason. */
const FALLBACK = "无法获取该上游的模型列表，请直接填写";

/**
 * One fetch per account, shared by every row of that account: the field asks
 * once, on first focus. A refused answer stays refused for the page's life,
 * which is what "once per account" means; reloading the page asks again.
 */
const fetches = new Map<string, Promise<UpstreamModelList>>();

function fetchOnce(accountId: string): Promise<UpstreamModelList> {
  let pending = fetches.get(accountId);
  if (!pending) {
    pending = upstreamModels.list(accountId);
    fetches.set(accountId, pending);
  }
  return pending;
}

/** The refusal's own words: a scrubbed upstream detail names what went wrong. */
export function why(e: unknown): string {
  if (e instanceof ApiError) {
    return [messageFor(e), ...e.details.map((d) => d.message)].join("；");
  }
  return errorText(e);
}

/**
 * An upstream model name: a combobox fed by the account's own model list, and
 * free text the moment that list is unavailable.
 *
 * The list is asked for once per account, on first focus. While it loads the
 * field is disabled, for at most [`LOAD_BUDGET_MS`]. With an answer the field
 * offers the ids, filtered by what is typed; selecting one is a shortcut, not
 * a constraint. A refusal of any kind, and an empty answer, replace the list
 * with [`FALLBACK`] and the server's own reason, and the field is then plain
 * text.
 *
 * **A typed value is always accepted**, listed or not: it is validated for
 * shape only (1 to [`MAX_CHARS`] characters, no control characters), never
 * for membership. A refused upstream must not be able to block configuring
 * an account. Empty is meaningful rather than invalid: it clears the mapping,
 * so the public name is what the account's upstream is asked for.
 */
export function UpstreamModelPicker({
  accountId,
  value,
  publicName,
  label,
  onChange,
  disabled = false,
  invalid = false,
  describedBy,
}: {
  /** The account whose list is offered; empty offers no list at all. */
  accountId: string;
  /** The typed name; empty means the public name is sent. */
  value: string;
  /** The public model's name: the placeholder, and what an empty field means. */
  publicName: string;
  /** The field's accessible name; `上游模型名称` by default. */
  label?: string;
  onChange: (next: string) => void;
  disabled?: boolean;
  /** The field's own refusal, from its save. */
  invalid?: boolean;
  describedBy?: string;
}) {
  const id = useId();
  const listId = `${id}-list`;
  const input = useRef<HTMLInputElement>(null);
  const [phase, setPhase] = useState<"idle" | "loading" | "listed" | "fallback">("idle");
  const [ids, setIds] = useState<string[]>([]);
  const [reason, setReason] = useState<string | null>(null);
  const [open, setOpen] = useState(false);

  // The budget's timer, and whether the field held the focus when it started:
  // a disabled control cannot keep it, so it is given back when the box is
  // usable again.
  const timer = useRef<number | null>(null);
  const held = useRef(false);

  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );

  function settle() {
    if (timer.current !== null) {
      window.clearTimeout(timer.current);
      timer.current = null;
    }
    if (held.current) input.current?.focus();
  }

  async function load() {
    if (phase !== "idle" || accountId === "") return;
    held.current = document.activeElement === input.current;
    setPhase("loading");
    timer.current = window.setTimeout(() => {
      // Past the budget the field is usable whether or not the call is back:
      // free text is the answer either way.
      setPhase((p) => (p === "loading" ? "fallback" : p));
      setReason(null);
      settle();
    }, LOAD_BUDGET_MS);
    try {
      const answer = await fetchOnce(accountId);
      const fetched = answer.items.map((m) => m.id);
      setIds(fetched);
      setPhase(fetched.length > 0 ? "listed" : "fallback");
      if (fetched.length === 0) setReason("该上游没有返回任何模型名称");
      if (answer.warnings.length > 0) setReason(answer.warnings.join("；"));
    } catch (e) {
      setPhase("fallback");
      setReason(why(e));
    } finally {
      settle();
    }
  }

  const query = value.trim().toLowerCase();
  const matches = query === "" ? ids : ids.filter((name) => name.toLowerCase().includes(query));
  const shown = matches.slice(0, MAX_OPTIONS);
  const listed = phase === "listed";

  return (
    <div className="upicker">
      <input
        ref={input}
        role="combobox"
        aria-label={label ?? "上游模型名称"}
        aria-expanded={listed && open}
        aria-controls={listed ? listId : undefined}
        aria-autocomplete="list"
        aria-invalid={invalid ? true : undefined}
        aria-describedby={describedBy}
        aria-busy={phase === "loading" || undefined}
        placeholder={publicName}
        value={value}
        disabled={disabled || phase === "loading"}
        onChange={(e) => {
          onChange(e.target.value);
          setOpen(true);
        }}
        onFocus={() => {
          void load();
          if (listed) setOpen(true);
        }}
        onBlur={() => setOpen(false)}
        onKeyDown={(e) => {
          if (e.key === "Escape") setOpen(false);
          else if (e.key === "ArrowDown" && listed) setOpen(true);
        }}
      />
      {phase === "loading" && (
        <span className="upicker-loading" role="status">
          载入中…
        </span>
      )}
      {listed && open && shown.length > 0 && (
        <ul className="upicker-list" role="listbox" id={listId} aria-label="上游模型名称">
          {shown.map((name) => (
            <li
              key={name}
              role="option"
              aria-selected={name === value}
              // `mousedown` rather than `click`: the field's blur would close
              // the list before a click landed.
              onMouseDown={(e) => {
                e.preventDefault();
                onChange(name);
                setOpen(false);
              }}
            >
              {name}
            </li>
          ))}
          {matches.length > shown.length && (
            <li className="upicker-more" aria-hidden="true">
              共 {matches.length} 个，只列出前 {MAX_OPTIONS} 个；继续输入可缩小范围
            </li>
          )}
        </ul>
      )}
      {phase === "fallback" && (
        <p className="hint upicker-fallback">
          {FALLBACK}
          {reason !== null && <span className="upicker-reason">{reason}</span>}
        </p>
      )}
    </div>
  );
}
