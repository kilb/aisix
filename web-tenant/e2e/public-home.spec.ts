import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

const GROUP = "public-home-e2e";
const PRICED_MODEL = "public-home-priced-model";
const UNPRICED_MODEL = "public-home-unpriced-model";
const UNIT_PRICE = {
  input: /^¥?\s*35\.10?$/,
  output: /^¥?\s*146\.88$/,
  cacheRead: /^¥?\s*0\.90?$/,
};

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
  await stack.admin.createGroup(GROUP, "公开价格测试分组", "7.2");
  await stack.admin.put("/admin/settings/tenant-defaults", { default_provider_groups: [GROUP] });
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: "public-home-test-account",
      provider: "openai",
      api_key: "sk-public-home-test-account",
      api_base: "https://api.example.test/v1",
      groups: [GROUP],
    },
  });
  const priced = await stack.admin.post("/admin/public-models", { name: PRICED_MODEL });
  const unpriced = await stack.admin.post("/admin/public-models", { name: UNPRICED_MODEL });
  await stack.admin.put(`/admin/provider-keys/${account.id}/channel-models/${priced.id}`, {});
  await stack.admin.put(`/admin/provider-keys/${account.id}/channel-models/${unpriced.id}`, {});
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: priced.id,
    effective_from: new Date(Date.now() - 4_000).toISOString(),
    card: { default: { base: { input: "4.875", output: "20.4", cache_read: "0.125" } } },
    note: "公开首页的测试价目卡",
  });
});

test.afterAll(async () => {
  await stack?.stop();
});

function modelRow(page: Page, model: string) {
  return page.getByRole("table", { name: "公开模型标准价" }).getByRole("row")
    .filter({ has: page.getByText(model, { exact: true }) });
}

test("访客可以查看人民币标准价并按模型名称搜索", async ({ page }) => {
  await page.goto(stack.previewUrl);
  await expect(page.getByRole("heading", { name: "模型价格", exact: true })).toBeVisible();
  await expect(page).toHaveURL(`${stack.previewUrl}/`);
  await expect(page.getByLabel("口令", { exact: true })).toHaveCount(0);
  await expect(page.getByText(/人民币元\s*\/\s*百万 Token/)).toBeVisible();

  const priced = modelRow(page, PRICED_MODEL);
  const unpriced = modelRow(page, UNPRICED_MODEL);
  // 每百万 Token 的美元基准价乘以默认分组的 7.2 人民币兑换率。
  await expect(priced.getByRole("cell", { name: UNIT_PRICE.input })).toBeVisible();
  await expect(priced.getByRole("cell", { name: UNIT_PRICE.output })).toBeVisible();
  await expect(priced.getByRole("cell", { name: UNIT_PRICE.cacheRead })).toBeVisible();
  await expect(unpriced).toBeVisible();
  await expect(unpriced.getByRole("cell", { name: "未提供价格", exact: true })).toHaveCount(3);

  const details = page.getByRole("table", { name: "完整模型单价" });
  const toggle = priced.getByRole("button", { name: PRICED_MODEL, exact: true });
  await expect(details).toHaveCount(0);
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
  await expect(details.getByRole("cell", { name: UNIT_PRICE.output })).toBeVisible();
  await toggle.click();
  await expect(details).toHaveCount(0);

  const search = page.getByRole("searchbox", { name: "搜索模型" });
  await search.fill(PRICED_MODEL);
  await expect(priced).toBeVisible();
  await expect(unpriced).toHaveCount(0);
  await search.fill("no-matching-model-in-public-prices");
  await expect(priced).toHaveCount(0);
  await expect(page.getByText("没有找到匹配的模型。", { exact: true })).toBeVisible();
  await search.clear();
  await expect(unpriced).toBeVisible();
});

test("首页链接可进入独立登录和注册页面并返回首页", async ({ page }) => {
  await page.goto(stack.previewUrl);
  const banner = page.getByRole("banner");
  await banner.getByRole("link", { name: "登录", exact: true }).click();
  await expect(page).toHaveURL(`${stack.previewUrl}/login`);
  const login = page.getByRole("form", { name: "登录", exact: true });
  await expect(login.getByLabel("口令", { exact: true })).toBeVisible();

  await page.getByRole("link", { name: "返回首页", exact: true }).click();
  await expect(page).toHaveURL(`${stack.previewUrl}/`);
  await banner.getByRole("link", { name: "注册", exact: true }).click();
  await expect(page).toHaveURL(`${stack.previewUrl}/signup`);
  const signup = page.getByRole("form", { name: "注册组织", exact: true });
  await expect(signup.getByLabel("组织名称", { exact: true })).toBeVisible();

  await page.getByRole("link", { name: "返回首页", exact: true }).click();
  await expect(page).toHaveURL(`${stack.previewUrl}/`);
  await expect(modelRow(page, PRICED_MODEL)).toBeVisible();
});

test("窄屏首屏直接展示价格且可横向完整比较", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 667 });
  await page.goto(stack.previewUrl);
  const row = modelRow(page, PRICED_MODEL);
  await expect(row.getByRole("cell", { name: UNIT_PRICE.input })).toBeInViewport({ ratio: 0.95 });
  await expect(row.getByRole("cell", { name: UNIT_PRICE.output })).toBeInViewport({ ratio: 0.95 });
  await expect(row.getByRole("cell", { name: UNIT_PRICE.cacheRead })).toBeInViewport({ ratio: 0.95 });
  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(overflow).toBeLessThanOrEqual(1);

  await page.getByRole("link", { name: "查看模型价格", exact: true }).click();
  await expect(page).toHaveURL(`${stack.previewUrl}/#pricing`);
});
