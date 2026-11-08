import { randomUUID } from "node:crypto";
import { expect, test, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";
import { CALL_MICRO_USD, chat, seedTenant, waitClearOfUtcMidnight, type BillingWorld } from "./world";

/**
 * 「计费 · 重新计价」 against the real control plane and gateway: a model with
 * no price, three real chats on it, then a price entered late. Expected
 * values come from the scenario: three calls at the world's card cost
 * 3 x 11 000 micro-dollars.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  // The form's window starts at 00:00 UTC today: no test may straddle two days.
  await waitClearOfUtcMidnight();
});

test.afterAll(async () => {
  await stack?.stop();
});

const CARD = { default: { base: { input: "1000", output: "2000" } } };
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** Writes a sell price and waits until it is in force (a price cannot start in the past). */
async function price(modelId: string, card: Json): Promise<void> {
  const startsAt = Date.now() + 3_000;
  await stack.admin.post("/admin/pricing/sell", { model_id: modelId, effective_from: new Date(startsAt).toISOString(), card });
  await sleep(Math.max(0, startsAt - Date.now()) + 300);
}

/** An organisation with its own model that has no price, and three chats made on it. */
async function unpricedWorld(): Promise<{ world: BillingWorld; model: { id: string; name: string } }> {
  const world = await seedTenant(stack);
  const suffix = randomUUID().slice(0, 8);
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: `acct-rp-${suffix}`,
      provider: "openai",
      api_key: `sk-acct-rp-${suffix}`,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  const made = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: `rp-model-${suffix}`, provider: "openai", model_name: `upstream-rp-${suffix}`, provider_key_id: account.id },
  });
  const model = { id: made.id as string, name: `rp-model-${suffix}` };
  const tenant = await stack.admin.get(`/admin/tenants/${world.tenantId}`);
  await stack.admin.patch(
    `/admin/tenants/${world.tenantId}`,
    { allowed_model_ids: [...tenant.allowed_model_ids, model.id] },
    tenant.version,
  );
  // Wait until the gateway lists the model for the key, so no chat is refused and filed as something else.
  await expect
    .poll(
      async () => {
        const res = await fetch(`${stack.proxyUrl}/v1/models`, { headers: { authorization: `Bearer ${world.key.plaintext}` } });
        const body = (await res.json()) as { data?: { id: string }[] };
        return (body.data ?? []).some((m) => m.id === model.name);
      },
      { timeout: 60_000 },
    )
    .toBe(true);
  expect(await chat(stack, world.key.plaintext, model.name)).toBe(200);
  expect(await chat(stack, world.key.plaintext, model.name)).toBe(200);
  expect(await chat(stack, world.key.plaintext, model.name)).toBe(200);
  await expect
    .poll(async () => (await stack.admin.get(`/admin/pricing/charges?tenant_id=${world.tenantId}`)).items.length, { timeout: 60_000 })
    .toBe(3);
  return { world, model };
}

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
  await page.locator('[data-tab="billing"]').click();
  await page.getByRole("button", { name: "重新计价", exact: true }).click();
  await expect(page.getByRole("heading", { name: "重新计价", exact: true })).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
}

/** Fills the form for one tenant's calls on one model, up to this very second, and previews. */
async function preview(page: Page, world: BillingWorld, model: { name: string }, reason = "补录价格-ui", status = "unpriced"): Promise<void> {
  // The window's end is the clock as the control plane will read it, so it includes the calls just made.
  const form = page.getByRole("form", { name: "新建重新计价" });
  await form.getByLabel("结束（UTC）").fill(new Date().toISOString().slice(0, 19));
  await form.getByLabel("模型").selectOption({ label: model.name });
  await form.getByLabel("租户").selectOption({ label: world.tenantName });
  await form.getByLabel("状态").selectOption({ value: status });
  await form.getByLabel("原因").fill(reason);
  await form.getByRole("button", { name: "预览", exact: true }).click();
  await expect(page.getByRole("region", { name: "预览结果" })).toBeVisible();
}

const totals = (page: Page) => page.getByRole("table", { name: "合计" });
const ledger = async (world: BillingWorld): Promise<Json[]> => (await stack.admin.get(`/admin/tenants/${world.tenantId}/billing/ledger`)).items;

test("重新计价：先预览影响的调用数与各租户差额，再由管理员确认执行", async ({ page }) => {
  const { world, model } = await unpricedWorld();
  await price(model.id, CARD);
  await login(page, stack.operator.email, stack.operator.password);
  await preview(page, world, model);

  await expect(totals(page).getByRole("row", { name: /选中的调用/ })).toContainText("3");
  await expect(totals(page).getByRole("row", { name: /有变化/ })).toContainText("3");
  await expect(totals(page).getByRole("row", { name: /合计差额/ })).toContainText("$0.033");
  expect(CALL_MICRO_USD * 3).toBe(33_000);
  await expect(page.getByRole("table", { name: "各租户差额" }).getByRole("row", { name: new RegExp(world.tenantName) })).toContainText("$0.033");
  // A preview moves no money.
  expect((await ledger(world)).filter((e) => e.kind === "adjustment")).toHaveLength(0);

  await page.getByRole("button", { name: "执行", exact: true }).click();
  await expect(page.getByRole("alert").filter({ hasText: "将按现行价格改写所选时间段内的费用，并为每个租户记一笔调整，无法撤销" })).toBeVisible();
  expect((await ledger(world)).filter((e) => e.kind === "adjustment")).toHaveLength(0);
  await page.getByRole("button", { name: "确认执行" }).click();

  const list = page.getByRole("table", { name: "重新计价运行" });
  await expect(list.getByRole("row", { name: /补录价格-ui/ })).toContainText("已执行");
  await expect(page.getByRole("status").filter({ hasText: "已执行" })).toContainText("$0.033");
});

test("执行后租户流水出现一笔调整；同一预览不能再次执行", async ({ page }) => {
  const { world, model } = await unpricedWorld();
  await price(model.id, CARD);
  await login(page, stack.operator.email, stack.operator.password);
  await preview(page, world, model, "流水-ui");
  await page.getByRole("button", { name: "执行", exact: true }).click();
  await page.getByRole("button", { name: "确认执行" }).click();
  await expect(page.getByRole("status").filter({ hasText: "已执行" })).toBeVisible();

  const entries = (await ledger(world)).filter((e) => e.kind === "adjustment");
  expect(entries).toHaveLength(1);
  expect(entries[0].amount_micro_usd).toBe(-33_000);

  // The run offers no 执行 any more, and the control plane agrees.
  await expect(page.getByRole("button", { name: /^(确认)?执行$/ })).toHaveCount(0);
  const runId = (await stack.admin.get("/admin/billing/repricing")).items.find((r: Json) => r.reason === "流水-ui").id;
  const again = await stack.admin.request("POST", `/admin/billing/repricing/${runId}/apply`, {});
  expect(again.status).toBe(409);
  expect(again.body.code).toBe("run_state");
  expect((await ledger(world)).filter((e) => e.kind === "adjustment")).toHaveLength(1);
});

test("预览之后价格又变，执行时提示需要重新预览", async ({ page }) => {
  const { world, model } = await unpricedWorld();
  await price(model.id, CARD);
  await login(page, stack.operator.email, stack.operator.password);
  await preview(page, world, model, "过期-ui");
  await expect(totals(page).getByRole("row", { name: /合计差额/ })).toContainText("$0.033");

  // A new sell price after the preview.
  await price(model.id, { default: { base: { input: "2000", output: "4000" } } });
  await page.getByRole("button", { name: "执行", exact: true }).click();
  await page.getByRole("button", { name: "确认执行" }).click();
  await expect(page.getByRole("alert").filter({ hasText: "预览之后价格或数据有变化，请重新预览" })).toBeVisible();
  expect((await ledger(world)).filter((e) => e.kind === "adjustment")).toHaveLength(0);
  // The stale run cannot be applied again from here; it can be previewed again.
  await expect(page.getByRole("button", { name: /^(确认)?执行$/ })).toHaveCount(0);

  await page.getByRole("button", { name: "重新预览" }).click();
  // At the new price: 3 x 22 000.
  await expect(totals(page).getByRole("row", { name: /合计差额/ })).toContainText("$0.066");
  await expect(page.getByRole("button", { name: "执行", exact: true })).toBeVisible();
});

test("运维角色只能预览，执行按钮只给管理员", async ({ page, browser }) => {
  const { world, model } = await unpricedWorld();
  await price(model.id, CARD);
  const op = { email: "operator-repricing@e2e.local", password: "operator-repricing-pass-4c1" };
  await stack.admin.post("/admin/operators", { ...op, role: "operator" });
  await login(page, op.email, op.password);
  await preview(page, world, model, "运维-ui");
  await expect(totals(page).getByRole("row", { name: /合计差额/ })).toContainText("$0.033");
  await expect(page.getByRole("button", { name: /执行/ })).toHaveCount(0);
  await expect(page.getByText("执行需要管理员权限")).toBeVisible();
  // The control plane agrees: an operator's apply is refused.
  const runId = (await stack.admin.get("/admin/billing/repricing")).items.find((r: Json) => r.reason === "运维-ui").id;
  const refused = await page.evaluate(async (id) => {
    const res = await fetch(`/admin/billing/repricing/${id}/apply`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: "{}",
    });
    return res.status;
  }, runId);
  expect(refused).toBe(403);

  // The same run, opened by an admin, offers 执行.
  const adminPage = await (await browser.newContext()).newPage();
  await login(adminPage, stack.operator.email, stack.operator.password);
  await adminPage.getByRole("table", { name: "重新计价运行" }).getByRole("row", { name: /运维-ui/ }).getByRole("button", { name: "查看" }).click();
  await expect(adminPage.getByRole("button", { name: "执行", exact: true })).toBeVisible();
  await adminPage.context().close();
});

test("请求没有得到回答时，不再提供执行，先读取运行状态", async ({ page, context }) => {
  const { world, model } = await unpricedWorld();
  await price(model.id, CARD);
  await login(page, stack.operator.email, stack.operator.password);
  await preview(page, world, model, "未知-ui");
  await page.getByRole("button", { name: "执行", exact: true }).click();
  // The browser loses its connection: the request never gets an answer.
  await context.setOffline(true);
  await page.getByRole("button", { name: "确认执行" }).click();
  await expect(page.getByRole("alert").filter({ hasText: "执行结果未知" })).toBeVisible();
  // No apply button comes back while the outcome is unknown.
  await expect(page.getByRole("button", { name: /^(确认)?执行$/ })).toHaveCount(0);
  await context.setOffline(false);

  await page.getByRole("button", { name: "查看最新状态" }).click();
  await expect(page.getByRole("status").filter({ hasText: "最新状态：尚未执行" })).toBeVisible();
  expect((await ledger(world)).filter((e) => e.kind === "adjustment")).toHaveLength(0);
  await expect(page.getByRole("button", { name: "执行", exact: true })).toBeVisible();
});

test("快速双击确认执行，只发出一次执行请求", async ({ page }) => {
  const { world, model } = await unpricedWorld();
  await price(model.id, CARD);
  await login(page, stack.operator.email, stack.operator.password);
  await preview(page, world, model, "双击-ui");
  // Observing traffic only: nothing is intercepted or answered by the test.
  let applies = 0;
  page.on("request", (r) => {
    if (r.method() === "POST" && /\/admin\/billing\/repricing\/[^/]+\/apply$/.test(new URL(r.url()).pathname)) applies++;
  });
  await page.getByRole("button", { name: "执行", exact: true }).click();
  await page.getByRole("button", { name: "确认执行" }).dblclick();
  await expect(page.getByRole("status").filter({ hasText: "已执行" })).toBeVisible();
  expect(applies).toBe(1);
  expect((await ledger(world)).filter((e) => e.kind === "adjustment")).toHaveLength(1);
});

test("价格调低之后重新计价是退还，合计与运行记录都写明退还", async ({ page }) => {
  const { world, model } = await unpricedWorld();
  await price(model.id, CARD);
  // Setup through the control plane: the first run charges 3 x 11 000.
  const first = await stack.admin.post("/admin/billing/repricing", {
    from: new Date(Date.now() - 3_600_000).toISOString(),
    to: new Date().toISOString(),
    model_id: model.id,
    tenant_id: world.tenantId,
    status: "unpriced",
    reason: "首次-ui",
  });
  await stack.admin.post(`/admin/billing/repricing/${first.id}/apply`, {});
  // The price then goes down by half: 3 x 5 500, so the tenant is owed 3 x 5 500.
  await price(model.id, { default: { base: { input: "500", output: "1000" } } });
  await login(page, stack.operator.email, stack.operator.password);
  await preview(page, world, model, "退还-ui", "");
  const total = totals(page).getByRole("row", { name: /合计差额/ });
  await expect(total).toContainText("-$0.0165");
  await expect(total).toContainText("退还");
  await expect(page.getByRole("table", { name: "重新计价运行" }).getByRole("row", { name: /退还-ui/ })).toContainText("退还");
  await expect(page.getByRole("table", { name: "各租户差额" }).getByRole("row", { name: new RegExp(world.tenantName) })).toContainText("退还");
});
