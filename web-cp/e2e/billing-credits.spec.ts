import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { seedTenant, tenantClient, type BillingWorld } from "./world";

/**
 * 计费 · 直接入账: an admin credits a tenant from its panel. Real control
 * plane; the tenant's own ledger is read through its own session.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

async function login(page: Page, email = stack.operator.email, password = stack.operator.password): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令").fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await page.locator('[data-tab="billing"]').click();
}

async function openPanel(page: Page, world: BillingWorld): Promise<Locator> {
  await page
    .getByRole("table", { name: "账户列表" })
    .locator("tbody tr")
    .filter({ hasText: world.tenantName })
    .getByRole("button", { name: world.tenantName })
    .click();
  return page.getByRole("dialog");
}

const credit = (panel: Locator): Locator => panel.getByRole("region", { name: "直接入账", exact: true });

async function topups(world: BillingWorld): Promise<Record<string, any>[]> {
  const tenant = await tenantClient(stack, world);
  return (await tenant.get("/api/billing/topups")).items;
}

test("直接入账需要原因与二次确认，租户看到这笔入账与原因", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditUsd: "0" });
  await login(page);
  const panel = await openPanel(page, world);
  const form = credit(panel);

  // No 原因: nothing is sent, the field is refused and holds focus.
  await form.getByLabel("金额（美元）").fill("0.05");
  await form.getByRole("button", { name: "入账", exact: true }).click();
  await expect(form.getByText("请填写入账原因")).toBeVisible();
  await expect(form.getByLabel("原因")).toBeFocused();
  await expect(form.getByRole("button", { name: "确认入账" })).toHaveCount(0);
  expect(await topups(world)).toHaveLength(0);

  // A malformed amount is refused too.
  await form.getByLabel("金额（美元）").fill("0.001");
  await form.getByLabel("原因").fill("线下收款-ui-7c1");
  await form.getByRole("button", { name: "入账", exact: true }).click();
  await expect(form.getByText("请输入金额，最多两位小数")).toBeVisible();
  await expect(form.getByLabel("金额（美元）")).toBeFocused();
  expect(await topups(world)).toHaveLength(0);

  await form.getByLabel("金额（美元）").fill("0.05");
  await form.getByLabel("凭证号").fill("REF-ui-7c1");
  await form.getByRole("button", { name: "入账", exact: true }).click();
  await expect(form.getByText("将立即计入该租户余额")).toBeVisible();
  expect(await topups(world)).toHaveLength(0);
  await form.getByRole("button", { name: "确认入账" }).click();

  await expect(page.getByText("已入账", { exact: true })).toBeVisible();
  await expect(panel.getByRole("region", { name: "余额", exact: true }).locator("dd.money").first()).toHaveText("$0.05");
  await expect(form.getByLabel("金额（美元）")).toHaveValue("");
  await expect(form.getByLabel("原因")).toHaveValue("");

  const items = await topups(world);
  expect(items).toHaveLength(1);
  const item = items[0]!;
  expect(item.origin).toBe("operator");
  expect(item.credit_reason).toBe("线下收款-ui-7c1");
  expect(item.reference).toBe("REF-ui-7c1");
  expect(item.amount_micro_usd).toBe(50_000);
});

test("服务端拒绝的原因显示在原因字段上，表单保持原样", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditUsd: "0" });
  await login(page);
  const panel = await openPanel(page, world);
  const form = credit(panel);
  await form.getByLabel("金额（美元）").fill("0.05");
  // A control character the browser input keeps (a tab) is refused by the backend at /reason.
  await form.getByLabel("原因").evaluate((el: HTMLInputElement) => {
    const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")!.set!;
    set.call(el, "a\u0007b");
    el.dispatchEvent(new Event("input", { bubbles: true }));
  });
  await form.getByRole("button", { name: "入账", exact: true }).click();
  await form.getByRole("button", { name: "确认入账" }).click();
  await expect(form.getByText("不能包含控制字符")).toBeVisible();
  await expect(form.getByLabel("金额（美元）")).toHaveValue("0.05");
  expect(await topups(world)).toHaveLength(0);
});

test("连点确认入账只入账一次", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditUsd: "0" });
  await login(page);
  const panel = await openPanel(page, world);
  const form = credit(panel);
  await form.getByLabel("金额（美元）").fill("0.07");
  await form.getByLabel("原因").fill("连点-ui-7c2");
  await form.getByRole("button", { name: "入账", exact: true }).click();
  await form.getByRole("button", { name: "确认入账" }).dblclick();
  await expect(page.getByText("已入账", { exact: true })).toBeVisible();
  await expect(panel.getByRole("region", { name: "余额", exact: true }).locator("dd.money").first()).toHaveText("$0.07");
  await page.waitForTimeout(1000);
  const items = await topups(world);
  expect(items).toHaveLength(1);
  expect(items[0]!.amount_micro_usd).toBe(70_000);
});

test("非管理员看不到直接入账", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditUsd: "0" });
  for (const role of ["operator", "viewer"] as const) {
    const who = { email: `${role}-credits@e2e.local`, password: `${role}-credits-pass-4k9` };
    await stack.admin.post("/admin/operators", { ...who, role });
    await login(page, who.email, who.password);
    const panel = await openPanel(page, world);
    await expect(panel.getByRole("region", { name: "余额", exact: true })).toBeVisible();
    await expect(credit(panel)).toHaveCount(0);
    await page.context().clearCookies();
  }
});

test("入账请求没有得到回答时，表单锁定，关闭面板后重开仍然锁定", async ({ page, context }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditUsd: "0" });
  await login(page);
  let panel = await openPanel(page, world);
  let form = credit(panel);
  await form.getByLabel("金额（美元）").fill("0.06");
  await form.getByLabel("原因").fill("未知-ui-7c3");
  await form.getByRole("button", { name: "入账", exact: true }).click();
  // The browser loses its connection: the request never gets an answer.
  await context.setOffline(true);
  await form.getByRole("button", { name: "确认入账" }).click();
  await expect(form.getByRole("alert").filter({ hasText: "入账结果未知" })).toBeVisible();
  await expect(form.getByRole("button", { name: /^(确认)?入账$/ })).toHaveCount(0);
  await context.setOffline(false);

  // Closing the panel must not hand a fresh, unlocked form back.
  await panel.getByRole("button", { name: "关闭" }).click();
  await expect(page.getByRole("dialog")).toHaveCount(0);
  panel = await openPanel(page, world);
  form = credit(panel);
  await expect(form.getByRole("alert").filter({ hasText: "入账结果未知" })).toBeVisible();
  await expect(form.getByRole("button", { name: /^(确认)?入账$/ })).toHaveCount(0);
  // The note says how to tell a platform credit from an approved tenant top-up.
  await expect(form.getByRole("alert")).toContainText("平台入账");
  expect(await topups(world)).toHaveLength(0);

  // Once the operator has checked, the form starts over and credits again.
  await form.getByRole("button", { name: "我已确认，重新填写" }).click();
  await form.getByLabel("金额（美元）").fill("0.06");
  await form.getByLabel("原因").fill("未知-ui-7c3");
  await form.getByRole("button", { name: "入账", exact: true }).click();
  await form.getByRole("button", { name: "确认入账" }).click();
  await expect(page.getByText("已入账", { exact: true })).toBeVisible();
  expect(await topups(world)).toHaveLength(1);
});
