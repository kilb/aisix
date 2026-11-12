export interface DisplayRate {
  key: string;
  label: string;
  amount: string;
  unit: string;
}

export interface DisplayBand {
  key: string;
  label: string;
  rates: DisplayRate[];
}

export interface DisplayTier {
  key: string;
  label: string;
  bands: DisplayBand[];
}

export interface MatrixGroup {
  label: string;
  rows: { key: string; label: string; unit: string; amounts: (string | null)[] }[];
}

const DIMENSION_GROUPS = [
  { label: "文本与推理", keys: ["input", "output", "reasoning"] },
  { label: "缓存", keys: ["cache_read", "cache_write", "cache_write_1h"] },
  { label: "音频", keys: ["audio_input", "audio_output"] },
  { label: "图像", keys: ["image_input", "image_output"] },
  { label: "联网", keys: ["web_search"] },
];
const KNOWN_DIMENSIONS = new Set(DIMENSION_GROUPS.flatMap((group) => group.keys));

/** 同档位的基础价和长上下文共享维度行，未提供的格子保持未知。 */
export function matrixGroups(tier: DisplayTier): MatrixGroup[] {
  const columns = tier.bands.map((band) => new Map(band.rates.map((rate) => [rate.key, rate.amount])));
  const unknown = [...new Set(tier.bands.flatMap((band) => band.rates.map((rate) => rate.key)))].filter((key) => !KNOWN_DIMENSIONS.has(key));
  return [...DIMENSION_GROUPS, ...(unknown.length ? [{ label: "其他计量项", keys: unknown }] : [])].map((group) => ({
    label: group.label,
    rows: group.keys.map((key) => ({
      key,
      label: dimensionText(key),
      unit: isPerCall(key) ? "每千次" : "每百万 tokens",
      amounts: columns.map((column) => column.get(key) ?? null),
    })),
  }));
}

/** 只构造展示行，金额与门槛保持字符串，所有维度与档位逐项保留。 */
export function cardDetails(card: unknown): { tiers: DisplayTier[]; problem: string | null } {
  if (card == null) return { tiers: [], problem: null };
  const problem = cardShapeProblem(card);
  if (problem !== null) return { tiers: [], problem };
  const source = card as Record<string, {
    base?: Record<string, string>;
    long_context?: { above_input_tokens?: unknown; rates?: Record<string, string> }[];
  }>;
  const keys = Object.keys(source).sort((a, b) => a === "default" ? -1 : b === "default" ? 1 : a.localeCompare(b));
  const tiers = keys.map((key): DisplayTier => {
    const part = source[key];
    const contexts = part?.long_context ?? [];
    const first = contexts[0];
    return {
      key,
      label: tierText(key),
      bands: [
        { key: "base", label: first ? `基础价 · 输入 ≤ ${String(first.above_input_tokens)} tokens` : "基础价 · 全部输入长度", rates: displayRates(part?.base ?? {}) },
        ...contexts.map((band, index) => {
          const next = contexts[index + 1];
          return {
            key: `long_context/${index}`,
            label: `长上下文 · 输入 > ${String(band.above_input_tokens)}${next ? ` 且 ≤ ${String(next.above_input_tokens)} tokens` : " tokens（无上界）"}`,
            rates: displayRates(band.rates ?? {}),
          };
        }),
      ],
    };
  });
  return { tiers, problem: null };
}

function displayRates(rates: Record<string, string>): DisplayRate[] {
  return Object.entries(rates).map(([key, rate]) => ({
    key,
    label: dimensionText(key),
    amount: `$${isPerCall(key) ? perThousandOfPerMillion(rate) : rate}`,
    unit: isPerCall(key) ? "每千次" : "每百万 tokens",
  }));
}
import { dimensionText, tierText } from "../../lib/importReview.ts";
import { perThousandOfPerMillion } from "../../lib/money.ts";
import { cardShapeProblem } from "../../lib/rateCardShape.ts";
import { isPerCall } from "../../lib/rateUnits.ts";
