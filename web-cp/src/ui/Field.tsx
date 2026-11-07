// Shared: web-cp/src/ui and web-tenant/src/ui carry byte-equal copies of
// this file, pinned by web-tenant/src/lib/tokenParity.test.ts; change both
// together. The contract is in CONTRACT.md beside it.
import { useId, type ReactNode } from "react";

/** What a control under a `Field` carries so its hint and its refusal describe it. */
export interface FieldAria {
  "aria-invalid"?: true;
  "aria-describedby"?: string;
}

/**
 * One labelled control: the label above it, then its hint, then, when the
 * value was refused, the reason in the error red. The label wraps the
 * control, so the control's accessible name is the label alone.
 *
 * `children` is the control, or a function given the attributes that tie
 * the control to its hint and its refusal (`aria-describedby`, and
 * `aria-invalid` while refused); spread them on the control. A control
 * with neither a hint nor a refusal needs no function.
 */
export function Field({
  label,
  hint,
  error,
  wide = false,
  children,
}: {
  label: string;
  hint?: ReactNode;
  error?: string | null;
  /** Spans both columns of a two-column form. */
  wide?: boolean;
  children: ReactNode | ((aria: FieldAria) => ReactNode);
}) {
  const id = useId();
  const hintId = `${id}-hint`;
  const errorId = `${id}-error`;
  const hinted = hint !== undefined && hint !== null && hint !== false;
  const describedBy = [hinted ? hintId : null, error ? errorId : null].filter(Boolean).join(" ");
  const aria: FieldAria = {
    ...(error ? { "aria-invalid": true as const } : {}),
    ...(describedBy ? { "aria-describedby": describedBy } : {}),
  };
  return (
    <div className={wide ? "field wide" : "field"}>
      <label className="f">
        <span className="f-label">{label}</span>
        {typeof children === "function" ? children(aria) : children}
      </label>
      {hinted && (
        <p className="hint" id={hintId}>
          {hint}
        </p>
      )}
      {error && (
        <p className="field-problem" id={errorId}>
          {error}
        </p>
      )}
    </div>
  );
}
