import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { groupModelWorld, loginForGroupModels } from "./group-model-options";

let stack: Stack;
let world: Awaited<ReturnType<typeof groupModelWorld>>;
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
  world = await groupModelWorld(stack);
});
test.afterAll(async () => { await stack?.stop(); });

test("分组价格展示可服务模型并保留失去上游后的覆盖维护入口", async ({ page }) => {
  const historical = await stack.admin.post("/admin/public-models", { name: "chat-price-history" });
  const mapping = `/admin/provider-keys/${world.first.account}/channel-models/${historical.id}`;
  const rates = `/admin/provider-groups/${world.first.groupId}/rates`;
  await stack.admin.put(mapping, { weight: 1 });
  await stack.admin.post(rates, { model_id: historical.id, cny_per_usd: "2", effective_from: new Date(Date.now() + 1500).toISOString() });
  await expect.poll(async () => {
    const plan = await stack.admin.get(rates);
    return Number(plan.models.find((model: { model_id: string }) => model.model_id === historical.id)?.rate.current?.cny_per_usd);
  }).toBe(2);
  await stack.admin.put(mapping, { enabled: false });
  await loginForGroupModels(page, stack.previewUrl, stack.operator);
  await page.goto(stack.previewUrl + "/#pricing?section=groups");
  await page.getByRole("button", { name: world.first.group, exact: true }).click();
  const table = page.getByRole("table", { name: "分组换算率" });
  await expect(table.getByText(world.first.model, { exact: true })).toBeVisible();
  await expect(table.getByText(world.ungranted, { exact: true })).toBeVisible();
  await expect(table.getByText(world.second.model, { exact: true })).toHaveCount(0);
  const previouslyPriced = page.getByRole("row").filter({ has: page.getByText(historical.name, { exact: true }) });
  await expect(previouslyPriced).toBeVisible();
  await expect(previouslyPriced).toContainText("当前分组不可用");
  for (const action of ["设置覆盖", "结束覆盖"]) {
    await previouslyPriced.getByRole("button", { name: action, exact: true }).click();
    await expect(page.getByRole("group", { name: action, exact: true })).toContainText(historical.name);
  }
});

test("管理员切组后仅能创建授权且在新组可用的模型密钥", async ({ page }) => {
  await loginForGroupModels(page, stack.previewUrl, stack.operator);
  await page.goto(stack.previewUrl + "/#keys?tenant=" + world.tenantId);
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await dialog.getByLabel("名称", { exact: true }).fill("group-options-admin");
  const select = async (field: string, name: string) => {
    await dialog.getByRole("combobox", { name: field, exact: true }).click();
    await page.getByRole("option", { name, exact: true }).click();
  };
  await select("分组（必选）", world.first.group);
  await select("模型范围", "指定模型");
  const first = dialog.getByRole("checkbox", { name: world.first.model, exact: true });
  const second = dialog.getByRole("checkbox", { name: world.second.model, exact: true });
  await first.check();
  await expect(second).toHaveCount(0);
  await expect(dialog.getByRole("checkbox", { name: world.ungranted, exact: true })).toHaveCount(0);
  await select("分组（必选）", world.second.group);
  await second.check();
  await expect(first).toHaveCount(0);
  const created = page.waitForResponse(response => response.url().endsWith(`/admin/tenants/${world.tenantId}/keys`) && response.request().method() === "POST");
  await dialog.getByRole("button", { name: "创建", exact: true }).click();
  const response = await created;
  expect(response.status()).toBe(201);
  const { key } = await response.json();
  expect(key.model_ids).toEqual([world.second.id]);

  // 移除该密钥依赖的映射后，仅改名仍应完整保留创建时的模型选择。
  await stack.admin.put(`/admin/provider-keys/${world.second.account}/channel-models/${world.second.id}`, { enabled: false });
  await page.goto(stack.previewUrl + "/#keys?tenant=" + world.tenantId + "&key=" + key.id);
  await page.getByLabel("名称", { exact: true }).fill("renamed-with-historical-model");
  const saved = page.waitForResponse(response => response.url().endsWith("/admin/keys/" + key.id) && response.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存配置", exact: true }).click();
  expect((await saved).status()).toBe(200);
  expect((await stack.admin.get("/admin/keys/" + key.id)).model_ids).toEqual([world.second.id]);
});

test("历史无分组密钥首次绑定分组时保留旧模型并排除本次新增的组外模型", async ({ page }) => {
  const tenantPath = "/admin/tenants/" + world.tenantId;
  const tenant = await stack.admin.get(tenantPath);
  const extra = await stack.admin.post("/admin/public-models", { name: "chat-legacy-extra" });
  await stack.admin.put(`/admin/provider-keys/${world.first.account}/channel-models/${extra.id}`, { weight: 1 });
  await stack.admin.patch(tenantPath, { allowed_model_ids: [world.first.id, world.second.id, extra.id] }, tenant.version);
  const minted = await stack.admin.post(tenantPath + "/keys", { label: "legacy-group-options", group_id: world.first.groupId, model_ids: [world.first.id] });
  const id = minted.key.id;
  // 旧版本签发的密钥没有 group_id；当前 API 每次创建都会指定分组。
  stack.clearKeyGroup(id);
  await loginForGroupModels(page, stack.previewUrl, stack.operator);
  await page.goto(stack.previewUrl + "/#keys?tenant=" + world.tenantId + "&key=" + id);
  await page.getByRole("checkbox", { name: extra.name, exact: true }).check();
  await page.getByRole("combobox", { name: "分组（必选）", exact: true }).click();
  await page.getByRole("option", { name: world.second.group, exact: true }).click();
  await expect(page.getByRole("checkbox", { name: new RegExp(world.first.model) })).toBeChecked();
  await page.getByLabel("名称", { exact: true }).fill("legacy-bound-to-beta");
  const saved = page.waitForResponse(response => response.url().endsWith("/admin/keys/" + id) && response.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存配置", exact: true }).click();
  const response = await saved;
  expect(response.status()).toBe(200);
  const patch = response.request().postDataJSON();
  if ("model_ids" in patch) expect(patch.model_ids).toEqual([world.first.id]);
  const stored = await stack.admin.get("/admin/keys/" + id);
  expect(stored.group.name).toBe(world.second.group);
  expect(stored.model_ids).toEqual([world.first.id]);
});
