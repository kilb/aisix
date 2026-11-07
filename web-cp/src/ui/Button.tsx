import { useId, type ComponentProps, type ReactNode } from "react";
import { Icon, type IconName } from "./Icon";

export type ButtonKind = "primary" | "secondary" | "danger" | "quiet";

export interface ButtonProps extends ComponentProps<"button"> {
  /**
   * `primary` is a Sign-yellow fill with Ink text, one per view; `secondary`
   * an Ink outline; `danger` the Stop-red second step of a destructive
   * action; `quiet` a text button for the header and dense rows.
   */
  kind?: ButtonKind;
  /** `sm` is the 32px toolbar and table height; the default is 36px. */
  size?: "md" | "sm";
  /** An icon before the label, or the whole button with `aria-label`. */
  icon?: IconName;
  /** In flight: a spinner replaces the label and the width stays. */
  busy?: boolean;
  /** Why the button is disabled, when nothing on screen says so: shown on hover and focus. */
  disabledReason?: string;
  children?: ReactNode;
}

/**
 * The one button. `type` is passed through untouched, so a button in a form
 * is a submit button unless it says otherwise, as a plain `<button>` is.
 */
export function Button({
  kind = "secondary",
  size = "md",
  icon,
  busy = false,
  disabledReason,
  className,
  children,
  disabled,
  ...rest
}: ButtonProps) {
  const tipId = useId();
  const iconOnly = icon !== undefined && (children === undefined || children === null);
  const classes = [
    "btn",
    `btn-${kind}`,
    size === "sm" && "btn-sm",
    iconOnly && "btn-icon",
    busy && "is-busy",
    className,
  ]
    .filter(Boolean)
    .join(" ");
  const tip = disabled && disabledReason;
  const button = (
    <button
      {...rest}
      className={classes}
      disabled={disabled}
      aria-busy={busy || undefined}
      aria-describedby={tip ? tipId : rest["aria-describedby"]}
    >
      {icon && <Icon name={icon} />}
      {children}
    </button>
  );
  if (!tip) return button;
  return (
    <span className="has-tip">
      {button}
      <span className="tip" role="tooltip" id={tipId}>
        {disabledReason}
      </span>
    </span>
  );
}
