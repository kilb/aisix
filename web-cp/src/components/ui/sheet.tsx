// shadcn/ui's Sheet (a Radix dialog sliding in from the side), for a form or
// a detail that should not push the list down. Radix gives it role="dialog",
// the title as its name, focus trapping and Escape to close.
import type { ComponentProps, ReactNode } from "react";
import { Dialog as SheetPrimitive } from "radix-ui";
import { X } from "lucide-react";
import { cn } from "../../lib/cn";

export const Sheet = SheetPrimitive.Root;
export const SheetTrigger = SheetPrimitive.Trigger;
export const SheetClose = SheetPrimitive.Close;

export function SheetContent({
  title,
  description,
  side = "right",
  className,
  children,
  ...props
}: ComponentProps<typeof SheetPrimitive.Content> & {
  title: ReactNode;
  description?: ReactNode;
  side?: "right" | "left";
}) {
  return (
    <SheetPrimitive.Portal>
      <SheetPrimitive.Overlay className="fixed inset-0 z-50 bg-black/30 data-[state=open]:animate-in data-[state=open]:fade-in-0 data-[state=closed]:animate-out data-[state=closed]:fade-out-0" />
      <SheetPrimitive.Content
        {...props}
        className={cn(
          "fixed inset-y-0 z-50 flex w-full flex-col gap-0 border-border bg-card shadow-float sm:max-w-[560px]",
          "data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=open]:duration-200 data-[state=closed]:duration-150",
          side === "right"
            ? "right-0 border-l data-[state=open]:slide-in-from-right data-[state=closed]:slide-out-to-right"
            : "left-0 border-r data-[state=open]:slide-in-from-left data-[state=closed]:slide-out-to-left",
          className,
        )}
      >
        <div className="flex items-start justify-between gap-4 border-b border-border px-6 py-4">
          <div className="min-w-0">
            <SheetPrimitive.Title className="text-base font-semibold tracking-tight">{title}</SheetPrimitive.Title>
            {description ? (
              <SheetPrimitive.Description className="mt-1 text-[13px] text-muted-foreground">{description}</SheetPrimitive.Description>
            ) : (
              <SheetPrimitive.Description className="vh">{title}</SheetPrimitive.Description>
            )}
          </div>
          <SheetPrimitive.Close
            className="grid size-8 shrink-0 place-items-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground"
            aria-label="关闭"
          >
            <X className="size-4" aria-hidden />
          </SheetPrimitive.Close>
        </div>
        <div className="flex-1 overflow-y-auto px-6 py-5">{children}</div>
      </SheetPrimitive.Content>
    </SheetPrimitive.Portal>
  );
}
