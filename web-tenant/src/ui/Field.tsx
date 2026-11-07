import type { ReactNode } from "react";

// The portal's copy of the Dashboard's Field (P2c-R4).

/**
 * One form field: its label above the control, then its hint, then its
 * refusal in Stop red. The label wraps the control, so the control's
 * accessible name is the label alone; the hint and the refusal come after
 * it and reach the control through the ids its `aria-describedby` names
 * (`hintId`, `errorId`), which the caller wires because only the caller
 * knows which of them describe it.
 */
export function Field({
  label,
  hint,
  hintId,
  error,
  errorId,
  wide,
  children,
}: {
  label: string;
  hint?: ReactNode;
  hintId?: string;
  error?: string;
  errorId?: string;
  /** Spans both columns of a two-column form. */
  wide?: boolean;
  children: ReactNode;
}) {
  return (
    <div className={wide ? "fld wide" : "fld"}>
      <label className="f">
        <span className="f-label">{label}</span>
        {children}
      </label>
      {hint !== undefined && (
        <p className="hint" id={hintId}>
          {hint}
        </p>
      )}
      {error && (
        <p className="field-err" id={errorId} role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
