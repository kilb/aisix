import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";
import { chat, seedTenant } from "./world";

/**
 * 价格 · 平台价格 against the real control plane through the built `dist/`.
 * Models are created through the real `/admin`; the only stand-in is the
 * upstream the gateway calls.
 */

let stack: Stack;
let account: string;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  const key = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: "acct-sell-4d1",
      provider: "openai",
      api_key: "sk-acct-sell-4d1-upstream",
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  account = key.id;
});

test.afterAll(async () => {
  await stack?.stop();
});

/** A new model with no price. The name sorts before any seeded one. */
async function newModel(): Promise<{ id: string; name: string }> {
  const name = `a0-sell-${randomUUID().slice(0, 8)}`;
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: name, provider: "openai", model_name: `upstream-${name}`, provider_key_id: account },
  });
  return { id: model.id, name };
}

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

async function openPrices(page: Page, who = stack.operator): Promise<void> {
  await login(page, who.email, who.password);
  await page.locator('[data-tab="pricing"]').click();
  await expect(page.getByRole("table", { name: "平台价格" })).toBeVisible();
}

function row(page: Page, name: string): Locator {
  return page.getByRole("table", { name: "平台价格" }).getByRole("row").filter({ has: page.getByRole("button", { name, exact: true }) });
}

async function openModel(page: Page, name: string): Promise<Locator> {
  await row(page, name).getByRole("button", { name, exact: true }).click();
  const panel = page.getByRole("dialog", { name: `${name} 的价格` });
  await expect(panel).toBeVisible();
  return panel;
}

/** Fills the card form's rates; a rate given as "" is cleared. */
async function fillCard(panel: Locator, rates: { 输入?: string; 输出?: string }): Promise<void> {
  for (const [label, value] of Object.entries(rates)) {
    await panel.getByLabel(label, { exact: true }).fill(value);
  }
}

/** The history table's rows, newest first, as the cells' text. */
async function historyRows(panel: Locator): Promise<string[][]> {
  const table = panel.getByRole("table", { name: "价格历史" });
  await expect(table).toBeVisible();
  return table.locator("tbody tr").evaluateAll((trs) =>
    trs.map((tr) => [...tr.querySelectorAll("td")].map((td) => (td.textContent ?? "").trim())),
  );
}

function parseUtc(text: string): number {
  return Date.parse(text.replace(" UTC", "").replace(" ", "T") + "Z");
}

/** `1.2`-style markup applied to a rate string, rounded half up to 6 decimals, in integers. */
function marked(rate: string, bps: bigint): string {
  const [whole = "0", frac = ""] = rate.split(".");
  const micro = BigInt(whole) * 1_000_000n + BigInt(frac.padEnd(6, "0"));
  const scaled = micro * (10_000n + bps);
  const out = (scaled + 5_000n) / 10_000n;
  const w = out / 1_000_000n;
  const f = (out % 1_000_000n).toString().padStart(6, "0").replace(/0+$/, "");
  return f ? `${w}.${f}` : `${w}`;
}

test("未定价的模型排在前面并标红，设置平台价后显示现行价格", async ({ page }) => {
  const model = await newModel();
  // A priced model that sorts before it by name: only the price puts it after.
  const pricedBefore = await newModel();
  await stack.admin.post("/admin/pricing/sell", {
    model_id: pricedBefore.id,
    effective_from: new Date().toISOString(),
    card: { default: { base: { input: "1", output: "2" } } },
  });
  await openPrices(page);

  const states = await page.getByRole("table", { name: "平台价格" }).locator("tbody tr").evaluateAll((trs) =>
    trs.map((tr) => tr.getAttribute("data-state")),
  );
  expect(states).toContain("priced");
  expect(states[0]).toBe("unpriced");
  expect(states).toEqual([...states].sort((a, b) => (a === b ? 0 : a === "unpriced" ? -1 : 1)));
  const first = page.getByRole("table", { name: "平台价格" }).locator("tbody tr").first();
  await expect(first).toHaveAttribute("data-state", "unpriced");
  await expect(first).toContainText("未定价");
  await expect(row(page, model.name)).toHaveAttribute("data-state", "unpriced");

  const panel = await openModel(page, model.name);
  await fillCard(panel, { 输入: "3", 输出: "6" });
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  const priced = row(page, model.name);
  await expect(priced).toHaveAttribute("data-state", "priced");
  await expect(priced).toContainText("输入 3 输出 6");
  await expect(priced).toContainText(/#\d+/);
  await expect(priced).not.toContainText("未定价");
});

test("按价格卡设置平台价：立即生效，历史按时间倒序", async ({ page }) => {
  const model = await newModel();
  await openPrices(page);
  const panel = await openModel(page, model.name);

  const saves: { at: number }[] = [];
  for (const [input, output] of [["1", "2"], ["4", "8"]]) {
    await fillCard(panel, { 输入: input, 输出: output });
    await panel.getByRole("button", { name: "保存" }).click();
    saves.push({ at: Date.now() });
    await expect(panel.getByRole("table", { name: "价格历史" }).locator("tbody tr")).toHaveCount(saves.length);
  }
  const rows = await historyRows(panel);
  // Newest first: the second save's rates on top.
  expect(rows[0]!.slice(2, 4)).toEqual(["4", "8"]);
  expect(rows[1]!.slice(2, 4)).toEqual(["1", "2"]);
  const revisions = rows.map((r) => Number(r[0]!.replace("#", "")));
  expect(revisions[0]).toBeGreaterThan(revisions[1]!);
  rows.forEach((r, i) => {
    const at = saves[saves.length - 1 - i]!.at;
    expect(Math.abs(parseUtc(r[1]!) - at)).toBeLessThan(60_000);
  });
});

test("打开表单 10 秒后保存，立即生效的价格仍被接受", async ({ page }) => {
  const model = await newModel();
  await openPrices(page);
  const panel = await openModel(page, model.name);
  await fillCard(panel, { 输入: "5", 输出: "10" });
  // The scenario is a form left open: the backend refuses a start more than 5 s in the past.
  await page.waitForTimeout(10_000);
  const clickedAt = Date.now();
  const created = page.waitForResponse((r) => r.url().endsWith("/admin/pricing/sell") && r.request().method() === "POST");
  await panel.getByRole("button", { name: "保存" }).click();
  expect((await created).status()).toBe(201);
  await expect(panel.getByRole("alert")).toHaveCount(0);
  const rows = await historyRows(panel);
  expect(rows).toHaveLength(1);
  expect(parseUtc(rows[0]![1]!)).toBeGreaterThanOrEqual(clickedAt - 3_000);
});

test("从价格目录导入并加价 20%，现行价格为目录价乘 1.2", async ({ page }) => {
  const model = await newModel();
  const { items } = await stack.admin.get("/admin/pricing/catalogue");
  const entry: Json = items.find((e: Json) => e.effective_from === null && !(e.notes ?? "").toLowerCase().includes("peak")) ?? items[0];
  const input: string = entry.card.default.base.input;
  const expected = marked(input, 2_000n);

  await openPrices(page);
  const panel = await openModel(page, model.name);
  await panel.getByLabel("方式").selectOption({ label: "从价格目录导入" });
  await panel.getByLabel("目录条目").selectOption(entry.key);
  await panel.getByLabel("加价（%）").fill("20");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  const priced = row(page, model.name);
  await expect(priced).toHaveAttribute("data-state", "priced");
  await expect(priced).toContainText(new RegExp(`输入 ${expected.replace(".", "\\.")} 输出`));
});

test("价格卡缺少输出单价时，错误显示在价格卡上，什么也没保存", async ({ page }) => {
  const model = await newModel();
  await openPrices(page);
  const panel = await openModel(page, model.name);
  await fillCard(panel, { 输入: "3", 输出: "" });
  await panel.getByRole("button", { name: "保存" }).click();

  const card = panel.getByRole("group", { name: /价格卡/ });
  await expect(card.getByRole("alert")).toContainText("价目卡不合法");
  await expect(row(page, model.name)).toHaveAttribute("data-state", "unpriced");

  await page.reload();
  await page.locator('[data-tab="pricing"]').click();
  await expect(row(page, model.name)).toHaveAttribute("data-state", "unpriced");
  const stored = await stack.admin.get(`/admin/pricing/sell?model_id=${model.id}`);
  expect(stored.items).toHaveLength(0);
});

test("生效时间早于现在时被拒绝并说明原因", async ({ page }) => {
  const model = await newModel();
  await openPrices(page);
  const panel = await openModel(page, model.name);
  await fillCard(panel, { 输入: "3", 输出: "6" });
  await panel.getByLabel("立即生效").uncheck();
  const hourAgo = new Date(Date.now() - 3_600_000).toISOString().slice(0, 16).replace("T", " ");
  await panel.getByLabel("生效时间（UTC）").fill(hourAgo);
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(panel.getByText("生效时间不能早于当前时间")).toBeVisible();
  await expect(panel.getByLabel("生效时间（UTC）")).toHaveAttribute("aria-invalid", "true");
  expect((await stack.admin.get(`/admin/pricing/sell?model_id=${model.id}`)).items).toHaveLength(0);
});

test("页面显示的现行价格就是调用实际计费的价格", async ({ page }) => {
  const world = await seedTenant(stack);
  await openPrices(page);
  const panel = await openModel(page, world.model.name);
  await fillCard(panel, { 输入: "7", 输出: "9" });
  const written = page.waitForResponse((r) => r.url().endsWith("/admin/pricing/sell") && r.request().method() === "POST");
  await panel.getByRole("button", { name: "保存" }).click();
  const revision: number = (await (await written).json()).revision_id;
  await expect(page.getByText("已保存")).toBeVisible();
  // Wait for the reloaded table to show the revision just written, not the one before it.
  await expect(row(page, world.model.name).getByRole("cell").nth(2)).toHaveText(`#${revision}`);
  const shown = revision;

  const requestId = `req-sell-${randomUUID().slice(0, 8)}`;
  expect(await chat(stack, world.key.plaintext, world.model.name, { requestId })).toBe(200);
  await expect
    .poll(async () => (await stack.admin.get(`/admin/usage/events?request_id=${requestId}`)).items.length, { timeout: 30_000 })
    .toBe(1);
  const event = (await stack.admin.get(`/admin/usage/events?request_id=${requestId}`)).items[0];
  await expect
    .poll(async () => (await stack.admin.get(`/admin/pricing/charges?event_id=${event.event_id}`)).items[0]?.sell_revision_id, {
      timeout: 30_000,
    })
    .toBe(shown);
});

test("只读运维看不到任何编辑控件", async ({ page }) => {
  const model = await newModel();
  const viewer = { email: `viewer-sell-${randomUUID().slice(0, 6)}@e2e.local`, password: "viewer-sell-pass-7d2" };
  await stack.admin.post("/admin/operators", { ...viewer, display_name: "只读", role: "viewer" });
  await stack.admin.post("/admin/pricing/sell", {
    model_id: model.id,
    effective_from: new Date().toISOString(),
    card: { default: { base: { input: "2", output: "4" } } },
  });
  await openPrices(page, viewer);
  await expect(row(page, model.name)).toContainText("输入 2 输出 4");
  const panel = await openModel(page, model.name);
  await expect(panel.getByRole("table", { name: "价格历史" })).toBeVisible();
  await expect(page.getByRole("button", { name: "保存" })).toHaveCount(0);
  await expect(page.getByText("新价格")).toHaveCount(0);
  await expect(page.getByRole("form", { name: "新价格" })).toHaveCount(0);
});

test("平台价格历史不含租户专属价", async ({ page }) => {
  const world = await seedTenant(stack);
  await stack.admin.post("/admin/pricing/sell", {
    model_id: world.model.id,
    effective_from: new Date().toISOString(),
    card: { default: { base: { input: "11", output: "12" } } },
  });
  await stack.admin.post("/admin/pricing/sell", {
    model_id: world.model.id,
    tenant_id: world.tenantId,
    effective_from: new Date().toISOString(),
    card: { default: { base: { input: "777", output: "888" } } },
  });
  await openPrices(page);
  const panel = await openModel(page, world.model.name);
  const table = panel.getByRole("table", { name: "价格历史" });
  await expect(table).toContainText("11");
  await expect(table).not.toContainText("777");
  await expect(table).not.toContainText("888");
});
