import { expect, test } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import type { Login } from "./world";

let stack: Stack;
const owner: Login = { email: "budget-alert-owner@example.test", password: "budget-alert-password" };
const member: Login = { email: "budget-alert-member@example.test", password: "budget-alert-password" };

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
  const { tenant } = await stack.tenant().post("/api/signup", { org_name: "Budget alerts E2E", ...owner });
  await stack.admin.post(`/admin/tenants/${tenant.id}/approve`);
  await stack.admin.post(`/admin/tenants/${tenant.id}/credits`, { amount_cny: "10", reason: "Budget alerts E2E" });
  const invitation = await stack.admin.post(`/admin/tenants/${tenant.id}/invites`, { role: "member", email: member.email });
  await stack.tenant().post("/api/invites/accept", { token: invitation.invite_token, password: member.password });
});

test.afterAll(async () => {
  await stack?.stop();
});

test("组织所有者可在预算页保存、查看并关闭预算通知", async ({ context, page }) => {
  const login = await context.request.post(stack.previewUrl + "/api/login", { data: owner });
  expect(login.status()).toBe(200);
  const settings = page.waitForResponse(r => r.url().endsWith("/api/billing/alerts") && r.request().method() === "GET");
  await page.goto(stack.previewUrl + "/console#budgets");
  expect((await settings).status()).toBe(200);
  const panel = page.getByRole("region", { name: "预算通知", exact: true });
  const balance = panel.getByLabel("可用额度低于或等于（元）");
  const percent = panel.getByLabel("组织预算使用率（%）");
  const spike = panel.getByLabel("消费突增阈值（%）");
  const email = panel.getByLabel("收件邮箱");
  await expect(balance).toHaveValue("");
  await balance.fill("1.25");
  await percent.fill("80");
  await spike.fill("250");
  await email.fill(owner.email);
  // 组织已充值 ¥10，此规则不会触发；保留地址的口令只用于验证回显。
  const webhook = panel.getByLabel("Webhook 地址");
  await webhook.fill("https://hooks.example.test/receive?token=opaque-e2e");
  const save = panel.getByRole("button", { name: "保存通知设置", exact: true });
  await save.click();
  await expect(page.getByText("通知设置已保存", { exact: true })).toBeVisible();
  await page.reload();
  await expect(balance).toHaveValue("1.25");
  await expect(percent).toHaveValue("80");
  await expect(spike).toHaveValue("250");
  await expect(email).toHaveValue(owner.email);
  await expect(webhook).toHaveValue("");
  await expect(panel.getByText("已配置 Webhook", { exact: true })).toBeVisible();

  await panel.getByRole("checkbox", { name: "移除已配置的 Webhook" }).check();
  await balance.clear();
  await percent.clear();
  await spike.clear();
  await email.clear();
  await save.click();
  await expect(panel.getByText("已配置 Webhook", { exact: true })).toHaveCount(0);
  await page.reload();
  await expect(save).toBeEnabled();
  await expect(balance).toHaveValue("");
  await expect(email).toHaveValue("");
});

test("普通成员在预算页看不到通知设置，也不能直接读取", async ({ context, page }) => {
  const login = await context.request.post(stack.previewUrl + "/api/login", { data: member });
  expect(login.status()).toBe(200);
  await page.goto(stack.previewUrl + "/console#budgets");
  await expect(page.getByRole("heading", { name: "预算", exact: true })).toBeVisible();
  await expect(page.getByRole("region", { name: "预算通知", exact: true })).toHaveCount(0);
  const status = await page.evaluate(async () => (await fetch("/api/billing/alerts")).status);
  expect(status).toBe(403);
});
