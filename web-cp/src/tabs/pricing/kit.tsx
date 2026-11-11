import type { ComponentProps, ReactNode } from "react";
import { cn } from "../../lib/cn";

/**
 * The 价格 sections' own pieces on top of components/page.tsx: a form's
 * fields in a grid that follows the width of the card it sits in (one
 * column in a side column, two or three in a full-width card), and the
 * footer bar a card's submit row sits in.
 */

/**
 * Fields in a grid sized by the card, not the screen: one column under
 * 32rem, two from there, three from 48rem when `cols` is 3. A `Field
 * wide` and anything with `col-span-full` take the whole row.
 */
export function FormGrid({ cols = 2, className, ...props }: ComponentProps<"div"> & { cols?: 2 | 3 }) {
  return (
    <div className="@container">
      <div
        {...props}
        className={cn(
          "grid items-start gap-x-4 gap-y-4 @lg:grid-cols-2 [&_.field]:mb-0 [&_label.check]:mb-0 [&>.field.wide]:col-span-full",
          cols === 3 && "@3xl:grid-cols-3",
          className,
        )}
      />
    </div>
  );
}

/** A card's footer bar: its actions at the bottom right, on a muted strip across the card's width. */
export function CardFooter({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <div
      className={cn(
        "right -mx-6 -mb-6 mt-6 flex flex-wrap items-center justify-end gap-2 rounded-b-[calc(var(--radius)+3px)] border-t border-border bg-muted/40 px-6 py-3",
        className,
      )}
    >
      {children}
    </div>
  );
}

/** A count beside a card's title. */
export function Count({ n }: { n: number }) {
  return <span className="tag ml-2 align-[1px] tabular-nums">{n}</span>;
}

/**
 * A detail's table and the forms that change it: the forms in a sticky
 * column beside the table once the screen holds both (from 1280px, or from
 * 1680px with `wide` for a table that needs the room), and in a row of
 * cards above it before that, where a lone form takes the whole row.
 */
export function WithForms({ forms, wide = false, children }: { forms: ReactNode; wide?: boolean; children: ReactNode }) {
  if (!forms) return <>{children}</>;
  return (
    <div
      className={cn(
        "grid items-start gap-6",
        wide ? "min-[1680px]:grid-cols-[minmax(0,1fr)_360px]" : "xl:grid-cols-[minmax(0,1fr)_380px]",
      )}
    >
      <div className={cn("min-w-0", wide ? "min-[1680px]:sticky min-[1680px]:top-20 min-[1680px]:order-2" : "xl:sticky xl:top-20 xl:order-2")}>
        <div
          className={cn(
            // Two across only while the forms sit above the table: a variant
            // bounded on both sides, so nothing has to override it beside it.
            "grid items-start gap-4 [&>.panel]:mb-0",
            wide
              ? "md:max-[1679px]:grid-cols-2 md:max-[1679px]:[&>:only-child]:col-span-2"
              : "md:max-xl:grid-cols-2 md:max-xl:[&>:only-child]:col-span-2",
          )}
        >
          {forms}
        </div>
      </div>
      <div className={cn("min-w-0", wide ? "min-[1680px]:order-1" : "xl:order-1")}>{children}</div>
    </div>
  );
}
