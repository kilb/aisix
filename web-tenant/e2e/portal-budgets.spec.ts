import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type ApiClient, type Stack } from "./stack";
import { chat, seedBillingWorld, waitClearOfUtcMidnight, type BillingWorld, type Login } from "./world";

/**
 * 预算: the organisation's, members' and keys' quotas, against the real
 * control plane and gateway. Quotas are set through the page or the real
 * `/api` and `/admin`; spend comes from real calls through the gateway in
 * front of the upstream stand-in.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

/** How long a quota's effect may take to reach the gateway. */
const REACH_MS = 15_000;

const region = (page: Page, name: string): Locator => page.getByRole("region", { name, exact: true });
const table = (page: Page, name: string): Locator => page.getByRole("table", { name, exact: true });
const rowOf = (t: Locator, text: string | RegExp): Locator => t.locator("tbody tr").filter({ hasText: text });

async function login(page: Page, who: Login): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("banner")).toBeVisible();
}

async function openBudgets(page: Page, who: Login): Promise<void> {
  await login(page, who);
  await page.getByRole("tab", { name: "预算" }).click();
  await expect(region(page, "成员额度")).toBeVisible();
}

async function apiAs(who: Login): Promise<ApiClient> {
  const api = stack.tenant();
  await api.post("/api/login", who);
  return api;
}

/** Waits until the gateway knows `plaintext`, without spending anything. */
async function admits(plaintext: string): Promise<void> {
  await expect
    .poll(
      async () => {
        const res = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${plaintext}` } });
        await res.arrayBuffer();
        return res.status;
      },
      { message: "the gateway knows the key" },
    )
    .toBe(200);
}

interface Key {
  id: string;
  label: string;
  plaintext: string;
}

async function mint(who: Login, label: string): Promise<Key> {
  const created = await (await apiAs(who)).post("/api/keys", { label });
  await admits(created.plaintext);
  return { id: created.key.id, label, plaintext: created.plaintext };
}

interface World {
  world: BillingWorld;
  /** The owner's keys: K1 is the world's own, K2 a second one. */
  k1: Key;
  k2: Key;
  /** Member M and its own key KM. */
  m: Login & { id: string };
  km: Key;
}

/** An organisation on a 1.00 credit: owner keys K1 and K2, and member M with its key KM. */
async function seed(extra: ("member" | "billing")[] = ["member"]): Promise<World> {
  const world = await seedBillingWorld(stack, { creditUsd: "1.00", members: extra });
  const k1: Key = { id: world.key.id, label: "world-owner-key", plaintext: world.key.plaintext };
  const k2 = await mint(world.owner, "owner-key-two");
  const m = world.members[0]!;
  // A billing member holds no keys; the world then has none for it.
  const km = extra[0] === "member" ? await mint(m, "m-key-one") : k2;
  return { world, k1, k2, m, km };
}

/** The owner's quota on `scope_ref`, set through the real API. */
async function setQuota(
  who: Login,
  body: { scope: "member" | "api_key"; scope_ref: string; kind?: string; period?: string; limit_usd: string },
) {
  return (await apiAs(who)).post("/api/budgets", body);
}

/** Fills 新建额度 as the page offers it and submits. */
async function createViaPage(
  page: Page,
  q: { scope: "成员" | "密钥"; target: string; kind?: "periodic" | "cumulative"; period?: string; limit: string },
): Promise<void> {
  const form = region(page, "新建额度");
  await form.getByLabel(/^对象类型/).selectOption({ label: q.scope });
  await form.getByLabel(/^对象(?!类型)/).selectOption(q.target);
  if (q.kind) await form.getByLabel(/^类型/).selectOption(q.kind);
  if (q.period) await form.getByLabel(/^周期/).selectOption(q.period);
  await form.getByLabel("限额（美元）").fill(q.limit);
  await form.getByRole("button", { name: "创建" }).click();
}

/** Calls through `key` until the gateway has taken `n` of them. */
async function spend(key: Key, world: BillingWorld, n: number): Promise<void> {
  for (let i = 0; i < n; i++) {
    await expect.poll(() => chat(stack, key.plaintext, world.model.name), { message: "the gateway admits the call" }).toBe(200);
  }
}

/** Waits until the control plane has booked `micro` of spend on the owner's view of quota `id`. */
async function spentIs(who: Login, id: string, micro: number): Promise<void> {
  await expect
    .poll(async () => ((await (await apiAs(who)).get("/api/budgets")).items as { id: string; spent_micro_usd: number }[]).find((b) => b.id === id)?.spent_micro_usd, {
      message: "the spend is booked",
      timeout: REACH_MS,
    })
    .toBe(micro);
}

// A fixed cadence bounds what the polling calls themselves can spend before the refusal lands.
const refused = (key: Key, world: BillingWorld) =>
  expect.poll(() => chat(stack, key.plaintext, world.model.name), {
    message: "the gateway refuses the call",
    timeout: REACH_MS,
    intervals: [1_000],
  });

test("为密钥设每日额度，用尽后该密钥被拒，另一把密钥照常", async ({ page }) => {
  await waitClearOfUtcMidnight();
  const s = await seed();
  await openBudgets(page, s.world.owner);
  await createViaPage(page, { scope: "密钥", target: s.k1.id, period: "day", limit: "0.02" });
  const keys = table(page, "密钥额度");
  await expect(rowOf(keys, "world-owner-key")).toContainText("每天");

  await spend(s.k1, s.world, 2);
  const listed = (await (await apiAs(s.world.owner)).get("/api/budgets")).items as { id: string; target: { id: string } }[];
  const quotaId = listed.find((b) => b.target.id === s.k1.id)!.id;
  await spentIs(s.world.owner, quotaId, 22_000);

  await page.getByRole("button", { name: "刷新" }).click();
  const row = rowOf(keys, "world-owner-key");
  await expect(row).toContainText("$0.022");
  await expect(row.locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
  await expect(row.locator("[data-state]")).toHaveText("已用尽");

  await refused(s.k1, s.world).toBe(429);
  expect(await chat(stack, s.k2.plaintext, s.world.model.name)).toBe(200);
});

test("一次性额度用尽后被拒，提高限额后恢复", async ({ page }) => {
  await waitClearOfUtcMidnight();
  const s = await seed();
  await openBudgets(page, s.world.owner);
  await createViaPage(page, { scope: "密钥", target: s.k1.id, kind: "cumulative", limit: "0.01" });
  const keys = table(page, "密钥额度");
  const today = new Date().toISOString().slice(0, 10);
  await expect(rowOf(keys, "world-owner-key")).toContainText(`自 ${today} 起`);
  await expect(rowOf(keys, "world-owner-key")).toContainText("一次性额度");

  await spend(s.k1, s.world, 1);
  await refused(s.k1, s.world).toBe(429);

  await page.getByRole("button", { name: "刷新" }).click();
  const row = rowOf(keys, "world-owner-key");
  await expect(row.locator("[data-state]")).toHaveAttribute("data-state", "exhausted");
  await row.getByRole("button", { name: "修改限额" }).click();
  await row.getByLabel("新的限额（美元）").fill("1.00");
  await row.getByRole("button", { name: "保存" }).click();
  await expect(row.locator("[data-state]")).toHaveAttribute("data-state", "ok");
  await expect(row.locator("[data-state]")).toHaveText("正常");
  await expect.poll(() => chat(stack, s.k1.plaintext, s.world.model.name), { timeout: REACH_MS }).toBe(200);
});

test("成员额度覆盖该成员的全部密钥", async ({ page }) => {
  const s = await seed();
  const km2 = await mint(s.m, "m-key-two");
  await openBudgets(page, s.world.owner);
  await createViaPage(page, { scope: "成员", target: s.m.id, limit: "0.02" });
  await expect(rowOf(table(page, "成员额度"), "每月")).toHaveCount(1);

  await spend(s.km, s.world, 2);
  await refused(s.km, s.world).toBe(429);
  await refused(km2, s.world).toBe(429);
  // The owner's own keys are not the member's.
  expect(await chat(stack, s.k2.plaintext, s.world.model.name)).toBe(200);
});

test("成员只能给自己的密钥设额度，看不到组织额度", async ({ page }) => {
  const s = await seed();
  await stack.admin.post(`/admin/tenants/${s.world.tenantId}/budgets`, { kind: "periodic", period: "month", limit_usd: "5.00" });
  await setQuota(s.world.owner, { scope: "api_key", scope_ref: s.km.id, kind: "periodic", period: "month", limit_usd: "0.02" });

  await openBudgets(page, s.m);
  await expect(page.getByRole("region", { name: /组织额度/ })).toHaveCount(0);
  await expect(page.locator("main")).not.toContainText("余额");

  const form = region(page, "新建额度");
  await expect(form.getByLabel(/^对象类型/).locator("option")).toHaveText(["密钥"]);
  const targets = form.getByLabel(/^对象(?!类型)/).locator("option");
  await expect(targets).toHaveCount(1);
  await expect(targets.first()).toHaveText("m-key-one");

  await createViaPage(page, { scope: "密钥", target: s.km.id, period: "month", limit: "0.01" });
  const keys = table(page, "密钥额度");
  await expect(rowOf(keys, "成员")).toBeVisible();
  const mine = keys.locator("tbody tr").filter({ has: page.getByText("成员", { exact: true }) });
  await expect(mine.getByRole("button", { name: "修改限额" })).toHaveCount(1);
  const theirs = rowOf(keys, "组织管理员");
  await expect(theirs).toHaveCount(1);
  await expect(theirs.getByRole("button")).toHaveCount(0);
});

test("超过上级额度时给出提示，但仍能保存", async ({ page }) => {
  const s = await seed();
  await openBudgets(page, s.world.owner);
  await createViaPage(page, { scope: "成员", target: s.m.id, limit: "0.01" });
  await expect(rowOf(table(page, "成员额度"), "每月")).toHaveCount(1);

  const form = region(page, "新建额度");
  await form.getByLabel(/^对象类型/).selectOption({ label: "密钥" });
  await form.getByLabel(/^对象(?!类型)/).selectOption(s.km.id);
  const warning = form.getByText("超过上级额度时以先用尽者为准");
  // Equal to the member's monthly 0.01 is not above it.
  await form.getByLabel("限额（美元）").fill("0.01");
  await expect(warning).toHaveCount(0);
  // A daily cap is not compared with a monthly one.
  await form.getByLabel(/^周期/).selectOption("day");
  await form.getByLabel("限额（美元）").fill("5.00");
  await expect(warning).toHaveCount(0);
  await form.getByLabel(/^周期/).selectOption("month");
  await expect(warning).toBeVisible();
  await form.getByRole("button", { name: "创建" }).click();

  const row = rowOf(table(page, "密钥额度"), "m-key-one");
  await expect(row).toHaveCount(1);
  await expect(row).toContainText("超过上级额度时以先用尽者为准");
  await expect(row).toContainText("$5.00");
});

test("实际可用显示最小的一级，并写明受限于哪一级", async ({ page }) => {
  const s = await seed();
  await setQuota(s.world.owner, { scope: "api_key", scope_ref: s.km.id, kind: "cumulative", limit_usd: "0.05" });
  await spend(s.km, s.world, 1);
  // The call is booked a moment after it is answered; the page reads once on load.
  await expect
    .poll(
      async () => ((await (await apiAs(s.world.owner)).get(`/api/keys/${s.km.id}/quota`)) as { effective_remaining_micro_usd: number }).effective_remaining_micro_usd,
      { message: "the spend is booked", timeout: REACH_MS },
    )
    .toBe(39_000);

  await openBudgets(page, s.world.owner);
  const row = rowOf(table(page, "密钥实际可用"), "m-key-one");
  await expect(row).toContainText("$0.039");
  await expect(row).toContainText("密钥");
  // A key no quota caps is bound by the organisation's balance alone.
  await expect(rowOf(table(page, "密钥实际可用"), "owner-key-two")).toContainText("组织");
});

test("删除额度需要二次确认，删除后不再拦截", async ({ page }) => {
  const s = await seed();
  await setQuota(s.world.owner, { scope: "api_key", scope_ref: s.k1.id, kind: "cumulative", limit_usd: "0.01" });
  await spend(s.k1, s.world, 1);
  await refused(s.k1, s.world).toBe(429);

  await openBudgets(page, s.world.owner);
  const row = rowOf(table(page, "密钥额度"), "world-owner-key");
  await row.getByRole("button", { name: "删除", exact: true }).click();
  await expect(row.getByRole("button", { name: "确认删除" })).toBeVisible();
  await expect(row.getByText("删除后不再按这个额度拦截")).toBeVisible();
  await row.getByRole("button", { name: "确认删除" }).click();
  await expect(rowOf(table(page, "密钥额度"), "world-owner-key")).toHaveCount(0);
  await expect.poll(() => chat(stack, s.k1.plaintext, s.world.model.name), { timeout: REACH_MS }).toBe(200);
});

test("同一对象同一周期不能建两个额度，提示已存在", async ({ page }) => {
  const s = await seed();
  await openBudgets(page, s.world.owner);
  await createViaPage(page, { scope: "密钥", target: s.k1.id, period: "month", limit: "1.00" });
  await expect(rowOf(table(page, "密钥额度"), "world-owner-key")).toHaveCount(1);
  await createViaPage(page, { scope: "密钥", target: s.k1.id, period: "month", limit: "2.00" });
  await expect(region(page, "新建额度").getByRole("alert")).toHaveText("该对象在这个周期已有额度");
  await expect(rowOf(table(page, "密钥额度"), "world-owner-key")).toHaveCount(1);
  await expect(rowOf(table(page, "密钥额度"), "world-owner-key")).toContainText("$1.00");
});

test("账单角色只能查看额度，没有新建与修改", async ({ page }) => {
  const s = await seed(["billing"]);
  const billing = s.world.members[0]!;
  await stack.admin.post(`/admin/tenants/${s.world.tenantId}/budgets`, { kind: "periodic", period: "month", limit_usd: "5.00" });
  await setQuota(s.world.owner, { scope: "api_key", scope_ref: s.k1.id, kind: "periodic", period: "month", limit_usd: "0.50" });
  await setQuota(s.world.owner, { scope: "member", scope_ref: billing.id, kind: "periodic", period: "month", limit_usd: "0.70" });

  await openBudgets(page, billing);
  await expect(rowOf(table(page, "组织额度"), "每月")).toHaveCount(1);
  await expect(rowOf(table(page, "成员额度"), "每月")).toHaveCount(1);
  await expect(rowOf(table(page, "密钥额度"), "world-owner-key")).toHaveCount(1);
  await expect(page.getByRole("region", { name: "新建额度" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "修改限额" })).toHaveCount(0);
  await expect(page.getByRole("button", { name: "删除", exact: true })).toHaveCount(0);
});

test("组织额度对所有者只读", async ({ page }) => {
  const s = await seed();
  await stack.admin.post(`/admin/tenants/${s.world.tenantId}/budgets`, { kind: "periodic", period: "month", limit_usd: "5.00" });
  await openBudgets(page, s.world.owner);
  const org = rowOf(table(page, "组织额度"), "每月");
  await expect(org).toHaveCount(1);
  await expect(org).toContainText("平台");
  await expect(org.getByRole("button")).toHaveCount(0);
});

test("管理员可以给任何密钥设额度，并修改、删除自己设的额度", async ({ page }) => {
  const s = await seed();
  await (await apiAs(s.world.owner)).patch(`/api/members/${s.m.id}`, { role: "admin" });
  await openBudgets(page, s.m);
  const form = region(page, "新建额度");
  await expect(form.getByLabel(/^对象类型/).locator("option")).toHaveText(["成员", "密钥"]);
  await createViaPage(page, { scope: "密钥", target: s.k1.id, period: "month", limit: "0.50" });
  const row = rowOf(table(page, "密钥额度"), "world-owner-key");
  await expect(row).toContainText("组织管理员");
  await row.getByRole("button", { name: "修改限额" }).click();
  await row.getByLabel("新的限额（美元）").fill("0.60");
  await row.getByRole("button", { name: "保存" }).click();
  await expect(row).toContainText("$0.60");
  await row.getByRole("button", { name: "删除", exact: true }).click();
  await row.getByRole("button", { name: "确认删除" }).click();
  await expect(rowOf(table(page, "密钥额度"), "world-owner-key")).toHaveCount(0);
});

test("成员可以修改自己为自己密钥设的额度", async ({ page }) => {
  const s = await seed();
  await setQuota(s.m, { scope: "api_key", scope_ref: s.km.id, kind: "periodic", period: "month", limit_usd: "0.01" });
  await openBudgets(page, s.m);
  const row = rowOf(table(page, "密钥额度"), "m-key-one");
  await expect(row).toContainText("$0.01");
  await row.getByRole("button", { name: "修改限额" }).click();
  await row.getByLabel("新的限额（美元）").fill("0.03");
  await row.getByRole("button", { name: "保存" }).click();
  await expect(row).toContainText("$0.03");
  await expect(row.getByRole("button", { name: "修改限额" })).toBeVisible();
});

test("所有者可以删除成员为自己密钥设的额度，但不能修改", async ({ page }) => {
  const s = await seed();
  await setQuota(s.m, { scope: "api_key", scope_ref: s.km.id, kind: "periodic", period: "month", limit_usd: "0.01" });
  await openBudgets(page, s.world.owner);
  const row = rowOf(table(page, "密钥额度"), "m-key-one");
  await expect(row).toContainText("成员");
  await expect(row.getByRole("button", { name: "修改限额" })).toHaveCount(0);
  await row.getByRole("button", { name: "删除", exact: true }).click();
  await row.getByRole("button", { name: "确认删除" }).click();
  await expect(rowOf(table(page, "密钥额度"), "m-key-one")).toHaveCount(0);
});
