import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type ApiClient, type Json, type Stack } from "./stack";

/**
 * The Keys tab, the operator's side of API keys: minting (the plaintext shown
 * once), the operator's switch, revocation and the per-key group override.
 * Against the real stack; what a tenant does goes through the real `/api`
 * (ruling 10), and every key is judged by the real gateway.
 *
 * Keys are minted only on approved tenants, so these hold whether or not the
 * control plane refuses a key for a pending tenant.
 */

let stack: Stack;
const models: Record<string, Json> = {};

const PASSWORD = "t6-e2e-pass-3b8d";
/** How long a switch on the control plane may take to reach the gateway. */
const ROLLOUT_BOUND_MS = 15_000;
/** How long a newly minted key may take to reach the gateway. */
const KEY_READY_MS = 30_000;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  const api_base = `${stack.mockUpstreamUrl}/v1`;
  const account = (display_name: string, groups: string[], api_key: string) =>
    stack.admin.post("/admin/resources/provider_keys", {
      doc: { display_name, provider: "openai", api_key, api_base, groups },
    });
  // Every account a tenant key reaches by default is in `shared`.
  const shared = await account("acct-t6-shared", ["shared"], "sk-t6-upstream-shared-1");
  const alpha = await account("acct-t6-alpha", ["grp-alpha-7c1"], "sk-t6-upstream-alpha-2");
  const direct = (display_name: string, model_name: string, provider_key_id: string) =>
    stack.admin.post("/admin/resources/models", {
      doc: { display_name, provider: "openai", model_name, provider_key_id },
    });
  models.chat = await direct("chat-t6-e2e", "gpt-4o-mini", shared.id);
  models.other = await direct("other-t6-e2e", "gpt-4o", shared.id);
  // Served only by an account in grp-alpha-7c1.
  models.alpha = await direct("alpha-t6-e2e", "gpt-4.1-mini", alpha.id);
});

test.afterAll(async () => {
  await stack?.stop();
});

interface TenantSide {
  /** Signed in as the tenant's owner. */
  client: ApiClient;
  id: string;
  org: string;
  email: string;
}

/** A company signed up, approved and granted `grants`, all through the APIs. */
async function signUpActive(org: string, email: string, grants: Json[] = []): Promise<TenantSide> {
  const client = stack.tenant();
  const body = await client.post("/api/signup", { org_name: org, email, password: PASSWORD });
  const id: string = body.tenant.id;
  await stack.admin.post(`/admin/tenants/${id}/approve`);
  if (grants.length > 0) {
    const current = await stack.admin.get(`/admin/tenants/${id}`);
    await stack.admin.patch(`/admin/tenants/${id}`, { allowed_model_ids: grants.map((m) => m.id) }, current.version);
  }
  return { client, id, org, email };
}

/** A key the tenant's owner mints on `/api`: `{key, plaintext}`. */
function mint(t: TenantSide, label: string): Promise<Json> {
  return t.client.post("/api/keys", { label });
}

/** The status a chat through the gateway with `key` for `model` gets. */
async function chatStatus(key: string, model = "chat-t6-e2e"): Promise<number> {
  const res = await fetch(`${stack.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ model, messages: [{ role: "user", content: "hi" }] }),
  });
  await res.arrayBuffer();
  return res.status;
}

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令").fill(password);
  await page.getByRole("button", { name: "登录" }).click();
}

/**
 * Signed in as the stack's admin, on the Keys tab, with `org`'s keys shown
 * when given. The tenant picker is found by a substring of its label: the
 * label holds the select, so its text takes in the options' too.
 */
async function openKeys(page: Page, org?: string): Promise<void> {
  await login(page, stack.operator.email, stack.operator.password);
  await page.locator('[data-tab="keys"]').click();
  if (org) await page.getByLabel("租户").selectOption({ label: org });
}

const keyTable = (page: Page) => page.getByRole("table", { name: "密钥列表" });
const keyRow = (page: Page, label: string) => keyTable(page).getByRole("row").filter({ hasText: label });

test("新建内部密钥：明文只显示一次，用它能调网关", async ({ page }) => {
  const logged: string[] = [];
  page.on("console", (m) => logged.push(m.text()));
  // A customer whose name sorts before the platform's own tenant, so that
  // "first" says something about the order.
  const internal = ((await stack.admin.get("/admin/tenants")).items as Json[]).find((t) => t.is_internal);
  const early = "0 T6 Early Co 2b7";
  expect(early.localeCompare(internal.name)).toBeLessThan(0);
  await signUpActive(early, "owner@early-t6.test");

  await openKeys(page);
  // The platform's own tenant comes first and is the one shown.
  const picker = page.getByLabel("租户");
  await expect(picker.locator("option")).not.toHaveCount(1);
  await expect(picker.locator("option").first()).toContainText("平台内部");
  expect(await picker.inputValue()).toBe(await picker.locator("option").first().getAttribute("value"));

  await page.getByRole("button", { name: "新建密钥" }).click();
  const form = page.getByRole("group", { name: "新建密钥" });
  await form.getByLabel("名称").fill("ops-key-t6");
  await form.getByLabel("模型（可写通配）").fill("chat-t6-*");
  await form.getByRole("button", { name: "创建" }).click();

  const notice = page.getByText("明文只显示这一次");
  await expect(notice).toBeVisible();
  await expect(page.getByRole("button", { name: "复制" })).toBeVisible();
  const shown = page.getByLabel("密钥明文");
  const plaintext = ((await shown.textContent()) ?? "").trim();
  expect(plaintext).toMatch(/^sk-aisix-\S{16,}$/);

  // The key the page showed is the real one: the gateway serves it, for the
  // models its pattern names only.
  await expect.poll(() => chatStatus(plaintext), { timeout: KEY_READY_MS }).toBe(200);
  expect(await chatStatus(plaintext, "other-t6-e2e")).not.toBe(200);

  // Shown, and kept nowhere else.
  expect(page.url()).not.toContain(plaintext);
  const stored = await page.evaluate(() =>
    JSON.stringify([Object.entries(localStorage), Object.entries(sessionStorage)]),
  );
  expect(stored).not.toContain(plaintext);
  const row = keyRow(page, "ops-key-t6");
  await expect(row).toContainText(`${plaintext.slice(0, 12)}…`);
  await expect(row).not.toContainText(plaintext);

  // Away to another tab and back: the plaintext is gone.
  await page.locator('[data-tab="tenants"]').click();
  await page.locator('[data-tab="keys"]').click();
  await expect(keyRow(page, "ops-key-t6")).toContainText(`${plaintext.slice(0, 12)}…`);
  await expect(page.getByLabel("密钥明文")).toHaveCount(0);
  expect(await page.content()).not.toContain(plaintext);

  // After a reload: the list shows the prefix only, and the plaintext is gone.
  await page.reload();
  await page.locator('[data-tab="keys"]').click();
  await expect(keyRow(page, "ops-key-t6")).toContainText(`${plaintext.slice(0, 12)}…`);
  expect(await page.content()).not.toContain(plaintext);
  expect(logged.join("\n")).not.toContain(plaintext);
});

test("停用后网关返回 401，启用后恢复", async ({ page }) => {
  const t = await signUpActive("T6 Pause Co 2a9", "owner@pause-t6.test", [models.chat]);
  const { key, plaintext } = await mint(t, "pause-key-t6");
  await expect.poll(() => chatStatus(plaintext), { timeout: KEY_READY_MS }).toBe(200);

  await openKeys(page, t.org);
  const row = keyRow(page, "pause-key-t6");
  await expect(row).toContainText("正常");
  await row.getByRole("button", { name: "停用" }).click();
  // Nothing is switched off until it is confirmed.
  const confirm = row.getByRole("button", { name: "确认停用" });
  await expect(confirm).toBeVisible();
  expect((await stack.admin.get(`/admin/keys/${key.id}`)).disabled).toBe(false);
  await confirm.click();
  await expect(row).toContainText("已停用");
  await expect.poll(() => chatStatus(plaintext), { timeout: ROLLOUT_BOUND_MS }).toBe(401);

  // The tenant sees the platform's switch on its key.
  expect((await t.client.get(`/api/keys/${key.id}`)).disabled_by_platform).toBe(true);

  // After a reload the page shows what the control plane holds: still off.
  await page.reload();
  await page.locator('[data-tab="keys"]').click();
  await page.getByLabel("租户").selectOption({ label: t.org });
  await expect(row).toContainText("已停用");
  await row.getByRole("button", { name: "启用" }).click();
  await expect(row).toContainText("正常");
  await expect(row).not.toContainText("已停用");
  await expect.poll(() => chatStatus(plaintext), { timeout: ROLLOUT_BOUND_MS }).toBe(200);
});

test("租户也停用了的密钥：运维停用后租户不能自行重新启用，运维启用后仍按租户的开关", async ({ page }) => {
  const t = await signUpActive("T6 Sticky Co 3d4", "owner@sticky-t6.test", [models.chat]);
  const { key, plaintext } = await mint(t, "sticky-key-t6");
  await expect.poll(() => chatStatus(plaintext), { timeout: KEY_READY_MS }).toBe(200);
  /** The tenant's own switch, set on `/api` against the key's current version. */
  const tenantSwitch = async (disabled: boolean) => {
    const own = await t.client.request("GET", `/api/keys/${key.id}`);
    return t.client.request("PATCH", `/api/keys/${key.id}`, { disabled }, own.version!);
  };
  // The tenant switches its key off first.
  const off = await tenantSwitch(true);
  expect(off.status, off.text).toBe(200);
  await expect.poll(() => chatStatus(plaintext), { timeout: ROLLOUT_BOUND_MS }).toBe(401);

  await openKeys(page, t.org);
  const row = keyRow(page, "sticky-key-t6");
  await expect(row).toContainText("租户自行停用");
  await row.getByRole("button", { name: "停用" }).click();
  await row.getByRole("button", { name: "确认停用" }).click();
  await expect(row).toContainText("运维停用");

  // The operator's switch sticks: the tenant cannot switch the key back on.
  const refused = await tenantSwitch(false);
  expect(refused.status, refused.text).toBe(403);
  let stored = await stack.admin.get(`/admin/keys/${key.id}`);
  expect([stored.disabled, stored.disabled_by_tenant, stored.effective_disabled]).toEqual([true, true, true]);
  expect(await chatStatus(plaintext)).toBe(401);

  // 启用 clears the operator's switch only: the tenant's stays off.
  await row.getByRole("button", { name: "启用" }).click();
  await expect(row).not.toContainText("运维停用");
  await expect(row).toContainText("租户自行停用");
  stored = await stack.admin.get(`/admin/keys/${key.id}`);
  expect([stored.disabled, stored.disabled_by_tenant, stored.effective_disabled]).toEqual([false, true, true]);
  // Now the tenant may switch it back on.
  const on = await tenantSwitch(false);
  expect(on.status, on.text).toBe(200);
  await expect.poll(() => chatStatus(plaintext), { timeout: ROLLOUT_BOUND_MS }).toBe(200);
});

test("吊销后它从列表消失，网关返回 401", async ({ page }) => {
  const t = await signUpActive("T6 Revoke Co 5c1", "owner@revoke-t6.test", [models.chat]);
  const { key, plaintext } = await mint(t, "revoke-key-t6");
  await mint(t, "keep-key-t6");
  await expect.poll(() => chatStatus(plaintext), { timeout: KEY_READY_MS }).toBe(200);

  await openKeys(page, t.org);
  const row = keyRow(page, "revoke-key-t6");
  await row.getByRole("button", { name: "吊销" }).click();
  // Nothing is revoked until it is confirmed.
  const confirm = row.getByRole("button", { name: "确认吊销" });
  await expect(confirm).toBeVisible();
  expect((await stack.admin.request("GET", `/admin/keys/${key.id}`)).status).toBe(200);
  await confirm.click();

  await expect(row).toHaveCount(0);
  await expect(keyRow(page, "keep-key-t6")).toHaveCount(1);
  await expect.poll(() => chatStatus(plaintext), { timeout: ROLLOUT_BOUND_MS }).toBe(401);
  expect((await stack.admin.request("GET", `/admin/keys/${key.id}`)).status).toBe(404);
  const listed = (await t.client.get("/api/keys")).items as Json[];
  expect(listed.map((k) => k.label)).toEqual(["keep-key-t6"]);
});

test("给租户 key 设置分组覆盖 grp-alpha-7c1：运维侧显示，租户的 /api/keys 不含该分组", async ({ page }) => {
  const t = await signUpActive("T6 Alpha Co 7e4", "owner@alpha-t6.test", [models.chat, models.alpha]);
  const { key, plaintext } = await mint(t, "alpha-key-t6");
  await expect.poll(() => chatStatus(plaintext), { timeout: KEY_READY_MS }).toBe(200);
  // The tenant's groups do not reach the alpha account.
  expect(await chatStatus(plaintext, "alpha-t6-e2e")).not.toBe(200);

  await openKeys(page, t.org);
  const row = keyRow(page, "alpha-key-t6");
  await expect(row).toContainText("shared");
  await expect(row).not.toContainText("grp-alpha-7c1");
  await row.getByRole("button", { name: "分组覆盖" }).click();
  const editor = keyTable(page).getByRole("group", { name: "分组覆盖" });
  await editor.getByRole("checkbox", { name: "grp-alpha-7c1" }).check();
  // grp-alpha-7c1 holds an upstream account, so the save asks for no confirmation.
  await editor.getByRole("button", { name: "保存覆盖" }).click();

  await expect(row).toContainText("grp-alpha-7c1");
  expect((await stack.admin.get(`/admin/keys/${key.id}`)).effective_provider_groups).toEqual(["grp-alpha-7c1"]);
  await expect.poll(() => chatStatus(plaintext, "alpha-t6-e2e"), { timeout: ROLLOUT_BOUND_MS }).toBe(200);
  for (const path of ["/api/keys", `/api/keys/${key.id}`]) {
    const answer = await t.client.request("GET", path);
    expect(answer.status, `${path}: ${answer.text}`).toBe(200);
    expect(answer.text, path).toContain("alpha-key-t6");
    expect(answer.text, path).not.toContain("grp-alpha-7c1");
  }
});

test("给客户租户新建密钥：只能调用所选模型，限额与过期时间按所填保存", async ({ page }) => {
  const t = await signUpActive("T6 Scoped Co 4d6", "owner@scoped-t6.test", [models.chat, models.other]);

  await openKeys(page, t.org);
  await page.getByRole("button", { name: "新建密钥" }).click();
  const form = page.getByRole("group", { name: "新建密钥" });
  const choices = form.getByRole("group", { name: "模型", exact: true });
  // Only the tenant's grants are offered.
  await expect(choices.getByRole("checkbox", { name: "other-t6-e2e" })).toBeVisible();
  await expect(choices.getByRole("checkbox", { name: "alpha-t6-e2e" })).toHaveCount(0);
  await form.getByLabel("名称").fill("scoped-key-t6");
  await choices.getByRole("checkbox", { name: "chat-t6-e2e" }).check();
  await form.getByLabel("并发").fill("2");
  await form.getByLabel("每分钟请求").fill("30");
  await form.getByLabel("过期时间").fill("2031-01-02T03:04");
  await form.getByRole("button", { name: "创建" }).click();

  const plaintext = ((await page.getByLabel("密钥明文").textContent()) ?? "").trim();
  expect(plaintext).toMatch(/^sk-aisix-\S{16,}$/);
  await expect.poll(() => chatStatus(plaintext), { timeout: KEY_READY_MS }).toBe(200);
  expect(await chatStatus(plaintext, "other-t6-e2e")).not.toBe(200);

  // Closed: gone from the page.
  await page.getByRole("button", { name: "关闭" }).click();
  await expect(page.getByLabel("密钥明文")).toHaveCount(0);
  expect(await page.content()).not.toContain(plaintext);

  const row = keyRow(page, "scoped-key-t6");
  await expect(row).toContainText("chat-t6-e2e");
  await expect(row).not.toContainText("other-t6-e2e");
  const listed = (await stack.admin.get(`/admin/tenants/${t.id}/keys`)).items as Json[];
  const stored = listed.find((k) => k.label === "scoped-key-t6");
  expect(stored.model_ids).toEqual([models.chat.id]);
  expect(stored.rate_limit).toEqual({ concurrency: 2, rpm: 30 });
  expect(new Date(stored.expires_at).getTime()).toBe(new Date("2031-01-02T03:04").getTime());
  expect(stored.created_by.kind).toBe("operator");
});

test("分组覆盖只选没有上游账号的分组时，先警告并要求确认", async ({ page }) => {
  const t = await signUpActive("T6 Unreach Co 8b3", "owner@unreach-t6.test", [models.chat]);
  const current = await stack.admin.request("GET", `/admin/tenants/${t.id}`);
  // A group no upstream account is in, beside the default one.
  await stack.admin.patch(`/admin/tenants/${t.id}`, { allowed_provider_groups: ["shared", "grp-none-6b2"] }, current.version!);
  const { key } = await mint(t, "unreach-key-t6");

  await openKeys(page, t.org);
  const row = keyRow(page, "unreach-key-t6");
  await row.getByRole("button", { name: "分组覆盖" }).click();
  const editor = keyTable(page).getByRole("group", { name: "分组覆盖" });
  await expect(editor.getByRole("listitem").filter({ hasText: "grp-none-6b2" })).toContainText("没有上游账号在此分组");
  await editor.getByRole("checkbox", { name: "grp-none-6b2" }).check();
  await editor.getByRole("button", { name: "保存覆盖" }).click();
  await expect(editor.getByRole("alert")).toContainText("grp-none-6b2");
  // Nothing is saved until it is confirmed.
  expect((await stack.admin.get(`/admin/keys/${key.id}`)).allowed_provider_groups).toBeNull();
  await editor.getByRole("button", { name: "确认保存覆盖" }).click();
  await expect
    .poll(async () => (await stack.admin.get(`/admin/keys/${key.id}`)).allowed_provider_groups)
    .toEqual(["grp-none-6b2"]);
});

test("分组覆盖里只要有一个分组带上游账号，保存就不需确认", async ({ page }) => {
  const t = await signUpActive("T6 Mixed Co 6f1", "owner@mixed-t6.test", [models.chat]);
  const current = await stack.admin.request("GET", `/admin/tenants/${t.id}`);
  await stack.admin.patch(`/admin/tenants/${t.id}`, { allowed_provider_groups: ["shared", "grp-none-4c8"] }, current.version!);
  const { key } = await mint(t, "mixed-key-t6");

  await openKeys(page, t.org);
  await keyRow(page, "mixed-key-t6").getByRole("button", { name: "分组覆盖" }).click();
  const editor = keyTable(page).getByRole("group", { name: "分组覆盖" });
  await expect(editor.getByRole("listitem").filter({ hasText: "grp-none-4c8" })).toContainText("没有上游账号在此分组");
  await editor.getByRole("checkbox", { name: "shared" }).check();
  await editor.getByRole("checkbox", { name: "grp-none-4c8" }).check();
  // shared holds an upstream account, so one click saves.
  await editor.getByRole("button", { name: "保存覆盖" }).click();
  await expect
    .poll(async () => [...((await stack.admin.get(`/admin/keys/${key.id}`)).allowed_provider_groups ?? [])].sort())
    .toEqual(["grp-none-4c8", "shared"]);
  await expect(keyTable(page).getByRole("button", { name: "确认保存覆盖" })).toHaveCount(0);
});

test("成员被改为计费角色后，他的密钥在网关上被拒，密钥页说明原因", async ({ page }) => {
  const t = await signUpActive("T6 Billing Co 5e9", "owner@billing-t6.test", [models.chat]);
  const join = async (email: string, role: string) => {
    const invite = await t.client.post("/api/members/invites", { email, role });
    return (await stack.tenant().post("/api/invites/accept", { token: invite.invite_token, password: PASSWORD })).member;
  };
  // A billing member cannot be given a key at all.
  const ledger = await join("ledger@billing-t6.test", "billing");
  const forLedger = await t.client.request("POST", "/api/keys", { label: "ledger-key-t6", member_id: ledger.id });
  expect(forLedger.status, forLedger.text).toBeGreaterThanOrEqual(400);
  expect(forLedger.status, forLedger.text).toBeLessThan(500);
  // So the key goes to a member, who is then moved to billing.
  const payer = await join("payer@billing-t6.test", "member");
  const { plaintext } = await t.client.post("/api/keys", { label: "payer-key-t6", member_id: payer.id });
  await expect.poll(() => chatStatus(plaintext), { timeout: KEY_READY_MS }).toBe(200);
  await t.client.patch(`/api/members/${payer.id}`, { role: "billing" });
  await expect.poll(() => chatStatus(plaintext), { timeout: ROLLOUT_BOUND_MS }).toBe(401);

  await openKeys(page, t.org);
  const row = keyRow(page, "payer-key-t6");
  await expect(row).toContainText("payer@billing-t6.test");
  await expect(row).toContainText("已停用");
  await expect(row).toContainText("计费成员不持有可用的密钥");
});

test("密钥的分组覆盖指向租户已没有的分组时，在租户页和密钥页都标出", async ({ page }) => {
  const t = await signUpActive("T6 Stray Co 1f7", "owner@stray-t6.test", [models.chat]);
  let tenant = await stack.admin.request("GET", `/admin/tenants/${t.id}`);
  await stack.admin.patch(`/admin/tenants/${t.id}`, { allowed_provider_groups: ["shared", "grp-alpha-7c1"] }, tenant.version!);
  const stray = (await mint(t, "stray-key-t6")).key;
  const kept = (await mint(t, "kept-key-t6")).key;
  for (const [k, groups] of [
    [stray, ["grp-alpha-7c1"]],
    [kept, ["shared"]],
  ] as const) {
    const v = (await stack.admin.request("GET", `/admin/keys/${k.id}`)).version!;
    await stack.admin.patch(`/admin/keys/${k.id}`, { allowed_provider_groups: groups }, v);
  }
  // The tenant loses grp-alpha-7c1; the first key's override still names it.
  tenant = await stack.admin.request("GET", `/admin/tenants/${t.id}`);
  await stack.admin.patch(`/admin/tenants/${t.id}`, { allowed_provider_groups: ["shared"] }, tenant.version!);

  const flag = "不在租户的可用分组中";
  await openKeys(page, t.org);
  await expect(keyRow(page, "stray-key-t6")).toContainText(flag);
  await expect(keyRow(page, "kept-key-t6")).toContainText("shared");
  await expect(keyRow(page, "kept-key-t6")).not.toContainText(flag);

  await page.locator('[data-tab="tenants"]').click();
  await page
    .getByRole("table", { name: "租户列表" })
    .getByRole("row")
    .filter({ hasText: t.org })
    .getByRole("button", { name: t.org })
    .click();
  const tenantKeys = page.getByRole("table", { name: "密钥" });
  await expect(tenantKeys.getByRole("row").filter({ hasText: "stray-key-t6" })).toContainText(flag);
  await expect(tenantKeys.getByRole("row").filter({ hasText: "kept-key-t6" })).not.toContainText(flag);
});

test("两个运维同时操作同一密钥：后操作的被拒并提示重新载入，先操作的保留", async ({ page }) => {
  const t = await signUpActive("T6 Race Co 6a2", "owner@race-t6.test", [models.chat]);
  const { key } = await mint(t, "race-key-t6");

  await openKeys(page, t.org);
  const row = keyRow(page, "race-key-t6");
  await expect(row).toContainText("正常");
  // Another operator renames it first.
  const current = await stack.admin.request("GET", `/admin/keys/${key.id}`);
  await stack.admin.patch(`/admin/keys/${key.id}`, { label: "race-key-t6-renamed" }, current.version!);

  await row.getByRole("button", { name: "停用" }).click();
  await row.getByRole("button", { name: "确认停用" }).click();
  await expect(row.getByRole("alert")).toContainText("该资源已被他人修改");
  expect((await stack.admin.get(`/admin/keys/${key.id}`)).disabled).toBe(false);
  await row.getByRole("button", { name: "重新载入" }).click();
  await expect(keyRow(page, "race-key-t6-renamed")).toContainText("正常");
  await expect(keyTable(page).getByRole("alert")).toHaveCount(0);
  const stored = await stack.admin.get(`/admin/keys/${key.id}`);
  expect([stored.label, stored.disabled]).toEqual(["race-key-t6-renamed", false]);
});

test("分组覆盖打开期间另一位运维改了它：刷新后再保存被拒，另一位的修改保留", async ({ page }) => {
  const t = await signUpActive("T6 Override Race Co 2d8", "owner@orace-t6.test", [models.chat]);
  const { key } = await mint(t, "orace-key-t6");

  await openKeys(page, t.org);
  const row = keyRow(page, "orace-key-t6");
  await expect(row).toContainText("shared");
  await row.getByRole("button", { name: "分组覆盖" }).click();
  const editor = keyTable(page).getByRole("group", { name: "分组覆盖" });
  await editor.getByRole("checkbox", { name: "shared" }).check();
  // Meanwhile another operator gives the key an override of its own.
  const current = await stack.admin.request("GET", `/admin/keys/${key.id}`);
  await stack.admin.patch(`/admin/keys/${key.id}`, { allowed_provider_groups: ["grp-alpha-7c1"] }, current.version!);
  // The page reads the list again while the editor stays open.
  await page.getByRole("button", { name: "刷新" }).click();
  await expect(row).toContainText("grp-alpha-7c1");

  await editor.getByRole("button", { name: "保存覆盖" }).click();
  await expect(editor.getByRole("alert")).toContainText("该资源已被他人修改");
  expect((await stack.admin.get(`/admin/keys/${key.id}`)).allowed_provider_groups).toEqual(["grp-alpha-7c1"]);
});

test("待审核的租户在密钥页没有新建密钥", async ({ page }) => {
  const pending = "T6 Pending Co 7b3";
  await stack.tenant().post("/api/signup", { org_name: pending, email: "owner@pending-t6.test", password: PASSWORD });
  const active = await signUpActive("T6 Approved Co 7b4", "owner@approved-t6.test");

  await openKeys(page, active.org);
  await expect(page.getByRole("button", { name: "新建密钥" })).toBeVisible();
  const picker = page.getByLabel("租户");
  const option = await picker.locator("option", { hasText: pending }).getAttribute("value");
  await picker.selectOption(option!);
  await expect(page.getByRole("region", { name: new RegExp(pending) })).toBeVisible();
  await expect(page.getByRole("button", { name: "新建密钥" })).toHaveCount(0);
});

test("管理员被降为成员后，他代租户创建的密钥标出创建者并建议轮换", async ({ page }) => {
  const t = await signUpActive("T6 Demote Co 9c5", "owner@demote-t6.test", [models.chat]);
  const leadEmail = "lead@demote-t6.test";
  const invite = await t.client.post("/api/members/invites", { email: leadEmail, role: "admin" });
  const lead = stack.tenant();
  const accepted = await lead.post("/api/invites/accept", { token: invite.invite_token, password: PASSWORD });
  // Minted by the admin for the tenant as a whole, then the admin is demoted.
  await lead.post("/api/keys", { label: "lead-shared-key-t6", member_id: null });
  await mint(t, "owner-own-key-t6");
  await t.client.patch(`/api/members/${accepted.member.id}`, { role: "member" });

  await openKeys(page, t.org);
  const minted = keyRow(page, "lead-shared-key-t6");
  await expect(minted).toContainText(leadEmail);
  await expect(minted).toContainText("建议轮换");
  const owned = keyRow(page, "owner-own-key-t6");
  await expect(owned).toContainText(t.email);
  await expect(owned).not.toContainText("建议轮换");
});

test("只读账号在密钥页看不到任何操作", async ({ page }) => {
  const t = await signUpActive("T6 Viewer Co 3e8", "owner@viewer-t6.test", [models.chat]);
  await mint(t, "viewer-key-t6");
  const viewer = { email: "viewer-t6@e2e.local", password: "viewer-t6-pass-5a2" };
  await stack.admin.post("/admin/operators", { ...viewer, role: "viewer" });

  await login(page, viewer.email, viewer.password);
  await page.locator('[data-tab="keys"]').click();
  await page.getByLabel("租户").selectOption({ label: t.org });
  await expect(keyRow(page, "viewer-key-t6")).toContainText("正常");
  await expect(page.getByRole("button", { name: /新建|创建|停用|启用|吊销|覆盖/ })).toHaveCount(0);
});
