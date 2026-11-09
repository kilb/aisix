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
 *
 * `busy` is the caller's request in flight (ruling 14). After 确认, the
 * step stays open while it is: the confirm button shows the spinner at its
 * width and is disabled, and so is `取消`, so the action cannot be
 * confirmed twice or taken back mid-flight. When the request ends (or when
 * none started) the step closes, and the focus returns to the action
 * unless the page has moved it since (to a refused field). When `ask` said
 * not to ask, the first-step button shows the busy state instead. A busy
 * button keeps its own label as its name unless `busyLabel` says otherwise.
 */
export function TwoStep({
  label,
  onConfirm,
  disabled = false,
  busy = false,
  busyLabel,
  kind = "secondary",
  size,
  ask = () => true,
  warning,
}: {
  label: string;
  onConfirm: () => void;
  disabled?: boolean;
  /** The confirmed request is in flight. */
  busy?: boolean;
  /** The busy button's name; its own label when absent. */
  busyLabel?: string;
  kind?: "primary" | "secondary";
  size?: "sm";
  ask?: () => boolean;
  warning?: ReactNode;
}) {
  const [asking, setAsking] = useState(false);
  const [swapped, setSwapped] = useState(false);
  // Confirmed, and open until the request it started has ended.
  const [confirmed, setConfirmed] = useState(false);
  const box = useRef<HTMLSpanElement>(null);
  const first = useRef<HTMLButtonElement>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  /** The box's width and whether it held focus, just before a swap. */
  const before = useRef<{ width: number; focused: boolean } | null>(null);
  /** Whether the box held focus when the step was confirmed. */
  const held = useRef(false);

  function swap(next: boolean, focused?: boolean) {
    const el = box.current;
    before.current = el
      ? { width: el.getBoundingClientRect().width, focused: focused ?? el.contains(document.activeElement) }
      : null;
    setSwapped(true);
    setAsking(next);
  }

  // The confirmed request has ended, or never started: the step closes, in
  // the same commit that ends it, so the page is never seen with the step's
  // warning still up beside what the page shows of the outcome.
  useLayoutEffect(() => {
    if (!confirmed || busy) return;
    setConfirmed(false);
    swap(false, held.current);
  }, [confirmed, busy]);

  useLayoutEffect(() => {
    const el = box.current;
    const from = before.current;
    before.current = null;
    if (!el || !from) return;
    // Only focus that went with the swapped control comes back: a refusal
    // may have moved it to a field meanwhile, and that stays.
    const active = document.activeElement;
    const lost = active === null || active === document.body || el.contains(active);
    if (from.focused && lost) (asking ? cancel : first).current?.focus();
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
        if (!asking || confirmed || e.key !== "Escape") return;
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
          busy={busy}
          busyLabel={busyLabel ?? label}
          className={swapped ? "is-in" : undefined}
          onClick={() => (ask() ? swap(true) : onConfirm())}
        >
          {label}
        </Button>
      ) : (
        <>
          <span className="two-step-confirm is-in">
            <Button
              kind="danger"
              size={size}
              disabled={disabled || confirmed}
              busy={busy}
              busyLabel={busyLabel ?? `确认${label}`}
              onClick={() => {
                held.current = box.current?.contains(document.activeElement) ?? false;
                setConfirmed(true);
                onConfirm();
              }}
            >
              确认{label}
            </Button>
            <Button ref={cancel} size={size} disabled={busy || confirmed} onClick={() => swap(false)}>
              取消
            </Button>
          </span>
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
