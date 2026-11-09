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
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("banner")).toBeVisible();
}

/** Signs in and opens 账户. */
async function openAccount(page: Page, who: Login): Promise<void> {
  await login(page, who);
  await page.getByRole("tab", { name: "账户" }).click();
  await expect(region(page, "余额")).toBeVisible();
}

const region = (page: Page, name: string): Locator => page.getByRole("region", { name, exact: true });

/** The body rows of a region's table, newest first as shown. */
const bodyRows = (r: Locator): Locator => r.locator("tbody tr");

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

function utcDay(offsetDays: number): string {
  return new Date(Date.now() + offsetDays * 86_400_000).toISOString().slice(0, 10);
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
    const form = region(page, "申请充值");
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
    await page.getByRole("tab", { name: "账户" }).click();
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
    await expect(rows.nth(0).locator("td").nth(4)).toHaveText("chat-ui");
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
    await page.getByRole("tab", { name: "账户" }).click();
    await expect(readout.getByTestId("readout-amount")).toHaveText("¥0.00");
    const key = readout.getByRole("listitem").filter({ hasText: "密钥" });
    await expect(key).toContainText("以此为准");
    await expect(readout.getByRole("listitem").filter({ hasText: "组织" })).not.toContainText("以此为准");
    await expect(readout.getByRole("listitem").filter({ hasText: "组织" })).toContainText("¥100.478");
  });

  test("按天汇总与时间筛选按 UTC 计算", async ({ browser }) => {
    await waitClearOfUtcMidnight();
    // A zone whose calendar date differs from UTC's at this very moment, at any hour:
    // +14 from 10:00 UTC on (already tomorrow), -11 before that (still yesterday).
    const timezoneId = new Date().getUTCHours() >= 10 ? "Pacific/Kiritimati" : "Pacific/Pago_Pago";
    const context = await browser.newContext({ timezoneId });
    const page = await context.newPage();
    try {
      await openAccount(page, world.owner);
      const today = utcDay(0);
      expect(await page.evaluate(() => new Date().toLocaleDateString("en-CA"))).not.toBe(today);
      const ledger = region(page, "余额流水");
      const summary = region(page, "按期汇总");
      await ledger.getByLabel("开始日期（UTC）").fill(today);
      await ledger.getByLabel("结束日期（UTC）").fill(today);
      await expect(summary.getByRole("combobox", { name: "汇总方式" })).toHaveValue("day");
      await expect(bodyRows(summary)).toHaveCount(1);
      const row = bodyRows(summary).first().locator("td");
      await expect(row.nth(0)).toHaveText(today);
      await expect(row.nth(1)).toHaveText("-¥0.022");
      await expect(row.nth(2)).toHaveText("¥100.50");
      await expect(row.nth(4)).toHaveText("¥100.478");
      await expect(bodyRows(ledger)).toHaveCount(3);

      // By month the same day is one period, labelled by its UTC month.
      await summary.getByLabel("汇总方式").selectOption("month");
      await expect(bodyRows(summary)).toHaveCount(1);
      await expect(bodyRows(summary).first().locator("td").nth(0)).toHaveText(today.slice(0, 7));
      await expect(bodyRows(summary).first().locator("td").nth(1)).toHaveText("-¥0.022");
      await summary.getByLabel("汇总方式").selectOption("day");

      // The 类型 filter narrows the ledger to one kind.
      await ledger.getByLabel("类型").selectOption("usage");
      await expect(bodyRows(ledger)).toHaveCount(2);
      await expect(bodyRows(ledger).locator("td:nth-child(2)")).toHaveText(["扣费", "扣费"]);
      await ledger.getByLabel("类型").selectOption("topup");
      await expect(bodyRows(ledger)).toHaveCount(1);
      await expect(bodyRows(ledger).first().locator("td").nth(1)).toHaveText("充值");
      await ledger.getByLabel("类型").selectOption("");
      await expect(bodyRows(ledger)).toHaveCount(3);

      // An end before the start names nothing, and the field says so.
      await ledger.getByLabel("结束日期（UTC）").fill(utcDay(-1));
      await expect(ledger.getByText("结束日期不能早于开始日期")).toBeVisible();
      await expect(bodyRows(ledger)).toHaveCount(0);
      await expect(bodyRows(summary)).toHaveCount(0);

      // Yesterday, as a whole range of its own, holds nothing.
      await ledger.getByLabel("开始日期（UTC）").fill(utcDay(-1));
      await expect(ledger.getByText("所选范围内没有流水")).toBeVisible();
      await expect(summary.getByText("所选范围内没有汇总")).toBeVisible();
      await expect(bodyRows(ledger)).toHaveCount(0);
      await expect(bodyRows(summary)).toHaveCount(0);
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
  await expect(bodyRows(region(page, "余额流水")).first().locator("td").nth(4)).toHaveText("平台入账");
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
  await expect(balance.locator(".entry").filter({ hasText: "信用额度" })).toContainText("¥0.02");
  // Before any call the whole credit line is open, and the organisation binds.
  await expect(region(page, "可用额度").getByTestId("readout-amount")).toHaveText("¥0.02");
  await expect(region(page, "可用额度").getByRole("listitem").filter({ hasText: "组织" })).toContainText("以此为准");
  await paidChat(world);
  await paidChat(world);
  await expect.poll(() => balanceOf(world)).toBe(-2 * CALL_MICRO_CNY);
  await page.reload();
  await page.getByRole("tab", { name: "账户" }).click();
  await expect(balance.locator(".entry").filter({ hasText: "可用额度" })).toContainText("-¥0.002");
  await expect(balance.locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
  await expect(balance).toContainText("已达信用额度，调用已停止");
});

test("金额格式不对时不发送请求并提示", async ({ page }) => {
  const world = await seedBillingWorld(stack, { creditCny: "0" });
  await openAccount(page, world.owner);
  const form = region(page, "申请充值");
  await form.getByLabel("充值金额（元）").fill("0.001");
  await form.getByRole("button", { name: "提交申请" }).click();
  await expect(form.getByText("请输入金额，最多两位小数")).toBeVisible();
  await expect(form.getByLabel("充值金额（元）")).toBeFocused();
  await page.reload();
  await page.getByRole("tab", { name: "账户" }).click();
  await expect(region(page, "充值记录").getByText("还没有充值记录")).toBeVisible();
  await expect(bodyRows(region(page, "充值记录"))).toHaveCount(0);
  const api = await ownerApi(world);
  expect((await api.get("/api/billing/topups")).items).toEqual([]);
});

test("成员看不到账户页；账单角色能看到余额并提交充值，但没有密钥页", async ({ page }) => {
  const world = await seedBillingWorld(stack, { creditCny: "0", members: ["member", "billing"] });
  const [plain, billing] = world.members;
  if (!plain || !billing) throw new Error("the world has both members");

  await login(page, plain);
  await expect(page.getByRole("tab", { name: "账户" })).toHaveCount(0);
  await page.getByRole("button", { name: "登出" }).click();

  await openAccount(page, billing);
  await expect(page.getByRole("tab", { name: "密钥" })).toHaveCount(0);
  await expect(region(page, "余额")).toContainText("¥0.00");
  const readout = region(page, "可用额度");
  await expect(readout.getByRole("listitem")).toHaveCount(1);
  await expect(readout.getByRole("listitem")).toContainText("组织");
  await expect(readout.getByLabel("密钥")).toHaveCount(0);
  await expect(readout.getByTestId("readout-amount")).toHaveText("¥0.00");

  const form = region(page, "申请充值");
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
  await page.getByRole("button", { name: "登出" }).click();

  // The owner reads the same lamp.
  await openAccount(page, world.owner);
  await expect(region(page, "余额").locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
  await expect(region(page, "余额")).toContainText("组织额度已用尽，调用已停止");
});

test("流水分页：加载更多既不重复也不遗漏", async ({ page }) => {
  test.setTimeout(180_000);
  const world = await seedBillingWorld(stack, { creditCny: "1.00" });
  await paidChat(world);
  for (let i = 1; i < 50; i++) expect(await chat(stack, world.key.plaintext, world.model.name)).toBe(200);
  await expect.poll(() => balanceOf(world), { timeout: 60_000 }).toBe(1_000_000 - 50 * CALL_MICRO_CNY);

  await openAccount(page, world.owner);
  const ledger = region(page, "余额流水");
  await expect(bodyRows(ledger)).toHaveCount(50);
  await ledger.getByRole("button", { name: "加载更多" }).click();
  await expect(bodyRows(ledger)).toHaveCount(51);
  await expect(ledger.getByRole("button", { name: "加载更多" })).toHaveCount(0);

  // Each entry's balance is the one before it less a call: 51 distinct, in order, none missing.
  const balances = await bodyRows(ledger).locator("td:nth-child(4)").allTextContents();
  const expected = [1_000_000];
  for (let k = 1; k <= 50; k++) expected.unshift(1_000_000 - k * CALL_MICRO_CNY);
  expect(balances).toEqual(expected.map(cny));
  expect(new Set(balances).size).toBe(51);
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
  await expect(readout.getByLabel("密钥")).toHaveValue(second.key.id);
  await expect(readout.getByRole("listitem")).toHaveCount(3);
  await expect(readout.getByTestId("readout-amount")).toHaveText("¥0.30");
  await expect(readout.getByRole("listitem").filter({ hasText: "密钥" })).toContainText("以此为准");
  await expect(readout.getByRole("listitem").filter({ hasText: "成员" })).toContainText("¥0.50");

  // The first key has no quota of its own: the member's 0.50 binds.
  await readout.getByLabel("密钥").selectOption(world.key.id);
  await expect(readout.getByRole("listitem")).toHaveCount(2);
  await expect(readout.getByTestId("readout-amount")).toHaveText("¥0.50");
  await expect(readout.getByRole("listitem").filter({ hasText: "成员" })).toContainText("以此为准");
});
