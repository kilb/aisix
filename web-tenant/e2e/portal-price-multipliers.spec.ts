import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type ApiClient, type Stack } from "./stack";

let stack: Stack;
const PASSWORD = "pricing-model-groups-pass-1112";
const DISCOUNT = 1375;
const RATES = { input: "4", output: "20", cache_read: "0", cache_write: null, cache_write_1h: "8", reasoning: "9", audio_input: "10", audio_output: "11", image_input: "12", image_output: "13", web_search: "1250" };
const TIERS = ["default", "ultrafast", "fast", "priority", "batch", "flex"] as const;
const DIMENSIONS = [
  ["input", "输入"], ["output", "输出"], ["cache_read", "缓存读"], ["cache_write", "缓存写"],
  ["cache_write_1h", "1 小时缓存写"], ["reasoning", "推理"], ["audio_input", "音频输入"],
  ["audio_output", "音频输出"], ["image_input", "图像输入"], ["image_output", "图像输出"], ["web_search", "联网搜索"],
] as const;

// 场景输入与独立十进制算法定义预期值，不从产品代码或报价回读预期金额。
function tierCard(tier: typeof TIERS[number]) {
  const input = tier === "default" ? "4" : tier === "batch" ? "2" : `${TIERS.indexOf(tier) + 1}.000001`;
  const rates = { ...RATES, web_search: tier === "flex" || tier === "default" ? null : "1250" };
  return {
    base: { ...rates, input, output: tier === "batch" ? "10" : "20" },
    long_context: [
      { above_input_tokens: 200000, rates: { ...RATES, input: "8.000001", output: "40" } },
      { above_input_tokens: 400000, rates: { ...RATES, input: "16.000001", output: "80" } },
    ],
  };
}

function amount(value: string, multiplier: string, currency: "USD" | "CNY", request = false): string {
  const decimal = (text: string): [bigint, number] => {
    const [whole, fraction = ""] = text.split(".");
    return [BigInt(`${whole}${fraction}`), fraction.length];
  };
  const [base, baseScale] = decimal(value);
  const [factor, factorScale] = decimal(multiplier);
  const scale = baseScale + factorScale + (request ? 3 : 0);
  const product = base * factor;
  const power = 10n ** BigInt(scale);
  const fraction = scale ? (product % power).toString().padStart(scale, "0").replace(/0+$/, "") : "";
  return `${currency === "USD" ? "$" : "¥"}${product / power}.${fraction.padEnd(2, "0")}`;
}

async function matrix(detail: Locator, currency: "USD" | "CNY", multiplier: string): Promise<void> {
  await expect(detail.getByRole("table")).toHaveCount(6);
  for (const tier of TIERS) {
    const section = detail.getByRole("region", { name: `${tier === "default" ? "标准" : tier}档完整模型单价`, exact: true });
    const table = section.getByRole("table");
    await expect(table.getByRole("rowheader").filter({ hasText: /^(输入|输出|缓存读|缓存写|1 小时缓存写|推理|音频输入|音频输出|图像输入|图像输出|联网搜索)$/ })).toHaveCount(11);
    const card = tierCard(tier);
    for (const [dimension, label] of DIMENSIONS) {
      await expect(table.getByRole("rowheader", { name: label, exact: true })).toHaveCount(1);
      const row = table.getByRole("rowheader", { name: label, exact: true }).locator("..");
      const unit = currency === "USD" ? "美元" : "元";
      await expect(row.getByRole("cell").nth(0)).toHaveText(`${unit}/${dimension === "web_search" ? "千次" : "百万 token"}`);
      for (const [index, band] of [card.base, ...card.long_context.map((entry) => entry.rates)].entries()) {
        const value = dimension === "cache_write" ? band.input : band[dimension];
        const cell = row.getByRole("cell").nth(index + 1);
        if (value === null) await expect(cell).toHaveText("未提供价格");
        else await expect(cell).toHaveText(`${amount(value, multiplier, currency, dimension === "web_search")}${dimension === "cache_write" ? "按输入价计费" : ""}`);
      }
    }
    await expect(table.getByRole("columnheader").filter({ hasText: "输入 > 200,000 token" })).toContainText("且 ≤ 400,000 token");
    await expect(table.getByRole("columnheader").filter({ hasText: "输入 > 400,000 token" })).toHaveCount(1);
  }
}

async function noPageOverflow(page: Page): Promise<void> {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
}

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
  expect(stack.mockUpstreamUrl).toBe("");
});
test.afterAll(async () => { await stack?.stop(); });

// 价格目录本身无需模型调用；所有分组、价卡和组织均写入真实隔离控制平面。
async function priceWorld(): Promise<{ api: ApiClient; email: string; a: string; b: string; ids: string[]; names: string[]; suffix: string }> {
  const suffix = randomUUID().slice(0, 8);
  const groups = await Promise.all([
    stack.admin.post("/admin/provider-groups", { name: `price-a-${suffix}`, display_name: `价格组 A ${suffix}`, description: "分组默认换算率", default_cny_per_usd: "7.2" }),
    stack.admin.post("/admin/provider-groups", { name: `price-b-${suffix}`, display_name: `价格组 B ${suffix}`, description: "有模型专属换算率", default_cny_per_usd: "5" }),
  ]);
  const account = await stack.admin.post("/admin/resources/provider_keys", { doc: {
    display_name: `价格目录配置 ${suffix}`, provider: "openai", api_key: `pricing-catalog-unused-${suffix}`,
    api_base: "https://api.example.test/v1", groups: [`price-a-${suffix}`, `price-b-${suffix}`],
  } });
  const models = await Promise.all(Array.from({ length: 12 }, async (_, index) => {
    const name = `价格模型 ${String(index + 1).padStart(2, "0")} ${suffix}`;
    const model = await stack.admin.post("/admin/resources/models", { doc: {
      display_name: name, provider: "openai", model_name: "catalogue-model", provider_key_id: account.id,
    } });
    await stack.admin.post("/admin/pricing/baselines", { model_id: model.id, effective_from: new Date().toISOString(), note: "租户模型分组报价与完整价卡验收", card: Object.fromEntries(TIERS.map((tier) => [tier, tierCard(tier)])) });
    return { id: model.id as string, name };
  }));
  await stack.admin.post(`/admin/provider-groups/${groups[1]!.id}/rates`, { model_id: models[0]!.id, cny_per_usd: "6", effective_from: new Date().toISOString() });
  const email = `price-${suffix}@models.test`;
  const api = stack.tenant();
  const signup = await api.post("/api/signup", { org_name: `价格组织 ${suffix}`, email, password: PASSWORD });
  await stack.admin.post(`/admin/tenants/${signup.tenant.id}/approve`);
  const tenant = await stack.admin.get(`/admin/tenants/${signup.tenant.id}`);
  await stack.admin.patch(`/admin/tenants/${signup.tenant.id}`, { allowed_model_ids: models.map((model) => model.id), allowed_provider_groups: [`price-a-${suffix}`, `price-b-${suffix}`] }, tenant.version);
  await stack.admin.post("/admin/pricing/discounts", { tenant_id: signup.tenant.id, discount_bps: DISCOUNT, effective_from: new Date().toISOString() });
  await api.post("/api/login", { email, password: PASSWORD });
  return { api, email, a: groups[0]!.id, b: groups[1]!.id, ids: models.map((model) => model.id), names: models.map((model) => model.name), suffix };
}

test("美元基准与分组精确报价切换、详情、搜索和分页保留完整价卡", async ({ page, context }, info) => {
  test.setTimeout(180_000);
  const world = await priceWorld();
  const quote = await world.api.get("/api/prices");
  expect(quote.discount_bps).toBe(DISCOUNT);
  for (const [id, fx, multiplier, input] of [[world.a, "7.2", "6.21", "24.84"], [world.b, "6", "5.175", "20.7"]]) {
    const group = quote.groups.find((item: { id: string }) => item.id === id);
    const model = group.models.find((item: { model_id: string }) => item.model_id === world.ids[0]);
    expect(model).toMatchObject({ cny_per_usd: fx, effective_multiplier: multiplier, input_per_million: input });
  }
  const baseline = quote.base_models.find((item: { model_id: string }) => item.model_id === world.ids[0]);
  expect(baseline.currency).toBe("USD");
  expect(baseline.prices_by_tier.default.base.find((rate: { dimension: string }) => rate.dimension === "input").price_usd_per_million).toBe("4");
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(world.email);
  await page.getByLabel("口令", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await page.getByRole("link", { name: "模型与价格", exact: true }).click();
  await expect(page).toHaveURL(/#prices(?:\?|$)/);
  await expect(page).toHaveTitle(/\S/);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
  const runtimeErrors: string[] = [];
  page.on("pageerror", (error) => runtimeErrors.push(error.message));
  page.on("console", (message) => { if (message.type() === "error") runtimeErrors.push(message.text()); });
  await expect(page.getByText("13.75%", { exact: true })).toBeVisible();
  const catalog = page.getByRole("table", { name: "模型单价", exact: true });
  const first = catalog.locator(`tbody[data-price-model="${world.ids[0]}"]`);
  const scopes = page.getByRole("group", { name: "价格分组", exact: true });
  const scope = (id: string) => scopes.locator(`button[data-price-scope="${id}"]`);
  await expect(scope("")).toHaveAttribute("aria-pressed", "true");
  await expect(first.locator(":scope > tr[data-price-group]")).toHaveCount(1);
  await expect(first.locator('[data-label="输入"]')).toHaveText("$4.00");
  await expect(first.locator('[data-label="缓存读"]')).toHaveText("$0.00");
  await first.getByRole("button", { name: "复制模型名", exact: true }).click();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(world.names[0]);
  await expect(first.getByRole("button", { name: "已复制", exact: true })).toHaveText("");
  const openDetails = first.getByRole("button", { name: `查看 ${world.names[0]} 的价格详情`, exact: true });
  await openDetails.click();
  const detail = page.getByRole("dialog", { name: world.names[0], exact: true });
  const detailScopes = detail.getByRole("group", { name: "详情价格分组", exact: true });
  await matrix(detail, "USD", "1");
  await expect(detailScopes.locator(`button[data-price-scope="${world.a}"]`)).toContainText("6.21×");
  await expect(detailScopes.locator(`button[data-price-scope="${world.b}"]`)).toContainText("5.175×");
  await detailScopes.locator(`button[data-price-scope="${world.a}"]`).click();
  await matrix(detail, "CNY", "6.21");
  for (const label of ["输入", "输出", "缓存读", "缓存写", "1 小时缓存写", "推理", "音频输入", "音频输出", "图像输入", "图像输出", "联网搜索"]) {
    await expect(detail.getByRole("rowheader", { name: label, exact: true }).first()).toBeVisible();
  }
  await expect(detail.getByText(/输入 > 200,000 token/).first()).toBeVisible();
  await expect(detail.getByText("batch", { exact: true }).first()).toBeVisible();
  await expect(detail.getByText("元/千次", { exact: true }).first()).toBeVisible();
  await detailScopes.locator(`button[data-price-scope="${world.b}"]`).click();
  await matrix(detail, "CNY", "5.175");
  await page.keyboard.press("Escape");
  await expect(detail).toHaveCount(0);
  await expect(openDetails).toBeFocused();
  await scope(world.a).click();
  await expect(first.locator(`tr[data-price-group="${world.a}"] [data-label="倍率"]`)).toContainText("6.21×");
  await expect(first.locator('[data-label="输入"]')).toHaveText("¥24.84");
  await expect(first.locator('[data-label="倍率"]')).toHaveAttribute("title", "7.2 ×（1 − 13.75%）");
  await scope(world.b).click();
  await expect(first.locator(`tr[data-price-group="${world.b}"] [data-label="倍率"]`)).toContainText("5.175×");
  await expect(first.locator('[data-label="输入"]')).toHaveText("¥20.70");
  await page.getByRole("searchbox", { name: "搜索模型价格", exact: true }).fill(world.names[0]!);
  await expect(catalog.locator("tbody[data-price-model]")).toHaveCount(1);
  await expect(first.locator(":scope > tr[data-price-group]")).toHaveCount(1);
  const pageOrigin = await page.evaluate(() => performance.timeOrigin);
  await page.evaluate((group) => { window.location.hash = `prices?group=${encodeURIComponent(group)}`; }, world.a);
  await expect(scope(world.a)).toHaveAttribute("aria-pressed", "true");
  await expect(first.locator(`:scope > tr[data-price-group="${world.a}"]`)).toBeVisible();
  await expect(first.locator(`:scope > tr[data-price-group="${world.b}"]`)).toHaveCount(0);
  // 新深链生效后仍可本地切组，后续深链不能恢复先前本地选择。
  await scope(world.b).click();
  await expect(scope(world.b)).toHaveAttribute("aria-pressed", "true");
  await scope("").click();
  await expect(scope("")).toHaveAttribute("aria-pressed", "true");
  await expect(first.locator(":scope > tr[data-price-group]")).toHaveCount(1);
  await page.evaluate((group) => { window.location.hash = `prices?group=${encodeURIComponent(group)}`; }, world.b);
  await expect(scope(world.b)).toHaveAttribute("aria-pressed", "true");
  await page.evaluate((group) => { window.location.hash = `prices?group=${encodeURIComponent(group)}`; }, world.a);
  await expect(scope(world.a)).toHaveAttribute("aria-pressed", "true");
  expect(await page.evaluate(() => performance.timeOrigin)).toBe(pageOrigin);
  await page.getByRole("searchbox", { name: "搜索模型价格", exact: true }).fill("");
  await scope("").click();
  await page.getByLabel("模型价格分页每页条数", { exact: true }).selectOption("10");
  await expect(catalog.locator("tbody[data-price-model]")).toHaveCount(10);
  await page.getByRole("button", { name: "下一页", exact: true }).click();
  await expect(catalog.locator("tbody[data-price-model]")).toHaveCount(2);
  await expect(catalog.locator("tr[data-price-group]")).toHaveCount(2);
  await page.getByRole("searchbox", { name: "搜索模型价格", exact: true }).fill(world.names[0]!);
  await expect(catalog.locator("tbody[data-price-model]")).toHaveCount(1);
  await expect(first).toBeVisible();
  await page.getByRole("searchbox", { name: "搜索模型价格", exact: true }).fill("");
  for (const width of [390, 900, 1440]) {
    await page.setViewportSize({ width, height: 720 });
    for (const theme of ["light", "dark"] as const) {
      const switchTheme = page.getByRole("button", { name: theme === "light" ? "切换到浅色" : "切换到深色", exact: true });
      if (await switchTheme.count()) await switchTheme.click();
      await scope("").click();
      await noPageOverflow(page);
      await expect(first.locator('[data-label="输入"]')).toHaveText("$4.00");
      await first.scrollIntoViewIfNeeded();
      await page.screenshot({ path: info.outputPath(`prices-${width}-${theme}-base.png`), fullPage: false });
      await scope(world.b).click();
      await noPageOverflow(page);
      await expect(first.locator('[data-label="倍率"]')).toContainText("5.175×");
      await first.scrollIntoViewIfNeeded();
      await page.screenshot({ path: info.outputPath(`prices-${width}-${theme}-group.png`), fullPage: false });
      expect(await page.evaluate(() => { scrollTo(0, 100); return scrollY; })).toBeGreaterThan(0);
      await page.evaluate(() => scrollTo(0, 0));
      await openDetails.click();
      await expect(detail).toBeVisible();
      await noPageOverflow(page);
      const backgroundScroll = await page.evaluate(() => scrollY);
      for (let tab = 0; tab < 12; tab++) {
        await page.keyboard.press("Tab");
        expect(await detail.evaluate((element) => element.contains(document.activeElement))).toBe(true);
      }
      await page.mouse.wheel(0, 400);
      expect(await page.evaluate(() => scrollY)).toBe(backgroundScroll);
      await detail.getByRole("heading", { name: world.names[0], exact: true }).scrollIntoViewIfNeeded();
      await page.screenshot({ path: info.outputPath(`prices-${width}-${theme}-detail.png`), fullPage: false });
      await page.keyboard.press("Escape");
      await expect(detail).toHaveCount(0);
      await expect(openDetails).toBeFocused();
    }
  }
  expect(runtimeErrors).toEqual([]);
  const visible = await page.locator("body").innerText();
  for (const secret of [`price-a-${world.suffix}`, `price-b-${world.suffix}`, "catalogue-model", "api.example.test", "pricing-catalog-unused-"]) expect(visible).not.toContain(secret);
});

test("报价到期时离线也撤下旧价格，恢复网络后重取同一时点的美元与人民币", async ({ page, context }) => {
  test.setTimeout(180_000);
  const world = await priceWorld();
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(world.email);
  await page.getByLabel("口令", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("link", { name: "模型与价格", exact: true })).toBeVisible();
  const next = new Date(Date.now() + 6_000).toISOString();
  await stack.admin.post("/admin/pricing/baselines", { model_id: world.ids[0], effective_from: next,
    note: "报价到期真实重取验收", card: { default: { base: { input: "9", output: "18" } } } });
  const response = page.waitForResponse((entry) => new URL(entry.url()).pathname === "/api/prices" && entry.request().method() === "GET");
  await page.getByRole("link", { name: "模型与价格", exact: true }).click();
  const quote = await (await response).json();
  expect(Date.parse(quote.valid_until)).toBe(Date.parse(next));
  const first = page.getByRole("table", { name: "模型单价", exact: true }).locator(`tbody[data-price-model="${world.ids[0]}"]`);
  await expect(first.locator('[data-label="输入"]')).toHaveText("$4.00");
  await context.setOffline(true);
  await expect(first).toHaveCount(0, { timeout: 15_000 });
  await expect(page.getByText("$4.00", { exact: true })).toHaveCount(0);
  await context.setOffline(false);
  await page.reload();
  await expect(first.locator('[data-label="输入"]')).toHaveText("$9.00");
  const current = await world.api.get("/api/prices");
  expect(Date.parse(current.as_of)).toBeGreaterThanOrEqual(Date.parse(next));
  expect(current.base_models.find((model: { model_id: string }) => model.model_id === world.ids[0]).prices_by_tier.default.base.find((rate: { dimension: string }) => rate.dimension === "input").price_usd_per_million).toBe("9");
  await page.getByRole("group", { name: "价格分组", exact: true }).locator(`button[data-price-scope="${world.b}"]`).click();
  await expect(first.locator('[data-label="输入"]')).toHaveText("¥46.575");
});
