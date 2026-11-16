import { expect, test, type Page } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 只读取已恢复的真实隔离世界，不创建业务数据或触发模型调用。
const sessionPath = resolve(process.env.CONSOLE_TEMPLATE_QA_SESSION!);
const state = JSON.parse(readFileSync(join(sessionPath, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(sessionPath, "local-auth.json"), "utf8"));
const baseUrl = process.env.CONSOLE_TEMPLATE_TENANT_URL ?? state.tenantUrl;
const output = resolve(process.env.CONSOLE_TEMPLATE_QA_OUTPUT!);
if (!state.ready || state.mockUpstream !== false) throw new Error("A ready real API world is required");
if (!["localhost", "127.0.0.1"].includes(new URL(baseUrl).hostname)) throw new Error("Loopback UI only");
const errors = new WeakMap<Page, string[]>();
const writes = new WeakMap<Page, string[]>();
const authenticated = new WeakMap<Page, boolean>();

test.beforeEach(async ({ page }) => {
  const failures: string[] = [];
  const changes: string[] = [];
  errors.set(page, failures);
  writes.set(page, changes);
  authenticated.set(page, false);
  page.on("console", (message) => {
    if (message.type() !== "error") return;
    const bootstrap = !authenticated.get(page) && /401|Unauthorized/.test(message.text()) && /\/(?:api\/me|api\/session)(?:\?|$)/.test(message.location().url);
    if (!bootstrap) failures.push(`console: ${message.text()}`);
  });
  page.on("response", (response) => {
    const path = new URL(response.url()).pathname;
    if (response.status() < 400 || !/^\/api\//.test(path)) return;
    if (response.status() === 401 && !authenticated.get(page) && ["/api/me", "/api/session"].includes(path)) return;
    failures.push(`HTTP ${response.status()} ${path}`);
  });
  page.on("pageerror", (error) => failures.push(error.message));
  page.on("request", (request) => {
    const url = new URL(request.url());
    if (!/^\/api\//.test(url.pathname) || ["GET", "HEAD"].includes(request.method())) return;
    if (!/^\/api\/(?:login|logout)$/.test(url.pathname)) changes.push(`${request.method()} ${url.pathname}`);
  });
});
test.afterEach(async ({ page }) => {
  expect(errors.get(page), "no runtime errors").toEqual([]);
  expect(writes.get(page), "no business writes or paid inference").toEqual([]);
});

async function settle(page: Page): Promise<void> {
  await page.waitForLoadState("networkidle");
  await page.evaluate(() => document.fonts.ready);
  await expect(page.getByRole("status").filter({ hasText: "载入中…" })).toHaveCount(0);
}
async function login(page: Page, who = auth.metricsOwner): Promise<void> {
  await page.goto(baseUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  const loggedIn = page.waitForResponse((response) => new URL(response.url()).pathname === "/api/login" && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await loggedIn).status()).toBe(200);
  authenticated.set(page, true);
  await settle(page);
}
async function navigate(page: Page, hash: string): Promise<void> {
  await page.goto(`${baseUrl}/#${hash}`);
  await settle(page);
}
async function screenshot(page: Page, name: string): Promise<void> {
  mkdirSync(join(output, "screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "screenshots", name + ".png"), fullPage: true });
}
async function noPageOverflow(page: Page): Promise<void> {
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), "only table regions may scroll horizontally").toBe(true);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
  await expect(page.locator("main h1")).toHaveCount(1);
}

for (const width of [1440, 390]) {
  test(`template surfaces and navigation at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    await page.goto(baseUrl);
    await settle(page);
    await expect(page.getByRole("heading", { name: "组织门户", exact: true })).toBeVisible();
    await screenshot(page, `tenant-login-${width}`);
    await login(page);
    for (const [route, title] of ([["org", "概览"], ["usage", "用量分析"], ["calls", "调用记录"], ["keys", "API 密钥"], ["prices", "模型与价格"], ["budgets", /^预算(?:与额度)?$/], ["billing", /^账务(?:账户)?$/], ["members", "成员"], ["account", "账号设置"]] satisfies [string, string | RegExp][])) {
      await navigate(page, route!);
      await expect(page.getByRole("heading", { name: title, exact: true, level: 1 })).toBeVisible();
      if (route === "org" && width === 390) {
        const analysisHref = await page.getByRole("link", { name: "查看用量分析", exact: true }).getAttribute("href");
        expect(analysisHref).toContain("?");
        const oracle = await page.evaluate(async (query) => {
          const response = await fetch("/api/usage/metrics?" + query, { credentials: "include" });
          if (response.status !== 200) throw new Error(`Money oracle status ${response.status}`);
          return response.json();
        }, analysisHref!.split("?")[1]!);
        expect(Object.hasOwn(oracle.totals, "charged_micro_cny")).toBe(true);
        expect(typeof oracle.totals.charged_micro_cny).toBe("string");
        const value = page.locator('[data-metric-kpi="cost"] .metric');
        await expect(value).toHaveText(exactCny(oracle.totals.charged_micro_cny));
        const geometry = await value.evaluate((element) => {
          const range = document.createRange(); range.selectNodeContents(element);
          return { lines: new Set([...range.getClientRects()].filter((rect) => rect.width > 0).map((rect) => Math.round(rect.top))).size, scroll: element.scrollWidth, client: element.clientWidth };
        });
        expect(geometry.lines, "ordinary exact money KPI fits on one line").toBe(1);
        expect(geometry.scroll, "money stays inside the KPI card").toBeLessThanOrEqual(geometry.client);
      }
      await noPageOverflow(page);
      if (route === "billing") {
        const control = page.locator('[data-tone="ink"], [data-tone="inverse"]').getByRole("combobox").first();
        const colors = await control.evaluate((element) => ({ foreground: getComputedStyle(element.querySelector(".ui-select-value") ?? element).color, background: getComputedStyle(element).backgroundColor }));
        expect(textContrast(colors.foreground, colors.background), "actual selected text stays readable on the ink-card field").toBeGreaterThanOrEqual(4.5);
      }
      await screenshot(page, `tenant-${route}-${width}`);
    }
    const look = await page.evaluate(() => ({ background: getComputedStyle(document.body).backgroundColor, font: getComputedStyle(document.body).fontFamily, weight: getComputedStyle(document.querySelector("main h1")!).fontWeight }));
    expect(look.background).toBe("rgb(243, 242, 238)");
    expect(look.font).toContain("Manrope");
    expect(Number(look.weight)).toBeGreaterThanOrEqual(700);
    if (width === 1440) {
      await page.getByRole("button", { name: "收起侧边栏", exact: true }).click();
      await expect(page.getByRole("button", { name: "展开侧边栏", exact: true })).toHaveAttribute("aria-expanded", "false");
      await screenshot(page, "tenant-sidebar-collapsed-1440");
      await page.reload(); await settle(page);
      await expect(page.getByRole("button", { name: "展开侧边栏", exact: true })).toBeVisible();
      await page.getByRole("button", { name: "展开侧边栏", exact: true }).click();
      await expect(page.getByRole("link", { name: "API 密钥", exact: true })).toBeVisible();
    } else {
      const trigger = page.getByRole("button", { name: "打开菜单", exact: true });
      await trigger.click();
      const menu = page.getByRole("dialog", { name: "组织门户导航", exact: true });
      await expect(menu).toBeVisible();
      await screenshot(page, "tenant-sidebar-drawer-390");
      await page.keyboard.press("Escape"); await expect(menu).toHaveCount(0); await expect(trigger).toBeFocused();
      await trigger.click(); await menu.getByRole("link", { name: "API 密钥", exact: true }).click();
      await expect(menu).toHaveCount(0); await expect(page.getByRole("heading", { name: "API 密钥", exact: true })).toBeVisible();
    }
  });
}

test("key and member dialogs retain complete actions without saving", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  await login(page);
  await navigate(page, "keys");
  await page.getByRole("button", { name: "新建密钥", exact: true }).click();
  const keyDialog = page.getByRole("dialog", { name: "新建密钥", exact: true });
  await expect(keyDialog).toBeVisible();
  await expect(keyDialog.getByRole("textbox", { name: "名称", exact: true })).toBeVisible();
  await screenshot(page, "tenant-create-key-1440");
  await page.keyboard.press("Escape"); await expect(keyDialog).toHaveCount(0);
  await navigate(page, "members");
  const table = page.getByRole("table", { name: "组织成员", exact: true });
  await expect(table.locator("tbody tr").first()).toBeVisible();
  await expect(page.getByRole("button", { name: "创建账号", exact: true })).toBeVisible();
});

const OUTPUT_TPS_TITLE = /^输出 Token（含推理，不含输入）÷ 完整上游耗时(?:；.+)?$/;

test("real call records retain tokens, TTFT, output TPS and right drawer", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  await login(page, auth.metricsOwner);
  await navigate(page, "calls");
  const table = page.getByRole("table", { name: "调用记录", exact: true });
  const calls = await page.evaluate(async () => {
    const response = await fetch("/api/usage/calls?limit=25", { credentials: "include" });
    if (!response.ok) throw new Error(`Call oracle status ${response.status}`);
    return (await response.json()).items as { request_id: string }[];
  });
  expect(calls.length, "real retained calls are present").toBeGreaterThan(0);
  await expect(table.locator("tbody tr")).toHaveCount(calls.length);
  const requestCounts = new Map<string, number>();
  for (const call of calls) {
    expect(typeof call.request_id).toBe("string");
    expect(call.request_id.length).toBeGreaterThan(0);
    requestCounts.set(call.request_id, (requestCounts.get(call.request_id) ?? 0) + 1);
  }
  for (const [requestId, count] of requestCounts) {
    await expect(table.getByRole("row").filter({ hasText: requestId })).toHaveCount(count);
  }
  await expect(table.getByRole("columnheader")).toContainText(["时间", "模型"]);
  const tpsLabels = table.locator("dt").and(table.getByText("输出 TPS", { exact: true }));
  await expect(tpsLabels).toHaveCount(calls.length);
  for (const label of await tpsLabels.all()) await expect(label.locator("xpath=..")).toHaveAttribute("title", OUTPUT_TPS_TITLE);
  const details = table.getByRole("button", { name: /详情|查看/ }).first();
  await details.click();
  const drawer = page.getByRole("dialog"); await expect(drawer).toBeVisible();
  const drawerTps = drawer.locator("dt").and(drawer.getByText("输出 TPS", { exact: true }));
  await expect(drawerTps).toHaveCount(1);
  await expect(drawerTps.locator("xpath=..")).toHaveAttribute("title", OUTPUT_TPS_TITLE);
  await expect.poll(async () => {
    const box = await drawer.boundingBox();
    return box ? Math.round(box.x + box.width) : null;
  }, { message: "drawer settles inside the 16px viewport gutter" }).toBe(1440 - 16);
  await screenshot(page, "tenant-call-details-1440");
  await page.keyboard.press("Escape"); await expect(drawer).toHaveCount(0);
});

// 价格原值直接来自真实公开报价；断言不调用产品 formatter，也不舍入成浮点数。
function publishedAmount(value: string, perThousand: boolean): string {
  if (!/^-?\d+(?:\.\d+)?$/.test(value)) throw new Error("Published exact decimal required");
  const negative = value.startsWith("-");
  const [integer, fraction = ""] = value.replace(/^-/, "").split(".");
  const scale = fraction.length + (perThousand ? 3 : 0);
  const digits = BigInt(integer! + fraction).toString().padStart(scale + 1, "0");
  const whole = scale ? digits.slice(0, -scale) : digits;
  const tail = (scale ? digits.slice(-scale) : "").replace(/0+$/, "").padEnd(2, "0");
  return `${negative ? "-" : ""}${whole}.${tail}`;
}

test("published USD and CNY prices, multiplier and missing values stay exact", async ({ page }) => {
  await page.setViewportSize({ width: 1440, height: 1000 });
  await login(page);
  await navigate(page, "prices");
  const quote = await page.evaluate(async () => {
    const response = await fetch("/api/prices", { credentials: "include" });
    if (!response.ok) throw new Error(`Price oracle status ${response.status}`);
    return response.json();
  });
  expect(Object.hasOwn(quote, "base_models")).toBe(true);
  expect(quote.base_models.length).toBeGreaterThan(0);
  const model = quote.base_models[0];
  const group = quote.groups.find((item: any) => item.models.some((row: any) => row.model_id === model.model_id));
  expect(group).toBeTruthy();
  const customerModel = group.models.find((item: any) => item.model_id === model.model_id);
  for (const field of ["cny_per_usd", "effective_multiplier", "prices_by_tier"]) expect(Object.hasOwn(customerModel, field), field).toBe(true);
  const labels: Record<string, string> = { input: "输入", output: "输出", reasoning: "推理", cache_read: "缓存读", cache_write: "缓存写", cache_write_1h: "1 小时缓存写", audio_input: "音频输入", audio_output: "音频输出", image_input: "图像输入", image_output: "图像输出", web_search: "联网搜索" };
  await page.getByRole("button", { name: new RegExp(model.name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")) }).first().click();
  const dialog = page.getByRole("dialog"); await expect(dialog).toBeVisible();
  await expect.poll(async () => { const box = await dialog.boundingBox(); return box ? Math.round(box.x + box.width) : null; }).toBe(1440);
  async function checkRates(rates: any[], field: string, currency: "$" | "¥"): Promise<void> {
    expect(rates.length).toBeGreaterThan(0);
    for (const rate of rates) {
      expect(Object.hasOwn(rate, field), field).toBe(true);
      expect(labels[rate.dimension]).toBeTruthy();
      const row = dialog.getByRole("row").filter({ has: page.getByRole("rowheader", { name: labels[rate.dimension], exact: true }) });
      await expect(row).toHaveCount(1);
      if (rate[field] === null) {
        await expect(row.getByText("未提供价格", { exact: true })).toBeVisible();
        await expect(row.getByText(`${currency}0.00`, { exact: true })).toHaveCount(0);
      } else {
        expect(typeof rate[field]).toBe("string");
        await expect(row.getByText(currency + publishedAmount(rate[field], rate.unit === "request"), { exact: true })).toBeVisible();
      }
    }
  }
  const tierKeys = Object.keys(model.prices_by_tier);
  expect(tierKeys.length).toBeGreaterThan(0);
  const tier = tierKeys[0]!;
  expect(Object.hasOwn(customerModel.prices_by_tier, tier)).toBe(true);
  await checkRates(model.prices_by_tier[tier].base, "price_usd_per_million", "$");
  await screenshot(page, "tenant-price-detail-usd-1440");
  await dialog.getByRole("button", { name: new RegExp(group.display_name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")) }).click();
  await checkRates(customerModel.prices_by_tier[tier].base, "price_cny_per_million", "¥");
  await expect(dialog.getByText(customerModel.effective_multiplier.replace(/\.0+$/, "") + "×", { exact: true }).first()).toBeVisible();
  await screenshot(page, "tenant-price-detail-cny-1440");
  await page.setViewportSize({ width: 390, height: 1000 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await screenshot(page, "tenant-price-detail-cny-390");
  await dialog.getByRole("rowheader", { name: "联网搜索", exact: true }).scrollIntoViewIfNeeded();
  await expect(dialog.getByRole("rowheader", { name: "联网搜索", exact: true })).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  await screenshot(page, "tenant-price-detail-cny-bottom-390");
  await page.keyboard.press("Escape"); await expect(dialog).toHaveCount(0);
});


function exactCny(micro: string): string {
  if (!/^-?\d+$/.test(micro)) throw new Error("An exact integer micro-CNY oracle is required");
  const amount = BigInt(micro); const absolute = amount < 0n ? -amount : amount;
  const whole = (absolute / 1_000_000n).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  const fraction = (absolute % 1_000_000n).toString().padStart(6, "0").replace(/0+$/, "").padEnd(2, "0");
  return `¥${amount < 0n ? "-" : ""}${whole}.${fraction}`;
}


function textContrast(foreground: string, background: string): number {
  const luminance = (value: string): number => {
    const channels = value.match(/[\d.]+/g)?.slice(0, 3);
    if (!channels || channels.length !== 3) throw new Error("Expected computed RGB colors");
    const linear = channels.map((channel) => { const normalized = Number(channel) / 255; return normalized <= .04045 ? normalized / 12.92 : ((normalized + .055) / 1.055) ** 2.4; });
    return .2126 * linear[0]! + .7152 * linear[1]! + .0722 * linear[2]!;
  };
  const first = luminance(foreground); const second = luminance(background);
  return (Math.max(first, second) + .05) / (Math.min(first, second) + .05);
}
