import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, fundTenant, startStack, type ApiClient, type Json, type Stack } from "./stack";

/**
 * The Tenants tab: approval, model grants, provider groups, limits, members,
 * operator invitations, the tenant's keys and the defaults new tenants start
 * with. Against the real stack; what a tenant does goes through the real
 * `/api` (ruling 10), and keys are judged by the real gateway.
 */

let stack: Stack;
const models: Record<string, Json> = {};
/** The account every tenant key reaches by default; it serves the public models below. */
let sharedAccount: Json;

const PASSWORD = "t5-e2e-pass-7c1a";
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
  // Every write that names a group names a live one, the accounts below and
  // the tenant and defaults edits in the specs included. `grp-ui-7e2` names a
  // display name different from its routing name, so a picker that offers the
  // routing name in its place is visible.
  for (const [name, display] of [
    ["grp-eu-5d2", "grp-eu-5d2"],
    ["grp-us-8a4", "grp-us-8a4"],
    ["grp-none-7d3", "grp-none-7d3"],
    ["grp-ui-7e2", "欧洲二区 7e2"],
  ] as const) {
    await stack.admin.createGroup(name, display);
  }
  // Every account a tenant key reaches by default is in `shared`.
  const shared = await account("acct-t5-shared", ["shared"], "sk-t5-upstream-shared-1");
  sharedAccount = shared;
  await account("acct-t5-eu", ["grp-eu-5d2"], "sk-t5-upstream-eu-2");
  await account("acct-t5-us", ["grp-us-8a4"], "sk-t5-upstream-us-3");
  const direct = (display_name: string, model_name: string) =>
    stack.admin.post("/admin/resources/models", {
      doc: { display_name, provider: "openai", model_name, provider_key_id: shared.id },
    });
  models.chat = await direct("chat-t5-e2e", "gpt-4o-mini");
  models.other = await direct("other-t5-e2e", "gpt-4o");
  // A wildcard row: it serves any model name under its prefix.
  models.wild = await direct("wild-t5-*", "*");
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

/** A company signing up on the real `/api`: pending until an operator approves it. */
async function signUp(org: string, email: string): Promise<TenantSide> {
  const client = stack.tenant();
  const body = await client.post("/api/signup", { org_name: org, email, password: PASSWORD });
  return { client, id: body.tenant.id, org, email };
}

/** A company signed up, approved and granted `grants`, all through the APIs. */
async function signUpActive(org: string, email: string, grants: Json[] = []): Promise<TenantSide> {
  const t = await signUp(org, email);
  await stack.admin.post(`/admin/tenants/${t.id}/approve`);
  await fundTenant(stack.admin, t.id);
  if (grants.length > 0) {
    const current = await stack.admin.get(`/admin/tenants/${t.id}`);
    // A hand-made model is outside the all-public-model scope every new
    // tenant starts on, so granting one is a move to the selected scope.
    await stack.admin.patch(
      `/admin/tenants/${t.id}`,
      { model_scope: "selected", allowed_model_ids: grants.map((m) => m.id) },
      current.version,
    );
  }
  return t;
}

/** The status a chat through the gateway with `key` gets. */
async function chatStatus(key: string): Promise<number> {
  const res = await fetch(`${stack.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ model: "chat-t5-e2e", messages: [{ role: "user", content: "hi" }] }),
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

/** What the group pickers label group `name` with: its display name, which a fresh database gives `shared` too. */
async function shownAs(name: string): Promise<string> {
  const { items } = await stack.admin.get("/admin/provider-groups");
  return (items as Json[]).find((g) => g.name === name).display_name as string;
}

async function openTenants(page: Page): Promise<void> {
  await login(page, stack.operator.email, stack.operator.password);
  await page.locator('[data-tab="tenants"]').click();
}

const tenantRow = (page: Page, org: string) =>
  page.getByRole("table", { name: "租户列表" }).getByRole("row").filter({ hasText: org });

async function openTenant(page: Page, org: string): Promise<void> {
  await tenantRow(page, org).getByRole("button", { name: org }).click();
}

test("经 /api 自助注册的租户出现在待审核列表，批准后其会话报告为正常", async ({ page }) => {
  const t = await signUp("T5 Approve Co 3a1", "owner@approve-t5.test");
  expect((await t.client.get("/api/session")).tenant.status).toBe("pending");

  await openTenants(page);
  await page.getByLabel("按状态筛选").selectOption({ label: "待审核" });
  const row = tenantRow(page, t.org);
  await expect(row).toContainText("待审核");
  // The filter leaves out the platform's own tenant, which is active.
  await expect(tenantRow(page, "平台内部")).toHaveCount(0);

  await row.getByRole("button", { name: "批准" }).click();
  await expect(row).toHaveCount(0);
  await page.getByLabel("按状态筛选").selectOption({ label: "全部" });
  await expect(tenantRow(page, t.org)).toContainText("正常");
  expect((await t.client.get("/api/session")).tenant.status).toBe("active");
});

test("授权一个模型后，租户的 /api/models 只列出它", async ({ page }) => {
  const t = await signUpActive("T5 Grant Co 8b2", "owner@grant-t5.test");
  // A new tenant starts on every public model; this stack has none, so it
  // reaches nothing until an operator selects one.
  expect((await t.client.get("/api/models")).items).toEqual([]);

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  await grants.getByRole("radio", { name: "指定模型" }).check();
  await grants.getByRole("checkbox", { name: "chat-t5-e2e" }).check();
  await grants.getByRole("button", { name: "保存授权" }).click();
  await expect
    .poll(async () => ((await t.client.get("/api/models")).items as Json[]).map((m) => m.name))
    .toEqual(["chat-t5-e2e"]);
});

test("给租户设置可用分组后，运维侧的 key 显示它的分组，/api 的任何响应都不含分组名", async ({ page }) => {
  const t = await signUpActive("T5 Groups Co 4c3", "owner@groups-t5.test", [models.chat]);
  await t.client.post("/api/keys", { label: "groups-key-t5" });

  await openTenants(page);
  await openTenant(page, t.org);
  const keyRow = page.getByRole("table", { name: "密钥" }).getByRole("row").filter({ hasText: "groups-key-t5" });
  // The key names the group its tenant has when it was minted; the tenant
  // shows groups by display name, with the routing name in small text.
  await expect(keyRow).toContainText("shared");
  const groups = page.getByRole("group", { name: "可用分组" });
  await expect(groups).toContainText("租户按显示名看到分组");
  await expect(groups.getByRole("checkbox", { name: "欧洲二区 7e2" })).toBeVisible();
  // `shared` is shown by its display name, 通用分组 on a fresh database.
  await groups.getByRole("checkbox", { name: "通用分组" }).uncheck();
  await groups.getByRole("checkbox", { name: "grp-eu-5d2" }).check();
  // The save stops the one key in `shared`, so it is confirmed first.
  await groups.getByRole("button", { name: "保存分组" }).click();
  await expect(groups.getByRole("alert")).toContainText("将停用该租户在此分组下的 1 个密钥");
  await groups.getByRole("button", { name: "确认保存分组" }).click();
  await expect
    .poll(async () => (await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups)
    .toEqual(["grp-eu-5d2"]);

  // The key keeps its group and is closed by it: a key is never moved.
  await expect(keyRow).toContainText("shared");
  await expect(keyRow).toContainText("已停用");
  await expect(keyRow).toContainText("不在租户的可用分组中");
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups).toEqual(["grp-eu-5d2"]);

  for (const path of ["/api/session", "/api/org", "/api/models", "/api/keys"]) {
    const answer = await t.client.request("GET", path);
    expect(answer.status, `${path}: ${answer.text}`).toBe(200);
    expect(answer.text, path).not.toContain("grp-eu-5d2");
    expect(answer.text, path).not.toContain("grp-us-8a4");
  }
});

test("把通配模型授给租户被拒，并在该模型旁说明原因", async ({ page }) => {
  const t = await signUpActive("T5 Wildcard Co 5d4", "owner@wild-t5.test");

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  // The list is read-only until the scope is the selected one.
  await grants.getByRole("radio", { name: "指定模型" }).check();
  // An ordinary model chosen with it, so the reason must land on the right one.
  await grants.getByRole("checkbox", { name: "chat-t5-e2e" }).check();
  await grants.getByRole("checkbox", { name: "wild-t5-*" }).check();
  await grants.getByRole("button", { name: "保存授权" }).click();

  const reason = "通配模型与 ensemble 模型不能授给租户";
  await expect(grants.getByRole("listitem").filter({ hasText: "wild-t5-*" })).toContainText(reason);
  await expect(grants.getByRole("checkbox", { name: "wild-t5-*" })).toHaveAccessibleDescription(reason);
  await expect(grants.getByRole("listitem").filter({ hasText: "chat-t5-e2e" })).not.toContainText(reason);
  // Refused as a whole: nothing was granted.
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_model_ids).toEqual([]);
});

test("公共模型生成的直连模型不出现在授权列表里，公共模型本身可以勾选", async ({ page }) => {
  const t = await signUpActive("T5 Direct Co 5d2", "owner@direct-t5.test");
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: "acct-gp-5d2",
      provider: "openai",
      api_key: "sk-t5-upstream-direct-5",
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  const pub = await stack.admin.post("/admin/public-models", { name: "pub-t5-direct" });
  await stack.admin.put(`/admin/provider-keys/${account.id}/channel-models/${pub.id}`, { priority: 1, weight: 1 });
  // The document that names the account exists: its absence below is the
  // picker leaving it out, not a catalog that never had it.
  const { items } = await stack.admin.get("/admin/resources/models");
  expect((items as Json[]).some((m) => (m.doc.display_name as string | undefined)?.includes("acct-gp-5d2"))).toBe(true);

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  await expect(grants.getByRole("checkbox", { name: "pub-t5-direct", exact: true })).toBeVisible();
  await expect(grants.getByRole("listitem").filter({ hasText: "acct-gp-5d2" })).toHaveCount(0);
  await expect(grants).not.toContainText("acct-gp-5d2");
});

test("密钥列表读取失败时不能移除分组；重新读取后先提示将停用的密钥数", async ({ page, context }) => {
  const t = await signUpActive("T5 Unread Co 4c7", "owner@unread-t5.test", [models.chat]);
  const tenant = await stack.admin.get(`/admin/tenants/${t.id}`);
  await stack.admin.patch(
    `/admin/tenants/${t.id}`,
    { allowed_provider_groups: ["grp-us-8a4", "grp-eu-5d2"] },
    tenant.version,
  );
  const us = (await stack.admin.get("/admin/provider-groups")).items.find(
    (g: { name: string }) => g.name === "grp-us-8a4",
  );
  for (const label of ["us-one-unread", "us-two-unread"]) {
    await stack.admin.post(`/admin/tenants/${t.id}/keys`, { label, group_id: us.id });
  }

  await openTenants(page);
  await openTenant(page, t.org);
  const keys = page.getByRole("table", { name: "密钥" });
  await expect(keys.getByRole("row").filter({ hasText: "us-two-unread" })).toBeVisible();

  // The connection drops, and reading everything again fails: the keys the
  // page showed may no longer be the tenant's keys.
  await context.setOffline(true);
  await page.getByRole("button", { name: "刷新", exact: true }).click();
  const groups = page.getByRole("group", { name: "可用分组" });
  await groups.getByRole("checkbox", { name: "grp-us-8a4" }).uncheck();
  // The removal is refused until the count is known: no save, and why.
  await expect(groups.getByRole("alert").filter({ hasText: "密钥列表读取失败" })).toBeVisible();
  await expect(groups.getByRole("button", { name: "保存分组" })).toBeDisabled();
  await context.setOffline(false);
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups).toEqual(["grp-us-8a4", "grp-eu-5d2"]);

  // Read again, the save asks first and names the keys it would stop.
  await groups.getByRole("button", { name: "重新读取密钥" }).click();
  await expect(groups.getByRole("alert").filter({ hasText: "密钥列表读取失败" })).toHaveCount(0);
  await groups.getByRole("button", { name: "保存分组" }).click();
  // The groups' own read failed offline too, and its note stays until the next
  // read: the confirmation is the alert that names the removal.
  await expect(groups.getByRole("alert").filter({ hasText: "移除分组" })).toContainText("将停用该租户在此分组下的 2 个密钥");
  await expect(groups.getByRole("button", { name: "确认保存分组" })).toBeVisible();
  await groups.getByRole("button", { name: "取消" }).click();
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups).toEqual(["grp-us-8a4", "grp-eu-5d2"]);
});

test("租户分组按显示名选择，路由名以小字显示", async ({ page }) => {
  const t = await signUpActive("T5 Picker Co 6a1", "owner@picker-t5.test");
  const current = await stack.admin.get(`/admin/tenants/${t.id}`);
  await stack.admin.patch(
    `/admin/tenants/${t.id}`,
    { allowed_provider_groups: ["grp-ui-7e2"] },
    current.version,
  );

  await openTenants(page);
  await openTenant(page, t.org);
  const groups = page.getByRole("group", { name: "可用分组" });
  const option = groups.getByRole("listitem").filter({ hasText: "欧洲二区 7e2" });
  await expect(option).toContainText("欧洲二区 7e2");
  await expect(option).toContainText("grp-ui-7e2");
  // The control is named by the display name; the routing name is text beside it.
  await expect(groups.getByRole("checkbox", { name: "欧洲二区 7e2" })).toBeChecked();
  // Choosing by display name saves that group's routing name, not its display name.
  await groups.getByRole("checkbox", { name: "grp-eu-5d2" }).check();
  await groups.getByRole("button", { name: "保存分组" }).click();
  await expect
    .poll(async () => [...(await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups].sort())
    .toEqual(["grp-eu-5d2", "grp-ui-7e2"]);
});

test("移除分组前提示将停用 N 个密钥；确认后这些密钥显示已停用，恢复后重新可用", async ({ page }) => {
  const t = await signUpActive("T5 Removal Co 8d2", "owner@removal-t5.test", [models.chat]);
  let tenant = await stack.admin.get(`/admin/tenants/${t.id}`);
  await stack.admin.patch(
    `/admin/tenants/${t.id}`,
    { allowed_provider_groups: ["grp-us-8a4", "grp-eu-5d2"] },
    tenant.version,
  );
  // Two keys in grp-us-8a4 and one elsewhere: the warning counts the group's
  // own keys, never every key the tenant has.
  const us = (await stack.admin.get("/admin/provider-groups")).items.find(
    (g: { name: string }) => g.name === "grp-us-8a4",
  );
  const eu = (await stack.admin.get("/admin/provider-groups")).items.find(
    (g: { name: string }) => g.name === "grp-eu-5d2",
  );
  for (const label of ["us-one-t5", "us-two-t5"]) {
    await stack.admin.post(`/admin/tenants/${t.id}/keys`, { label, group_id: us.id });
  }
  const euKey = await stack.admin.post(`/admin/tenants/${t.id}/keys`, { label: "eu-one-t5", group_id: eu.id });

  await openTenants(page);
  await openTenant(page, t.org);
  const groups = page.getByRole("group", { name: "可用分组" });
  await groups.getByRole("checkbox", { name: "grp-us-8a4" }).uncheck();
  await groups.getByRole("button", { name: "保存分组" }).click();
  await expect(groups.getByRole("alert")).toContainText("将停用该租户在此分组下的 2 个密钥");
  // Nothing is saved until it is confirmed.
  tenant = await stack.admin.get(`/admin/tenants/${t.id}`);
  expect(tenant.allowed_provider_groups).toEqual(["grp-us-8a4", "grp-eu-5d2"]);
  await groups.getByRole("button", { name: "确认保存分组" }).click();
  await expect
    .poll(async () => (await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups)
    .toEqual(["grp-eu-5d2"]);

  const keys = page.getByRole("table", { name: "密钥" });
  await expect(keys.getByRole("row").filter({ hasText: "us-one-t5" })).toContainText("已停用");
  await expect(keys.getByRole("row").filter({ hasText: "us-two-t5" })).toContainText("已停用");
  await expect(keys.getByRole("row").filter({ hasText: "eu-one-t5" })).toContainText("正常");
  // A closed key keeps its group's name as its routing name, but the gateway
  // disables it: the operator API marks it disabled with the group reason.
  const closed = await stack.admin.get(`/admin/keys/${euKey.key.id}`);
  expect(closed.effective_disabled).toBe(false);

  // Assigning the group back reopens exactly its keys: a key is never moved.
  await groups.getByRole("checkbox", { name: "grp-us-8a4" }).check();
  await groups.getByRole("button", { name: "保存分组" }).click();
  await expect(keys.getByRole("row").filter({ hasText: "us-one-t5" })).toContainText("正常");
  await expect(keys.getByRole("row").filter({ hasText: "us-two-t5" })).toContainText("正常");
  await expect(keys.getByRole("row").filter({ hasText: "eu-one-t5" })).toContainText("正常");
});

test("在 Dashboard 停用成员后，他的 key 在网关上被拒，启用后恢复", async ({ page }) => {
  const t = await signUpActive("T5 Member Co 6e5", "owner@member-t5.test", [models.chat]);
  const memberEmail = "dev@member-t5.test";
  const invite = await t.client.post("/api/members/invites", { email: memberEmail, role: "member" });
  const member = stack.tenant();
  await member.post("/api/invites/accept", { token: invite.invite_token, password: PASSWORD });
  const key: string = (await member.post("/api/keys", { label: "member-key-t5" })).plaintext;
  await expect.poll(() => chatStatus(key), { timeout: KEY_READY_MS }).toBe(200);

  await openTenants(page);
  await openTenant(page, t.org);
  await expect(page.getByText("停用后该成员的所有密钥立即失效")).toBeVisible();
  const row = page.getByRole("table", { name: "成员" }).getByRole("row").filter({ hasText: memberEmail });
  await row.getByRole("button", { name: "停用" }).click();
  await row.getByRole("button", { name: "确认停用" }).click();
  await expect(row).toContainText("已停用");
  await expect.poll(() => chatStatus(key), { timeout: ROLLOUT_BOUND_MS }).toBe(401);

  await row.getByRole("button", { name: "启用" }).click();
  await expect(row).not.toContainText("已停用");
  await expect.poll(() => chatStatus(key), { timeout: ROLLOUT_BOUND_MS }).toBe(200);
});

test("停用租户后它的 key 在网关上被拒，恢复后可用", async ({ page }) => {
  const t = await signUpActive("T5 Suspend Co 7f6", "owner@suspend-t5.test", [models.chat]);
  const key: string = (await t.client.post("/api/keys", { label: "suspend-key-t5" })).plaintext;
  await expect.poll(() => chatStatus(key), { timeout: KEY_READY_MS }).toBe(200);

  await openTenants(page);
  const row = tenantRow(page, t.org);
  await row.getByRole("button", { name: "停用" }).click();
  await row.getByRole("button", { name: "确认停用" }).click();
  await expect(row).toContainText("已停用");
  await expect.poll(() => chatStatus(key), { timeout: ROLLOUT_BOUND_MS }).toBe(401);

  await row.getByRole("button", { name: "恢复" }).click();
  await expect(row).toContainText("正常");
  await expect.poll(() => chatStatus(key), { timeout: ROLLOUT_BOUND_MS }).toBe(200);
});

test("运维邀请成员：邀请 token 只显示一次", async ({ page }) => {
  const t = await signUpActive("T5 Invite Co 8a7", "owner@invite-t5.test");
  const email = "ops-invited@invite-t5.test";

  await openTenants(page);
  await openTenant(page, t.org);
  const form = page.getByRole("group", { name: "邀请成员" });
  await form.getByLabel("邮箱").fill(email);
  await form.getByLabel("角色").selectOption({ label: "成员" });
  await form.getByRole("button", { name: "生成邀请" }).click();
  const shown = form.getByLabel("邀请 token");
  await expect(shown).toBeVisible();
  const token = ((await shown.textContent()) ?? "").trim();
  expect(token).toMatch(/^\S{16,}$/);

  // Away to another tab and back: gone.
  await page.locator('[data-tab="overview"]').click();
  await page.locator('[data-tab="tenants"]').click();
  await openTenant(page, t.org);
  await expect(page.getByRole("group", { name: "邀请成员" })).toBeVisible();
  expect(await page.content()).not.toContain(token);
  // After a reload: gone.
  await page.reload();
  await page.locator('[data-tab="tenants"]').click();
  await openTenant(page, t.org);
  await expect(page.getByRole("group", { name: "邀请成员" })).toBeVisible();
  expect(await page.content()).not.toContain(token);

  // What was shown is the real invitation: it lets the invitee in.
  const accepted = await stack.tenant().request("POST", "/api/invites/accept", { token, password: PASSWORD });
  expect(accepted.status, accepted.text).toBe(201);
  expect(accepted.body.member.email).toBe(email);
});

test("两个运维同时改同一租户的限额：后保存的被拒并提示重新载入，先保存的保留", async ({ page }) => {
  const t = await signUpActive("T5 Conflict Co 9b8", "owner@conflict-t5.test");

  await openTenants(page);
  await openTenant(page, t.org);
  const limits = page.getByRole("group", { name: "限额" });
  await expect(limits.getByLabel("租户每分钟请求")).toBeVisible();
  // Another operator saves first.
  const current = await stack.admin.request("GET", `/admin/tenants/${t.id}`);
  await stack.admin.patch(`/admin/tenants/${t.id}`, { limits: { ...current.body.limits, rpm: 600 } }, current.version!);

  await limits.getByLabel("租户每分钟请求").fill("300");
  await limits.getByRole("button", { name: "保存限额" }).click();
  await expect(limits.getByRole("alert")).toContainText("该资源已被他人修改");
  await limits.getByRole("button", { name: "重新载入" }).click();
  await expect(limits.getByLabel("租户每分钟请求")).toHaveValue("600");
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).limits.rpm).toBe(600);
});

test("两个运维同时改同一租户的授权：后保存的被拒，重新载入后显示先保存的", async ({ page }) => {
  const t = await signUpActive("T5 GrantRace Co 6c2", "owner@grantrace-t5.test");

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  await grants.getByRole("radio", { name: "指定模型" }).check();
  await expect(grants.getByRole("checkbox", { name: "chat-t5-e2e" })).not.toBeChecked();
  // Another operator grants a different model first.
  const current = await stack.admin.request("GET", `/admin/tenants/${t.id}`);
  await stack.admin.patch(`/admin/tenants/${t.id}`, { allowed_model_ids: [models.other.id] }, current.version!);

  await grants.getByRole("checkbox", { name: "chat-t5-e2e" }).check();
  await grants.getByRole("button", { name: "保存授权" }).click();
  await expect(grants.getByRole("alert")).toContainText("该资源已被他人修改");
  await grants.getByRole("button", { name: "重新载入" }).click();
  await expect(grants.getByRole("checkbox", { name: "other-t5-e2e" })).toBeChecked();
  await expect(grants.getByRole("checkbox", { name: "chat-t5-e2e" })).not.toBeChecked();
  await expect(grants.getByRole("alert")).toHaveCount(0);
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_model_ids).toEqual([models.other.id]);
});

test("另一位运维新建并授权的模型，页面重新载入后再保存别的授权，它仍在授权里", async ({ page }) => {
  const t = await signUpActive("T5 Catalog Co 2e6", "owner@catalog-t5.test", [models.chat]);

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  await expect(grants.getByRole("checkbox", { name: "chat-t5-e2e" })).toBeChecked();
  // Meanwhile another operator adds a model to the catalog and grants it.
  const late = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: "late-t5-e2e", provider: "openai", model_name: "gpt-4.1", provider_key_id: models.chat.doc.provider_key_id },
  });
  const current = await stack.admin.request("GET", `/admin/tenants/${t.id}`);
  await stack.admin.patch(`/admin/tenants/${t.id}`, { allowed_model_ids: [models.chat.id, late.id] }, current.version!);

  // A save elsewhere on the page meets that change and reads the tenant again.
  const limits = page.getByRole("group", { name: "限额" });
  await limits.getByLabel("租户每分钟请求").fill("90");
  await limits.getByRole("button", { name: "保存限额" }).click();
  await limits.getByRole("button", { name: "重新载入" }).click();
  await expect(grants.getByRole("checkbox", { name: "late-t5-e2e" })).toBeChecked();

  // A different grant, saved from the page.
  await grants.getByRole("checkbox", { name: "other-t5-e2e" }).check();
  await grants.getByRole("button", { name: "保存授权" }).click();
  await expect(grants).toContainText("已保存授权");
  const stored = (await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_model_ids as string[];
  expect([...stored].sort()).toEqual([models.chat.id, models.other.id, late.id].sort());
});

test("撤销授权要先确认：确认前不保存，确认后该模型从授权中移除", async ({ page }) => {
  const t = await signUpActive("T5 Ungrant Co 3f8", "owner@ungrant-t5.test", [models.chat, models.other]);

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  await grants.getByRole("checkbox", { name: "chat-t5-e2e" }).uncheck();
  await grants.getByRole("button", { name: "保存授权" }).click();
  // The model about to be withdrawn is named, and nothing is saved yet.
  await expect(grants.getByRole("alert")).toContainText("chat-t5-e2e");
  const before = (await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_model_ids as string[];
  expect([...before].sort()).toEqual([models.chat.id, models.other.id].sort());

  await grants.getByRole("button", { name: "确认保存授权" }).click();
  await expect(grants).toContainText("已保存授权");
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_model_ids).toEqual([models.other.id]);
});

test("改回全部公共模型后保存成功，范围是 all 且不再带模型列表", async ({ page }) => {
  // A tenant on a list, which is the state the scope can be widened from. The
  // save under `all` sends no list: the control plane refuses the two
  // together, so sending one answers 422 and the scope never moves.
  const t = await signUpActive("T5 Widen Co 9c7", "owner@widen-t5.test", [models.chat]);

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  await expect(grants.getByRole("radio", { name: "指定模型" })).toBeChecked();
  await grants.getByRole("radio", { name: "全部公共模型" }).check();
  await grants.getByRole("button", { name: "保存授权" }).click();
  // The hand-made model the list holds is outside the all scope, so the
  // widening takes it away from every key: it is named and confirmed first,
  // and nothing is saved until then.
  await expect(grants.getByRole("alert")).toContainText("chat-t5-e2e");
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).model_scope).toBe("selected");
  await grants.getByRole("button", { name: "确认保存授权" }).click();
  await expect(grants).toContainText("已保存授权");

  const stored = await stack.admin.get(`/admin/tenants/${t.id}`);
  expect(stored.model_scope).toBe("all");
  // The stored selection is what a later narrowing returns to, not a grant
  // the all scope carries.
  expect(stored.allowed_model_ids).toEqual([models.chat.id]);
});

test("刷新后列出页面打开后才注册的租户", async ({ page }) => {
  await openTenants(page);
  await expect(tenantRow(page, "平台内部")).toHaveCount(1);
  const t = await signUp("T5 Late Co 9d1", "owner@late-t5.test");
  await expect(tenantRow(page, t.org)).toHaveCount(0);

  await page.getByRole("button", { name: "刷新" }).click();
  await expect(tenantRow(page, t.org)).toContainText("待审核");
});

test("一处未保存的修改，不会被另一处的保存冲掉", async ({ page }) => {
  const t = await signUpActive("T5 Draft Co 7d3", "owner@draft-t5.test");

  await openTenants(page);
  await openTenant(page, t.org);
  const limits = page.getByRole("group", { name: "限额" });
  const grants = page.getByRole("group", { name: "授权模型" });
  await grants.getByRole("radio", { name: "指定模型" }).check();
  // A limit typed but not saved, then a grant saved.
  await limits.getByLabel("租户每分钟请求").fill("777");
  await grants.getByRole("checkbox", { name: "chat-t5-e2e" }).check();
  await grants.getByRole("button", { name: "保存授权" }).click();
  await expect(grants).toContainText("已保存授权");
  await expect(limits.getByLabel("租户每分钟请求")).toHaveValue("777");

  // Both edits land. The list holds what the tenant reached when the scope
  // became the selected one (every public model this stack has, which is not
  // the point here) plus the model just ticked.
  await limits.getByRole("button", { name: "保存限额" }).click();
  await expect
    .poll(async () => {
      const tenant = await stack.admin.get(`/admin/tenants/${t.id}`);
      return [tenant.limits.rpm, (tenant.allowed_model_ids as string[]).includes(models.chat.id)];
    })
    .toEqual([777, true]);
});

test("停用租户唯一的所有者被拒，并在该成员旁说明原因", async ({ page }) => {
  const t = await signUpActive("T5 Owner Co 1c9", "owner@owner-t5.test");

  await openTenants(page);
  await openTenant(page, t.org);
  const row = page.getByRole("table", { name: "成员" }).getByRole("row").filter({ hasText: t.email });
  await row.getByRole("button", { name: "停用" }).click();
  await row.getByRole("button", { name: "确认停用" }).click();
  await expect(row).toContainText("租户至少要保留一位所有者");
  await expect(row).not.toContainText("已停用");
  const members = await stack.admin.get(`/admin/tenants/${t.id}/members`);
  expect((members.items as Json[]).find((m) => m.email === t.email)?.disabled).toBe(false);
});

test("客户租户的可用分组不能清空：被拒并在分组处说明，原分组保留", async ({ page }) => {
  const t = await signUpActive("T5 NoGroup Co 2d1", "owner@nogroup-t5.test");
  const before = (await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups;
  expect(before.length).toBeGreaterThan(0);

  await openTenants(page);
  await openTenant(page, t.org);
  const groups = page.getByRole("group", { name: "可用分组" });
  for (const group of before) await groups.getByRole("checkbox", { name: await shownAs(group), exact: true }).uncheck();
  // No group at all reaches no upstream: the save is confirmed first.
  await groups.getByRole("button", { name: "保存分组" }).click();
  await groups.getByRole("button", { name: "确认保存分组" }).click();
  // The two-step warning fades out after the answer; wait for it to go.
  await expect(groups.getByRole("alert")).toHaveCount(1);
  await expect(groups.getByRole("alert")).toBeVisible();
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups).toEqual(before);
});

test("保存后租户没有任何带上游账号的分组时，先警告并要求确认", async ({ page }) => {
  const t = await signUpActive("T5 Unreach Co 8e4", "owner@unreach-t5.test");
  const current = await stack.admin.request("GET", `/admin/tenants/${t.id}`);
  // A group no upstream account is in, beside the default one.
  await stack.admin.patch(
    `/admin/tenants/${t.id}`,
    { allowed_provider_groups: ["shared", "grp-none-7d3"] },
    current.version!,
  );

  await openTenants(page);
  await openTenant(page, t.org);
  const groups = page.getByRole("group", { name: "可用分组" });
  await expect(groups.getByRole("listitem").filter({ hasText: "grp-none-7d3" })).toContainText("没有上游账号在此分组");
  // `shared` is shown by its display name, 通用分组 on a fresh database.
  await groups.getByRole("checkbox", { name: "通用分组" }).uncheck();
  await groups.getByRole("button", { name: "保存分组" }).click();
  await expect(groups.getByRole("alert")).toContainText("grp-none-7d3");
  // Nothing is saved until it is confirmed.
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups).toEqual(["shared", "grp-none-7d3"]);
  await groups.getByRole("button", { name: "确认保存分组" }).click();
  await expect
    .poll(async () => (await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups)
    .toEqual(["grp-none-7d3"]);
});

test("在租户页给旧密钥选择一次分组后，它的分组随之改变，且不能再次更改", async ({ page }) => {
  const t = await signUpActive("T5 Override Co 4f3", "owner@override-t5.test", [models.chat]);
  // The key is minted while the tenant has one group, then set back to no
  // group: the state an older control-plane replica leaves.
  const created = await t.client.post("/api/keys", { label: "override-key-t5" });
  stack.clearKeyGroup(created.key.id);
  const current = await stack.admin.request("GET", `/admin/tenants/${t.id}`);
  await stack.admin.patch(
    `/admin/tenants/${t.id}`,
    { allowed_provider_groups: ["shared", "grp-us-8a4", "grp-eu-5d2"] },
    current.version!,
  );

  await openTenants(page);
  await openTenant(page, t.org);
  const row = page.getByRole("table", { name: "密钥" }).getByRole("row").filter({ hasText: "override-key-t5" });
  await expect(row).toContainText("未选分组");
  await row.getByRole("button", { name: "选择分组" }).click();
  const editor = page.getByRole("group", { name: "选择分组" });
  await editor.getByRole("checkbox", { name: "grp-us-8a4" }).check();
  await editor.getByRole("button", { name: "保存分组" }).click();
  await expect(row).toContainText("grp-us-8a4");
  expect((await stack.admin.get(`/admin/keys/${created.key.id}`)).effective_provider_groups).toEqual(["grp-us-8a4"]);
  // One group, once: the one-time action is gone from the row.
  await expect(row.getByRole("button", { name: "选择分组" })).toHaveCount(0);
});

test("授权一个未定价的模型时显示未定价提示", async ({ page }) => {
  const t = await signUpActive("T5 Unpriced Co 9a1", "owner@unpriced-t5.test");
  // `chat-t5-e2e` has no baseline; this second model gets one, so the notice
  // is pinned to the unpriced one rather than to every grant.
  const priced = await stack.admin.post("/admin/resources/models", {
    doc: {
      display_name: "priced-t5-e2e",
      provider: "openai",
      model_name: "gpt-4.1",
      provider_key_id: (await stack.admin.get("/admin/resources/provider_keys")).items.find(
        (k: Json) => k.doc.display_name === "acct-t5-shared",
      ).id,
    },
  });
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: priced.id,
    effective_from: new Date(Date.now() - 4_000).toISOString(),
    card: { default: { base: { input: "1000", output: "2000" } } },
    note: "定价提示用例",
  });

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  await grants.getByRole("radio", { name: "指定模型" }).check();
  const notice = "未定价：调用会暂不计费，直到设置基准价";
  await grants.getByRole("checkbox", { name: "chat-t5-e2e" }).check();
  await grants.getByRole("button", { name: "保存授权" }).click();
  const unpriced = grants.getByRole("listitem").filter({ hasText: "chat-t5-e2e" });
  await expect(unpriced).toContainText(notice);
  await expect(grants.getByRole("listitem").filter({ hasText: "priced-t5-e2e" })).not.toContainText(notice);
  // The grant itself was saved: the notice is advice, not a refusal.
  await expect
    .poll(async () => (await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_model_ids)
    .toContain(models.chat.id);
});

test("只读账号在租户页看不到任何操作", async ({ page }) => {
  const t = await signUp("T5 Viewer Co 3e2", "owner@viewer-t5.test");
  const viewer = { email: "viewer-t5@e2e.local", password: "viewer-t5-pass-4f1" };
  await stack.admin.post("/admin/operators", { ...viewer, role: "viewer" });

  await login(page, viewer.email, viewer.password);
  await page.locator('[data-tab="tenants"]').click();
  await expect(tenantRow(page, t.org)).toContainText("待审核");
  await openTenant(page, t.org);
  await expect(page.getByRole("table", { name: "成员" })).toContainText(t.email);
  await expect(page.getByRole("group", { name: "授权模型" }).getByRole("checkbox")).not.toHaveCount(0);
  await expect(page.getByRole("button", { name: /批准|停用|启用|恢复|保存|邀请|覆盖|默认值/ })).toHaveCount(0);
  await expect(page.getByRole("checkbox", { disabled: false })).toHaveCount(0);
  await expect(page.getByRole("spinbutton", { disabled: false })).toHaveCount(0);
});

test("新租户默认值的默认分组不能清空：被拒并在该字段说明，原默认值保留", async ({ page }) => {
  const before: Json = await stack.admin.get("/admin/settings/tenant-defaults");
  expect(before.default_provider_groups.length).toBeGreaterThan(0);

  await openTenants(page);
  await page.getByRole("button", { name: "新租户默认值" }).click();
  const panel = page.getByRole("region", { name: "新租户默认值" });
  const groups = panel.getByRole("group", { name: "默认分组" });
  for (const group of before.default_provider_groups) {
    await groups.getByRole("checkbox", { name: await shownAs(group), exact: true }).uncheck();
  }
  await panel.getByRole("button", { name: "保存默认值" }).click();
  await expect(groups.getByRole("alert")).toBeVisible();
  expect(await stack.admin.get("/admin/settings/tenant-defaults")).toEqual(before);
});

test("新租户默认分组使用同一选择器：按显示名选择，路由名以小字显示", async ({ page }) => {
  const before: Json = await stack.admin.get("/admin/settings/tenant-defaults");
  try {
    await openTenants(page);
    await page.getByRole("button", { name: "新租户默认值" }).click();
    const panel = page.getByRole("region", { name: "新租户默认值" });
    const groups = panel.getByRole("group", { name: "默认分组" });
    const option = groups.getByRole("listitem").filter({ hasText: "欧洲二区 7e2" });
    await expect(option).toContainText("欧洲二区 7e2");
    await expect(option).toContainText("grp-ui-7e2");
    await expect(groups.getByRole("checkbox", { name: "欧洲二区 7e2" })).toBeVisible();
    await groups.getByRole("checkbox", { name: "欧洲二区 7e2" }).check();
    await panel.getByRole("button", { name: "保存默认值" }).click();
    // The saved value is the routing name the list stores, not the display name.
    await expect
      .poll(async () => (await stack.admin.get("/admin/settings/tenant-defaults")).default_provider_groups)
      .toContain("grp-ui-7e2");
  } finally {
    const restored = await stack.admin.request("PUT", "/admin/settings/tenant-defaults", before);
    expect(restored.status, restored.text).toBe(200);
  }
});

/**
 * Two public models served by the shared account, made once for the cases
 * below: the earlier cases rely on this stack having none, so they are made
 * only when a case that needs them runs, after those.
 */
let servedPublics: Promise<Json[]> | undefined;
function publicPair(): Promise<Json[]> {
  servedPublics ??= (async () => {
    const made: Json[] = [];
    for (const name of ["pub-a-t5-e2e", "pub-b-t5-e2e"]) {
      const pub = await stack.admin.post("/admin/public-models", { name });
      await stack.admin.put(`/admin/provider-keys/${sharedAccount.id}/channel-models/${pub.id}`, { priority: 1, weight: 1 });
      made.push(pub);
    }
    return made;
  })();
  return servedPublics;
}

/** The model names a tenant's own `/api/models` lists: what its scope reaches. */
async function reached(t: TenantSide): Promise<string[]> {
  return ((await t.client.get("/api/models")).items as Json[]).map((m) => m.name as string).sort();
}

test("全部公共模型改为指定模型时，去掉的公共模型要先确认：取消则不改，确认后只收回它", async ({ page }) => {
  const [a, b] = await publicPair();
  const t = await signUpActive("T5 Narrow Co 4e2", "owner@narrow-t5.test");
  expect(await reached(t)).toEqual([a.name, b.name].sort());

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  await grants.getByRole("radio", { name: "指定模型" }).check();
  // Narrowing starts from what the tenant reaches now.
  await expect(grants.getByRole("checkbox", { name: a.name })).toBeChecked();
  await expect(grants.getByRole("checkbox", { name: b.name })).toBeChecked();
  await grants.getByRole("checkbox", { name: b.name }).uncheck();
  await grants.getByRole("button", { name: "保存授权" }).click();
  // Asked first, naming exactly what is taken away; nothing is saved yet.
  const warning = grants.getByRole("alert");
  await expect(warning).toContainText(b.name);
  await expect(warning).not.toContainText(a.name);
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).model_scope).toBe("all");

  // Cancelled: the tenant keeps both.
  await grants.getByRole("button", { name: "取消" }).click();
  await expect(grants.getByRole("button", { name: "确认保存授权" })).toHaveCount(0);
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).model_scope).toBe("all");
  expect(await reached(t)).toEqual([a.name, b.name].sort());

  // Confirmed: only the model named is taken away.
  await grants.getByRole("button", { name: "保存授权" }).click();
  await grants.getByRole("button", { name: "确认保存授权" }).click();
  await expect(grants).toContainText("已保存授权");
  const after = await stack.admin.get(`/admin/tenants/${t.id}`);
  expect(after.model_scope).toBe("selected");
  expect(after.allowed_model_ids).toEqual([a.id]);
  expect(await reached(t)).toEqual([a.name]);
});

/**
 * The public-model list is read separately from the tenant and the model
 * catalogue, so it can fail while they load. Narrowing must still start from
 * what the tenant reaches — never from an empty set that a save would then
 * store as "no models" — and widening back, which needs the list, waits for
 * it. The failure is made at the network for that one read: no request to
 * the control plane produces it on demand.
 */
test("公共模型列表读取失败时：改为指定模型仍从当前可用的模型开始，不会存成空授权；改回全部要等列表读到", async ({ page }) => {
  const [a, b] = await publicPair();
  const t = await signUpActive("T5 Unread Co 8f1", "owner@unread-t5.test");
  const PUBLIC_LIST = "**/admin/public-models";
  await page.route(PUBLIC_LIST, (route) => route.abort());

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  // Under all, what the scope covers is unknown: the failure says so and the save waits.
  await expect(grants.getByRole("alert").filter({ hasText: "公共模型列表读取失败" })).toBeVisible();
  await expect(grants.getByRole("button", { name: "保存授权" })).toBeDisabled();

  await grants.getByRole("radio", { name: "指定模型" }).check();
  await expect(grants.getByRole("checkbox", { name: a.name })).toBeChecked();
  await expect(grants.getByRole("checkbox", { name: b.name })).toBeChecked();
  await grants.getByRole("button", { name: "保存授权" }).click();
  await expect(grants).toContainText("已保存授权");
  const narrowed = await stack.admin.get(`/admin/tenants/${t.id}`);
  expect(narrowed.model_scope).toBe("selected");
  expect([...narrowed.allowed_model_ids].sort()).toEqual([a.id, b.id].sort());
  expect(await reached(t)).toEqual([a.name, b.name].sort());

  // Back to all: the save waits for the list, and the retry brings it.
  await grants.getByRole("radio", { name: "全部公共模型" }).check();
  await expect(grants.getByRole("button", { name: "保存授权" })).toBeDisabled();
  await page.unroute(PUBLIC_LIST);
  await grants.getByRole("button", { name: "重试" }).click();
  await expect(grants.getByRole("checkbox", { name: a.name })).toBeChecked();
  await expect(grants.getByRole("button", { name: "保存授权" })).toBeEnabled();
  await grants.getByRole("button", { name: "保存授权" }).click();
  // The first save's note is still shown, so the stored scope is what tells.
  await expect.poll(async () => (await stack.admin.get(`/admin/tenants/${t.id}`)).model_scope).toBe("all");
});

// Last: it changes what every later signup starts with.
test("管理员修改新租户默认值后，新注册的租户带上新的默认分组与限额", async ({ page }) => {
  const before: Json = await stack.admin.get("/admin/settings/tenant-defaults");

  await openTenants(page);
  await page.getByRole("button", { name: "新租户默认值" }).click();
  const panel = page.getByRole("region", { name: "新租户默认值" });
  const groups = panel.getByRole("group", { name: "默认分组" });
  for (const group of before.default_provider_groups) {
    const shown = await shownAs(group);
    await expect(groups.getByRole("checkbox", { name: shown, exact: true })).toBeChecked();
    await groups.getByRole("checkbox", { name: shown, exact: true }).uncheck();
  }
  await groups.getByRole("checkbox", { name: "grp-eu-5d2" }).check();
  await panel.getByLabel("租户每分钟 token").fill("50000");
  await panel.getByRole("button", { name: "保存默认值" }).click();

  const expected = { ...before, tpm: 50000, default_provider_groups: ["grp-eu-5d2"] };
  await expect.poll(() => stack.admin.get("/admin/settings/tenant-defaults")).toEqual(expected);
  const t = await signUp("T5 Defaults Co 5a4", "owner@defaults-t5.test");
  const tenant = await stack.admin.get(`/admin/tenants/${t.id}`);
  expect(tenant.allowed_provider_groups).toEqual(["grp-eu-5d2"]);
  expect(tenant.limits.tpm).toBe(50000);
});
