import { randomUUID } from "node:crypto";
import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { chat, seedTenant } from "./world";

/**
 * 价格 · 租户方案 against the real control plane through the built `dist/`.
 * A tenant's plan is its discount: its prices are its keys' groups', so the
 * page has no per-tenant price to edit. The only stand-in is the upstream the
 * gateway calls.
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

test("租户方案：设置 20% 折扣，下一次调用按折后价计费", async ({ page }) => {
  const world = await seedTenant(stack);
  await openPlan(page, world.tenantName);
  const discount = page.getByRole("region", { name: "折扣" });
  await expect(discount).toContainText("无折扣");

  const form = page.getByRole("form", { name: "新折扣" });
  await form.getByLabel("折扣（%）").fill("20");
  await form.getByRole("button", { name: "保存折扣" }).click();
  await expect(discount.getByTestId("current-discount")).toHaveText("20%");

  // The world's card is 1000 / 2000 USD per million and its group's rate is 1
  // CNY per USD, so one chat is 5 x 1000 + 3 x 2000 = 11 000 µ¥; 20% off is
  // 8 800 µ¥, rounded once.
  const requestId = `req-plan-${randomUUID().slice(0, 8)}`;
  expect(await chat(stack, world.key.plaintext, world.model.name, { requestId })).toBe(200);
  await expect
    .poll(async () => (await stack.admin.get(`/admin/usage/events?request_id=${requestId}`)).items.length, { timeout: 30_000 })
    .toBe(1);
  const event = (await stack.admin.get(`/admin/usage/events?request_id=${requestId}`)).items[0];
  await expect
    .poll(async () => (await stack.admin.get(`/admin/pricing/charges?event_id=${event.event_id}`)).items[0]?.charge_micro_cny, {
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

test("只读运维看到折扣，但没有编辑控件，也看不到专属价", async ({ page }) => {
  const world = await seedTenant(stack);
  await stack.admin.post("/admin/pricing/discounts", {
    tenant_id: world.tenantId,
    effective_from: new Date().toISOString(),
    discount_bps: 1250,
  });
  const viewer = { email: `viewer-plan-${randomUUID().slice(0, 6)}@e2e.local`, password: "viewer-plan-pass-6c1" };
  await stack.admin.post("/admin/operators", { ...viewer, display_name: "只读", role: "viewer" });
  await openPlan(page, world.tenantName, viewer);

  await expect(page.getByTestId("current-discount")).toHaveText("12.5%");
  // The tenant's price is its key's group's: this page has no per-tenant price.
  await expect(page.getByRole("region", { name: "专属价" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /保存/ })).toHaveCount(0);
  await expect(page.getByRole("form")).toHaveCount(0);
});
