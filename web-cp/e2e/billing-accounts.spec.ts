import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";
import { chat, seedTenant, waitClearOfUtcMidnight, type BillingWorld } from "./world";

/**
 * 计费 · 账户: every tenant's mode, balance, credit limit and state, and one
 * tenant's ledger, usage and quotas in a side panel. Real control plane, real
 * gateway; the upstream stand-in is the only fake.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

async function login(page: Page, email = stack.operator.email, password = stack.operator.password): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令").fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await page.locator('[data-tab="billing"]').click();
}

const accountRow = (page: Page, world: BillingWorld): Locator =>
  page.getByRole("table", { name: "账户列表" }).locator("tbody tr").filter({ hasText: world.tenantName });
const cell = (row: Locator, column: number): Locator => row.getByRole("cell").nth(column);
const region = (page: Page, name: string): Locator => page.getByRole("region", { name, exact: true });

/** Reads the list again until `check` holds: a charge reaches the account a moment after the call. */
async function refreshUntil(page: Page, check: () => Promise<void>): Promise<void> {
  await expect(async () => {
    await page.getByRole("button", { name: "刷新" }).click();
    await check();
  }).toPass({ timeout: 30_000 });
}

/** Waits until the tenant's ledger holds `n` entries: the charges are posted asynchronously. */
async function ledgerHolds(world: BillingWorld, n: number): Promise<void> {
  await expect
    .poll(async () => (await stack.admin.get(`/admin/tenants/${world.tenantId}/billing/ledger`)).items.length, { timeout: 30_000 })
    .toBeGreaterThanOrEqual(n);
}

test("账户列表显示每个租户的模式、余额与是否停止，平台内部租户在最后", async ({ page }) => {
  const prepaid = await seedTenant(stack, { mode: "prepaid", creditCny: "0" });
  const postpaid = await seedTenant(stack, { mode: "postpaid", creditLimitCny: "0.05" });
  await login(page);

  const a = accountRow(page, prepaid);
  await expect(cell(a, 1)).toHaveText("预付");
  await expect(cell(a, 2)).toHaveText("¥0.00");
  await expect(cell(a, 5)).toHaveText("已停止");
  await expect(cell(a, 5).locator("[data-state]")).toHaveAttribute("data-state", "exhausted");

  const b = accountRow(page, postpaid);
  await expect(cell(b, 1)).toHaveText("后付");
  await expect(cell(b, 2)).toHaveText("¥0.00");
  await expect(cell(b, 3)).toHaveText("¥0.05");
  await expect(cell(b, 4)).toHaveText("¥0.05");
  await expect(cell(b, 5)).toHaveText("正常");
  await expect(cell(b, 5).locator("[data-state]")).toHaveAttribute("data-state", "ok");

  const last = page.getByRole("table", { name: "账户列表" }).locator("tbody tr").last();
  await expect(last).toContainText("平台内部");
  await expect(cell(last, 5)).not.toContainText("正常");
  await expect(cell(last, 5)).not.toContainText("已停止");
});

test("后付租户未设信用额度时给出提示，设置后状态随之变化", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "postpaid" });
  await login(page);
  await expect(cell(accountRow(page, world), 5)).toHaveText("已停止");

  await accountRow(page, world).getByRole("button", { name: world.tenantName }).click();
  const panel = page.getByRole("dialog");
  await expect(panel.getByText("后付租户尚未设置信用额度，所有调用都会被拒绝")).toBeVisible();

  await panel.getByLabel("信用额度（元）").fill("1.00");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(panel.getByText("后付租户尚未设置信用额度，所有调用都会被拒绝")).toHaveCount(0);
  await expect(cell(accountRow(page, world), 5)).toHaveText("正常");
  await expect(cell(accountRow(page, world), 3)).toHaveText("¥1.00");
  // The gateway now admits the tenant: the limit reached it with the answer.
  await expect.poll(() => chat(stack, world.key.plaintext, world.model.name), { timeout: 15_000 }).toBe(200);
});

test("金额格式不对时不发送请求并提示", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "postpaid", creditLimitCny: "0.05" });
  await login(page);
  await accountRow(page, world).getByRole("button", { name: world.tenantName }).click();
  const panel = page.getByRole("dialog");
  await panel.getByLabel("信用额度（元）").fill("0.001");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(panel.getByText("请输入金额，最多两位小数")).toBeVisible();
  await expect(panel.getByLabel("信用额度（元）")).toBeFocused();
  expect((await stack.admin.get(`/admin/tenants/${world.tenantId}/billing`)).credit_limit_micro_cny).toBe(50_000);
});

test("后付租户用尽信用额度后显示已停止，提高后恢复", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "postpaid", creditLimitCny: "0.05" });
  // Five calls cost 0.055, one past the limit; the fifth is admitted because four cost only 0.044.
  for (let i = 0; i < 5; i++) expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await login(page);
  await refreshUntil(page, async () => {
    await expect(cell(accountRow(page, world), 5)).toHaveText("已停止", { timeout: 1_000 });
  });
  await expect(cell(accountRow(page, world), 2)).toHaveText("-¥0.055");

  await accountRow(page, world).getByRole("button", { name: world.tenantName }).click();
  const panel = page.getByRole("dialog");
  await panel.getByLabel("信用额度（元）").fill("1.00");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(cell(accountRow(page, world), 5)).toHaveText("正常");
  await expect(cell(accountRow(page, world), 4)).toHaveText("¥0.945");
});

test("预付租户设置信用额度后可以透支调用，用尽后停止，改为后付后状态随新的计费方式更新", async ({ page }) => {
  // Signup, approval and a key are the world's own seed through the real
  // `/api`; the organisation starts prepaid with no balance and no credit,
  // so the credit limit is what opens it.
  const world = await seedTenant(stack, { mode: "prepaid", creditCny: "0" });
  await login(page);
  await accountRow(page, world).getByRole("button", { name: world.tenantName }).click();
  const panel = page.getByRole("dialog");

  // A prepaid tenant with no credit is stopped, and the panel says so.
  await expect(cell(accountRow(page, world), 5)).toHaveText("已停止");
  await expect(panel.getByText("余额与信用额度都用尽，调用会被拒绝")).toBeVisible();
  // The input is there for a prepaid tenant too: the limit is its overdraft.
  await panel.getByLabel("信用额度（元）").fill("0.05");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(cell(accountRow(page, world), 3)).toHaveText("¥0.05");
  await expect(cell(accountRow(page, world), 5)).toHaveText("正常");
  // The gateway admits the prepaid tenant on the credit alone.
  await expect.poll(() => chat(stack, world.key.plaintext, world.model.name), { timeout: 15_000 }).toBe(200);

  // Four more calls cost 0.011 each, so the five together take the balance to
  // -0.055, past the 0.05 limit; the fifth is admitted because four cost 0.044.
  for (let i = 0; i < 4; i++) expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  // The limit is spent: the gateway refuses the next call, prepaid or not.
  // Calls on their way while the mark is still being written may be
  // admitted, so the balance ends at or past the limit, never at a figure
  // that would leave money to spend.
  await expect.poll(() => chat(stack, world.key.plaintext, world.model.name), { timeout: 15_000 }).toBe(429);

  await refreshUntil(page, async () => {
    await expect(cell(accountRow(page, world), 5)).toHaveText("已停止", { timeout: 1_000 });
  });
  const past = (await stack.admin.get(`/admin/tenants/${world.tenantId}/billing`)).balance_micro_cny as number;
  expect(past).toBeLessThanOrEqual(-50_000);

  // The billing mode is changed here, on the account's own panel, with a
  // confirmation: it moves the block decision.
  const mode = region(page, "计费方式");
  await mode.getByRole("button", { name: "改为后付" }).click();
  await mode.getByRole("button", { name: "确认改为后付" }).click();
  await expect(cell(accountRow(page, world), 1)).toHaveText("后付");
  await expect(mode).toContainText("后付");
  expect((await stack.admin.get(`/admin/tenants/${world.tenantId}`)).billing_mode).toBe("postpaid");
  // The same money under the same floor: still stopped, and the gateway
  // still refuses until the limit is raised.
  await expect(cell(accountRow(page, world), 5)).toHaveText("已停止");
  await expect.poll(() => chat(stack, world.key.plaintext, world.model.name), { timeout: 15_000 }).toBe(429);

  // Raising the limit lifts the mark for the postpaid tenant, and the
  // gateway answers again.
  await panel.getByLabel("信用额度（元）").fill("1.00");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(cell(accountRow(page, world), 5)).toHaveText("正常");
  await expect.poll(() => chat(stack, world.key.plaintext, world.model.name), { timeout: 15_000 }).toBe(200);
});

test("预付租户没有信用额度时，面板说明余额用尽即停止", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditCny: "10.00" });
  await login(page);
  await accountRow(page, world).getByRole("button", { name: world.tenantName }).click();
  const panel = page.getByRole("dialog");
  await expect(panel.getByText("尚无信用额度：余额用尽即停止调用")).toBeVisible();
  // The limit reads as a figure on the panel for a prepaid tenant, not as
  // an absent one.
  await expect(region(page, "余额")).toContainText("信用额度");
});

test("租户详情显示流水、用量汇总与只读额度", async ({ page }) => {
  await waitClearOfUtcMidnight();
  const world = await seedTenant(stack, { mode: "prepaid", creditCny: "10.00" });
  await stack.admin.post(`/admin/tenants/${world.tenantId}/budgets`, { kind: "periodic", period: "month", limit_cny: "2.00" });
  expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await ledgerHolds(world, 2);
  await login(page);
  await accountRow(page, world).getByRole("button", { name: world.tenantName }).click();

  const ledger = region(page, "余额流水");
  const usage = ledger.getByRole("row").filter({ hasText: "chat-ui" });
  await expect(cell(usage, 1)).toHaveText("扣费");
  await expect(cell(usage, 2)).toHaveText("-¥0.011");
  await expect(cell(usage, 3)).toHaveText("¥9.989");
  const credit = ledger.getByRole("row").filter({ hasText: "充值" });
  await expect(cell(credit, 2)).toHaveText("¥10.00");

  // Today, in UTC, whatever the browser's zone: one period row.
  const periods = region(page, "按期汇总").getByRole("row").filter({ has: page.getByRole("cell") });
  await expect(periods).toHaveCount(1);
  await expect(cell(periods, 1)).toHaveText("-¥0.011");
  await expect(cell(periods, 2)).toHaveText("¥10.00");
  await expect(cell(periods, 4)).toHaveText("¥9.989");

  const calls = region(page, "用量汇总").getByRole("row").filter({ hasText: "chat-ui" });
  await expect(cell(calls, 2)).toHaveText("1");
  await expect(cell(calls, 3)).toHaveText("¥0.011");
  await expect(cell(calls, 4)).toContainText("输入 5");

  const quotas = region(page, "额度").getByRole("row").filter({ hasText: "平台" });
  await expect(cell(quotas, 0)).toHaveText("组织");
  await expect(cell(quotas, 4)).toHaveText("每月");
  await expect(cell(quotas, 5)).toHaveText("¥2.00");
  await expect(cell(quotas, 8)).toHaveText("正常");
  await expect(region(page, "额度").getByRole("button")).toHaveCount(0);
});

test("只读运维看不到修改额度", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "postpaid", creditLimitCny: "0.05" });
  const viewer = { email: "viewer-accounts@e2e.local", password: "viewer-accounts-pass-4k9" };
  await stack.admin.post("/admin/operators", { ...viewer, role: "viewer" });
  await login(page, viewer.email, viewer.password);
  await accountRow(page, world).getByRole("button", { name: world.tenantName }).click();
  const panel = page.getByRole("dialog");
  await expect(region(page, "信用额度")).toContainText("¥0.05");
  await expect(panel.getByRole("button", { name: "保存" })).toHaveCount(0);
  await expect(panel.getByLabel("信用额度（元）")).toHaveCount(0);
});

test("按期汇总：后端会拒绝的日期范围在本地说明原因，不发出请求", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "prepaid", creditCny: "0" });
  await login(page);
  await accountRow(page, world).getByRole("button", { name: world.tenantName }).click();
  const periods = region(page, "按期汇总");
  await expect(periods.getByRole("alert")).toHaveCount(0);

  // 按天 over 92 days is refused by the backend, so it is never asked.
  let asked = 0;
  page.on("request", (r) => {
    if (r.url().includes("/summary?")) asked += 1;
  });
  await periods.getByLabel("开始日期（UTC）").fill("2020-01-01");
  await expect(periods.getByText("按天最多汇总 92 天，按月最多 731 天，请缩短日期范围。")).toBeVisible();
  await expect(periods.getByRole("button", { name: "重试" })).toHaveCount(0);
  await expect(periods.getByText("提交的内容未通过校验")).toHaveCount(0);

  // A start date after today, with no end date.
  const tomorrow = new Date(Date.now() + 2 * 86_400_000).toISOString().slice(0, 10);
  await periods.getByLabel("开始日期（UTC）").fill(tomorrow);
  await expect(periods.getByText("开始日期不能晚于今天")).toBeVisible();
  await expect(periods.getByRole("button", { name: "重试" })).toHaveCount(0);
  expect(asked).toBe(0);
});

test("后付租户没有信用额度但余额为正：不说所有调用都会被拒绝，提示余额用尽即停止，网关照常放行", async ({ page }) => {
  const world = await seedTenant(stack, { mode: "postpaid", creditCny: "10.00" });
  await login(page);
  await expect(cell(accountRow(page, world), 5)).toHaveText("正常");
  await accountRow(page, world).getByRole("button", { name: world.tenantName }).click();
  const limit = region(page, "信用额度");
  await expect(limit.getByText("尚无信用额度：余额用尽即停止调用")).toBeVisible();
  await expect(limit.getByText("后付租户尚未设置信用额度，所有调用都会被拒绝")).toHaveCount(0);
  await expect(limit.getByRole("status")).toHaveCount(0);
  // Both modes spend balance plus credit: the call is admitted.
  expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
});

/**
 * A 加载更多 still on its way when the filter changes belongs to the old
 * filter: its page must not be appended to the new list, nor its cursor
 * replace the new one. The old continuation is held at the network until
 * the new filter's list is shown, then let through with its real answer.
 * The new filter, 全部, has a next page of its own, so a cursor the old
 * answer replaced shows as a 加载更多 that loads the wrong page or is gone.
 */
test("余额流水加载更多途中换了类型：旧类型的下一页不混进新类型的列表，新类型照常翻页", async ({ page }) => {
  test.setTimeout(120_000);
  // One funding top-up and 50 more: two pages of 充值 (a page is 50).
  const world = await seedTenant(stack, { mode: "prepaid", creditCny: "10.00" });
  for (let i = 0; i < 50; i++) {
    await stack.admin.post(`/admin/tenants/${world.tenantId}/credits`, { amount_cny: "0.01", reason: `翻页-${i}` });
  }
  // And two 扣费.
  for (let i = 0; i < 2; i++) expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await expect
    .poll(async () => (await stack.admin.get(`/admin/tenants/${world.tenantId}/billing/ledger?kind=usage`)).items.length, { timeout: 30_000 })
    .toBe(2);

  await login(page);
  await accountRow(page, world).getByRole("button", { name: world.tenantName }).click();
  const ledger = region(page, "余额流水");
  const rows = ledger.locator("tbody tr");
  const kinds = () => rows.locator("td:nth-child(2)").allTextContents();
  await ledger.getByLabel("类型").selectOption({ label: "充值" });
  await expect(rows).toHaveCount(50);

  const NEXT_PAGE = /\/billing\/ledger\?.*before=/;
  let release!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  let reached!: () => void;
  const arrived = new Promise<void>((resolve) => (reached = resolve));
  await page.route(NEXT_PAGE, async (route) => {
    reached();
    await held;
    await route.continue();
  });
  await ledger.getByRole("button", { name: "加载更多" }).click();
  await arrived;

  // 全部: 51 top-ups and 2 扣费, a first page of 50 and a next one of 3.
  await ledger.getByLabel("类型").selectOption({ label: "全部" });
  await expect(rows).toHaveCount(50);
  const answered = page.waitForResponse(NEXT_PAGE);
  release();
  await answered;
  await page.unroute(NEXT_PAGE);
  // Give the page a moment to take (and drop) the old answer.
  await page.waitForTimeout(500);
  await expect(rows).toHaveCount(50);
  // The new list pages on with its own cursor: its next page, and nothing of the old filter's.
  await ledger.getByRole("button", { name: "加载更多" }).click();
  await expect(rows).toHaveCount(53);
  const all = await kinds();
  expect(all.filter((k) => k === "扣费")).toHaveLength(2);
  expect(all.filter((k) => k === "充值")).toHaveLength(51);
  await expect(ledger.getByRole("button", { name: "加载更多" })).toHaveCount(0);
});
