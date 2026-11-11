// Shared: web-cp/src/lib and web-tenant/src/lib carry byte-equal copies of
// this file. shadcn/ui's class joiner: later Tailwind classes win over
// earlier ones that set the same property.
import { clsx, type ClassValue } from "clsx";
import { twMerge } from "tailwind-merge";

export function cn(...inputs: ClassValue[]): string {
  return twMerge(clsx(inputs));
}
