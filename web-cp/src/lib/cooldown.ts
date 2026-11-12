import type { CooldownSettings } from "./api.ts";

// JSON 数字超出浏览器安全整数范围时，禁止将舍入后的值重新发给后端。
export function cooldownProblem(value: CooldownSettings | null | undefined): string | null {
  for (const key of ["default_seconds", "max_seconds"] as const) {
    const seconds = value?.[key];
    if (seconds !== undefined && (!Number.isSafeInteger(seconds) || seconds < (key === "max_seconds" ? 1 : 0))) {
      return "冷却秒数超出浏览器可精确编辑的范围或不是合法整数；请通过管理 API 修改，或清除冷却配置。";
    }
  }
  return null;
}
