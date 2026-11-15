import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { build } from "esbuild";
import { chromium, expect } from "@playwright/test";
import type { PriceQuoteList } from "./priceQuote";

// 组件单元测试使用真实 ReactDOM 连续交互；不启动或拦截 API，不以 SSR 单帧代替状态转换。
// 真实后端合同由另外的 CP/PG 浏览器旅程验证。
const source = new URL("../Prices.tsx", import.meta.url);
const compiled = await build({
  stdin: {
    loader: "tsx", resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "price-tier-journey.tsx",
    contents: `${await readFile(source, "utf8")}
      import { createRoot } from "react-dom/client";
      function TierJourney({ quote }) {
        const [group, setGroup] = useState("");
        return <PriceCatalog quote={quote} groupId={group} onGroupChange={setGroup} />;
      }
      globalThis.mountTierJourney = quote => createRoot(document.getElementById("root")).render(<TierJourney quote={quote} />);`,
  },
  bundle: true, platform: "browser", format: "iife", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "import.meta.env.BASE_URL": '"/"', "process.env.NODE_ENV": '"test"' },
});
const rates = (input: string, output: string) => ({ base: [
  { dimension: "input" as const, unit: "token" as const, price_cny_per_million: input },
  { dimension: "output" as const, unit: "token" as const, price_cny_per_million: output },
], long_context: [] });
const quote: PriceQuoteList = {
  currency: "CNY", discount_bps: 0, pricing_version: 1, as_of: "2026-11-14T00:00:00Z", valid_until: null,
  base_models: [
    { model_id: "model-a", name: "模型 A", currency: "USD", prices_by_tier: { default: { base: [{ dimension: "input", unit: "token", price_usd_per_million: "2.00000000000001" }, { dimension: "output", unit: "token", price_usd_per_million: "4" }], long_context: [] }, batch: { base: [{ dimension: "input", unit: "token", price_usd_per_million: "1" }], long_context: [] } } },
    { model_id: "model-b", name: "模型 B", currency: "USD", prices_by_tier: { default: { base: [{ dimension: "input", unit: "token", price_usd_per_million: "3" }], long_context: [] } } },
  ],
  groups: [
    { id: "a", display_name: "分组 A", description: "", models: [{ model_id: "model-a", name: "模型 A", input_per_million: "10", output_per_million: "20", cny_per_usd: "1", effective_multiplier: "1", prices_by_tier: { default: rates("10", "20"), batch: rates("5", "8") } }] },
    { id: "b", display_name: "分组 B", description: "", models: [{ model_id: "model-b", name: "模型 B", input_per_million: "7", output_per_million: "11", cny_per_usd: "1", effective_multiplier: "1", prices_by_tier: { default: rates("7", "11") } }] },
  ],
};

test("分组切换的选择框、档位标题与精确报价同步，并恢复有效用户偏好", { timeout: 30_000 }, async () => {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    const requests: string[] = [];
    page.on("request", request => requests.push(request.url()));
    await page.setContent('<div id="root"></div>');
    await page.addScriptTag({ content: compiled.outputFiles[0]!.text });
    await page.evaluate(value => (globalThis as typeof globalThis & { mountTierJourney: (q: PriceQuoteList) => void }).mountTierJourney(value), quote);
    const scopes = page.getByRole("group", { name: "价格分组", exact: true });
    const group = (name: string) => scopes.getByRole("button", { name, exact: true });
    const tier = page.getByRole("combobox", { name: "服务档位", exact: true });
    const search = page.getByRole("searchbox", { name: "搜索模型价格", exact: true });
    const title = page.locator(".price-model-tier");
    await expect(group("Base USD")).toHaveAttribute("aria-pressed", "true");
    await expect(page.locator('[data-price-model="model-a"] [data-label="输入"] .price-rate-amount')).toHaveText("$2.00000000000001");
    await group("分组 A").click();
    await tier.click(); await page.getByRole("option", { name: "Batch", exact: true }).click();
    await expect(title).toHaveText("Batch");
    await expect(page.locator('[data-label="输入"] .price-rate-amount')).toHaveText("¥5.00");
    await group("分组 B").click();
    await expect(tier).toHaveAttribute("data-value", "default");
    await expect(title).toHaveText("标准");
    await expect(page.locator('[data-label="输入"] .price-rate-amount')).toHaveText("¥7.00");
    await expect(page.locator('[data-label="输出"] .price-rate-amount')).toHaveText("¥11.00");
    await group("分组 A").click();
    await expect(tier).toHaveAttribute("data-value", "batch");
    await expect(title).toHaveText("Batch");
    await expect(page.locator('[data-label="输入"] .price-rate-amount')).toHaveText("¥5.00");
    await search.fill("不存在的模型");
    await expect(page.getByRole("table")).toHaveCount(0);
    await search.fill("");
    await expect(tier).toHaveAttribute("data-value", "batch");
    await expect(title).toHaveText("Batch");
    await expect(page.locator('[data-label="输出"] .price-rate-amount')).toHaveText("¥8.00");
    assert.deepEqual(requests, [], "组件测试不发 API 或模型请求");
  } finally {
    await browser.close();
  }
});

test("模型详情列出全部分组倍率，切组后精确价卡可达，Escape 返回模型按钮", { timeout: 30_000 }, async () => {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    const shared = { ...quote, base_models: quote.base_models!.filter(model => model.model_id === "model-a"), groups: [
      quote.groups[0]!,
      { ...quote.groups[1]!, models: [{ ...quote.groups[1]!.models[0]!, model_id: "model-a", name: "模型 A", effective_multiplier: "2.33333333333333" }] },
    ] };
    await page.setContent('<div id="root"></div>');
    await page.addScriptTag({ content: compiled.outputFiles[0]!.text });
    await page.evaluate(value => (globalThis as typeof globalThis & { mountTierJourney: (q: PriceQuoteList) => void }).mountTierJourney(value), shared);
    const trigger = page.getByRole("button", { name: "查看 模型 A 的价格详情", exact: true });
    await trigger.click();
    const sheet = page.getByRole("dialog", { name: "模型 A", exact: true });
    await expect(sheet).toBeVisible();
    const groups = sheet.getByRole("group", { name: "详情价格分组", exact: true });
    await expect(groups.getByRole("button")).toHaveCount(3);
    await expect(groups.locator('[data-price-scope="a"]')).toContainText("1×");
    await expect(groups.locator('[data-price-scope="b"]')).toContainText("2.33333333333333×");
    await expect(sheet.getByText("$2.00000000000001", { exact: true })).toBeVisible();
    await groups.locator('[data-price-scope="b"]').click();
    await expect(sheet.getByText("¥7.00", { exact: true })).toBeVisible();
    await expect(sheet.getByText("$2.00000000000001", { exact: true })).toHaveCount(0);
    await page.keyboard.press("Escape");
    await expect(sheet).toHaveCount(0);
    await expect(trigger).toBeFocused();
    await expect(page.getByRole("group", { name: "价格分组", exact: true }).getByRole("button", { name: "Base USD", exact: true })).toHaveAttribute("aria-pressed", "true");
  } finally {
    await browser.close();
  }
});

test("旧接口缺美元字段时明确不可用，仍能读取分组人民币报价", { timeout: 30_000 }, async () => {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    const legacy = { ...quote };
    delete legacy.base_models;
    await page.setContent('<div id="root"></div>');
    await page.addScriptTag({ content: compiled.outputFiles[0]!.text });
    await page.evaluate(value => (globalThis as typeof globalThis & { mountTierJourney: (q: PriceQuoteList) => void }).mountTierJourney(value), legacy);
    await expect(page.getByText("暂无法读取美元基准价，请稍后刷新或选择分组查看人民币报价。", { exact: true })).toBeVisible();
    await expect(page.getByRole("table")).toHaveCount(0);
    await page.getByRole("group", { name: "价格分组", exact: true }).getByRole("button", { name: "分组 A", exact: true }).click();
    await expect(page.locator('[data-label="输入"] .price-rate-amount')).toHaveText("¥10.00");
  } finally {
    await browser.close();
  }
});
