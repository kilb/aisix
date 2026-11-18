import { expect, test } from "@playwright/test";
import { chooseSelect, selectOptions } from "./select";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

let stack: Stack;
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});
test.afterAll(async () => { await stack?.stop(); });

test("公共模型按分组编辑与添加账号，保存后其它组独有的映射保持原值", async ({ page }) => {
  const alpha = "route-alpha";
  const beta = "route-beta";
  await stack.admin.seedGroups([alpha, beta]);
  async function account(name: string, groups: string[]): Promise<string> {
    const result = await stack.admin.post("/admin/resources/provider_keys", {
      doc: { display_name: name, provider: "openai", api_key: "test-account", groups },
    });
    return result.id;
  }
  const [shared, onlyBeta, addAlpha, addBeta] = await Promise.all([
    account("both-groups", [alpha, beta]), account("beta-mapped", [beta]),
    account("alpha-available", [alpha]), account("beta-available", [beta]),
  ]);
  const model = await stack.admin.post("/admin/public-models", { name: "group-route-editor" });
  const mapping = (id: string) => `/admin/provider-keys/${id}/channel-models/${model.id}`;
  await stack.admin.put(mapping(shared), { priority: 9, weight: 2 });
  await stack.admin.put(mapping(onlyBeta), { priority: 6, weight: 7 });
  const resource = `/admin/public-models/${model.id}`;
  const before = await stack.admin.get(resource);

  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(stack.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(stack.operator.password);
  const login = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await login).status()).toBe(200);
  await page.goto(`${stack.previewUrl}/#public-routes?section=${model.id}`);
  const form = page.getByRole("form", { name: "编辑模型「group-route-editor」" });
  await expect(form).toBeVisible();
  const scope = form.getByRole("combobox", { name: "查看分组", exact: true });
  await expect(scope).toBeVisible();
  const sharedRow = form.getByRole("listitem", { name: "目标 both-groups", exact: true });
  const betaRow = form.getByRole("listitem", { name: "目标 beta-mapped", exact: true });
  await expect(scope).toHaveText(alpha);
  await expect(sharedRow).toBeVisible();
  await expect(betaRow).toHaveCount(0);
  await expect(sharedRow).toContainText(/修改.*所有分组|各分组共享/);
  await expect(form.getByRole("region", { name: "选路设置" })).toContainText(/适用于所有分组/);
  await sharedRow.getByRole("spinbutton", { name: "权重" }).fill("4");
  await expect(scope).toBeDisabled();
  await form.getByRole("button", { name: "添加目标", exact: true }).click();
  const picker = form.getByRole("combobox", { name: "上游账号", exact: true });
  const options = await selectOptions(picker);
  await expect(options.filter({ hasText: "alpha-available" })).toBeVisible();
  await expect(options.filter({ hasText: "beta-available" })).toHaveCount(0);
  await page.getByRole("option", { name: /alpha-available/ }).click();
  const writes: string[] = [];
  page.on("request", request => {
    if (request.method() === "PUT" && request.url().includes("/channel-models/")) writes.push(new URL(request.url()).pathname);
  });
  await form.getByRole("button", { name: "保存", exact: true }).click();
  await expect(page.getByText("已保存「group-route-editor」的路由设置。", { exact: true })).toBeVisible();
  const after = await stack.admin.get(resource);
  const row = (rows: { provider_key_id: string }[], id: string) => rows.find(item => item.provider_key_id === id);
  expect(writes.sort()).toEqual([mapping(addAlpha), mapping(shared)].sort());
  expect(row(before.served_by, onlyBeta)).toMatchObject({ priority: 6, weight: 7, enabled: true });
  expect(row(after.served_by, onlyBeta)).toEqual(row(before.served_by, onlyBeta));
  expect(row(after.served_by, shared)).toMatchObject({ weight: 4 });
  expect(row(after.served_by, addAlpha)).toMatchObject({ enabled: true });
  expect(row(after.served_by, addBeta)).toBeUndefined();
  await chooseSelect(scope, beta);
  await expect(betaRow).toBeVisible();
  await expect(sharedRow.getByRole("spinbutton", { name: "权重" })).toHaveValue("4");
  await expect(form.getByRole("listitem", { name: "目标 alpha-available", exact: true })).toHaveCount(0);
  await chooseSelect(scope, { label: "全部分组" });
  await expect(form.getByRole("listitem", { name: /^目标 / })).toHaveCount(3);
});
