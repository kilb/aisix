import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

/**
 * 价格 · 成本价 against the real control plane through the built `dist/`.
 * Provider keys are created through the real `/admin`; nothing is stubbed.
 */

let stack: Stack;
let accountName: string;
let accountId: string;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  accountName = `acct-cost-${randomUUID().slice(0, 6)}`;
  const key = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: accountName,
      provider: "openai",
      api_key: "sk-acct-cost-upstream",
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  accountId = key.id;
});

test.afterAll(async () => {
  await stack?.stop();
});

async function openCost(page: Page, who = stack.operator): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("button", { name: "成本价" }).click();
  await expect(page.getByRole("search", { name: "筛选成本价" })).toBeVisible();
}

function historyRows(page: Page): Locator {
  return page.getByRole("table", { name: "成本价历史" }).locator("tbody tr");
}

test("成本价按上游模型设置，历史可查", async ({ page }) => {
  const upstreamModel = `upstream-mdl-ui-${randomUUID().slice(0, 4)}`;
  await openCost(page);

  const form = page.getByRole("form", { name: "新成本价" });
  await form.getByLabel("上游", { exact: true }).fill("openai");
  await form.getByLabel("上游模型", { exact: true }).fill(upstreamModel);
  await form.getByLabel("输入", { exact: true }).fill("0.5");
  await form.getByLabel("输出", { exact: true }).fill("1");
  await form.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  // Narrow the history to this model, so only what this test wrote is read.
  const filters = page.getByRole("search", { name: "筛选成本价" });
  await filters.getByLabel("上游模型", { exact: true }).fill(upstreamModel);
  await expect(historyRows(page)).toHaveCount(1);
  const first = historyRows(page).first();
  await expect(first).toContainText("openai");
  await expect(first).toContainText(upstreamModel);
  await expect(first).toContainText("全部");
  await expect(first.locator("td").nth(5)).toHaveText("0.5");
  await expect(first.locator("td").nth(6)).toHaveText("1");

  // The backend holds the same, with no account.
  const stored = await stack.admin.get(`/admin/pricing/cost?upstream_model=${upstreamModel}`);
  expect(stored.items).toHaveLength(1);
  expect(stored.items[0].provider_key_id).toBeNull();
});

test("经某个上游账号的成本价，按账号筛选，新记录排在最前", async ({ page }) => {
  const upstreamModel = `upstream-mdl-acct-${randomUUID().slice(0, 4)}`;
  await openCost(page);
  const form = page.getByRole("form", { name: "新成本价" });

  async function add(input: string, output: string, withAccount: boolean): Promise<void> {
    await form.getByLabel("上游", { exact: true }).fill("openai");
    await form.getByLabel("上游模型", { exact: true }).fill(upstreamModel);
    await form.getByLabel(/^上游账号/).selectOption(withAccount ? accountId : "");
    await form.getByLabel("输入", { exact: true }).fill(input);
    await form.getByLabel("输出", { exact: true }).fill(output);
    await form.getByRole("button", { name: "保存" }).click();
    await expect(page.getByText("已保存").first()).toBeVisible();
  }
  await add("1", "2", false);
  await add("3", "4", true);

  const filters = page.getByRole("search", { name: "筛选成本价" });
  await filters.getByLabel("上游模型", { exact: true }).fill(upstreamModel);
  await expect(historyRows(page)).toHaveCount(2);
  // Newest first: the one through the account.
  await expect(historyRows(page).first()).toContainText(accountName);
  await expect(historyRows(page).first().locator("td").nth(5)).toHaveText("3");
  await expect(historyRows(page).nth(1)).toContainText("全部");

  await filters.getByLabel(/^上游账号/).selectOption(accountId);
  await expect(historyRows(page)).toHaveCount(1);
  await expect(historyRows(page).first()).toContainText(accountName);
});

test("上游模型为空时保存被拒，焦点回到该字段", async ({ page }) => {
  await openCost(page);
  const form = page.getByRole("form", { name: "新成本价" });
  await form.getByLabel("上游", { exact: true }).fill("openai");
  await form.getByLabel("输入", { exact: true }).fill("1");
  await form.getByLabel("输出", { exact: true }).fill("2");
  await form.getByRole("button", { name: "保存" }).click();
  await expect(form.getByText("上游模型不能为空")).toBeVisible();
  await expect(form.getByLabel("上游模型", { exact: true })).toBeFocused();
});

test("只读运维看不到成本价的新建按钮", async ({ page }) => {
  const upstreamModel = `upstream-mdl-ro-${randomUUID().slice(0, 4)}`;
  const viewer = { email: `viewer-cost-${randomUUID().slice(0, 6)}@e2e.local`, password: "viewer-cost-pass-3c8" };
  await stack.admin.post("/admin/operators", { ...viewer, display_name: "只读", role: "viewer" });
  await stack.admin.post("/admin/pricing/cost", {
    upstream_provider: "openai",
    upstream_model: upstreamModel,
    effective_from: new Date().toISOString(),
    card: { default: { base: { input: "2", output: "4" } } },
  });
  await openCost(page, viewer);
  await page.getByRole("search", { name: "筛选成本价" }).getByLabel("上游模型", { exact: true }).fill(upstreamModel);
  await expect(historyRows(page)).toHaveCount(1);
  await expect(historyRows(page).first()).toContainText(upstreamModel);
  await expect(page.getByRole("button", { name: "保存" })).toHaveCount(0);
  await expect(page.getByRole("form", { name: "新成本价" })).toHaveCount(0);
});
