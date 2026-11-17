import { randomUUID } from "node:crypto";
import { execFile } from "node:child_process";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { promisify } from "node:util";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { chat, seedTenant } from "./world";
import { chooseSelect } from "./select";

/**
 * 价格 · 租户方案 against the real control plane through the built `dist/`.
 * A tenant's plan is its discount: its prices are its keys' groups', so the
 * page has no per-tenant price to edit. The only stand-in is the upstream the
 * gateway calls.
 */

let stack: Stack;
const pageErrors = new WeakMap<Page, string[]>();
const consoleMessages = new WeakMap<Page, string[]>();

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  const backupDir = process.env.CP_E2E_BACKUP_DIR;
  if (stack && backupDir) {
    const pgUrl = process.env.CP_E2E_PG_URL;
    if (!pgUrl) throw new Error("CP_E2E_PG_URL is required for the test database backup");
    await mkdir(backupDir, { recursive: true, mode: 0o700 });
    await Promise.all([stack.cpDatabase, stack.cpDatabase.replace(/^cp_/, "kine_")].map(async (database) => {
      const url = new URL(pgUrl);
      url.pathname = `/${database}`;
      try {
        await promisify(execFile)("pg_dump", ["--format=custom", `--file=${join(backupDir, `${database}.dump`)}`, url.toString()], { timeout: 30_000 });
      } catch (_error: unknown) {
        throw new Error(`pg_dump failed for test database ${database}`);
      }
    }));
  }
  await stack?.stop();
});

test.beforeEach(async ({ page }) => {
  const errors: string[] = [];
  const messages: string[] = [];
  pageErrors.set(page, errors);
  consoleMessages.set(page, messages);
  page.on("pageerror", (error) => errors.push(error.message));
  page.on("console", (message) => {
    if (message.type() === "error" || message.type() === "warning") messages.push(`${message.type()}: ${message.text()}`);
  });
});

test.afterEach(async ({ page }, testInfo) => {
  await testInfo.attach("browser-health", {
    body: JSON.stringify({ url: page.url(), title: await page.title(), pageErrors: pageErrors.get(page), console: consoleMessages.get(page) }),
    contentType: "application/json",
  });
  expect(pageErrors.get(page)).toEqual([]);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
});

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

/** 从真实列表入口打开租户方案。 */
async function openPlans(page: Page, who = stack.operator): Promise<void> {
  await login(page, who.email, who.password);
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("button", { name: "租户方案" }).click();
  await expect(page.getByRole("heading", { name: "租户方案", exact: true })).toBeVisible();
  await expect(page.getByRole("table", { name: "租户折扣", exact: true })).toBeVisible();
}

function tenantRow(page: Page, tenantName: string): Locator {
  return page.getByRole("table", { name: "租户折扣", exact: true }).getByRole("row")
    .filter({ has: page.getByRole("button", { name: tenantName, exact: true }) });
}

async function openPlan(page: Page, tenantName: string, who = stack.operator): Promise<Locator> {
  await openPlans(page, who);
  await tenantRow(page, tenantName).getByRole("button", { name: tenantName, exact: true }).click();
  const detail = page.getByRole("region", { name: `租户折扣 ${tenantName}`, exact: true });
  await expect(detail).toBeVisible();
  return detail;
}

/** 没有折扣历史的租户从搜索选择入口开始。 */
async function newDiscount(page: Page, search: string, tenantId: string): Promise<Locator> {
  await page.getByRole("button", { name: "新租户折扣", exact: true }).click();
  const form = page.getByRole("form", { name: "新租户折扣", exact: true });
  await form.getByLabel("搜索租户", { exact: true }).fill(search);
  await chooseSelect(form.getByRole("combobox", { name: "租户", exact: true }), { value: tenantId });
  return form;
}

async function tenantWithoutCalls(label: string): Promise<{ tenantId: string; tenantName: string }> {
  const suffix = randomUUID().slice(0, 8);
  const tenantName = `${label} ${suffix}`;
  const signedUp = await stack.tenant().post("/api/signup", {
    org_name: tenantName,
    email: `discount-${suffix}@e2e.local`,
    password: "discount-tenant-pass-6c1",
  });
  await stack.admin.post(`/admin/tenants/${signedUp.tenant.id}/approve`);
  return { tenantId: signedUp.tenant.id, tenantName };
}

async function revision(tenantId: string, discountBps: number, effectiveFrom = new Date().toISOString()) {
  return stack.admin.post("/admin/pricing/discounts", {
    tenant_id: tenantId,
    effective_from: effectiveFrom,
    discount_bps: discountBps,
  });
}

function displayTime(iso: string): string {
  return new Date(Date.parse(iso) + 8 * 3_600_000).toISOString().slice(0, 19).replace("T", " ");
}

test("租户方案：设置 20% 折扣，下一次调用按折后价计费", async ({ page }) => {
  const world = await seedTenant(stack);
  await openPlans(page);
  const form = await newDiscount(page, world.tenantName, world.tenantId);
  await expect(form).toContainText("无折扣");

  await form.getByLabel("折扣（%）").fill("20");
  await form.getByRole("button", { name: "保存折扣" }).click();
  await expect(page.getByTestId("current-discount")).toHaveText("20%");

  // The world's card is 1000 / 2000 USD per million and its group's rate is 1
  // CNY per USD, so one chat is 5 x 1000 + 3 x 2000 = 11 000 µ¥; 20% off is
  // 8 800 µ¥, rounded once.
  const requestId = `req-plan-${randomUUID().slice(0, 8)}`;
  expect(await chat(stack, world.key.plaintext, world.model.name, { requestId })).toBe(200);
  await expect
    .poll(async () => (await stack.admin.get(`/admin/usage/events?request_id=${requestId}`)).items.length, { timeout: 30_000 })
    .toBe(1);
  const event = (await stack.admin.get(`/admin/usage/events?request_id=${requestId}`)).items[0];
  await expect
    .poll(async () => (await stack.admin.get(`/admin/pricing/charges?event_id=${event.event_id}`)).items[0]?.charge_micro_cny, {
      timeout: 30_000,
    })
    .toBe(8_800);
});

test("折扣超出 0 到 100 时被拒绝并说明原因，什么也没保存", async ({ page }) => {
  const world = await seedTenant(stack);
  await openPlans(page);
  const form = await newDiscount(page, world.tenantName, world.tenantId);
  const percent = form.getByLabel("折扣（%）");
  await percent.fill("101");
  await form.getByRole("button", { name: "保存折扣" }).click();
  await expect(form.getByText("折扣应是 0 到 100 的百分数")).toBeVisible();
  await expect(percent).toHaveAttribute("aria-invalid", "true");
  await expect(percent).toBeFocused();
  await expect(page.getByTestId("current-discount")).toHaveText("无折扣");
  expect((await stack.admin.get(`/admin/pricing/discounts?tenant_id=${world.tenantId}`)).items).toHaveLength(0);

  // 先保留拒绝时的所有断言，再验证原表单可以直接纠正。
  await percent.fill("15");
  await form.getByRole("button", { name: "保存折扣" }).click();
  await expect(page.getByTestId("current-discount")).toHaveText("15%");
  expect((await stack.admin.get(`/admin/pricing/discounts?tenant_id=${world.tenantId}`)).items).toHaveLength(1);
});

test("只读运维看到折扣，但没有编辑控件，也看不到专属价", async ({ page }) => {
  const world = await seedTenant(stack);
  await stack.admin.post("/admin/pricing/discounts", {
    tenant_id: world.tenantId,
    effective_from: new Date().toISOString(),
    discount_bps: 1250,
  });
  const viewer = { email: `viewer-plan-${randomUUID().slice(0, 6)}@e2e.local`, password: "viewer-plan-pass-6c1" };
  await stack.admin.post("/admin/operators", { ...viewer, display_name: "只读", role: "viewer" });
  await openPlan(page, world.tenantName, viewer);

  await expect(page.getByTestId("current-discount")).toHaveText("12.5%");
  // The tenant's price is its key's group's: this page has no per-tenant price.
  await expect(page.getByRole("region", { name: "专属价" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /保存/ })).toHaveCount(0);
  await expect(page.getByRole("form")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "新折扣", exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "返回租户方案", exact: true }).click();
  await expect(tenantRow(page, world.tenantName)).toContainText("12.5%");
  await expect(page.getByRole("button", { name: "新租户折扣", exact: true })).toHaveCount(0);
});

test("折扣租户列表区分当前、预约、已取消和从未配置，名称及 ID 都可搜索", async ({ page }, testInfo) => {
  const prefix = `折扣状态-${randomUUID().slice(0, 6)}`;
  const [active, scheduled, cancelled, none] = await Promise.all([
    tenantWithoutCalls(`${prefix}-当前`),
    tenantWithoutCalls(`${prefix}-预约`),
    tenantWithoutCalls(`${prefix}-取消`),
    tenantWithoutCalls(`${prefix}-无记录`),
  ]);
  const [current, future] = await Promise.all([
    revision(active.tenantId, 2000),
    revision(scheduled.tenantId, 3000, new Date(Date.now() + 86_400_000).toISOString()),
  ]);
  const expired = await revision(cancelled.tenantId, 1500);
  const revoked = await revision(cancelled.tenantId, 0);
  await openPlans(page);
  await page.getByLabel("搜索租户折扣", { exact: true }).fill(prefix);
  await expect(tenantRow(page, active.tenantName)).toContainText("20%");
  await expect(tenantRow(page, active.tenantName)).toContainText(displayTime(current.effective_from));
  await expect(tenantRow(page, scheduled.tenantName)).toContainText("30%");
  await expect(tenantRow(page, scheduled.tenantName)).toContainText(displayTime(future.effective_from));
  await expect(tenantRow(page, cancelled.tenantName)).toHaveCount(0);
  await expect(tenantRow(page, none.tenantName)).toHaveCount(0);
  await expect(page.getByRole("table", { name: "租户折扣", exact: true }).locator("tbody tr")).toHaveCount(2);
  await page.screenshot({ path: testInfo.outputPath("desktop-list.png"), fullPage: true });

  await chooseSelect(page.getByRole("combobox", { name: "折扣状态", exact: true }), { label: "全部已配置" });
  await expect(tenantRow(page, cancelled.tenantName)).toBeVisible();
  await expect(tenantRow(page, none.tenantName)).toHaveCount(0);
  await expect(page.getByRole("table", { name: "租户折扣", exact: true }).locator("tbody tr")).toHaveCount(3);
  await page.getByLabel("搜索租户折扣", { exact: true }).fill(cancelled.tenantId);
  await expect(page.getByRole("table", { name: "租户折扣", exact: true }).locator("tbody tr")).toHaveCount(1);
  await tenantRow(page, cancelled.tenantName).getByRole("button", { name: cancelled.tenantName, exact: true }).click();
  const history = page.getByRole("table", { name: "折扣修改记录", exact: true });
  await expect(page.getByTestId("current-discount")).toHaveText("无折扣");
  await expect(history.locator("tbody tr")).toHaveCount(2);
  await expect(history.getByRole("cell", { name: "15%", exact: true })).toBeVisible();
  await expect(history.getByRole("cell", { name: "0% 恢复原价", exact: true })).toBeVisible();
  await expect(history).toContainText(displayTime(expired.effective_from));
  await expect(history).toContainText(displayTime(revoked.effective_from));
  await page.screenshot({ path: testInfo.outputPath("cancelled-history.png"), fullPage: true });
});

test("完整历史跨页保留同一生效时间的每次修改，新折扣追加记录", async ({ page }, testInfo) => {
  const tenant = await tenantWithoutCalls("完整折扣历史");
  const effectiveFrom = new Date(Date.now() + 86_400_000).toISOString();
  // 超过一页并复用生效时间：截断或按生效时间去重都会丢失可观察记录。
  const originals = [];
  for (let percent = 1; percent <= 53; percent += 1) {
    originals.push(await revision(tenant.tenantId, percent * 100, effectiveFrom));
  }
  const detail = await openPlan(page, tenant.tenantName);
  const history = detail.getByRole("table", { name: "折扣修改记录", exact: true });
  await expect(history.locator("tbody tr")).toHaveCount(50);
  await detail.getByRole("button", { name: "读取更早记录", exact: true }).click();
  await expect(history.locator("tbody tr")).toHaveCount(53);
  await expect(detail.getByRole("button", { name: "读取更早记录", exact: true })).toHaveCount(0);
  const percentages = await history.getByRole("cell").filter({ hasText: /^\d+(?:\.\d+)?%$/ }).allTextContents();
  expect(percentages).toEqual(Array.from({ length: 53 }, (_, index) => `${53 - index}%`));
  await expect(history.locator("tbody tr").first().getByRole("cell", { name: "53%", exact: true })).toBeVisible();
  await expect(history.locator("tbody tr").last().getByRole("cell", { name: "1%", exact: true })).toBeVisible();
  await expect(history.locator("tbody tr").last()).toContainText(displayTime(effectiveFrom));

  for (const percent of [22.5, 0]) {
    await detail.getByRole("button", { name: "新折扣", exact: true }).click();
    const form = page.getByRole("form", { name: "新折扣", exact: true });
    await form.getByLabel("折扣（%）").fill(String(percent));
    await form.getByRole("button", { name: "保存折扣" }).click();
    await expect(detail.getByTestId("current-discount")).toHaveText(percent === 0 ? "无折扣" : `${percent}%`);
  }
  const earlier = detail.getByRole("button", { name: "读取更早记录", exact: true });
  if (await earlier.isVisible()) await earlier.click();
  await expect(history.locator("tbody tr")).toHaveCount(55);
  for (const percent of [0, 1, 22.5, 53]) {
    await expect(history.getByRole("cell", { name: percent === 0 ? "0% 恢复原价" : `${percent}%`, exact: true })).toBeVisible();
  }
  const stored = await stack.admin.get(`/admin/pricing/discounts?tenant_id=${tenant.tenantId}&limit=100`);
  expect(stored.items).toHaveLength(55);
  const immutable = (row: { revision_id: number; discount_bps: number; effective_from: string }) => ({
    revision_id: row.revision_id, discount_bps: row.discount_bps, effective_from: row.effective_from,
  });
  expect(stored.items.slice(2).map(immutable)).toEqual(originals.map(immutable).reverse());
  await page.screenshot({ path: testInfo.outputPath("complete-history.png"), fullPage: true });
});

test("手机布局可按租户 ID 搜索并预约新折扣，详情和返回列表均可操作", async ({ page }, testInfo) => {
  const tenant = await tenantWithoutCalls("手机预约折扣");
  await openPlans(page);
  await page.setViewportSize({ width: 390, height: 844 });
  const form = await newDiscount(page, tenant.tenantId, tenant.tenantId);
  await form.getByLabel("折扣（%）").fill("25");
  await form.getByLabel("立即生效", { exact: true }).uncheck();
  const typed = displayTime(new Date(Date.now() + 2 * 86_400_000).toISOString()).slice(0, 16);
  await form.getByLabel("生效时间", { exact: true }).fill(typed);
  await page.screenshot({ path: testInfo.outputPath("mobile-new-discount.png"), fullPage: true });
  await form.getByRole("button", { name: "保存折扣" }).click();
  const detail = page.getByRole("region", { name: `租户折扣 ${tenant.tenantName}`, exact: true });
  await expect(detail).toBeVisible();
  await expect(detail.getByTestId("current-discount")).toHaveText("无折扣");
  const history = detail.getByRole("table", { name: "折扣修改记录", exact: true });
  await expect(history.getByRole("cell", { name: "25%", exact: true })).toBeVisible();
  await expect(history).toContainText(`${typed}:00`);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.screenshot({ path: testInfo.outputPath("mobile-detail.png"), fullPage: true });
  await detail.getByRole("button", { name: "返回租户方案", exact: true }).click();
  await page.getByLabel("搜索租户折扣", { exact: true }).fill(tenant.tenantId);
  await expect(tenantRow(page, tenant.tenantName)).toContainText("25%");
  await expect(tenantRow(page, tenant.tenantName)).toContainText(`${typed}:00`);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.screenshot({ path: testInfo.outputPath("mobile-list.png"), fullPage: true });
  const stored = await stack.admin.get(`/admin/pricing/discounts?tenant_id=${tenant.tenantId}`);
  expect(stored.items).toHaveLength(1);
  expect(stored.items[0].discount_bps).toBe(2500);
  expect(Date.parse(stored.items[0].effective_from)).toBe(Date.parse(`${typed.replace(" ", "T")}:00+08:00`));
});
