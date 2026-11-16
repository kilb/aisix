import { expect, test, type BrowserContext, type CDPSession, type Locator, type Page, type Response } from "@playwright/test";
import { createHash, randomUUID } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 只在保留的真实隔离组织中创建本次专用记录；口令和密钥明文不进入文件或附件。
type Json = Record<string, unknown>;
interface Credentials { email: string; password: string }
interface ApiResult { status: number; body: unknown }
const session = resolve(required("CONSOLE_TEMPLATE_QA_SESSION"));
const output = resolve(required("CONSOLE_TEMPLATE_QA_OUTPUT"));
const state = object(JSON.parse(readFileSync(join(session, "public.json"), "utf8")));
const auth = object(JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8")));
const base = String(process.env.CONSOLE_TEMPLATE_TENANT_URL ?? state.tenantUrl).replace(/\/$/, "");
const origin = new URL(base);
if (state.ready !== true || state.mockUpstream !== false || !["127.0.0.1", "localhost"].includes(origin.hostname)) {
  throw new Error("A ready real loopback QA world is required");
}
const owner = credentials(auth.metricsOwner);
// Playwright 的失败 DOM 上下文会包含普通文本框中的密钥明文，禁用该自动附件。
process.env.PLAYWRIGHT_NO_COPY_PROMPT = "1";
test.use({ screenshot: "off", trace: "off", video: "off", actionTimeout: 15_000, navigationTimeout: 20_000 });
test.setTimeout(180_000);

function required(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required`);
  return value;
}
function object(value: unknown): Json {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("Expected an explicit JSON object");
  return value as Json;
}
function credentials(value: unknown): Credentials {
  const item = object(value);
  if (typeof item.email !== "string" || typeof item.password !== "string") throw new Error("Existing owner credentials are required");
  return { email: item.email, password: item.password };
}
function textField(item: Json, name: string): string {
  if (!Object.hasOwn(item, name) || typeof item[name] !== "string" || item[name] === "") throw new Error(`Explicit ${name} is required`);
  return item[name];
}
async function secret(field: Locator, value: string): Promise<void> {
  try { await field.fill(value); } catch { throw new Error("Could not fill a password control"); }
}
function responseFor(page: Page, method: string, path: string): Promise<Response> {
  return page.waitForResponse(response => new URL(response.url()).origin === origin.origin
    && new URL(response.url()).pathname === path && response.request().method() === method, { timeout: 15_000 });
}
async function api(page: Page, method: string, path: string, body?: Json, version?: number): Promise<ApiResult> {
  if (!path.startsWith("/api/") || new URL(path, base).origin !== origin.origin) throw new Error("Same-origin QA API only");
  try {
    return await page.evaluate(async ({ method, path, body, version }) => {
      const response = await fetch(path, {
        method, credentials: "include",
        headers: { "content-type": "application/json", ...(version === undefined ? {} : { "if-match": `"${version}"` }) },
        ...(body === undefined ? {} : { body: JSON.stringify(body) }),
      });
      const raw = await response.text();
      return { status: response.status, body: raw ? JSON.parse(raw) as unknown : null };
    }, { method, path, body, version });
  } catch { throw new Error(`QA ${method} ${path} did not complete`); }
}
async function settle(page: Page): Promise<void> {
  await page.waitForLoadState("networkidle");
  await page.evaluate(() => document.fonts.ready);
  await expect(page.getByRole("status").filter({ hasText: "载入中…" })).toHaveCount(0);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
}
async function login(page: Page, actor: Credentials): Promise<void> {
  await page.goto(base);
  await page.getByLabel("邮箱", { exact: true }).fill(actor.email);
  await secret(page.getByLabel("口令", { exact: true }), actor.password);
  const loggedIn = responseFor(page, "POST", "/api/login");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await loggedIn).status(), "real UI login").toBe(200);
  await settle(page);
}
async function choose(page: Page, field: string, option: string): Promise<void> {
  await page.getByRole("combobox", { name: field, exact: true }).click();
  await page.getByRole("option", { name: option, exact: true }).click();
}
async function capture(page: Page, name: string): Promise<void> {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), "no page overflow").toBe(true);
  mkdirSync(join(output, "screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "screenshots", `${name}.png`), fullPage: true, mask: [page.locator('input[type="password"]')] });
}
async function clearSecretControls(page: Page): Promise<void> {
  if (page.isClosed()) return;
  try {
    await page.locator('input[type="password"], input[aria-label^="密钥明文"]').evaluateAll(inputs => {
      for (const input of inputs) if (input instanceof HTMLInputElement) input.value = "";
    });
  } catch { /* 页面已被测试运行器关闭时，不回显输入或生成额外快照。 */ }
}
test.afterEach(async ({ page }) => { await clearSecretControls(page); });
async function noRawKeyDom(page: Page, plaintext: string): Promise<void> {
  const exposed = await page.evaluate(value => [...document.querySelectorAll("*")].some(element => {
    const text = [...element.childNodes].filter(node => node.nodeType === Node.TEXT_NODE).map(node => node.textContent ?? "").join("");
    const fields = [text, ...[...element.attributes].map(attribute => attribute.value)];
    if (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement) fields.push(element.value);
    return fields.some(field => field.includes(value));
  }), plaintext);
  expect(exposed, "key plaintext never enters DOM text, attributes or input properties").toBe(false);
  await expect(page.getByLabel("手动复制密钥", { exact: true })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /^(显示|隐藏)(密钥)?$/ })).toHaveCount(0);
}
async function clipboardEquals(page: Page, plaintext: string): Promise<void> {
  const exact = await page.evaluate(async value => {
    try { return (await navigator.clipboard.readText()) === value; }
    catch { return false; }
  }, plaintext);
  expect(exact, "native clipboard contains the exact memory-only key").toBe(true);
}
async function clipboardWritePermission(cdp: CDPSession, browserContextId: string, setting: "granted" | "denied"): Promise<void> {
  // 原生 Chromium 权限覆盖，既不改写 navigator，也不拦截请求或响应。
  for (const allowWithoutSanitization of [false, true]) {
    await cdp.send("Browser.setPermission", { permission: { name: "clipboard-write", allowWithoutSanitization }, setting, origin: origin.origin, browserContextId });
  }
}
async function members(page: Page): Promise<Json[]> {
  const answer = await api(page, "GET", "/api/members");
  expect(answer.status).toBe(200);
  const items = object(answer.body).items;
  if (!Array.isArray(items)) throw new Error("Explicit member items are required");
  return items.map(object);
}
async function storedKey(page: Page, id: string): Promise<Json> {
  const answer = await api(page, "GET", `/api/keys/${encodeURIComponent(id)}`);
  expect(answer.status, "fresh key state").toBe(200);
  return object(answer.body);
}
function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (typeof value !== "object" || value === null) return value;
  return Object.fromEntries(Object.entries(value).sort(([left], [right]) => left.localeCompare(right)).map(([key, item]) => [key, canonical(item)]));
}
async function usageSnapshot(page: Page): Promise<{ count: number; eventIds: string[]; sha256: string }> {
  const answer = await api(page, "GET", "/api/usage/calls?limit=25");
  expect(answer.status, "retained calls remain readable").toBe(200);
  const items = object(answer.body).items;
  if (!Array.isArray(items)) throw new Error("Explicit retained call items are required");
  const rows = items.map(object).sort((left, right) => textField(left, "event_id").localeCompare(textField(right, "event_id")));
  return { count: rows.length, eventIds: rows.map(row => textField(row, "event_id")), sha256: createHash("sha256").update(JSON.stringify(canonical(rows))).digest("hex") };
}

test("real member creation, role persistence, password reset, permissions and complete key editing", async ({ page, browser }) => {
  const suffix = randomUUID();
  const actor = { email: `ui448-${suffix}@example.test`, password: `UI448-${randomUUID()}!` };
  const nextPassword = `UI448-reset-${randomUUID()}!`;
  const displayName = `UI448 成员 ${suffix.slice(0, 8)}`;
  const keyLabel = `UI448 密钥 ${suffix.slice(0, 8)}`;
  const editedLabel = `${keyLabel} 已修改`;
  let memberId: string | null = null;
  let keyId: string | null = null;
  let mintedPlaintext: string | null = null;
  let clipboardControl: { cdp: CDPSession; browserContextId: string } | null = null;
  let createMemberSent = false;
  let createKeySent = false;
  let usageBefore: Awaited<ReturnType<typeof usageSnapshot>> | null = null;
  let journeyComplete = false;
  const cleanup: { keyStatus: number | null; memberStatus: number | null; errors: string[] } = { keyStatus: null, memberStatus: null, errors: [] };
  const contexts: BrowserContext[] = [];
  const runtime: string[] = [];
  const unexpectedWrites: string[] = [];
  let passwordWrites = 0;
  function watch(target: Page): void {
    target.on("pageerror", error => runtime.push(error.message));
    target.on("request", request => {
      const url = new URL(request.url());
      if (url.origin !== origin.origin || ["GET", "HEAD"].includes(request.method())) return;
      const path = url.pathname;
      if (["/api/login", "/api/logout"].includes(path)) return;
      if (request.method() === "POST" && path === "/api/members") {
        const body = object(request.postDataJSON());
        if (body.email === actor.email) { createMemberSent = true; return; }
      }
      if (request.method() === "POST" && path === "/api/keys") {
        if (object(request.postDataJSON()).label === keyLabel) { createKeySent = true; return; }
      }
      if (memberId && path === `/api/members/${encodeURIComponent(memberId)}` && ["PATCH", "DELETE"].includes(request.method())) return;
      if (memberId && path === `/api/members/${encodeURIComponent(memberId)}/password` && request.method() === "POST") { passwordWrites += 1; return; }
      if (keyId && path === `/api/keys/${encodeURIComponent(keyId)}` && ["PATCH", "DELETE"].includes(request.method())) return;
      if (keyId && path === `/api/keys/${encodeURIComponent(keyId)}/reveal` && request.method() === "POST") return;
      unexpectedWrites.push(`${request.method()} ${path}`);
    });
  }
  watch(page);
  await page.setViewportSize({ width: 1440, height: 1000 });
  await login(page, owner);
  const ownSession = await api(page, "GET", "/api/session");
  expect(ownSession.status).toBe(200);
  expect(object(object(ownSession.body).member).role, "dedicated QA owner").toBe("owner");
  try {
    usageBefore = await usageSnapshot(page);
    expect(usageBefore.count, "two original retained usage events").toBe(2);
    await page.goto(`${base}/#members`); await settle(page);
    await page.getByRole("button", { name: "创建账号", exact: true }).click();
    const create = page.getByRole("dialog", { name: "创建账号", exact: true });
    await create.getByLabel("邮箱", { exact: true }).fill(actor.email);
    await create.getByLabel("显示名", { exact: true }).fill(displayName);
    await secret(create.getByLabel("初始密码", { exact: true }), actor.password);
    await choose(page, "角色", "成员");
    const created = responseFor(page, "POST", "/api/members");
    await create.getByRole("button", { name: "创建账号", exact: true }).click();
    const createdResponse = await created;
    expect(createdResponse.status()).toBe(201);
    memberId = textField(object(await createdResponse.json()), "id");
    console.info("Management stage: dedicated member created");
    const creation = object(createdResponse.request().postDataJSON());
    expect(Object.keys(creation).sort()).toEqual(["display_name", "email", "password", "role"]);
    expect(creation.email === actor.email && creation.password === actor.password && creation.display_name === displayName && creation.role === "member", "UI entered member fields are exact").toBe(true);
    await expect(create).toHaveCount(0);
    const memberPath = `/api/members/${encodeURIComponent(memberId)}`;
    const memberUrl = `${base}/#members?member=${encodeURIComponent(memberId)}`;
    await page.goto(memberUrl); await settle(page);
    const memberInfo = page.getByRole("region", { name: "成员信息", exact: true });
    for (const [value, label] of [["admin", "管理员"], ["member", "成员"]] as const) {
      await choose(page, "角色", label);
      await memberInfo.getByRole("button", { name: "更改角色", exact: true }).click();
      const saved = responseFor(page, "PATCH", memberPath);
      await memberInfo.getByRole("button", { name: "确认更改角色", exact: true }).click();
      const savedResponse = await saved;
      expect(savedResponse.status()).toBe(200);
      expect(savedResponse.request().postDataJSON()).toEqual({ role: value });
      expect((await members(page)).find(item => item.id === memberId)?.role).toBe(value);
      await page.reload(); await settle(page);
      await expect(memberInfo.getByRole("combobox", { name: "角色", exact: true })).toContainText(label);
    }
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 1000 });
      await capture(page, `tenant-management-member-detail-${width}`);
    }
    console.info("Management stage: member roles persisted and responsive detail captured");
    const oldContext = await browser.newContext(); contexts.push(oldContext);
    const oldPage = await oldContext.newPage(); watch(oldPage); await login(oldPage, actor);
    await page.getByRole("button", { name: "重置密码", exact: true }).click();
    const reset = page.getByRole("dialog", { name: "重置密码", exact: true });
    const password = reset.getByLabel("新密码", { exact: true });
    const confirmation = reset.getByLabel("再次输入新密码", { exact: true });
    await secret(password, "short"); await secret(confirmation, "different");
    await reset.getByRole("button", { name: "重置密码", exact: true }).click();
    await reset.getByRole("button", { name: "确认重置密码", exact: true }).click();
    await expect(password).toHaveAttribute("aria-invalid", "true");
    await expect(confirmation).toHaveAttribute("aria-invalid", "true");
    expect(passwordWrites, "invalid reset never reaches the API").toBe(0);
    await secret(password, nextPassword); await secret(confirmation, "different");
    await reset.getByRole("button", { name: "重置密码", exact: true }).click();
    await reset.getByRole("button", { name: "确认重置密码", exact: true }).click();
    await expect(password).not.toHaveAttribute("aria-invalid", "true");
    await expect(confirmation).toHaveAttribute("aria-invalid", "true");
    await expect(reset.getByText("两次输入的密码不一致", { exact: true })).toBeVisible();
    expect(passwordWrites, "confirmation mismatch never reaches the API").toBe(0);
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 1000 });
      await capture(page, `tenant-management-reset-errors-${width}`);
    }
    await secret(confirmation, nextPassword);
    await reset.getByRole("button", { name: "重置密码", exact: true }).click();
    const passwordReset = responseFor(page, "POST", `${memberPath}/password`);
    await reset.getByRole("button", { name: "确认重置密码", exact: true }).click();
    const resetResponse = await passwordReset;
    expect(resetResponse.status()).toBe(204);
    const resetBody = object(resetResponse.request().postDataJSON());
    expect(Object.keys(resetBody)).toEqual(["new_password"]);
    expect(resetBody.new_password === nextPassword, "new password is exact without exposing it").toBe(true);
    expect(passwordWrites).toBe(1);
    await expect(reset).toHaveCount(0);
    console.info("Management stage: password validation and real reset completed");
    expect((await api(oldPage, "GET", "/api/org")).status, "reset revokes existing dedicated-member session").toBe(401);
    expect((await api(oldPage, "POST", "/api/login", { email: actor.email, password: actor.password })).status, "old password is invalid").toBe(401);
    const freshContext = await browser.newContext(); contexts.push(freshContext);
    const freshPage = await freshContext.newPage(); watch(freshPage);
    await login(freshPage, { email: actor.email, password: nextPassword });
    const freshSession = await api(freshPage, "GET", "/api/session");
    expect(freshSession.status).toBe(200);
    const freshMember = object(object(freshSession.body).member);
    expect(freshMember.id).toBe(memberId); expect(freshMember.role).toBe("member");
    await freshPage.setViewportSize({ width: 1440, height: 1000 });
    await expect(freshPage.getByRole("navigation", { name: "组织门户分区", exact: true }).getByRole("link", { name: "成员", exact: true })).toHaveCount(0);
    expect((await api(freshPage, "GET", "/api/members")).status, "member cannot read management API").toBe(403);
    console.info("Management stage: fresh member login and permissions verified");

    await page.setViewportSize({ width: 1440, height: 1000 });
    const groups = await api(page, "GET", "/api/groups"); expect(groups.status).toBe(200);
    const groupRows = object(groups.body).items;
    if (!Array.isArray(groupRows) || groupRows.length === 0) throw new Error("A real granted group is required");
    const selectedGroup = object(groupRows[0]);
    const groupId = textField(selectedGroup, "id");
    const groupName = textField(selectedGroup, "display_name");
    await page.goto(`${base}/#keys`); await settle(page);
    await page.getByRole("button", { name: "新建密钥", exact: true }).click();
    const createKey = page.getByRole("dialog", { name: "新建密钥", exact: true });
    await createKey.getByLabel("名称", { exact: true }).fill(keyLabel);
    await createKey.getByRole("radio", { name: groupName, exact: false }).check();
    await createKey.getByRole("radio", { name: "不限制模型（继承组织授权）", exact: true }).check();
    await createKey.getByLabel("并发", { exact: true }).fill("1");
    await createKey.getByLabel("每分钟请求", { exact: true }).fill("2");
    const keyCreated = responseFor(page, "POST", "/api/keys");
    await createKey.getByRole("button", { name: "新建密钥", exact: true }).click();
    const keyCreatedResponse = await keyCreated;
    expect(keyCreatedResponse.status()).toBe(201);
    // 不比较或保存创建响应中的 plaintext，只保留公开资源字段。
    const keyCreationResult = object(await keyCreatedResponse.json());
    const createdKey = object(keyCreationResult.key);
    keyId = textField(createdKey, "id");
    mintedPlaintext = textField(keyCreationResult, "plaintext");
    console.info("Management stage: dedicated key created");
    const keyCreateBody = object(keyCreatedResponse.request().postDataJSON());
    expect(Object.hasOwn(keyCreateBody, "group_id")).toBe(true);
    expect(keyCreateBody.group_id).toBe(groupId);
    expect(keyCreateBody.label).toBe(keyLabel);
    expect(keyCreateBody.rate_limit).toEqual({ concurrency: 1, rpm: 2 });
    expect(Object.hasOwn(keyCreateBody, "model_ids"), "inherited scope is not converted to an empty allow-list").toBe(false);
    const createdKeyDialog = page.getByRole("dialog", { name: "密钥已创建", exact: true });
    await expect(createdKeyDialog).toBeVisible();
    await noRawKeyDom(page, mintedPlaintext);
    await capture(page, "tenant-management-created-key-masked-1440");
    await page.setViewportSize({ width: 390, height: 1000 });
    await capture(page, "tenant-management-created-key-masked-390");
    await page.setViewportSize({ width: 1440, height: 1000 });
    const cdp = await page.context().newCDPSession(page);
    const target = await cdp.send("Target.getTargetInfo");
    if (!target.targetInfo.browserContextId) throw new Error("Dedicated browser context identity is required");
    clipboardControl = { cdp, browserContextId: target.targetInfo.browserContextId };
    await page.context().grantPermissions(["clipboard-read"], { origin: origin.origin });
    await page.bringToFront();
    await clipboardWritePermission(cdp, clipboardControl.browserContextId, "granted");
    const createdCopy = createdKeyDialog.locator(".secret-acts button").first();
    await expect(createdCopy).toHaveText("");
    await createdCopy.click();
    await expect(createdCopy).toHaveAttribute("aria-label", "已复制");
    await clipboardEquals(page, mintedPlaintext);
    await noRawKeyDom(page, mintedPlaintext);
    await clipboardWritePermission(cdp, clipboardControl.browserContextId, "denied");
    expect(await page.evaluate(async () => (await navigator.permissions.query({ name: "clipboard-write" as PermissionName })).state)).toBe("denied");
    await createdCopy.click();
    await expect(createdKeyDialog.getByText("无法写入剪贴板", { exact: false })).toBeVisible();
    await noRawKeyDom(page, mintedPlaintext);
    await clipboardWritePermission(cdp, clipboardControl.browserContextId, "granted");
    await page.evaluate(() => navigator.clipboard.writeText("ui448-retry-baseline"));
    await createdCopy.click();
    await expect(createdCopy).toHaveAttribute("aria-label", "已复制");
    await clipboardEquals(page, mintedPlaintext);
    await noRawKeyDom(page, mintedPlaintext);
    await createdKeyDialog.getByRole("button", { name: "我已保存", exact: true }).click();
    await expect(createdKeyDialog).toHaveCount(0);
    console.info("Management stage: secret result dismissed");
    const ownKeyRow = page.getByRole("table", { name: "密钥列表", exact: true }).getByRole("row").filter({ has: page.getByRole("link", { name: keyLabel, exact: true }) });
    await expect(ownKeyRow).toHaveCount(1);
    const listedCopy = ownKeyRow.getByRole("button", { name: /^(复制密钥|已复制)$/ });
    await expect(listedCopy).toHaveText("");
    await clipboardWritePermission(cdp, clipboardControl.browserContextId, "denied");
    await listedCopy.click();
    await expect(ownKeyRow.getByText("无法写入剪贴板", { exact: false })).toBeVisible();
    await noRawKeyDom(page, mintedPlaintext);
    await clipboardWritePermission(cdp, clipboardControl.browserContextId, "granted");
    await page.evaluate(() => navigator.clipboard.writeText("ui448-retry-baseline"));
    await listedCopy.click();
    await expect(listedCopy).toHaveAttribute("aria-label", "已复制");
    await clipboardEquals(page, mintedPlaintext);
    await noRawKeyDom(page, mintedPlaintext);
    console.info("Management stage: native clipboard denial and retry keep all key surfaces masked");
    const keyPath = `/api/keys/${encodeURIComponent(keyId)}`;
    const beforeEdit = await storedKey(page, keyId);
    expect(object(beforeEdit.group).id).toBe(groupId);
    expect(Object.hasOwn(beforeEdit, "model_ids")).toBe(true); expect(beforeEdit.model_ids).toBeNull();
    const keyEditUrl = `${base}/#keys?key=${encodeURIComponent(keyId)}&edit=1`;
    await page.goto(keyEditUrl); await settle(page);
    await page.getByLabel("名称", { exact: true }).fill(editedLabel);
    await page.getByLabel("并发", { exact: true }).fill("2");
    await page.getByLabel("每分钟请求", { exact: true }).fill("3");
    const keySaved = responseFor(page, "PATCH", keyPath);
    await page.getByRole("button", { name: "保存修改", exact: true }).click();
    const keySavedResponse = await keySaved;
    expect(keySavedResponse.status()).toBe(200);
    const patch = object(keySavedResponse.request().postDataJSON());
    expect(Object.keys(patch).sort()).toEqual(["label", "rate_limit"]);
    expect(patch.label).toBe(editedLabel);
    const limits = object(patch.rate_limit);
    expect(Object.keys(limits).sort()).toEqual(["concurrency", "rpm"]);
    expect(limits.concurrency).toBe(2); expect(limits.rpm).toBe(3);
    expect(Object.hasOwn(beforeEdit, "version")).toBe(true);
    expect(keySavedResponse.request().headers()["if-match"]).toBe(`"${beforeEdit.version}"`);
    const savedKey = await storedKey(page, keyId);
    expect(savedKey.label).toBe(editedLabel); expect(savedKey.rate_limit).toEqual({ concurrency: 2, rpm: 3 });
    expect(object(savedKey.group).id).toBe(groupId); expect(savedKey.model_ids).toBeNull();
    console.info("Management stage: complete key edit persisted");
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 1000 });
      await page.goto(keyEditUrl); await settle(page);
      await expect(page.getByLabel("名称", { exact: true })).toHaveValue(editedLabel);
      await expect(page.getByLabel("并发", { exact: true })).toHaveValue("2");
      await expect(page.getByLabel("每分钟请求", { exact: true })).toHaveValue("3");
      await expect(page.getByRole("radio", { name: "不限制模型（继承组织授权）", exact: true })).toBeChecked();
      await noRawKeyDom(page, mintedPlaintext);
      await capture(page, `tenant-management-key-edit-${width}`);
    }
    expect(runtime, "no client runtime errors").toEqual([]);
    expect(unexpectedWrites, "only dedicated management writes, no inference").toEqual([]);
    journeyComplete = true;
  } finally {
    await clearSecretControls(page);
    if (clipboardControl) {
      try {
        await clipboardWritePermission(clipboardControl.cdp, clipboardControl.browserContextId, "granted");
        await page.evaluate(async () => { await navigator.clipboard.writeText(""); });
        await clipboardControl.cdp.send("Browser.resetPermissions", { browserContextId: clipboardControl.browserContextId });
        await clipboardControl.cdp.detach();
      } catch { cleanup.errors.push("Dedicated clipboard permission restoration failed"); }
    }
    mintedPlaintext = null;
    await Promise.allSettled(contexts.map(context => context.close()));
    // 失败发生在响应解码前时，仍只按本次不可碰撞的身份恢复精确 ID。
    try {
      if (!keyId && createKeySent) {
        const listed = await api(page, "GET", "/api/keys");
        expect(listed.status, "dedicated key recovery list").toBe(200);
        const items = object(listed.body).items;
        if (!Array.isArray(items)) throw new Error("Dedicated key recovery needs explicit items");
        const matches = items.map(object).filter(item => item.label === keyLabel || item.label === editedLabel);
        if (matches.length === 1) keyId = textField(matches[0]!, "id");
        else if (matches.length > 1) throw new Error("Dedicated key cleanup identity is ambiguous");
      }
      if (keyId) {
        const current = await storedKey(page, keyId);
        if (![keyLabel, editedLabel].includes(textField(current, "label"))) throw new Error("Refusing to clean up a key outside this journey");
        if (!Object.hasOwn(current, "version") || typeof current.version !== "number" || !Number.isSafeInteger(current.version)) throw new Error("Fresh key cleanup version is required");
        cleanup.keyStatus = (await api(page, "DELETE", `/api/keys/${encodeURIComponent(keyId)}`, undefined, current.version)).status;
        expect(cleanup.keyStatus, "delete only dedicated key with fresh If-Match").toBe(204);
      }
    } catch { cleanup.errors.push("Dedicated key cleanup failed"); }
    try {
      if (!memberId && createMemberSent) {
        const matches = (await members(page)).filter(item => item.email === actor.email && item.display_name === displayName);
        if (matches.length === 1) memberId = textField(matches[0]!, "id");
        else if (matches.length > 1) throw new Error("Dedicated member cleanup identity is ambiguous");
      }
      if (memberId) {
        const target = (await members(page)).find(item => item.id === memberId);
        if (!target || target.email !== actor.email || target.display_name !== displayName) throw new Error("Refusing to clean up a member outside this journey");
        cleanup.memberStatus = (await api(page, "DELETE", `/api/members/${encodeURIComponent(memberId)}`)).status;
        expect(cleanup.memberStatus, "delete only dedicated member").toBe(204);
      }
    } catch { cleanup.errors.push("Dedicated member cleanup failed"); }
    let usageAfter: Awaited<ReturnType<typeof usageSnapshot>> | null = null;
    try {
      usageAfter = await usageSnapshot(page);
      expect(usageAfter, "original events and stable full rows remain byte-equivalent; no added usage").toEqual(usageBefore);
    } catch { cleanup.errors.push("Retained usage identity and row hash verification failed"); }
    mkdirSync(output, { recursive: true });
    writeFileSync(join(output, "management-journey-receipt.json"), JSON.stringify({ memberId, keyId, journeyComplete, cleanup, usageBefore, usageAfter, unexpectedWrites }, null, 2), { mode: 0o600 });
    expect(cleanup.errors, "both dedicated records cleaned and original usage unchanged").toEqual([]);
  }
});
