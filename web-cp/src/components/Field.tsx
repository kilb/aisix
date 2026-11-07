import type { ReactNode } from "react";
import { Field as UiField, type FieldAria } from "../ui/Field";

export type { FieldAria };

/**
 * The P1b signature of `ui/Field` (`problem` for its `error`), kept so a
 * page written against it still resolves. No page imports it any more; it
 * is kept only until the owner agrees to delete the file.
 *
 * @deprecated Use `Field` from `ui/Field`.
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
