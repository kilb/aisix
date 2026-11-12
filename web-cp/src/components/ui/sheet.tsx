// 侧面弹层承载表单与详情，不挤开原列表；保留 Radix 的对话框名称、
// 焦点约束、关闭后的焦点返回和 Escape 关闭行为。
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
      <SheetPrimitive.Overlay className="fixed inset-0 z-50 bg-ink/35" />
      <SheetPrimitive.Content
        {...props}
        className={cn(
          "fixed inset-y-0 z-50 flex w-full flex-col gap-0 border-line bg-surface text-ink sm:max-w-140",
          side === "right" ? "right-0 border-l" : "left-0 border-r",
          className,
        )}
      >
        <div className="flex items-start justify-between gap-4 border-b border-line p-7">
          <div className="min-w-0">
            <SheetPrimitive.Title className="text-stat font-medium tracking-tight">{title}</SheetPrimitive.Title>
            {description ? (
              <SheetPrimitive.Description className="mt-2 text-caption text-muted">{description}</SheetPrimitive.Description>
            ) : (
              <SheetPrimitive.Description className="vh">{title}</SheetPrimitive.Description>
            )}
          </div>
          <SheetPrimitive.Close
            className="grid size-11 shrink-0 place-items-center rounded-full border border-line bg-surface text-ink hover:bg-well focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-ink"
            aria-label="关闭"
          >
            <X className="size-4" aria-hidden />
          </SheetPrimitive.Close>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto p-7">{children}</div>
      </SheetPrimitive.Content>
    </SheetPrimitive.Portal>
  );
}
