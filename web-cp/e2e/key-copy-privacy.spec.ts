import { randomUUID } from "node:crypto";
import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { chooseSelect } from "./select";

let stack: Stack;
test.use({ trace: "off" });
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
  expect(stack.mockUpstreamUrl).toBe("");
});
test.afterAll(async () => { await stack?.stop(); });

/** DOM、控件属性、存储与日志分别检查，断言仅输出泄漏通道。 */
async function secretGone(page: Page, secret: string, consoleLines: readonly string[]): Promise<void> {
  const leaks = await page.evaluate(value => ({
    text: (document.body.textContent ?? "").includes(value),
    html: document.documentElement.outerHTML.includes(value),
    attributes: [...document.querySelectorAll("*")].some(element => [...element.attributes].some(attribute => attribute.value.includes(value))),
    inputs: [...document.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("input, textarea")].some(input => input.value.includes(value)),
    storage: JSON.stringify([Object.entries(localStorage), Object.entries(sessionStorage)]).includes(value),
    cookie: document.cookie.includes(value),
    url: location.href.includes(value),
  }), secret);
  expect(leaks).toEqual({ text: false, html: false, attributes: false, inputs: false, storage: false, cookie: false, url: false });
  expect(consoleLines.some(line => line.includes(secret))).toBe(false);
  await expect(page.getByLabel("密钥明文", { exact: true })).toHaveCount(0);
  await expect(page.getByRole("textbox", { name: "手动复制密钥", exact: true })).toHaveCount(0);
}

test("内部密钥始终遮罩与图标复制，剪贴板拒绝仅提示重试，客户和旧密钥保持权限边界", async ({ page, context }) => {
  const suffix = randomUUID().slice(0, 8);
  const modelName = `copy-internal-${suffix}`;
  const account = await stack.admin.post("/admin/resources/provider_keys", { doc: {
    display_name: `内部复制验收账号 ${suffix}`, provider: "openai", api_key: `unused-copy-${suffix}`,
    api_base: "https://api.example.test/v1", groups: ["shared"],
  } });
  await stack.admin.post("/admin/resources/models", { doc: {
    display_name: modelName, provider: "openai", model_name: "unused-copy-model", provider_key_id: account.id,
  } });
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: stack.previewUrl });
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  page.on("pageerror", error => consoleLines.push(error.message));
  // 创建与列表两个复制入口都用 Chromium 的真实权限拒绝路径。
  const cdp = await context.newCDPSession(page);
  const target = await cdp.send("Target.getTargetInfo");
  const permission = async (setting: "denied" | "granted"): Promise<void> => {
    for (const allowWithoutSanitization of [false, true]) await cdp.send("Browser.setPermission", {
      permission: { name: "clipboard-write", allowWithoutSanitization }, setting,
      origin: stack.previewUrl, browserContextId: target.targetInfo.browserContextId,
    });
  };
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令").fill(stack.operator.password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await page.locator('[data-tab="keys"]').click();
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  const form = page.getByRole("group", { name: "新建密钥", exact: true });
  const label = `内部真实复制 ${suffix}`;
  await form.getByLabel("名称", { exact: true }).fill(label);
  await chooseSelect(form.getByLabel("分组（必选）"), { label: "通用分组（shared）" });
  await form.getByLabel("模型（可写通配）").fill(modelName);
  const creation = page.waitForResponse((response) => /\/admin\/tenants\/[^/]+\/keys$/.test(new URL(response.url()).pathname) && response.request().method() === "POST");
  await form.getByRole("button", { name: "创建", exact: true }).click();
  const response = await creation;
  expect(response.status()).toBe(201);
  const body: { plaintext?: unknown } = await response.json();
  if (typeof body.plaintext !== "string") throw new Error("密钥创建响应缺少明文凭据");
  const secret = body.plaintext;
  expect(/^sk-rt-[A-Za-z0-9]{16,}$/.test(secret)).toBe(true);
  const notice = page.getByRole("region", { name: "密钥已创建", exact: true });
  await expect(notice).toBeVisible();
  await expect(notice).toContainText(`${secret.slice(0, 12)}…`);
  await secretGone(page, secret, consoleLines);
  await notice.getByRole("button", { name: "复制", exact: true }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(secret);
  await expect(notice.getByRole("button", { name: "已复制", exact: true })).toHaveText("");
  await secretGone(page, secret, consoleLines);
  await permission("denied");
  expect(await page.evaluate(async () => (await navigator.permissions.query({ name: "clipboard-write" as PermissionName })).state)).toBe("denied");
  await notice.getByRole("button", { name: /^(复制|已复制)$/ }).click();
  await expect(notice).toContainText("无法写入剪贴板");
  await expect(notice).not.toContainText(/手动|选中/);
  await expect(notice.getByRole("button", { name: "复制", exact: true })).toBeEnabled();
  await secretGone(page, secret, consoleLines);
  await permission("granted");
  await notice.getByRole("button", { name: "复制", exact: true }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(secret);
  await expect(notice.getByRole("button", { name: "已复制", exact: true })).toHaveText("");
  await secretGone(page, secret, consoleLines);
  await notice.getByRole("button", { name: "关闭", exact: true }).click();
  await expect(notice).toHaveCount(0);
  await secretGone(page, secret, consoleLines);
  await page.reload();
  await page.locator('[data-tab="keys"]').click();
  const row = page.getByRole("table", { name: "密钥列表", exact: true }).getByRole("row").filter({ hasText: label });
  await expect(row).toContainText(`${secret.slice(0, 12)}…`);
  await expect(row.getByRole("button", { name: /^(显示|隐藏)$/ })).toHaveCount(0);
  await row.getByRole("button", { name: "复制密钥", exact: true }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(secret);
  await expect(row.getByRole("button", { name: "已复制", exact: true })).toHaveText("");
  await secretGone(page, secret, consoleLines);
  await page.reload();
  await page.locator('[data-tab="keys"]').click();
  // 后端的取密钥与审计仍由真实控制平面完成。
  await permission("denied");
  expect(await page.evaluate(async () => (await navigator.permissions.query({ name: "clipboard-write" as PermissionName })).state)).toBe("denied");
  await row.getByRole("button", { name: "复制密钥", exact: true }).click();
  const manual = row.getByRole("textbox", { name: "手动复制密钥", exact: true });
  await expect(row).toContainText("无法写入剪贴板");
  await expect(row).not.toContainText(/手动|选中/);
  await expect(manual).toHaveCount(0);
  await expect(row.getByRole("button", { name: "复制密钥", exact: true })).toBeEnabled();
  await secretGone(page, secret, consoleLines);
  await permission("granted");
  await row.getByRole("button", { name: "复制密钥", exact: true }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(secret);
  await expect(row.getByRole("button", { name: "已复制", exact: true })).toHaveText("");
  await expect(manual).toHaveCount(0);
  await secretGone(page, secret, consoleLines);
  await permission("denied");
  await row.getByRole("button", { name: /^(复制密钥|已复制)$/ }).click();
  await expect(row).toContainText("无法写入剪贴板");
  await expect(row).not.toContainText(/手动|选中/);
  await expect(manual).toHaveCount(0);
  await secretGone(page, secret, consoleLines);
  expect(await row.getByRole("button", { name: "复制密钥", exact: true }).count()).toBe(1);
  await page.locator('[data-tab="tenants"]').click();
  await page.locator('[data-tab="keys"]').click();
  await expect(manual).toHaveCount(0);
  await secretGone(page, secret, consoleLines);
  const internal = (await stack.admin.get("/admin/tenants")).items.find((tenant: { is_internal: boolean }) => tenant.is_internal);
  const group = (await stack.admin.get("/admin/provider-groups")).items.find((item: { name: string }) => item.name === "shared");
  const old = await stack.admin.post(`/admin/tenants/${internal.id}/keys`, { label: `旧密钥 ${suffix}`, group_id: group.id });
  stack.clearKeySealed(old.key.id);
  const api = stack.tenant();
  const customer = await api.post("/api/signup", { org_name: `客户复制边界 ${suffix}`, email: `customer-${suffix}@copy-real.test`, password: "customer-copy-real-1115" });
  await stack.admin.post(`/admin/tenants/${customer.tenant.id}/approve`);
  await api.post("/api/login", { email: `customer-${suffix}@copy-real.test`, password: "customer-copy-real-1115" });
  await api.post("/api/keys", { label: `客户密钥 ${suffix}` });
  await page.reload();
  await page.locator('[data-tab="keys"]').click();
  const table = page.getByRole("table", { name: "密钥列表", exact: true });
  const stale = table.getByRole("row").filter({ hasText: `旧密钥 ${suffix}` });
  await expect(stale.getByRole("button", { name: "复制密钥", exact: true })).toHaveAttribute("aria-disabled", "true");
  await chooseSelect(page.getByRole("combobox", { name: /^租户/ }), { label: `客户复制边界 ${suffix}` });
  const theirs = table.getByRole("row").filter({ hasText: `客户密钥 ${suffix}` });
  await expect(theirs).toBeVisible();
  await expect(theirs.getByRole("button", { name: /复制密钥|显示|隐藏/ })).toHaveCount(0);
  expect(consoleLines.join("\n")).not.toContain(secret);
  const copied = (await stack.admin.get(`/admin/tenants/${internal.id}/keys`)).items.find((key: { label: string }) => key.label === label);
  const audit = await stack.admin.get(`/admin/audit?target_id=${copied.id}&limit=50`);
  expect(audit.items.filter((entry: { action: string }) => entry.action === "key.reveal").length).toBeGreaterThanOrEqual(2);
  expect(JSON.stringify(audit)).not.toContain(secret);
});
