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
  await page.goto(new URL("/login", origin).href);
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

  await page.goto(new URL("/login", origin).href);
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

  const allowCredentials = async (email: string) => {
    const response = await page.request.post(new URL("/api/auth/passkeys/login/start", origin).href, { data: { email } });
    expect(response.status()).toBe(200);
    const { options } = await response.json();
    return options.publicKey.allowCredentials.map(({ id }: { id: string }) => id) as string[];
  };
  const enrolled = await allowCredentials(email);
  expect(enrolled).toContain(credentialId);
  expect(enrolled.length).toBeGreaterThan(1);
  const unknown = await allowCredentials("unknown@example.net");
  expect(unknown).toHaveLength(enrolled.length);
  expect(unknown).not.toContain(credentialId);
  expect(await allowCredentials("unknown@example.net")).toEqual(unknown);

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

test("注册页看到新的 Turnstile 要求后保留已填内容并可重新提交", async ({ page }) => {
  const origin = new URL(stack.previewUrl);
  origin.hostname = "localhost";
  await togglePasskey(origin.origin, false);
  await page.setViewportSize({ width: 390, height: 850 });
  await page.goto(new URL("/login", origin).href);
  await page.getByRole("tab", { name: "注册组织", exact: true }).click();
  const orgName = page.getByLabel("组织名称", { exact: true });
  const email = page.getByLabel("邮箱", { exact: true });
  const password = page.getByLabel("口令", { exact: true });
  const displayName = page.getByLabel("显示名", { exact: true });
  const submit = page.getByRole("button", { name: "注册组织", exact: true });
  await orgName.fill("Turnstile 验收组织");
  await email.fill("turnstile-browser@portal-auth.test");
  await password.fill("portal-auth-browser-test-password");
  await displayName.fill("等待配置更新");
  await expect(submit).toBeEnabled();

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
  const refused = page.waitForResponse((response) => response.url().endsWith("/api/signup"));
  const refreshed = page.waitForResponse((response) => response.url().endsWith("/api/auth/options"), { timeout: 10_000 });
  await submit.click();
  expect((await refused).status()).toBe(422);
  expect((await (await refreshed).json()).turnstile_site_key).toBe("1x00000000000000000000AA");
  await expect(page.getByRole("alert")).toContainText("认证设置已更新");
  await expect(orgName).toHaveValue("Turnstile 验收组织");
  await expect(email).toHaveValue("turnstile-browser@portal-auth.test");
  await expect(password).toHaveValue("portal-auth-browser-test-password");
  await expect(displayName).toHaveValue("等待配置更新");

  const signup = page.waitForResponse((response) => response.url().endsWith("/api/signup"), { timeout: 25_000 });
  await submit.click();
  const response = await signup;
  expect(response.status()).toBe(201);
  expect(response.request().postDataJSON().turnstile_token).toEqual(expect.any(String));
  await expect(page.getByText(/组织正在等待平台审核/)).toBeVisible();
});
