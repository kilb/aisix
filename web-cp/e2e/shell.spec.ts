import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

/**
 * The Dashboard's shell: the login gate, the tabs an operator's role
 * shows, and signing out. Against the real control plane through the built
 * `dist/`.
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
  await page.getByLabel("口令").fill(password);
  await page.getByRole("button", { name: "登录" }).click();
}

test("未登录时不渲染任何数据", async ({ page }) => {
  await page.goto(stack.previewUrl);
  await expect(page.getByLabel("邮箱")).toBeVisible();
  await expect(page.locator("[data-tab]")).toHaveCount(0);
});

test("口令错误时报错且不进入", async ({ page }) => {
  await login(page, stack.operator.email, "wrong-password-123");
  await expect(page.getByRole("alert")).toContainText("邮箱或口令不正确");
  await expect(page.locator("[data-tab]")).toHaveCount(0);
});

test("管理员登录后看到全部页签，登出后回到登录框", async ({ page }) => {
  await login(page, stack.operator.email, stack.operator.password);
  await expect(page.locator("[data-tab]")).toHaveText(["概览", "网关", "上游账号", "模型与路由", "租户", "密钥", "缓存策略", "全部资源", "运维账号", "审计日志"]);
  await page.getByRole("button", { name: "登出" }).click();
  await expect(page.getByLabel("邮箱")).toBeVisible();
});

test("刷新页面后仍保持登录，登出后刷新仍是登录框", async ({ page }) => {
  await login(page, stack.operator.email, stack.operator.password);
  await expect(page.locator("[data-tab]")).toHaveCount(10);
  await page.reload();
  await expect(page.locator("[data-tab]")).toHaveCount(10);
  await page.getByRole("button", { name: "登出" }).click();
  await expect(page.getByLabel("邮箱")).toBeVisible();
  await page.reload();
  await expect(page.getByLabel("邮箱")).toBeVisible();
  await expect(page.locator("[data-tab]")).toHaveCount(0);
});

test("只读账号看得到除运维账号外的全部页签", async ({ page }) => {
  const email = "viewer-shell-e2e@e2e.local";
  const password = "viewer-shell-pass-6b2";
  await stack.admin.post("/admin/operators", { email, role: "viewer", password });
  await login(page, email, password);
  await expect(page.locator("[data-tab]")).toHaveText(["概览", "网关", "上游账号", "模型与路由", "租户", "密钥", "缓存策略", "全部资源", "审计日志"]);
});
