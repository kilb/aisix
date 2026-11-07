import { useId, type ReactNode } from "react";

/** What a control under a `Field` carries so its refusal describes it. */
export interface FieldAria {
  "aria-invalid"?: true;
  "aria-describedby"?: string;
}

/**
 * One labelled control and, when the control plane refused it, the reason
 * under it. `children` is given the attributes that tie the control to that
 * reason.
 */
export function Field({
  label,
  problem,
  children,
}: {
  label: string;
  problem: string | null;
  children: (aria: FieldAria) => ReactNode;
}) {
  const id = useId();
  const aria: FieldAria = problem ? { "aria-invalid": true, "aria-describedby": id } : {};
  return (
    <div className="field">
      <label className="f">
        <span>{label}</span>
        {children(aria)}
      </label>
      {problem && (
        <p id={id} className="field-problem">
          {problem}
        </p>
      )}
    </div>
  );
}
