// Shared: web-cp/src/lib and web-tenant/src/lib carry byte-equal copies of
// this file. shadcn/ui's class joiner: later Tailwind classes win over
// earlier ones that set the same property.
import { clsx, type ClassValue } from "clsx";
import { extendTailwindMerge } from "tailwind-merge";

// 注册设计令牌字号，避免类合并器把 text-caption 等误判为颜色并删掉按钮前景。
const mergeBento = extendTailwindMerge({
  extend: {
    classGroups: {
      "font-size": [{ text: ["hero", "display", "figure", "value", "title", "lead", "stat", "brand", "body", "caption", "meta", "micro"] }],
    },
  },
});

export function cn(...inputs: ClassValue[]): string {
  return mergeBento(clsx(inputs));
}
