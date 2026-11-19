import { randomUUID } from "node:crypto";
import { readFile } from "node:fs/promises";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { chooseSelect } from "./select";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack({ mockUpstream: false });
});

test.afterAll(async () => {
  await stack?.stop();
});

async function csv(page: Page, button: Locator, endpoint: string, filename: string) {
  const responsePromise = page.waitForResponse(response => new URL(response.url()).pathname === endpoint);
  const downloadPromise = page.waitForEvent("download");
  await button.click();
  const [response, download] = await Promise.all([responsePromise, downloadPromise]);
  expect(response.status()).toBe(200);
  expect(download.suggestedFilename()).toBe(filename);
  const text = await readFile(await download.path(), "utf8");
  expect(text.startsWith("\ufeff")).toBe(true);
  return { text, query: new URL(response.url()).searchParams };
}

test("组织成员按当前日期和状态下载真实记录", async ({ page }) => {
  const owner = { email: `exports-${randomUUID()}@tenant.test`, password: "Exports-local-owner-123" };
  const tenant = stack.tenant();
  const organization = await tenant.post("/api/signup", { org_name: "CSV 下载验收", ...owner });
  await stack.admin.post(`/admin/tenants/${organization.tenant.id}/approve`);
  await tenant.post("/api/login", owner);
  const ref = "private-finance@example.test";
  const credit = await stack.admin.post(`/admin/tenants/${organization.tenant.id}/credits`, { amount_cny: "24.71", reason: ref });
  const topup = await tenant.post("/api/billing/topups", { amount_cny: "8.53", reference: ref });

  await page.goto(`${stack.previewUrl}/login`);
  await page.getByLabel("邮箱", { exact: true }).fill(owner.email);
  await page.getByLabel("口令", { exact: true }).fill(owner.password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  const account = page.getByRole("link", { name: "账务账户", exact: true });
  await expect(account).toBeVisible();
  await account.click();

  const topups = page.getByRole("region", { name: "充值记录", exact: true });
  await chooseSelect(topups.getByLabel("充值状态"), "approved");
  const selectedDay = new Date(credit.requested_at).toLocaleDateString("sv-SE", { timeZone: "Asia/Shanghai" });
  await topups.getByLabel("充值开始日期").fill(selectedDay);
  await topups.getByLabel("充值结束日期").fill(selectedDay);
  const topupCsv = await csv(page, topups.getByRole("button", { name: "下载 CSV", exact: true }), "/api/billing/topups/export", "billing-topups.csv");
  expect(topupCsv.query.get("status")).toBe("approved");
  expect(topupCsv.query.get("from")).toBe(new Date(`${selectedDay}T00:00:00+08:00`).toISOString());
  expect(topupCsv.query.get("to")).toBe(new Date(Date.parse(`${selectedDay}T00:00:00+08:00`) + 86_400_000).toISOString());
  expect(topupCsv.text).toContain(credit.id);
  expect(topupCsv.text).toContain("24710000");
  expect(topupCsv.text).not.toContain(topup.id);
  expect(topupCsv.text).not.toContain(ref);

  const ledger = page.getByRole("region", { name: "余额流水", exact: true });
  await chooseSelect(ledger.getByLabel("类型"), "topup");
  const ledgerCsv = await csv(page, ledger.getByRole("button", { name: "下载 CSV", exact: true }), "/api/billing/ledger/export", "billing-ledger.csv");
  expect(ledgerCsv.query.get("kind")).toBe("topup");
  expect(ledgerCsv.text).toContain(credit.id);
  expect(ledgerCsv.text).toContain("24710000");
  expect(ledgerCsv.text).not.toContain(topup.id);

  await page.getByRole("link", { name: "用量分析", exact: true }).click();
  const usage = page.getByRole("region", { name: "用量分析筛选", exact: true });
  await usage.getByLabel("开始日期").fill(selectedDay);
  await usage.getByLabel("结束日期").fill(selectedDay);
  const usageCsv = await csv(page, usage.getByRole("button", { name: "下载调用 CSV", exact: true }), "/api/usage/calls/export", "usage-calls.csv");
  expect(usageCsv.query.get("from")).toBe(topupCsv.query.get("from"));
  expect(usageCsv.query.get("to")).toBe(topupCsv.query.get("to"));
  expect(usageCsv.text.trim().split("\r\n")).toHaveLength(1);
  expect(usageCsv.text).toContain("charge_micro_cny");

  await usage.getByRole("link", { name: "查看调用记录", exact: true }).click();
  await page.setViewportSize({ width: 390, height: 844 });
  const calls = await csv(page, page.getByRole("button", { name: "下载 CSV", exact: true }), "/api/usage/calls/export", "usage-calls.csv");
  expect(calls.query.get("from")).toBe(usageCsv.query.get("from"));
  expect(calls.query.get("to")).toBe(usageCsv.query.get("to"));
  expect(calls.text).toBe(usageCsv.text);
});
