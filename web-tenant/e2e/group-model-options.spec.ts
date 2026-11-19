import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { groupModelWorld, loginForGroupModels } from "../../web-cp/e2e/group-model-options";

let stack: Stack;
let world: Awaited<ReturnType<typeof groupModelWorld>>;
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
  world = await groupModelWorld(stack);
});
test.afterAll(async () => { await stack?.stop(); });

test("租户切组后只可勾选新组中已授权的模型", async ({ page }) => {
  await loginForGroupModels(page, `${stack.previewUrl}/login`, world.owner);
  await page.goto(stack.previewUrl + "/#keys");
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "新建密钥", exact: true });
  await dialog.getByLabel("名称", { exact: true }).fill("group-options-tenant");
  await dialog.getByRole("radio", { name: world.first.group }).check();
  await dialog.getByRole("radio", { name: "指定模型", exact: true }).check();
  const first = dialog.getByRole("checkbox", { name: world.first.model, exact: true });
  const second = dialog.getByRole("checkbox", { name: world.second.model, exact: true });
  await first.check();
  await expect(second).toHaveCount(0);
  await expect(dialog.getByRole("checkbox", { name: world.ungranted, exact: true })).toHaveCount(0);
  await dialog.getByRole("radio", { name: world.second.group }).check();
  await dialog.getByRole("searchbox", { name: "搜索模型", exact: true }).fill("*");
  await dialog.getByRole("button", { name: "选择全部匹配模型", exact: true }).click();
  await expect(second).toBeChecked();
  await expect(first).toHaveCount(0);
  const created = page.waitForResponse(response => response.url().endsWith("/api/keys") && response.request().method() === "POST");
  await dialog.getByRole("button", { name: "新建密钥", exact: true }).click();
  const response = await created;
  expect(response.status()).toBe(201);
  const { key } = await response.json();
  expect(key.model_ids).toEqual([world.second.id]);

  // 移除该密钥依赖的映射后，仅改名仍应完整保留创建时的模型选择。
  await stack.admin.put(`/admin/provider-keys/${world.second.account}/channel-models/${world.second.id}`, { enabled: false });
  await page.goto(stack.previewUrl + "/#keys?key=" + key.id + "&edit=1");
  await page.getByLabel("名称", { exact: true }).fill("renamed-with-historical-model");
  const saved = page.waitForResponse(response => response.url().endsWith("/api/keys/" + key.id) && response.request().method() === "PATCH");
  await page.getByRole("button", { name: "保存修改", exact: true }).click();
  expect((await saved).status()).toBe(200);
  expect((await stack.admin.get("/admin/keys/" + key.id)).model_ids).toEqual([world.second.id]);
});
