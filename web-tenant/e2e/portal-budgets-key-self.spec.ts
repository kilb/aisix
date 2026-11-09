// The page's 实际可用 for a key agrees with what the key reads about itself through the gateway (GET /v1/key).
import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { chat, memberKey, seedBillingWorld } from "./world";

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

test("实际可用与 API 密钥自查一致", async ({ page }) => {
  const world = await seedBillingWorld(stack, { creditCny: "1.00", members: ["member"] });
  const m = world.members[0]!;
  const km = await memberKey(stack, world, m);
  const owner = stack.tenant();
  await owner.post("/api/login", world.owner);
  await owner.post("/api/budgets", { scope: "api_key", scope_ref: km.id, kind: "cumulative", limit_cny: "0.05" });
  await expect
    .poll(() => chat(stack, km.plaintext, world.model.name), { message: "the gateway admits the call" })
    .toBe(200);

  // The call is booked a moment after it is answered: the key's own answer through
  // the gateway tells when, and it must agree with the page.
  await expect
    .poll(
      async () => {
        const res = await fetch(`${stack.proxyUrl}/v1/key`, { headers: { authorization: `Bearer ${km.plaintext}` } });
        if (res.status !== 200) return res.status;
        return ((await res.json()) as { data: { effective_remaining: string } }).data.effective_remaining;
      },
      { message: "GET /v1/key reports the booked spend", timeout: 15_000 },
    )
    .toBe("0.039000");

  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(world.owner.email);
  await page.getByLabel("口令", { exact: true }).fill(world.owner.password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await page.getByRole("tab", { name: "预算" }).click();
  const figures = page
    .getByRole("table", { name: "密钥实际可用", exact: true })
    .locator("tbody tr")
    .filter({ hasText: /member-key-/ });
  await expect(figures).toContainText("¥0.039");
  await expect(figures).toContainText("密钥");
});
