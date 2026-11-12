import { useEffect, useId, useState } from "react";
import type { Detail, RateCard, RateMap, TierRates } from "../lib/api";
import { problemAt } from "../lib/errors";
import { perMillionOfPerThousand, perThousandOfPerMillion } from "../lib/money";
import { cardShapeProblem } from "../lib/rateCardShape";
import { isPerCall, TIER_HINT } from "../lib/rateUnits";
import { Button } from "../ui/Button";
import { Field } from "../ui/Field";
import { JsonEditor } from "./JsonEditor";

/**
 * The rates the form edits, in every tier's base and every long-context
 * band. Any other rate a card carries is kept as it is and edited in the
 * JSON view.
 */
const RATES: { dimension: string; label: string }[] = [
  { dimension: "input", label: "输入" },
  { dimension: "output", label: "输出" },
  { dimension: "cache_read", label: "缓存读" },
  { dimension: "cache_write", label: "缓存写" },
  { dimension: "cache_write_1h", label: "1 小时缓存写" },
  { dimension: "reasoning", label: "推理" },
  { dimension: "web_search", label: "联网搜索（每千次）" },
];

/** The tiers 添加档位 offers by name; any other name a served tier can carry is typed in. */
const TIERS: { tier: string; label: string }[] = [
  { tier: "batch", label: "批量（batch）" },
  { tier: "flex", label: "flex" },
  { tier: "priority", label: "priority" },
  { tier: "fast", label: "fast" },
];

/** The picker's value for a tier name typed in. */
const OTHER = "";

/** A tier as the form names it: its section's heading and accessible name. */
function tierTitle(tier: string): string {
  if (tier === "default") return "标准档";
  if (tier === "batch") return "批量档";
  return `档位 ${tier}`;
}

/** One long-context band as the card holds it. The threshold is whatever was typed until it is a whole number. */
interface Band {
  above_input_tokens: unknown;
  rates: RateMap;
}

function bandsOf(tier: TierRates | undefined): Band[] {
  const bands = tier?.long_context;
  return Array.isArray(bands) ? (bands as Band[]) : [];
}

/** One JSON Pointer segment, as the control plane writes a refusal's path. */
function segment(s: string): string {
  return s.replace(/~/g, "~0").replace(/\//g, "~1");
}

/** `map` with `dimension` set to `rate`, or without it for `undefined`. */
function withRate(map: RateMap, dimension: string, rate: string | undefined): RateMap {
  const next = { ...map };
  if (rate === undefined) delete next[dimension];
  else next[dimension] = rate;
  return next;
}

/** 费率字段随容器宽度分为一至三列，小屏保持完整输入宽度。 */
const RATE_GRID = "grid grid-cols-1 items-start gap-3 @md:grid-cols-2 @2xl:grid-cols-3 [&_.field]:mb-0";

/** The units the field view takes: a rate per call is typed per thousand. */
const FIELD_UNITS = "token 价格按每百万 tokens 填写；联网搜索按每千次填写，例如「每 1,000 次 10 美元」填 10。";

/** The units the JSON view takes: the card as stored, every rate per million units, a rate per call included. */
const JSON_UNITS = "JSON 里的每个价格都按每百万单位填写：token 按每百万 tokens，联网搜索按每百万次，例如「每 1,000 次 10 美元」填 \"10000\"。";

/** What a rate per call typed per thousand says when it is no amount a card can hold. */
const PER_THOUSAND_PROBLEM = "按每千次的美元数填写，最多 9 位小数";

/**
 * A price card as a form: every tier (the `default` one first, the others
 * added with 添加档位) with its base rates and its long-context bands, each
 * band a threshold and a set of rates. A token rate is typed in USD per
 * million tokens; a rate per call (web search) in USD per thousand calls,
 * and stored, exactly, per million (`perMillionOfPerThousand`), the unit
 * every card holds. A rate left empty is left out of the card, and the
 * server decides whether the card is complete. `编辑完整价格卡（JSON）` opens
 * the whole card in the JSON editor instead, where every rate, a rate per
 * call included, is per million units as the card stores it; the hint above
 * the card says which unit the view on screen takes.
 *
 * Problems whose path starts `/card` show at their field, at their tier or
 * band, or else above the card. `onValidity` reports whether the card can be
 * sent as shown: false while the JSON view holds a document that does not
 * parse (and `value` is the last one that did), a document the field view
 * cannot read, or a rate per call is no amount.
 *
 * The JSON view accepts any document that parses, and the field view can
 * only render one whose tiers, base rates and bands are the shapes it reads.
 * Closing the JSON view over a document that is not one keeps the JSON view
 * open, with the problem named ([`cardShapeProblem`]): the field view would
 * throw while rendering and take the page with it, and the text the operator
 * typed would be gone.
 */
export function RateCardEditor({
  value,
  onChange,
  details,
  onValidity,
}: {
  value: RateCard | null;
  onChange(c: RateCard): void;
  details: Detail[];
  onValidity?(ok: boolean): void;
}) {
  const [json, setJson] = useState<string | null>(null);
  const [jsonError, setJsonError] = useState<string | null>(null);
  /** What the last closing of the JSON view found unreadable as fields, if anything. */
  const [shapeError, setShapeError] = useState<string | null>(null);
  /** What was typed into each rate-per-call field, by its path, so a half-typed amount stays on screen. */
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [adding, setAdding] = useState(TIERS[0]!.tier);
  const [named, setNamed] = useState("");
  const aboveId = useId();

  const card: RateCard = value ?? {};
  const tiers = ["default", ...Object.keys(card).filter((t) => t !== "default")];
  const mine = details.filter((d) => d.path.startsWith("/card"));

  /** Whether a draft is no amount a card can hold. */
  const unreadable = (text: string) => text.trim() !== "" && perMillionOfPerThousand(text.trim()) === null;
  const unread = Object.values(drafts).filter(unreadable).length;
  useEffect(() => {
    onValidity?.(jsonError === null && shapeError === null && unread === 0);
    // onValidity is the form's setter; these are what change.
  }, [jsonError, shapeError, unread]); // eslint-disable-line react-hooks/exhaustive-deps

  // Every path the form shows a refusal at; any other goes above the card.
  const placed = new Set<string>();
  for (const tier of tiers) {
    const at = `/card/${segment(tier)}`;
    placed.add(at);
    placed.add(`${at}/long_context`);
    for (const r of RATES) placed.add(`${at}/base/${r.dimension}`);
    bandsOf(card[tier]).forEach((_, n) => {
      placed.add(`${at}/long_context/${n}/above_input_tokens`);
      for (const r of RATES) placed.add(`${at}/long_context/${n}/rates/${r.dimension}`);
    });
  }
  const above = mine.filter((d) => !placed.has(d.path));
  const at = (path: string) => problemAt(mine, (p) => p === path);

  const tierOf = (tier: string): TierRates => card[tier] ?? { base: {} };
  const setTier = (tier: string, next: TierRates) => onChange({ ...card, [tier]: next });

  function setBands(tier: string, bands: Band[]) {
    const next: TierRates = { ...tierOf(tier) };
    if (bands.length === 0) delete next.long_context;
    else next.long_context = bands;
    setTier(tier, next);
  }

  function setThreshold(tier: string, n: number, text: string) {
    const typed = text.trim();
    // A whole number is sent as one; anything else as typed, for the server to refuse at the field.
    const threshold = /^\d+$/.test(typed) && Number.isSafeInteger(Number(typed)) ? Number(typed) : typed;
    setBands(tier, bandsOf(card[tier]).map((b, i) => (i === n ? { ...b, above_input_tokens: threshold } : b)));
  }

  function addBand(tier: string) {
    const bands = bandsOf(card[tier]);
    // Another tier's bands sit at the default tier's thresholds: the next one is offered.
    const threshold = tier === "default" ? undefined : bandsOf(card.default)[bands.length]?.above_input_tokens;
    setBands(tier, [...bands, { above_input_tokens: threshold ?? "", rates: {} }]);
  }

  /** A change of shape moves fields; what was typed for one is not carried to another. */
  function reshaped(next: RateCard) {
    setDrafts({});
    onChange(next);
  }

  function removeBand(tier: string, n: number) {
    setDrafts({});
    setBands(tier, bandsOf(card[tier]).filter((_, i) => i !== n));
  }

  const offered = TIERS.filter((t) => !Object.hasOwn(card, t.tier));
  const picked = adding === OTHER || offered.some((t) => t.tier === adding) ? adding : (offered[0]?.tier ?? OTHER);
  const newTier = picked === OTHER ? named.trim() : picked;

  function addTier() {
    if (newTier === "" || Object.hasOwn(card, newTier)) return;
    // At the default tier's thresholds, the one shape the server accepts for another tier's bands.
    const bands = bandsOf(card.default).map((b) => ({ above_input_tokens: b.above_input_tokens, rates: {} }));
    reshaped({ ...card, [newTier]: bands.length > 0 ? { base: {}, long_context: bands } : { base: {} } });
    setNamed("");
  }

  function removeTier(tier: string) {
    const next = { ...card };
    delete next[tier];
    reshaped(next);
  }

  /** One rate's field, at `path`, in `map`. */
  function rateField(path: string, dimension: string, label: string, map: RateMap, set: (rate: string | undefined) => void) {
    const stored = map[dimension];
    if (!isPerCall(dimension)) {
      return (
        <Field key={path} label={label} error={at(path)}>
          {(aria) => (
            <input
              {...aria}
              inputMode="decimal"
              autoComplete="off"
              placeholder="美元/百万 tokens"
              value={stored ?? ""}
              onChange={(e) => {
                const rate = e.target.value.trim();
                set(rate === "" ? undefined : rate);
              }}
            />
          )}
        </Field>
      );
    }
    const draft = drafts[path];
    const shown = draft ?? (stored === undefined ? "" : perThousandOfPerMillion(stored));
    return (
      <Field key={path} label={label} error={draft !== undefined && unreadable(draft) ? PER_THOUSAND_PROBLEM : at(path)}>
        {(aria) => (
          <input
            {...aria}
            inputMode="decimal"
            autoComplete="off"
            placeholder="美元/千次"
            value={shown}
            onChange={(e) => {
              const text = e.target.value;
              setDrafts((d) => ({ ...d, [path]: text }));
              const typed = text.trim();
              // An amount that is not one yet is left out of the card until it is.
              set(typed === "" ? undefined : (perMillionOfPerThousand(typed) ?? undefined));
            }}
          />
        )}
      </Field>
    );
  }

  function setJsonText(text: string) {
    setJson(text);
    try {
      const parsed: unknown = JSON.parse(text);
      if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) throw new Error("价格卡应是一个对象");
      setJsonError(null);
      // The text changed: a shape complaint was about the previous document.
      setShapeError(null);
      onChange(parsed as RateCard);
    } catch (e) {
      setJsonError(`不是合法的价格卡 JSON，修改尚未生效：${e instanceof Error ? e.message : String(e)}`);
    }
  }

  function toggleJson() {
    // 无法换算的每千次单价只存在字段草稿中，先纠正再切换视图。
    if (json === null && unread > 0) return;
    setDrafts({});
    if (json === null) {
      // Opening the JSON view: the fields' card as a document.
      setJsonError(null);
      setShapeError(null);
      setJson(JSON.stringify(value ?? {}, null, 2));
      return;
    }
    // 语法错误时 value 仍是上次合法文档，不能用它切回字段并丢掉原始输入。
    if (jsonError !== null) return;
    // Closing it: the fields can render only a card whose tiers, base rates
    // and bands are the shapes they read. A card that is not stays here, with
    // what is wrong named, so the text typed is not lost to a blank page.
    const problem = cardShapeProblem(value ?? {});
    if (problem !== null) {
      setShapeError(problem);
      return;
    }
    setShapeError(null);
    setJson(null);
  }

  return (
    <fieldset className="rate-card min-w-0 text-body" aria-invalid={mine.length > 0 ? true : undefined} aria-describedby={above.length > 0 ? aboveId : undefined}>
      <legend>价格卡（美元）</legend>
      <p className="hint mb-3 text-caption text-ink-soft">{json === null ? FIELD_UNITS : JSON_UNITS}</p>
      {above.length > 0 && (
        <div className="note crit" role="alert" id={aboveId}>
          {above.map((d) => d.message).join("；")}
        </div>
      )}
      {json === null ? (
        <>
          {tiers.map((tier) => {
            const t = tierOf(tier);
            const path = `/card/${segment(tier)}`;
            const bands = bandsOf(card[tier]);
            const tierProblem = at(path);
            const bandsProblem = at(`${path}/long_context`);
            return (
              <section
                key={tier}
                className="rate-tier @container m-0 mb-3 rounded-3xl border border-line bg-surface p-7"
                role="group"
                aria-label={tierTitle(tier)}
              >
                <div className="mb-4 flex min-h-11 flex-wrap items-center justify-between gap-3">
                  <h4 className="m-0 font-mono text-meta font-medium text-ink">{tierTitle(tier)}</h4>
                  {tier !== "default" && (
                    <Button type="button" size="sm" kind="quiet" onClick={() => removeTier(tier)}>
                      移除档位
                    </Button>
                  )}
                </div>
                {tierProblem && (
                  <div className="note crit" role="alert">
                    {tierProblem}
                  </div>
                )}
                <div className={RATE_GRID} role="group" aria-label={`${tierTitle(tier)} 基础价`}>
                  {RATES.map((r) =>
                    rateField(`${path}/base/${r.dimension}`, r.dimension, r.label, t.base ?? {}, (rate) =>
                      setTier(tier, { ...t, base: withRate(t.base ?? {}, r.dimension, rate) }),
                    ),
                  )}
                </div>
                {bandsProblem && (
                  <div className="note crit" role="alert">
                    {bandsProblem}
                  </div>
                )}
                {bands.map((band, n) => {
                  const bandPath = `${path}/long_context/${n}`;
                  const threshold = `${bandPath}/above_input_tokens`;
                  return (
                    <div
                      key={n}
                      className="rate-band mt-3 mb-0 rounded-2xl border border-line bg-well p-4 text-ink-soft"
                      role="group"
                      aria-label={`${tierTitle(tier)} 长上下文档 ${n + 1}`}
                    >
                      <div className={RATE_GRID}>
                        <Field label="输入超过（tokens）" error={at(threshold)}>
                          {(aria) => (
                            <input
                              {...aria}
                              inputMode="numeric"
                              autoComplete="off"
                              placeholder="200000"
                              value={String(band.above_input_tokens ?? "")}
                              onChange={(e) => setThreshold(tier, n, e.target.value)}
                            />
                          )}
                        </Field>
                        {RATES.map((r) =>
                          rateField(`${bandPath}/rates/${r.dimension}`, r.dimension, r.label, band.rates ?? {}, (rate) =>
                            setBands(
                              tier,
                              bands.map((b, i) => (i === n ? { ...b, rates: withRate(b.rates ?? {}, r.dimension, rate) } : b)),
                            ),
                          ),
                        )}
                      </div>
                      <div className="mt-2 flex justify-end">
                        <Button type="button" size="sm" kind="quiet" onClick={() => removeBand(tier, n)}>
                          移除这个长上下文档
                        </Button>
                      </div>
                    </div>
                  );
                })}
                <div className="mt-3">
                  <Button type="button" size="sm" onClick={() => addBand(tier)}>
                    添加长上下文档
                  </Button>
                </div>
              </section>
            );
          })}
          <div className="rate-add-tier flex min-w-0 flex-wrap items-end gap-3 [&_.field]:mb-0 [&_.field]:min-w-0 [&_.field]:basis-48 [&_.field]:flex-1">
            <Field label="要添加的档位">
              <select value={picked} onChange={(e) => setAdding(e.target.value)}>
                {offered.map((t) => (
                  <option key={t.tier} value={t.tier}>
                    {t.label}
                  </option>
                ))}
                <option value={OTHER}>其他（填写名称）</option>
              </select>
            </Field>
            {picked === OTHER && (
              <Field label="档位名称" hint="上游报告的服务档位名称：小写字母、数字、_ 和 -">
                {(aria) => <input {...aria} autoComplete="off" value={named} onChange={(e) => setNamed(e.target.value)} />}
              </Field>
            )}
            <Button type="button" size="sm" disabled={newTier === "" || Object.hasOwn(card, newTier)} onClick={addTier}>
              添加档位
            </Button>
          </div>
          <p className="hint mt-3 text-caption text-ink-soft">{TIER_HINT}</p>
        </>
      ) : (
        <>
          <JsonEditor value={json} onChange={setJsonText} details={mine.map((d) => ({ ...d, path: d.path.slice("/card".length) }))} />
          {(jsonError ?? shapeError) && (
            <div className="note crit" role="alert">
              {jsonError ?? shapeError}
            </div>
          )}
        </>
      )}
      <div className="mt-3">
        <Button type="button" size="sm" aria-pressed={json !== null} onClick={toggleJson}>
          编辑完整价格卡（JSON）
        </Button>
      </div>
    </fieldset>
  );
}
