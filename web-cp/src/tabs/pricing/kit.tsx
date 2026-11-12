import type { ComponentProps, ReactNode } from "react";
import { cn } from "../../lib/cn";
import { BentoGrid, CardFooter as PageCardFooter, span } from "../../components/page";

/** 字段随卡片容器排成一至三列；宽字段保留整行。 */
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

/** 表单操作沿用页面卡片页脚，按钮与提示自然换行。 */
export function CardFooter({ className, children }: { className?: string; children: ReactNode }) {
  return (
    <PageCardFooter className={cn("right mt-6 flex flex-wrap items-center justify-end gap-2 border-t border-ink/7 pt-4", className)}>
      {children}
    </PageCardFooter>
  );
}

/** 卡片标题旁的完整条目计数。 */
export function Count({ n }: { n: number }) {
  return <span className="ml-2 inline-flex rounded-full bg-ink/6 px-2 py-1 font-mono text-micro font-normal tabular-nums text-ink-soft">{n.toLocaleString("zh-CN")}</span>;
}

/** 明细先于表单阅读；桌面分为八加四列，窄屏顺序堆叠。 */
export function WithForms({ forms, wide = false, children }: { forms: ReactNode; wide?: boolean; children: ReactNode }) {
  if (!forms) return <>{children}</>;
  return (
    <BentoGrid className="items-start" data-wide-table={wide || undefined}>
      <div className={cn("min-w-0", span.d8)}>{children}</div>
      <div className={cn("min-w-0", span.d4, "tablet:col-span-12")}>
        <div className="grid items-start gap-3 [&>.panel]:mb-0">
          {forms}
        </div>
      </div>
    </BentoGrid>
  );
}
