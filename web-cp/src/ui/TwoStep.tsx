// Shared: web-cp/src/ui and web-tenant/src/ui carry byte-equal copies of
// this file, pinned by web-tenant/src/lib/tokenParity.test.ts; change both
// together. The contract is in CONTRACT.md beside it.
import { useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { Button } from "./Button";

const SWAP_MS = 120;

/**
 * A destructive action in two steps, in place (ruling P1b-R6): the first
 * click swaps the button for `确认<label>` in Stop red and `取消`, with
 * `warning` on a line under them, and only the confirmation acts. Never
 * `window.confirm`, which leaves the page's own flow.
 *
 * `ask` decides, at each first click, whether to ask at all; when it says
 * no, the first click acts. `kind` styles the first step only.
 *
 * The swap fades the new step in while the box eases from the old width
 * to the new one, over 120ms. Focus that was on the swapped control moves
 * to the safe choice, `取消`, when the step opens, so a second Enter takes
 * it back rather than acting, and back to the action when it closes.
 * Escape takes the step back too, and not the panel around it. The
 * warning is an `alert`, so it is heard when the step opens.
 */
export function TwoStep({
  label,
  onConfirm,
  disabled = false,
  kind = "secondary",
  size,
  ask = () => true,
  warning,
}: {
  label: string;
  onConfirm: () => void;
  disabled?: boolean;
  kind?: "primary" | "secondary";
  size?: "sm";
  ask?: () => boolean;
  warning?: ReactNode;
}) {
  const [asking, setAsking] = useState(false);
  const [swapped, setSwapped] = useState(false);
  const box = useRef<HTMLSpanElement>(null);
  const first = useRef<HTMLButtonElement>(null);
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
    if (from.focused) (asking ? cancel : first).current?.focus();
    const to = el.getBoundingClientRect().width;
    if (to === from.width || window.matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    el.classList.add("is-swapping");
    const run = el.animate([{ width: `${from.width}px` }, { width: `${to}px` }], { duration: SWAP_MS, easing: "ease-out" });
    run.onfinish = run.oncancel = () => el.classList.remove("is-swapping");
  }, [asking]);

  return (
    <span
      ref={box}
      className={asking ? "two-step is-asking" : "two-step"}
      onKeyDown={(e) => {
        if (!asking || e.key !== "Escape") return;
        e.preventDefault();
        swap(false);
      }}
    >
      {!asking ? (
        <Button
          key="first"
          ref={first}
          kind={kind}
          size={size}
          disabled={disabled}
          className={swapped ? "is-in" : undefined}
          onClick={() => (ask() ? swap(true) : onConfirm())}
        >
          {label}
        </Button>
      ) : (
        <>
          <Button
            key="confirm"
            kind="danger"
            size={size}
            disabled={disabled}
            className="is-in"
            onClick={() => {
              swap(false);
              onConfirm();
            }}
          >
            确认{label}
          </Button>
          <Button key="cancel" ref={cancel} size={size} className="is-in" onClick={() => swap(false)}>
            取消
          </Button>
          {warning && (
            <span className="two-step-warn is-in" role="alert">
              {warning}
            </span>
          )}
        </>
      )}
    </span>
  );
}
