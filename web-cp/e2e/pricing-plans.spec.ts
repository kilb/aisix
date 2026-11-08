import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { chat, seedTenant } from "./world";

/**
 * 价格 · 租户方案 against the real control plane through the built `dist/`.
 * The only stand-in is the upstream the gateway calls.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

/** Opens 价格 · 租户方案 with `tenantName` chosen. */
async function openPlan(page: Page, tenantName: string, who = stack.operator): Promise<void> {
  await login(page, who.email, who.password);
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("button", { name: "租户方案" }).click();
  await page.getByRole("combobox", { name: "租户" }).selectOption({ label: tenantName });
  await expect(page.getByRole("region", { name: "折扣" })).toBeVisible();
}

function overrideRow(page: Page, model: string): Locator {
  return page.getByRole("table", { name: "专属价" }).getByRole("row").filter({ hasText: model });
}

test("租户方案：设置 20% 折扣与一个专属价，结束专属价后显示按平台价", async ({ page }) => {
  const world = await seedTenant(stack);
  await openPlan(page, world.tenantName);
  const discount = page.getByRole("region", { name: "折扣" });
  await expect(discount).toContainText("无折扣");

  const form = page.getByRole("form", { name: "新折扣" });
  await form.getByLabel("折扣（%）").fill("20");
  await form.getByRole("button", { name: "保存折扣" }).click();
  await expect(discount.getByTestId("current-discount")).toHaveText("20%");

  const add = page.getByRole("form", { name: "添加专属价" });
  await add.getByLabel("模型").selectOption({ label: world.model.name });
  await add.getByLabel("输入", { exact: true }).fill("500");
  await add.getByLabel("输出", { exact: true }).fill("900");
  await add.getByRole("button", { name: "保存专属价" }).click();
  const row = overrideRow(page, world.model.name);
  await expect(row).toContainText("输入 500 输出 900");
  await expect(row).not.toContainText("已结束");

  await row.getByRole("button", { name: "结束专属价" }).click();
  await row.getByRole("button", { name: "确认结束专属价" }).click();
  await expect(row).toContainText("已结束，按平台价");
  await expect(row).not.toContainText("输入 500");
  await expect(row.getByRole("button", { name: "结束专属价" })).toHaveCount(0);

  // The platform price of the world's model is 1000 / 2000 per million; one chat is 5 + 3 tokens.
  const requestId = `req-plan-${randomUUID().slice(0, 8)}`;
  expect(await chat(stack, world.key.plaintext, world.model.name, { requestId })).toBe(200);
  await expect
    .poll(async () => (await stack.admin.get(`/admin/usage/events?request_id=${requestId}`)).items.length, { timeout: 30_000 })
    .toBe(1);
  const event = (await stack.admin.get(`/admin/usage/events?request_id=${requestId}`)).items[0];
  await expect
    .poll(async () => (await stack.admin.get(`/admin/pricing/charges?event_id=${event.event_id}`)).items[0]?.charge_micro_usd, {
      timeout: 30_000,
    })
    .toBe(8_800);
});

test("折扣超出 0 到 100 时被拒绝并说明原因，什么也没保存", async ({ page }) => {
  const world = await seedTenant(stack);
  await openPlan(page, world.tenantName);
  const form = page.getByRole("form", { name: "新折扣" });
  const percent = form.getByLabel("折扣（%）");
  await percent.fill("101");
  await form.getByRole("button", { name: "保存折扣" }).click();
  await expect(form.getByText("折扣应是 0 到 100 的百分数")).toBeVisible();
  await expect(percent).toHaveAttribute("aria-invalid", "true");
  await expect(percent).toBeFocused();
  await expect(page.getByTestId("current-discount")).toHaveText("无折扣");
  expect((await stack.admin.get(`/admin/pricing/discounts?tenant_id=${world.tenantId}`)).items).toHaveLength(0);
});

test("只读运维看不到折扣与专属价的编辑控件", async ({ page }) => {
  const world = await seedTenant(stack);
  await stack.admin.post("/admin/pricing/discounts", {
    tenant_id: world.tenantId,
    effective_from: new Date().toISOString(),
    discount_bps: 1250,
  });
  await stack.admin.post("/admin/pricing/sell", {
    model_id: world.model.id,
    tenant_id: world.tenantId,
    effective_from: new Date().toISOString(),
    card: { default: { base: { input: "300", output: "600" } } },
  });
  const viewer = { email: `viewer-plan-${randomUUID().slice(0, 6)}@e2e.local`, password: "viewer-plan-pass-6c1" };
  await stack.admin.post("/admin/operators", { ...viewer, display_name: "只读", role: "viewer" });
  await openPlan(page, world.tenantName, viewer);

  await expect(page.getByTestId("current-discount")).toHaveText("12.5%");
  await expect(overrideRow(page, world.model.name)).toContainText("输入 300 输出 600");
  await expect(page.getByRole("button", { name: /保存/ })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "结束专属价" })).toHaveCount(0);
  await expect(page.getByRole("form")).toHaveCount(0);
});
