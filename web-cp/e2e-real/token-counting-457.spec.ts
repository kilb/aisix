import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { spawnSync } from "node:child_process";
import { ApiClient } from "../e2e/stack";
import { join, resolve } from "node:path";
import { startStack, STACK_START_TIMEOUT_MS, type Stack } from "../e2e/stack";
import { chooseSelect, selectOptions } from "../e2e/select";
import { startEmptyFleet } from "./token-counting-457-empty-fleet";

// 场景与反例先固定在 task-4-scenarios.md；只使用真实隔离控制面，不替换网络。
interface Resource { id: string; version: number; doc: Record<string, unknown>; rollout?: { state: string; applied: number; online: number } }
interface Mapping { model_id: string; source_model_name: string; model_name: string }
interface Target { provider_key_id: string; model_mappings: Mapping[] }
interface Counting { messages_count_tokens?: Target; responses_input_tokens?: Target }
const path = "/admin/resources/token_counting";
const output = resolve(process.env.TOKEN_COUNTING_QA_OUTPUT!);
let stack: Stack;
const fixture: Record<string, Resource> = {};
const sourceVersion = "same-source-version-2026-01-01";
const viewer = { email: "viewer-counting-457@e2e.local", password: "isolated-viewer-counting-457" };

function isolatedDatabase(): void {
  const configured = process.env.CP_E2E_PG_URL;
  if (!configured || !["127.0.0.1", "localhost"].includes(new URL(configured).hostname)) {
    throw new Error("Token-counting acceptance requires an explicitly configured isolated loopback Postgres");
  }
}
async function account(name: string, provider: string, groups: string[]): Promise<Resource> {
  return stack.admin.post("/admin/resources/provider_keys", { doc: {
    display_name: `457-${name}`, provider, api_key: `isolated-management-only-${name}`,
    api_base: "http://127.0.0.1:9/v1", groups,
  } });
}
async function model(name: string, owner: Resource, provider: string): Promise<Resource> {
  return stack.admin.post("/admin/resources/models", { doc: {
    display_name: `457-${name}`, provider, provider_key_id: owner.id, model_name: sourceVersion,
  } });
}
async function counting(): Promise<Resource> {
  const listed = await stack.admin.get(path);
  expect(listed.items, "the environment singleton exists exactly once").toHaveLength(1);
  return listed.items[0] as Resource;
}
async function applied(expected: Counting): Promise<Resource> {
  let latest: Resource | undefined;
  await expect.poll(async () => {
    latest = await counting();
    return latest.rollout?.state;
  }, { timeout: 45_000, message: "the exact saved token-counting resource reaches a real managed gateway" }).toBe("applied");
  expect(latest!.rollout!.online).toBeGreaterThanOrEqual(1);
  expect(latest!.rollout!.applied).toBe(latest!.rollout!.online);
  for (const key of ["messages_count_tokens", "responses_input_tokens"] as const) {
    if (expected[key] === undefined) expect(latest!.doc[key] == null, `${key} is disabled in stored and propagated configuration`).toBe(true);
    else {
      const observed = structuredClone(latest!.doc[key]);
      // 仅验收断言的负向自检：模拟线上缺字段，不能改产品或伪造上游响应。
      if (process.env.TOKEN_COUNTING_QA_MUTATION === "drop_mapping_snapshot" && key === "messages_count_tokens") delete (observed as { model_mappings: Partial<Mapping>[] }).model_mappings[0].source_model_name;
      expect(observed).toEqual(expected[key]);
    }
  }
  return latest!;
}
async function login(page: Page, who = stack.operator, direct = false): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  const response = page.waitForResponse(reply => new URL(reply.url()).pathname === "/admin/login" && reply.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status()).toBe(200);
  await page.goto(`${stack.previewUrl}/#providers${direct ? "?section=token-counting" : ""}`);
  if (!direct) {
    await expect(page.getByRole("table", { name: "上游账号", exact: true })).toBeVisible();
    await page.getByRole("navigation", { name: "上游账号管理", exact: true }).getByRole("link", { name: "Token计数", exact: true }).click();
  }
  expect(new URLSearchParams(page.url().split("?")[1]).get("section")).toBe("token-counting");
  await expect(page.getByRole("region", { name: "Token计数", exact: true })).toBeVisible();
}
async function save(page: Page, region: Locator): Promise<void> {
  const response = page.waitForResponse(reply => new URL(reply.url()).pathname.startsWith(path) && ["POST", "PUT"].includes(reply.request().method()));
  await region.getByRole("button", { name: "保存Token计数", exact: true }).click();
  const result = await response;
  expect([200, 201], await result.text()).toContain(result.status());
}
async function mapping(group: Locator, source: Resource, target: string): Promise<void> {
  await group.getByRole("button", { name: "添加模型映射", exact: true }).click();
  const index = await group.getByRole("combobox", { name: "源模型", exact: true }).count() - 1;
  await chooseSelect(group.getByRole("combobox", { name: "源模型", exact: true }).nth(index), { value: source.id });
  await expect(group.getByRole("textbox", { name: "源上游模型（只读）", exact: true }).nth(index)).toHaveValue(sourceVersion);
  await group.getByRole("textbox", { name: "计数上游模型", exact: true }).nth(index).fill(target);
}
async function rejectedDelete(kind: string, resource: Resource): Promise<void> {
  const resourcePath = `/admin/resources/${kind}/${resource.id}`;
  const current = await stack.admin.request("GET", resourcePath);
  expect(current.status).toBe(200);
  const deleted = await stack.admin.request("DELETE", resourcePath, undefined, current.version!);
  expect([409, 422], deleted.text).toContain(deleted.status);
  expect((await stack.admin.request("GET", resourcePath)).status, "a refusal must preserve the referenced object").toBe(200);
}

test.describe("real management stack", () => {
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  isolatedDatabase();
  stack = await startStack({ mockUpstream: false });
  expect(stack.mockUpstreamUrl).toBe("");
  await expect.poll(async () => (await stack.admin.get("/admin/gateways/instances")).items.filter((instance: { online: boolean }) => instance.online).length, { timeout: 30_000 }).toBe(1);
  await stack.admin.seedGroups(["counts-only-457"]);
  await stack.admin.post("/admin/operators", { ...viewer, role: "viewer" });
  fixture.sourceA = await account("source-a", "anthropic", ["shared"]);
  fixture.sourceB = await account("source-b", "anthropic", ["shared"]);
  fixture.sourceR = await account("source-r", "openai", ["shared"]);
  fixture.messages = await account("count-messages", "anthropic", ["counts-only-457"]);
  fixture.messages2 = await account("count-messages-replacement", "anthropic", ["counts-only-457"]);
  fixture.responses = await account("count-responses", "openai", ["counts-only-457"]);
  fixture.modelA = await model("source-a-default", fixture.sourceA, "anthropic");
  fixture.modelB = await model("source-b-default", fixture.sourceB, "anthropic");
  fixture.modelR = await model("source-r-default", fixture.sourceR, "openai");
  mkdirSync(output, { recursive: true });
  writeFileSync(join(output, "environment-public.json"), JSON.stringify({ realBackend: true, mockUpstream: false, officialProviderCalls: "NOT EXECUTED", httpUrl: stack.httpUrl, adminUrl: stack.previewUrl, proxyUrl: stack.proxyUrl, cpDatabase: stack.cpDatabase, fixtureIds: Object.fromEntries(Object.entries(fixture).map(([name, resource]) => [name, resource.id])) }));
});
test.afterAll(async () => { await stack?.stop(); });

test("457: fixed count accounts persist by source Model ID, propagate, reject stale snapshots, protect references and disable", async ({ page }) => {
  const failures: string[] = [];
  const upstreamActions: string[] = [];
  const consoles: object[] = [];
  page.on("pageerror", error => failures.push(error.message));
  page.on("console", message => {
    if (["error", "warning"].includes(message.type())) consoles.push({ type: message.type(), text: message.text() });
    const bootstrap = message.text().includes("401") && /\/admin\/(me|session)$/.test(message.location().url);
    if (message.type() === "error" && !bootstrap) failures.push(message.text());
  });
  page.on("request", request => {
    const url = new URL(request.url());
    expect(["127.0.0.1", "localhost"], "the browser only contacts isolated loopback services").toContain(url.hostname);
    if (/^\/v1\/|\/probe(?:\/|$)|\/upstream-models$|\/model-tests(?:\/|$)/.test(url.pathname)) upstreamActions.push(`${request.method()} ${url.pathname}`);
  });
  await login(page);
  const region = page.getByRole("region", { name: "Token计数", exact: true });
  await expect(region.getByText("引用已有文件、会话或加密上下文的计数请求仍沿用原上游。", { exact: true })).toBeVisible();
  const messages = region.getByRole("group", { name: "Messages · count_tokens", exact: true });
  const responses = region.getByRole("group", { name: "Responses · input_tokens", exact: true });
  await chooseSelect(messages.getByRole("combobox", { name: "计数账号", exact: true }), { value: fixture.messages.id });
  await mapping(messages, fixture.modelA, "count-model-a");
  await mapping(messages, fixture.modelB, "count-model-b");
  await chooseSelect(responses.getByRole("combobox", { name: "计数账号", exact: true }), { value: fixture.responses.id });
  await mapping(responses, fixture.modelR, "count-model-r");
  const expected: Counting = {
    messages_count_tokens: { provider_key_id: fixture.messages.id, model_mappings: [
      { model_id: fixture.modelA.id, source_model_name: sourceVersion, model_name: "count-model-a" },
      { model_id: fixture.modelB.id, source_model_name: sourceVersion, model_name: "count-model-b" },
    ] },
    responses_input_tokens: { provider_key_id: fixture.responses.id, model_mappings: [{ model_id: fixture.modelR.id, source_model_name: sourceVersion, model_name: "count-model-r" }] },
  };
  await save(page, region);
  await applied(expected);
  // 租户只获源 A，helper 仍在独立组，既不增加模型授权也不扩大生成权限。
  const helperModel = await stack.admin.post("/admin/resources/models", { doc: { display_name: "457-helper-paid-model", provider: "anthropic", provider_key_id: fixture.messages.id, model_name: "helper-paid-model" } });
  const tenant = await stack.admin.post("/admin/tenants", { name: "457-source-acl-only" });
  await stack.admin.patch(`/admin/tenants/${tenant.id}`, { allowed_model_ids: [fixture.modelA.id], allowed_provider_groups: ["shared"] }, tenant.version);
  await stack.admin.post(`/admin/tenants/${tenant.id}/credits`, { amount_cny: "10.00", reason: "隔离权限验收，不发送生成请求" });
  const groups = await stack.admin.get("/admin/provider-groups");
  const shared = groups.items.find((group: { name: string }) => group.name === "shared");
  expect(shared).toBeDefined();
  const caller = await stack.admin.post(`/admin/tenants/${tenant.id}/keys`, { label: "457-source-only", group_id: shared.id, allowed_endpoints: ["chat_completions", "messages", "count_tokens", "responses", "models"] });
  const headers = { authorization: `Bearer ${caller.plaintext}`, "content-type": "application/json" };
  await expect.poll(async () => { const result = await fetch(`${stack.proxyUrl}/v1/models`, { headers }); await result.arrayBuffer(); return result.status; }, { timeout: 30_000 }).toBe(200);
  for (const [endpoint, request] of [
    ["/v1/messages/count_tokens", { model: "457-source-b-default", messages: [{ role: "user", content: "blocked by source model ACL" }] }],
    ["/v1/responses/input_tokens", { model: "457-source-r-default", input: "blocked by source model ACL" }],
    ["/v1/chat/completions", { model: "457-helper-paid-model", messages: [{ role: "user", content: "helper receives no generation grant" }] }],
  ] as const) {
    const refused = await fetch(`${stack.proxyUrl}${endpoint}`, { method: "POST", headers, body: JSON.stringify(request) });
    expect(refused.status, `${endpoint} retains caller ACL before any upstream request`).toBe(403);
    await refused.arrayBuffer();
  }
  const tenantRead = await stack.admin.get(`/admin/tenants/${tenant.id}`);
  expect(tenantRead.allowed_model_ids).toEqual([fixture.modelA.id]);
  expect(tenantRead.allowed_provider_groups).toEqual(["shared"]);
  expect(helperModel.id).not.toBe(fixture.modelA.id);
  await page.reload();
  await expect(messages.getByRole("combobox", { name: "计数账号", exact: true })).toHaveAttribute("data-value", fixture.messages.id);
  await expect(responses.getByRole("combobox", { name: "计数账号", exact: true })).toHaveAttribute("data-value", fixture.responses.id);
  for (const [index, target] of ["count-model-a", "count-model-b"].entries()) {
    await expect(messages.getByRole("textbox", { name: "计数上游模型", exact: true }).nth(index)).toHaveValue(target);
    await expect(messages.getByRole("textbox", { name: "源上游模型（只读）", exact: true }).nth(index)).toHaveValue(sourceVersion);
  }
  await expect(messages.getByRole("combobox", { name: "源模型", exact: true }).nth(0)).toContainText(fixture.modelA.id);
  await expect(messages.getByRole("combobox", { name: "源模型", exact: true }).nth(1)).toContainText(fixture.modelB.id);
  await expect(page).toHaveTitle("Routio 运维台");
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
  await page.screenshot({ path: join(output, "457-counting-saved-1440.png"), fullPage: true });
  await page.setViewportSize({ width: 390, height: 1000 });
  const sourcePicker = messages.getByRole("combobox", { name: "源模型", exact: true }).first();
  await sourcePicker.scrollIntoViewIfNeeded();
  expect(await (await selectOptions(sourcePicker)).count()).toBeGreaterThan(0);
  const popup = page.getByRole("listbox");
  const popupBounds = await popup.boundingBox();
  expect(popupBounds, "the actual long-label source-model popup is visible").not.toBeNull();
  expect(popupBounds!.x).toBeGreaterThanOrEqual(0);
  expect(popupBounds!.x + popupBounds!.width).toBeLessThanOrEqual(391);
  expect(popupBounds!.y).toBeGreaterThanOrEqual(0);
  expect(popupBounds!.y + popupBounds!.height).toBeLessThanOrEqual(1001);
  await page.screenshot({ path: join(output, "457-counting-source-options-390.png"), fullPage: true });
  writeFileSync(join(output, "457-counting-source-options-bounds.json"), JSON.stringify({ viewport: { width: 390, height: 1000 }, popup: popupBounds }));
  await page.keyboard.press("Escape");
  await expect(popup).toHaveCount(0);
  await page.setViewportSize({ width: 1440, height: 1000 });

  // 控制组可删除，引用中的账号和源模型必须拒绝删除，避免“任何删除都失败”的假绿。
  const free = await account("unreferenced-control", "openai", ["shared"]);
  const freeRead = await stack.admin.request("GET", `/admin/resources/provider_keys/${free.id}`);
  const removed = await stack.admin.request("DELETE", `/admin/resources/provider_keys/${free.id}`, undefined, freeRead.version!);
  expect([200, 204], removed.text).toContain(removed.status);
  expect((await stack.admin.request("GET", `/admin/resources/provider_keys/${free.id}`)).status).toBe(404);
  await rejectedDelete("provider_keys", fixture.messages);
  await rejectedDelete("models", fixture.modelA);
  await applied(expected);

  await chooseSelect(messages.getByRole("combobox", { name: "计数账号", exact: true }), { value: fixture.messages2.id });
  await messages.getByRole("textbox", { name: "计数上游模型", exact: true }).first().fill("count-model-a-updated");
  expected.messages_count_tokens!.provider_key_id = fixture.messages2.id;
  expected.messages_count_tokens!.model_mappings[0].model_name = "count-model-a-updated";
  await save(page, region);
  const lastValid = await applied(expected);
  await page.reload();
  await expect(messages.getByRole("combobox", { name: "计数账号", exact: true })).toHaveAttribute("data-value", fixture.messages2.id);
  await expect(messages.getByRole("textbox", { name: "计数上游模型", exact: true }).first()).toHaveValue("count-model-a-updated");
  await expect(responses.getByRole("combobox", { name: "计数账号", exact: true })).toHaveAttribute("data-value", fixture.responses.id);

  // 模拟保存表单后源模型改版；旧快照必须拒绝，而不是静默改映射。
  const currentSource = await stack.admin.request("GET", `/admin/resources/models/${fixture.modelA.id}`);
  expect(currentSource.status).toBe(200);
  const sourceUpdate = await stack.admin.request("PUT", `/admin/resources/models/${fixture.modelA.id}`, { doc: { ...currentSource.body.doc, model_name: "source-revised-version" } }, currentSource.version!);
  expect(sourceUpdate.status, sourceUpdate.text).toBe(200);
  const freshConfig = await counting();
  const staleSave = await stack.admin.request("PUT", `${path}/${lastValid.id}`, { doc: expected }, freshConfig.version);
  expect(staleSave.status, staleSave.text).toBe(422);
  expect(staleSave.body.code).toBe("validation_failed");
  expect(staleSave.body.details.map((detail: { path: string }) => detail.path)).toContain("/messages_count_tokens/model_mappings/0/source_model_name");
  expect((await counting()).doc).toEqual(lastValid.doc);

  // 恢复源行后通过 UI 关闭一个独立块，另一个协议保持原配置。
  const revised = await stack.admin.request("GET", `/admin/resources/models/${fixture.modelA.id}`);
  expect((await stack.admin.request("PUT", `/admin/resources/models/${fixture.modelA.id}`, { doc: { ...revised.body.doc, model_name: sourceVersion } }, revised.version!)).status).toBe(200);
  await chooseSelect(messages.getByRole("combobox", { name: "计数账号", exact: true }), { label: "未启用（沿用原上游）" });
  delete expected.messages_count_tokens;
  await save(page, region);
  await applied(expected);
  await page.reload();
  await expect(messages.getByRole("combobox", { name: "计数账号", exact: true })).toContainText("未启用（沿用原上游）");
  await expect(responses.getByRole("textbox", { name: "计数上游模型", exact: true })).toHaveValue("count-model-r");
  await rejectedDelete("provider_keys", fixture.responses);
  await rejectedDelete("models", fixture.modelR);
  await page.setViewportSize({ width: 390, height: 1000 });
  await region.scrollIntoViewIfNeeded();
  await page.screenshot({ path: join(output, "457-counting-disabled-390.png"), fullPage: true });
  const geometry = await page.evaluate(() => ({ width: document.documentElement.clientWidth, scroll: document.documentElement.scrollWidth }));
  expect(geometry.scroll, "Token计数 fits the mobile viewport").toBeLessThanOrEqual(geometry.width + 1);
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.getByRole("navigation", { name: "上游账号管理", exact: true }).getByRole("link", { name: "上游账号", exact: true }).click();
  const accountTable = page.getByRole("table", { name: "上游账号", exact: true });
  await expect(accountTable).toBeVisible();
  await accountTable.getByRole("row").filter({ has: page.getByRole("cell", { name: "457-source-a", exact: true }) }).getByRole("button", { name: /编辑|查看/ }).click();
  await expect(page.getByRole("region", { name: "模型", exact: true })).toBeVisible();
  const viewContext = await page.context().browser()!.newContext();
  const viewPage = await viewContext.newPage();
  await login(viewPage, viewer, true);
  const readOnly = viewPage.getByRole("region", { name: "Token计数", exact: true });
  const counterValue = readOnly.getByRole("textbox", { name: "计数上游模型", exact: true });
  if (await counterValue.count()) await expect(counterValue).toHaveValue("count-model-r");
  else await expect(readOnly).toContainText("count-model-r");
  const controls = readOnly.getByRole("button", { name: "保存Token计数", exact: true });
  if (await controls.count()) await expect(controls).toBeDisabled();
  for (const control of await readOnly.getByRole("combobox").all()) await expect(control).toBeDisabled();
  const viewerApi = new ApiClient(stack.httpUrl);
  await viewerApi.post("/admin/login", viewer);
  const saved = await counting();
  expect((await viewerApi.request("PUT", `${path}/${saved.id}`, { doc: {} }, saved.version)).status).toBe(403);
  expect((await counting()).doc).toEqual(saved.doc);
  await viewPage.screenshot({ path: join(output, "457-counting-viewer.png"), fullPage: true });
  await viewContext.close();
  expect(failures, "the real UI has no runtime errors").toEqual([]);
  expect(upstreamActions, "management never probes providers or sends count/generation requests").toEqual([]);
  await test.info().attach("457-console", { body: JSON.stringify(consoles), contentType: "application/json" });
});
});

test("457: actual archived 641 gateway rejects fixed count configuration", async () => {
  isolatedDatabase();
  mkdirSync(output, { recursive: true });
  const binary = process.env.TOKEN_COUNTING_OLD_GATEWAY_BIN;
  if (!binary) throw new Error("TOKEN_COUNTING_OLD_GATEWAY_BIN must name the authorized archived 641 executable; no skipped compatibility assertion");
  expect(createHash("sha256").update(readFileSync(binary)).digest("hex")).toBe("dbe75f6601df47e1084a1d8a01974162c46d24f4cfbf1f541d9918037a380782");
  const version = spawnSync(binary, ["--version"], { encoding: "utf8" });
  expect(version.status).toBe(0);
  expect(version.stdout.trim()).toBe("aisix dev+sha-641ccf9");
  const oldStack = await startEmptyFleet();
  const assertOld = (refused: { status: number; body: any; text: string }, block = "messages_count_tokens"): void => {
    expect(refused.status, refused.text).toBe(422);
    expect(refused.body.code).toBe("gateway_too_old");
    expect(refused.body.details[0].path).toBe(`/${block}`);
    expect(refused.body.details[0].message).toContain("网关");
  };
  try {
    expect((await oldStack.admin.get("/admin/gateways/instances")).items).toEqual([]);
    const owner = await oldStack.admin.post("/admin/resources/provider_keys", { doc: { display_name: "457-old-count", provider: "anthropic", api_key: "isolated-old-management-only", api_base: "http://127.0.0.1:9/v1", groups: ["shared"] } });
    const other = await oldStack.admin.post("/admin/resources/provider_keys", { doc: { display_name: "457-old-count-other", provider: "anthropic", api_key: "isolated-old-other-management-only", api_base: "http://127.0.0.1:9/v1", groups: ["shared"] } });
    const responsesOwner = await oldStack.admin.post("/admin/resources/provider_keys", { doc: { display_name: "457-old-responses-count", provider: "openai", api_key: "isolated-old-responses-only", api_base: "http://127.0.0.1:9/v1", groups: ["shared"] } });
    const source = await oldStack.admin.post("/admin/resources/models", { doc: { display_name: "457-old-source", provider: "anthropic", provider_key_id: owner.id, model_name: sourceVersion } });
    const responsesSource = await oldStack.admin.post("/admin/resources/models", { doc: { display_name: "457-old-responses-source", provider: "openai", provider_key_id: responsesOwner.id, model_name: sourceVersion } });
    const enabled = {
      messages_count_tokens: { provider_key_id: owner.id, model_mappings: [{ model_id: source.id, source_model_name: sourceVersion, model_name: "count-model" }] },
      responses_input_tokens: { provider_key_id: responsesOwner.id, model_mappings: [{ model_id: responsesSource.id, source_model_name: sourceVersion, model_name: "count-responses-model" }] },
    };
    const created = await oldStack.admin.request("POST", path, { doc: enabled });
    expect(created.status, created.text).toBe(201);
    const resourcePath = `${path}/${created.body.id}`;
    expect((await oldStack.admin.get(resourcePath)).doc.messages_count_tokens).toEqual(enabled.messages_count_tokens);
    expect((await oldStack.admin.get(resourcePath)).doc.responses_input_tokens).toEqual(enabled.responses_input_tokens);

    await oldStack.bootArchivedGateway(binary);
    await expect.poll(async () => (await oldStack.admin.get("/admin/gateways/instances")).items.some((instance: { version: string; online: boolean }) => instance.online && instance.version === "dev+sha-641ccf9"), { timeout: 30_000 }).toBe(true);
    const retargeted = { ...enabled, messages_count_tokens: { ...enabled.messages_count_tokens, provider_key_id: other.id } };
    const remapped = { ...enabled, messages_count_tokens: { provider_key_id: owner.id, model_mappings: [{ model_id: source.id, source_model_name: sourceVersion, model_name: "count-model-remapped" }] } };
    const changeStatuses: number[] = [];
    for (const changed of [retargeted, remapped]) {
      const fresh = await oldStack.admin.request("GET", resourcePath);
      const update = await oldStack.admin.request("PUT", resourcePath, { doc: changed }, fresh.version!);
      assertOld(update);
      changeStatuses.push(update.status);
      expect((await oldStack.admin.get(resourcePath)).doc.messages_count_tokens).toEqual(enabled.messages_count_tokens);
      expect((await oldStack.admin.get(resourcePath)).doc.responses_input_tokens).toEqual(enabled.responses_input_tokens);
    }

    const beforeDisable = await oldStack.admin.request("GET", resourcePath);
    const disabled = await oldStack.admin.request("PUT", resourcePath, { doc: {} }, beforeDisable.version!);
    expect(disabled.status, disabled.text).toBe(200);
    const off = await oldStack.admin.request("GET", resourcePath);
    expect(off.body.doc.messages_count_tokens == null).toBe(true);
    expect(off.body.doc.responses_input_tokens == null).toBe(true);
    const removed = await oldStack.admin.request("DELETE", resourcePath, undefined, off.version!);
    expect([200, 204], removed.text).toContain(removed.status);
    expect((await oldStack.admin.get(path)).items).toEqual([]);
    const firstSave = await oldStack.admin.request("POST", path, { doc: { messages_count_tokens: enabled.messages_count_tokens } });
    assertOld(firstSave);
    expect((await oldStack.admin.get(path)).items).toEqual([]);
    const responsesFirstSave = await oldStack.admin.request("POST", path, { doc: { responses_input_tokens: enabled.responses_input_tokens } });
    assertOld(responsesFirstSave, "responses_input_tokens");
    expect((await oldStack.admin.get(path)).items).toEqual([]);
    writeFileSync(join(output, "457-old641-public.json"), JSON.stringify({ executableSha256: "dbe75f6601df47e1084a1d8a01974162c46d24f4cfbf1f541d9918037a380782", executableVersion: version.stdout.trim(), actualGatewayRegistered: true, emptyFleetSaveStatus: created.status, retargetStatus: changeStatuses[0], remapStatus: changeStatuses[1], disableStatus: disabled.status, emptySingletonDeleteStatus: removed.status, messagesFirstSaveWithOldGatewayStatus: firstSave.status, responsesFirstSaveWithOldGatewayStatus: responsesFirstSave.status, officialProviderCalls: "NOT EXECUTED" }));
  } finally {
    await oldStack.stop();
  }
});
