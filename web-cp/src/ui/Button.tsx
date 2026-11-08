// Shared: web-cp/src/ui and web-tenant/src/ui carry byte-equal copies of
// this file, pinned by web-tenant/src/lib/tokenParity.test.ts; change both
// together. The contract is in CONTRACT.md beside it.
import { useId, type ComponentProps, type MouseEvent, type ReactNode } from "react";
import { Icon, type IconName } from "./Icon";

export type ButtonKind = "primary" | "secondary" | "danger" | "quiet";

export interface ButtonProps extends ComponentProps<"button"> {
  /**
   * `primary` is a Sign-yellow fill with Ink text, one per view;
   * `secondary`, the default, an Ink outline; `danger` the Stop-red fill a
   * destructive action takes in its second step only; `quiet` a text
   * button for headers and dense rows.
   */
  kind?: ButtonKind;
  /** `sm` is the 32px toolbar and table height; the default is 36px. */
  size?: "sm";
  /** Takes the full width of its container. */
  block?: boolean;
  /** An icon before the label, or, with no children and an `aria-label`, the whole button. */
  icon?: IconName;
  /**
   * In flight: the button is disabled, so a second click cannot send the
   * request again, and a spinner takes the label's place at the same
   * width. Its accessible name becomes `busyLabel`.
   */
  busy?: boolean;
  /** The accessible name while `busy`. */
  busyLabel?: string;
  /**
   * Why the button is disabled, when nothing on screen says so. The button
   * then stays focusable (`aria-disabled` rather than `disabled`, and a
   * click does nothing) and shows the reason in a tooltip on hover and on
   * keyboard focus.
   */
  disabledReason?: string;
  children?: ReactNode;
}

/**
 * The one button. Its `type` is `button` unless it says otherwise: a
 * submit says `type="submit"`.
 */
export function Button({
  kind = "secondary",
  size,
  block = false,
  icon,
  busy = false,
  busyLabel = "处理中…",
  disabledReason,
  type = "button",
  disabled = false,
  onClick,
  className,
  children,
  "aria-describedby": describedBy,
  ...rest
}: ButtonProps) {
  const tipId = useId();
  const iconOnly = icon !== undefined && (children === undefined || children === null || children === false);
  // Disabled with a stated reason: focusable, so the reason reaches the keyboard too.
  const reasoned = disabled && !busy && disabledReason !== undefined;
  const classes = [
    "btn",
    `btn-${kind}`,
    size === "sm" && "btn-sm",
    block && "btn-block",
    iconOnly && "btn-icon",
    busy && "is-busy",
    className,
  ]
    .filter(Boolean)
    .join(" ");
  const content = (
    <>
      {icon && <Icon name={icon} />}
      {children}
    </>
  );
  const button = (
    <button
      {...rest}
      type={type}
      className={classes}
      disabled={busy || (disabled && !reasoned)}
      aria-disabled={reasoned || undefined}
      aria-busy={busy || undefined}
      aria-describedby={[describedBy, reasoned ? tipId : undefined].filter(Boolean).join(" ") || undefined}
      onClick={reasoned ? (e: MouseEvent<HTMLButtonElement>) => e.preventDefault() : onClick}
    >
      {busy ? (
        <>
          <span className="btn-label" aria-hidden="true">
            {content}
          </span>
          <span className="vh">{busyLabel}</span>
        </>
      ) : (
        content
      )}
    </button>
  );
  if (!reasoned) return button;
  return (
    <span className="has-tip">
      {button}
      <span className="tip" role="tooltip" id={tipId}>
        {disabledReason}
      </span>
    </span>
  );
}
