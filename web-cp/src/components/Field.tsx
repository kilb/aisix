import type { ReactNode } from "react";
import { Field as UiField, type FieldAria } from "../ui/Field";

export type { FieldAria };

/**
 * The P1b signature of `ui/Field` (`problem` for its `error`), kept so a
 * page written against it still resolves. New code uses `ui/Field`.
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
  return (
    <UiField label={label} error={problem}>
      {children}
    </UiField>
  );
}
