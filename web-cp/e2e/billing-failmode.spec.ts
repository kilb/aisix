import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { seedTenant } from "./world";

/**
 * 计费 · 预算失败模式: the environment-wide note, on a tenant's panel and on
 * 全部资源 for `budget_policy`. Real control plane; both read the stored document.
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

const SCOPE = "这个设置对整个环境的所有租户生效，不能按租户单独设置。";
const CLOSED_CONSEQUENCE = "所以 closed 在宽限时间过后会拒绝所有客户的请求。";

test("预算失败模式的说明写明对整个环境生效", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditCny: "10.00" });
  await login(page);
  await page.locator('[data-tab="billing"]').click();
  await page
    .getByRole("table", { name: "账户列表" })
    .locator("tbody tr")
    .filter({ hasText: world.tenantName })
    .getByRole("button", { name: world.tenantName })
    .click();
  const note = page.getByRole("dialog").getByRole("region", { name: "预算失败模式", exact: true });

  // No stored policy: the gateway behaves as sticky.
  await expect(note).toContainText("预算失败模式：sticky: 保持最后状态");
  await expect(note).toContainText(SCOPE);
  await expect(note).toContainText(CLOSED_CONSEQUENCE);

  // 去修改 opens 全部资源 on budget_policy, which shows the same note.
  await note.getByRole("button", { name: "去修改" }).click();
  await expect(page.getByLabel("资源类型")).toHaveValue("budget_policy");
  const generic = page.getByRole("note", { name: "预算失败模式" });
  await expect(generic).toContainText("预算失败模式：sticky: 保持最后状态");
  await expect(generic).toContainText(SCOPE);

  // A stored closed policy is what both places then read.
  await stack.admin.post("/admin/resources/budget_policy", { doc: { fail_mode: "closed", closed_grace_seconds: 60 } });
  await page.getByRole("button", { name: "刷新" }).click();
  await page.locator('[data-tab="billing"]').click();
  await page
    .getByRole("table", { name: "账户列表" })
    .locator("tbody tr")
    .filter({ hasText: world.tenantName })
    .getByRole("button", { name: world.tenantName })
    .click();
  await expect(page.getByRole("dialog").getByRole("region", { name: "预算失败模式", exact: true })).toContainText(
    "预算失败模式：closed: 关闭",
  );
});
