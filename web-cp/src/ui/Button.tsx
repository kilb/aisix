// Shared: web-cp/src/ui and web-tenant/src/ui carry byte-equal copies of
// this file, pinned by web-tenant/src/lib/tokenParity.test.ts; change both
// together. The contract is in CONTRACT.md beside it.
import { useId, type ComponentProps, type MouseEvent, type ReactNode } from "react";
import { cva } from "class-variance-authority";
import { cn } from "../lib/cn";
import { Icon, type IconName } from "./Icon";

export type ButtonKind = "primary" | "secondary" | "danger" | "quiet";

export interface ButtonProps extends ComponentProps<"button"> {
  /**
   * `primary` is the near-black fill, one per view; `secondary`, the
   * default, the outlined white button; `danger` the red fill a destructive
   * action takes in its second step only; `quiet` a text button for headers
   * and dense rows.
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

/** shadcn/ui's button, in this app's four kinds and two sizes. */
const buttonClass = cva(
  "btn relative inline-flex shrink-0 cursor-pointer select-none items-center justify-center gap-2 whitespace-nowrap rounded-[10px] text-sm font-medium leading-4 transition-[color,background-color,border-color,box-shadow,transform,opacity] duration-150 enabled:active:translate-y-px disabled:cursor-not-allowed disabled:opacity-50 aria-disabled:cursor-not-allowed aria-disabled:opacity-50 [&_svg]:size-4 [&_svg]:shrink-0",
  {
    variants: {
      kind: {
        // A filled action sits in its own coloured light; the outlined one is
        // a white chip lifted a step off the page.
        primary: "bg-primary text-primary-foreground shadow-btn enabled:hover:bg-primary/92",
        secondary: "border border-border bg-card text-foreground shadow-sm enabled:hover:border-(--input) enabled:hover:bg-accent",
        danger: "bg-destructive text-destructive-foreground shadow-xs enabled:hover:bg-destructive/90",
        quiet: "text-text-2 enabled:hover:bg-accent enabled:hover:text-foreground",
      },
      size: { md: "h-9 px-4", sm: "h-8 px-3 text-[13px]" },
      iconOnly: { true: "px-0", false: "" },
      block: { true: "w-full", false: "" },
    },
    compoundVariants: [
      { iconOnly: true, size: "md", className: "w-9" },
      { iconOnly: true, size: "sm", className: "w-8" },
      { kind: "quiet", iconOnly: false, size: "md", className: "px-3" },
      { kind: "quiet", iconOnly: false, size: "sm", className: "px-2" },
    ],
  },
);

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
  const classes = cn(
    buttonClass({ kind, size: size ?? "md", iconOnly, block }),
    `btn-${kind}`,
    busy && "is-busy",
    className,
  );
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
