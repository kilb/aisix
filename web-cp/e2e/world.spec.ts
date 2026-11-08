import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { chat, internalKey, seedTenant, tenantClient } from "./world";

/** The billing world the other specs build on: seeded through the real API, listed by the real Dashboard. */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

test("种子租户出现在租户页", async ({ page }) => {
  const world = await seedTenant(stack);
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令").fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
  await page.locator('[data-tab="tenants"]').click();
  await expect(page.getByRole("row").filter({ hasText: world.tenantName })).toBeVisible();
  expect((await (await tenantClient(stack, world)).get("/api/session")).tenant.id).toBe(world.tenantId);
});

test("内部租户的密钥可调用模型", async () => {
  const world = await seedTenant(stack);
  expect(await chat(stack, await internalKey(stack), world.model.name)).toBe(200);
});
