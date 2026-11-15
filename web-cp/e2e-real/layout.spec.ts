import { expect, test, type Page } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const session = resolve(process.env.CP_LAYOUT_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (!state.ready || state.mockUpstream !== false) throw new Error("Layout E2E requires a ready real isolated stack");
const baseUrl = process.env.CP_LAYOUT_URL ?? state.adminUrl;
if (!["127.0.0.1", "localhost"].includes(new URL(baseUrl).hostname)) throw new Error("Only isolated loopback URLs are allowed");
const output = resolve(process.env.CP_LAYOUT_OUTPUT!);
const health = new WeakMap<Page, { authenticated: boolean; console: object[]; unexpected: string[] }>();
test.beforeEach(async ({ page }) => {
  const ledger = { authenticated: false, console: [] as object[], unexpected: [] as string[] };
  health.set(page, ledger);
  page.on("pageerror", error => ledger.unexpected.push(error.message));
  page.on("console", message => {
    if (!["error", "warning"].includes(message.type())) return;
    ledger.console.push({ type: message.type(), text: message.text(), location: message.location() });
    const path = message.location().url;
    if (message.type() === "error" && !(message.text().includes("401") && !ledger.authenticated && /\/admin\/(me|session)/.test(path))) ledger.unexpected.push(message.text());
  });
  page.on("response", response => {
    const path = new URL(response.url()).pathname;
    if (response.status() < 400 || !path.startsWith("/admin/")) return;
    if (response.status() === 401 && !ledger.authenticated && /^\/admin\/(me|session)$/.test(path)) return;
    ledger.unexpected.push(`HTTP ${response.status()} ${path}`);
  });
});
test.afterEach(async ({ page }) => {
  await test.info().attach("console-and-http-health", { body: JSON.stringify(health.get(page)), contentType: "application/json" });
  expect.soft(health.get(page)!.unexpected, "unexpected application console / HTTP errors").toEqual([]);
});
test.describe.configure({ mode: "parallel" });
const pages = [
  "gateways", "providers", "models", "public-routes", "model-tests", "cache", "tenants", "keys", "resources", "operators", "audit", "calls",
  ...["groups", "plans", "cost", "catalogue", "status", "baselines", "imports", "library"].map(section => `pricing?section=${section}`),
  ...["accounts", "topups", "repricing", "reconciliation"].map(section => `billing?section=${section}`),
];

async function settle(page: Page): Promise<void> {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
  await expect(page.getByText("载入中…", { exact: true })).toHaveCount(0);
}
async function login(page: Page): Promise<void> {
  await page.goto(baseUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const result = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login" && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await result).status()).toBe(200);
  health.get(page)!.authenticated = true;
  await settle(page);
}
async function navigate(page: Page, route: string): Promise<void> {
  await page.goto("about:blank");
  await page.goto(`${baseUrl}/#${route}`);
  await settle(page);
  await expect(page.locator("main h1").first()).toBeVisible();
  expect(new URL(page.url()).hash).toBe(`#${route}`);
  await test.info().attach(`identity-${route}`, { body: JSON.stringify({ url: page.url(), title: await page.title() }), contentType: "application/json" });
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
}
async function screenshot(page: Page, name: string): Promise<void> {
  mkdirSync(join(output, "screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "screenshots", `${name}.png`), fullPage: false });
}

// 回归：页面溢出、表格挤在其他卡片旁、或固定列把操作挤出单元格都会失败。
async function checkLayout(page: Page, label: string): Promise<void> {
  const result = await page.evaluate(() => {
    const visible = (element: Element) => { const r = element.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
    const rect = (element: Element) => { const r = element.getBoundingClientRect(); return { x: r.x, y: r.y, right: r.right, bottom: r.bottom, width: r.width, height: r.height }; };
    const panels = [...document.querySelectorAll("main .panel")].filter(visible);
    const parallel = panels.flatMap((a, i) => panels.slice(i + 1).flatMap(b => {
      if (a.contains(b) || b.contains(a) || !(a.querySelector("table") || b.querySelector("table"))) return [];
      const ar = rect(a), br = rect(b);
      const overlap = Math.min(ar.bottom, br.bottom) - Math.max(ar.y, br.y);
      return overlap > 4 && (ar.right <= br.x + 1 || br.right <= ar.x + 1)
        ? [{ a: a.querySelector("h2,h3")?.textContent, b: b.querySelector("h2,h3")?.textContent, ar, br }] : [];
    }));
    const tables = [...document.querySelectorAll<HTMLElement>("main .dt-wrap")].filter(visible).map(element => ({ label: element.getAttribute("aria-label"), ...rect(element), client: element.clientWidth, scroll: element.scrollWidth }));
    const escapedCells = [...document.querySelectorAll("main table td")].filter(visible).flatMap(cell => {
      const cr = cell.getBoundingClientRect();
      if (getComputedStyle(cell).display !== "table-cell") return [];
      return [...cell.children].filter(visible).flatMap(child => { const r = child.getBoundingClientRect(); return r.right > cr.right + 2 || r.x < cr.x - 2 ? [{ cell: cell.textContent?.slice(0, 80), child: child.tagName, cr: rect(cell), r: rect(child) }] : []; });
    });
    return { viewport: document.documentElement.clientWidth, pageWidth: document.documentElement.scrollWidth, parallel, tables, escapedCells };
  });
  await test.info().attach(label, { body: JSON.stringify(result), contentType: "application/json" });
  expect.soft(result.pageWidth, `${label}: page horizontal overflow`).toBeLessThanOrEqual(result.viewport + 1);
  expect.soft(result.parallel, `${label}: tables must occupy their own row`).toEqual([]);
  expect.soft(result.escapedCells, `${label}: controls must fit their cells`).toEqual([]);
  for (const table of result.tables) {
    expect.soft(table.x, `${label}: ${table.label} left boundary`).toBeGreaterThanOrEqual(-1);
    expect.soft(table.right, `${label}: ${table.label} right boundary`).toBeLessThanOrEqual(result.viewport + 1);
  }
}

for (const width of [390, 900, 1440]) {
  test(`tables-use-full-main-area-${width}`, async ({ page }) => {
    const errors: string[] = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.setViewportSize({ width, height: 1000 });
    await login(page);
    for (const route of pages) {
      await navigate(page, route);
      await checkLayout(page, `${width}-${route}`);
      await screenshot(page, `${width}-${route.replace(/[^a-z0-9]+/gi, "-")}`);
    }
    expect(errors, "uncaught application errors").toEqual([]);
  });
}

test("upstream-edit-has-no-account-list-and-still-saves", async ({ page }) => {
  await login(page);
  await navigate(page, "providers");
  const row = page.getByRole("table", { name: "上游账号", exact: true }).getByRole("row").filter({ has: page.getByRole("button", { name: "编辑", exact: true }) }).first();
  await expect(row).toBeVisible();
  const originalName = (await row.getByRole("cell").first().innerText()).trim();
  await row.getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑上游账号「${originalName}」`, exact: true });
  await expect(form).toBeVisible();
  for (const width of [390, 900, 1440]) {
    await page.setViewportSize({ width, height: 1000 });
    await expect(page.getByRole("heading", { name: "全部账号", exact: true })).toHaveCount(0);
    await expect(page.getByRole("table", { name: "上游账号", exact: true })).toHaveCount(0);
    await checkLayout(page, `edit-${width}`);
    await screenshot(page, `edit-${width}`);
  }
  await expect(form.getByRole("button", { name: "取消", exact: true })).toBeVisible();
  const saved = page.waitForResponse(response => /\/admin\/resources\/provider_keys\/[^/]+$/.test(new URL(response.url()).pathname) && response.request().method() === "PUT");
  await form.getByRole("button", { name: "保存", exact: true }).click();
  expect((await saved).status()).toBe(200);
  await expect(form).toHaveCount(0);
  await expect(page.getByRole("table", { name: "上游账号", exact: true }).getByRole("row").filter({ has: page.getByRole("cell", { name: originalName, exact: true }) })).toBeVisible();
  await expect(page.getByText(`已保存「${originalName}」。`, { exact: true })).toBeVisible();
});

// 回归：目录说明不能被挤成逐字窄列，完整来源链接也要保持可读。
test("catalogue-keeps-readable-source-and-note-columns", async ({ page }) => {
  await page.setViewportSize({ width: 900, height: 1000 });
  await login(page);
  await navigate(page, "pricing?section=catalogue");
  const table = page.getByRole("table", { name: "价格目录", exact: true });
  for (const [name, minimum] of [["来源", 160], ["说明", 200]] as const) {
    const heading = table.getByRole("columnheader", { name, exact: true });
    await expect(heading).toBeVisible();
    expect((await heading.boundingBox())!.width, `${name} readable column width`).toBeGreaterThanOrEqual(minimum);
  }
  expect((await table.locator("tbody > tr").first().boundingBox())!.height, "catalogue summaries must keep rows scannable").toBeLessThanOrEqual(240);
  await checkLayout(page, "catalogue-readable-columns");
  await screenshot(page, "catalogue-readable-columns");
});

// 回归：打开编辑器或详情不能再次把主表压进半宽网格。
test("opened-details-keep-tables-on-their-own-row", async ({ page }) => {
  await login(page);
  for (const [route, label] of [
    ["pricing?section=groups", "分组"],
    ["pricing?section=cost", "上游账号成本价"],
    ["public-routes", "公共模型"],
    ["tenants", "租户列表"],
    ["billing?section=accounts", "账户列表"],
    ["pricing?section=baselines", "基准价"],
    ["pricing?section=imports", "提议"],
  ]) {
    await navigate(page, route!);
    const headings = await page.locator("main h1,main h2,main h3").allTextContents();
    await page.getByRole("table", { name: label, exact: true }).locator("tbody").getByRole("button").first().click();
    await settle(page);
    await expect.poll(() => page.locator("main h1,main h2,main h3").allTextContents(), `${label} opens a detail view`).not.toEqual(headings);
    for (const width of [390, 900, 1440]) {
      await page.setViewportSize({ width, height: 1000 });
      await checkLayout(page, `${label}-detail-${width}`);
      await screenshot(page, `detail-${route!.replace(/[^a-z0-9]+/gi, "-")}-${width}`);
    }
  }
});

// 待统一QA结束后追加到layout.spec.ts；只使用页面可访问名称和真实保存响应。
test("upstream-edit-keyboard-focus-returns-after-cancel-and-save", async ({ page }) => {
  await login(page);
  await navigate(page, "providers");
  const table = page.getByRole("table", { name: "上游账号", exact: true });
  const chosen = table.getByRole("row").filter({ has: page.getByRole("button", { name: "编辑", exact: true }) }).last();
  const originalName = (await chosen.getByRole("cell").first().innerText()).trim();
  await page.getByLabel("搜索上游账号", { exact: true }).fill(originalName);
  await expect(chosen.getByRole("cell").first()).toHaveText(originalName);
  const edit = chosen.getByRole("button", { name: "编辑", exact: true });
  const form = page.getByRole("form", { name: `编辑上游账号「${originalName}」`, exact: true });
  await edit.focus();
  await expect(edit).toBeFocused();
  await expect(page.getByLabel("搜索上游账号", { exact: true })).toHaveValue(originalName);
  await edit.press("Enter");
  await expect(form.getByLabel("名称", { exact: true })).toBeFocused();
  await expect(form.getByLabel("名称", { exact: true })).toHaveValue(originalName);
  const cancel = form.getByRole("button", { name: "取消", exact: true });
  await cancel.focus();
  await cancel.press("Enter");
  await expect(form).toHaveCount(0);
  await expect(edit).toBeFocused();
  await expect(page.getByLabel("搜索上游账号", { exact: true })).toHaveValue(originalName);
  await edit.press("Enter");
  await expect(form.getByLabel("名称", { exact: true })).toBeFocused();
  const saved = page.waitForResponse(response => /\/admin\/resources\/provider_keys\/[^/]+$/.test(new URL(response.url()).pathname) && response.request().method() === "PUT");
  const save = form.getByRole("button", { name: "保存", exact: true });
  await save.focus();
  await save.press("Enter");
  expect((await saved).status()).toBe(200);
  await expect(form).toHaveCount(0);
  await expect(edit).toBeFocused();
  await screenshot(page, "upstream-focus-returned-after-save");
});
