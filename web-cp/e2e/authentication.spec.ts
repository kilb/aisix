import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

const SETTINGS = "/admin/settings/authentication";
const PORTAL = "https://portal.example.com";
let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});

test.afterAll(async () => {
  await stack?.stop();
});

async function login(page: Page, email: string, password: string) {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.locator('[data-tab="overview"]')).toBeVisible();
}

test("管理员可以配置门户认证，回调 URL 可见，读取设置时密钥不回显", async ({ page }) => {
  await login(page, stack.operator.email, stack.operator.password);
  await page.locator('[data-tab="authentication"]').click();
  await expect(page.getByRole("heading", { name: "认证设置" })).toBeVisible();
  await expect(page.getByLabel("启用 Passkey")).not.toBeChecked();
  await expect(page.getByLabel("启用口令找回")).not.toBeChecked();
  await expect(page.getByLabel("租户门户地址")).toBeEditable();

  const portalOrigin = page.getByLabel("租户门户地址");
  await portalOrigin.fill(`${PORTAL}/account`);
  await page.getByRole("button", { name: "保存认证设置" }).click();
  await expect(portalOrigin).toHaveAttribute("aria-invalid", "true");
  await expect(portalOrigin).toBeFocused();

  await portalOrigin.fill(PORTAL);
  await page.getByLabel("启用邮件验证码").check();
  await page.getByLabel("邮件接口 URL").fill("https://example.com/send");
  await page.getByLabel("发件邮箱").fill("verify@example.com");
  const mailSecret = "e2e-mail-credential-for-settings-only";
  await page.getByLabel("发信 API Key").fill(mailSecret);
  await page.getByLabel("启用 Turnstile").check();
  await page.getByLabel("Site Key", { exact: true }).fill("0x4AAAAAAABAuthConfigDemo");
  const turnstileSecret = "0x4AAAAAAABAuthConfigDemoSecret";
  await page.getByLabel("Secret Key", { exact: true }).fill(turnstileSecret);
  await page.getByLabel("启用 Passkey").check();
  for (const provider of ["GitHub", "Google"] as const) {
    const panel = page.getByRole("region", { name: `${provider} 登录`, exact: true });
    await panel.getByLabel(`启用 ${provider} 登录`).check();
    await panel.getByLabel(`${provider} Client ID`).fill(`${provider.toLowerCase()}-e2e-client`);
    await panel.getByLabel(`${provider} Client Secret`).fill(`${provider.toLowerCase()}-e2e-secret`);
    await expect(panel.getByRole("code").filter({ hasText: "/callback" }))
      .toHaveText(`${PORTAL}/api/auth/oauth/${provider.toLowerCase()}/callback`);
  }
  await page.getByRole("button", { name: "保存认证设置" }).click();
  await expect(page.getByRole("status").filter({ hasText: "认证设置已保存" })).toBeVisible();

  await page.reload();
  await expect(page.getByLabel("租户门户地址")).toHaveValue(PORTAL);
  await expect(page.getByLabel("启用 Passkey")).toBeChecked();
  await expect(page.getByLabel("发信 API Key")).toHaveValue("");
  await expect(page.getByLabel("Secret Key", { exact: true })).toHaveValue("");
  await expect(page.getByLabel("GitHub Client Secret")).toHaveValue("");
  await expect(page.getByLabel("Google Client Secret")).toHaveValue("");

  const settings = await stack.admin.get(SETTINGS);
  expect(settings.email).toMatchObject({ enabled: true, from: "verify@example.com", api_key_configured: true });
  expect(settings.turnstile).toMatchObject({ enabled: true, secret_key_configured: true });
  expect(settings.github).toMatchObject({ client_secret_configured: true, callback_url: `${PORTAL}/api/auth/oauth/github/callback` });
  expect(settings.google).toMatchObject({ client_secret_configured: true, callback_url: `${PORTAL}/api/auth/oauth/google/callback` });
  expect(JSON.stringify(settings)).not.toContain(mailSecret);
  expect(JSON.stringify(settings)).not.toContain(turnstileSecret);
  expect(JSON.stringify(settings)).not.toContain("e2e-secret");
  expect(await stack.tenant().get("/api/auth/options")).toEqual({
    email_verification: true,
    password_recovery: false,
    turnstile_site_key: "0x4AAAAAAABAuthConfigDemo",
    passkey: true,
    github: true,
    google: true,
  });

  await page.getByLabel("启用口令找回").check();
  await page.getByRole("button", { name: "保存认证设置" }).click();
  await expect(page.getByRole("status").filter({ hasText: "认证设置已保存" })).toBeVisible();
  expect(await stack.tenant().get("/api/auth/options")).toMatchObject({ password_recovery: true });

  // 关闭登录方式后仍可保留已配置的凭据。
  for (const label of ["启用邮件验证码", "启用口令找回", "启用 Turnstile", "启用 Passkey", "启用 GitHub 登录", "启用 Google 登录"]) {
    await page.getByLabel(label).uncheck();
  }
  await page.getByRole("button", { name: "保存认证设置" }).click();
  await expect(page.getByRole("status").filter({ hasText: "认证设置已保存" })).toBeVisible();
  expect((await stack.admin.get(SETTINGS)).email).toMatchObject({ enabled: false, api_key_configured: true });
  expect(await stack.tenant().get("/api/auth/options")).toMatchObject({ password_recovery: false });

  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.getByRole("heading", { name: "认证设置" })).toBeVisible();
  expect(await page.locator("main").evaluate(main => main.scrollWidth <= main.clientWidth)).toBe(true);
});

test("运维账号不能打开认证设置或读取其配置", async ({ page }) => {
  const account = { email: "auth-settings-operator@example.com", password: "e2e-staff-password-2026" };
  await stack.admin.post("/admin/operators", { ...account, role: "operator" });
  await login(page, account.email, account.password);
  await expect(page.locator('[data-tab="authentication"]')).toHaveCount(0);
  await page.goto(`${stack.previewUrl}/#authentication`);
  await expect(page.locator('[data-tab="overview"]')).toHaveAttribute("aria-current", "page");
  const operator = stack.tenant();
  await operator.post("/admin/login", account);
  expect((await operator.request("GET", SETTINGS)).status).toBe(403);
});

test("配置被其他页面修改时拒绝覆盖，管理员可以载入最新设置", async ({ page }) => {
  await login(page, stack.operator.email, stack.operator.password);
  await page.locator('[data-tab="authentication"]').click();
  const origin = page.getByLabel("租户门户地址");
  await expect(origin).toBeEditable();

  const other = await page.context().newPage();
  await other.goto(`${stack.previewUrl}/#authentication`);
  await other.getByLabel("租户门户地址").fill("https://new.example.com");
  await other.getByRole("button", { name: "保存认证设置" }).click();
  await expect(other.getByRole("status").filter({ hasText: "认证设置已保存" })).toBeVisible();

  await origin.fill("https://draft.example.com");
  const save = page.getByRole("button", { name: "保存认证设置" });
  await save.click();
  await expect(page.getByRole("alert")).toContainText("已被他人修改");
  await expect(origin).toHaveValue("https://draft.example.com");
  await expect(save).toBeDisabled();
  await page.getByRole("button", { name: "放弃草稿并重新载入" }).click();
  await expect(origin).toHaveValue("https://new.example.com");
  await other.close();
});
