import { randomUUID } from "node:crypto";
import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, fundTenant, startStack, type Json, type Stack } from "./stack";
import { chat } from "./world";

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});

test.afterAll(async () => {
  await stack?.stop();
});

async function account(name: string) {
  return stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: name,
      provider: "openai",
      api_key: "sk-test-account-toggle",
      api_base: "http://127.0.0.1:9/v1",
      groups: ["shared"],
    },
  });
}

async function openList(page: Page, who = stack.operator) {
  const login = await page.request.post(`${stack.previewUrl}/admin/login`, { data: who });
  expect(login.ok()).toBe(true);
  await page.goto(`${stack.previewUrl}/#providers`);
}

function accountRow(page: Page, name: string) {
  return page.getByRole("table", { name: "上游账号", exact: true }).getByRole("row").filter({
    has: page.getByText(name, { exact: true }),
  });
}

test("账号列表一键下架和上架，保留连接设置与原有模型映射", async ({ page }) => {
  const name = "availability-operations";
  const saved = await account(name);
  const path = `/admin/resources/provider_keys/${saved.id}`;
  const mappings = `/admin/provider-keys/${saved.id}/channel-models`;
  const enabled = await stack.admin.post("/admin/public-models", { name: "availability-serving" });
  const disabled = await stack.admin.post("/admin/public-models", { name: "availability-idle" });
  await stack.admin.put(`${mappings}/${enabled.id}`, { priority: 3, weight: 7, upstream_model: "active-name" });
  await stack.admin.put(`${mappings}/${disabled.id}`, { priority: 2, weight: 5, upstream_model: "inactive-name" });
  await stack.admin.put(`${mappings}/${disabled.id}`, { enabled: false });
  const tenant = await stack.admin.post("/admin/tenants", { name: "availability-tenant" });
  await fundTenant(stack.admin, tenant.id);
  await stack.admin.patch(`/admin/tenants/${tenant.id}`, { allowed_model_ids: [enabled.id] }, tenant.version);
  const { plaintext } = await stack.admin.post(`/admin/tenants/${tenant.id}/keys`, { label: "availability-key" });
  const original = await stack.admin.get(path);
  const operator = { email: "availability-operator@example.test", password: "account-operator-test-104" };
  await stack.admin.post("/admin/operators", { ...operator, role: "operator" });
  await openList(page, operator);
  const row = accountRow(page, name);
  await expect(row.getByText("已上架", { exact: true })).toBeVisible();

  for (const [action, status, off] of [["下架", "已下架", true], ["上架", "已上架", false]] as const) {
    await row.getByRole("button", { name: action, exact: true }).click();
    await expect(row.getByText(status, { exact: true })).toBeVisible();
    if (off) await page.setViewportSize({ width: 390, height: 844 });
    await page.reload();
    await expect(row.getByText(status, { exact: true })).toBeVisible();
    const stored = await stack.admin.get(path);
    const { disabled: isDisabled = false, ...settings } = stored.doc;
    expect(isDisabled).toBe(off);
    const { disabled: _originalState, ...originalSettings } = original.doc;
    expect(settings).toEqual(originalSettings);
    expect((await stack.admin.get(mappings)).items).toEqual(expect.arrayContaining([
      expect.objectContaining({ public_model_id: enabled.id, enabled: true, weight: 7, priority: 3, upstream_model: "active-name" }),
      expect.objectContaining({ public_model_id: disabled.id, enabled: false, weight: 5, priority: 2, upstream_model: "inactive-name" }),
    ]));
    await expect.poll(async () => (await stack.admin.get(path)).rollout.state, { timeout: 30_000 }).toBe("applied");
    const requestId = randomUUID();
    // 上游端口没有服务。下架与连接失败都可能返回 503，终态日志能区分是否选过这个账号。
    expect(await chat(stack, plaintext, enabled.name, { requestId })).toBe(503);
    await expect.poll(async () => {
      const { items } = await stack.admin.get(`/admin/usage/events?request_id=${requestId}`);
      return items.filter((row: Json) => row.event.is_final_attempt).map((row: Json) => row.event.provider_key_id ?? null);
    }, { timeout: 30_000 }).toEqual([off ? null : saved.id]);
  }
});

test("同时编辑的账号先显示版本冲突，重读后才能再次下架", async ({ page }) => {
  const name = "availability-concurrent";
  const stored = await account(name);
  const path = `/admin/resources/provider_keys/${stored.id}`;
  await openList(page);
  const action = accountRow(page, name).getByRole("button", { name: "下架", exact: true });
  await expect(action).toBeVisible();
  const latestName = "availability-renamed";
  await stack.admin.put(path, { doc: { ...stored.doc, display_name: latestName } }, stored.version);
  const result = page.waitForResponse((reply) => new URL(reply.url()).pathname === path && reply.request().method() === "PUT");
  await action.click();
  expect((await result).status()).toBe(412);
  await expect(page.getByText(/下架失败.*已被他人修改/)).toBeVisible();
  const row = accountRow(page, latestName);
  await expect(row.getByText("已上架", { exact: true })).toBeVisible();
  await row.getByRole("button", { name: "下架", exact: true }).click();
  await expect(row.getByText("已下架", { exact: true })).toBeVisible();
  expect((await stack.admin.get(path)).doc).toMatchObject({ display_name: latestName, disabled: true });
});

test("只读运维能看上下架状态", async ({ page }) => {
  const name = "availability-viewer";
  await account(name);
  const viewer = { email: "availability-viewer@example.test", password: "account-viewer-test-104" };
  await stack.admin.post("/admin/operators", { ...viewer, role: "viewer" });
  await openList(page, viewer);
  const row = accountRow(page, name);
  await expect(row.getByText("已上架", { exact: true })).toBeVisible();
  await expect(row.getByRole("button", { name: /^(下架|上架)$/ })).toHaveCount(0);
});
