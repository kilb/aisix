import { randomUUID } from "node:crypto";
import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

let stack: Stack;
const PASSWORD = "copy-privacy-real-pass-1115";
test.use({ trace: "off" });
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
  expect(stack.mockUpstreamUrl).toBe("");
});
test.afterAll(async () => { await stack?.stop(); });

async function secretGone(page: Page, secret: string): Promise<void> {
  expect(await page.content()).not.toContain(secret);
  expect(await page.evaluate(() => JSON.stringify([
    { ...localStorage }, { ...sessionStorage }, document.cookie,
    [...document.querySelectorAll("input")].map((input) => input.value),
  ]))).not.toContain(secret);
  expect(page.url()).not.toContain(secret);
}

test("真实密钥一次性结果关闭清理，图标复制保持遮罩，系统拒绝剪贴板后可手动选择", async ({ page, context }) => {
  const suffix = randomUUID().slice(0, 8);
  const email = `copy-${suffix}@privacy-real.test`;
  const account = await stack.admin.post("/admin/resources/provider_keys", { doc: {
    display_name: `复制验收账号 ${suffix}`, provider: "openai", api_key: `unused-copy-${suffix}`,
    api_base: "https://api.example.test/v1", groups: ["shared"],
  } });
  const model = await stack.admin.post("/admin/resources/models", { doc: {
    display_name: `复制验收模型 ${suffix}`, provider: "openai", model_name: "unused-copy-model", provider_key_id: account.id,
  } });
  const api = stack.tenant();
  const signed = await api.post("/api/signup", { org_name: `复制验收组织 ${suffix}`, email, password: PASSWORD });
  await stack.admin.post(`/admin/tenants/${signed.tenant.id}/approve`);
  const tenant = await stack.admin.get(`/admin/tenants/${signed.tenant.id}`);
  await stack.admin.patch(`/admin/tenants/${signed.tenant.id}`, { allowed_model_ids: [model.id] }, tenant.version);
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: stack.previewUrl });
  const consoleLines: string[] = [];
  page.on("console", (message) => consoleLines.push(message.text()));
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await page.getByRole("link", { name: "API 密钥", exact: true }).click();
  const label = `真实复制密钥 ${suffix}`;
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  const form = page.getByRole("dialog", { name: "新建密钥", exact: true });
  await form.getByLabel("名称", { exact: true }).fill(label);
  await form.getByRole("button", { name: "新建密钥", exact: true }).click();
  const notice = page.getByRole("region", { name: "密钥明文（可再次复制）", exact: true });
  const secret = await notice.getByRole("textbox").inputValue();
  expect(secret).toMatch(/^sk-rt-[A-Za-z0-9]{16,}$/);
  await notice.getByRole("button", { name: "复制", exact: true }).click();
  await expect(notice.getByRole("status")).toHaveText("已复制");
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(secret);
  await expect(notice.getByRole("button", { name: "已复制", exact: true })).toHaveText("");
  await notice.getByRole("button", { name: "我已保存", exact: true }).click();
  await expect(notice).toHaveCount(0);
  await secretGone(page, secret);
  await page.reload();
  const row = page.getByRole("row").filter({ hasText: label });
  await expect(row).toContainText(`${secret.slice(0, 12)}…`);
  await expect(row.getByRole("button", { name: /^(显示|隐藏)$/ })).toHaveCount(0);
  await row.getByRole("button", { name: "复制密钥", exact: true }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(secret);
  await expect(row.getByRole("button", { name: "已复制", exact: true })).toHaveText("");
  await secretGone(page, secret);
  await page.reload();
  // 使用浏览器真实权限拒绝，不替换 clipboard，也不拦截后端请求。
  const cdp = await context.newCDPSession(page);
  const target = await cdp.send("Target.getTargetInfo");
  const permission = async (setting: "denied" | "granted"): Promise<void> => {
    for (const allowWithoutSanitization of [false, true]) await cdp.send("Browser.setPermission", {
      permission: { name: "clipboard-write", allowWithoutSanitization }, setting,
      origin: stack.previewUrl, browserContextId: target.targetInfo.browserContextId,
    });
  };
  await permission("denied");
  expect(await page.evaluate(async () => (await navigator.permissions.query({ name: "clipboard-write" as PermissionName })).state)).toBe("denied");
  await row.getByRole("button", { name: "复制密钥", exact: true }).click();
  const manual = row.getByRole("textbox", { name: "手动复制密钥", exact: true });
  await expect(manual).toHaveValue(secret);
  await expect(manual).toHaveAttribute("readonly", "");
  await manual.click();
  await manual.press("ControlOrMeta+A");
  expect(await manual.evaluate((input: HTMLInputElement) => [input.selectionStart, input.selectionEnd])).toEqual([0, secret.length]);
  await permission("granted");
  await row.getByRole("button", { name: "复制密钥", exact: true }).click();
  await expect.poll(() => page.evaluate(() => navigator.clipboard.readText())).toBe(secret);
  await expect(row.getByRole("button", { name: "已复制", exact: true })).toHaveText("");
  await expect(manual).toHaveCount(0);
  await secretGone(page, secret);
  await permission("denied");
  await row.getByRole("button", { name: /^(复制密钥|已复制)$/ }).click();
  await expect(manual).toHaveValue(secret);
  expect(await row.getByRole("button", { name: "复制密钥", exact: true }).count()).toBe(1);
  await page.getByRole("link", { name: "概览", exact: true }).click();
  await page.getByRole("link", { name: "API 密钥", exact: true }).click();
  await expect(manual).toHaveCount(0);
  await secretGone(page, secret);
  expect(consoleLines.join("\n")).not.toContain(secret);
});
