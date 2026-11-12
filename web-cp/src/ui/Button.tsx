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
   * `primary` 为品牌色主操作；`secondary` 为默认轮廓按钮；
   * `danger` 仅用于破坏性操作第二步，以危险色与确认文字区分；`quiet` 为安静文本操作。
   */
  kind?: ButtonKind;
  /** 保留 sm 调用接口；两种尺寸均有 44px 命中区。 */
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
  "btn relative inline-flex shrink-0 cursor-pointer select-none items-center justify-center gap-2 whitespace-nowrap rounded-(--r-button) text-body font-medium leading-4 transition-colors duration-150 disabled:cursor-not-allowed disabled:opacity-50 aria-disabled:cursor-not-allowed aria-disabled:opacity-50 [&_svg]:size-4 [&_svg]:shrink-0",
  {
    variants: {
      kind: {
        primary: "border border-primary bg-primary text-primary-foreground enabled:hover:bg-primary/90",
        secondary: "border border-line bg-card text-foreground enabled:hover:border-input enabled:hover:bg-foreground/5",
        danger: "border border-destructive bg-destructive text-destructive-foreground enabled:hover:bg-destructive/90",
        quiet: "border border-transparent text-text-2 enabled:hover:bg-foreground/6 enabled:hover:text-foreground",
      },
      size: { md: "h-11 min-w-11 px-5", sm: "h-11 min-w-11 px-4 text-caption" },
      iconOnly: { true: "px-0", false: "" },
      block: { true: "w-full", false: "" },
    },
    compoundVariants: [
      { iconOnly: true, size: "md", className: "w-11" },
      { iconOnly: true, size: "sm", className: "w-11" },
      { kind: "quiet", iconOnly: false, size: "md", className: "px-4" },
      { kind: "quiet", iconOnly: false, size: "sm", className: "px-3" },
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
