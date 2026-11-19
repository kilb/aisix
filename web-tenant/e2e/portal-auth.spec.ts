import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

let stack: Stack;
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});
test.afterAll(async () => { await stack?.stop(); });

async function togglePasskey(origin: string, enabled: boolean) {
  const { version } = await stack.admin.get("/admin/settings/authentication");
  await stack.admin.put("/admin/settings/authentication", {
    version,
    portal_origin: origin,
    email: { enabled: false, endpoint: "", from: "" },
    turnstile: { enabled: false, site_key: "" },
    passkey: { enabled },
    github: { enabled: false, client_id: "" },
    google: { enabled: false, client_id: "" },
  });
}

test("待审核组织的成员可注册 Passkey、登录并移除它", async ({ context, page }) => {
  const origin = new URL(stack.previewUrl);
  origin.hostname = "localhost";
  await page.goto(origin.href);
  await expect(page.getByRole("button", { name: "登录", exact: true })).toBeEnabled();
  await expect(page.getByRole("button", { name: "使用 Passkey 登录", exact: true })).toHaveCount(0);
  await togglePasskey(origin.origin, true);

  const devTools = await context.newCDPSession(page);
  await devTools.send("WebAuthn.enable");
  const { authenticatorId } = await devTools.send("WebAuthn.addVirtualAuthenticator", {
    options: {
      protocol: "ctap2",
      transport: "internal",
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  });

  await page.goto(origin.href);
  const email = "passkey-owner@portal-auth.test";
  const password = "portal-auth-test-password";
  await page.getByRole("tab", { name: "注册组织", exact: true }).click();
  await page.getByLabel("组织名称", { exact: true }).fill("Passkey 验收组织");
  await page.getByLabel("邮箱", { exact: true }).fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "注册组织", exact: true }).click();
  await expect(page.getByText(/组织正在等待平台审核/)).toBeVisible();

  await page.getByRole("button", { name: "账号设置", exact: true }).click();
  await page.getByRole("button", { name: "添加 Passkey", exact: true }).click();
  await page.getByLabel("Passkey 名称", { exact: true }).fill("验收浏览器");
  await page.getByRole("dialog", { name: "添加 Passkey" }).getByLabel("当前口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "创建 Passkey", exact: true }).click();
  await expect(page.getByText("验收浏览器", { exact: true })).toBeVisible();
  const { credentials } = await devTools.send("WebAuthn.getCredentials", { authenticatorId });
  expect(credentials).toHaveLength(1);
  const credentialId = Buffer.from(credentials[0]!.credentialId, "base64").toString("base64url");
  await page.getByRole("button", { name: "登出", exact: true }).click();

  await page.getByLabel("邮箱", { exact: true }).fill(email);
  await page.getByRole("button", { name: "使用 Passkey 登录", exact: true }).click();
  await expect(page.getByText(/组织正在等待平台审核/)).toBeVisible();
  await togglePasskey(origin.origin, false);
  await page.getByRole("button", { name: "账号设置", exact: true }).click();
  await expect(page.getByText(/平台已暂停 Passkey 登录/)).toBeVisible();
  await expect(page.getByRole("button", { name: "添加 Passkey", exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "移除 Passkey 验收浏览器", exact: true }).click();
  await page.getByRole("dialog", { name: "移除 Passkey" }).getByLabel("当前口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "确认移除", exact: true }).click();
  await expect(page.getByText("验收浏览器", { exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "登出", exact: true }).click();

  await expect(page.getByRole("button", { name: "登录", exact: true })).toBeEnabled();
  await expect(page.getByRole("button", { name: "使用 Passkey 登录", exact: true })).toHaveCount(0);
  await togglePasskey(origin.origin, true);
  await page.reload();
  await page.getByLabel("邮箱", { exact: true }).fill(email);
  const challenge = page.waitForResponse((response) => response.url().endsWith("/api/auth/passkeys/login/start"));
  await page.getByRole("button", { name: "使用 Passkey 登录", exact: true }).click();
  const response = await challenge;
  expect(response.ok()).toBe(true);
  const { options } = await response.json();
  expect(options.publicKey.allowCredentials.map(({ id }: { id: string }) => id)).not.toContain(credentialId);
  await expect(page.getByRole("button", { name: "账号设置", exact: true })).toHaveCount(0);
});

test("浏览器使用 Turnstile 官方测试密钥注册组织", async ({ page }) => {
  const origin = new URL(stack.previewUrl);
  origin.hostname = "localhost";
  const { version } = await stack.admin.get("/admin/settings/authentication");
  // 官方测试密钥走真实的 Cloudflare 小组件和 Siteverify。
  await stack.admin.put("/admin/settings/authentication", {
    version, portal_origin: origin.origin,
    email: { enabled: false, endpoint: "", from: "" },
    turnstile: {
      enabled: true,
      site_key: "1x00000000000000000000AA",
      secret_key: "1x0000000000000000000000000000000AA",
    },
    passkey: { enabled: false },
    github: { enabled: false, client_id: "" },
    google: { enabled: false, client_id: "" },
  });
  await page.setViewportSize({ width: 390, height: 850 });
  await page.goto(origin.href);
  await page.getByRole("tab", { name: "注册组织", exact: true }).click();
  await page.getByLabel("组织名称", { exact: true }).fill("Turnstile 验收组织");
  await page.getByLabel("邮箱", { exact: true }).fill("turnstile-browser@portal-auth.test");
  await page.getByLabel("口令", { exact: true }).fill("portal-auth-browser-test-password");
  const signup = page.waitForResponse((response) => response.url().endsWith("/api/signup"), { timeout: 25_000 });
  await page.getByRole("button", { name: "注册组织", exact: true }).click();
  const response = await signup;
  expect(response.status()).toBe(201);
  expect(response.request().postDataJSON().turnstile_token).toEqual(expect.any(String));
  await expect(page.getByText(/组织正在等待平台审核/)).toBeVisible();
});
