import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";

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

/** An upstream account the stand-in answers for, in `groups`. */
async function mockAccount(name: string, groups: string[]): Promise<Json> {
  return createAccount({
    display_name: name,
    provider: "openai",
    api_key: `sk-e2e-${name}-upstream`,
    api_base: `${stack.mockUpstreamUrl}/v1`,
    groups,
  });
}

async function modelNamed(name: string): Promise<Json | undefined> {
  const { items } = await stack.admin.get("/admin/resources/models");
  return (items as Json[]).find((v) => v.doc.display_name === name);
}

async function status(path: string): Promise<number> {
  return (await stack.admin.request("GET", path)).status;
}

/** A key of the platform's own tenant that may call `models` by name. */
async function internalKey(label: string, models: string[]): Promise<string> {
  const { items } = await stack.admin.get("/admin/tenants");
  const internal = (items as Json[]).find((t) => t.is_internal);
  expect(internal, JSON.stringify(items)).toBeDefined();
  const { plaintext } = await stack.admin.post(`/admin/tenants/${internal.id}/keys`, { label, allowed_models: models });
  return plaintext;
}

/** A customer tenant granted `modelIds`, and one key that follows its grants. */
async function grantedTenant(name: string, modelIds: string[]): Promise<{ tenant: Json; key: string }> {
  const created = await stack.admin.post("/admin/tenants", { name });
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
