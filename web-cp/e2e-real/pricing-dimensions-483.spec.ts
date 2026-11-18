import { randomUUID } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { ApiClient } from "../e2e/stack";
import { chooseSelect } from "../e2e/select";

const session = process.env.PRICING_DIMENSIONS_QA_SESSION!;
const output = process.env.PRICING_DIMENSIONS_QA_OUTPUT!;
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (!state.ready || state.realBackend !== true || state.mockUpstream !== false || !["localhost", "127.0.0.1"].includes(new URL(state.httpUrl).hostname)) throw new Error("Pricing acceptance requires a real isolated loopback control plane");

// 期望价卡来自独立场景：标准档公布长价，priority 只公布短价，12 小时缓存写单独计价。
const CARD = {
  default: {
    base: { input: "1", output: "2", cache_write: "1.25", cache_write_12h: "2.5" },
    long_context: [{ above_input_tokens: 272000, rates: { input: "2", output: "3", cache_write: "2.5", cache_write_12h: "5" } }],
  },
  priority: { base: { input: "2", output: "4", cache_write: "2.5", cache_write_12h: "5" }, unpriced_above_input_tokens: 272000 },
};
interface BaselineRow { model_id: string; current: { card: unknown } | null; upcoming: unknown[] }
interface QuotedRate { dimension: string; price_usd_per_million?: string | null; price_cny_per_million?: string | null }
interface QuotedTier { base: QuotedRate[]; long_context?: { above_input_tokens: number; rates: QuotedRate[] }[]; unpriced_above_input_tokens?: number | null }
interface QuotedModel { model_id: string; prices_by_tier: Record<string, QuotedTier> }
interface Quote { base_models: QuotedModel[]; groups: { id: string; models: QuotedModel[] }[] }

async function fill(group: Locator, entries: [string, string][]): Promise<void> {
  for (const [label, value] of entries) await group.getByLabel(label, { exact: true }).fill(value);
}
async function login(page: Page, origin: string, route: "admin" | "api", credentials: { email: string; password: string }): Promise<void> {
  await page.goto(origin);
  await page.getByLabel("邮箱", { exact: true }).fill(credentials.email);
  await page.getByLabel("口令", { exact: true }).fill(credentials.password);
  const signedIn = page.waitForResponse(response => new URL(response.url()).pathname === `/${route}/login` && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await signedIn).status()).toBe(200);
}
function cacheWrite(rates: QuotedRate[], currency: "USD" | "CNY", expected: string): void {
  const row = rates.find(rate => rate.dimension === "cache_write_12h");
  expect(row, "a distinct 12-hour cache-write quote is present").toBeDefined();
  expect(row?.[currency === "USD" ? "price_usd_per_million" : "price_cny_per_million"]).toBe(expected);
}

// 只写本次新建的分组、账号配置、模型和组织；不发模型请求，不读取或改动既有价格。
test("manual pricing preserves 12-hour cache-write rates and a short-only priority boundary", async ({ page, context }) => {
  const errors: string[] = [];
  const httpErrors: string[] = [];
  page.on("pageerror", error => errors.push(error.message));
  page.on("console", message => { if (message.type() === "error" && !message.text().includes("422")) errors.push(message.text()); });
  page.on("response", response => { if (response.status() >= 500) httpErrors.push(`${response.status()} ${new URL(response.url()).pathname}`); });
  const admin = new ApiClient(state.httpUrl);
  await admin.post("/admin/login", auth.operator);
  const suffix = randomUUID().slice(0, 8);
  const groupName = `qa483-dim-${suffix}`;
  const group = await admin.post("/admin/provider-groups", { name: groupName, display_name: `报价组 ${suffix}`, description: "隔离价格验收，不调用上游", default_cny_per_usd: "2" });
  const account = await admin.post("/admin/resources/provider_keys", { doc: { display_name: `报价配置 ${suffix}`, provider: "openai", api_key: `qa483-unused-${suffix}`, api_base: "https://api.openai.com/v1", groups: [groupName] } });
  const modelName = `qa483-pricing-${suffix}`;
  const model = await admin.post("/admin/resources/models", { doc: { display_name: modelName, provider: "openai", model_name: modelName, provider_key_id: account.id } });

  await login(page, state.adminUrl, "admin", auth.operator);
  await page.goto(state.adminUrl + "/#pricing?section=baselines");
  await expect(page.getByRole("heading", { name: "基准价", exact: true })).toBeVisible();
  await expect(page).toHaveTitle(/\S/);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
  const openModel = async (): Promise<Locator> => {
    await page.getByRole("table", { name: "基准价", exact: true }).getByRole("button", { name: modelName, exact: true }).click();
    const panel = page.getByRole("dialog", { name: `${modelName} 的基准价`, exact: true });
    await expect(panel).toBeVisible();
    await panel.getByRole("button", { name: "新基准价", exact: true }).click();
    const editor = page.getByRole("form", { name: "新基准价", exact: true });
    await expect(editor).toBeVisible();
    await chooseSelect(editor.getByRole("combobox", { name: "方式", exact: true }), "manual");
    return editor;
  };
  const panel = await openModel();
  await fill(panel.getByRole("group", { name: "标准档 基础价", exact: true }), [["输入", "1"], ["输出", "2"], ["缓存写", "1.25"], ["12 小时缓存写", "2.5"]]);
  await panel.getByRole("group", { name: "标准档", exact: true }).getByRole("button", { name: "添加长上下文档", exact: true }).click();
  await fill(panel.getByRole("group", { name: "标准档 长上下文档 1", exact: true }), [["输入超过（tokens）", "272000"], ["输入", "2"], ["输出", "3"], ["缓存写", "2.5"], ["12 小时缓存写", "5"]]);
  await chooseSelect(panel.getByRole("combobox", { name: "要添加的档位", exact: true }), "priority");
  await panel.getByRole("button", { name: "添加档位", exact: true }).click();
  const priority = panel.getByRole("group", { name: "档位 priority", exact: true });
  await priority.getByRole("button", { name: "移除这个长上下文档", exact: true }).click();
  await fill(panel.getByRole("group", { name: "档位 priority 基础价", exact: true }), [["输入", "2"], ["输出", "4"], ["缓存写", "2.5"], ["12 小时缓存写", "5"]]);
  const cap = priority.getByLabel("超过此输入量时无公布价格（tokens）", { exact: true });
  await cap.fill("1");
  await panel.getByLabel("说明", { exact: true }).fill("真实隔离 UI：12 小时缓存写与仅短上下文 priority");
  const rejected = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/pricing/baselines" && response.request().method() === "POST");
  await panel.getByRole("button", { name: "保存", exact: true }).click();
  const rejection = await rejected;
  expect(rejection.status()).toBe(422);
  const rejectionBody = await rejection.json();
  const detail = rejectionBody.details.find((entry: { path: string }) => entry.path === "/card/priority/unpriced_above_input_tokens");
  expect(detail, "refusal names the short-only boundary field").toBeDefined();
  expect(typeof detail.message).toBe("string");
  expect(detail.message.length).toBeGreaterThan(0);
  await expect(cap).toHaveAttribute("aria-invalid", "true");
  await expect(priority.getByText(detail.message, { exact: true })).toBeVisible();
  const unpriced = (await admin.get("/admin/pricing/baselines")).items.find((row: BaselineRow) => row.model_id === model.id) as BaselineRow;
  expect(unpriced, "the new model is listed before any price is stored").toBeDefined();
  expect(unpriced.current).toBeNull();
  expect(unpriced.upcoming).toEqual([]);

  await cap.fill("272000");
  const accepted = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/pricing/baselines" && response.request().method() === "POST");
  await panel.getByRole("button", { name: "保存", exact: true }).click();
  const acceptance = await accepted;
  expect(acceptance.status()).toBe(201);
  expect(acceptance.request().postDataJSON().card).toEqual(CARD);
  const stored = (await admin.get("/admin/pricing/baselines")).items.find((row: BaselineRow) => row.model_id === model.id) as BaselineRow;
  expect(stored.current, "a successful write creates a current baseline").not.toBeNull();
  expect(stored.current?.card).toEqual(CARD);
  await page.getByRole("button", { name: "关闭新基准价", exact: true }).click();
  const published = page.getByRole("region", { name: "现行基准价", exact: true });
  await expect(published).toContainText("基础价 · 输入 ≤ 272000 tokens");
  await expect(published).toContainText("输入 > 272000 tokens · 未公布价格");
  await expect(published).toContainText("12 小时缓存写");
  await page.screenshot({ path: join(output, "admin-pricing-dimensions-desktop.png") });
  await published.getByText("输入 > 272000 tokens · 未公布价格", { exact: true }).filter({ visible: true }).first().scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(output, "admin-pricing-priority-boundary-desktop.png") });
  await page.reload();
  const restored = await openModel();
  await expect(restored.getByRole("group", { name: "标准档 基础价", exact: true }).getByLabel("12 小时缓存写", { exact: true })).toHaveValue("2.5");
  await expect(restored.getByRole("group", { name: "标准档 长上下文档 1", exact: true }).getByLabel("12 小时缓存写", { exact: true })).toHaveValue("5");
  const restoredPriority = restored.getByRole("group", { name: "档位 priority", exact: true });
  await expect(restoredPriority.getByLabel("超过此输入量时无公布价格（tokens）", { exact: true })).toHaveValue("272000");
  await expect(restoredPriority.getByRole("button", { name: "移除这个长上下文档", exact: true })).toHaveCount(0);
  await expect(restored.getByRole("group", { name: "档位 priority 基础价", exact: true }).getByLabel("12 小时缓存写", { exact: true })).toHaveValue("5");
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  await restoredPriority.getByLabel("超过此输入量时无公布价格（tokens）", { exact: true }).scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(output, "admin-pricing-dimensions-form-mobile.png") });
  await page.getByRole("button", { name: "关闭新基准价", exact: true }).click();

  const ownerLogin = { email: `price-owner-${suffix}@qa483.test`, password: `price-owner-local-${suffix}` };
  const tenantApi = new ApiClient(state.httpUrl);
  const signup = await tenantApi.post("/api/signup", { org_name: `价格验收 ${suffix}`, ...ownerLogin });
  await admin.post(`/admin/tenants/${signup.tenant.id}/approve`);
  const tenant = await admin.get(`/admin/tenants/${signup.tenant.id}`);
  await admin.patch(`/admin/tenants/${tenant.id}`, { allowed_model_ids: [model.id], allowed_provider_groups: [groupName] }, tenant.version);
  await tenantApi.post("/api/login", ownerLogin);
  const quote = await tenantApi.get("/api/prices") as Quote;
  const baseModel = quote.base_models.find(entry => entry.model_id === model.id)!;
  expect(baseModel, "the authorized model has a USD quote").toBeDefined();
  expect(baseModel.prices_by_tier.priority?.unpriced_above_input_tokens).toBe(272000);
  cacheWrite(baseModel.prices_by_tier.default!.base, "USD", "2.5");
  cacheWrite(baseModel.prices_by_tier.priority!.base, "USD", "5");
  const usdLong = baseModel.prices_by_tier.default!.long_context;
  expect(usdLong).toHaveLength(1);
  expect(usdLong![0]!.above_input_tokens).toBe(272000);
  cacheWrite(usdLong![0]!.rates, "USD", "5");
  const cnyModel = quote.groups.find(entry => entry.id === group.id)?.models.find(entry => entry.model_id === model.id)!;
  expect(cnyModel, "the assigned group has a CNY quote").toBeDefined();
  expect(cnyModel.prices_by_tier.priority?.unpriced_above_input_tokens).toBe(272000);
  cacheWrite(cnyModel.prices_by_tier.default!.base, "CNY", "5");
  cacheWrite(cnyModel.prices_by_tier.priority!.base, "CNY", "10");
  const cnyLong = cnyModel.prices_by_tier.default!.long_context;
  expect(cnyLong).toHaveLength(1);
  expect(cnyLong![0]!.above_input_tokens).toBe(272000);
  cacheWrite(cnyLong![0]!.rates, "CNY", "10");
  writeFileSync(join(output, "api-pricing-dimensions.json"), JSON.stringify({ model_id: model.id, baseline: stored.current, base_quote: baseModel, cny_quote: cnyModel, rejection: rejectionBody }, null, 2) + "\n");
  const tenantPage = await context.newPage();
  tenantPage.on("pageerror", error => errors.push(error.message));
  tenantPage.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
  await login(tenantPage, state.tenantUrl, "api", ownerLogin);
  await tenantPage.getByRole("link", { name: "模型与价格", exact: true }).click();
  await expect(tenantPage).toHaveURL(/#prices(?:\?|$)/);
  await tenantPage.getByRole("button", { name: `查看 ${modelName} 的价格详情`, exact: true }).click();
  const priceDetail = tenantPage.getByRole("dialog", { name: modelName, exact: true });
  const priorityQuote = priceDetail.getByRole("region", { name: "priority档完整模型单价", exact: true });
  const inputRow = priorityQuote.getByRole("rowheader", { name: "输入", exact: true }).locator("..");
  const cacheRow = priorityQuote.getByRole("rowheader", { name: "12 小时缓存写", exact: true }).locator("..");
  await expect(priorityQuote.getByRole("columnheader").filter({ hasText: "输入 ≤ 272,000 token" })).toHaveCount(1);
  await expect(priorityQuote.getByRole("columnheader").filter({ hasText: "输入 > 272,000 token" })).toContainText("未公布价格");
  await expect(inputRow.getByRole("cell").nth(1)).toHaveText("$2.00");
  await expect(inputRow.getByRole("cell").nth(2)).toHaveText("未提供价格");
  await expect(cacheRow.getByRole("cell").nth(1)).toHaveText("$5.00");
  await expect(cacheRow.getByRole("cell").nth(2)).toHaveText("未提供价格");
  const scopes = priceDetail.getByRole("group", { name: "详情价格分组", exact: true });
  await scopes.getByRole("button", { name: group.display_name, exact: false }).click();
  await expect(inputRow.getByRole("cell").nth(1)).toHaveText("¥4.00");
  await expect(inputRow.getByRole("cell").nth(2)).toHaveText("未提供价格");
  await expect(cacheRow.getByRole("cell").nth(1)).toHaveText("¥10.00");
  await expect(cacheRow.getByRole("cell").nth(2)).toHaveText("未提供价格");
  await tenantPage.screenshot({ path: join(output, "tenant-pricing-dimensions-desktop.png") });
  await tenantPage.setViewportSize({ width: 390, height: 844 });
  expect(await tenantPage.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
  await cacheRow.getByRole("rowheader").scrollIntoViewIfNeeded();
  await tenantPage.screenshot({ path: join(output, "tenant-pricing-dimensions-mobile.png") });
  expect(httpErrors).toEqual([]);
  expect(errors).toEqual([]);
});
