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
  // the tenant and defaults edits in the specs included.
  for (const name of ["grp-eu-5d2", "grp-us-8a4", "grp-none-7d3"]) {
    await stack.admin.createGroup(name, name);
  }
  // Every account a tenant key reaches by default is in `shared`.
  const shared = await account("acct-t5-shared", ["shared"], "sk-t5-upstream-shared-1");
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
    await stack.admin.patch(
      `/admin/tenants/${t.id}`,
      { allowed_model_ids: grants.map((m) => m.id) },
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
  expect((await t.client.get("/api/models")).items).toEqual([]);

  await openTenants(page);
  await openTenant(page, t.org);
  const grants = page.getByRole("group", { name: "授权模型" });
  await grants.getByRole("checkbox", { name: "chat-t5-e2e" }).check();
  await grants.getByRole("button", { name: "保存授权" }).click();
  await expect
    .poll(async () => ((await t.client.get("/api/models")).items as Json[]).map((m) => m.name))
    .toEqual(["chat-t5-e2e"]);
});

test("给租户设置可用分组后，运维侧的 key 显示生效分组，/api 的任何响应都不含分组名", async ({ page }) => {
  const t = await signUpActive("T5 Groups Co 4c3", "owner@groups-t5.test", [models.chat]);
  await t.client.post("/api/keys", { label: "groups-key-t5" });

  await openTenants(page);
  await openTenant(page, t.org);
  const keyRow = page.getByRole("table", { name: "密钥" }).getByRole("row").filter({ hasText: "groups-key-t5" });
  // Before the change the key follows the tenant's default group.
  await expect(keyRow).toContainText("shared");
  const groups = page.getByRole("group", { name: "可用分组" });
  await expect(groups).toContainText("租户看不到分组名");
  await groups.getByRole("checkbox", { name: "shared" }).uncheck();
  await groups.getByRole("checkbox", { name: "grp-eu-5d2" }).check();
  // grp-eu-5d2 holds an upstream account, so the save asks for no confirmation.
  await groups.getByRole("button", { name: "保存分组" }).click();

  await expect(keyRow).toContainText("grp-eu-5d2");
  await expect(keyRow).not.toContainText("grp-us-8a4");
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
  // A limit typed but not saved, then a grant saved.
  await limits.getByLabel("租户每分钟请求").fill("777");
  await grants.getByRole("checkbox", { name: "chat-t5-e2e" }).check();
  await grants.getByRole("button", { name: "保存授权" }).click();
  await expect(grants).toContainText("已保存授权");
  await expect(limits.getByLabel("租户每分钟请求")).toHaveValue("777");

  // Both edits land.
  await limits.getByRole("button", { name: "保存限额" }).click();
  await expect
    .poll(async () => {
      const tenant = await stack.admin.get(`/admin/tenants/${t.id}`);
      return [tenant.limits.rpm, tenant.allowed_model_ids];
    })
    .toEqual([777, [models.chat.id]]);
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
  for (const group of before) await groups.getByRole("checkbox", { name: group }).uncheck();
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
  await groups.getByRole("checkbox", { name: "shared" }).uncheck();
  await groups.getByRole("button", { name: "保存分组" }).click();
  await expect(groups.getByRole("alert")).toContainText("grp-none-7d3");
  // Nothing is saved until it is confirmed.
  expect((await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups).toEqual(["shared", "grp-none-7d3"]);
  await groups.getByRole("button", { name: "确认保存分组" }).click();
  await expect
    .poll(async () => (await stack.admin.get(`/admin/tenants/${t.id}`)).allowed_provider_groups)
    .toEqual(["grp-none-7d3"]);
});

test("在租户页给 key 设置分组覆盖后，该 key 的生效分组随之改变", async ({ page }) => {
  const t = await signUpActive("T5 Override Co 4f3", "owner@override-t5.test", [models.chat]);
  const created = await t.client.post("/api/keys", { label: "override-key-t5" });

  await openTenants(page);
  await openTenant(page, t.org);
  const row = page.getByRole("table", { name: "密钥" }).getByRole("row").filter({ hasText: "override-key-t5" });
  await expect(row).not.toContainText("grp-us-8a4");
  await row.getByRole("button", { name: "分组覆盖" }).click();
  const editor = page.getByRole("group", { name: "分组覆盖" });
  await editor.getByRole("checkbox", { name: "grp-us-8a4" }).check();
  await editor.getByRole("button", { name: "保存覆盖" }).click();
  await expect(row).toContainText("grp-us-8a4");
  expect((await stack.admin.get(`/admin/keys/${created.key.id}`)).effective_provider_groups).toEqual(["grp-us-8a4"]);
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
  for (const group of before.default_provider_groups) await groups.getByRole("checkbox", { name: group }).uncheck();
  await panel.getByRole("button", { name: "保存默认值" }).click();
  await expect(groups.getByRole("alert")).toBeVisible();
  expect(await stack.admin.get("/admin/settings/tenant-defaults")).toEqual(before);
});

// Last: it changes what every later signup starts with.
test("管理员修改新租户默认值后，新注册的租户带上新的默认分组与限额", async ({ page }) => {
  const before: Json = await stack.admin.get("/admin/settings/tenant-defaults");

  await openTenants(page);
  await page.getByRole("button", { name: "新租户默认值" }).click();
  const panel = page.getByRole("region", { name: "新租户默认值" });
  const groups = panel.getByRole("group", { name: "默认分组" });
  for (const group of before.default_provider_groups) {
    await expect(groups.getByRole("checkbox", { name: group })).toBeChecked();
    await groups.getByRole("checkbox", { name: group }).uncheck();
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
