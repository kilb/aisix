import { useId, type ReactNode } from "react";

/** What a control under a `Field` carries so its refusal describes it. */
export interface FieldAria {
  "aria-invalid"?: true;
  "aria-describedby"?: string;
}

/**
 * One labelled control and, when it was refused, the reason under it in
 * Stop red. `children` is the control, or a function given the attributes
 * that tie the control to that reason.
 */
export function Field({
  label,
  error,
  children,
}: {
  label: string;
  error?: string | null;
  children: ReactNode | ((aria: FieldAria) => ReactNode);
}) {
  const id = useId();
  const aria: FieldAria = error ? { "aria-invalid": true, "aria-describedby": id } : {};
  return (
    <div className="field">
      <label className="f">
        <span>{label}</span>
        {typeof children === "function" ? children(aria) : children}
      </label>
      {error && (
        <p id={id} className="field-problem">
          {error}
        </p>
      )}
    </div>
  );
}
