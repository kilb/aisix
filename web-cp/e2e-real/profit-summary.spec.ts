import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 只读保留的真实调用历史；本文件不创建调用、金额、token 或替代网络响应。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (state.ready !== true || state.mockUpstream !== false) throw new Error("Ready real CP/Postgres history is required");
const base = String(state.adminUrl).replace(/\/$/, "");
if (!["127.0.0.1", "localhost"].includes(new URL(base).hostname)) throw new Error("Only isolated loopback origins are accepted");
const output = resolve(process.env.REFINED_QA_OUTPUT!);
const api = "/admin/billing/profit-summary";
const errors = new WeakMap<Page, string[]>();
const writes = new WeakMap<Page, string[]>();

interface Totals {
  charged_micro_cny: string; adjusted_micro_cny: string; known_cost_micro_cny: string;
  charged_with_cost_micro_cny: string; rows_with_cost: string; rows_without_cost: string;
  unpriced_rows: string; rows: string;
}
interface Answer { from: string | null; to: string | null; totals: Totals }
interface Charge {
  occurred_at: string; status: string; charge_micro_cny: number | null;
  adjusted_micro_cny: number | null; cost_micro_cny: number | null;
}

// 从微元定义独立排版，不导入产品解码器、金额或利润计算函数。
function money(raw: string | bigint): string {
  const value = BigInt(raw), magnitude = value < 0n ? -value : value;
  const whole = (magnitude / 1_000_000n).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  let fraction = (magnitude % 1_000_000n).toString().padStart(6, "0");
  while (fraction.length > 2 && fraction.endsWith("0")) fraction = fraction.slice(0, -1);
  return `${value < 0n ? "-" : ""}¥${whole}.${fraction}`;
}
function margin(profit: bigint, revenue: bigint): string {
  if (revenue <= 0n) return "—";
  const scaled = profit * 10_000n;
  let rounded = scaled / revenue;
  const remainder = scaled % revenue;
  if ((remainder < 0n ? -remainder : remainder) * 2n >= revenue) rounded += scaled < 0n ? -1n : 1n;
  const magnitude = rounded < 0n ? -rounded : rounded;
  return `${rounded < 0n ? "-" : ""}${magnitude / 100n}.${String(magnitude % 100n).padStart(2, "0")}%`;
}
function field(card: Locator, name: string): Locator {
  return card.locator("dl > div").filter({ has: card.page().getByText(name, { exact: true }) }).locator("dd");
}
function wire(value: Answer): void {
  expect(Object.hasOwn(value, "from")).toBe(true);
  expect(Object.hasOwn(value, "to")).toBe(true);
  for (const name of ["charged_micro_cny", "adjusted_micro_cny", "known_cost_micro_cny", "charged_with_cost_micro_cny", "rows_with_cost", "rows_without_cost", "unpriced_rows", "rows"] as const) {
    expect(Object.hasOwn(value.totals, name), name).toBe(true);
    expect(typeof value.totals[name], name).toBe("string");
    expect(value.totals[name], name).toMatch(/^-?\d+$/);
  }
}
async function read<T>(page: Page, path: string): Promise<T> {
  const result = await page.evaluate(async path => {
    const response = await fetch(path, { credentials: "include" });
    return { status: response.status, body: await response.json() };
  }, path);
  expect(result.status, path).toBe(200);
  return result.body as T;
}
async function login(page: Page): Promise<void> {
  await page.goto(base);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const response = page.waitForResponse(r => new URL(r.url()).pathname === "/admin/login" && r.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status()).toBe(200);
  await page.waitForLoadState("networkidle");
}
async function screenshot(page: Page, name: string): Promise<void> {
  mkdirSync(join(output, "screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "screenshots", name + ".png"), fullPage: true });
}
async function assertCards(page: Page, answer: Answer): Promise<void> {
  wire(answer);
  const data = answer.totals;
  const revenue = page.getByRole("region", { name: "总收入统计", exact: true });
  const cost = page.getByRole("region", { name: "总成本统计", exact: true });
  const profit = page.getByRole("region", { name: "利润率统计", exact: true });
  await expect(page.locator("main h1")).toHaveText("本利汇算");
  await expect(revenue.locator(".metric")).toHaveText(money(data.charged_micro_cny));
  await expect(field(revenue, "已含费用调整")).toHaveText(money(data.adjusted_micro_cny));
  await expect(field(cost, "成本缺失的记录")).toHaveText(BigInt(data.rows_without_cost).toLocaleString("en-US"));
  await expect(field(cost, "成本已知的记录")).toHaveText(BigInt(data.rows_with_cost).toLocaleString("en-US"));
  // 保留世界必须包含真实未知成本，才能证明没有把未知显示成零或百分之百利润。
  expect(BigInt(data.rows_without_cost), "real unknown-cost sample is required").toBeGreaterThan(0n);
  await expect(profit.locator(".metric")).toHaveText("—");
  await expect(field(profit, "完整利润")).toHaveText("—");
  await expect(profit.getByText("收入或成本尚不完整，完整利润与利润率未知；已知部分只对成本已知的同一批记录汇算。", { exact: true })).toBeVisible();
  if (BigInt(data.rows_with_cost) === 0n) {
    await expect(cost.locator(".metric")).toHaveText("—");
    await expect(field(profit, "已知成本部分利润率")).toHaveText("—");
  } else {
    await expect(cost.locator(".metric")).toHaveText(money(data.known_cost_micro_cny));
    await expect(field(profit, "已知成本部分利润")).toHaveText(money(BigInt(data.charged_with_cost_micro_cny) - BigInt(data.known_cost_micro_cny)));
    await expect(field(profit, "已知成本部分利润率")).toHaveText(margin(BigInt(data.charged_with_cost_micro_cny) - BigInt(data.known_cost_micro_cny), BigInt(data.charged_with_cost_micro_cny)));
  }
  await expect(field(profit, "已知成本部分收入")).toHaveText(money(data.charged_with_cost_micro_cny));
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
}

test.beforeEach(async ({ page }) => {
  const unexpected: string[] = [], failed: string[] = [];
  writes.set(page, unexpected); errors.set(page, failed);
  page.on("pageerror", error => failed.push(error.message));
  page.on("request", request => {
    const path = new URL(request.url()).pathname;
    if (!["GET", "HEAD"].includes(request.method()) && path !== "/admin/login") unexpected.push(request.method() + " " + path);
    if (/^\/v1(?:\/|$)|\/(?:probe|model-tests|upstream-models)(?:\/|$)/.test(path)) unexpected.push(request.method() + " " + path);
  });
});
test.afterEach(async ({ page }) => {
  expect(writes.get(page)).toEqual([]);
  expect(errors.get(page)).toEqual([]);
});

for (const width of [1440, 390]) {
  test(`billing profit summary uses complete real CNY history and preserves unknown margin at ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    await login(page);
    await page.goto(base + "/#billing");
    const response = page.waitForResponse(r => new URL(r.url()).pathname === api && !new URL(r.url()).search);
    await page.getByRole("button", { name: "本利汇算", exact: true }).click();
    const actual = await response; expect(actual.status()).toBe(200);
    const answer: Answer = await actual.json();
    expect(answer.from).toBeNull(); expect(answer.to).toBeNull();
    await expect(page.getByRole("button", { name: "全部时间", exact: true })).toHaveAttribute("aria-pressed", "true");
    await assertCards(page, answer);
    const records = await read<{ items: Charge[]; next_before: number | null }>(page, "/admin/pricing/charges?limit=200");
    expect(records.next_before, "the retained business sample must fit in this complete oracle page").toBeNull();
    expect(records.items.length).toBeGreaterThan(0);
    const amounts = (value: number | null) => { expect(value === null || Number.isSafeInteger(value)).toBe(true); return BigInt(value ?? 0); };
    const baseCharge = records.items.reduce((sum, row) => sum + amounts(row.charge_micro_cny), 0n);
    const adjustment = records.items.reduce((sum, row) => sum + amounts(row.adjusted_micro_cny), 0n);
    expect(BigInt(answer.totals.rows)).toBe(BigInt(records.items.length));
    expect(BigInt(answer.totals.charged_micro_cny)).toBe(baseCharge + adjustment);
    expect(BigInt(answer.totals.adjusted_micro_cny)).toBe(adjustment);
    await test.info().attach("real-profit-money-oracle", { body: JSON.stringify({ totals: answer.totals, baseCharge: String(baseCharge), adjustment: String(adjustment) }), contentType: "application/json" });
    await screenshot(page, `profit-summary-real-all-time-${width}`);
  });
}

test("profit summary accepts a period longer than 92 days and the legacy totals URL", async ({ page }) => {
  await login(page);
  const all = await read<Answer>(page, api);
  const records = await read<{ items: Charge[]; next_before: number | null }>(page, "/admin/pricing/charges?limit=200");
  expect(records.next_before).toBeNull(); expect(records.items.length).toBeGreaterThan(0);
  const times = records.items.map(row => Date.parse(row.occurred_at));
  const from = new Date(Math.min(...times) - 100 * 86_400_000).toISOString();
  const to = new Date(Math.max(...times) + 100 * 86_400_000).toISOString();
  expect(Date.parse(to) - Date.parse(from)).toBeGreaterThan(92 * 86_400_000);
  const query = new URLSearchParams({ from, to });
  const reply = page.waitForResponse(r => new URL(r.url()).pathname === api && new URL(r.url()).searchParams.get("from") === from && new URL(r.url()).searchParams.get("to") === to);
  await page.goto(base + "/#totals?" + query);
  const response = await reply; expect(response.status()).toBe(200);
  const answer: Answer = await response.json();
  expect(answer.totals).toEqual(all.totals);
  await assertCards(page, answer);
  await expect(page.getByRole("button", { name: "指定时间段", exact: true })).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByLabel("开始时间（UTC+8）", { exact: true })).toBeEnabled();
  await screenshot(page, "profit-summary-real-long-window-legacy-entry");
});
