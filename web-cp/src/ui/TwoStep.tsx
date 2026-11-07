import { useEffect, useRef, useState, type ReactNode } from "react";
import { Button } from "./Button";

/**
 * A destructive action in two steps (ruling P1b-R6): the first click swaps
 * the button, in place, for `确认<label>` in Stop red and `取消`, under
 * `warning`, and only the confirmation acts. `ask` decides, at each first
 * click, whether to ask at all. `kind` styles the first step only.
 */
export function TwoStep({
  label,
  onConfirm,
  disabled,
  kind = "secondary",
  size = "md",
  ask = () => true,
  warning,
}: {
  label: string;
  onConfirm: () => void;
  disabled?: boolean;
  kind?: "primary" | "secondary";
  size?: "md" | "sm";
  ask?: () => boolean;
  warning?: ReactNode;
}) {
  const [asking, setAsking] = useState(false);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const firstRef = useRef<HTMLButtonElement>(null);
  // Focus follows the swap, so the keyboard stays on the action: onto the
  // safe choice, 取消, when the step opens, so a second Enter takes it back
  // instead of acting; back onto the action when it closes.
  const moved = useRef(false);
  useEffect(() => {
    if (!moved.current) return;
    (asking ? cancelRef : firstRef).current?.focus();
  }, [asking]);

  const swap = (next: boolean) => {
    moved.current = true;
    setAsking(next);
  };

  if (!asking) {
    return (
      <Button
        ref={firstRef}
        type="button"
        kind={kind}
        size={size}
        disabled={disabled}
        onClick={() => (ask() ? swap(true) : onConfirm())}
      >
        {label}
      </Button>
    );
  }
  return (
    <span
      className="two-step"
      onKeyDown={(e) => {
        if (e.key !== "Escape") return;
        // Escape takes back this step only, not the panel around it.
        e.preventDefault();
        swap(false);
      }}
    >
      {warning && (
        <div className="note warn" role="alert">
          {warning}
        </div>
      )}
      <span className="two-step-confirm">
        <Button
          type="button"
          kind="danger"
          size={size}
          disabled={disabled}
          onClick={() => {
            swap(false);
            onConfirm();
          }}
        >
          确认{label}
        </Button>
        <Button ref={cancelRef} type="button" size={size} onClick={() => swap(false)}>
          取消
        </Button>
      </span>
    </span>
  );
}
