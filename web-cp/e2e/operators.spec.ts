import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type ApiClient, type Json, type Stack } from "./stack";

/**
 * Operator accounts, roles and the audit log, against the real control
 * plane through the built `dist/`. Setup goes through the real `/admin`
 * API. Every test leaves the stack's own operator as the only enabled
 * admin, which "不能停用最后一个管理员" checks before it starts.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

/**
 * Buttons that create, change, approve, issue, revoke, reconcile or delete
 * something, or open a form that does.
 */
const EDIT_CONTROL = /^(新增|新建|创建|保存|删除|编辑|批准|停用|启用|恢复|签发|吊销|允许清理|接管|邀请|生成邀请|分组覆盖|确认)/;

/** Controls only an admin may use, on any tab. */
const ADMIN_ONLY = /^(新建运维账号|签发|允许清理|接管|新租户默认值|保存默认值)/;

/** Controls only an admin may use on one tab (an operator revokes tenant keys elsewhere). */
const ADMIN_ONLY_ON: Record<string, RegExp> = { gateways: /^吊销/ };

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

function loginAsAdmin(page: Page): Promise<void> {
  return login(page, stack.operator.email, stack.operator.password);
}

/** Waits until the page on screen has read what it lists. */
async function settled(page: Page): Promise<void> {
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
}

async function openTab(page: Page, tab: string): Promise<void> {
  await page.locator(`[data-tab="${tab}"]`).click();
  await expect(page.locator(`[data-tab="${tab}"]`)).toHaveAttribute("aria-selected", "true");
  await settled(page);
}

/** The labels of the buttons on the page on screen (the header's are not on it). */
async function pageButtons(page: Page): Promise<string[]> {
  return (await page.locator("main button").allInnerTexts()).map((t) => t.trim());
}

/** Every tab of the Dashboard, in order. Every role but admin sees all of them but `operators`. */
const ADMIN_TABS = ["overview", "gateways", "providers", "models", "tenants", "keys", "cache", "resources", "operators", "audit"];
const STAFF_TABS = ADMIN_TABS.filter((t) => t !== "operators");

/**
 * Each tab's edit controls, by `data-tab`. The tab bar must hold exactly
 * `expected`: a tab that goes missing fails here instead of slipping out
 * of the checks. Every tab must show its real page, never the placeholder
 * of a page not built yet, so the controls counted are the page's own.
 */
async function editControlsByTab(page: Page, expected: string[]): Promise<Record<string, string[]>> {
  const tabs = await page.locator("[data-tab]").evaluateAll((els) => els.map((e) => e.getAttribute("data-tab") ?? ""));
  expect(tabs, "the tabs this role sees").toEqual(expected);
  const out: Record<string, string[]> = {};
  for (const tab of tabs) {
    await openTab(page, tab);
    await expect(page.locator("main").getByText("此页尚未开放"), `${tab} shows its real page`).toHaveCount(0);
    out[tab] = (await pageButtons(page)).filter((n) => EDIT_CONTROL.test(n));
  }
  return out;
}

function createOperator(email: string, role: string, password: string, displayName?: string): Promise<Json> {
  return stack.admin.post("/admin/operators", { email, role, password, display_name: displayName });
}

async function operatorByEmail(email: string): Promise<Json> {
  const { items } = await stack.admin.get("/admin/operators");
  const found = items.find((o: Json) => o.email === email);
  if (!found) throw new Error(`no operator ${email}`);
  return found;
}

/** A fresh API client signed in as `email`. */
async function signedIn(email: string, password: string): Promise<ApiClient> {
  const client = stack.tenant();
  await client.post("/admin/login", { email, password });
  return client;
}

/** The table row that names `text`. */
function rowWith(page: Page, text: string): Locator {
  return page.locator("main tbody tr").filter({ hasText: text });
}

/** The role a row's role picker shows. */
function shownRole(row: Locator): Locator {
  return row.getByLabel("角色").locator("option:checked");
}

/** The cells of the column headed `header` in the page's table, top to bottom. */
function column(page: Page, header: string): Promise<string[]> {
  return page.locator("main table").evaluate((table, h) => {
    const heads = [...table.querySelectorAll("thead th")].map((th) => th.textContent?.trim() ?? "");
    const at = heads.indexOf(h);
    if (at < 0) return [];
    return [...table.querySelectorAll("tbody tr")].map((tr) => tr.children[at]?.textContent?.trim() ?? "");
  }, header);
}

/** An upstream account at the stand-in and one direct model on it; the model's view. */
async function seedModel(name: string): Promise<{ account: Json; model: Json }> {
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: `${name}-account`,
      provider: "openai",
      api_key: `sk-${name}-upstream`,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: name, provider: "openai", model_name: "gpt-4o-mini", provider_key_id: account.id },
  });
  return { account, model };
}

/** A write sent from the page itself, with its session cookie; the answer's status and body. */
function writeFromPage(page: Page, method: string, path: string, body: unknown): Promise<{ status: number; body: Json }> {
  return page.evaluate(
    async ({ method, path, body }) => {
      const r = await fetch(path, {
        method,
        headers: { "content-type": "application/json" },
        body: JSON.stringify(body),
      });
      return { status: r.status, body: await r.json() };
    },
    { method, path, body },
  );
}

test("只读运维看不到任何编辑入口，从页面直接发写请求被拒", async ({ page, browser }) => {
  const viewer = { email: "viewer@e2e.local", password: "viewer-e2e-pass-2c7" };
  // Rows a viewer can open on the pages that list them.
  const { account } = await seedModel("view-model-e2e");
  await stack.admin.post("/admin/resources/cache_policies", {
    doc: { name: "cache-view-e2e", scope: "team", ttl_seconds: 120 },
  });

  await loginAsAdmin(page);
  // The check finds edit controls where an admin has them.
  const adminControls = await editControlsByTab(page, ADMIN_TABS);
  expect(adminControls.gateways).toEqual(expect.arrayContaining(["签发网关凭据", "吊销"]));
  expect(adminControls.providers).toContain("新增上游账号");
  expect(adminControls.models).toContain("新增直连模型");
  expect(adminControls.tenants).toContain("新租户默认值");
  expect(adminControls.keys).toContain("新建密钥");
  expect(adminControls.cache).toContain("新建缓存策略");
  expect(adminControls.resources).toContain("新建");
  expect(adminControls.operators).toContain("新建运维账号");

  // The admin creates the viewer on the page.
  await openTab(page, "operators");
  await page.getByRole("button", { name: "新建运维账号" }).click();
  const form = page.getByRole("form", { name: "新建运维账号" });
  await form.getByLabel("邮箱").fill(viewer.email);
  await form.getByLabel("显示名").fill("只读 E2E");
  await form.getByLabel("角色").selectOption({ label: "只读" });
  await form.getByLabel("初始口令").fill(viewer.password);
  await form.getByRole("button", { name: "创建" }).click();
  const row = rowWith(page, viewer.email);
  await expect(shownRole(row)).toHaveText("只读");
  await expect(row).toContainText("正常");
  expect(await page.content()).not.toContain(viewer.password);
  const stored = await operatorByEmail(viewer.email);
  expect(stored).toMatchObject({ role: "viewer", display_name: "只读 E2E", disabled: false });

  const viewerContext = await browser.newContext();
  const vpage = await viewerContext.newPage();
  await login(vpage, viewer.email, viewer.password);
  await expect(vpage.locator('[data-tab="operators"]')).toHaveCount(0);
  for (const [tab, names] of Object.entries(await editControlsByTab(vpage, STAFF_TABS))) {
    expect(names, `edit controls on ${tab}`).toEqual([]);
  }

  // Every kind of the generic page, and a row opened on it.
  await openTab(vpage, "resources");
  const picker = vpage.getByLabel("资源类型");
  const kinds = await picker.locator("option").evaluateAll((os) => os.map((o) => (o as HTMLOptionElement).value));
  expect(kinds.length).toBeGreaterThan(1);
  for (const kind of kinds) {
    await picker.selectOption(kind);
    await settled(vpage);
    expect((await pageButtons(vpage)).filter((n) => EDIT_CONTROL.test(n)), `edit controls for ${kind}`).toEqual([]);
  }
  await picker.selectOption("models");
  await rowWith(vpage, "view-model-e2e").getByRole("button").click();
  await expect(vpage.getByLabel("资源文档")).toHaveValue(/view-model-e2e/);
  await expect(vpage.getByLabel("资源文档")).not.toBeEditable();
  expect((await pageButtons(vpage)).filter((n) => EDIT_CONTROL.test(n))).toEqual([]);

  // A cache policy opened on its own page.
  await openTab(vpage, "cache");
  await rowWith(vpage, "cache-view-e2e").getByRole("button").click();
  await expect(vpage.getByLabel("名称")).toHaveValue("cache-view-e2e");
  await expect(vpage.getByLabel("名称")).not.toBeEditable();
  expect((await pageButtons(vpage)).filter((n) => EDIT_CONTROL.test(n))).toEqual([]);

  // A write sent from the page anyway is refused by the control plane.
  const answer = await writeFromPage(vpage, "POST", "/admin/resources/models", {
    doc: { display_name: "viewer-write-e2e", provider: "openai", model_name: "gpt-4o-mini", provider_key_id: account.id },
  });
  expect(answer.status).toBe(403);
  expect(answer.body.code).toBe("forbidden");
  const models = await stack.admin.get("/admin/resources/models");
  expect(models.items.map((m: Json) => m.doc.display_name)).not.toContain("viewer-write-e2e");
  await viewerContext.close();
});

test("运维角色看不到运维账号、网关凭据与清理入口，越权写被拒", async ({ page }) => {
  const op = { email: "operator-role@e2e.local", password: "operator-e2e-pass-8d1" };
  const mine = await createOperator(op.email, "operator", op.password);
  await login(page, op.email, op.password);
  await expect(page.locator('[data-tab="operators"]')).toHaveCount(0);
  const controls = await editControlsByTab(page, STAFF_TABS);
  for (const [tab, names] of Object.entries(controls)) {
    const adminOnly = names.filter((n) => ADMIN_ONLY.test(n) || (ADMIN_ONLY_ON[tab]?.test(n) ?? false));
    expect(adminOnly, `admin-only controls on ${tab}`).toEqual([]);
  }
  // An operator does write gateway resources, and manages tenants' keys.
  expect(controls.providers).toContain("新增上游账号");
  expect(controls.models).toContain("新增直连模型");
  expect(controls.keys).toContain("新建密钥");
  expect(controls.resources).toContain("新建");
  expect(controls.cache).toContain("新建缓存策略");

  // Writes an admin's role is needed for, sent from the page anyway.
  const create = await writeFromPage(page, "POST", "/admin/operators", {
    email: "escalate@e2e.local",
    role: "admin",
    password: "escalate-e2e-pass-7c2",
  });
  expect(create.status).toBe(403);
  expect(create.body.code).toBe("forbidden");
  const raise = await writeFromPage(page, "PATCH", `/admin/operators/${mine.id}`, { role: "admin" });
  expect(raise.status).toBe(403);
  expect(raise.body.code).toBe("forbidden");
  const { items } = await stack.admin.get("/admin/operators");
  expect(items.map((o: Json) => o.email)).not.toContain("escalate@e2e.local");
  expect((await operatorByEmail(op.email)).role).toBe("operator");
});

test("不能停用最后一个管理员", async ({ page }) => {
  const { items } = await stack.admin.get("/admin/operators");
  const admins = items.filter((o: Json) => o.role === "admin" && !o.disabled).map((o: Json) => o.email);
  expect(admins).toEqual([stack.operator.email]);

  await loginAsAdmin(page);
  await openTab(page, "operators");
  const row = rowWith(page, stack.operator.email);
  await row.getByRole("button", { name: "停用" }).click();
  await row.getByRole("button", { name: "确认停用" }).click();
  await expect(page.getByRole("alert")).toContainText("至少要保留一个可用的管理员");
  await expect(row).toContainText("正常");
  expect((await operatorByEmail(stack.operator.email)).disabled).toBe(false);

  // Nor lower its role.
  await row.getByLabel("角色").selectOption({ label: "运维" });
  await row.getByRole("button", { name: "确认改为运维" }).click();
  await expect(page.getByRole("alert")).toContainText("至少要保留一个可用的管理员");
  await expect(shownRole(row)).toHaveText("管理员");
  expect((await operatorByEmail(stack.operator.email)).role).toBe("admin");

  // Still signed in, still an admin.
  await page.reload();
  await expect(page.locator('[data-tab="operators"]')).toBeVisible();
});

test("停用、启用与降级都在页面上确认，停用即结束该账号的会话", async ({ page }) => {
  const target = { email: "toggle-op@e2e.local", password: "toggle-e2e-pass-4b6" };
  await createOperator(target.email, "operator", target.password);
  const theirs = await signedIn(target.email, target.password);
  expect((await theirs.get("/admin/session")).authed).toBe(true);

  await loginAsAdmin(page);
  await openTab(page, "operators");
  const row = rowWith(page, target.email);

  // A cancelled disable sends nothing.
  await row.getByRole("button", { name: "停用" }).click();
  await row.getByRole("button", { name: "取消" }).click();
  await expect(row.getByRole("button", { name: "确认停用" })).toHaveCount(0);
  expect((await operatorByEmail(target.email)).disabled).toBe(false);

  await row.getByRole("button", { name: "停用" }).click();
  await row.getByRole("button", { name: "确认停用" }).click();
  await expect(row).toContainText("已停用");
  expect((await operatorByEmail(target.email)).disabled).toBe(true);
  expect((await theirs.get("/admin/session")).authed).toBe(false);

  await row.getByRole("button", { name: "启用" }).click();
  await expect(row).toContainText("正常");
  expect((await operatorByEmail(target.email)).disabled).toBe(false);

  // A lower role waits for its confirmation too.
  await row.getByLabel("角色").selectOption({ label: "只读" });
  await row.getByRole("button", { name: "取消" }).click();
  await expect(shownRole(row)).toHaveText("运维");
  expect((await operatorByEmail(target.email)).role).toBe("operator");

  await row.getByLabel("角色").selectOption({ label: "只读" });
  await row.getByRole("button", { name: "确认改为只读" }).click();
  await expect(row.getByRole("button", { name: "确认改为只读" })).toHaveCount(0);
  await expect(shownRole(row)).toHaveText("只读");
  expect((await operatorByEmail(target.email)).role).toBe("viewer");
});

test("会话中被降权后，页面上仍显示的写操作被服务端拒绝并说明原因", async ({ page }) => {
  const me = { email: "demote-me@e2e.local", password: "demote-e2e-pass-1a9" };
  const bystander = "bystander@e2e.local";
  const mine = await createOperator(me.email, "admin", me.password);
  await createOperator(bystander, "operator", "bystander-e2e-pass-5d3");
  await login(page, me.email, me.password);
  await openTab(page, "operators");
  const row = rowWith(page, bystander);
  await expect(row).toBeVisible();

  // Another admin lowers this account to operator while it is signed in.
  await stack.admin.patch(`/admin/operators/${mine.id}`, { role: "operator" });
  await row.getByRole("button", { name: "停用" }).click();
  await row.getByRole("button", { name: "确认停用" }).click();
  await expect(page.getByRole("alert")).toContainText("没有执行此操作的权限");
  expect((await operatorByEmail(bystander)).disabled).toBe(false);

  // Then to viewer: a gateway write is refused the same way.
  await stack.admin.patch(`/admin/operators/${mine.id}`, { role: "viewer" });
  await openTab(page, "cache");
  await page.getByRole("button", { name: "新建缓存策略" }).click();
  await page.getByLabel("名称").fill("cache-demoted-e2e");
  await page.getByLabel("缓存时长（秒）").fill("60");
  await page.getByRole("button", { name: "保存" }).click();
  await expect(page.getByRole("alert")).toContainText("没有执行此操作的权限");
  const policies = await stack.admin.get("/admin/resources/cache_policies");
  expect(policies.items.map((p: Json) => p.doc.name)).not.toContain("cache-demoted-e2e");
});

test("审计日志记录了谁做了什么，且不含口令", async ({ page }) => {
  const email = "audit-op@e2e.local";
  const displayName = "审计运维 7f3";
  const initial = "audit-initial-pass-5e1";
  const changed = "audit-changed-pass-9c4";
  const upstreamSecret = "sk-audit-upstream-3d8";

  await loginAsAdmin(page);
  await openTab(page, "operators");
  await page.getByRole("button", { name: "新建运维账号" }).click();
  const form = page.getByRole("form", { name: "新建运维账号" });
  await form.getByLabel("邮箱").fill(email);
  await form.getByLabel("显示名").fill(displayName);
  await form.getByLabel("角色").selectOption({ label: "运维" });
  await form.getByLabel("初始口令").fill(initial);
  await form.getByRole("button", { name: "创建" }).click();
  await expect(shownRole(rowWith(page, email))).toHaveText("运维");
  expect(await page.content()).not.toContain(initial);
  const created = await operatorByEmail(email);

  // That operator, through the API, changes its password and writes an
  // upstream account with a secret.
  const theirs = await signedIn(email, initial);
  await theirs.post("/admin/password", { current_password: initial, new_password: changed });
  const account = await theirs.post("/admin/resources/provider_keys", {
    doc: {
      display_name: "acct-audit-e2e",
      provider: "openai",
      api_key: upstreamSecret,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });

  await openTab(page, "audit");
  const rows = page.locator("main tbody tr");
  const createRow = rows.filter({ hasText: "operator.create" }).filter({ hasText: created.id });
  await expect(createRow).toHaveCount(1);
  await expect(createRow).toContainText(stack.operator.email);
  await expect(createRow).toContainText(email);
  await expect(createRow).toContainText(displayName);
  const passwordRow = rows.filter({ hasText: "operator.password" }).filter({ hasText: created.id });
  await expect(passwordRow).toHaveCount(1);
  await expect(passwordRow).toContainText(email);
  const accountRow = rows.filter({ hasText: "resource.create" }).filter({ hasText: account.id });
  await expect(accountRow).toHaveCount(1);
  await expect(accountRow).toContainText(email);
  await expect(accountRow).toContainText("provider_keys");
  // The fields written are named; their values are not shown.
  await expect(accountRow).toContainText("api_key");

  // Newest first.
  const texts = await rows.allInnerTexts();
  const at = (action: string, id: string) => texts.findIndex((t) => t.includes(action) && t.includes(id));
  expect(at("resource.create", account.id)).toBeGreaterThanOrEqual(0);
  expect(at("resource.create", account.id)).toBeLessThan(at("operator.password", created.id));
  expect(at("operator.password", created.id)).toBeLessThan(at("operator.create", created.id));

  const html = await page.content();
  for (const secret of [initial, changed, upstreamSecret]) expect(html).not.toContain(secret);
});

test("审计日志按对象类型与租户筛选，加载更多沿用服务端的分页", async ({ page }) => {
  // More rows about operators than one page holds.
  const filler = await createOperator("filler-op@e2e.local", "viewer", "filler-e2e-pass-3f0");
  for (let i = 0; i < 60; i++) {
    await stack.admin.patch(`/admin/operators/${filler.id}`, { display_name: `filler ${i}` });
  }
  const tenantA = await stack.admin.post("/admin/tenants", { name: "audit-tenant-a-e2e" });
  const tenantB = await stack.admin.post("/admin/tenants", { name: "audit-tenant-b-e2e" });

  await loginAsAdmin(page);
  await openTab(page, "audit");

  // By target kind, page after page, exactly as the API pages it.
  await page.getByLabel("对象类型").selectOption("operator");
  const expected: string[] = [];
  let before: number | null = null;
  let pages = 0;
  for (;;) {
    const q = new URLSearchParams({ target_kind: "operator" });
    if (before !== null) q.set("before", String(before));
    const answer = await stack.admin.get(`/admin/audit?${q}`);
    expected.push(...answer.items.map((i: Json) => String(i.id)));
    if (pages > 0) await page.getByRole("button", { name: "加载更多" }).click();
    await expect.poll(() => column(page, "编号")).toEqual(expected);
    pages += 1;
    before = answer.next_before;
    if (before === null || pages > 10) break;
  }
  expect(pages).toBeGreaterThan(1);
  await expect(page.getByRole("button", { name: "加载更多" })).toHaveCount(0);

  // By tenant: only that tenant's rows.
  await page.getByLabel("对象类型").selectOption({ label: "全部" });
  // By role and name: the target-kind picker's own options mention 租户 too.
  await page.getByRole("combobox", { name: /^租户/ }).selectOption({ label: "audit-tenant-a-e2e" });
  const forA = await stack.admin.get(`/admin/audit?tenant_id=${tenantA.id}`);
  const forB = await stack.admin.get(`/admin/audit?tenant_id=${tenantB.id}`);
  expect(forA.items.length).toBeGreaterThan(0);
  expect(forB.items.length).toBeGreaterThan(0);
  await expect.poll(() => column(page, "编号")).toEqual(forA.items.map((i: Json) => String(i.id)));
  const shown = await column(page, "编号");
  for (const b of forB.items) expect(shown).not.toContain(String(b.id));
  await expect(rowWith(page, "tenant.create")).toHaveCount(1);
});

test("修改口令后，同一账号的其他会话失效", async ({ page, browser }) => {
  // A viewer: every role changes its own password.
  const email = "pw-viewer@e2e.local";
  const first = "pw-first-pass-6a2";
  const second = "pw-second-pass-0d5";
  await createOperator(email, "viewer", first);

  const otherContext = await browser.newContext();
  const other = await otherContext.newPage();
  await login(other, email, first);
  await login(page, email, first);

  // A wrong current password changes nothing.
  await page.getByRole("button", { name: "修改口令" }).click();
  const form = page.getByRole("form", { name: "修改口令" });
  await form.getByLabel("当前口令").fill("not-the-password-1");
  await form.getByLabel("新口令", { exact: true }).fill(second);
  await form.getByLabel("确认新口令").fill(second);
  await form.getByRole("button", { name: "保存新口令" }).click();
  await expect(form.getByRole("alert")).toContainText("当前口令不正确");
  await openTab(other, "audit");
  await expect(other.locator("[data-tab]").first()).toBeVisible();
  await expect(other.locator("main tbody tr").first()).toBeVisible();

  await form.getByLabel("当前口令").fill(first);
  await form.getByRole("button", { name: "保存新口令" }).click();
  await expect(page.getByRole("status")).toContainText("口令已修改");

  // The other session ends: its next request drops it back to the login gate.
  await other.locator('[data-tab="resources"]').click();
  await expect(other.getByRole("button", { name: "登录" })).toBeVisible();
  await expect(other.locator("[data-tab]")).toHaveCount(0);

  // This one stays signed in.
  await page.reload();
  await expect(page.locator("[data-tab]").first()).toBeVisible();

  // Only the new password signs in.
  expect((await stack.tenant().request("POST", "/admin/login", { email, password: first })).status).toBe(401);
  expect((await stack.tenant().request("POST", "/admin/login", { email, password: second })).status).toBe(200);
  await otherContext.close();
});

test("会话在别处登出后，下一次操作就回到登录框", async ({ page, browser }) => {
  await loginAsAdmin(page);
  await openTab(page, "cache");
  // A second browser holding the same session: signed in while it lives.
  const copy = await browser.newContext();
  await copy.addCookies(await page.context().cookies());
  const other = await copy.newPage();
  await other.goto(stack.previewUrl);
  await expect(other.locator("[data-tab]").first()).toBeVisible();
  await other.getByRole("button", { name: "登出" }).click();
  await expect(other.getByRole("button", { name: "登录" })).toBeVisible();

  // The first page acts without reloading.
  await page.locator('[data-tab="audit"]').click();
  await expect(page.getByRole("button", { name: "登录" })).toBeVisible();
  await expect(page.locator("[data-tab]")).toHaveCount(0);
  await expect(page.getByText("登录已过期")).toBeVisible();

  // Signing in again with a wrong password reports that, and not a second expiry.
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令", { exact: true }).fill("wrong-password-e2e-1");
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.getByRole("alert")).toContainText("邮箱或口令不正确");
  await expect(page.getByRole("alert")).not.toContainText("登录已过期");
  await expect(page.getByText("登录已过期")).toHaveCount(1);
  await expect(page.locator("[data-tab]")).toHaveCount(0);
  await copy.close();
});
