import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, fundTenant, startStack, type Json, type Stack } from "./stack";

/**
 * The model catalog and routing tab: direct models on an upstream account,
 * routing models over them with schema-driven strategies, the groups each
 * target can serve, a rename that keeps every grant, and the two kinds of
 * delete (refused while configuration references the model; withdrawn from
 * tenant grants otherwise). Against the real control plane and the real
 * managed gateway, whose upstream is the stack's stand-in.
 */

let stack: Stack;

test.beforeAll(async () => {

  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  // Every write that names a group names a live one: the accounts and the
  // tenant edits below included. A name a test here creates itself is
  // skipped.
  await stack.admin.seedGroups(["grp-alpha-7c1", "grp-beta-3e9", "grp-view-5e8"]);
});

test.afterAll(async () => {
  await stack?.stop();
});

/** A saved document reaches the stack's one gateway within this. */
const APPLIED_WITHIN_MS = 30_000;

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令").fill(password);
  await page.getByRole("button", { name: "登录" }).click();
}

async function openModels(page: Page, as?: { email: string; password: string }): Promise<void> {
  const who = as ?? stack.operator;
  await login(page, who.email, who.password);
  await page.getByRole("tab", { name: "模型与路由" }).click();
}

/** `name` with every regex metacharacter escaped, for a role-name pattern. */
function escapeRe(name: string): string {
  return name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/** The catalog row whose name cell is exactly `name`. */
function modelRow(page: Page, name: string): Locator {
  return page.getByRole("row").filter({ has: page.getByRole("cell", { name, exact: true }) });
}

function badge(row: Locator): Locator {
  return row.locator("[data-state]");
}

/** The target rows of a routing form, in order. */
function targetRows(form: Locator): Locator {
  return form.getByRole("group", { name: /^目标 \d+$/ });
}

async function createAccount(doc: Json): Promise<Json> {
  return stack.admin.post("/admin/resources/provider_keys", { doc });
}

async function createModel(doc: Json): Promise<Json> {
  return stack.admin.post("/admin/resources/models", { doc });
}

async function directModel(name: string, accountId: string, upstreamModel = "gpt-4o-mini"): Promise<Json> {
  return createModel({ display_name: name, provider: "openai", model_name: upstreamModel, provider_key_id: accountId });
}

/**
 * An upstream account the stand-in answers for, in `groups` and in the group
 * the spec's internal key names: that key reaches only the accounts of the
 * one group it names.
 */
async function mockAccount(name: string, groups: string[]): Promise<Json> {
  await internalGroupId();
  return createAccount({
    display_name: name,
    provider: "openai",
    api_key: `sk-e2e-${name}-upstream`,
    api_base: `${stack.mockUpstreamUrl}/v1`,
    groups: [...groups, INTERNAL_GROUP],
  });
}

async function modelNamed(name: string): Promise<Json | undefined> {
  const { items } = await stack.admin.get("/admin/resources/models");
  return (items as Json[]).find((v) => v.doc.display_name === name);
}

async function status(path: string): Promise<number> {
  return (await stack.admin.request("GET", path)).status;
}

/** The group every mock account of this spec joins, and its key names. */
const INTERNAL_GROUP = "grp-models-ui-7c1";
const internalGroups = new WeakMap<Stack, Promise<string>>();
function internalGroupId(): Promise<string> {
  let group = internalGroups.get(stack);
  if (!group) {
    group = stack.admin
      .post("/admin/provider-groups", {
        name: INTERNAL_GROUP,
        display_name: "模型自用 7c1",
        default_cny_per_usd: "7.2",
      })
      .then((created: Json) => created.id as string);
    internalGroups.set(stack, group);
  }
  return group;
}

/** A key of the platform's own tenant that may call `models` by name. */
async function internalKey(label: string, models: string[]): Promise<string> {
  const { items } = await stack.admin.get("/admin/tenants");
  const internal = (items as Json[]).find((t) => t.is_internal);
  expect(internal, JSON.stringify(items)).toBeDefined();
  // Every key names one group; the platform's own tenant has no assignment
  // list, so it names the one this spec's accounts join.
  const { plaintext } = await stack.admin.post(`/admin/tenants/${internal.id}/keys`, {
    label,
    allowed_models: models,
    group_id: await internalGroupId(),
  });
  return plaintext;
}

/** A customer tenant granted `modelIds`, and one key that follows its grants. */
async function grantedTenant(name: string, modelIds: string[]): Promise<{ tenant: Json; key: string }> {
  const created = await stack.admin.post("/admin/tenants", { name });
  await fundTenant(stack.admin, created.id);
  const tenant = await stack.admin.patch(`/admin/tenants/${created.id}`, { allowed_model_ids: modelIds }, created.version);
  const { plaintext } = await stack.admin.post(`/admin/tenants/${created.id}/keys`, { label: `key-${name}` });
  return { tenant, key: plaintext };
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

/** `"client error"` for a 4xx status, else the status itself, so a failed check names what it got. */
function clientError(status: number): string | number {
  return status >= 400 && status < 500 ? "client error" : status;
}

/** The upstream `model` of each chat request the stand-in received. */
async function upstreamModels(): Promise<string[]> {
  const res = await fetch(`${stack.mockUpstreamUrl}/__requests`);
  const records = (await res.json()) as Json[];
  return records.filter((r) => r.path === "/v1/chat/completions").map((r) => r.body?.model);
}

async function clearUpstream(): Promise<void> {
  const res = await fetch(`${stack.mockUpstreamUrl}/__requests`, { method: "DELETE" });
  expect(res.status).toBe(204);
}

test("模型与路由说明：租户可见的模型在上游账号的模型里建为公共模型", async ({ page }) => {
  await openModels(page);
  const panel = page.locator(".page-header").filter({ has: page.getByRole("heading", { name: /模型与路由/, level: 2 }) });
  const hint = panel.locator(".hint").filter({ hasText: "上游账号 → 模型" });
  await expect(hint).toHaveCount(1);
  await expect(hint).toContainText("公共模型");
  await expect(hint).toContainText("分组定价");
  await expect(hint).toContainText("授权");
});

test("直连模型：保存后网关能用它的对外名称调用", async ({ page }) => {
  const account = await mockAccount("acct-chat-e2e", ["shared"]);

  await openModels(page);
  await page.getByRole("button", { name: "新增直连模型" }).click();
  const form = page.getByRole("form", { name: "新增直连模型" });
  await form.getByLabel("对外名称").fill("chat-e2e");
  await form.getByLabel("上游账号").selectOption({ label: "acct-chat-e2e" });
  // The provider comes from the account.
  await expect(form.getByLabel("供应商标识")).toHaveValue("openai");
  await form.getByLabel("上游模型").fill("gpt-4o-mini");
  await form.getByRole("button", { name: "保存" }).click();

  const row = modelRow(page, "chat-e2e");
  await expect(badge(row)).toHaveAttribute("data-state", "applied", { timeout: APPLIED_WITHIN_MS });
  await expect(row).toContainText("直连");
  await expect(row).toContainText("acct-chat-e2e");
  const stored = await modelNamed("chat-e2e");
  expect(stored?.doc).toEqual({
    display_name: "chat-e2e",
    provider_key_id: account.id,
    provider: "openai",
    model_name: "gpt-4o-mini",
  });

  // A key created after the save calls the model by its platform name.
  const key = await internalKey("chat-e2e-key", ["chat-e2e"]);
  await clearUpstream();
  await expect.poll(() => chat(key, "chat-e2e"), { timeout: APPLIED_WITHIN_MS }).toBe(200);
  expect(await upstreamModels()).toEqual(["gpt-4o-mini"]);
});

test("路由模型：选路策略的选项来自 schema，目标行显示各自可服务的分组", async ({ page }) => {
  const alphaAccount = await mockAccount("acct-alpha-e2e", ["grp-alpha-7c1"]);
  const betaAccount = await mockAccount("acct-beta-e2e", ["grp-beta-3e9"]);
  const alpha = await directModel("chat-alpha-e2e", alphaAccount.id, "gpt-alpha-up-e2e");
  const beta = await directModel("chat-beta-e2e", betaAccount.id, "gpt-beta-up-e2e");
  const schema = await stack.admin.get("/admin/schemas/model");
  const named = schema.definitions.RoutingStrategy;
  const strategies: string[] = named.enum ?? named.oneOf.flatMap((b: Json) => b.enum ?? [b.const]);
  const preset: string = schema.definitions.Routing.properties.strategy.default;
  const chosen = strategies.find((s) => s !== preset);
  expect(strategies.length).toBeGreaterThan(1);

  await openModels(page);
  await page.getByRole("button", { name: "新增路由模型" }).click();
  const form = page.getByRole("form", { name: "新增路由模型" });
  await expect(form.getByLabel("选路策略").locator("option")).toHaveText(strategies);
  await expect(form.getByLabel("选路策略")).toHaveValue(preset);

  await form.getByLabel("对外名称").fill("route-e2e");
  await form.getByLabel("选路策略").selectOption(chosen!);
  const first = targetRows(form).nth(0);
  await first.getByLabel("目标模型").selectOption({ label: "chat-alpha-e2e" });
  await first.getByLabel("权重").fill("3");
  await expect(first).toContainText("grp-alpha-7c1");
  await expect(first).not.toContainText("grp-beta-3e9");
  await form.getByRole("button", { name: "添加目标" }).click();
  const second = targetRows(form).nth(1);
  await second.getByLabel("目标模型").selectOption({ label: "chat-beta-e2e" });
  await second.getByLabel("优先级").fill("1");
  await expect(second).toContainText("grp-beta-3e9");
  await expect(second).not.toContainText("grp-alpha-7c1");
  await form.getByRole("button", { name: "保存" }).click();

  const row = modelRow(page, "route-e2e");
  await expect(badge(row)).toHaveAttribute("data-state", "applied", { timeout: APPLIED_WITHIN_MS });
  await expect(row).toContainText("路由");
  for (const token of ["chat-alpha-e2e", "grp-alpha-7c1", "chat-beta-e2e", "grp-beta-3e9"]) {
    await expect(row).toContainText(token);
  }
  const stored = await modelNamed("route-e2e");
  expect(stored?.doc).toEqual({
    display_name: "route-e2e",
    routing: {
      strategy: chosen,
      targets: [
        { model_id: alpha.id, weight: 3 },
        { model_id: beta.id, priority: 1 },
      ],
    },
  });

  // The higher priority tier is tried first.
  const key = await internalKey("route-e2e-key", ["route-e2e"]);
  await clearUpstream();
  await expect.poll(() => chat(key, "route-e2e"), { timeout: APPLIED_WITHIN_MS }).toBe(200);
  expect(await upstreamModels()).toEqual(["gpt-beta-up-e2e"]);
});

test("改名后，按 id 获授权的租户 key 用新名字调用仍成功", async ({ page }) => {
  const account = await mockAccount("acct-rename-e2e", ["shared"]);
  const model = await directModel("chat-rename-e2e", account.id, "gpt-rename-up-e2e");
  // A route that names the model by id, and a key for the route.
  await createModel({ display_name: "route-rename-e2e", routing: { targets: [{ model_id: model.id }] } });
  const routeKey = await internalKey("route-rename-e2e-key", ["route-rename-e2e"]);
  const { tenant, key } = await grantedTenant("tenant-rename-e2e", [model.id]);
  // Before the rename: the grant and the route both reach the model.
  await expect.poll(() => chat(key, "chat-rename-e2e"), { timeout: APPLIED_WITHIN_MS }).toBe(200);
  await clearUpstream();
  await expect.poll(() => chat(routeKey, "route-rename-e2e"), { timeout: APPLIED_WITHIN_MS }).toBe(200);
  expect(await upstreamModels()).toEqual(["gpt-rename-up-e2e"]);

  await openModels(page);
  await modelRow(page, "chat-rename-e2e").getByRole("button", { name: "编辑" }).click();
  const form = page.getByRole("form", { name: /chat-rename-e2e/ });
  await expect(form).toContainText("改名不会影响已授权的租户和引用它的路由、缓存策略");
  await form.getByLabel("对外名称").fill("chat-e2e-renamed");
  await form.getByRole("button", { name: "保存" }).click();

  const row = modelRow(page, "chat-e2e-renamed");
  await expect(badge(row)).toHaveAttribute("data-state", "applied", { timeout: APPLIED_WITHIN_MS });
  await expect(modelRow(page, "chat-rename-e2e")).toHaveCount(0);
  await expect(modelRow(page, "route-rename-e2e")).toContainText("chat-e2e-renamed");
  // The same model under its new name, and the grant still names it.
  const renamed = await stack.admin.get(`/admin/resources/models/${model.id}`);
  expect(renamed.doc.display_name).toBe("chat-e2e-renamed");
  expect((await stack.admin.get(`/admin/tenants/${tenant.id}`)).allowed_model_ids).toEqual([model.id]);

  expect(await chat(key, "chat-e2e-renamed")).toBe(200);
  expect(clientError(await chat(key, "chat-rename-e2e"))).toBe("client error");
  // The route still reaches the same upstream model.
  await clearUpstream();
  expect(await chat(routeKey, "route-rename-e2e")).toBe(200);
  expect(await upstreamModels()).toEqual(["gpt-rename-up-e2e"]);
});

test("被路由目标引用的模型不能删", async ({ page }) => {
  const account = await mockAccount("acct-held-e2e", ["shared"]);
  const held = await directModel("chat-held-e2e", account.id);
  await createModel({ display_name: "route-held-e2e", routing: { targets: [{ model_id: held.id }] } });

  await openModels(page);
  const row = modelRow(page, "chat-held-e2e");
  await row.getByRole("button", { name: "删除" }).click();
  await row.getByRole("button", { name: "确认删除" }).click();
  const refused = page.getByRole("alert").filter({ hasText: "被以下资源引用：" });
  await expect(refused).toContainText("route-held-e2e");
  expect(await status(`/admin/resources/models/${held.id}`)).toBe(200);
  await expect(row).toBeVisible();
});

test("删除已授权给租户的模型：确认前点名受影响的租户，取消不删，确认后从授权中移除", async ({ page }) => {
  const account = await mockAccount("acct-grant-e2e", ["shared"]);
  const model = await directModel("chat-grant-e2e", account.id);
  const { tenant, key } = await grantedTenant("tenant-grant-e2e", [model.id]);
  await stack.admin.post("/admin/tenants", { name: "tenant-other-e2e" });
  await expect.poll(() => chat(key, "chat-grant-e2e"), { timeout: APPLIED_WITHIN_MS }).toBe(200);

  await openModels(page);
  const row = modelRow(page, "chat-grant-e2e");
  const warning = page.getByRole("region", { name: "删除模型" });
  await row.getByRole("button", { name: "删除" }).click();
  await expect(warning).toContainText("删除后，该模型会从所有租户的授权里移除");
  await expect(warning).toContainText("tenant-grant-e2e");
  await expect(warning).not.toContainText("tenant-other-e2e");
  await row.getByRole("button", { name: "取消" }).click();
  await expect(warning).toHaveCount(0);
  await expect(row.getByRole("button", { name: "确认删除" })).toHaveCount(0);
  expect(await status(`/admin/resources/models/${model.id}`)).toBe(200);

  await row.getByRole("button", { name: "删除" }).click();
  await row.getByRole("button", { name: "确认删除" }).click();
  await expect(modelRow(page, "chat-grant-e2e")).toHaveCount(0);
  expect(await status(`/admin/resources/models/${model.id}`)).toBe(404);
  expect((await stack.admin.get(`/admin/tenants/${tenant.id}`)).allowed_model_ids).toEqual([]);
  await expect
    .poll(async () => clientError(await chat(key, "chat-grant-e2e")), { timeout: APPLIED_WITHIN_MS })
    .toBe("client error");
});

test("移除路由目标要两步确认：取消保留，确认后移除，保存后网关只走剩下的目标", async ({ page }) => {
  const account = await mockAccount("acct-trim-e2e", ["shared"]);
  const keep = await directModel("chat-keep-t-e2e", account.id, "gpt-keep-up-e2e");
  const drop = await directModel("chat-drop-t-e2e", account.id, "gpt-drop-up-e2e");
  // `max_fallbacks` is a field the form does not show.
  await createModel({
    display_name: "route-trim-e2e",
    routing: { max_fallbacks: 1, targets: [{ model_id: drop.id, priority: 1 }, { model_id: keep.id }] },
  });
  const route = await modelNamed("route-trim-e2e");
  const key = await internalKey("route-trim-e2e-key", ["route-trim-e2e"]);
  // Before the edit the higher tier, the target to be removed, serves.
  await clearUpstream();
  await expect.poll(() => chat(key, "route-trim-e2e"), { timeout: APPLIED_WITHIN_MS }).toBe(200);
  expect(await upstreamModels()).toEqual(["gpt-drop-up-e2e"]);

  await openModels(page);
  await modelRow(page, "route-trim-e2e").getByRole("button", { name: "编辑" }).click();
  const form = page.getByRole("form", { name: /route-trim-e2e/ });
  await expect(targetRows(form)).toHaveCount(2);
  const first = targetRows(form).nth(0);
  await expect(first.getByLabel("目标模型").locator("option:checked")).toHaveText("chat-drop-t-e2e");
  await first.getByRole("button", { name: "移除" }).click();
  await first.getByRole("button", { name: "取消" }).click();
  await expect(targetRows(form)).toHaveCount(2);
  await expect(first.getByRole("button", { name: "确认移除" })).toHaveCount(0);

  await first.getByRole("button", { name: "移除" }).click();
  await first.getByRole("button", { name: "确认移除" }).click();
  await expect(targetRows(form)).toHaveCount(1);
  await expect(targetRows(form).nth(0).getByLabel("目标模型").locator("option:checked")).toHaveText("chat-keep-t-e2e");
  // Nothing is written before the save.
  expect((await stack.admin.get(`/admin/resources/models/${route.id}`)).doc.routing.targets).toHaveLength(2);
  await form.getByRole("button", { name: "保存" }).click();

  // Until the save is answered the row shows the rollout it was listed with,
  // which may already read `applied`: wait for the answer (the form closes),
  // so the badge below is the saved version's.
  await expect(form).toHaveCount(0);
  await expect(badge(modelRow(page, "route-trim-e2e"))).toHaveAttribute("data-state", "applied", {
    timeout: APPLIED_WITHIN_MS,
  });
  // Only the targets changed: the rest of the routing block is as it was.
  const saved = await stack.admin.get(`/admin/resources/models/${route.id}`);
  expect(saved.doc.routing).toEqual({ max_fallbacks: 1, targets: [{ model_id: keep.id }] });
  await clearUpstream();
  expect(await chat(key, "route-trim-e2e")).toBe(200);
  expect(await upstreamModels()).toEqual(["gpt-keep-up-e2e"]);
});

test("只读运维看得到模型与目标，没有任何编辑入口", async ({ page }) => {
  const account = await mockAccount("acct-view-e2e", ["grp-view-5e8"]);
  const target = await directModel("chat-view-e2e", account.id);
  await createModel({ display_name: "route-view-e2e", routing: { targets: [{ model_id: target.id }] } });
  const viewer = { email: "viewer-models-e2e@e2e.local", password: "viewer-models-pass-4b9" };
  await stack.admin.post("/admin/operators", { ...viewer, role: "viewer" });

  await openModels(page, viewer);
  const row = modelRow(page, "route-view-e2e");
  await expect(row).toContainText("grp-view-5e8");
  const main = page.locator("main");
  for (const name of ["新增直连模型", "新增路由模型", "编辑", "删除"]) {
    await expect(main.getByRole("button", { name, exact: true })).toHaveCount(0);
  }
  await row.getByRole("button", { name: "查看" }).click();
  const form = page.getByRole("form", { name: /route-view-e2e/ });
  await expect(form.getByLabel("对外名称")).toBeDisabled();
  await expect(form.getByLabel("选路策略")).toBeDisabled();
  await expect(targetRows(form).nth(0).getByLabel("目标模型")).toBeDisabled();
  for (const name of ["保存", "添加目标", "移除"]) {
    await expect(form.getByRole("button", { name, exact: true })).toHaveCount(0);
  }
  await expect(form.getByRole("button", { name: "关闭" })).toBeVisible();
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

test("所选上游账号在保存前被删掉：被拒的原因标在上游账号旁，模型没有保存", async ({ page }) => {
  const account = await mockAccount("acct-gone-e2e", ["shared"]);

  await openModels(page);
  await page.getByRole("button", { name: "新增直连模型" }).click();
  const form = page.getByRole("form", { name: "新增直连模型" });
  await form.getByLabel("对外名称").fill("gone-e2e");
  await form.getByLabel("上游账号").selectOption({ label: "acct-gone-e2e" });
  await form.getByLabel("上游模型").fill("gpt-4o-mini");
  // Another operator deletes the account before the save.
  await stack.admin.del(`/admin/resources/provider_keys/${account.id}`, account.version);
  await form.getByRole("button", { name: "保存" }).click();

  const field = form.getByLabel("上游账号");
  await expect(field).toHaveAttribute("aria-invalid", "true");
  await expect(field).toHaveAccessibleDescription(/\S/);
  // The reason is at the field, not repeated in the note under the form.
  await expect(form.getByRole("alert")).toBeVisible();
  await expect(form.getByRole("alert")).not.toContainText(await description(field));
  await expect(form.getByLabel("对外名称")).not.toHaveAttribute("aria-invalid", "true");
  expect(await modelNamed("gone-e2e")).toBeUndefined();
});

test("刷新后列出页面打开后才新增的模型", async ({ page }) => {
  const account = await mockAccount("acct-late-model-e2e", ["shared"]);
  await openModels(page);
  // The page has read its list before the model is written.
  await expect(page.getByRole("button", { name: "新增直连模型" })).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
  await directModel("late-model-e2e", account.id);
  await expect(modelRow(page, "late-model-e2e")).toHaveCount(0);

  await page.getByRole("button", { name: "刷新" }).click();
  await expect(modelRow(page, "late-model-e2e")).toBeVisible();
});

/** A public model through the real API. */
async function newPublicModel(name: string): Promise<Json> {
  return stack.admin.post("/admin/public-models", { name });
}

/** Maps `publicModelId` on this account, which is how the generate-a-model path starts. */
async function serve(accountId: string, publicModelId: string): Promise<void> {
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${publicModelId}`, {
    priority: 1,
    weight: 1,
  });
}

/** The 分类 cell of a row whose 对外名称 is exactly `name`. */
function sourceCell(page: Page, name: string): Locator {
  return modelRow(page, name).getByRole("cell").nth(1);
}

/** Shows or hides the 内部直连 rows the list leaves out by default. */
async function showInternal(page: Page, on: boolean): Promise<void> {
  const box = page.getByRole("checkbox", { name: /显示内部直连模型/ });
  if ((await box.isChecked()) !== on) await box.click();
}

/** A live public model's own view. */
async function publicView(id: string): Promise<Json> {
  return stack.admin.get(`/admin/public-models/${id}`);
}

/** The routing block of the model document at `id`, which must be live. */
async function routingOf(id: string): Promise<Json> {
  return (await stack.admin.get(`/admin/resources/models/${id}`)).doc.routing;
}

test("公共模型的路由设置在这里改：保存后网关照它选路，之后账号上的勾选不会把它改回去", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-route-settings-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const publicName = `pub-route-settings-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(account.id, pub.id);
  const directName = `${publicName}#${accountName}`;

  await openModels(page);
  const row = modelRow(page, publicName);
  await expect(row).toContainText("路由");
  await row.getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑模型「${publicName}」` });

  // A generated model starts on the generator's strategy, which is not the
  // schema's own default, and on no setting of its own.
  await expect(form.getByLabel("选路策略")).toHaveValue("round_robin");
  await expect(form.getByLabel("重试次数")).toHaveValue("");

  await form.getByLabel("选路策略").selectOption("consistent_hash");
  await form.getByLabel("重试次数").fill("2");
  await form.getByLabel("额外重试的状态码").fill("408, 429");
  await form.getByLabel("429 也参与重试和切换").check();
  await form.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText(`已保存「${publicName}」的路由设置`)).toBeVisible();

  // The control plane stored what the operator set, and the document carries
  // it: this is what a hand edit of the document could never do, since the
  // document is owned by the public model (`409 managed_resource`).
  await expect.poll(() => routingOf(pub.id), { timeout: APPLIED_WITHIN_MS }).toMatchObject({
    strategy: "consistent_hash",
    retries: 2,
    fallback_on_statuses: [408, 429],
    retry_on_429: true,
  });
  expect((await publicView(pub.id)).routing).toEqual({
    strategy: "consistent_hash",
    retries: 2,
    fallback_on_statuses: [408, 429],
    retry_on_429: true,
  });

  // Now an account change on the same model — a tick that regenerates the
  // routing document. Before this change the generator wrote a hard-coded
  // `round_robin` on every regeneration, so this second write reverted the
  // operator's choice, silently.
  await stack.admin.put(`/admin/provider-keys/${account.id}/channel-models/${pub.id}`, {
    priority: 3,
    weight: 1,
  });
  const after = await routingOf(pub.id);
  expect(after).toMatchObject({
    strategy: "consistent_hash",
    retries: 2,
    fallback_on_statuses: [408, 429],
    retry_on_429: true,
  });
  // The tick still decides the targets, which is the one field the generator
  // owns outright.
  expect(after.targets).toEqual([
    { model_id: (await modelNamed(directName)).id, priority: 3, weight: 1 },
  ]);

  // And the page shows the settings it stored when it is opened again.
  await page.reload();
  await page.getByRole("tab", { name: "模型与路由" }).click();
  const again = modelRow(page, publicName);
  await again.getByRole("button", { name: "编辑", exact: true }).click();
  const reopened = page.getByRole("form", { name: `编辑模型「${publicName}」` });
  await expect(reopened.getByLabel("选路策略")).toHaveValue("consistent_hash");
  await expect(reopened.getByLabel("重试次数")).toHaveValue("2");
  await expect(reopened.getByLabel("额外重试的状态码")).toHaveValue("408, 429");
  await expect(reopened.getByLabel("429 也参与重试和切换")).toBeChecked();
});

test("从路由表单里移除一个目标：那个账号的映射随之取消，它不再出现在路由里", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const keepName = `acct-target-keep-${suffix}`;
  const dropName = `acct-target-drop-${suffix}`;
  const keep = await mockAccount(keepName, ["shared"]);
  const drop = await mockAccount(dropName, ["shared"]);
  const publicName = `pub-target-remove-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(keep.id, pub.id);
  await serve(drop.id, pub.id);
  const keptDirect = `${publicName}#${keepName}`;
  const droppedDirect = `${publicName}#${dropName}`;

  await openModels(page);
  await modelRow(page, publicName).getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑模型「${publicName}」` });
  // Each target says which account it is, so the operator can pick the right one.
  await expect(form).toContainText(`账号：${keepName}`);
  await expect(form).toContainText(`账号：${dropName}`);

  // Two steps, and the warning names the account that will stop serving it.
  const target = form.locator("li").filter({ hasText: `账号：${dropName}` });
  await target.getByRole("button", { name: "移除该目标" }).click();
  const warning = target.getByRole("alert");
  await expect(warning).toContainText(`账号「${dropName}」`);
  await expect(warning).toContainText(publicName);
  await target.getByRole("button", { name: "取消" }).click();
  await expect(target.getByRole("button", { name: "移除该目标" })).toBeVisible();

  await target.getByRole("button", { name: "移除该目标" }).click();
  await target.getByRole("button", { name: "确认移除该目标" }).click();
  await expect(page.getByText(`已移除账号「${dropName}」对「${publicName}」的映射`)).toBeVisible();
  // The target list is regenerated from the rows: the account that no longer
  // serves the model is no longer a target, and the other one still is.
  await expect(target).toHaveCount(0);
  await expect(form.locator("li").filter({ hasText: `账号：${keepName}` })).toHaveCount(1);

  // The mapping is gone on the account side too, and the routing document no
  // longer names that account's direct document — the other target stays.
  const table = await stack.admin.get(`/admin/provider-keys/${drop.id}/channel-models`);
  expect((table.items as Json[]).find((i) => i.public_model_id === pub.id)?.served).toBe(false);
  expect(await modelNamed(droppedDirect)).toBeUndefined();
  const kept = await modelNamed(keptDirect);
  expect(kept).toBeDefined();
  await expect.poll(() => routingOf(pub.id), { timeout: APPLIED_WITHIN_MS }).toMatchObject({
    targets: [{ model_id: kept.id, priority: 1, weight: 1 }],
  });
  // Removing a target is the account's mapping, not the model's settings.
  expect((await publicView(pub.id)).routing).toEqual({});
});

/** Whether the model list still holds a row at `id`. */
async function modelLive(id: string): Promise<boolean> {
  return (await status(`/admin/resources/models/${id}`)) === 200;
}

/** Signs in, opens 上游账号, and one account's 模型 section. */
async function openAccountModels(page: Page, name: string): Promise<Locator> {
  await login(page, stack.operator.email, stack.operator.password);
  await page.locator('[data-tab="providers"]').click();
  const row = page.getByRole("row", { name: new RegExp(name) });
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: /编辑|查看/ }).click();
  const section = page.getByRole("region", { name: "模型" });
  await expect(section).toBeVisible();
  await expect(section.getByText("载入中…")).toHaveCount(0);
  return section;
}

test("分类列与可折叠的内部直连：默认只列对外的和手工创建的模型", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-source-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const handMade = `chat-source-manual-${suffix}`;
  await directModel(handMade, account.id, "gpt-source-up-manual");
  const publicName = `pub-source-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(account.id, pub.id);
  // Named `<public>#<account>`, so neither row can be mistaken for the other.
  const directName = `${publicName}#${accountName}`;

  await openModels(page);
  // The default view lists the row a caller names and the operator's own
  // models; the per-account documents behind the public model are hidden.
  await expect(modelRow(page, publicName)).toBeVisible();
  await expect(modelRow(page, handMade)).toBeVisible();
  await expect(modelRow(page, directName)).toHaveCount(0);
  // And the page says how many it is holding back.
  await expect(page.getByRole("checkbox", { name: /显示内部直连模型（\d+）/ })).not.toHaveCount(0);

  // The public model's routing document is the facing row, and names itself.
  await expect(sourceCell(page, publicName)).toContainText("对外");
  await expect(sourceCell(page, publicName)).toContainText(publicName);
  // The hand-made model's class reads as written here, with no public name.
  await expect(sourceCell(page, handMade)).toHaveText("手工创建");

  // Asking for them lists the internal row, which names the public model it
  // belongs to and the account it serves through.
  await showInternal(page, true);
  await expect(modelRow(page, directName)).toBeVisible();
  await expect(sourceCell(page, directName)).toContainText("内部直连");
  await expect(sourceCell(page, directName)).toContainText(publicName);
  await expect(sourceCell(page, directName)).toContainText(accountName);

  // Putting it back hides it again, and leaves everything else listed.
  await showInternal(page, false);
  await expect(modelRow(page, directName)).toHaveCount(0);
  await expect(modelRow(page, publicName)).toBeVisible();
  await expect(modelRow(page, handMade)).toBeVisible();
});

test("公共模型生成的模型在这里只能停用：停用后两张文档都消失，手工模型不受影响", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-retire-row-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const handMade = `chat-retire-manual-${suffix}`;
  const kept = await directModel(handMade, account.id, "gpt-retire-up-manual");
  const publicName = `pub-retire-row-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(account.id, pub.id);
  const directName = `${publicName}#${accountName}`;

  await openModels(page);
  const routeRow = modelRow(page, publicName);
  await expect(routeRow).toBeVisible();
  await expect(routeRow).toContainText("路由");
  // The public model's per-account document is 内部直连, listed once asked for.
  await showInternal(page, true);
  const directRow = modelRow(page, directName);
  await expect(directRow).toBeVisible();

  // The delete that cannot work is gone: a generated row offers 停用, not 删除.
  // (Before this change it offered 删除, whose confirmation answered
  // `409 managed_resource` and left the row in place.)
  await expect(routeRow.getByRole("button", { name: "删除", exact: true })).toHaveCount(0);
  await expect(directRow.getByRole("button", { name: "删除", exact: true })).toHaveCount(0);
  // The routing document is the public name itself, so its settings and its
  // targets are edited here; the per-account document is one account's
  // mapping, edited as a target of the routing row, so this row only views it
  // and says where it is changed.
  await expect(routeRow.getByRole("button", { name: "编辑", exact: true })).toBeVisible();
  await expect(directRow.getByRole("button", { name: "查看", exact: true })).toBeVisible();
  await expect(directRow).toContainText(`在「${publicName}」的路由里改`);
  await expect(directRow).not.toContainText("账号的「模型」");
  // The per-account row's one removal takes that account out of the routing
  // model; retiring the public name stays the routing row's own action.
  await expect(directRow.getByRole("button", { name: "移除该目标" })).toBeVisible();
  await expect(directRow.getByRole("button", { name: "停用", exact: true })).toHaveCount(0);
  // No row leads anywhere else for the public model: the page completes it.
  await expect(page.getByRole("button", { name: "去公共模型" })).toHaveCount(0);

  // An enabled account still serves it, so the retire is refused: the
  // warning says so before the confirmation, and the answer says why.
  await routeRow.getByRole("button", { name: "停用", exact: true }).click();
  const warning = routeRow.getByRole("alert");
  await expect(warning).toContainText("1 个启用的账号");
  await expect(warning).toContainText("停用会被拒绝");
  await routeRow.getByRole("button", { name: "确认停用" }).click();
  await expect(page.getByRole("alert").filter({ hasText: "仍在使用中" })).toContainText("启用的账号 1");
  expect((await publicView(pub.id)).retired_at).toBeNull();
  expect(await modelLive(pub.id)).toBe(true);

  // With nothing naming it any more, the same two clicks retire it.
  await stack.admin.del(`/admin/provider-keys/${account.id}/channel-models/${pub.id}`);
  await page.reload();
  await page.getByRole("tab", { name: "模型与路由" }).click();
  const again = modelRow(page, publicName);
  await expect(again).toBeVisible();
  await again.getByRole("button", { name: "停用", exact: true }).click();
  await expect(again.getByRole("alert")).toContainText("停用后不能恢复");
  await expect(again.getByRole("alert")).toContainText("路由文档");
  await again.getByRole("button", { name: "确认停用" }).click();

  // Both documents the public model owned are gone, and the page says what happened.
  await expect(page.getByText(`已停用公共模型「${publicName}」`)).toBeVisible();
  await expect(modelRow(page, publicName)).toHaveCount(0, { timeout: APPLIED_WITHIN_MS });
  await expect(modelRow(page, directName)).toHaveCount(0);
  expect(await modelLive(pub.id)).toBe(false);
  expect((await publicView(pub.id)).retired_at).not.toBeNull();
  // The hand-made model is untouched.
  expect(await modelLive(kept.id)).toBe(true);
  await expect(modelRow(page, handMade)).toBeVisible();
});

test("账号页的删除模型：只有这个账号在用时会移除映射并停用公共模型", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-del-model-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const publicName = `pub-del-model-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(account.id, pub.id);
  const directName = `${publicName}#${accountName}`;

  const section = await openAccountModels(page, accountName);
  const row = section.getByRole("row", { name: new RegExp(`^服务 ${escapeRe(publicName)}(?:\\s|$)`) });
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: "删除模型" }).click();
  const warning = row.getByRole("alert");
  await expect(warning).toContainText(publicName);
  await expect(warning).toContainText("先移除本账号对它的映射，再停用它");
  // Nothing else holds it once this account's row goes, so the confirmation
  // promises the deletion rather than a refusal.
  await expect(warning).toContainText("停用后不能恢复");
  await expect(warning).toContainText("将删除它的路由文档");
  await row.getByRole("button", { name: "确认删除模型" }).click();

  // Both writes happened, and the page says so in the success voice.
  await expect(section.getByText(`已删除「${publicName}」`)).toBeVisible();
  await expect(section.getByText("本账号的映射和它生成的模型都已移除")).toBeVisible();
  expect((await publicView(pub.id)).retired_at).not.toBeNull();
  expect(await modelLive(pub.id)).toBe(false);
  // The generated document and the account's mapping are gone with it.
  const { items } = await stack.admin.get("/admin/resources/models");
  expect((items as Json[]).some((m) => m.doc.display_name === directName)).toBe(false);
  const table = await stack.admin.get(`/admin/provider-keys/${account.id}/channel-models`);
  expect((table.items as Json[]).some((i) => i.public_model_id === pub.id)).toBe(false);
  // The checklist is read again: the public model is no longer offered.
  await expect(section.getByRole("row", { name: new RegExp(`^服务 ${escapeRe(publicName)}(?:\\s|$)`) })).toHaveCount(0);
});

test("账号页的删除模型：租户还授权着它时，说明映射已移除而模型保留，模型仍在列表里", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-del-hold-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const publicName = `pub-del-hold-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(account.id, pub.id);
  // A tenant granted it, and one key of that tenant naming it: the count the
  // refusal carries, and the reason the model stays.
  const { tenant } = await grantedTenant(`tenant-del-hold-${suffix}`, [pub.id]);

  const section = await openAccountModels(page, accountName);
  const row = section.getByRole("row", { name: new RegExp(`^服务 ${escapeRe(publicName)}(?:\\s|$)`) });
  await row.getByRole("button", { name: "删除模型" }).click();
  await row.getByRole("button", { name: "确认删除模型" }).click();

  // The partial outcome, unmistakably: the mapping went, the model did not.
  const partial = section.getByRole("alert").filter({ hasText: "本账号的映射已移除" });
  await expect(partial).toBeVisible();
  await expect(partial).toContainText("「" + publicName + "」没有停用，仍然存在");
  await expect(partial).toContainText("1 个租户、1 个密钥的授权");
  await expect(partial).toContainText("从租户与密钥中移除它");
  // Never the success sentence, and never a plain failure either.
  await expect(section.getByText("都已移除")).toHaveCount(0);

  // The mapping is gone, and the account no longer serves it.
  const table = await stack.admin.get(`/admin/provider-keys/${account.id}/channel-models`);
  const served = (table.items as Json[]).find((i) => i.public_model_id === pub.id);
  expect(served?.served).toBe(false);
  // The model stayed live, with its routing document, and the tenant keeps its grant.
  const view = await publicView(pub.id);
  expect(view.retired_at).toBeNull();
  expect(view.tenants).toBe(1);
  expect(await modelLive(pub.id)).toBe(true);
  expect((await stack.admin.get(`/admin/tenants/${tenant.id}`)).allowed_model_ids).toEqual([pub.id]);

  // 价格 · 公共模型 still lists it: the operator sees it where it can be handled.
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("group", { name: "价格分区" }).getByRole("button", { name: "公共模型", exact: true }).click();
  const panel = page.locator(".panel").filter({ has: page.getByRole("heading", { name: "公共模型", level: 2 }) });
  await expect(panel.getByRole("row", { name: new RegExp(`^${escapeRe(publicName)}(?:\\s|$)`) })).toBeVisible();

  // And the catalog keeps both its rows: the routing document is still there,
  // the direct one for this account is not.
  await page.getByRole("tab", { name: "模型与路由" }).click();
  await expect(modelRow(page, publicName)).toBeVisible();
  await expect(modelRow(page, `${publicName}#${accountName}`)).toHaveCount(0);
});

test("只读运维在账号页看得到模型，但没有删除模型的入口", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-del-viewer-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const publicName = `pub-del-viewer-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(account.id, pub.id);
  const viewer = { email: `viewer-del-${suffix}@e2e.local`, password: "viewer-del-pass-9f1" };
  const created = await stack.admin.request("POST", "/admin/operators", { ...viewer, role: "viewer" });
  expect(created.status, created.text).toBe(201);

  await login(page, viewer.email, viewer.password);
  await page.locator('[data-tab="providers"]').click();
  const row = page.getByRole("row", { name: new RegExp(accountName) });
  await row.getByRole("button", { name: "查看" }).click();
  const section = page.getByRole("region", { name: "模型" });
  await expect(section).toBeVisible();
  await expect(section.getByText("载入中…")).toHaveCount(0);
  // The row is visible; the action is not, and nothing changed.
  await expect(section.getByRole("row", { name: new RegExp(`^服务 ${escapeRe(publicName)}(?:\\s|$)`) })).toBeVisible();
  await expect(section.getByRole("button", { name: "删除模型" })).toHaveCount(0);
  expect((await publicView(pub.id)).retired_at).toBeNull();
});

/** The account section's row for `name`: the one whose 服务 tick names it. */
function accountRow(section: Locator, name: string): Locator {
  return section.getByRole("row", { name: new RegExp(`^服务 ${escapeRe(name)}(?:\\s|$)`) });
}

test("账号页删除模型被保留后，只保存另一行的改动：被移除的映射不会被写回", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-del-keep-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const other = await mockAccount(`acct-del-other-${suffix}`, ["shared"]);
  const removed = await newPublicModel(`pub-del-p-${suffix}`);
  const edited = await newPublicModel(`pub-del-q-${suffix}`);
  await serve(account.id, removed.id);
  await serve(account.id, edited.id);
  // Another account still serves P, so the retire after this account's
  // removal is refused: P stays live, unserved here.
  await serve(other.id, removed.id);
  const requests: string[] = [];
  page.on("request", (r) => {
    if (r.url().includes(`/admin/provider-keys/${account.id}/channel-models/`)) requests.push(`${r.method()} ${r.url()}`);
  });

  const section = await openAccountModels(page, accountName);
  // A genuine edit of Q, made before the delete and kept across it.
  await accountRow(section, edited.name).getByRole("textbox", { name: `优先级 ${edited.name}` }).fill("7");
  const pRow = accountRow(section, removed.name);
  await pRow.getByRole("button", { name: "删除模型" }).click();
  await pRow.getByRole("button", { name: "确认删除模型" }).click();
  await expect(section.getByRole("alert").filter({ hasText: "本账号的映射已移除" })).toContainText(
    `「${removed.name}」没有停用，仍然存在`,
  );
  // P is no longer this account's, so the default view no longer lists it.
  await expect(accountRow(section, removed.name)).toHaveCount(0);
  await expect(accountRow(section, edited.name).getByRole("textbox", { name: `优先级 ${edited.name}` })).toHaveValue("7");

  await section.getByRole("button", { name: "写入映射" }).click();
  // One row was edited, so one is written.
  await expect(section.getByText("已保存 1 个模型的映射。")).toBeVisible();
  const table = await stack.admin.get(`/admin/provider-keys/${account.id}/channel-models`);
  const rowOf = (id: string) => (table.items as Json[]).find((i) => i.public_model_id === id);
  expect(rowOf(removed.id)?.served, "the removed mapping stayed removed").toBe(false);
  expect(rowOf(edited.id)).toMatchObject({ served: true, priority: 7 });
  const puts = requests.filter((r) => r.startsWith("PUT "));
  expect(puts.some((r) => r.endsWith(`/${removed.id}`)), puts.join("\n")).toBe(false);
  expect(puts.some((r) => r.endsWith(`/${edited.id}`)), puts.join("\n")).toBe(true);
});

test("账号页删除模型时它已被别人停用：说它已经停用，而不是说没有停用", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-del-gone-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const pub = await newPublicModel(`pub-del-gone-${suffix}`);
  await serve(account.id, pub.id);

  const section = await openAccountModels(page, accountName);
  const row = accountRow(section, pub.name);
  await expect(row).toBeVisible();
  // Another operator deletes the public model while this page is open.
  await stack.admin.post(`/admin/public-models/${pub.id}/delete`, {});
  expect((await publicView(pub.id)).retired_at).not.toBeNull();

  await row.getByRole("button", { name: "删除模型" }).click();
  await row.getByRole("button", { name: "确认删除模型" }).click();
  await expect(section.getByRole("status").filter({ hasText: `「${pub.name}」已经停用` })).toBeVisible();
  // Never the refusal's claim that nothing changed or that it is still live.
  await expect(section.getByText("没有停用")).toHaveCount(0);
  await expect(section.getByText("没有改动")).toHaveCount(0);
  await expect(accountRow(section, pub.name)).toHaveCount(0);
});

test("公共模型的选路策略不提供 least_cost：目标不带价格，控制面也拒绝它", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-least-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const publicName = `pub-least-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(account.id, pub.id);

  const refused = await stack.admin.request("PATCH", `/admin/public-models/${pub.id}`, {
    routing: { strategy: "least_cost" },
  });
  expect(refused.status, refused.text).toBe(422);
  expect(JSON.parse(refused.text).details.map((d: Json) => d.path)).toEqual(["/routing/strategy"]);

  await openModels(page);
  await modelRow(page, publicName).getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑模型「${publicName}」` });
  const options = form.getByLabel("选路策略").locator("option");
  await expect(options.filter({ hasText: /^least_latency$/ })).toHaveCount(1);
  await expect(options.filter({ hasText: /^least_cost$/ })).toHaveCount(0);
});

test("保存路由设置后接着改：模型列表先读回来也从刚保存的设置继续，再改一项不丢前面的设置", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-route-again-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const publicName = `pub-route-again-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(account.id, pub.id);

  await openModels(page);
  await modelRow(page, publicName).getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑模型「${publicName}」` });
  await form.getByLabel("选路策略").selectOption("failover");
  await form.getByLabel("重试次数").fill("2");

  // After the save, the public models' list is held back for a moment, so
  // the model list's read — which brings the regenerated document — lands
  // first. The request still reaches the real control plane and its answer is
  // passed on unchanged; only when it arrives moves, to the order the race
  // needs.
  let hold = false;
  await page.route(/\/admin\/public-models$/, async (route) => {
    if (hold && route.request().method() === "GET") await new Promise((r) => setTimeout(r, 2_500));
    await route.continue();
  });
  hold = true;
  const modelsRead = page.waitForResponse(
    (r) => r.request().method() === "GET" && /\/admin\/resources\/models$/.test(new URL(r.url()).pathname),
  );
  await form.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText(`已保存「${publicName}」的路由设置`)).toBeVisible();
  await modelsRead;
  // The form shows what was just saved, not the defaults.
  const again = page.getByRole("form", { name: `编辑模型「${publicName}」` });
  await expect(again.getByLabel("选路策略")).toHaveValue("failover");
  await expect(again.getByLabel("重试次数")).toHaveValue("2");

  // Changing one more field and saving keeps the others.
  await again.getByLabel("最多切换几个目标").fill("1");
  await again.getByRole("button", { name: "保存" }).click();
  await expect
    .poll(async () => (await publicView(pub.id)).routing, { timeout: APPLIED_WITHIN_MS })
    .toEqual({ strategy: "failover", retries: 2, max_fallbacks: 1 });
  await page.unroute(/\/admin\/public-models$/);
});

test("内部直连行的查看打开只读表单：名称、账号、上游模型都在，没有修改入口", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-view-direct-${suffix}`;
  const account = await mockAccount(accountName, ["shared"]);
  const publicName = `pub-view-direct-${suffix}`;
  const pub = await newPublicModel(publicName);
  const upstream = `gpt-view-up-${suffix}`;
  await stack.admin.put(`/admin/provider-keys/${account.id}/channel-models/${pub.id}`, {
    upstream_model: upstream,
    priority: 1,
    weight: 1,
  });
  const directName = `${publicName}#${accountName}`;
  const viewer = { email: `viewer-direct-${suffix}@e2e.local`, password: "viewer-direct-pass-4c2" };
  const created = await stack.admin.request("POST", "/admin/operators", { ...viewer, role: "viewer" });
  expect(created.status, created.text).toBe(201);

  for (const who of [stack.operator, viewer]) {
    await page.context().clearCookies();
    await openModels(page, who);
    await showInternal(page, true);
    const row = modelRow(page, directName);
    await row.getByRole("button", { name: "查看", exact: true }).click();
    const form = page.getByRole("form", { name: `模型「${directName}」` });
    await expect(form, who.email).toBeVisible();
    await expect(form.getByLabel("对外名称")).toHaveValue(directName);
    await expect(form.getByLabel("对外名称")).toBeDisabled();
    await expect(form.getByLabel("上游账号")).toHaveValue(account.id);
    await expect(form.getByLabel("上游账号")).toBeDisabled();
    await expect(form.getByLabel("上游模型")).toHaveValue(upstream);
    await expect(form.getByLabel("上游模型")).toBeDisabled();
    await expect(form.getByRole("button", { name: "保存" })).toHaveCount(0);
    await form.getByRole("button", { name: "关闭" }).click();
    await expect(form).toHaveCount(0);
  }
});

/** The target of a public model's routing form that the account `accountName` serves through. */
function publicTarget(form: Locator, accountName: string): Locator {
  return form.getByRole("listitem").filter({ hasText: `账号：${accountName}` });
}

/** The upstream-model field of a target, once it has finished reading the account's list. */
async function readyUpstream(target: Locator): Promise<Locator> {
  const field = target.getByRole("combobox", { name: "上游模型名称" });
  await field.focus();
  await expect(field).toBeEnabled();
  return field;
}

/** The channel row `accountId` holds for `publicModelId`, as the account's own 模型 section reads it. */
async function channelRow(accountId: string, publicModelId: string): Promise<Json> {
  const { items } = await stack.admin.get(`/admin/provider-keys/${accountId}/channel-models`);
  return (items as Json[]).find((i) => i.public_model_id === publicModelId);
}

/** One chat call through the gateway, and the upstream model the stand-in was asked for. */
async function servedAs(key: string, model: string): Promise<string[]> {
  await clearUpstream();
  await chat(key, model);
  return upstreamModels();
}

test("公共模型的路由表单里直接改目标的上游模型、优先级和权重：保存后路由文档、账号映射和重新打开的表单都是新值", async ({
  page,
}) => {
  const suffix = randomUUID().slice(0, 8);
  const firstName = `acct-tgt-edit-a-${suffix}`;
  const secondName = `acct-tgt-edit-b-${suffix}`;
  const first = await mockAccount(firstName, ["shared"]);
  const second = await mockAccount(secondName, ["shared"]);
  const publicName = `pub-tgt-edit-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(first.id, pub.id);
  await serve(second.id, pub.id);
  const key = await internalKey(`tgt-edit-key-${suffix}`, [publicName]);

  await openModels(page);
  await modelRow(page, publicName).getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑模型「${publicName}」` });
  // The page no longer sends the operator to the account for these.
  await expect(form).not.toContainText("上游账号 → 模型");
  await expect(form).not.toContainText("账号的「模型」");

  const a = publicTarget(form, firstName);
  const b = publicTarget(form, secondName);
  await expect(a.getByLabel("优先级")).toHaveValue("1");
  await expect(a.getByLabel("权重")).toHaveValue("1");
  await a.getByLabel("优先级").fill("5");
  await a.getByLabel("权重").fill("3");
  await (await readyUpstream(a)).fill(`gpt-tgt-edit-up-${suffix}`);
  await b.getByLabel("权重").fill("2");
  await form.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText(`已保存「${publicName}」`)).toBeVisible();

  // The routing document the gateway reads carries the new tiers.
  const directA = await modelNamed(`${publicName}#${firstName}`);
  const directB = await modelNamed(`${publicName}#${secondName}`);
  await expect.poll(async () => (await routingOf(pub.id)).targets, { timeout: APPLIED_WITHIN_MS }).toEqual([
    { model_id: directA.id, priority: 5, weight: 3 },
    { model_id: directB.id, priority: 1, weight: 2 },
  ]);
  expect((await modelNamed(`${publicName}#${firstName}`)).doc.model_name).toBe(`gpt-tgt-edit-up-${suffix}`);
  // They are the accounts' own mappings, which their 模型 sections show too.
  expect(await channelRow(first.id, pub.id)).toMatchObject({
    served: true,
    upstream_model: `gpt-tgt-edit-up-${suffix}`,
    priority: 5,
    weight: 3,
  });
  expect(await channelRow(second.id, pub.id)).toMatchObject({ served: true, priority: 1, weight: 2 });
  // The routing settings were not touched by a target edit.
  expect((await publicView(pub.id)).routing).toEqual({});
  // The higher tier serves, under the upstream name it was given.
  await expect
    .poll(() => servedAs(key, publicName), { timeout: APPLIED_WITHIN_MS })
    .toEqual([`gpt-tgt-edit-up-${suffix}`]);

  // A reload shows what was stored.
  await page.reload();
  await page.getByRole("tab", { name: "模型与路由" }).click();
  await modelRow(page, publicName).getByRole("button", { name: "编辑", exact: true }).click();
  const again = page.getByRole("form", { name: `编辑模型「${publicName}」` });
  const a2 = publicTarget(again, firstName);
  await expect(a2.getByLabel("优先级")).toHaveValue("5");
  await expect(a2.getByLabel("权重")).toHaveValue("3");
  await expect(a2.getByRole("combobox", { name: "上游模型名称" })).toHaveValue(`gpt-tgt-edit-up-${suffix}`);
  await expect(publicTarget(again, secondName).getByLabel("权重")).toHaveValue("2");
});

test("在公共模型的路由表单里添加目标：选另一个上游账号并填写映射，保存后它出现在路由里", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const servingName = `acct-tgt-add-a-${suffix}`;
  const addedName = `acct-tgt-add-b-${suffix}`;
  const serving = await mockAccount(servingName, ["shared"]);
  const added = await mockAccount(addedName, ["grp-beta-3e9"]);
  const publicName = `pub-tgt-add-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(serving.id, pub.id);
  const key = await internalKey(`tgt-add-key-${suffix}`, [publicName]);

  await openModels(page);
  await modelRow(page, publicName).getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑模型「${publicName}」` });
  await expect(publicTarget(form, servingName)).toHaveCount(1);
  await form.getByRole("button", { name: "添加目标" }).click();
  const row = form.getByRole("listitem", { name: /^新目标/ });
  await expect(row).toHaveCount(1);
  // An account that already serves the model is not offered again.
  await expect(row.getByLabel("上游账号").locator(`option[value="${serving.id}"]`)).toHaveCount(0);
  await row.getByLabel("上游账号").selectOption(added.id);
  // The operator sees which groups the chosen account serves.
  await expect(row).toContainText("grp-beta-3e9");
  await (await readyUpstream(row)).fill(`gpt-tgt-add-up-${suffix}`);
  await row.getByLabel("优先级").fill("9");
  await row.getByLabel("权重").fill("4");
  // Nothing is written before the save.
  expect((await channelRow(added.id, pub.id)).served).toBe(false);
  await form.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText(`已保存「${publicName}」`)).toBeVisible();

  // The account's mapping exists, with what was typed, and the routing
  // document names its direct model first.
  expect(await channelRow(added.id, pub.id)).toMatchObject({
    served: true,
    enabled: true,
    upstream_model: `gpt-tgt-add-up-${suffix}`,
    priority: 9,
    weight: 4,
  });
  const directAdded = await modelNamed(`${publicName}#${addedName}`);
  const directServing = await modelNamed(`${publicName}#${servingName}`);
  expect(directAdded?.doc.model_name).toBe(`gpt-tgt-add-up-${suffix}`);
  await expect.poll(async () => (await routingOf(pub.id)).targets, { timeout: APPLIED_WITHIN_MS }).toEqual([
    { model_id: directAdded.id, priority: 9, weight: 4 },
    { model_id: directServing.id, priority: 1, weight: 1 },
  ]);

  // The form reads the targets back: the new one is an ordinary target now.
  const listed = publicTarget(form, addedName);
  await expect(listed).toHaveCount(1);
  await expect(listed.getByLabel("优先级")).toHaveValue("9");
  await expect(listed.getByLabel("权重")).toHaveValue("4");
  await expect(listed).toContainText("grp-beta-3e9");
  await expect(form.getByRole("listitem", { name: /^新目标/ })).toHaveCount(0);

  // And the gateway sends the top tier to the account just added.
  await expect
    .poll(() => servedAs(key, publicName), { timeout: APPLIED_WITHIN_MS })
    .toEqual([`gpt-tgt-add-up-${suffix}`]);
});

test("路由表单保存时一个目标被拒：错误标在那一行，其他改动照常保存、不丢；改正后再保存只补剩下的", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const goodName = `acct-tgt-part-a-${suffix}`;
  const badName = `acct-tgt-part-b-${suffix}`;
  const good = await mockAccount(goodName, ["shared"]);
  const bad = await mockAccount(badName, ["shared"]);
  const publicName = `pub-tgt-part-${suffix}`;
  const pub = await newPublicModel(publicName);
  await serve(good.id, pub.id);
  await serve(bad.id, pub.id);
  const writes: string[] = [];
  page.on("request", (r) => {
    const path = new URL(r.url()).pathname;
    if (r.method() === "PUT" || r.method() === "PATCH") writes.push(`${r.method()} ${path}`);
  });

  await openModels(page);
  await modelRow(page, publicName).getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑模型「${publicName}」` });
  const a = publicTarget(form, goodName);
  const b = publicTarget(form, badName);
  await a.getByLabel("权重").fill("3");
  // A weight cannot be negative.
  await b.getByLabel("权重").fill("-1");
  await form.getByLabel("重试次数").fill("2");
  await form.getByRole("button", { name: "保存" }).click();

  // The refusal is at the refused field, on its own row only.
  const refused = b.getByLabel("权重");
  await expect(refused).toHaveAttribute("aria-invalid", "true");
  expect(await description(refused)).not.toBe("");
  await expect(a.getByLabel("权重")).not.toHaveAttribute("aria-invalid", "true");
  // Nothing typed is lost, and the row that was written says so.
  await expect(refused).toHaveValue("-1");
  await expect(a.getByLabel("权重")).toHaveValue("3");
  await expect(form.getByLabel("重试次数")).toHaveValue("2");
  await expect(a).toContainText("已保存");
  await expect(b).not.toContainText("已保存");
  // What was accepted is stored; the refused row is as it was.
  const directA = await modelNamed(`${publicName}#${goodName}`);
  const directB = await modelNamed(`${publicName}#${badName}`);
  await expect.poll(() => routingOf(pub.id), { timeout: APPLIED_WITHIN_MS }).toMatchObject({
    retries: 2,
    targets: [
      { model_id: directA.id, priority: 1, weight: 3 },
      { model_id: directB.id, priority: 1, weight: 1 },
    ],
  });

  // Fixing the refused value and saving again finishes the rest, and only the rest.
  writes.length = 0;
  await refused.fill("2");
  await form.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText(`已保存「${publicName}」`)).toBeVisible();
  await expect(refused).not.toHaveAttribute("aria-invalid", "true");
  expect(writes).toEqual([`PUT /admin/provider-keys/${bad.id}/channel-models/${pub.id}`]);
  await expect.poll(() => routingOf(pub.id), { timeout: APPLIED_WITHIN_MS }).toMatchObject({
    retries: 2,
    targets: [
      { model_id: directA.id, priority: 1, weight: 3 },
      { model_id: directB.id, priority: 1, weight: 2 },
    ],
  });
});

test("只读运维打开公共模型的路由：看得到每个目标的上游模型、优先级和权重，但不能修改、添加或移除", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const accountName = `acct-tgt-view-${suffix}`;
  const account = await mockAccount(accountName, ["grp-view-5e8"]);
  const publicName = `pub-tgt-view-${suffix}`;
  const pub = await newPublicModel(publicName);
  await stack.admin.put(`/admin/provider-keys/${account.id}/channel-models/${pub.id}`, {
    upstream_model: `gpt-tgt-view-up-${suffix}`,
    priority: 2,
    weight: 3,
  });
  const viewer = { email: `viewer-tgt-${suffix}@e2e.local`, password: "viewer-tgt-pass-6d3" };
  const created = await stack.admin.request("POST", "/admin/operators", { ...viewer, role: "viewer" });
  expect(created.status, created.text).toBe(201);

  await openModels(page, viewer);
  await modelRow(page, publicName).getByRole("button", { name: "查看", exact: true }).click();
  const form = page.getByRole("form", { name: `模型「${publicName}」` });
  const target = publicTarget(form, accountName);
  await expect(target).toContainText("grp-view-5e8");
  await expect(target.getByLabel("优先级")).toHaveValue("2");
  await expect(target.getByLabel("优先级")).toBeDisabled();
  await expect(target.getByLabel("权重")).toHaveValue("3");
  await expect(target.getByLabel("权重")).toBeDisabled();
  await expect(target.getByRole("combobox", { name: "上游模型名称" })).toHaveValue(`gpt-tgt-view-up-${suffix}`);
  await expect(target.getByRole("combobox", { name: "上游模型名称" })).toBeDisabled();
  for (const name of ["保存", "添加目标", "移除该目标"]) {
    await expect(form.getByRole("button", { name, exact: true })).toHaveCount(0);
  }
  await expect(form.getByRole("button", { name: "关闭" })).toBeVisible();
});

test("手工路由模型的编辑表单：改目标的权重和优先级、添加目标，保存后文档就是改后的值", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const account = await mockAccount(`acct-hand-edit-${suffix}`, ["shared"]);
  const one = await directModel(`chat-hand-one-${suffix}`, account.id, "gpt-hand-one-up");
  const two = await directModel(`chat-hand-two-${suffix}`, account.id, "gpt-hand-two-up");
  const routeName = `route-hand-edit-${suffix}`;
  await createModel({ display_name: routeName, routing: { targets: [{ model_id: one.id, weight: 1 }] } });
  const route = await modelNamed(routeName);

  await openModels(page);
  await modelRow(page, routeName).getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑模型「${routeName}」` });
  const first = targetRows(form).nth(0);
  await first.getByLabel("权重").fill("5");
  await first.getByLabel("优先级").fill("2");
  await form.getByRole("button", { name: "添加目标" }).click();
  const second = targetRows(form).nth(1);
  await second.getByLabel("目标模型").selectOption({ label: `chat-hand-two-${suffix}` });
  await second.getByLabel("优先级").fill("1");
  await form.getByRole("button", { name: "保存" }).click();
  await expect(form).toHaveCount(0);

  expect((await stack.admin.get(`/admin/resources/models/${route.id}`)).doc.routing).toEqual({
    targets: [
      { model_id: one.id, weight: 5, priority: 2 },
      { model_id: two.id, priority: 1 },
    ],
  });
});
