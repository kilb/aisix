import { useId, type ReactNode } from "react";
import type { CooldownSettings, ResourceView } from "../../lib/api";
import { cn } from "../../lib/cn";
import type { FieldAria } from "../../ui/Field";

/** One routing target; fields this page does not edit (such as `tags`) are kept as stored. */
export interface Target {
  model_id?: string | null;
  /** The display-name form of the reference, read only without `model_id`. */
  model?: string;
  weight?: number;
  priority?: number;
  [field: string]: unknown;
}

/** The fields of a `model` this page edits; every other field is kept as stored. */
export interface ModelDoc {
  display_name?: string;
  provider?: string;
  model_name?: string;
  provider_key_id?: string;
  cooldown?: CooldownSettings | null;
  routing?: { strategy?: string; targets?: Target[]; [field: string]: unknown } | null;
  ensemble?: unknown;
  semantic?: unknown;
  [field: string]: unknown;
}

/** What this page reads of an upstream account. */
export interface AccountDoc {
  display_name?: string;
  provider?: string;
  groups?: string[];
  [field: string]: unknown;
}

export type Model = ResourceView<ModelDoc>;
export type Account = ResourceView<AccountDoc>;

/** The models and accounts a row or form names, by id. */
export interface Catalog {
  models: Map<string, Model>;
  accounts: Map<string, Account>;
}

export type SchemaInfo = { strategies: string[]; preset: string | null } | { error: string };
export const GENERATED_STRATEGY = "round_robin";

/** `可服务分组：` and the groups, on a line of their own under what they belong to. */
export function ServedGroups({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div className={cn("sub mt-1 flex flex-wrap items-center gap-x-1.5 gap-y-1 font-mono text-meta", className)}>
      <span>可服务分组：</span>
      {children}
    </div>
  );
}

/** A form card's closing bar: its buttons at the bottom right, on a muted strip the card's width. */
export const FORM_FOOTER =
  "-mx-[var(--pad)] -mb-[var(--pad)] mt-6 flex max-w-none flex-wrap items-center justify-end gap-2 rounded-b-card border-t border-line bg-well px-[var(--pad)] py-3 [&_.hint]:text-ink-soft";

/**
 * A field in a row of fields: its label above it while the row is stacked,
 * and only for assistive technology once the row is one line under a header
 * that names its columns (the row's container query, `@lg`, or `@3xl` with
 * `at="3xl"`). Its refusal, when there is one, is under it and describes it.
 */
export function RowField({
  label,
  error,
  at = "lg",
  className,
  children,
}: {
  label: string;
  error?: string | null;
  at?: "lg" | "3xl";
  className?: string;
  children: (aria: FieldAria) => ReactNode;
}) {
  const id = useId();
  const errorId = `${id}-error`;
  const aria: FieldAria = error ? { "aria-invalid": true, "aria-describedby": errorId } : {};
  return (
    <div className={cn("min-w-0", className)}>
      <label className="flex flex-col gap-1.5">
        <span className={cn("text-caption font-medium text-muted", at === "lg" ? "@lg:sr-only" : "@3xl:sr-only")}>{label}</span>
        {children(aria)}
      </label>
      {error && (
        <p className="field-problem" id={errorId}>
          {error}
        </p>
      )}
    </div>
  );
}
