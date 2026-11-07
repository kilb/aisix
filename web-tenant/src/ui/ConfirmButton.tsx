import { useLayoutEffect, useRef, useState } from "react";
import { Button } from "./Button";

// Moved from controls.tsx (P1b), labels and roles unchanged; restyled as the
// design system's two-step confirm.

const SWAP_MS = 120;

/**
 * A destructive action in two steps, inline: `label`, then `确认<label>`
 * beside `取消`, with `warning` between them. Never `window.confirm`, which
 * leaves the page's own flow and is blocked in some embeddings.
 *
 * The confirm button takes Stop red only in the second step. The swap
 * happens in place: the new step fades in while the box eases from the old
 * width to the new one, over 120ms. Focus that was on the swapped control
 * moves to the safe choice (`取消`), and back to the action after a cancel.
 */
export function ConfirmButton({
  label,
  warning,
  disabled,
  onConfirm,
}: {
  label: string;
  warning?: string;
  disabled?: boolean;
  onConfirm: () => void;
}) {
  const [asking, setAsking] = useState(false);
  const [swapped, setSwapped] = useState(false);
  const box = useRef<HTMLSpanElement>(null);
  const action = useRef<HTMLButtonElement>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  /** The box's width and whether it held focus, just before a swap. */
  const before = useRef<{ width: number; focused: boolean } | null>(null);

  function swap(next: boolean) {
    const el = box.current;
    before.current = el ? { width: el.getBoundingClientRect().width, focused: el.contains(document.activeElement) } : null;
    setSwapped(true);
    setAsking(next);
  }

  useLayoutEffect(() => {
    const el = box.current;
    const from = before.current;
    before.current = null;
    if (!el || !from) return;
    if (from.focused) (asking ? cancel : action).current?.focus();
    const to = el.getBoundingClientRect().width;
    if (to === from.width || window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    el.classList.add("is-swapping");
    const run = el.animate([{ width: `${from.width}px` }, { width: `${to}px` }], { duration: SWAP_MS, easing: "ease-out" });
    run.onfinish = run.oncancel = () => el.classList.remove("is-swapping");
  }, [asking]);

  return (
    <span className="two" ref={box}>
      {!asking ? (
        <Button ref={action} size="sm" className={swapped ? "is-in" : undefined} disabled={disabled} onClick={() => swap(true)}>
          {label}
        </Button>
      ) : (
        <span className="two-step is-in">
          {warning && <span className="two-warn">{warning}</span>}
          <Button
            kind="danger"
            size="sm"
            disabled={disabled}
            onClick={() => {
              setAsking(false);
              onConfirm();
            }}
          >
            确认{label}
          </Button>
          <Button ref={cancel} size="sm" onClick={() => swap(false)}>
            取消
          </Button>
        </span>
      )}
    </span>
  );
}
