import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, fundTenant, startStack, type Json, type Stack } from "./stack";

/**
 * The cache policies page and the generic resource page ("全部资源"), against
 * the real control plane and a managed gateway through the built `dist/`.
 * Setup goes through the real `/admin` API; the only stand-in is the
 * upstream provider the gateway calls.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

/** Long enough for a save to be projected and reported by a heartbeat. */
const ROLLOUT_MS = 30_000;

async function loginAsAdmin(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令").fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

async function openTab(page: Page, tab: string): Promise<void> {
  await page.locator(`[data-tab="${tab}"]`).click();
}

/** A gateway document written through the generic API; its view. */
function createResource(kind: string, doc: Json): Promise<Json> {
  return stack.admin.post(`/admin/resources/${kind}`, { doc });
}

/**
 * An upstream account at the stand-in, in the default group `shared` and in
 * the group the internal key of the last case names, and one direct model on
 * it that asks the upstream for `upstreamModel`.
 */
async function seedModel(name: string, upstreamModel: string, apiKey = `sk-${name}-upstream`): Promise<{ account: Json; model: Json }> {
  const account = await createResource("provider_keys", {
    display_name: `${name}-account`,
    provider: "openai",
    api_key: apiKey,
    api_base: `${stack.mockUpstreamUrl}/v1`,
    groups: ["shared", "grp-resources-ui-7c1"],
  });
  const model = await createResource("models", {
    display_name: name,
    provider: "openai",
    model_name: upstreamModel,
    provider_key_id: account.id,
  });
  return { account, model };
}

/** The nodes `node` stands for through JSON Schema's `$ref`, `allOf`, `anyOf` and `oneOf`, itself first. */
function composed(root: Json, node: Json): Json[] {
  const out: Json[] = [node];
  if (typeof node.$ref === "string") {
    const target = node.$ref
      .replace(/^#\//, "")
      .split("/")
      .reduce((n: Json, s: string) => n?.[s], root);
    out.push(...composed(root, target));
  }
  for (const key of ["allOf", "anyOf", "oneOf"]) {
    for (const sub of node[key] ?? []) out.push(...composed(root, sub));
  }
  return out;
}

/** Every string value the schema allows at `node`, in schema order. */
function allowedValues(root: Json, node: Json): string[] {
  const values: string[] = [];
  for (const n of composed(root, node)) {
    for (const v of n.enum ?? ("const" in n ? [n.const] : [])) {
      if (typeof v === "string" && !values.includes(v)) values.push(v);
    }
  }
  return values;
}

/** The description of the schema branch at `node` that allows exactly `value`. */
function branchDescription(root: Json, node: Json, value: string): string | undefined {
  return composed(root, node).find((n) => Array.isArray(n.enum) && n.enum.length === 1 && n.enum[0] === value)
    ?.description;
}

/** The chat requests the stand-in recorded for `upstreamModel`. */
async function upstreamChats(upstreamModel: string): Promise<Json[]> {
  const records: Json[] = await (await fetch(`${stack.mockUpstreamUrl}/__requests`)).json();
  return records.filter((r) => r.path === "/v1/chat/completions" && r.body?.model === upstreamModel);
}

/** Waits until the gateway authenticates `key`, the readiness gate for everything written before it. */
async function waitAuthenticates(key: string): Promise<void> {
  await expect
    .poll(
      async () => (await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${key}` } })).status,
      { timeout: ROLLOUT_MS },
    )
    .toBe(200);
}

function chat(key: string, model: string, content: string): Promise<Response> {
  return fetch(`${stack.proxyUrl}/v1/chat/completions`, {
    method: "POST",
    headers: { authorization: `Bearer ${key}`, "content-type": "application/json" },
    body: JSON.stringify({ model, messages: [{ role: "user", content }] }),
  });
}

/** The table row that names `name`. */
function row(page: Page, name: string): Locator {
  return page.getByRole("row").filter({ hasText: name });
}

/** Picks `kind` on the generic page. */
async function pickKind(page: Page, kind: string): Promise<void> {
  await page.getByLabel("资源类型").selectOption(kind);
}

test("缓存策略的范围选项里没有 env，默认选中 team，作用于写入模型 id", async ({ page }) => {
  const { model } = await seedModel("chat-e2e", "upstream-chat-e2e");
  const schema = await stack.admin.get("/admin/schemas/cache_policy");
  const scopes = allowedValues(schema, schema.properties.scope);
  // The premise: the gateway's schema offers env, and team beside it.
  expect(scopes).toContain("env");
  expect(scopes).toContain("team");
  const teamDescription = branchDescription(schema, schema.properties.scope, "team");
  expect(teamDescription).toBeTruthy();

  await loginAsAdmin(page);
  await openTab(page, "cache");
  await page.getByRole("button", { name: "新建缓存策略" }).click();

  const scope = page.getByLabel("范围");
  const options = await scope.locator("option").evaluateAll((os) => os.map((o) => (o as HTMLOptionElement).value));
  expect(options).toEqual(scopes.filter((s) => s !== "env"));
  await expect(scope).toHaveValue("team");
  await expect(page.getByText(teamDescription!)).toBeVisible();

  await page.getByLabel("名称").fill("cache-scope-e2e");
  await page.getByLabel("作用于").selectOption({ label: "chat-e2e" });
  await page.getByRole("button", { name: "保存" }).click();
  await expect(row(page, "cache-scope-e2e").locator("[data-state]")).toHaveAttribute("data-state", "applied", {
    timeout: ROLLOUT_MS,
  });

  const stored = (await stack.admin.get("/admin/resources/cache_policies")).items.find(
    (i: Json) => i.doc.name === "cache-scope-e2e",
  );
  expect(stored, "the policy was saved").toBeDefined();
  expect(stored.doc.applies_to_model_id).toBe(model.id);
  expect(stored.doc.scope).toBe("team");
});

test("同租户两把 key 命中同一缓存条目", async ({ page }) => {
  const { model } = await seedModel("cached-e2e", "upstream-cached-e2e");

  await loginAsAdmin(page);
  await openTab(page, "cache");
  await page.getByRole("button", { name: "新建缓存策略" }).click();
  await page.getByLabel("名称").fill("cache-team-e2e");
  await page.getByLabel("作用于").selectOption({ label: "cached-e2e" });
  // 范围 is left as the page offers it.
  await page.getByRole("button", { name: "保存" }).click();
  await expect(row(page, "cache-team-e2e").locator("[data-state]")).toHaveAttribute("data-state", "applied", {
    timeout: ROLLOUT_MS,
  });

  // Tenant T1 and its two keys, written after the policy.
  const t1 = await stack.admin.post("/admin/tenants", { name: "Cache E2E Tenant" });
  await fundTenant(stack.admin, t1.id);
  await stack.admin.patch(`/admin/tenants/${t1.id}`, { allowed_model_ids: [model.id] }, t1.version);
  const keyA = (await stack.admin.post(`/admin/tenants/${t1.id}/keys`, { label: "cache-e2e-a" })).plaintext;
  const keyB = (await stack.admin.post(`/admin/tenants/${t1.id}/keys`, { label: "cache-e2e-b" })).plaintext;
  await waitAuthenticates(keyA);
  await waitAuthenticates(keyB);

  const first = await chat(keyA, "cached-e2e", "the same question 3f1");
  expect(first.status, await first.text()).toBe(200);
  expect(await upstreamChats("upstream-cached-e2e")).toHaveLength(1);
  const second = await chat(keyB, "cached-e2e", "the same question 3f1");
  expect(second.status, await second.text()).toBe(200);
  expect(await upstreamChats("upstream-cached-e2e")).toHaveLength(1);
});

test("通用页：未知字段被服务端拒绝，路径标在编辑器下方，且没有保存", async ({ page }) => {
  const created = await createResource("cache_policies", { name: "gen-unknown-e2e", ttl_seconds: 120 });

  await loginAsAdmin(page);
  await openTab(page, "resources");
  await pickKind(page, "cache_policies");
  await row(page, "gen-unknown-e2e").getByRole("button", { name: "编辑" }).click();

  const editor = page.getByLabel("资源文档");
  await expect(editor).toHaveValue(/gen-unknown-e2e/);
  const doc = JSON.parse(await editor.inputValue());
  // A misspelt ttl_seconds.
  doc.ttl_second = 60;
  await editor.fill(JSON.stringify(doc));
  await page.getByRole("button", { name: "格式化" }).click();
  await expect(editor).toHaveValue(JSON.stringify(doc, null, 2));
  await page.getByRole("button", { name: "保存" }).click();

  const problems = page.getByRole("list", { name: "校验问题" });
  // A whole word: `ttl_second` is also the start of the valid `ttl_seconds`.
  await expect(problems).toContainText(/ttl_second\b/);
  await expect(editor).toHaveAttribute("aria-invalid", "true");
  const editorBox = await editor.boundingBox();
  const problemsBox = await problems.boundingBox();
  expect(editorBox && problemsBox && problemsBox.y >= editorBox.y + editorBox.height, "the problems are under the editor").toBe(true);

  const stored = await stack.admin.get(`/admin/resources/cache_policies/${created.id}`);
  expect(stored.version).toBe(created.version);
  expect(stored.doc).not.toHaveProperty("ttl_second");
  expect(stored.doc.ttl_seconds).toBe(120);
});

test("通用页：网关构建不了的护栏显示被拒绝及原因", async ({ page }) => {
  const { model: quiet } = await seedModel("quiet-e2e", "upstream-quiet-e2e");

  await loginAsAdmin(page);
  await openTab(page, "resources");
  await pickKind(page, "guardrails");
  await page.getByRole("button", { name: "新建" }).click();
  const editor = page.getByLabel("资源文档");
  await editor.fill(
    JSON.stringify({ name: "guard-bad-e2e", kind: "keyword", hook_point: "input", patterns: [{ kind: "regex", value: "(" }] }),
  );
  await page.getByRole("button", { name: "创建" }).click();
  await expect(row(page, "guard-bad-e2e")).toHaveCount(1);

  // Attached to a model nobody calls, so only its build is in question.
  const guardrail = (await stack.admin.get("/admin/resources/guardrails")).items.find(
    (i: Json) => i.doc.name === "guard-bad-e2e",
  );
  expect(guardrail, "the guardrail was created").toBeDefined();
  await pickKind(page, "guardrail_attachments");
  await page.getByRole("button", { name: "新建" }).click();
  await editor.fill(
    JSON.stringify({ guardrail_id: guardrail.id, scope_type: "model", scope_id: quiet.id, priority: 100 }),
  );
  await page.getByRole("button", { name: "创建" }).click();
  await expect
    .poll(async () =>
      (await stack.admin.get("/admin/resources/guardrail_attachments")).items.some(
        (i: Json) => i.doc.guardrail_id === guardrail.id,
      ),
    )
    .toBe(true);

  await pickKind(page, "guardrails");
  const badge = row(page, "guard-bad-e2e").locator("[data-state]");
  // The gateway reports the refusal in a later heartbeat; the operator refreshes until it shows.
  await expect(async () => {
    await page.getByRole("button", { name: "刷新" }).click();
    await expect(badge).toHaveAttribute("data-state", "rejected", { timeout: 2_000 });
  }).toPass({ timeout: ROLLOUT_MS });
  await expect(badge.getByRole("listitem").first()).toHaveText(/：\s*\S/);
});

test("托管资源与租户派生的限流策略只读", async ({ page }) => {
  const tenant = await stack.admin.post("/admin/tenants", { name: "Readonly E2E Tenant" });
  await stack.admin.patch(`/admin/tenants/${tenant.id}`, { limits: { rpm: 600 } }, tenant.version);
  await stack.admin.post(`/admin/tenants/${tenant.id}/keys`, { label: "ro-key-e2e" });
  await createResource("rate_limit_policies", { name: "rl-operator-e2e", limits: { rpm: 100 } });
  const managedNote = "由租户与密钥管理生成，在对应页面修改";

  await loginAsAdmin(page);
  await openTab(page, "resources");

  // A kind only tenants and keys write: nothing to create, save or delete.
  await pickKind(page, "api_keys");
  await expect(page.getByText(managedNote).first()).toBeVisible();
  await expect(page.getByRole("button", { name: "新建" })).toHaveCount(0);
  await row(page, "ro-key-e2e").getByRole("button", { name: "查看" }).click();
  await expect(page.getByLabel("资源文档")).toHaveValue(/ro-key-e2e/);
  await expect(page.getByRole("button", { name: "保存" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "删除" })).toHaveCount(0);

  // An operator's own rate-limit policy is editable: the control for the next check.
  await pickKind(page, "rate_limit_policies");
  await expect(page.getByRole("button", { name: "新建" })).toBeVisible();
  await row(page, "rl-operator-e2e").getByRole("button", { name: "编辑" }).click();
  await expect(page.getByLabel("资源文档")).toHaveValue(/rl-operator-e2e/);
  await expect(page.getByRole("button", { name: "保存" })).toBeVisible();
  await expect(page.getByRole("button", { name: "删除" })).toBeVisible();
  await expect(page.getByText(managedNote)).toHaveCount(0);

  // The tenant's limits policy, in the same writable kind, is not.
  const limits = `tenant-${tenant.id}-limits`;
  await row(page, limits).getByRole("button", { name: "查看" }).click();
  await expect(page.getByLabel("资源文档")).toHaveValue(new RegExp(limits));
  await expect(page.getByText(managedNote).first()).toBeVisible();
  await expect(page.getByRole("button", { name: "保存" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "删除" })).toHaveCount(0);
});

test("通用页：原样带回的上游密钥保持不变", async ({ page }) => {
  const secret = "sk-keep-e2e-secret-41c";
  await seedModel("keep-e2e", "upstream-keep-e2e", secret);
  const internal = (await stack.admin.get("/admin/tenants")).items.find((t: Json) => t.is_internal);
  expect(internal, "the platform's own tenant").toBeDefined();
  // Every key names one group, and the platform's own tenant has no
  // assignment list to take it from.
  const group = await stack.admin.post("/admin/provider-groups", {
    name: "grp-resources-ui-7c1",
    display_name: "资源页自用 7c1",
    default_cny_per_usd: "7.2",
  });
  const caller = (
    await stack.admin.post(`/admin/tenants/${internal.id}/keys`, {
      label: "keep-caller-e2e",
      allowed_models: ["*"],
      group_id: group.id,
    })
  ).plaintext;

  await loginAsAdmin(page);
  await openTab(page, "resources");
  await pickKind(page, "provider_keys");
  await row(page, "keep-e2e-account").getByRole("button", { name: "编辑" }).click();
  const editor = page.getByLabel("资源文档");
  await expect(editor).toHaveValue(/\[REDACTED\]/);
  expect(await editor.inputValue()).not.toContain(secret);
  expect(await page.content()).not.toContain(secret);

  // Renamed, with the credential left as the page shows it.
  const doc = JSON.parse(await editor.inputValue());
  doc.display_name = "keep-e2e-account-renamed";
  await editor.fill(JSON.stringify(doc, null, 2));
  await page.getByRole("button", { name: "保存" }).click();
  await expect(row(page, "keep-e2e-account-renamed").locator("[data-state]")).toHaveAttribute("data-state", "applied", {
    timeout: ROLLOUT_MS,
  });

  await waitAuthenticates(caller);
  const res = await chat(caller, "keep-e2e", "which key do you get");
  expect(res.status, await res.text()).toBe(200);
  const sent = await upstreamChats("upstream-keep-e2e");
  expect(sent).toHaveLength(1);
  expect(sent[0].authorization).toBe(`Bearer ${secret}`);
});

test("通用页：基于旧版本的保存被拒绝，先保存的保留；重新载入后可保存与删除", async ({ page }) => {
  const created = await createResource("cache_policies", { name: "gen-conflict-e2e", ttl_seconds: 100 });

  await loginAsAdmin(page);
  await openTab(page, "resources");
  await pickKind(page, "cache_policies");
  await row(page, "gen-conflict-e2e").getByRole("button", { name: "编辑" }).click();
  const editor = page.getByLabel("资源文档");
  await expect(editor).toHaveValue(/gen-conflict-e2e/);

  // Someone else saves first.
  await stack.admin.put(
    `/admin/resources/cache_policies/${created.id}`,
    { doc: { ...created.doc, ttl_seconds: 200 } },
    created.version,
  );

  const doc = JSON.parse(await editor.inputValue());
  doc.ttl_seconds = 300;
  await editor.fill(JSON.stringify(doc, null, 2));
  await page.getByRole("button", { name: "保存" }).click();
  await expect(page.getByRole("alert")).toContainText("该资源已被他人修改");
  expect((await stack.admin.get(`/admin/resources/cache_policies/${created.id}`)).doc.ttl_seconds).toBe(200);

  await page.getByRole("button", { name: "重新载入" }).click();
  await expect(editor).toHaveValue(/"ttl_seconds": 200/);
  const reloaded = JSON.parse(await editor.inputValue());
  reloaded.ttl_seconds = 300;
  await editor.fill(JSON.stringify(reloaded, null, 2));
  await page.getByRole("button", { name: "保存" }).click();
  await expect
    .poll(async () => (await stack.admin.get(`/admin/resources/cache_policies/${created.id}`)).doc.ttl_seconds)
    .toBe(300);

  // Deleting asks once more on the page; 取消 leaves the row alone.
  await page.getByRole("button", { name: "删除" }).click();
  await page.getByRole("button", { name: "取消" }).click();
  await expect(page.getByRole("button", { name: "确认删除" })).toHaveCount(0);
  await expect(row(page, "gen-conflict-e2e")).toHaveCount(1);
  expect((await stack.admin.request("GET", `/admin/resources/cache_policies/${created.id}`)).status).toBe(200);
  await page.getByRole("button", { name: "删除" }).click();
  await page.getByRole("button", { name: "确认删除" }).click();
  await expect(row(page, "gen-conflict-e2e")).toHaveCount(0);
  expect((await stack.admin.request("GET", `/admin/resources/cache_policies/${created.id}`)).status).toBe(404);
});

test("缓存策略表单：作用于列出后建的模型，缓存时长必填，被拒绝的原因标在该字段旁", async ({ page }) => {
  await seedModel("early-e2e", "upstream-early-e2e");
  await loginAsAdmin(page);
  await openTab(page, "cache");
  const target = page.getByLabel("作用于");
  await page.getByRole("button", { name: "新建缓存策略" }).click();
  await expect(target.locator("option", { hasText: "early-e2e" })).toHaveCount(1);
  await page.getByRole("button", { name: "取消" }).click();

  // A model created elsewhere while this page is open.
  await seedModel("late-e2e", "upstream-late-e2e");
  await page.getByRole("button", { name: "新建缓存策略" }).click();
  await expect(target.locator("option", { hasText: "late-e2e" })).toHaveCount(1);

  await page.getByLabel("名称").fill("cache-field-e2e");
  const ttl = page.getByLabel("缓存时长（秒）");
  const save = page.getByRole("button", { name: "保存" });
  await ttl.fill("");
  await expect(save).toBeDisabled();
  await expect(ttl).toHaveAttribute("aria-invalid", "true");

  // Below the schema's one-second minimum: refused, and the reason sits at the field.
  await ttl.fill("0");
  await expect(save).toBeEnabled();
  await save.click();
  await expect(ttl).toHaveAttribute("aria-invalid", "true");
  await expect(ttl).toHaveAccessibleDescription(/\S/);
  const stored = (await stack.admin.get("/admin/resources/cache_policies")).items.find(
    (i: Json) => i.doc.name === "cache-field-e2e",
  );
  expect(stored, "nothing was saved").toBeUndefined();
});

test("缓存策略的删除要两步确认：取消不删，确认后删除", async ({ page }) => {
  const created = await createResource("cache_policies", { name: "cache-delete-e2e", scope: "team", ttl_seconds: 60 });
  const status = async () => (await stack.admin.request("GET", `/admin/resources/cache_policies/${created.id}`)).status;

  await loginAsAdmin(page);
  await openTab(page, "cache");
  const policy = row(page, "cache-delete-e2e");
  await policy.getByRole("button", { name: "删除", exact: true }).click();
  await policy.getByRole("button", { name: "取消" }).click();
  await expect(policy.getByRole("button", { name: "确认删除" })).toHaveCount(0);
  expect(await status()).toBe(200);

  await policy.getByRole("button", { name: "删除", exact: true }).click();
  // Nothing is deleted until it is confirmed.
  await expect(policy.getByRole("button", { name: "确认删除" })).toBeVisible();
  expect(await status()).toBe(200);
  await policy.getByRole("button", { name: "确认删除" }).click();
  await expect(row(page, "cache-delete-e2e")).toHaveCount(0);
  expect(await status()).toBe(404);
});

test("缓存策略页刷新后列出页面打开后才新建的策略", async ({ page }) => {
  await loginAsAdmin(page);
  await openTab(page, "cache");
  // The page has read its list before the policy is written.
  await expect(page.getByRole("button", { name: "新建缓存策略" })).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
  await createResource("cache_policies", { name: "cache-late-e2e", scope: "team", ttl_seconds: 60 });
  await expect(row(page, "cache-late-e2e")).toHaveCount(0);

  await page.getByRole("button", { name: "刷新" }).click();
  await expect(row(page, "cache-late-e2e")).toBeVisible();
});
