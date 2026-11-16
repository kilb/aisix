import { expect, test, type BrowserContext, type CDPSession, type Page } from "@playwright/test";
import { randomUUID } from "node:crypto";
import { readFileSync, mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { chooseSelect } from "../e2e/select";

const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const base: string = state.adminUrl;
const output = resolve(process.env.REFINED_QA_OUTPUT!);
if (!state.ready || state.mockUpstream !== false || !["127.0.0.1", "localhost"].includes(new URL(base).hostname)) throw new Error("A real loopback API world is required");

// 只报告布尔渠道，失败报告也不能带回测试密钥。
async function leakage(page: Page, secret: string) {
  return page.evaluate(value => ({
    text: (document.body.textContent ?? "").includes(value),
    html: document.documentElement.outerHTML.includes(value),
    values: [...document.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("input,textarea")].some(input => input.value.includes(value)),
    storage: JSON.stringify([Object.entries(localStorage), Object.entries(sessionStorage)]).includes(value),
    url: location.href.includes(value),
  }), secret);
}
const sessions = new WeakMap<Page, CDPSession>();
async function permission(context: BrowserContext, page: Page, setting: "denied" | "granted") {
  let cdp = sessions.get(page);
  if (!cdp) { cdp = await context.newCDPSession(page); sessions.set(page, cdp); }
  const target = await cdp.send("Target.getTargetInfo");
  for (const allowWithoutSanitization of [false, true]) await cdp.send("Browser.setPermission", {
    permission: { name: "clipboard-write", allowWithoutSanitization }, setting,
    origin: new URL(base).origin, browserContextId: target.targetInfo.browserContextId,
  });
}

test("new and existing keys stay masked on successful copy and real clipboard refusal", async ({ page, context }) => {
  const label = `template-mask-${randomUUID().slice(0, 8)}`;
  let secret = "", keyId = "", tenantId = "";
  const consoleLines: string[] = [], mutations: string[] = [];
  const receipts: { stage: string; leak: Awaited<ReturnType<typeof leakage>> }[] = [];
  const cleanup = { attempted: false, status: 0, clipboardCleared: false, keyId: "", tenantId: "", label };
  page.on("console", message => consoleLines.push(message.text()));
  page.on("request", request => {
    const path = new URL(request.url()).pathname;
    if (/^\/v1\//.test(path) || /\/probe(?:\/|$)|\/upstream-models$/.test(path)) mutations.push(path);
  });
  const safe = async (stage: string) => {
    const leak = await leakage(page, secret); receipts.push({ stage, leak });
    expect(Object.values(leak).some(Boolean), `${stage}: no plaintext in any rendered or persisted channel`).toBe(false);
    expect(consoleLines.some(line => line.includes(secret)), `${stage}: no plaintext console output`).toBe(false);
  };
  try {
    await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: new URL(base).origin });
    await page.goto(base + "/#keys");
    await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
    await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
    const login = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login" && response.request().method() === "POST");
    await page.getByRole("button", { name: "登录", exact: true }).click(); expect((await login).status()).toBe(200);
    await page.goto(base + "/#keys");
    await expect(page.getByRole("heading", { name: "密钥", level: 1, exact: true })).toBeVisible();
    const tenants = await page.evaluate(async () => { const response = await fetch("/admin/tenants"); if (!response.ok) throw new Error("tenant oracle failed"); return (await response.json()).items as { id: string; is_internal: boolean }[]; });
    const internal = tenants.find(tenant => tenant.is_internal); expect(Boolean(internal)).toBe(true); tenantId = internal!.id;
    await chooseSelect(page.getByRole("combobox", { name: "租户", exact: true }), tenantId);
    await page.getByRole("button", { name: "新建密钥", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "新建密钥", exact: true });
    await dialog.getByRole("textbox", { name: "名称", exact: true }).fill(label);
    await chooseSelect(dialog.getByRole("combobox", { name: "分组（必选）", exact: true }), state.tokenUiFixture.groupName);
    const created = page.waitForResponse(response => new URL(response.url()).pathname === `/admin/tenants/${tenantId}/keys` && response.request().method() === "POST");
    await dialog.getByRole("button", { name: "创建", exact: true }).click();
    const response = await created; expect(response.status()).toBe(201);
    const minted = await response.json();
    secret = minted.plaintext; keyId = minted.key.id; cleanup.keyId = keyId; cleanup.tenantId = tenantId;
    expect(typeof secret === "string" && secret.length > 20, "real API returned a complete credential into test memory").toBe(true);
    expect(minted.key.label === label && typeof keyId === "string").toBe(true);
    const notice = page.getByRole("region", { name: "密钥已创建", exact: true }); await expect(notice).toBeVisible();
    await safe("created"); await expect(notice.getByLabel("密钥前缀", { exact: true })).toHaveText(minted.key.prefix + "…");
    await notice.getByRole("button", { name: "复制", exact: true }).click();
    await expect.poll(() => page.evaluate(async expected => await navigator.clipboard.readText() === expected, secret)).toBe(true);
    await safe("created-copy");
    await permission(context, page, "denied");
    await notice.getByRole("button", { name: /^(复制|已复制)$/ }).click();
    await expect(notice.getByText("无法写入剪贴板，请允许剪贴板权限后重试。", { exact: true })).toBeVisible();
    await safe("created-denied");
    await expect(notice.getByRole("textbox")).toHaveCount(0);
    mkdirSync(join(output, "screenshots"), { recursive: true });
    await page.screenshot({ path: join(output, "screenshots", "key-created-masked-denied-1440.png"), fullPage: true });
    await notice.getByRole("button", { name: "关闭", exact: true }).click();
    const row = page.getByRole("table", { name: "密钥列表", exact: true }).getByRole("row").filter({ hasText: label });
    await expect(row).toBeVisible();
    await permission(context, page, "granted");
    const revealed = page.waitForResponse(response => new URL(response.url()).pathname === `/admin/keys/${keyId}/reveal` && response.request().method() === "POST");
    await row.getByRole("button", { name: "复制密钥", exact: true }).click(); expect((await revealed).status()).toBe(200);
    await expect.poll(() => page.evaluate(async expected => await navigator.clipboard.readText() === expected, secret)).toBe(true);
    await safe("existing-copy");
    await permission(context, page, "denied"); await row.getByRole("button", { name: /^(复制密钥|已复制)$/ }).click();
    await expect(row.getByText("无法写入剪贴板，请允许剪贴板权限后重试。", { exact: true })).toBeVisible(); await safe("existing-denied");
    await expect(row.getByRole("textbox")).toHaveCount(0);
    await page.setViewportSize({ width: 390, height: 1000 }); await safe("existing-denied-mobile");
    const routingName = row.locator('td[data-label="分组"]').getByText(state.tokenUiFixture.groupName, { exact: true });
    await expect(routingName).toBeVisible();
    const groupLayout = await routingName.evaluate(element => {
      const range = document.createRange(); range.selectNodeContents(element);
      return { width: element.getBoundingClientRect().width, lines: new Set([...range.getClientRects()].map(box => Math.round(box.top))).size, length: element.textContent?.length ?? 0 };
    });
    expect(groupLayout.width, "the full routing name retains a readable column").toBeGreaterThanOrEqual(72);
    expect(groupLayout.lines, "the routing name does not become one character per line").toBeLessThanOrEqual(Math.ceil(groupLayout.length / 6));
    await page.screenshot({ path: join(output, "screenshots", "key-existing-masked-denied-390.png"), fullPage: true });
    await permission(context, page, "granted"); await row.getByRole("button", { name: "复制密钥", exact: true }).click();
    await expect.poll(() => page.evaluate(async expected => await navigator.clipboard.readText() === expected, secret)).toBe(true); await safe("retry");
    expect(mutations, "no inference or provider probes").toEqual([]);
  } finally {
    // 先离开密钥页，测试回归失败也不能让自动失败快照存入真实密钥。
    await page.goto(base + "/#overview");
    if (keyId && tenantId) {
      cleanup.attempted = true;
      cleanup.status = await page.evaluate(async ({ keyId, tenantId, label }) => {
        const list = await fetch(`/admin/tenants/${tenantId}/keys`); if (!list.ok) return list.status;
        const key = (await list.json()).items.find((item: { id: string; label: string }) => item.id === keyId && item.label === label);
        if (!key) return 404;
        return (await fetch(`/admin/keys/${keyId}`, { method: "DELETE", headers: { "content-type": "application/json", "if-match": String(key.version) } })).status;
      }, { keyId, tenantId, label });
    }
    try {
      await permission(context, page, "granted"); await page.bringToFront();
      await page.evaluate(() => navigator.clipboard.writeText("")); cleanup.clipboardCleared = true;
    } catch {
      // 不覆盖原始失败；隔离浏览器关闭时同时清理剪贴板环境。
    }
    mkdirSync(output, { recursive: true }); writeFileSync(join(output, "privacy-receipt.json"), JSON.stringify({ receipts, cleanup, noInference: mutations.length === 0 }, null, 2));
    secret = "";
  }
  expect(cleanup.status, "the dedicated test key was revoked through the real API").toBe(204);
});
