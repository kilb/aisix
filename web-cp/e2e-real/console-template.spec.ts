import { expect, test, type Locator, type Page, type Response } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 期望来自导航合同、模板视觉和真实 API；不导入产品组件或 formatter，不替换响应。
interface World { ready: boolean; mockUpstream: boolean; realBackend?: boolean; adminUrl: string; tokenUiFixture?: { providerAccountId?: string } }
interface Operator { email: string; password: string }
const session = resolve(process.env.CONSOLE_TEMPLATE_QA_SESSION!);
const world = JSON.parse(readFileSync(join(session, "public.json"), "utf8")) as World;
const { operator } = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8")) as { operator: Operator };
const baseUrl = process.env.CONSOLE_TEMPLATE_ADMIN_URL ?? world.adminUrl;
const base = new URL(baseUrl);
const output = resolve(process.env.CONSOLE_TEMPLATE_QA_OUTPUT!);
if (world.ready !== true || world.mockUpstream !== false || world.realBackend === false) throw new Error("Console template QA requires a ready real API world");
if (!["127.0.0.1", "localhost"].includes(base.hostname)) throw new Error("Console template QA only runs on an isolated loopback UI");
if (!operator?.email || !operator.password) throw new Error("The real operator login is required");
const routes = [
  "overview", "analytics", "calls", "upstream-monitoring", "providers", "models", "public-routes", "model-tests", "cache",
  "tenants", "pricing", "billing", "keys", "gateways", "resources", "operators", "audit", "account",
  ...["groups", "plans", "cost", "catalogue", "status", "baselines", "imports", "library"].map(section => `pricing?section=${section}`),
  ...["accounts", "topups", "repricing", "reconciliation"].map(section => `billing?section=${section}`),
];
const bootstrapPaths = new Set(["/admin/me", "/admin/session"]);
const callPaths = new Set(["/admin/usage/calls", "/admin/charges", "/admin/pricing/charges"]);
interface Health {
  authenticated: boolean;
  runtime: string[];
  console: { type: string; text: string; location: string }[];
  http: string[];
  forbidden: string[];
  requests: { method: string; path: string }[];
}
const health = new WeakMap<Page, Health>();
test.beforeEach(async ({ page }) => {
  const ledger: Health = { authenticated: false, runtime: [], console: [], http: [], forbidden: [], requests: [] };
  health.set(page, ledger);
  page.on("pageerror", error => ledger.runtime.push(error.message));
  page.on("console", message => {
    if (!["warning", "error"].includes(message.type())) return;
    const location = message.location().url;
    const path = /^https?:/.test(location) ? new URL(location).pathname : "";
    const bootstrap = !ledger.authenticated && message.text().includes("401") && bootstrapPaths.has(path);
    ledger.console.push({ type: message.type(), text: message.text(), location: path });
    if (message.type() === "error" && !bootstrap) ledger.runtime.push(message.text());
  });
  page.on("response", response => {
    const url = new URL(response.url());
    if (response.status() < 400 || url.origin !== base.origin) return;
    if (response.status() === 401 && !ledger.authenticated && bootstrapPaths.has(url.pathname)) return;
    ledger.http.push(`HTTP ${response.status()} ${url.pathname}`);
  });
  page.on("request", request => {
    const url = new URL(request.url());
    const method = request.method();
    ledger.requests.push({ method, path: url.pathname });
    if (/^\/(?:v1|v1beta|v2)(?:\/|$)|\/(?:upstream-models|probe|probes)(?:\/|$)/.test(url.pathname)) ledger.forbidden.push(`inference/probe ${method} ${url.pathname}`);
    if (!["GET", "HEAD"].includes(method) && !(url.origin === base.origin && /^\/admin\/(?:login|logout)$/.test(url.pathname))) ledger.forbidden.push(`write ${method} ${url.pathname}`);
  });
});
test.afterEach(async ({ page }) => {
  await test.info().attach("console-template-health-and-request-ledger", { body: JSON.stringify(health.get(page)), contentType: "application/json" });
  expect.soft(health.get(page)!.runtime, "no runtime or unexpected console errors").toEqual([]);
  expect.soft(health.get(page)!.http, "no unexpected HTTP failures after login").toEqual([]);
  expect(health.get(page)!.forbidden, "inspection must not issue configuration writes, model calls or probes").toEqual([]);
});

async function settle(page: Page): Promise<void> {
  // 先等 React 更新和懒加载产生可见标题，避免 networkidle 抢在登录更新之前通过。
  await expect(page.locator("main h1:visible")).toHaveCount(1);
  await page.waitForLoadState("networkidle");
  await page.evaluate(async () => {
    await document.fonts.ready;
    await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
  });
  await expect(page.getByText("载入中…", { exact: true })).toHaveCount(0);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
}
async function capture(page: Page, name: string): Promise<void> {
  mkdirSync(join(output, "screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "screenshots", `${name.replace(/[^a-z0-9-]+/gi, "-")}.png`), fullPage: true, animations: "disabled" });
}
async function login(page: Page): Promise<void> {
  await page.goto(baseUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(operator.email);
  await page.getByLabel("口令", { exact: true }).fill(operator.password);
  const result = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login" && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await result).status(), "real operator authentication succeeds").toBe(200);
  health.get(page)!.authenticated = true;
  await settle(page);
}
async function navigate(page: Page, route: string): Promise<void> {
  await page.goto(`${baseUrl.replace(/\/$/, "")}/#${route}`);
  await settle(page);
  expect(new URL(page.url()).hash, "the requested route and section are retained").toBe(`#${route}`);
  await expect(page.locator("main h1:visible")).not.toHaveText("");
}

// 宽于视口的内容、表格与卡片并排，或长文本被挤成逐字竖排，都会触发检查。
async function checkPage(page: Page, label: string): Promise<void> {
  await expect(page.locator("main h1:visible"), "one visible page title").toHaveCount(1);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
  const layout = await page.evaluate(() => {
    const visible = (element: Element) => {
      const box = element.getBoundingClientRect();
      for (let parent: Element | null = element; parent; parent = parent.parentElement) {
        const style = getComputedStyle(parent);
        if (style.display === "none" || style.visibility === "hidden" || style.clip === "rect(0px, 0px, 0px, 0px)" || style.clipPath === "inset(50%)") return false;
      }
      return box.width > 0 && box.height > 0;
    };
    const bounds = (element: Element) => { const box = element.getBoundingClientRect(); return { left: box.left, top: box.top, right: box.right, bottom: box.bottom, width: box.width }; };
    const panels = [...document.querySelectorAll("main .panel")].filter(visible);
    const parallelTables = panels.flatMap((a, index) => panels.slice(index + 1).flatMap(b => {
      if (a.contains(b) || b.contains(a) || !([a, b].some(panel => [...panel.querySelectorAll("table")].some(visible)))) return [];
      const ar = bounds(a), br = bounds(b);
      return Math.min(ar.bottom, br.bottom) - Math.max(ar.top, br.top) > 4 && (ar.right <= br.left + 1 || br.right <= ar.left + 1)
        ? [{ a: a.getAttribute("aria-label"), b: b.getAttribute("aria-label"), ar, br }] : [];
    }));
    const crampedCells = [...document.querySelectorAll("main table td")].filter(visible).flatMap(cell => {
      if (getComputedStyle(cell).display !== "table-cell") return [];
      const box = bounds(cell), text = cell.textContent?.trim() ?? "";
      return text.length > 40 && /[\p{L}]/u.test(text) && box.width < 120 ? [{ text: text.slice(0, 100), width: box.width }] : [];
    });
    return { viewport: document.documentElement.clientWidth, document: document.documentElement.scrollWidth, body: document.body.scrollWidth, parallelTables, crampedCells, title: document.querySelector("main h1")?.textContent };
  });
  await test.info().attach(`layout-${label}`, { body: JSON.stringify(layout), contentType: "application/json" });
  expect(layout.document, `${label}: only table regions may scroll horizontally`).toBeLessThanOrEqual(layout.viewport + 1);
  expect(layout.body, `${label}: the body stays inside the viewport`).toBeLessThanOrEqual(layout.viewport + 1);
  expect(layout.parallelTables, `${label}: tables occupy their own row`).toEqual([]);
  expect(layout.crampedCells, `${label}: long cells remain readable`).toEqual([]);
  const fee = page.locator('[data-metric-kpi="cost"] .metric');
  if (await fee.count()) {
    const measure = (element: Element) => {
      const range = document.createRange(); range.selectNodeContents(element);
      const lines = [...new Set([...range.getClientRects()].filter(box => box.width > 0).map(box => Math.round(box.top)))];
      const box = element.getBoundingClientRect(), card = element.closest("section")!.getBoundingClientRect();
      return { lines, left: box.left, right: box.right, cardLeft: card.left, cardRight: card.right, width: element.clientWidth, scroll: element.scrollWidth };
    };
    const amount = await fee.evaluate(measure);
    expect(amount.lines, `${label}: exact currency stays on one readable line`).toHaveLength(1);
    expect(amount.left).toBeGreaterThanOrEqual(amount.cardLeft); expect(amount.right).toBeLessThanOrEqual(amount.cardRight);
    if (layout.viewport === 390) {
      const previous = await fee.getAttribute("style");
      await fee.evaluate(element => { element.style.fontSize = "30px"; element.style.whiteSpace = "normal"; element.style.wordBreak = "break-all"; });
      const old = await fee.evaluate(measure);
      if (previous === null) await fee.evaluate(element => element.removeAttribute("style")); else await fee.evaluate((element, style) => element.setAttribute("style", style), previous);
      expect(() => expect(old.lines, "the previous large wrapping currency must be rejected").toHaveLength(1)).toThrow();
    }
  }
}
async function checkTemplate(page: Page): Promise<void> {
  const look = await page.evaluate(() => ({
    background: getComputedStyle(document.body).backgroundColor,
    font: getComputedStyle(document.body).fontFamily,
    titleWeight: getComputedStyle(document.querySelector("main h1")!).fontWeight,
    fonts: [...document.fonts].filter(font => font.status === "loaded").map(font => font.family),
    tones: [...document.querySelectorAll<HTMLElement>("main [data-tone]")].filter(element => { const box = element.getBoundingClientRect(); return box.width > 0 && box.height > 0; }).map(element => ({ tone: element.dataset.tone!, background: (() => {
      for (let painted: Element | null = element; painted; painted = painted.parentElement) {
        const color = getComputedStyle(painted).backgroundColor;
        if (color !== "rgba(0, 0, 0, 0)" && color !== "transparent") return color;
      }
      return "transparent";
    })() })),
  }));
  expect(look.background).toBe("rgb(243, 242, 238)");
  expect(look.font).toContain("Manrope"); expect(look.font).toContain("Noto Sans SC");
  expect(look.fonts.some(font => font.includes("Manrope")), "the intended Latin font is actually loaded").toBe(true);
  expect(look.fonts.some(font => font.includes("Noto Sans SC")), "the intended Chinese font is actually loaded").toBe(true);
  expect(Number(look.titleWeight)).toBeGreaterThanOrEqual(700);
  for (const card of look.tones.filter(card => ["surface", "default"].includes(card.tone))) expect(card.background).toBe("rgb(255, 255, 255)");
  for (const [tones, color] of [[["ink", "inverse"], "rgb(17, 17, 16)"], [["lilac"], "rgb(227, 208, 255)"], [["lime"], "rgb(213, 249, 95)"]] as const) {
    const cards = look.tones.filter(card => (tones as readonly string[]).includes(card.tone));
    expect(cards.length, `at most one ${tones.join("/")} card on a page`).toBeLessThanOrEqual(1);
    for (const card of cards) expect(card.background).toBe(color);
  }
}
for (const width of [1440, 390]) {
  test(`all console routes and template navigation at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 }); await login(page);
    for (const route of routes) {
      await navigate(page, route); await checkPage(page, `${route}-${width}`); await checkTemplate(page); await capture(page, `admin-${route}-${width}`);
    }
    if (width === 1440) {
      const main = page.locator("main"), expandedWidth = (await main.boundingBox())!.width;
      await page.getByRole("button", { name: "收起侧边栏", exact: true }).click();
      await expect(page.getByRole("button", { name: "展开侧边栏", exact: true })).toHaveAttribute("aria-expanded", "false");
      await expect.poll(async () => (await main.boundingBox())!.width, { message: "collapsing the rail gives the page usable width" }).toBeGreaterThan(expandedWidth + 80);
      await expect.poll(async () => (await page.locator(".shell-sidebar").boundingBox())!.width, { message: "the rail reaches its 68px collapsed geometry" }).toBe(68);
      const collapsedWidth = (await main.boundingBox())!.width;
      await capture(page, "admin-sidebar-collapsed-1440"); await page.reload(); await settle(page);
      await expect(page.getByRole("button", { name: "展开侧边栏", exact: true })).toBeVisible();
      expect(Math.abs((await main.boundingBox())!.width - collapsedWidth), "collapsed layout survives reload").toBeLessThanOrEqual(2);
      await page.getByRole("button", { name: "展开侧边栏", exact: true }).click();
      await expect.poll(async () => Math.abs((await main.boundingBox())!.width - expandedWidth)).toBeLessThanOrEqual(2);
    } else {
      const trigger = page.getByRole("button", { name: "打开菜单", exact: true }); await trigger.click();
      const drawer = page.getByRole("dialog", { name: "运维导航", exact: true }); await expect(drawer).toBeVisible();
      await capture(page, "admin-navigation-drawer-390"); await page.keyboard.press("Escape");
      await expect(drawer).toHaveCount(0); await expect(trigger).toBeFocused(); await trigger.click();
      await drawer.getByRole("link", { name: "调用记录", exact: true }).click(); await expect(drawer).toHaveCount(0); await settle(page);
      expect(new URL(page.url()).hash).toBe("#calls"); await checkPage(page, "drawer-navigation-calls-390");
    }
  });
}

const OUTPUT_TPS_TITLE = /^输出 Token（含推理，不含输入）÷ 完整上游耗时(?:；.+)?$/;

interface Call extends Record<string, unknown> {
  event_id: string; error_message: string | null; first_token_latency_ms: number | null; throughput_tps: number | null; usage_estimated: boolean | null;
  charge_micro_cny: number | string | null; cost_micro_cny: number | string | null;
  io_audit: { version: number; boundary: string; exchange_count?: number | null; input_text?: { utf8_bytes: number; complete: boolean } | null; output_text?: { utf8_bytes: number; complete: boolean } | null } | null;
  io_token_reference: { method: string; input_tokens?: number | null; output_tokens?: number | null } | null;
}
function isCallList(response: Response, eventId?: string): boolean {
  const url = new URL(response.url());
  return response.request().method() === "GET" && callPaths.has(url.pathname) && (eventId === undefined ? !url.searchParams.has("event_id") : url.searchParams.get("event_id") === eventId);
}
async function readCalls(page: Page, path: string): Promise<Call[]> {
  expect(callPaths.has(new URL(path, baseUrl).pathname)).toBe(true);
  const result = await page.evaluate(async path => {
    const response = await fetch(path, { credentials: "include" });
    return { status: response.status, body: await response.json() as { items: Call[] } };
  }, path);
  expect(result.status, "the independent real GET oracle succeeds").toBe(200); expect(Array.isArray(result.body.items)).toBe(true);
  expect(result.body.items.length, "use retained real calls rather than an empty or fabricated scenario").toBeGreaterThan(0);
  return result.body.items;
}
function field(scope: Locator, label: string): Locator {
  return scope.locator("dt").and(scope.getByText(label, { exact: true })).locator("xpath=following-sibling::dd[1]");
}
function checkMoney(text: string, amount: number | string | null): void {
  if (amount === null) { expect(text.trim()).toBe("—"); return; }
  if (typeof amount === "number") expect(Number.isSafeInteger(amount), "the oracle amount must be exact").toBe(true);
  const normalized = text.trim().replace(/−/g, "-").replace(/￥/g, "¥").replace(/^¥-/, "-¥");
  const match = /^(-?)¥([\d,]+)(?:\.(\d{1,6}))?$/.exec(normalized);
  expect(match, "money retains its currency and full decimal amount").not.toBeNull();
  const value = BigInt(match![2]!.replace(/,/g, "")) * 1_000_000n + BigInt((match![3] ?? "").padEnd(6, "0"));
  expect(match![1] ? -value : value, "displayed CNY stays exact to a micro-CNY").toBe(BigInt(amount));
}
function estimate(call: Call): string {
  const audit = call.io_audit;
  const supported = audit && (audit.version === 1 && audit.boundary === "client_exchange" || audit.version === 2 && audit.boundary === "upstream_attempt");
  if (!supported) return `按字节估算 Token：${audit === null ? "未采集文本长度" : "统计口径暂不支持"}`;
  if (audit.version === 2 && (audit.exchange_count ?? 1) > 1) return "按字节估算 Token：多次上游交换，无法单次估算";
  const reference = call.io_token_reference;
  if (reference !== null && reference.method !== "utf8_bytes_div4_v1") return "按字节估算 Token：估算方法暂不支持";
  const value = (tokens: number | null | undefined, text: { utf8_bytes: number; complete: boolean } | null | undefined) => {
    if (text == null) return "不可估算（未采集文本）"; if (tokens == null) return "不可估算（参考未记录）";
    expect(tokens, "the byte reference remains a token count").toBe(Math.ceil(text.utf8_bytes / 4));
    return `≈ ${new Intl.NumberFormat("en-US").format(tokens)} Token${text.complete ? "" : "（部分）"}`;
  };
  return `按字节估算 Token：输入 ${value(reference?.input_tokens, audit.input_text)} / 输出 ${value(reference?.output_tokens, audit.output_text)}`;
}
async function checkDuration(value: Locator, milliseconds: number | null): Promise<void> {
  if (milliseconds === null) { await expect(value).toHaveText(/^(?:—|未记录)$/); return; }
  expect(Number.isFinite(milliseconds)).toBe(true);
  const match = /^(\d+(?:\.\d+)?)\s*(ms|s)$/.exec((await value.innerText()).replace(/,/g, "").trim());
  expect(match, "a measured duration keeps its numeric value and unit").not.toBeNull();
  const scale = milliseconds >= 1000 ? 1000 : 1;
  expect(match![2], "duration units preserve milliseconds below one second").toBe(scale === 1000 ? "s" : "ms");
  const rounded = Number(new Intl.NumberFormat("en-US", { useGrouping: false, maximumFractionDigits: 2 }).format(milliseconds / scale));
  expect(Number(match![1]), "a coarse zero or whole second cannot hide a recorded duration").toBe(rounded);
}
async function checkThroughput(value: Locator, call: Call): Promise<void> {
  if (call.throughput_tps === null || call.throughput_tps <= 0) { await expect(value).toHaveText("—"); return; }
  expect(Number.isFinite(call.throughput_tps)).toBe(true);
  await expect(value).toHaveText(`${call.usage_estimated === true ? "≈ " : ""}${new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 }).format(call.throughput_tps)}`);
}
// 用户在宽表右端展开，明细必须立即出现在当前视口，不能靠滚回最左侧才能阅读。
async function checkNarrowExpansion(page: Page, eventId: string): Promise<void> {
  const table = page.getByRole("region", { name: "计费明细表格滚动区域", exact: true });
  const expand = page.getByRole("button", { name: `展开用量明细 ${eventId}`, exact: true });
  await expand.click();
  const quantities = page.getByRole("region", { name: `计费数量 ${eventId}`, exact: true });
  const heading = quantities.getByRole("heading", { name: "计费数量", exact: true });
  await expect(heading).toBeInViewport();
  const geometry = await quantities.locator(":scope > h4, :scope > p").evaluateAll(elements => elements.map(element => {
    const box = element.getBoundingClientRect();
    return { text: element.textContent, left: box.left, right: box.right, viewport: document.documentElement.clientWidth };
  }));
  expect(geometry.length, "the expanded quantities have readable headings or explanation").toBeGreaterThan(0);
  for (const box of geometry) {
    expect(box.left, `${box.text}: expanded detail starts inside the viewport`).toBeGreaterThanOrEqual(-1);
    expect(box.right, `${box.text}: expanded detail ends inside the viewport`).toBeLessThanOrEqual(box.viewport + 1);
  }
  const scroll = await table.evaluate(element => {
    const before = element.scrollLeft;
    element.scrollLeft = 0;
    const left = element.scrollLeft;
    element.scrollLeft = before;
    return { before, left, after: element.scrollLeft, client: element.clientWidth, scroll: element.scrollWidth };
  });
  expect(scroll.scroll, "the wide table retains its horizontal scroll region").toBeGreaterThan(scroll.client);
  expect(scroll.before, "the user expanded from the right end of the table").toBeGreaterThan(0);
  expect(scroll.left).toBe(0);
  expect(scroll.after).toBe(scroll.before);
  // 暂时恢复普通静态定位，要求同一左边界断言转红；恢复样式后才继续截图。
  const staticLeft = await quantities.evaluate(section => {
    const expansion = section.parentElement!;
    const original = expansion.getAttribute("style");
    expansion.style.setProperty("position", "static", "important");
    const left = section.querySelector("h4")!.getBoundingClientRect().left;
    if (original === null) expansion.removeAttribute("style"); else expansion.setAttribute("style", original);
    return left;
  });
  expect(() => expect(staticLeft, "static detail must fail the same viewport boundary").toBeGreaterThanOrEqual(-1)).toThrow();
  await expect(heading).toBeInViewport();
  await test.info().attach("narrow-expanded-quantities", { body: JSON.stringify({ geometry, scroll, staticLeft }), contentType: "application/json" });
  await capture(page, "admin-calls-expanded-390");
  await page.getByRole("button", { name: `收起用量明细 ${eventId}`, exact: true }).click();
}
for (const width of [1440, 390]) {
  test(`retained calls preserve exact fees, timing, estimates and independent logs at ${width}px`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 }); await login(page);
    const listRead = page.waitForResponse(response => isCallList(response)); await navigate(page, "calls");
    const listAnswer = await listRead; expect(listAnswer.status()).toBe(200);
    const url = new URL(listAnswer.url()), records = await readCalls(page, url.pathname + url.search);
    for (const record of records) {
      for (const key of ["event_id", "first_token_latency_ms", "throughput_tps", "usage_estimated", "io_audit", "io_token_reference", "charge_micro_cny", "cost_micro_cny"]) expect(Object.hasOwn(record, key), `the real call explicitly reports ${key}; absence is not zero`).toBe(true);
      expect(typeof record.event_id).toBe("string"); expect(record.event_id.length).toBeGreaterThan(0);
    }
    const call = records.find(record => typeof record.error_message === "string" && record.error_message.length > 0);
    expect(call, "retained real failures must provide a complete error scenario").toBeDefined();
    const eventId = call!.event_id, filter = `calls?event_id=${encodeURIComponent(eventId)}`;
    const filteredRead = page.waitForResponse(response => isCallList(response, eventId)); await navigate(page, filter); expect((await filteredRead).status()).toBe(200);
    const link = page.getByRole("link", { name: `查看调用日志 ${eventId}`, exact: true }), row = page.getByRole("row").filter({ has: link });
    await expect(row).toHaveCount(1); await checkDuration(field(row, "首字"), call!.first_token_latency_ms); await checkThroughput(field(row, "输出 TPS"), call!);
    await expect(field(row, "输出 TPS").locator("xpath=..")).toHaveAttribute("title", OUTPUT_TPS_TITLE);
    await expect(row.getByText(estimate(call!), { exact: true })).toBeVisible();
    const fee = await row.locator("td").filter({ has: page.getByText("售价", { exact: true }) }).innerText();
    const split = fee.indexOf("成本"); expect(split).toBeGreaterThanOrEqual(0);
    checkMoney(fee.slice(fee.indexOf("售价") + 2, split), call!.charge_micro_cny); checkMoney(fee.slice(split + 2), call!.cost_micro_cny);
    await checkPage(page, `calls-oracle-${width}`); await capture(page, `admin-calls-oracle-${width}`);
    if (width === 390) await checkNarrowExpansion(page, eventId);
    const eventRead = page.waitForResponse(response => response.request().method() === "GET" && new URL(response.url()).pathname === `/admin/usage/events/${encodeURIComponent(eventId)}`);
    await link.click(); const storedAnswer = await eventRead; expect(storedAnswer.status(), "the selected real event remains stored").toBe(200);
    const stored = await storedAnswer.json() as { event_id: string; event: Record<string, unknown> };
    expect(Object.hasOwn(stored, "event_id")).toBe(true); expect(stored.event_id).toBe(eventId); await settle(page);
    const detailQuery = new URLSearchParams(new URL(page.url()).hash.split("?")[1]); expect(detailQuery.get("event_id")).toBe(eventId); expect(detailQuery.get("detail")).toBe(eventId);
    await expect(page.getByRole("table", { name: "计费明细", exact: true })).toBeHidden();
    const performance = page.getByRole("region", { name: "时间与性能", exact: true });
    await checkDuration(field(performance, "首 Token 延迟"), typeof stored.event.first_token_latency_ms === "number" ? stored.event.first_token_latency_ms : call!.first_token_latency_ms);
    await checkThroughput(field(performance, "输出 TPS"), call!);
    await expect(field(performance, "输出 TPS").locator("xpath=preceding-sibling::dt[1]")).toHaveAttribute("title", OUTPUT_TPS_TITLE);
    const message = stored.event.error_message ?? call!.error_message; expect(typeof message).toBe("string"); expect((message as string).length).toBeGreaterThan(0);
    await expect(page.getByRole("region", { name: "错误信息", exact: true }).locator("pre")).toHaveText(message as string);
    await expect(page.getByRole("button", { name: "复制错误信息", exact: true })).toBeVisible();
    const audit = page.locator('details[aria-label="输入输出与 Token 参考"]'); await expect(audit).toBeVisible(); await audit.locator("summary").click();
    if (call!.io_token_reference?.method === "utf8_bytes_div4_v1") {
      for (const [label, value] of [["输入 Token 参考", call!.io_token_reference.input_tokens], ["输出 Token 参考", call!.io_token_reference.output_tokens]] as const) {
        if (value != null) await expect(field(audit, label)).toContainText(`≈ ${new Intl.NumberFormat("en-US").format(value)} Token`);
      }
    } else await expect(audit).toContainText(/未采集|不可用|暂不支持|多次上游/);
    await checkPage(page, `call-log-oracle-${width}`); await capture(page, `admin-call-log-oracle-${width}`);
    const deepLink = page.url(); await page.reload(); await settle(page); expect(page.url()).toBe(deepLink);
    await expect(page.getByRole("region", { name: "错误信息", exact: true }).locator("pre")).toHaveText(message as string);
    await page.getByRole("link", { name: "返回调用记录", exact: true }).click(); await settle(page); expect(new URL(page.url()).hash).toBe(`#${filter}`);
    await expect(page.getByRole("link", { name: `查看调用日志 ${eventId}`, exact: true })).toBeVisible();
  });
}

// 编辑付费账号只读取现有配置并取消；清单不能在旁边占据半个工作区。
test("provider editor keeps fields reachable at desktop and mobile without its account list", async ({ page }) => {
  const accountId = world.tokenUiFixture?.providerAccountId;
  expect(typeof accountId === "string" && accountId.length > 0, "the restored world names one real provider account").toBe(true);
  await page.setViewportSize({ width: 1440, height: 1000 }); await login(page);
  const account = await page.evaluate(async id => {
    const response = await fetch(`/admin/resources/provider_keys/${encodeURIComponent(id)}`, { credentials: "include" });
    const document = await response.json() as { id: string; doc: { display_name?: string } };
    return { status: response.status, id: document.id, displayName: document.doc.display_name };
  }, accountId!);
  expect(account.status, "the formal configuration GET succeeds").toBe(200);
  expect(account.id, "the oracle is the requested real account").toBe(accountId);
  const name = account.displayName || accountId!;
  await navigate(page, "providers");
  await page.getByLabel("搜索上游账号", { exact: true }).fill(name);
  const table = page.getByRole("table", { name: "上游账号", exact: true });
  const row = table.getByRole("row").filter({ has: page.getByText(name, { exact: true }) });
  await expect(row, "the retained account has one edit entry").toHaveCount(1);
  await row.getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑上游账号「${name}」`, exact: true });
  await expect(form).toBeVisible();
  for (const width of [1440, 390]) {
    await page.setViewportSize({ width, height: 1000 });
    await expect(table, "editing owns the work area instead of keeping the account list beside it").toBeHidden();
    const fields = form.getByRole("textbox");
    expect(await fields.count(), "the editor retains real editable fields").toBeGreaterThan(0);
    await fields.first().scrollIntoViewIfNeeded(); await fields.first().focus(); await expect(fields.first()).toBeFocused();
    if (account.displayName) {
      const values = await fields.evaluateAll(elements => elements.map(element => (element as HTMLInputElement | HTMLTextAreaElement).value));
      expect(values, "the real account name is still offered for editing").toContain(account.displayName);
    }
    await expect(form.getByRole("button", { name: "取消", exact: true })).toBeVisible();
    await checkPage(page, `provider-editor-${width}`); await capture(page, `admin-provider-editor-${width}`);
  }
  await form.getByRole("button", { name: "取消", exact: true }).click();
  await expect(form).toHaveCount(0); await expect(table).toBeVisible();
  await expect(table.getByRole("row").filter({ has: page.getByText(name, { exact: true }) })).toHaveCount(1);
});

// 不先把编辑字段滚进视口，才能捕获长列表切换后落在表单中段的回归。
test("in-place editors open at the object title from a scrolled mobile list", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 844 }); await login(page);
  for (const route of ["models", "providers", "public-routes"]) {
    await navigate(page, route);
    const table = page.getByRole("table").filter({ has: page.locator("tbody tr") }).first();
    const publicName = route === "public-routes" ? await page.evaluate(async () => {
      const response = await fetch("/admin/public-models");
      if (!response.ok) throw new Error("public model oracle failed");
      return (await response.json()).items.find((model: { retired_at: string | null }) => model.retired_at === null)?.name as string | undefined;
    }) : undefined;
    if (route === "public-routes") expect(typeof publicName === "string" && publicName.length > 0).toBe(true);
    const entry = route === "public-routes" ? table.getByRole("button", { name: publicName!, exact: true }) : table.getByRole("button", { name: "编辑", exact: true }).first();
    await entry.scrollIntoViewIfNeeded();
    expect(await page.evaluate(() => scrollY), "editing starts from the scrolled collection").toBeGreaterThan(100);
    await entry.click(); await expect(page.getByRole("form")).toBeVisible();
    const position = await page.locator("main h1").evaluate(element => { const box = element.getBoundingClientRect(); return { top: box.top, bottom: box.bottom, scroll: scrollY }; });
    expect(position.top, `${route}: object title is visible immediately`).toBeGreaterThanOrEqual(0);
    expect(position.bottom).toBeLessThan(844); expect(position.scroll).toBe(0);
    await expect.poll(() => page.locator("main").evaluate(element => {
      const active = document.activeElement;
      if (!active || !element.contains(active)) return false;
      if (active === element) return true;
      const box = active.getBoundingClientRect();
      return box.top >= 0 && box.bottom <= innerHeight;
    }), { message: "focus moves to the new page or a visible editor control" }).toBe(true);
    await capture(page, `admin-${route}-edit-entry-390`);
    const back = route === "public-routes" ? page.getByRole("button", { name: "返回列表", exact: true }) : page.getByRole("form").getByRole("button", { name: "取消", exact: true });
    await back.click(); await expect(page.getByRole("table").first()).toBeVisible();
  }
});
