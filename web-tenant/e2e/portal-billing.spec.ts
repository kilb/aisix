import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type ApiClient, type Stack } from "./stack";
import {
  CALL_MICRO_CNY,
  approveTopup,
  chat,
  rejectTopup,
  seedBillingWorld,
  waitClearOfUtcMidnight,
  type BillingWorld,
  type Login,
} from "./world";
import { chooseSelect } from "./select";

/**
 * 账户: the balance, the readout of what can still be spent, top-up requests
 * and the ledger, against the real control plane and gateway. Funds move
 * only through the real operator and organisation APIs and real calls.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

async function login(page: Page, who: Login): Promise<void> {
  await page.goto(`${stack.previewUrl}/login`);
  await page.getByLabel("邮箱").fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("banner")).toBeVisible();
}

function accountLink(page: Page): Locator {
  return page.getByRole("navigation", { name: "组织门户分区" }).getByRole("link", { name: "账务账户", exact: true });
}

async function signOut(page: Page): Promise<void> {
  await page.getByRole("banner").getByRole("button", { name: "账号设置", exact: true }).click();
  await page.getByRole("main").getByRole("button", { name: "登出", exact: true }).click();
  await expect(page.getByRole("form", { name: "登录", exact: true })).toBeVisible();
}

async function openAccount(page: Page, who: Login): Promise<void> {
  await login(page, who);
  await accountLink(page).click();
  await expect(region(page, "余额")).toBeVisible();
}

const region = (page: Page, name: string): Locator => page.getByRole("region", { name, exact: true });

async function openTopupRequest(page: Page): Promise<Locator> {
  await page.getByRole("button", { name: "申请充值", exact: true }).click();
  return page.getByRole("dialog", { name: "申请充值", exact: true });
}

/** The body rows of a region's table, newest first as shown. */
const bodyRows = (r: Locator): Locator => r.locator("tbody tr");

const nextLedgerPage = (ledger: Locator): Locator => ledger
  .getByRole("navigation", { name: "余额流水分页", exact: true })
  .getByRole("button", { name: "下一页", exact: true });

async function expectLedgerBalances(ledger: Locator, expected: readonly number[]): Promise<void> {
  const balances = bodyRows(ledger).locator("td:nth-child(4)");
  const next = nextLedgerPage(ledger);
  for (let offset = 0; offset < expected.length; offset += 25) {
    await expect(balances).toHaveText(expected.slice(offset, offset + 25).map(cny));
    if (offset + 25 < expected.length) await next.click();
  }
  await expect(next).toBeDisabled();
}

async function ownerApi(world: BillingWorld): Promise<ApiClient> {
  const api = stack.tenant();
  await api.post("/api/login", world.owner);
  return api;
}

async function balanceOf(world: BillingWorld): Promise<number> {
  return (await stack.admin.get(`/admin/tenants/${world.tenantId}/billing`)).balance_micro_cny;
}

/** One call that is admitted and charged, once the gateway has the balance. */
async function paidChat(world: BillingWorld): Promise<void> {
  await expect.poll(() => chat(stack, world.key.plaintext, world.model.name), { message: "the gateway admits the call" }).toBe(200);
}

/**
 * Yuan as the portal writes them, computed here rather than read off the
 * product: at least two decimals, more only where the amount needs them.
 * The figures in these tests carry none past the third, so no thousands
 * separator is reached.
 */
function cny(micro: number): string {
  const sign = micro < 0 ? "-" : "";
  const abs = Math.abs(micro);
  let frac = String(abs % 1_000_000).padStart(6, "0");
  while (frac.length > 2 && frac.endsWith("0")) frac = frac.slice(0, -1);
  return `${sign}¥${Math.floor(abs / 1_000_000)}.${frac}`;
}

function billingDay(offsetDays: number): string {
  return new Intl.DateTimeFormat("en-CA", { timeZone: "Asia/Shanghai" })
    .format(new Date(Date.now() + offsetDays * 86_400_000));
}

test("新开通的预付组织余额为零，状态写明调用已停止", async ({ page }) => {
  const world = await seedBillingWorld(stack, { creditCny: "0" });
  await openAccount(page, world.owner);
  const balance = region(page, "余额");
  await expect(balance).toContainText("¥0.00");
  await expect(balance.locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
  await expect(balance).toContainText("余额不足，调用已停止");
  await expect.poll(() => chat(stack, world.key.plaintext, world.model.name), { message: "the gateway refuses the call" }).toBe(429);
});

test.describe.serial("充值与扣费", () => {
  let world: BillingWorld;
  test.beforeAll(async () => {
    test.setTimeout(STACK_START_TIMEOUT_MS);
    await waitClearOfUtcMidnight();
    world = await seedBillingWorld(stack, { creditCny: "0" });
  });

  test("提交充值申请后显示待审批；运营批准后余额到账，流水多一笔充值", async ({ page }) => {
    await openAccount(page, world.owner);
    const form = await openTopupRequest(page);
    await form.getByLabel("充值金额（元）").fill("100.50");
    await form.getByLabel("付款凭证号").fill("wire-ui-7c1");
    await form.getByRole("button", { name: "提交申请" }).click();
    const topups = region(page, "充值记录");
    await expect(bodyRows(topups).first()).toContainText("¥100.50");
    await expect(bodyRows(topups).first()).toContainText("待审批");
    await expect(region(page, "余额")).toContainText("1 笔");

    const api = await ownerApi(world);
    const listed = await api.get("/api/billing/topups");
    await approveTopup(stack, listed.items[0].id);

    await page.reload();
    await accountLink(page).click();
    const balance = region(page, "余额");
    await expect(balance).toContainText("¥100.50");
    await expect(balance.locator("[data-state]")).toHaveAttribute("data-state", "ok");
    const first = bodyRows(region(page, "余额流水")).first();
    await expect(first.locator("td").nth(1)).toHaveText("充值");
    await expect(first.locator("td").nth(2)).toHaveText("¥100.50");
    await expect(first.locator("td").nth(3)).toHaveText("¥100.50");
    await expect(bodyRows(topups).first()).toContainText("已到账");
  });

  test("调用逐笔扣费，流水带每笔之后的余额", async ({ page }) => {
    await paidChat(world);
    await paidChat(world);
    await expect.poll(() => balanceOf(world)).toBe(100_500_000 - 2 * CALL_MICRO_CNY);
    await openAccount(page, world.owner);
    const rows = bodyRows(region(page, "余额流水"));
    await expect(rows.nth(0).locator("td").nth(2)).toHaveText("-¥0.011");
    await expect(rows.nth(0).locator("td").nth(3)).toHaveText("¥100.478");
    await expect(rows.nth(1).locator("td").nth(2)).toHaveText("-¥0.011");
    await expect(rows.nth(1).locator("td").nth(3)).toHaveText("¥100.489");
    await expect(rows.nth(0).getByRole("cell", { name: "chat-ui", exact: true })).toBeVisible();
  });

  test("可用额度显示各级剩余，并标出以哪一级为准", async ({ page }) => {
    await openAccount(page, world.owner);
    const readout = region(page, "可用额度");
    await expect(readout.getByTestId("readout-amount")).toHaveText("¥100.478");
    const org = readout.getByRole("listitem").filter({ hasText: "组织" });
    await expect(org).toContainText("以此为准");
    await expect(readout.getByRole("listitem")).toHaveCount(1);

    // The key is capped at a monthly 0.01, and this month's 0.022 already counts against it.
    const api = await ownerApi(world);
    await api.post("/api/budgets", { scope: "api_key", scope_ref: world.key.id, kind: "periodic", period: "month", limit_cny: "0.01" });
    await page.reload();
    await accountLink(page).click();
    await expect(readout.getByTestId("readout-amount")).toHaveText("¥0.00");
    const key = readout.getByRole("listitem").filter({ hasText: "密钥" });
    await expect(key).toContainText("以此为准");
    await expect(readout.getByRole("listitem").filter({ hasText: "组织" })).not.toContainText("以此为准");
    await expect(readout.getByRole("listitem").filter({ hasText: "组织" })).toContainText("¥100.478");
  });

  test("按天汇总与时间筛选按北京时间计算，与浏览器时区无关", async ({ browser }) => {
    await waitClearOfUtcMidnight();
    // 选择日历日期与北京时间不同的浏览器时区。
    const hour = new Date().getUTCHours();
    const timezoneId = hour >= 11 && hour < 16 ? "Pacific/Kiritimati" : "Pacific/Pago_Pago";
    const context = await browser.newContext({ timezoneId });
    const page = await context.newPage();
    try {
      await openAccount(page, world.owner);
      const today = billingDay(0);
      expect(await page.evaluate(() => new Date().toLocaleDateString("en-CA"))).not.toBe(today);
      const ledger = region(page, "余额流水");
      const summary = region(page, "按期汇总");
      const summaryRows = bodyRows(summary.getByRole("table", { name: "按期汇总", exact: true }));
      await expect(summary).toContainText("UTC+8");
      await ledger.getByLabel("开始日期", { exact: true }).fill(today);
      await ledger.getByLabel("结束日期", { exact: true }).fill(today);
      await expect(summary.getByRole("combobox", { name: "汇总方式" })).toHaveAttribute("data-value", "day");
      await expect(summaryRows).toHaveCount(1);
      const row = summaryRows.first().locator("td");
      await expect(row.nth(0)).toHaveText(today);
      await expect(row.nth(1)).toHaveText("-¥0.022");
      await expect(row.nth(2)).toHaveText("¥100.50");
      await expect(row.nth(4)).toHaveText("¥100.478");
      await expect(bodyRows(ledger)).toHaveCount(3);

      // 月汇总使用同一个北京时间月份。
      await chooseSelect(summary.getByLabel("汇总方式"), "month");
      await expect(summaryRows).toHaveCount(1);
      await expect(summaryRows.first().locator("td").nth(0)).toHaveText(today.slice(0, 7));
      await expect(summaryRows.first().locator("td").nth(1)).toHaveText("-¥0.022");
      await chooseSelect(summary.getByLabel("汇总方式"), "day");

      // The 类型 filter narrows the ledger to one kind.
      await chooseSelect(ledger.getByLabel("类型"), "usage");
      await expect(bodyRows(ledger)).toHaveCount(2);
      await expect(bodyRows(ledger).locator("td:nth-child(2)")).toHaveText(["扣费", "扣费"]);
      await chooseSelect(ledger.getByLabel("类型"), "topup");
      await expect(bodyRows(ledger)).toHaveCount(1);
      await expect(bodyRows(ledger).first().locator("td").nth(1)).toHaveText("充值");
      await chooseSelect(ledger.getByLabel("类型"), "");
      await expect(bodyRows(ledger)).toHaveCount(3);

      // An end before the start names nothing, and the field says so.
      await ledger.getByLabel("结束日期", { exact: true }).fill(billingDay(-1));
      await expect(ledger.getByText("结束日期不能早于开始日期")).toBeVisible();
      await expect(bodyRows(ledger)).toHaveCount(0);
      await expect(summaryRows).toHaveCount(0);

      // Yesterday, as a whole range of its own, holds nothing.
      await ledger.getByLabel("开始日期", { exact: true }).fill(billingDay(-1));
      await expect(ledger.getByText("所选范围内没有流水")).toBeVisible();
      await expect(summary.getByText("所选范围内没有汇总")).toBeVisible();
      await expect(bodyRows(ledger)).toHaveCount(0);
      await expect(summaryRows).toHaveCount(0);
    } finally {
      await context.close();
    }
  });
});

test("平台直接入账显示为平台入账并带原因", async ({ page }) => {
  const world = await seedBillingWorld(stack, { creditCny: "0" });
  await stack.admin.post(`/admin/tenants/${world.tenantId}/credits`, { amount_cny: "0.03", reason: "线下收款-ui-4b7" });
  await openAccount(page, world.owner);
  const row = bodyRows(region(page, "充值记录")).first().locator("td");
  await expect(row.nth(0)).toHaveText("¥0.03");
  await expect(row.nth(2)).toHaveText("平台入账");
  await expect(row.nth(3)).toHaveText("平台");
  await expect(row.nth(6)).toContainText("线下收款-ui-4b7");
  await expect(bodyRows(region(page, "余额流水")).first().getByRole("cell", { name: "平台入账", exact: true })).toBeVisible();
});

test("被驳回的申请显示驳回原因，余额不变", async ({ page }) => {
  const world = await seedBillingWorld(stack, { creditCny: "0" });
  const api = await ownerApi(world);
  const asked = await api.post("/api/billing/topups", { amount_cny: "0.10" });
  await rejectTopup(stack, asked.id, "未收到款项-4b7");
  await openAccount(page, world.owner);
  const row = bodyRows(region(page, "充值记录")).first();
  await expect(row).toContainText("已驳回");
  await expect(row).toContainText("未收到款项-4b7");
  await expect(region(page, "余额")).toContainText("¥0.00");
  await expect(region(page, "余额").locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
});

test("后付组织显示信用额度与可用额度，用尽后显示已达信用额度", async ({ page }) => {
  const world = await seedBillingWorld(stack, { mode: "postpaid", creditLimitCny: "0.02" });
  await openAccount(page, world.owner);
  const balance = region(page, "余额");
  await expect(balance).toContainText("信用额度");
  await expect(balance.getByText("信用额度", { exact: true }).locator("..")).toContainText("¥0.02");
  // Before any call the whole credit line is open, and the organisation binds.
  await expect(region(page, "可用额度").getByTestId("readout-amount")).toHaveText("¥0.02");
  await expect(region(page, "可用额度").getByRole("listitem").filter({ hasText: "组织" })).toContainText("以此为准");
  await paidChat(world);
  await paidChat(world);
  await expect.poll(() => balanceOf(world)).toBe(-2 * CALL_MICRO_CNY);
  await page.reload();
  await accountLink(page).click();
  await expect(balance.getByText("可用额度", { exact: true }).locator("..")).toContainText("-¥0.002");
  await expect(balance.locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
  await expect(balance).toContainText("已达信用额度，调用已停止");
});

test("金额格式不对时不发送请求并提示", async ({ page }) => {
  const world = await seedBillingWorld(stack, { creditCny: "0" });
  await openAccount(page, world.owner);
  const form = await openTopupRequest(page);
  await form.getByLabel("充值金额（元）").fill("0.001");
  await form.getByRole("button", { name: "提交申请" }).click();
  await expect(form.getByText("请输入金额，最多两位小数")).toBeVisible();
  await expect(form.getByLabel("充值金额（元）")).toBeFocused();
  await page.reload();
  await accountLink(page).click();
  await expect(region(page, "充值记录").getByText("所选范围内没有充值记录", { exact: true })).toBeVisible();
  await expect(bodyRows(region(page, "充值记录"))).toHaveCount(0);
  const api = await ownerApi(world);
  expect((await api.get("/api/billing/topups")).items).toEqual([]);
});

test("成员看不到账户页；账单角色能看到余额并提交充值，但没有密钥页", async ({ page }) => {
  const world = await seedBillingWorld(stack, { creditCny: "0", members: ["member", "billing"] });
  const [plain, billing] = world.members;
  if (!plain || !billing) throw new Error("the world has both members");

  await login(page, plain);
  await expect(accountLink(page)).toHaveCount(0);
  await signOut(page);

  await openAccount(page, billing);
  await expect(page.getByRole("navigation", { name: "组织门户分区" }).getByRole("link", { name: "API 密钥", exact: true })).toHaveCount(0);
  await expect(region(page, "余额")).toContainText("¥0.00");
  const readout = region(page, "可用额度");
  await expect(readout.getByRole("listitem")).toHaveCount(1);
  await expect(readout.getByRole("listitem")).toContainText("组织");
  await expect(readout.getByLabel("密钥")).toHaveCount(0);
  await expect(readout.getByTestId("readout-amount")).toHaveText("¥0.00");

  const form = await openTopupRequest(page);
  await form.getByLabel("充值金额（元）").fill("0.01");
  await form.getByRole("button", { name: "提交申请" }).click();
  await expect(bodyRows(region(page, "充值记录")).first()).toContainText("待审批");
});

test("组织额度未用尽时，账单角色的可用额度取余额与组织额度中较小的一项", async ({ page }) => {
  const world = await seedBillingWorld(stack, { members: ["billing"] }); // balance 10.00
  const [billing] = world.members;
  if (!billing) throw new Error("the world has a billing member");
  await stack.admin.post(`/admin/tenants/${world.tenantId}/budgets`, { kind: "periodic", period: "month", limit_cny: "5.00" });

  await openAccount(page, billing);
  await expect(region(page, "余额")).toContainText("¥10.00");
  await expect(region(page, "余额").locator("[data-state]")).toHaveAttribute("data-state", "ok");
  await expect(region(page, "可用额度").getByTestId("readout-amount")).toHaveText("¥5.00");
});

test("组织额度用尽而余额仍为正：状态灯写明调用已停止，账单角色的可用额度为零", async ({ page }) => {
  const world = await seedBillingWorld(stack, { members: ["billing"] }); // balance 10.00
  const [billing] = world.members;
  if (!billing) throw new Error("the world has a billing member");
  // A monthly 0.01 is spent by one call (0.011), long before the balance is.
  await stack.admin.post(`/admin/tenants/${world.tenantId}/budgets`, { kind: "periodic", period: "month", limit_cny: "0.01" });
  const api = await ownerApi(world);
  await paidChat(world);
  await expect.poll(async () => (await api.get("/api/budgets")).items.find((b: { scope: string }) => b.scope === "team")?.exhausted).toBe(true);
  expect(await balanceOf(world)).toBeGreaterThan(0);
  await expect.poll(() => chat(stack, world.key.plaintext, world.model.name), { message: "the gateway refuses the call" }).toBe(429);

  // The billing member, who holds no key: the readout and the lamp both say calls are stopped.
  await openAccount(page, billing);
  const balance = region(page, "余额");
  await expect(balance).toContainText("¥9.");
  await expect(balance.locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
  await expect(balance).toContainText("组织额度已用尽，调用已停止");
  await expect(region(page, "可用额度").getByTestId("readout-amount")).toHaveText("¥0.00");
  await signOut(page);

  // The owner reads the same lamp.
  await openAccount(page, world.owner);
  await expect(region(page, "余额").locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
  await expect(region(page, "余额")).toContainText("组织额度已用尽，调用已停止");
});

test("流水分页：逐页查看既不重复也不遗漏", async ({ page }) => {
  test.setTimeout(180_000);
  const world = await seedBillingWorld(stack, { creditCny: "1.00" });
  await paidChat(world);
  for (let i = 1; i < 50; i++) expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await expect.poll(() => balanceOf(world), { timeout: 60_000 }).toBe(1_000_000 - 50 * CALL_MICRO_CNY);

  await openAccount(page, world.owner);
  const ledger = region(page, "余额流水");

  // Each entry's balance is the one before it less a call: 51 distinct, in order, none missing.
  const expected = [1_000_000];
  for (let k = 1; k <= 50; k++) expected.unshift(1_000_000 - k * CALL_MICRO_CNY);
  await expectLedgerBalances(ledger, expected);
});

test("密钥选择切换可用额度，成员额度单独成一站", async ({ page }) => {
  const world = await seedBillingWorld(stack, { creditCny: "1.00" });
  const api = await ownerApi(world);
  const me = (await api.get("/api/session")).member;
  const second = await api.post("/api/keys", { label: "second-key-4b7" });
  await api.post("/api/budgets", { scope: "member", scope_ref: me.id, kind: "periodic", period: "month", limit_cny: "0.50" });
  await api.post("/api/budgets", { scope: "api_key", scope_ref: second.key.id, kind: "periodic", period: "month", limit_cny: "0.30" });

  await openAccount(page, world.owner);
  const readout = region(page, "可用额度");
  // The newest key of the viewer is read first: organisation, member and key all limit it.
  await expect(readout.getByLabel("密钥")).toHaveAttribute("data-value", second.key.id);
  await expect(readout.getByRole("listitem")).toHaveCount(3);
  await expect(readout.getByTestId("readout-amount")).toHaveText("¥0.30");
  await expect(readout.getByRole("listitem").filter({ hasText: "密钥" })).toContainText("以此为准");
  await expect(readout.getByRole("listitem").filter({ hasText: "成员" })).toContainText("¥0.50");

  // The first key has no quota of its own: the member's 0.50 binds.
  await chooseSelect(readout.getByLabel("密钥"), world.key.id);
  await expect(readout.getByRole("listitem")).toHaveCount(2);
  await expect(readout.getByTestId("readout-amount")).toHaveText("¥0.50");
  await expect(readout.getByRole("listitem").filter({ hasText: "成员" })).toContainText("以此为准");
});

/**
 * The organisation's quotas are a read of their own. When it fails, nothing
 * that depends on them may be claimed: not "可正常调用", and not the whole
 * balance as what a billing member can still spend while an exhausted quota
 * refuses every call. The read fails at the network for that one request;
 * nothing the control plane answers produces it on demand.
 */
test("组织额度读取失败时：不说可正常调用，也不把全部余额当作可用额度；重试后显示已停止", async ({ page }) => {
  const world = await seedBillingWorld(stack, { members: ["billing"] }); // balance 10.00
  const [billing] = world.members;
  if (!billing) throw new Error("the world has a billing member");
  await stack.admin.post(`/admin/tenants/${world.tenantId}/budgets`, { kind: "periodic", period: "month", limit_cny: "0.01" });
  const api = await ownerApi(world);
  await paidChat(world);
  await expect.poll(async () => (await api.get("/api/budgets")).items.find((b: { scope: string }) => b.scope === "team")?.exhausted).toBe(true);
  expect(await balanceOf(world)).toBeGreaterThan(0);

  const BUDGETS = "**/api/budgets";
  await page.route(BUDGETS, (route) => route.abort());
  await openAccount(page, billing);
  const balance = region(page, "余额");
  await expect(balance).toContainText("¥9.");
  await expect(balance.getByRole("alert")).toContainText("组织额度读取失败");
  await expect(balance.locator("[data-state]")).not.toHaveAttribute("data-state", "ok");
  await expect(balance).not.toContainText("可正常调用");
  const readout = region(page, "可用额度");
  await expect(readout.getByRole("alert")).toContainText("组织额度读取失败");
  await expect(readout.getByTestId("readout-amount")).toHaveCount(0);

  // The read recovers: stopped, and nothing left to spend.
  await page.unroute(BUDGETS);
  await balance.getByRole("button", { name: "重试" }).click();
  await expect(balance.locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
  await expect(balance).toContainText("组织额度已用尽，调用已停止");
  await expect(readout.getByTestId("readout-amount")).toHaveText("¥0.00");
});

/**
 * A new filter's first page is still on its way: the old filter's cursor must
 * not be offered for it, or 下一页 continues the new filter from the old
 * position and skips the entries in between. The new first page is held at
 * the network and then let through with its real answer.
 */
test("换筛选后首页未到时不能翻页；到了之后逐页翻完，每条只出现一次", async ({ page }) => {
  test.setTimeout(150_000);
  // Oldest first: the funding top-up, 50 more top-ups, then two calls. 充值
  // and 全部 then both run past two pages, at different positions.
  const world = await seedBillingWorld(stack, { creditCny: "1.00" });
  for (let i = 0; i < 50; i++) {
    await stack.admin.post(`/admin/tenants/${world.tenantId}/credits`, { amount_cny: "0.01", reason: `翻页-${i}` });
  }
  await paidChat(world);
  expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await expect.poll(() => balanceOf(world), { timeout: 60_000 }).toBe(1_500_000 - 2 * CALL_MICRO_CNY);

  await openAccount(page, world.owner);
  const ledger = region(page, "余额流水");
  const rows = bodyRows(ledger);
  await expect(rows).toHaveCount(25);
  await chooseSelect(ledger.getByLabel("类型"), { label: "充值" });
  await expect(rows.locator("td:nth-child(2)")).toHaveText(Array(25).fill("充值"));
  const next = nextLedgerPage(ledger);
  await expect(next).toBeEnabled();

  // Back to 全部, whose first page is held.
  const firstOfAll = (url: URL) =>
    url.pathname.endsWith("/api/billing/ledger") && !url.searchParams.has("kind") && !url.searchParams.has("before");
  let release!: () => void;
  const held = new Promise<void>((resolve) => (release = resolve));
  let reached!: () => void;
  const arrived = new Promise<void>((resolve) => (reached = resolve));
  await page.route(firstOfAll, async (route) => {
    reached();
    await held;
    await route.continue();
  });
  await chooseSelect(ledger.getByLabel("类型"), { label: "全部" });
  await arrived;
  try {
    await expect(next.and(ledger.locator("button:enabled"))).toHaveCount(0);
  } finally {
    release();
  }
  await expect(rows).toHaveCount(25);
  await page.unroute(firstOfAll);

  // Every entry once, in order: each balance is the one before it less what it moved.
  const expected: number[] = [];
  let after = 1_500_000 - 2 * CALL_MICRO_CNY;
  for (let k = 0; k < 2; k++) {
    expected.push(after);
    after += CALL_MICRO_CNY;
  }
  for (let k = 0; k < 50; k++) {
    expected.push(after);
    after -= 10_000;
  }
  expected.push(after);
  expect(after).toBe(1_000_000);
  await expectLedgerBalances(ledger, expected);
});
