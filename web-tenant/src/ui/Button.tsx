import type { ComponentProps } from "react";

// The portal's copy of the Dashboard's Button (P2c-R4): one implementation
// per app, the same conventions.

export type ButtonKind = "primary" | "secondary" | "quiet" | "danger";

/**
 * A button in the design system's kinds: `primary` a Sign-yellow fill with
 * Ink text (one per panel), `secondary` an Ink outline, `quiet` text only
 * (header and inline actions), `danger` the Stop-red fill a destructive
 * action takes in its second step only.
 *
 * `busy` disables the button and shows a spinner in the label's place; the
 * label stays laid out underneath, so the button keeps its width, while its
 * accessible name becomes `busyLabel`, as the label itself read before.
 * The type defaults to `button`: a submit says so.
 */
export function Button({
  kind = "secondary",
  size,
  block,
  busy,
  busyLabel = "处理中…",
  className,
  type = "button",
  disabled,
  children,
  ...rest
}: ComponentProps<"button"> & {
  kind?: ButtonKind;
  size?: "sm";
  block?: boolean;
  busy?: boolean;
  busyLabel?: string;
}) {
  const classes = ["btn", `btn-${kind}`, size === "sm" && "btn-sm", block && "btn-block", busy && "is-busy", className]
    .filter(Boolean)
    .join(" ");
  return (
    <button {...rest} type={type} className={classes} disabled={disabled || busy} aria-busy={busy || undefined}>
      {busy ? (
        <>
          <span className="btn-label" aria-hidden="true">
            {children}
          </span>
          <span className="vh">{busyLabel}</span>
        </>
      ) : (
        children
      )}
    </button>
  );
}
