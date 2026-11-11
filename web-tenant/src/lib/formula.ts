/** 精确十进制的整数与小数位；不经过二进制浮点。 */
function decimal(value: string): { units: bigint; scale: number } | null {
  if (!/^\d+(?:\.\d+)?$/.test(value)) return null;
  const [whole = "0", fraction = ""] = value.split(".");
  return { units: BigInt(whole + fraction), scale: fraction.length };
}

function decimalText(units: bigint, scale: number): string {
  const digits = units.toString().padStart(scale + 1, "0");
  if (scale === 0) return digits;
  const fraction = digits.slice(-scale).replace(/0+$/, "");
  return fraction ? `${digits.slice(0, -scale)}.${fraction}` : digits.slice(0, -scale);
}

function retained(discount: number | null): bigint | null {
  return discount !== null && Number.isInteger(discount) && discount >= 0 && discount <= 10000 ? BigInt(10000 - discount) : null;
}

/** 历史折前单价乘当次保留比例，保持全部十进制位。 */
export function discountedRate(rate: string, discount: number | null): string | null {
  const parsed = decimal(rate);
  const keep = retained(discount);
  return parsed === null || keep === null ? null : decimalText(parsed.units * keep, parsed.scale + 4);
}

/** 元/百万单位乘数量后恰为微元；整笔折扣后仅向上取整一次。 */
export function feeMicroCny(lines: readonly { quantity: number; rate_cny: string | null }[], discount: number | null): string | null {
  const keep = retained(discount);
  if (keep === null) return null;
  let sum = 0n;
  let scale = 0;
  for (const line of lines) {
    if (!Number.isSafeInteger(line.quantity) || line.quantity < 0 || line.rate_cny === null) return null;
    const rate = decimal(line.rate_cny);
    if (rate === null) return null;
    if (rate.scale > scale) {
      sum *= 10n ** BigInt(rate.scale - scale);
      scale = rate.scale;
    }
    sum += BigInt(line.quantity) * rate.units * 10n ** BigInt(scale - rate.scale);
  }
  const denominator = 10n ** BigInt(scale) * 10000n;
  return ((sum * keep + denominator - 1n) / denominator).toString();
}
