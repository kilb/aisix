import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";

/**
 * The upstream accounts tab: the account form and its schema-driven
 * options, the platform's default groups, credentials that are kept without
 * ever being shown, two operators saving one account, and the two-step
 * delete and group removal. Against the real control plane and the real
 * managed gateway, whose upstream is the stack's stand-in.
 */

let stack: Stack;

test.beforeAll(async () => {

  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  // Every write that names a group names a live one: the accounts and the
  // tenant edits below included. A name a test here creates itself is
  // skipped.
  // The groups a test here creates itself (`grp-internal-keep-7c1`,
  // `grp-solo-6b2`, `grp-covered-6b2`, `grp-swap-a1c`, `grp-idle-4e1`) are
  // not seeded here: the edit that creates one also asserts what it answered.
  await stack.admin.seedGroups(["grp-alpha-7c1", "grp-beta-3e9", "grp-dflt-8a3", "grp-x-41f", "grp-y-92c"]);
});

test.afterAll(async () => {
  await stack?.stop();
});

/** The upstream credential the gateway must keep sending. */
const SECRET = "sk-e2e-upstream-41c7";

/** A saved document reaches the stack's one gateway within this. */
const APPLIED_WITHIN_MS = 30_000;

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令").fill(password);
  await page.getByRole("button", { name: "登录" }).click();
}

async function loginAsOperator(page: Page): Promise<void> {
  await login(page, stack.operator.email, stack.operator.password);
}

async function openTab(page: Page, label: string): Promise<void> {
  await page.getByRole("tab", { name: label }).click();
}

function accountRow(page: Page, name: string) {
  return page.getByRole("row", { name: new RegExp(name) });
}

function groupsField(page: Page) {
  return page.getByRole("group", { name: "分组", exact: true });
}

async function addGroup(page: Page, name: string): Promise<void> {
  await page.getByRole("textbox", { name: "添加分组" }).fill(name);
  await page.getByRole("button", { name: "添加", exact: true }).click();
}

async function createAccount(doc: Json): Promise<Json> {
  return stack.admin.post("/admin/resources/provider_keys", { doc });
}

async function account(id: string): Promise<Json> {
  return stack.admin.get(`/admin/resources/provider_keys/${id}`);
}

async function accountNamed(name: string): Promise<Json> {
  const { items } = await stack.admin.get("/admin/resources/provider_keys");
  const found = (items as Json[]).find((v) => v.doc.display_name === name);
  expect(found, JSON.stringify(items)).toBeDefined();
  return found;
}

/** Every value typed into a field of `scope`, which text checks do not see. */
async function fieldValues(page: Page, selector: string): Promise<string> {
  return page.evaluate(
    (sel) =>
      Array.from(document.querySelectorAll(`${sel} input, ${sel} textarea, ${sel} select`))
        .map((el) => (el as HTMLInputElement).value)
        .join("\n"),
    selector,
  );
}

async function chat(key: string, model: string): Promise<number> {
  const res = await fetch(`${stack.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${key}` },
    body: JSON.stringify({ model, messages: [{ role: "user", content: "hi" }] }),
  });
  await res.arrayBuffer();
  return res.status;
}

/** The `Authorization` of each chat request the upstream stand-in received. */
async function upstreamAuthorizations(): Promise<string[]> {
  const res = await fetch(`${stack.mockUpstreamUrl}/__requests`);
  const records = (await res.json()) as Json[];
  return records.filter((r) => r.path === "/v1/chat/completions").map((r) => r.authorization);
}

async function clearUpstream(): Promise<void> {
  const res = await fetch(`${stack.mockUpstreamUrl}/__requests`, { method: "DELETE" });
  expect(res.status).toBe(204);
}

test("协议族的选项就是网关 schema 里的 Adapter 枚举", async ({ page }) => {
  const schema = await stack.admin.get("/admin/schemas/provider_key");
  await loginAsOperator(page);
  await openTab(page, "上游账号");
  await page.getByRole("button", { name: "新增上游账号" }).click();
  await expect(page.getByLabel("协议族").locator("option")).toHaveText(schema.definitions.Adapter.enum);
});

test("新建上游账号默认带平台默认分组，保存后显示已生效", async ({ page }) => {
  await loginAsOperator(page);
  await openTab(page, "上游账号");
  await page.getByRole("button", { name: "新增上游账号" }).click();
  await expect(groupsField(page).getByRole("listitem")).toHaveText(["shared"]);
  await addGroup(page, "grp-alpha-7c1");
  await expect(groupsField(page).getByRole("listitem")).toHaveText(["shared", "grp-alpha-7c1"]);
  await page.getByLabel("名称").fill("acct-alpha-e2e");
  await page.getByLabel("供应商标识").fill("openai");
  await page.getByLabel("接口地址").fill(`${stack.mockUpstreamUrl}/v1`);
  await page.getByLabel("密钥").fill("sk-e2e-alpha-upstream-5a1");
  await page.getByRole("button", { name: "保存" }).click();

  const row = accountRow(page, "acct-alpha-e2e");
  await expect(row.locator("[data-state]")).toHaveAttribute("data-state", "applied", { timeout: APPLIED_WITHIN_MS });
  await expect(row).toContainText("grp-alpha-7c1");
  // What the control plane stored, and its own view of the rollout.
  const stored = await accountNamed("acct-alpha-e2e");
  expect(stored.doc.groups).toEqual(["shared", "grp-alpha-7c1"]);
  expect(stored.doc.api_base).toBe(`${stack.mockUpstreamUrl}/v1`);
  expect(stored.doc.api_key).toBe("[REDACTED]");
  expect(stored.rollout.state).toBe("applied");
});

test("新建上游账号的默认分组跟随平台设置", async ({ page }) => {
  const before = await stack.admin.get("/admin/settings/tenant-defaults");
  const changed = await stack.admin.request("PUT", "/admin/settings/tenant-defaults", {
    ...before,
    default_provider_groups: ["grp-dflt-8a3"],
  });
  try {
    expect(changed.status, changed.text).toBe(200);
    await loginAsOperator(page);
    await openTab(page, "上游账号");
    await page.getByRole("button", { name: "新增上游账号" }).click();
    await expect(groupsField(page).getByRole("listitem")).toHaveText(["grp-dflt-8a3"]);
  } finally {
    const restored = await stack.admin.request("PUT", "/admin/settings/tenant-defaults", before);
    expect(restored.status).toBe(200);
  }
});

test("只改分组不碰密钥：网关发给上游的仍是原来的密钥", async ({ page }) => {
  const { items: tenants } = await stack.admin.get("/admin/tenants");
  const internal = (tenants as Json[]).find((t) => t.is_internal);
  expect(internal, JSON.stringify(tenants)).toBeDefined();
  // Every key names one group, and the platform's own tenant has no
  // assignment list to take it from: this group holds the account too. An
  // account lists only live groups, so it exists before the account.
  const group = await stack.admin.post("/admin/provider-groups", {
    name: "grp-internal-keep-7c1",
    display_name: "保留自用 7c1",
    default_cny_per_usd: "7.2",
  });
  const created = await createAccount({
    display_name: "acct-keep-e2e",
    provider: "openai",
    api_key: SECRET,
    api_base: `${stack.mockUpstreamUrl}/v1`,
    groups: ["shared", "grp-internal-keep-7c1"],
  });
  await stack.admin.post("/admin/resources/models", {
    doc: { display_name: "chat-keep-e2e", provider: "openai", model_name: "gpt-4o-mini", provider_key_id: created.id },
  });
  const { plaintext } = await stack.admin.post(`/admin/tenants/${internal.id}/keys`, {
    label: "keep-e2e",
    allowed_models: ["chat-keep-e2e"],
    group_id: group.id,
  });
  // Before any edit: the gateway serves the account with its stored key.
  await clearUpstream();
  await expect.poll(() => chat(plaintext, "chat-keep-e2e"), { timeout: APPLIED_WITHIN_MS }).toBe(200);
  expect(await upstreamAuthorizations()).toContain(`Bearer ${SECRET}`);

  await loginAsOperator(page);
  await openTab(page, "上游账号");
  await accountRow(page, "acct-keep-e2e").getByRole("button", { name: "编辑" }).click();
  const form = page.getByRole("form", { name: /acct-keep-e2e/ });
  await expect(form.getByRole("group", { name: "密钥" })).toContainText("已设置");
  await expect(form).not.toContainText("[REDACTED]");
  expect(await fieldValues(page, "form")).not.toContain("[REDACTED]");
  await expect(form.getByRole("button", { name: "清除" })).toHaveCount(0);

  await addGroup(page, "grp-beta-3e9");
  await form.getByRole("button", { name: "保存" }).click();
  // The row shows the saved version first, then its rollout.
  const row = accountRow(page, "acct-keep-e2e");
  await expect(row).toContainText("grp-beta-3e9");
  await expect(row.locator("[data-state]")).toHaveAttribute("data-state", "applied", { timeout: APPLIED_WITHIN_MS });
  // The edit was stored and the gateway has it.
  await expect.poll(async () => (await account(created.id)).rollout.state, { timeout: APPLIED_WITHIN_MS }).toBe("applied");
  const saved = await account(created.id);
  expect(saved.version).toBeGreaterThan(created.version);
  expect([...(saved.doc.groups as string[])].sort()).toEqual(
    ["grp-beta-3e9", "grp-internal-keep-7c1", "shared"].sort(),
  );
  expect(saved.rollout.state).toBe("applied");

  await clearUpstream();
  expect(await chat(plaintext, "chat-keep-e2e")).toBe(200);
  expect(await upstreamAuthorizations()).toEqual([`Bearer ${SECRET}`]);
});

test("以 secret 写入的账号，只读运维看到已设置，页面上没有密钥", async ({ page }) => {
  await createAccount({ display_name: "acct-secret-e2e", provider: "openai", secret: SECRET });
  const email = "viewer-pk-e2e@e2e.local";
  const password = "viewer-pk-pass-7e3";
  await stack.admin.post("/admin/operators", { email, role: "viewer", password });
  await login(page, email, password);
  await openTab(page, "上游账号");
  const row = accountRow(page, "acct-secret-e2e");
  await expect(row).toBeVisible();
  await expect(page.getByRole("button", { name: "新增上游账号" })).toHaveCount(0);
  await expect(row.getByRole("button", { name: "删除" })).toHaveCount(0);
  await row.getByRole("button", { name: "查看" }).click();
  const form = page.getByRole("form", { name: /acct-secret-e2e/ });
  await expect(form.getByRole("group", { name: "密钥" })).toContainText("已设置");
  await expect(form.getByRole("button", { name: "替换" })).toHaveCount(0);
  await expect(form.getByRole("button", { name: "保存" })).toHaveCount(0);
  // Nowhere on the page: not in its text, its markup or any field.
  expect(await page.content()).not.toContain(SECRET);
  expect(await fieldValues(page, "body")).not.toContain(SECRET);
});

test("两个运维同时编辑同一个上游账号：后保存的被拒，先保存的保留", async ({ page, browser }) => {
  const created = await createAccount({
    display_name: "acct-race-e2e",
    provider: "openai",
    api_key: "sk-e2e-race-upstream-2c8",
    api_base: `${stack.mockUpstreamUrl}/v1`,
    groups: ["shared"],
  });
  const second = await browser.newContext();
  try {
    const other = await second.newPage();
    for (const p of [page, other]) {
      await loginAsOperator(p);
      await openTab(p, "上游账号");
      await accountRow(p, "acct-race-e2e").getByRole("button", { name: "编辑" }).click();
      await expect(groupsField(p).getByRole("listitem")).toHaveText(["shared"]);
    }

    await addGroup(page, "grp-x-41f");
    await page.getByRole("button", { name: "保存" }).click();
    await expect(accountRow(page, "acct-race-e2e")).toContainText("grp-x-41f");
    expect((await account(created.id)).doc.groups).toEqual(["shared", "grp-x-41f"]);

    await addGroup(other, "grp-y-92c");
    await other.getByRole("button", { name: "保存" }).click();
    await expect(other.getByRole("alert").filter({ hasText: "已被他人修改" })).toBeVisible();
    // The first save stands.
    expect((await account(created.id)).doc.groups).toEqual(["shared", "grp-x-41f"]);

    // Reloading from the note shows what is stored now.
    await other.getByRole("button", { name: "重新载入" }).click();
    await expect(groupsField(other).getByRole("listitem")).toHaveText(["shared", "grp-x-41f"]);

    await other.reload();
    await openTab(other, "上游账号");
    const row = accountRow(other, "acct-race-e2e");
    await expect(row).toContainText("grp-x-41f");
    await expect(row).not.toContainText("grp-y-92c");
  } finally {
    await second.close();
  }
});

test("被模型引用的上游账号不能删，并点名引用它的模型", async ({ page }) => {
  const created = await createAccount({
    display_name: "acct-held-e2e",
    provider: "openai",
    api_key: "sk-e2e-held-upstream-9d4",
    groups: ["shared"],
  });
  await stack.admin.post("/admin/resources/models", {
    doc: { display_name: "chat-held-e2e", provider: "openai", model_name: "gpt-4o-mini", provider_key_id: created.id },
  });
  await loginAsOperator(page);
  await openTab(page, "上游账号");
  const row = accountRow(page, "acct-held-e2e");
  await row.getByRole("button", { name: "删除" }).click();
  await row.getByRole("button", { name: "确认删除" }).click();
  const refused = page.getByRole("alert").filter({ hasText: "被以下资源引用：" });
  await expect(refused).toContainText("chat-held-e2e");
  expect((await stack.admin.request("GET", `/admin/resources/provider_keys/${created.id}`)).status).toBe(200);
  await expect(row).toBeVisible();
});

test("删除上游账号要两步确认：取消不删，确认后才删", async ({ page }) => {
  const created = await createAccount({
    display_name: "acct-drop-e2e",
    provider: "openai",
    api_key: "sk-e2e-drop-upstream-3f0",
    groups: ["shared"],
  });
  await loginAsOperator(page);
  await openTab(page, "上游账号");
  const row = accountRow(page, "acct-drop-e2e");
  await row.getByRole("button", { name: "删除" }).click();
  await row.getByRole("button", { name: "取消" }).click();
  await expect(row.getByRole("button", { name: "确认删除" })).toHaveCount(0);
  expect((await stack.admin.request("GET", `/admin/resources/provider_keys/${created.id}`)).status).toBe(200);

  await row.getByRole("button", { name: "删除" }).click();
  await row.getByRole("button", { name: "确认删除" }).click();
  await expect(accountRow(page, "acct-drop-e2e")).toHaveCount(0);
  expect((await stack.admin.request("GET", `/admin/resources/provider_keys/${created.id}`)).status).toBe(404);
});

test("移除分组前，点名因此用不到该账号的租户与密钥；取消则不保存", async ({ page }) => {
  // A tenant may only be assigned a group that exists, an account may only
  // list one, and a key names it by id.
  const soloGroup = await stack.admin.post("/admin/provider-groups", {
    name: "grp-solo-6b2",
    display_name: "单租户 6b2",
    default_cny_per_usd: "7.2",
  });
  const soloGroupId: string = soloGroup.id;
  // Its groups lose the account, but its only key names a group that keeps
  // it: every key names one group from creation.
  const keepGroup = await stack.admin.post("/admin/provider-groups", {
    name: "grp-covered-6b2",
    display_name: "已覆盖 6b2",
    default_cny_per_usd: "7.2",
  });
  const created = await createAccount({
    display_name: "acct-solo-e2e",
    provider: "openai",
    api_key: "sk-e2e-solo-upstream-6b2",
    groups: ["shared", "grp-solo-6b2", "grp-covered-6b2"],
  });
  // Reaches the account only through grp-solo-6b2, with a key that follows it: loses it.
  const solo = await stack.admin.post("/admin/tenants", { name: "tenant-solo-e2e" });
  await stack.admin.patch(`/admin/tenants/${solo.id}`, { allowed_provider_groups: ["grp-solo-6b2"] }, solo.version);
  await stack.admin.post(`/admin/tenants/${solo.id}/keys`, { label: "key-follows-e2e" });
  const covered = await stack.admin.post("/admin/tenants", { name: "tenant-covered-e2e" });
  await stack.admin.patch(
    `/admin/tenants/${covered.id}`,
    { allowed_provider_groups: ["grp-solo-6b2", "grp-covered-6b2"] },
    covered.version,
  );
  await stack.admin.post(`/admin/tenants/${covered.id}/keys`, {
    label: "key-covered-e2e",
    group_id: keepGroup.id,
  });
  // Also reaches it through shared: keeps it.
  const both = await stack.admin.post("/admin/tenants", { name: "tenant-both-e2e" });
  await stack.admin.patch(
    `/admin/tenants/${both.id}`,
    { allowed_provider_groups: ["shared", "grp-solo-6b2"] },
    both.version,
  );
  // A key naming the account's group, in a tenant that keeps the account
  // through another group: loses the account when `grp-solo-6b2` goes.
  const host = await stack.admin.post("/admin/tenants", { name: "tenant-host-e2e" });
  await stack.admin.patch(
    `/admin/tenants/${host.id}`,
    { allowed_provider_groups: ["shared", "grp-solo-6b2"] },
    host.version,
  );
  await stack.admin.post(`/admin/tenants/${host.id}/keys`, {
    label: "key-solo-e2e",
    group_id: soloGroupId,
  });

  await loginAsOperator(page);
  await openTab(page, "上游账号");
  await accountRow(page, "acct-solo-e2e").getByRole("button", { name: "编辑" }).click();
  await page.getByRole("button", { name: "移除 grp-solo-6b2" }).click();
  await page.getByRole("button", { name: "保存" }).click();
  const confirm = page.getByRole("region", { name: "移除分组" });
  await expect(confirm).toContainText("grp-solo-6b2");
  await expect(confirm.getByRole("listitem").filter({ hasText: "tenant-solo-e2e" })).toContainText(
    "租户默认分组不再包含该账号",
  );
  await expect(confirm).toContainText("key-solo-e2e");
  await expect(confirm).not.toContainText("tenant-both-e2e");
  await expect(confirm).not.toContainText("tenant-covered-e2e");
  await expect(confirm).not.toContainText("key-covered-e2e");
  // Nothing is saved before the confirmation.
  expect((await account(created.id)).doc.groups).toEqual(["shared", "grp-solo-6b2", "grp-covered-6b2"]);

  await confirm.getByRole("button", { name: "取消" }).click();
  await expect(confirm).toHaveCount(0);
  expect((await account(created.id)).doc.groups).toEqual(["shared", "grp-solo-6b2", "grp-covered-6b2"]);

  await page.getByRole("button", { name: "保存" }).click();
  await confirm.getByRole("button", { name: "确认移除分组" }).click();
  // Saved: the form closes, and the row shows the account without the group.
  await expect(page.getByRole("form", { name: /acct-solo-e2e/ })).toHaveCount(0);
  await expect(accountRow(page, "acct-solo-e2e")).toContainText("shared");
  await expect(accountRow(page, "acct-solo-e2e")).not.toContainText("grp-solo-6b2");
  expect((await account(created.id)).doc.groups).toEqual(["shared", "grp-covered-6b2"]);
});

test("取消后改为移除另一个分组：确认框只列这一次的影响，没人用的分组确认后移除", async ({ page }) => {
  // Uses grp-swap-a1c; nobody uses grp-idle-4e1. An account lists only live
  // groups, so both exist before it.
  await stack.admin.post("/admin/provider-groups", { name: "grp-swap-a1c", display_name: "换组 a1c", default_cny_per_usd: "7.2" });
  await stack.admin.post("/admin/provider-groups", { name: "grp-idle-4e1", display_name: "闲置 4e1", default_cny_per_usd: "7.2" });
  const created = await createAccount({
    display_name: "acct-swap-e2e",
    provider: "openai",
    api_key: "sk-e2e-swap-upstream-a1c",
    groups: ["shared", "grp-swap-a1c", "grp-idle-4e1"],
  });
  const swap = await stack.admin.post("/admin/tenants", { name: "tenant-swap-e2e" });
  await stack.admin.patch(`/admin/tenants/${swap.id}`, { allowed_provider_groups: ["grp-swap-a1c"] }, swap.version);
  await stack.admin.post(`/admin/tenants/${swap.id}/keys`, { label: "key-swap-e2e" });

  await loginAsOperator(page);
  await openTab(page, "上游账号");
  await accountRow(page, "acct-swap-e2e").getByRole("button", { name: "编辑" }).click();
  const confirm = page.getByRole("region", { name: "移除分组" });
  // The first removal names the tenant that uses the group.
  await page.getByRole("button", { name: "移除 grp-swap-a1c" }).click();
  await page.getByRole("button", { name: "保存" }).click();
  await expect(confirm).toContainText("tenant-swap-e2e");
  await confirm.getByRole("button", { name: "取消" }).click();

  // The same removal again, cancelled while its lookup may still run, then
  // a different one: what is offered for confirmation is the second's.
  await page.getByRole("button", { name: "保存" }).click();
  await confirm.getByRole("button", { name: "取消" }).click();
  await addGroup(page, "grp-swap-a1c");
  await page.getByRole("button", { name: "移除 grp-idle-4e1" }).click();
  await page.getByRole("button", { name: "保存" }).click();
  await expect(confirm.getByRole("button", { name: "确认移除分组" })).toBeEnabled();
  // Read once, the moment confirming is offered: a stale list would be showing now.
  const offered = (await confirm.textContent()) ?? "";
  expect(offered).toContain("grp-idle-4e1");
  expect(offered).toContain("没有租户或密钥会因此用不到该账号");
  expect(offered).not.toContain("tenant-swap-e2e");

  await confirm.getByRole("button", { name: "确认移除分组" }).click();
  await expect(page.getByRole("form", { name: /acct-swap-e2e/ })).toHaveCount(0);
  await expect(accountRow(page, "acct-swap-e2e")).not.toContainText("grp-idle-4e1");
  expect((await account(created.id)).doc.groups).toEqual(["shared", "grp-swap-a1c"]);
});

/** The text of the elements that describe `field` (its refusal, when there is one). */
function description(field: Locator): Promise<string> {
  return field.evaluate((el) =>
    (el.getAttribute("aria-describedby") ?? "")
      .split(/\s+/)
      .filter(Boolean)
      .map((id) => document.getElementById(id)?.textContent ?? "")
      .join(" "),
  );
}

test("改了接口地址却没有重填密钥：被拒的原因标在密钥旁，地址没有保存", async ({ page }) => {
  const created = await createAccount({
    display_name: "acct-moved-e2e",
    provider: "openai",
    api_key: "sk-e2e-moved-upstream-8c3",
    api_base: `${stack.mockUpstreamUrl}/v1`,
  });
  await loginAsOperator(page);
  await openTab(page, "上游账号");
  await accountRow(page, "acct-moved-e2e").getByRole("button", { name: "编辑" }).click();
  const form = page.getByRole("form", { name: /acct-moved-e2e/ });
  await form.getByLabel("接口地址").fill("https://moved.example.test/v1");
  await form.getByRole("button", { name: "保存" }).click();

  const secret = form.getByRole("group", { name: "密钥" });
  await expect(secret).toHaveAttribute("aria-invalid", "true");
  await expect(secret).toHaveAccessibleDescription(/\S/);
  // The reason is at the field, not repeated in the note under the form.
  await expect(form.getByRole("alert")).toBeVisible();
  await expect(form.getByRole("alert")).not.toContainText(await description(secret));
  await expect(form.getByLabel("接口地址")).not.toHaveAttribute("aria-invalid", "true");
  expect((await account(created.id)).doc.api_base).toBe(`${stack.mockUpstreamUrl}/v1`);
});

test("刷新后列出页面打开后才新增的上游账号", async ({ page }) => {
  await loginAsOperator(page);
  await openTab(page, "上游账号");
  // The page has read its list before the account is written.
  await expect(page.getByRole("button", { name: "新增上游账号" })).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
  await createAccount({ display_name: "acct-late-e2e", provider: "openai", api_key: "sk-e2e-late-upstream-1d5" });
  await expect(accountRow(page, "acct-late-e2e")).toHaveCount(0);

  await page.getByRole("button", { name: "刷新" }).click();
  await expect(accountRow(page, "acct-late-e2e")).toBeVisible();
});
