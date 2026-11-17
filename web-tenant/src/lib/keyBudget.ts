import { isCnyAmount } from "./money.ts";

const MICRO = 1_000_000n;

/** 新输入沿用账务API的元分合同；读取旧值仍保留存储的微元精度。 */
export function isKeyBudgetAmount(text: string): boolean {
  return isCnyAmount(text, { allowZero: false });
}

/** 原值逐位转回输入，不能把API中的微元额度改成整分。 */
export function keyBudgetAmountInput(micro: number | string | bigint): string {
  if (typeof micro === "number" && !Number.isSafeInteger(micro)) throw new RangeError("额度超过浏览器可精确表示的整数范围");
  const value = BigInt(micro);
  const fraction = (value % MICRO).toString().padStart(6, "0").replace(/0+$/, "");
  return `${value / MICRO}${fraction ? `.${fraction}` : ""}`;
}
