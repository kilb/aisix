import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { CALL_MICRO_USD, chat, seedBillingWorld } from "./world";

/** The billing world the other specs build on: seeded through the real API, usable through the real portal and gateway. */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

test("种子组织的所有者能登录并看到组织页", async ({ page }) => {
  const world = await seedBillingWorld(stack);
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(world.owner.email);
  await page.getByLabel("口令", { exact: true }).fill(world.owner.password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("banner")).toContainText(world.tenantName);
  await expect(page.getByRole("tab")).toHaveText(["组织", "密钥", "成员", "用量", "预算", "账户"]);
});

test("种子组织的密钥能调用模型，一次调用扣 11 000 µ$，成员各有各的角色", async () => {
  const world = await seedBillingWorld(stack, { members: ["member", "billing"], unpricedModel: true });
  expect(world.members.map((m) => m.role)).toEqual(["member", "billing"]);
  expect(world.laterModel?.name).toBe("chat-ui-later");
  const before = (await stack.admin.get(`/admin/tenants/${world.tenantId}/billing`)).balance_micro_usd;
  expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await expect
    .poll(async () => (await stack.admin.get(`/admin/tenants/${world.tenantId}/billing`)).balance_micro_usd, {
      message: "the call is debited from the balance",
    })
    .toBe(before - CALL_MICRO_USD);
});
