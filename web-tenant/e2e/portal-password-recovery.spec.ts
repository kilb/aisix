import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});

test.afterAll(async () => { await stack?.stop(); });

test("窄屏登录页只在管理员启用口令找回时提供入口", async ({ page }) => {
  await page.setViewportSize({ width: 375, height: 812 });
  const login = new URL("/login", stack.previewUrl).href;
  await page.goto(login);
  await expect(page.getByRole("button", { name: "登录", exact: true })).toBeEnabled();
  await expect(page.getByRole("button", { name: "忘记口令？" })).toHaveCount(0);

  const { version } = await stack.admin.get("/admin/settings/authentication");
  const settings = {
    version, portal_origin: "https://portal.example.com",
    email: { enabled: true, endpoint: "https://relay.invalid/emails", from: "sender@example.com", api_key: "test-relay-key" },
    turnstile: { enabled: false, site_key: "" },
    passkey: { enabled: false },
    github: { enabled: false, client_id: "" },
    google: { enabled: false, client_id: "" },
  };
  const emailOnly = await stack.admin.put("/admin/settings/authentication", settings);
  await page.goto(login);
  await expect(page.getByRole("button", { name: "登录", exact: true })).toBeEnabled();
  await expect(page.getByRole("button", { name: "忘记口令？" })).toHaveCount(0);
  await stack.admin.put("/admin/settings/authentication", {
    ...settings, version: emailOnly.version, password_recovery_enabled: true,
  });

  await page.goto(login);
  const address = "visitor@example.com";
  await page.getByLabel("邮箱", { exact: true }).fill(address);
  await page.getByRole("button", { name: "忘记口令？" }).click();
  await expect(page.getByRole("heading", { name: "找回口令" })).toBeVisible();
  await expect(page.getByLabel("邮箱", { exact: true })).toHaveValue(address);

  await page.getByRole("button", { name: "发送验证码", exact: true }).click();
  await expect(page.getByRole("status")).toHaveText("如果该邮箱可以找回口令，验证码将发送到你的收件箱。");
  await expect(page.getByRole("button", { name: "重新发送", exact: true })).toBeDisabled();
  await page.getByLabel("邮箱验证码", { exact: true }).fill("123456");
  await page.getByLabel("新口令", { exact: true }).fill("a new privacy test password");
  await page.getByRole("button", { name: "重置口令", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("验证码错误、已失效或已过期");

  const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(overflow).toBeLessThanOrEqual(1);
  await page.getByRole("button", { name: "返回登录", exact: true }).click();
  await expect(page.getByRole("form", { name: "登录", exact: true })).toBeVisible();
  await expect(page.getByLabel("口令", { exact: true })).toHaveValue("");
});
