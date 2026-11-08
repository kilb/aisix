import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";
import { chat, seedTenant } from "./world";

/**
 * 价格 · 计价状态 and the 概览 card: unpriced calls by reason. Real gateway and
 * control plane; the upstream stand-in is the only fake. Expected counts come
 * from the scenario (two calls to a model with no price).
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

async function login(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令").fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
}

test("未定价模型上的调用出现在计价状态里，按原因计数，概览页同时提示", async ({ page }) => {
  const world = await seedTenant(stack, { unpricedModel: true });
  expect(await chat(stack, world.key.plaintext, world.laterModel!.name)).toBe(200);
  expect(await chat(stack, world.key.plaintext, world.laterModel!.name)).toBe(200);
  expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await expect
    .poll(async () => {
      const r = (await stack.admin.get(`/admin/pricing/charges?tenant_id=${world.tenantId}`)) as { items: Json[] };
      return r.items.length;
    }, { timeout: 60_000 })
    .toBe(3);

  await login(page);
  // 概览: the card shows the count and links to the status page.
  const card = page.getByRole("region", { name: "计价异常" });
  await expect(card.locator(".entry", { hasText: "未计价调用" }).locator(".val")).toHaveText("2");
  await expect(card.locator(".entry", { hasText: "不可用的价格版本" }).locator(".val")).toHaveText("0");
  await card.getByRole("button", { name: /计价状态/ }).click();

  // 计价状态 opens directly, by reason.
  await expect(page.getByRole("heading", { name: "计价状态" })).toBeVisible();
  const table = page.getByRole("table", { name: "未计价的调用" });
  await expect(table.getByRole("columnheader")).toContainText(["原因", "次数"]);
  await expect(table.getByRole("row", { name: /^未定价/ }).getByRole("cell").nth(1)).toHaveText("2");
  await expect(table.getByRole("row", { name: /^合计/ })).toContainText("2");
  await expect(page.getByText("价格版本", { exact: false }).first()).toBeVisible();
  await expect(page.getByText("没有不可用的价格版本。")).toBeVisible();
});
