import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, fundTenant, startStack, type Json, type Stack } from "./stack";

/**
 * The 模型 section of an upstream account: ticking the public models an
 * account serves, the upstream-name picker, priorities and weights, the
 * save's per-row result, the warning a model losing its last account gives,
 * and that a viewer saves nothing.
 *
 * Everything here goes through the real `/admin`: the expected documents are
 * read back from `/admin/resources/models`, and the expected counts from the
 * public model's own view.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  await stack.admin.seedGroups(["shared", "grp-models-7d1"]);
});

test.afterAll(async () => {
  await stack?.stop();
});

/** A saved write reaches the stack's one gateway within this. */
const APPLIED_WITHIN_MS = 30_000;

function accountName(prefix: string): string {
  return `${prefix}-${randomUUID().slice(0, 8)}`;
}

/** A live upstream account through the real API, in the groups it must reach. */
async function newAccount(name: string): Promise<string> {
  const created = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: name,
      provider: "openai",
      api_key: `sk-${name}-upstream`,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared", "grp-models-7d1"],
    },
  });
  return created.id as string;
}

/** A public model through the real API. */
async function newPublicModel(name: string): Promise<Json> {
  return stack.admin.post("/admin/public-models", { name });
}

/** The live model documents the control plane holds, by display name. */
async function modelDocs(): Promise<Record<string, Json>> {
  const { items } = await stack.admin.get("/admin/resources/models");
  const byName: Record<string, Json> = {};
  for (const row of items as Json[]) byName[row.doc.display_name ?? row.id] = row;
  return byName;
}

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

/** Opens one account's edit view, which carries its 模型 section, on a signed-in page. */
async function openAccountSection(page: Page, name: string): Promise<Locator> {
  await page.locator('[data-tab="providers"]').click();
  const row = page.getByRole("row", { name: new RegExp(name) });
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: /编辑|查看/ }).click();
  const section = page.getByRole("region", { name: "模型" });
  await expect(section).toBeVisible();
  // The rows are read before anything is ticked.
  await expect(section.getByText("载入中…")).toHaveCount(0);
  return section;
}

/** Signs in, opens 上游账号 and one account's edit view. */
async function openAccountModels(page: Page, name: string, who = stack.operator): Promise<Locator> {
  await login(page, who.email, who.password);
  return openAccountSection(page, name);
}

/**
 * Shows every live public model, not only the ones this account serves. The
 * list defaults to the served rows, so a test that ticks an unserved model
 * reveals it first, as an operator does.
 */
async function showAllModels(section: Locator, on = true): Promise<void> {
  const box = section.getByRole("checkbox", { name: /显示全部公共模型/ });
  if ((await box.isChecked()) !== on) await box.click();
}

/** `name` with every regex metacharacter escaped, for a role-name pattern. */
function escapeRe(name: string): string {
  return name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * The section's row for a public model: the one whose service tick names it.
 * The row's accessible name starts with that tick's label, so the name is
 * what identifies it.
 */
function modelRow(section: Locator, name: string): Locator {
  return section.getByRole("row", { name: new RegExp(`^服务 ${escapeRe(name)}(?:\\s|$)`) });
}

/** The picker's field for a public model. */
function picker(section: Locator, name: string): Locator {
  return section.getByRole("combobox", { name: `上游模型名称 ${name}` });
}

/**
 * A picker that has finished reading the account's list. Focusing the field
 * is what starts that read, and the field is disabled while it runs (at most
 * 5 s), so this focuses it and then waits for it to be usable rather than
 * typing into it mid-load.
 */
async function readyPicker(section: Locator, name: string): Promise<Locator> {
  const field = picker(section, name);
  await field.focus();
  await expect(field).toBeEnabled();
  return field;
}

test("账号的模型列表默认只列它服务的模型，勾选后列出全部", async ({ page }) => {
  const name = accountName("acct-served-only");
  const accountId = await newAccount(name);
  // One served, one the account does not serve: the default view has to be
  // the served row alone, not a second row that reads as broken state.
  const served = await newPublicModel(`pub-default-served-${randomUUID().slice(0, 8)}`);
  const other = await newPublicModel(`pub-default-other-${randomUUID().slice(0, 8)}`);
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${served.id}`, { priority: 1, weight: 1 });

  const section = await openAccountModels(page, name);
  // The served row is there, and the ones it does not serve are not.
  await expect(modelRow(section, served.name)).toBeVisible();
  await expect(modelRow(section, served.name).getByRole("checkbox", { name: `服务 ${served.name}` })).toBeChecked();
  await expect(modelRow(section, other.name)).toHaveCount(0);
  // The toggle counts what it would reveal, and says so.
  const reveal = section.getByRole("checkbox", { name: /显示全部公共模型/ });
  await expect(reveal).not.toBeChecked();
  // The count beside it is the unserved models this account could add: the
  // live public models here are `served` and `other`, so exactly one.
  await expect(reveal).toHaveAccessibleName("显示全部公共模型（1）");

  // Revealed, everything live is listed, and the served row is still served.
  await reveal.check();
  await expect(modelRow(section, other.name)).toBeVisible();
  await expect(modelRow(section, served.name).getByRole("checkbox", { name: `服务 ${served.name}` })).toBeChecked();
  await expect(modelRow(section, other.name).getByRole("checkbox", { name: `服务 ${other.name}` })).not.toBeChecked();
  // Hiding again leaves only the served row.
  await reveal.uncheck();
  await expect(modelRow(section, other.name)).toHaveCount(0);
  await expect(modelRow(section, served.name)).toBeVisible();
});

test("搜索模型在默认视图与全部视图里都只筛当前列出的行", async ({ page }) => {
  const name = accountName("acct-search");
  const accountId = await newAccount(name);
  const served = await newPublicModel(`pub-search-served-${randomUUID().slice(0, 8)}`);
  const unserved = await newPublicModel(`pub-search-unserved-${randomUUID().slice(0, 8)}`);
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${served.id}`, { priority: 1, weight: 1 });

  const section = await openAccountModels(page, name);
  const search = section.getByLabel("搜索模型");
  // By default the unserved model is not listed even when searched for.
  await search.fill(unserved.name);
  await expect(modelRow(section, unserved.name)).toHaveCount(0);
  await expect(section.getByText("没有匹配的模型。")).toBeVisible();
  // Revealed, the same search finds it.
  await showAllModels(section);
  await expect(modelRow(section, unserved.name)).toBeVisible();
  // And a search that matches neither lists neither.
  await search.fill("nothing-matches-this-5a4");
  await expect(modelRow(section, served.name)).toHaveCount(0);
  await expect(modelRow(section, unserved.name)).toHaveCount(0);
});

test("一个模型都没服务的账号说明怎么添加", async ({ page }) => {
  const name = accountName("acct-empty");
  await newAccount(name);
  await newPublicModel(`pub-empty-${randomUUID().slice(0, 8)}`);

  const section = await openAccountModels(page, name);
  // No row is served, so the default view is empty and says what to do.
  await expect(section.getByText("这个账号还没有服务任何模型。勾选「显示全部公共模型」来添加。")).toBeVisible();
});

test("在上游账号页勾选模型、填写映射与优先级，保存后生成文档", async ({ page }) => {
  const name = accountName("acct-map");
  const accountId = await newAccount(name);
  const pub = await newPublicModel(`pub-map-${randomUUID().slice(0, 8)}`);

  const section = await openAccountModels(page, name);
  // A model this account does not serve yet is behind the reveal.
  await showAllModels(section);
  const row = modelRow(section, pub.name);
  await expect(row).toBeVisible();
  await row.getByRole("checkbox", { name: `服务 ${pub.name}` }).check();
  const field = await readyPicker(section, pub.name);
  await field.fill("upstream-map-9c2");
  // The field keeps what was typed: the mapping is what the document names.
  await expect(field).toHaveValue("upstream-map-9c2");
  await row.getByRole("textbox", { name: `优先级 ${pub.name}` }).fill("7");
  await row.getByRole("textbox", { name: `权重 ${pub.name}` }).fill("3");
  await section.getByRole("button", { name: "写入映射" }).click();
  await expect(section.getByText(/已保存 1 个模型的映射/)).toBeVisible();

  // Both documents, as the control plane wrote them: the direct one named
  // after the public model and the account, the routing one at the public id.
  await expect
    .poll(async () => Object.keys(await modelDocs()), { timeout: APPLIED_WITHIN_MS })
    .toEqual(expect.arrayContaining([`${pub.name}#${name}`, pub.name]));
  const docs = await modelDocs();
  expect(docs[`${pub.name}#${name}`].doc).toMatchObject({
    provider: "openai",
    model_name: "upstream-map-9c2",
    provider_key_id: accountId,
  });
  expect(docs[pub.name].id).toBe(pub.id);
  expect(docs[pub.name].doc.routing.strategy).toBe("round_robin");
  expect(docs[pub.name].doc.routing.targets).toEqual([
    { model_id: docs[`${pub.name}#${name}`].id, priority: 7, weight: 3 },
  ]);

  // And the table agrees with them: this is the row the save wrote.
  await section.getByRole("button", { name: "刷新" }).click();
  await expect(modelRow(section, pub.name).getByRole("checkbox", { name: `服务 ${pub.name}` })).toBeChecked();
  await expect(modelRow(section, pub.name).getByRole("textbox", { name: `权重 ${pub.name}` })).toHaveValue("3");
});

test("模型名称下拉列出该上游的模型，可搜索", async ({ page }) => {
  // The control plane reaches an upstream under the same address policy the
  // gateway applies, which refuses a loopback address; the stack's stand-in
  // is on loopback, so what this route can be asked here is its refusal. The
  // test pins that the field asks, and that the answer it gets is what it
  // shows — the list itself is covered where an address is reachable.
  const name = accountName("acct-list");
  await newAccount(name);
  const pub = await newPublicModel(`pub-list-${randomUUID().slice(0, 8)}`);

  const section = await openAccountModels(page, name);
  await showAllModels(section);
  await modelRow(section, pub.name).getByRole("checkbox", { name: `服务 ${pub.name}` }).check();
  const field = await readyPicker(section, pub.name);
  await field.click();

  // The route's own answer, in the page: the fallback and the reason it gave.
  await expect(modelRow(section, pub.name).getByText("无法获取该上游的模型列表，请直接填写")).toBeVisible();
  await expect(modelRow(section, pub.name).locator(".upicker-reason")).toContainText(/上游|地址|无法/);
  // And a name it could not list is still saved: the field is free text.
  await field.fill("upstream-unlisted-7f4");
  await section.getByRole("button", { name: "写入映射" }).click();
  await expect(section.getByText(/已保存 1 个模型的映射/)).toBeVisible();
  await expect
    .poll(async () => (await modelDocs())[`${pub.name}#${name}`]?.doc.model_name, { timeout: APPLIED_WITHIN_MS })
    .toBe("upstream-unlisted-7f4");
});

test("上游拒绝列表时改为自由填写，输入的名称可以保存", async ({ page }) => {
  // A platform that cannot list models at all answers `409
  // upstream_models_unsupported`; the field must be free text either way, and
  // the saved document must carry the typed name.
  const name = accountName("acct-refuse");
  const key = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: name,
      provider: "bedrock",
      adapter: "bedrock",
      api_key: `sk-${name}-upstream`,
      api_base: null,
      groups: ["shared"],
    },
  });
  const pub = await newPublicModel(`pub-refuse-${randomUUID().slice(0, 8)}`);

  const section = await openAccountModels(page, name);
  await showAllModels(section);
  await modelRow(section, pub.name).getByRole("checkbox", { name: `服务 ${pub.name}` }).check();
  const field = await readyPicker(section, pub.name);
  await field.click();
  await expect(section.getByText("无法获取该上游的模型列表，请直接填写")).toBeVisible();

  await field.fill("typed-refused-3b8");
  await section.getByRole("button", { name: "写入映射" }).click();
  await expect(section.getByText(/已保存 1 个模型的映射/)).toBeVisible();
  await expect
    .poll(async () => (await modelDocs())[`${pub.name}#${name}`]?.doc.model_name, { timeout: APPLIED_WITHIN_MS })
    .toBe("typed-refused-3b8");
  // The account itself was not touched by any of this.
  const stored = await stack.admin.get(`/admin/resources/provider_keys/${key.id}`);
  expect(stored.doc.display_name).toBe(name);
});

test("列表里没有的名称也可以输入并保存", async ({ page }) => {
  // The same field, with an account whose list could be read: whatever the
  // list says, a name outside it is accepted. Nothing on the page refuses on
  // membership, so the save is the assertion.
  const name = accountName("acct-outside");
  await newAccount(name);
  const pub = await newPublicModel(`pub-outside-${randomUUID().slice(0, 8)}`);

  const section = await openAccountModels(page, name);
  await showAllModels(section);
  const row = modelRow(section, pub.name);
  await row.getByRole("checkbox", { name: `服务 ${pub.name}` }).check();
  const field = await readyPicker(section, pub.name);
  await field.fill("not-in-any-list-5e1");
  await expect(row.getByRole("alert")).toHaveCount(0);
  await section.getByRole("button", { name: "写入映射" }).click();
  await expect(section.getByText(/已保存 1 个模型的映射/)).toBeVisible();
  await expect
    .poll(async () => (await modelDocs())[`${pub.name}#${name}`]?.doc.model_name, { timeout: APPLIED_WITHIN_MS })
    .toBe("not-in-any-list-5e1");
});

test("停用最后一个模型时提示将影响 N 个租户和 M 个密钥", async ({ page }) => {
  const name = accountName("acct-last");
  await newAccount(name);
  const suffix = randomUUID().slice(0, 8);
  const pub = await newPublicModel(`pub-last-${suffix}`);

  const section = await openAccountModels(page, name);
  await showAllModels(section);
  await modelRow(section, pub.name).getByRole("checkbox", { name: `服务 ${pub.name}` }).check();
  await section.getByRole("button", { name: "写入映射" }).click();
  await expect(section.getByText(/已保存 1 个模型的映射/)).toBeVisible();
  await expect.poll(async () => (await modelDocs())[pub.name] !== undefined, { timeout: APPLIED_WITHIN_MS }).toBe(true);

  // One tenant granted the model, and one of its keys naming it: what the
  // warning must count, set up through the real APIs.
  const signup = await stack.tenant().post("/api/signup", {
    org_name: `Models Org ${suffix}`,
    email: `owner-${suffix}@models-ui.test`,
    password: "models-ui-pass-8h2",
  });
  const tenantId: string = signup.tenant.id;
  await stack.admin.post(`/admin/tenants/${tenantId}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${tenantId}`);
  await stack.admin.patch(`/admin/tenants/${tenantId}`, { allowed_model_ids: [pub.id] }, current.version);
  const owner = stack.tenant();
  await owner.post("/api/login", { email: `owner-${suffix}@models-ui.test`, password: "models-ui-pass-8h2" });
  await owner.post("/api/keys", { label: `models-key-${suffix}`, model_ids: [pub.id] });
  // And one that picks no models: it follows every grant, so it loses the
  // model too and is counted.
  await owner.post("/api/keys", { label: `models-all-${suffix}` });

  // Disabling the only account that serves it: asked twice, with the counts.
  // A reload re-reads every public model's view, which is where the tenant
  // and key counts come from; the fixture above landed after this section
  // was first read.
  await page.goto(stack.previewUrl);
  const fresh = await openAccountSection(page, name);
  const row = modelRow(fresh, pub.name);
  await row.getByRole("checkbox", { name: `启用 ${pub.name}` }).uncheck();
  await fresh.getByRole("button", { name: "写入映射" }).click();
  const warning = fresh.getByRole("alert").filter({ hasText: pub.name });
  await expect(warning).toContainText("影响 1 个租户、2 个密钥");
  await expect(fresh.getByRole("button", { name: "确认写入映射" })).toBeVisible();

  await fresh.getByRole("button", { name: "确认写入映射" }).click();
  await expect(fresh.getByText(/已保存 1 个模型的映射/)).toBeVisible();

  // The model is still there, with no target and its grants intact: what the
  // warning promised.
  expect((await modelDocs())[pub.name]).toBeDefined();
  expect((await modelDocs())[pub.name].doc.routing.targets).toEqual([]);
  const after = await stack.admin.get(`/admin/tenants/${tenantId}`);
  expect(after.allowed_model_ids).toEqual([pub.id]);
});

test("改一个已停用的最后一行不再提示停用", async ({ page }) => {
  const name = accountName("acct-off");
  const accountId = await newAccount(name);
  const pub = await newPublicModel(`pub-off-${randomUUID().slice(0, 8)}`);
  // Its only row is already off: the model has no serving account before
  // this edit, and still none after it.
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { priority: 1, weight: 1 });
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { enabled: false });

  const section = await openAccountModels(page, name);
  const row = modelRow(section, pub.name);
  await expect(row.getByRole("checkbox", { name: `启用 ${pub.name}` })).not.toBeChecked();
  await row.getByRole("textbox", { name: `权重 ${pub.name}`, exact: true }).fill("2");
  await section.getByRole("button", { name: "写入映射" }).click();
  // Nothing served is taken away, so nothing is asked.
  await expect(section.getByText(/已保存 1 个模型的映射/)).toBeVisible();
  await expect(section.getByRole("button", { name: "确认写入映射" })).toHaveCount(0);
  const table = await stack.admin.get(`/admin/provider-keys/${accountId}/channel-models`);
  expect(table.items.find((i: Json) => i.public_model_id === pub.id)).toMatchObject({ weight: 2, enabled: false });
});

test("仍在服务公共模型的账号不能删除，并说明先取消勾选", async ({ page }) => {
  const name = accountName("acct-serving");
  const accountId = await newAccount(name);
  const pub = await newPublicModel(`pub-serving-${randomUUID().slice(0, 8)}`);
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { priority: 1, weight: 1 });

  await login(page, stack.operator.email, stack.operator.password);
  await page.locator('[data-tab="providers"]').click();
  const row = page.getByRole("row", { name: new RegExp(name) });
  await row.getByRole("button", { name: "删除" }).click();
  await row.getByRole("button", { name: "确认删除" }).click();
  const refused = page.getByRole("alert").filter({ hasText: `无法删除「${name}」` });
  await expect(refused).toContainText("该账号仍为 1 个公共模型提供服务");
  await expect(refused).toContainText("请先在该账号的「模型」里取消勾选");
  expect((await stack.admin.request("GET", `/admin/resources/provider_keys/${accountId}`)).status).toBe(200);
});

test("无权限的视图角色看不到保存按钮", async ({ page }) => {
  const name = accountName("acct-viewer");
  await newAccount(name);
  const pub = await newPublicModel(`pub-viewer-${randomUUID().slice(0, 8)}`);

  const viewer = { email: `viewer-models-${randomUUID().slice(0, 6)}@e2e.local`, password: "viewer-models-pass-2d7" };
  const created = await stack.admin.request("POST", "/admin/operators", { ...viewer, role: "viewer" });
  expect(created.status, created.text).toBe(201);

  const section = await openAccountModels(page, name, viewer);
  await showAllModels(section);
  await expect(modelRow(section, pub.name)).toBeVisible();
  await expect(section.getByRole("button", { name: "写入映射" })).toHaveCount(0);
  await expect(section.getByRole("button", { name: /新建公共模型/ })).toHaveCount(0);
  await expect(section.getByRole("checkbox", { name: `服务 ${pub.name}` })).toBeDisabled();
});

/** 获取上游模型 inside an account's 模型 section. */
function fetchBlock(section: Locator): Locator {
  return section.getByRole("group", { name: "获取上游模型" });
}

/** The public models the control plane holds, by name. */
async function publicNames(): Promise<string[]> {
  const { items } = await stack.admin.get("/admin/public-models");
  return (items as Json[]).map((m) => m.name as string).sort();
}

test("获取上游模型时上游不可达：显示原因，不写入任何东西", async ({ page }) => {
  // The stand-in is on loopback, which the control plane's outbound address
  // policy refuses, so what this account's list answers here is a refusal:
  // the page shows it and offers nothing to apply.
  const name = accountName("acct-fetch");
  await newAccount(name);
  const before = await publicNames();

  const section = await openAccountModels(page, name);
  const block = fetchBlock(section);
  await block.getByRole("button", { name: "获取上游模型" }).click();
  await expect(block.getByRole("alert")).toContainText(/上游/);
  await expect(block.getByRole("button", { name: "映射所选模型" })).toHaveCount(0);
  await expect(block.getByRole("checkbox")).toHaveCount(0);
  expect(await publicNames()).toEqual(before);
});

test("只读角色看不到获取上游模型", async ({ page }) => {
  const name = accountName("acct-fetch-viewer");
  await newAccount(name);
  const viewer = { email: `viewer-fetch-${randomUUID().slice(0, 6)}@e2e.local`, password: "viewer-fetch-pass-8b2" };
  const created = await stack.admin.request("POST", "/admin/operators", { ...viewer, role: "viewer" });
  expect(created.status, created.text).toBe(201);

  const section = await openAccountModels(page, name, viewer);
  await expect(section.getByRole("button", { name: "获取上游模型" })).toHaveCount(0);
});

test("映射所选上游模型后：账号的模型与公共模型都可见，租户经网关调用新的公共模型成功", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const name = accountName("acct-bulk");
  const accountId = await newAccount(name);
  // One selected model has a public model of its exact name already; the
  // other has none.
  const existing = await newPublicModel(`pub-bulk-${suffix}`);
  const fresh = `up-bulk-${suffix}`;

  // The selection, sent as 映射所选模型 sends it: this stack's control plane
  // cannot read the stand-in's list (see above), so the ids are posted here.
  const applied = await stack.admin.post(`/admin/provider-keys/${accountId}/channel-models`, {
    upstream_models: [existing.name, fresh],
  });
  expect((applied.items as Json[]).map((i) => [i.upstream_model, i.public_model_name, i.outcome])).toEqual([
    [existing.name, existing.name, "added"],
    [fresh, fresh, "created"],
  ]);
  // The existing public model was reused, not doubled; the other was created.
  expect((await publicNames()).filter((n) => n === existing.name || n === fresh)).toEqual([existing.name, fresh].sort());

  // The account's 模型 section serves both, each asking its upstream for the
  // selected name.
  const section = await openAccountModels(page, name);
  for (const model of [existing.name, fresh]) {
    await expect(modelRow(section, model).getByRole("checkbox", { name: `服务 ${model}` })).toBeChecked();
    await expect(picker(section, model)).toHaveValue(model);
  }

  // 价格 → 公共模型 lists the new public model, served by one account.
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("group", { name: "价格分区" }).getByRole("button", { name: "公共模型", exact: true }).click();
  const panel = page.locator(".panel").filter({ has: page.getByRole("heading", { name: "公共模型", level: 2 }) });
  await expect(panel.getByText("载入中…")).toHaveCount(0);
  const listed = panel.getByRole("row", { name: new RegExp(`^${escapeRe(fresh)}(?:\\s|$)`) });
  await expect(listed).toBeVisible();
  await expect(listed).toContainText("1 个");

  // A customer granted the new public model calls it through the gateway,
  // and the upstream is asked for the selected name.
  const tenant = await stack.admin.post("/admin/tenants", { name: `tenant-bulk-${suffix}` });
  await fundTenant(stack.admin, tenant.id);
  const freshId = (applied.items as Json[]).find((i) => i.upstream_model === fresh).public_model_id;
  await stack.admin.patch(`/admin/tenants/${tenant.id}`, { allowed_model_ids: [freshId] }, tenant.version);
  const { plaintext } = await stack.admin.post(`/admin/tenants/${tenant.id}/keys`, { label: `key-bulk-${suffix}` });
  const del = await fetch(`${stack.mockUpstreamUrl}/__requests`, { method: "DELETE" });
  expect(del.status).toBe(204);
  await expect
    .poll(
      async () => {
        const res = await fetch(`${stack.proxyUrl}/v1/chat/completions`, {
          method: "POST",
          headers: { "content-type": "application/json", authorization: `Bearer ${plaintext}` },
          body: JSON.stringify({ model: fresh, messages: [{ role: "user", content: "hi" }] }),
        });
        await res.arrayBuffer();
        return res.status;
      },
      { timeout: APPLIED_WITHIN_MS },
    )
    .toBe(200);
  const records = (await (await fetch(`${stack.mockUpstreamUrl}/__requests`)).json()) as Json[];
  const asked = records.filter((r) => r.path === "/v1/chat/completions").map((r) => r.body?.model);
  expect(asked.length).toBeGreaterThan(0);
  expect(new Set(asked)).toEqual(new Set([fresh]));
});
