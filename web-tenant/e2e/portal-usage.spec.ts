import { createServer } from "node:net";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type ApiClient, type Json, type Stack } from "./stack";
import { SHARED_DISPLAY_NAME, chat, memberKey, seedBillingWorld, waitClearOfUtcMidnight, type BillingWorld, type Login } from "./world";
import { chooseSelect } from "./select";

/**
 * The tenant portal's 用量 page: the summary and the call log, against the
 * real control plane and gateway; the only stand-in is the upstream. Every
 * test seeds its own organisation on a funded balance.
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
  await expect(page.getByRole("navigation", { name: "组织门户分区", exact: true })).toBeVisible();
}

function usageLink(page: Page, name: "调用记录" | "用量分析"): Locator {
  return page.getByRole("navigation", { name: "组织门户分区", exact: true }).getByRole("link", { name, exact: true });
}

async function openUsage(page: Page, who: Login): Promise<{ log: Locator; summary: Locator }> {
  await login(page, who);
  await usageLink(page, "调用记录").click();
  return {
    log: page.getByRole("region", { name: "调用记录", exact: true }),
    summary: page.getByRole("region", { name: "用量汇总", exact: true }),
  };
}

function logRows(log: Locator): Locator {
  return log.getByRole("table", { name: "调用记录", exact: true }).locator("tbody > tr");
}

function callCell(row: Locator, label: "模型 / 请求" | "状态" | "费用"): Locator {
  return row.getByRole("cell").and(row.locator(`[data-label="${label}"]`));
}

async function signedIn(who: Login): Promise<ApiClient> {
  const client = stack.tenant();
  await client.post("/api/login", who);
  return client;
}

/** Waits until the organisation's own call log lists at least `n` calls, none still waiting for a price unless `allowPending`. */
async function logged(who: Login, n: number, allowPending = false): Promise<Json[]> {
  const api = await signedIn(who);
  let items: Json[] = [];
  await expect
    .poll(
      async () => {
        items = (await api.get("/api/usage/calls?limit=200")).items;
        return items.length >= n && (allowPending || items.every((c: Json) => c.charge.status !== "pending"));
      },
      { message: `${n} priced calls reach the log`, timeout: 30_000 },
    )
    .toBe(true);
  return items;
}

/** Waits until the gateway lists `model` for the key. */
async function gatewayListsModel(plaintext: string, model: string): Promise<void> {
  await expect
    .poll(
      async () => {
        const res = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${plaintext}` } });
        const body = res.status === 200 ? ((await res.json()) as { data?: { id: string }[] }) : { data: [] };
        return (body.data ?? []).some((m) => m.id === model);
      },
      { message: `the gateway lists ${model}`, timeout: 60_000 },
    )
    .toBe(true);
}

async function keyAdmitted(plaintext: string): Promise<void> {
  await expect
    .poll(async () => (await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${plaintext}` } })).status, {
      message: "the new key reaches the gateway",
      timeout: 60_000,
    })
    .toBe(200);
}

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = createServer();
    srv.once("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const address = srv.address();
      const port = typeof address === "object" && address ? address.port : 0;
      srv.close(() => (port ? resolve(port) : reject(new Error("no port"))));
    });
  });
}

/** An upstream account the stand-in serves, and a model on `apiBase` instead when given. */
async function addModel(name: string, opts: { apiBase?: string } = {}): Promise<string> {
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: `acct-${name}`,
      provider: "openai",
      api_key: `sk-${name}-upstream`,
      api_base: opts.apiBase ?? `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: name, provider: "openai", model_name: `upstream-${name}`, provider_key_id: account.id },
  });
  return model.id as string;
}

/** Adds `modelId` to the organisation's grants. */
async function grant(world: BillingWorld, modelId: string): Promise<void> {
  const current = await stack.admin.get(`/admin/tenants/${world.tenantId}`);
  await stack.admin.patch(
    `/admin/tenants/${world.tenantId}`,
    { allowed_model_ids: [...current.allowed_model_ids, modelId] },
    current.version,
  );
}

/** A model of its own with the standard price, granted to `world` and listed by the gateway. */
async function pricedModel(world: BillingWorld, name: string): Promise<string> {
  const id = await addModel(name);
  const startsAt = Date.now() + 3_000;
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: id,
    effective_from: new Date(startsAt).toISOString(),
    card: { default: { base: { input: "1000", output: "2000" } } },
    note: "the usage page's card",
  });
  await grant(world, id);
  await gatewayListsModel(world.key.plaintext, name);
  await expect.poll(() => Date.now() > startsAt + 500).toBe(true);
  return id;
}

/** The names a tenant must never read, anywhere in the page or in what the API sent it. */
const CHANNEL = ["acct-ui-7c1", "grp-ui-5d2", "upstream-mdl-ui-9f3", "sk-acct-ui-7c1-upstream", "ECONNREFUSED", "127.0.0.1"];

test("调用记录逐次列出模型、密钥、状态与费用，抽屉内看到计费明细", async ({ page }) => {
  const world = await seedBillingWorld(stack);
  expect(await chat(stack, world.key.plaintext, "chat-ui")).toBe(200);
  await logged(world.owner, 1);
  const { log } = await openUsage(page, world.owner);

  const rows = logRows(log);
  await expect(rows).toHaveCount(1);
  const row = rows.first();
  const identity = callCell(row, "模型 / 请求");
  await expect(identity.getByText("chat-ui", { exact: true })).toBeVisible();
  await expect(identity).toContainText("密钥 world-owner-key");
  const status = callCell(row, "状态");
  await expect(status.getByText("成功", { exact: true })).toBeVisible();
  await expect(status.getByText(`分组 ${SHARED_DISPLAY_NAME}`, { exact: true })).toBeVisible();
  await expect(callCell(row, "费用")).toHaveText("¥0.011");

  await rows.first().getByRole("button", { name: "查看调用详情", exact: true }).click();
  const drawer = page.getByRole("dialog", { name: "调用详情", exact: true });
  const detail = drawer.getByRole("table", { name: "计费明细" });
  await expect(detail.locator("tbody > tr")).toHaveCount(2);
  await expect(detail.getByRole("row").filter({ hasText: "输入" }).getByRole("cell")).toHaveText(["输入", "5", "¥1,000.00", "¥1,000.00"]);
  await expect(detail.getByRole("row").filter({ hasText: "输出" }).getByRole("cell")).toHaveText(["输出", "3", "¥2,000.00", "¥2,000.00"]);
  // No discount, so nothing says one was taken.
  await expect(drawer.getByText(/组织折扣/)).toHaveCount(0);
});

test("有组织折扣时，计费明细的单价标明为折扣前，并说明这次按组织折扣减免了多少", async ({ page }) => {
  const world = await seedBillingWorld(stack);
  // 10 % off, from a moment ago: the call below is priced with it.
  await stack.admin.post("/admin/pricing/discounts", {
    tenant_id: world.tenantId,
    effective_from: new Date(Date.now() - 4_000).toISOString(),
    discount_bps: 1_000,
  });
  expect(await chat(stack, world.key.plaintext, "chat-ui")).toBe(200);
  await logged(world.owner, 1);
  const { log } = await openUsage(page, world.owner);

  const rows = logRows(log);
  await expect(rows).toHaveCount(1);
  // 5 input tokens at ¥1,000/M and 3 output at ¥2,000/M is ¥0.011 before
  // the discount; 10 % off leaves ¥0.0099.
  await expect(callCell(rows.first(), "费用")).toHaveText("¥0.0099");
  await rows.first().getByRole("button", { name: "查看调用详情", exact: true }).click();
  const drawer = page.getByRole("dialog", { name: "调用详情", exact: true });
  const detail = drawer.getByRole("table", { name: "计费明细" });
  // The unit prices are the group's, before the discount, and say so.
  await expect(detail.getByRole("columnheader")).toHaveText(["计量", "数量", "折扣前单价（元/百万）", "折后单价（元/百万）"]);
  await expect(detail.getByRole("row").filter({ hasText: "输入" }).getByRole("cell")).toHaveText(["输入", "5", "¥1,000.00", "¥900.00"]);
  await expect(detail.getByRole("row").filter({ hasText: "输出" }).getByRole("cell")).toHaveText(["输出", "3", "¥2,000.00", "¥1,800.00"]);
  await expect(drawer.getByText("本次按组织折扣计费：在折扣前的价格上减免 10%。")).toBeVisible();
});

test("调用记录与汇总里没有任何上游渠道、账号、分组或上游模型名", async ({ page }) => {
  const world = await seedBillingWorld(stack);
  expect(await chat(stack, world.key.plaintext, "chat-ui")).toBe(200);
  await logged(world.owner, 1);
  const { log, summary } = await openUsage(page, world.owner);
  await expect(logRows(log)).toHaveCount(1);
  await logRows(log).first().getByRole("button", { name: "查看调用详情", exact: true }).click();
  const drawer = page.getByRole("dialog", { name: "调用详情", exact: true });
  await expect(drawer.getByRole("table", { name: "计费明细" })).toBeVisible();
  const html = await page.content();
  expect(html).toContain(SHARED_DISPLAY_NAME);
  for (const channel of [...CHANNEL, "shared"]) expect(html).not.toContain(channel);
  await drawer.getByRole("button", { name: "关闭调用详情", exact: true }).click();
  await expect(logRows(log).first()).toContainText("chat-ui");
  await expect(callCell(logRows(log).first(), "状态")).toContainText(SHARED_DISPLAY_NAME);

  await usageLink(page, "用量分析").click();
  const summaryRows = summary.getByRole("table", { name: "用量汇总" }).locator("tbody > tr");
  await expect(summaryRows).toHaveCount(1);
  await expect(summaryRows.first()).toContainText("chat-ui");
  const summaryHtml = await page.content();
  for (const channel of [...CHANNEL, "shared"]) expect(summaryHtml).not.toContain(channel);
  const raw = JSON.stringify((await (await signedIn(world.owner)).get("/api/usage/calls")).items);
  expect(raw).toContain("chat-ui");
  expect(raw).toContain(SHARED_DISPLAY_NAME);
  // The group's routing name is not in the answer either, only its display name.
  for (const channel of [...CHANNEL, "shared", "error_message", "provider", "upstream_model"]) expect(raw).not.toContain(channel);
});

test("无权使用的模型与不存在的模型，在调用记录里显示相同的状态", async ({ page }) => {
  const world = await seedBillingWorld(stack);
  // An existing model this organisation is not granted: another organisation is granted it, and once
  // the gateway lists it there, the gateway knows it.
  const hiddenId = await addModel("chat-hidden-ui");
  const other = await seedBillingWorld(stack);
  await grant(other, hiddenId);
  await gatewayListsModel(other.key.plaintext, "chat-hidden-ui");
  const hiddenStatus = await chat(stack, world.key.plaintext, "chat-hidden-ui");
  expect(await chat(stack, world.key.plaintext, "no-such-model-ui-3a1")).toBe(hiddenStatus);
  await logged(world.owner, 2, true);
  const { log } = await openUsage(page, world.owner);

  const hidden = logRows(log).filter({ hasText: "chat-hidden-ui" });
  const missing = logRows(log).filter({ hasText: "no-such-model-ui-3a1" });
  await expect(missing).toHaveCount(1);
  const missingStatus = "失败（模型不存在）";
  await expect(callCell(missing, "状态").getByText(missingStatus, { exact: true })).toBeVisible();
  await expect(hidden).toHaveCount(1);
  await expect(callCell(hidden, "状态").getByText(missingStatus, { exact: true })).toBeVisible();
});

test("上游无法访问时只显示归一后的原因", async ({ page }) => {
  const world = await seedBillingWorld(stack);
  const closed = await freePort();
  const id = await addModel("chat-ui-down", { apiBase: `http://127.0.0.1:${closed}/v1` });
  await grant(world, id);
  await gatewayListsModel(world.key.plaintext, "chat-ui-down");
  expect(await chat(stack, world.key.plaintext, "chat-ui-down")).toBeGreaterThanOrEqual(500);
  await logged(world.owner, 1, true);
  const { log } = await openUsage(page, world.owner);

  const row = logRows(log).filter({ hasText: "chat-ui-down" });
  await expect(row).toHaveCount(1);
  await expect(callCell(row, "状态").getByText("失败（上游暂不可用）", { exact: true })).toBeVisible();
  // Nothing of the address the upstream was tried at, or of how it failed, in the page or in the API's answer.
  const html = await page.content();
  const raw = JSON.stringify((await (await signedIn(world.owner)).get("/api/usage/calls")).items);
  expect(raw).toContain("chat-ui-down");
  for (const text of [...CHANNEL, String(closed), "acct-chat-ui-down", "sk-chat-ui-down", "upstream-chat-ui-down"]) {
    expect(html).not.toContain(text);
    expect(raw).not.toContain(text);
  }
  // The Call shape has a `status_code`, which a failed call leaves null: no upstream status reaches the tenant.
  for (const key of ["error_message", "provider"]) expect(raw).not.toContain(`"${key}"`);
  expect(JSON.parse(raw).map((c: Json) => [c.status, c.error_class, c.status_code])).toEqual([["failed", "upstream_unavailable", null]]);
});

test("按请求 ID 找到一次调用", async ({ page }) => {
  const world = await seedBillingWorld(stack);
  expect(await chat(stack, world.key.plaintext, "chat-ui", { requestId: "req-ui-7c1" })).toBe(200);
  expect(await chat(stack, world.key.plaintext, "chat-ui")).toBe(200);
  await logged(world.owner, 2);
  const { log } = await openUsage(page, world.owner);

  await expect(logRows(log)).toHaveCount(2);
  await log.getByLabel("请求 ID").fill("req-ui-7c1");
  await expect(logRows(log)).toHaveCount(1);
  await expect(callCell(logRows(log).first(), "模型 / 请求").getByText("req-ui-7c1", { exact: true })).toBeVisible();
  // An id that only starts like it finds nothing: the match is exact.
  await log.getByLabel("请求 ID").fill("req-ui-7c");
  await expect(logRows(log)).toHaveCount(0);
});

test("尚未计价的调用显示待计价", async ({ page }) => {
  const world = await seedBillingWorld(stack, { unpricedModel: true });
  await gatewayListsModel(world.key.plaintext, "chat-ui-later");
  expect(await chat(stack, world.key.plaintext, "chat-ui-later")).toBe(200);
  await logged(world.owner, 1, true);
  const { log } = await openUsage(page, world.owner);

  const row = logRows(log).filter({ hasText: "chat-ui-later" });
  await expect(row).toHaveCount(1);
  await expect(callCell(row, "费用")).toHaveText("金额未知");
  await row.getByRole("button", { name: "查看调用详情", exact: true }).click();
  await expect(page.getByRole("dialog").getByText("待计价，最终金额未知。", { exact: true })).toBeVisible();
});

test("成员只看到自己密钥的调用", async ({ page, browser }) => {
  const world = await seedBillingWorld(stack, { members: ["member"] });
  const member = world.members[0]!;
  const mine = await memberKey(stack, world, member);
  await keyAdmitted(mine.plaintext);
  expect(await chat(stack, world.key.plaintext, "chat-ui")).toBe(200);
  expect(await chat(stack, world.key.plaintext, "chat-ui")).toBe(200);
  expect(await chat(stack, mine.plaintext, "chat-ui")).toBe(200);
  await logged(world.owner, 3);

  const { log } = await openUsage(page, member);
  await expect(logRows(log)).toHaveCount(1);
  const identity = callCell(logRows(log).first(), "模型 / 请求");
  await expect(identity).toContainText("密钥 member-key-");
  await expect(identity).not.toContainText("world-owner-key");
  // The member's summary counts only the member's own call, at its own charge.
  await usageLink(page, "用量分析").click();
  const mySummary = page.getByRole("region", { name: "用量汇总", exact: true });
  const mine1 = mySummary.locator("tbody tr");
  await expect(mine1).toHaveCount(1);
  await expect(mine1.first().getByRole("cell").nth(2)).toHaveText("1");
  await expect(mine1.first().getByRole("cell").nth(3)).toHaveText("¥0.011");

  // The owner, in a browser of their own, sees all three.
  const ownerPage = await (await browser.newContext()).newPage();
  const { log: ownerLog } = await openUsage(ownerPage, world.owner);
  await expect(logRows(ownerLog)).toHaveCount(3);
});

test("账单角色看到全部调用，但没有密钥筛选和错误提示", async ({ page }) => {
  const world = await seedBillingWorld(stack, { members: ["billing"] });
  const billingMember = world.members[0]!;
  expect(await chat(stack, world.key.plaintext, "chat-ui")).toBe(200);
  await logged(world.owner, 1);

  const { log, summary } = await openUsage(page, billingMember);
  await expect(logRows(log)).toHaveCount(1);
  await expect(page.getByLabel("密钥", { exact: true })).toHaveCount(0);
  await expect(page.getByRole("alert")).toHaveCount(0);
  await usageLink(page, "用量分析").click();
  await expect(summary.getByRole("row").filter({ hasText: "chat-ui" })).toHaveCount(1);
  await expect(page.getByLabel("密钥", { exact: true })).toHaveCount(0);
  await expect(page.getByRole("alert")).toHaveCount(0);
});

test("用量汇总与调用记录一致", async ({ page }) => {
  await waitClearOfUtcMidnight();
  const world = await seedBillingWorld(stack);
  for (let i = 0; i < 3; i++) expect(await chat(stack, world.key.plaintext, "chat-ui")).toBe(200);
  await logged(world.owner, 3);
  const { log, summary } = await openUsage(page, world.owner);

  await expect(logRows(log)).toHaveCount(3);
  await usageLink(page, "用量分析").click();
  const today = new Date().toLocaleDateString("sv-SE", { timeZone: "Asia/Shanghai" });
  const row = summary.getByRole("row").filter({ hasText: today });
  await expect(row).toHaveCount(1);
  const cells = row.getByRole("cell");
  await summary.getByText("计费与汇总口径", { exact: true }).click();
  await expect(summary.getByText(/^按用量发生的时间汇总/)).toBeVisible();
  await expect(cells.nth(1)).toHaveText("chat-ui");
  await expect(cells.nth(2)).toHaveText("3");
  await expect(cells.nth(3)).toHaveText("¥0.033");
  await expect(cells.nth(4)).toHaveText(/输入\s*15(?!\d).*输出\s*9(?!\d)/);
});

test("缓存命中的调用显示不计费", async ({ page }) => {
  const world = await seedBillingWorld(stack);
  // A model of its own, so the cache policy touches no other test.
  const id = await pricedModel(world, "chat-ui-cached");
  await stack.admin.post("/admin/resources/cache_policies", {
    doc: { name: `cache-ui-${world.tenantId.slice(0, 8)}`, scope: "team", applies_to_model_id: id, ttl_seconds: 300 },
  });
  expect(await chat(stack, world.key.plaintext, "chat-ui-cached")).toBe(200);
  await logged(world.owner, 1);
  // The same request again: the answer is served from the cache, which may take a moment to fill.
  let calls = 1;
  await expect
    .poll(
      async () => {
        expect(await chat(stack, world.key.plaintext, "chat-ui-cached")).toBe(200);
        calls += 1;
        const items = await logged(world.owner, calls, true);
        return items[0].cache_hit === true;
      },
      { message: "a repeated request is served from the cache", timeout: 30_000 },
    )
    .toBe(true);
  const { log } = await openUsage(page, world.owner);

  // Newest first: the top row is the cache hit.
  await expect(callCell(logRows(log).first(), "费用")).toHaveText("¥0.00");
  await expect(callCell(logRows(log).last(), "费用")).toHaveText("¥0.011");
  await logRows(log).first().getByRole("button", { name: "查看调用详情", exact: true }).click();
  await expect(page.getByRole("dialog").getByText("费用 不计费（缓存命中）", { exact: true })).toBeVisible();
});

test("重新计价后，调用的费用显示已调整的金额，汇总计入调整", async ({ page }) => {
  await waitClearOfUtcMidnight();
  const world = await seedBillingWorld(stack);
  // A model of its own, so the new price touches no other test.
  const modelId = await pricedModel(world, "chat-ui-repriced");
  expect(await chat(stack, world.key.plaintext, "chat-ui-repriced")).toBe(200);
  await logged(world.owner, 1);

  // The price doubles, from a moment ahead; a run then corrects the call at the new price.
  const startsAt = Date.now() + 3_000;
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: modelId,
    effective_from: new Date(startsAt).toISOString(),
    card: { default: { base: { input: "2000", output: "4000" } } },
    note: "the usage page's doubled card",
  });
  await expect.poll(() => Date.now() > startsAt + 500).toBe(true);
  const from = new Date(Date.now() - 3_600_000).toISOString();
  let runId = "";
  await expect
    .poll(
      async () => {
        const run = await stack.admin.post("/admin/billing/repricing", {
          from,
          to: new Date(Date.now() - 1_000).toISOString(),
          tenant_id: world.tenantId,
          reason: "usage page e2e",
        });
        runId = run.id;
        return run.changed;
      },
      { message: "a run at the new price changes the call", timeout: 30_000 },
    )
    .toBe(1);
  await stack.admin.post(`/admin/billing/repricing/${runId}/apply`, {});

  const { log, summary } = await openUsage(page, world.owner);
  await expect(logRows(log)).toHaveCount(1);
  await expect(callCell(logRows(log).first(), "费用")).toHaveText("¥0.022（含调整）");
  await logRows(log).first().getByRole("button", { name: "查看调用详情", exact: true }).click();
  const drawer = page.getByRole("dialog", { name: "调用详情", exact: true });
  await expect(drawer.getByText("¥0.011 +¥0.011 = ¥0.022", { exact: true })).toBeVisible();
  await drawer.getByRole("button", { name: "关闭调用详情", exact: true }).click();
  // The correction counts in the period the call occurred in: ¥0.011 charged and ¥0.011 corrected.
  await usageLink(page, "用量分析").click();
  const today = new Date().toLocaleDateString("sv-SE", { timeZone: "Asia/Shanghai" });
  await expect(summary.getByRole("row").filter({ hasText: today }).getByRole("cell").nth(3)).toHaveText("¥0.022");
});

test("先未计价、后经重新计价的调用，费用显示重新计价的金额", async ({ page }) => {
  await waitClearOfUtcMidnight();
  const world = await seedBillingWorld(stack);
  // A model of its own with no price, granted to the organisation.
  const name = "chat-ui-unpriced-then-priced";
  const modelId = await addModel(name);
  await grant(world, modelId);
  await gatewayListsModel(world.key.plaintext, name);
  expect(await chat(stack, world.key.plaintext, name)).toBe(200);
  await logged(world.owner, 1, true);

  const startsAt = Date.now() + 3_000;
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: modelId,
    effective_from: new Date(startsAt).toISOString(),
    card: { default: { base: { input: "1000", output: "2000" } } },
    note: "the usage page's late card",
  });
  await expect.poll(() => Date.now() > startsAt + 500).toBe(true);
  const from = new Date(Date.now() - 3_600_000).toISOString();
  let runId = "";
  await expect
    .poll(
      async () => {
        const run = await stack.admin.post("/admin/billing/repricing", {
          from,
          to: new Date(Date.now() - 1_000).toISOString(),
          tenant_id: world.tenantId,
          reason: "usage page e2e, unpriced call",
        });
        runId = run.id;
        return run.changed;
      },
      { message: "a run prices the call", timeout: 30_000 },
    )
    .toBe(1);
  await stack.admin.post(`/admin/billing/repricing/${runId}/apply`, {});

  const { log } = await openUsage(page, world.owner);
  await expect(logRows(log)).toHaveCount(1);
  await expect(callCell(logRows(log).first(), "费用")).toHaveText("¥0.011（含调整）");
  await logRows(log).first().getByRole("button", { name: "查看调用详情", exact: true }).click();
  const drawer = page.getByRole("dialog", { name: "调用详情", exact: true });
  await expect(drawer.getByText("原调用待计价，未形成原始费用", { exact: true })).toBeVisible();
  await expect(drawer.getByText("0（未形成原始费用） +¥0.011 = ¥0.011", { exact: true })).toBeVisible();
});

test("汇总的时间范围超过上限时，在日期旁说明，不发出请求", async ({ page }) => {
  const world = await seedBillingWorld(stack);
  const { summary } = await openUsage(page, world.owner);
  const requests: string[] = [];
  page.on("request", request => {
    const url = new URL(request.url());
    if (url.pathname === "/api/usage/summary" && url.searchParams.get("from")?.startsWith("2019-12-31")) {
      requests.push(url.searchParams.get("group") ?? "");
    }
  });
  await usageLink(page, "用量分析").click();
  const grouping = summary.getByRole("combobox", { name: "汇总方式" });
  await expect(grouping).toContainText("按天");
  const filters = page.getByRole("region", { name: "用量分析筛选", exact: true });
  await filters.getByLabel("开始日期").fill("2020-01-01");
  await filters.getByLabel("结束日期").fill("2020-06-30");
  await expect(summary.getByRole("alert")).toContainText("时间范围过大");
  const response = page.waitForResponse(answer => {
    const url = new URL(answer.url());
    return url.pathname === "/api/usage/summary" && url.searchParams.get("group") === "month";
  });
  await chooseSelect(grouping, "month");
  expect((await response).status()).toBe(200);
  await expect(summary.getByRole("alert")).toHaveCount(0);
  expect(requests).toEqual(["month"]);
});
