import { randomUUID } from "node:crypto";
import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type ApiClient, type Stack } from "./stack";

let stack: Stack;
const PASSWORD = "pricing-model-groups-pass-1112";
const DISCOUNT = 1375;
const RATES = { input: "4", output: "20", cache_read: "0", cache_write: "5", cache_write_1h: "8", reasoning: "9", audio_input: "10", audio_output: "11", image_input: "12", image_output: "13", web_search: "1000" };

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
    await stack.admin.post("/admin/pricing/baselines", { model_id: model.id, effective_from: new Date().toISOString(), note: "租户模型分组报价与完整价卡验收", card: {
      default: { base: RATES, long_context: [{ above_input_tokens: 200000, rates: { ...RATES, input: "8", output: "40" } }] },
      batch: { base: { ...RATES, input: "2", output: "10" }, long_context: [{ above_input_tokens: 200000, rates: { ...RATES, input: "4", output: "20" } }] },
    } });
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

test("模型下同时列多组精确倍率，搜索与分页不拆分组，完整价卡保留", async ({ page, context }) => {
  test.setTimeout(180_000);
  const world = await priceWorld();
  const quote = await world.api.get("/api/prices");
  expect(quote.discount_bps).toBe(DISCOUNT);
  for (const [id, fx, multiplier, input] of [[world.a, "7.2", "6.21", "24.84"], [world.b, "6", "5.175", "20.7"]]) {
    const group = quote.groups.find((item: { id: string }) => item.id === id);
    const model = group.models.find((item: { model_id: string }) => item.model_id === world.ids[0]);
    expect(model).toMatchObject({ cny_per_usd: fx, effective_multiplier: multiplier, input_per_million: input });
  }
  await context.grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(world.email);
  await page.getByLabel("口令", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await page.getByRole("tab", { name: "价格", exact: true }).click();
  await expect(page.getByText("13.75%", { exact: true })).toBeVisible();
  const catalog = page.getByRole("table", { name: "模型单价", exact: true });
  const first = catalog.locator(`tbody[data-price-model="${world.ids[0]}"]`);
  await expect(first.locator(":scope > tr[data-price-group]")).toHaveCount(2);
  await expect(first.locator(`tr[data-price-group="${world.a}"]`)).toContainText("6.21×");
  await expect(first.locator(`tr[data-price-group="${world.b}"]`)).toContainText("5.175×");
  await expect(first.locator(`tr[data-price-group="${world.a}"]`)).toContainText("7.2 ×（1 − 13.75%）");
  await first.getByRole("button", { name: "复制模型名", exact: true }).click();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(world.names[0]);
  await first.getByRole("button", { name: "查看全部价格", exact: true }).click();
  for (const label of ["输入", "输出", "缓存读", "缓存写", "1 小时缓存写", "推理", "音频输入", "音频输出", "图像输入", "图像输出", "联网搜索"]) {
    await expect(first.getByRole("rowheader", { name: label, exact: true }).first()).toBeVisible();
  }
  await expect(first.getByText(/输入 > 200,000 token/).first()).toBeVisible();
  await expect(first.getByText("batch", { exact: true }).first()).toBeVisible();
  await expect(first.getByText("元/千次", { exact: true }).first()).toBeVisible();
  await page.getByRole("searchbox", { name: "搜索模型价格", exact: true }).fill(world.names[0]!);
  await expect(catalog.locator("tbody[data-price-model]")).toHaveCount(1);
  await expect(first.locator(":scope > tr[data-price-group]")).toHaveCount(2);
  const groupFilter = page.getByRole("combobox", { name: "分组筛选", exact: true });
  await groupFilter.selectOption(world.b);
  await expect(first.locator(":scope > tr[data-price-group]")).toHaveCount(1);
  await expect(first.locator(`:scope > tr[data-price-group="${world.b}"]`)).toBeVisible();
  const pageOrigin = await page.evaluate(() => performance.timeOrigin);
  await page.evaluate((group) => { window.location.hash = `prices?group=${encodeURIComponent(group)}`; }, world.a);
  await expect(groupFilter).toHaveValue(world.a);
  await expect(first.locator(":scope > tr[data-price-group]")).toHaveCount(1);
  await expect(first.locator(`:scope > tr[data-price-group="${world.a}"]`)).toBeVisible();
  await expect(first.locator(`:scope > tr[data-price-group="${world.b}"]`)).toHaveCount(0);
  // 新深链生效后仍可本地切组，后续深链不能恢复先前本地选择。
  await groupFilter.selectOption(world.b);
  await expect(groupFilter).toHaveValue(world.b);
  await groupFilter.selectOption("");
  await expect(groupFilter).toHaveValue("");
  await expect(first.locator(":scope > tr[data-price-group]")).toHaveCount(2);
  await page.evaluate((group) => { window.location.hash = `prices?group=${encodeURIComponent(group)}`; }, world.b);
  await expect(groupFilter).toHaveValue(world.b);
  await page.evaluate((group) => { window.location.hash = `prices?group=${encodeURIComponent(group)}`; }, world.a);
  await expect(groupFilter).toHaveValue(world.a);
  expect(await page.evaluate(() => performance.timeOrigin)).toBe(pageOrigin);
  await page.getByRole("searchbox", { name: "搜索模型价格", exact: true }).fill("");
  await groupFilter.selectOption("");
  await page.getByLabel("模型价格分页每页条数", { exact: true }).selectOption("10");
  await expect(catalog.locator("tbody[data-price-model]")).toHaveCount(10);
  await page.getByRole("button", { name: "下一页", exact: true }).click();
  await expect(catalog.locator("tbody[data-price-model]")).toHaveCount(2);
  await expect(catalog.locator("tr[data-price-group]")).toHaveCount(4);
  await page.getByRole("searchbox", { name: "搜索模型价格", exact: true }).fill(world.names[0]!);
  await expect(catalog.locator("tbody[data-price-model]")).toHaveCount(1);
  await expect(first).toBeVisible();
  const visible = await page.locator("body").innerText();
  for (const secret of [`price-a-${world.suffix}`, `price-b-${world.suffix}`, "catalogue-model", "api.example.test", "pricing-catalog-unused-"]) expect(visible).not.toContain(secret);
});
