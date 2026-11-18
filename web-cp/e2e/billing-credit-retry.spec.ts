import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

let stack: Stack;
test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});
test.afterAll(async () => { await stack?.stop(); });

test("刷新后重试平台入账，已经登记的款项仍只有一笔", async ({ page, context }) => {
  const tenant = await stack.admin.post("/admin/tenants", { name: "Credit Retry", billing_mode: "prepaid" });
  const path = `/admin/tenants/${tenant.id}/credits`;
  const topups = () => stack.admin.get(`/admin/billing/topups?tenant_id=${tenant.id}&origin=operator`);
  const reason = "年度服务退款";
  const reference = "BANK-0720";
  const errors: string[] = [];
  page.on("pageerror", error => errors.push(error.message));
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(stack.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(stack.operator.password);
  const login = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await login).status()).toBe(200);
  await page.goto(`${stack.previewUrl}/#billing?section=accounts&tenant=${tenant.id}`);
  const form = page.getByRole("region", { name: "直接入账", exact: true });
  await form.getByLabel("金额（元）").fill("0.08");
  await form.getByLabel("原因").fill(reason);
  await form.getByLabel("凭证号").fill(reference);
  await form.getByRole("button", { name: "入账", exact: true }).click();

  // 浏览器断网时观察实际提交内容，独立客户端随后把同一操作送达真实控制面。
  await context.setOffline(true);
  const attempt = page.waitForRequest(request => new URL(request.url()).pathname === path && request.method() === "POST");
  await form.getByRole("button", { name: "确认入账" }).click();
  const original = (await attempt).postDataJSON();
  await expect(form.getByRole("alert")).toContainText("入账结果未知");
  await context.setOffline(false);
  const receipt = await stack.admin.post(path, original);
  expect(receipt.amount_micro_cny).toBe(80_000);
  expect((await topups()).items).toHaveLength(1);

  await page.reload();
  await expect(form.getByLabel("金额（元）")).toHaveValue("0.08");
  await expect(form.getByLabel("原因")).toHaveValue(reason);
  await expect(form.getByLabel("凭证号")).toHaveValue(reference);
  const replay = page.waitForResponse(response => new URL(response.url()).pathname === path && response.request().method() === "POST");
  await form.getByRole("button", { name: "重试这笔入账" }).click();
  const answer = await replay;
  expect(answer.status()).toBe(201);
  expect(await answer.json()).toEqual(receipt);
  await expect(page.getByText("已入账", { exact: true })).toBeVisible();
  await expect(page.getByRole("region", { name: "余额", exact: true }).getByText("¥0.08", { exact: true }).first()).toBeVisible();
  const account = await stack.admin.get(`/admin/tenants/${tenant.id}/billing`);
  expect(account.balance_micro_cny).toBe(80_000);
  const ledger = await stack.admin.get(`/admin/tenants/${tenant.id}/billing/ledger?kind=topup`);
  expect(ledger.items.map((item: { amount_micro_cny: number }) => item.amount_micro_cny)).toEqual([80_000]);
  await expect(form.getByLabel("金额（元）")).toHaveValue("");
  const { items } = await topups();
  expect(items.map((item: { id: string }) => item.id)).toEqual([receipt.id]);
  expect(errors).toEqual([]);
});
