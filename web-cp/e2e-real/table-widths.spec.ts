import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const output = resolve(process.env.REFINED_QA_OUTPUT!, "table-widths");
if (!state.ready || state.mockUpstream !== false) throw new Error("Table E2E requires a ready real isolated stack");
for (const url of [state.adminUrl, state.tenantUrl]) {
  if (!["127.0.0.1", "localhost"].includes(new URL(url).hostname)) throw new Error("Only isolated loopback URLs are allowed");
}

async function settle(page: Page): Promise<void> {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
  await expect(page.getByText("载入中…", { exact: true })).toHaveCount(0);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
}

async function login(page: Page, app: "admin" | "tenant"): Promise<void> {
  const account = app === "admin" ? auth.operator : auth.metricsOwner;
  await page.goto(app === "admin" ? state.adminUrl : state.tenantUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(account.email);
  await page.getByLabel("口令", { exact: true }).fill(account.password);
  const result = page.waitForResponse(response => new URL(response.url()).pathname === (app === "admin" ? "/admin/login" : "/api/login") && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await result).status()).toBe(200);
  await settle(page);
}

async function checkCells(table: Locator): Promise<void> {
  await expect(table.locator("tbody > tr").first()).toBeVisible();
  const escaped = await table.locator("tbody td").evaluateAll(cells => cells.flatMap(cell => {
    const bounds = cell.getBoundingClientRect();
    return [...cell.querySelectorAll("button, input, [role=combobox], code, .lamp, .trunc")].flatMap(child => {
      const rect = child.getBoundingClientRect();
      if (!rect.width || !rect.height) return [];
      return rect.left < bounds.left - 2 || rect.right > bounds.right + 2
        ? [{ label: cell.getAttribute("data-label"), element: child.tagName, width: rect.width, cellWidth: bounds.width }] : [];
    });
  }));
  expect(escaped, "text and controls must stay inside their own cells").toEqual([]);
}

async function capture(page: Page, name: string): Promise<void> {
  const geometry = await page.evaluate(() => ({ viewport: document.documentElement.clientWidth, page: document.documentElement.scrollWidth }));
  expect(geometry.page, "tables must not widen the page").toBeLessThanOrEqual(geometry.viewport + 1);
  mkdirSync(output, { recursive: true });
  await page.screenshot({ path: join(output, `${name}.png`), fullPage: false });
}

// 真实用户先读名称和接口，再筛选或打开菜单；短状态不应占走描述列的空间。
for (const width of [1440, 900, 390]) {
  test(`tables-keep-content-readable-and-controls-working-${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    const runtime: string[] = [];
    page.on("pageerror", error => runtime.push(error.message));
    await login(page, "admin");
    await page.goto(`${state.adminUrl}/#providers`);
    await settle(page);
    const accounts = page.getByRole("table", { name: "上游账号", exact: true });
    await checkCells(accounts);
    if (width === 1440) {
      expect((await accounts.getByRole("columnheader", { name: "名称", exact: true }).boundingBox())!.width).toBeGreaterThanOrEqual(180);
      expect((await accounts.getByRole("columnheader", { name: "接口地址", exact: true }).boundingBox())!.width).toBeGreaterThanOrEqual(220);
      expect((await accounts.getByRole("columnheader", { name: "状态", exact: true }).boundingBox())!.width).toBeLessThanOrEqual(140);
    }
    const accountName = (await accounts.locator("tbody > tr").first().getByRole("cell").first().innerText()).trim();
    await page.getByLabel("搜索上游账号", { exact: true }).fill(accountName);
    await expect(accounts.locator("tbody > tr").first().getByRole("cell").first()).toHaveText(accountName);
    await capture(page, `admin-accounts-${width}`);

    await login(page, "tenant");
    await page.goto(`${state.tenantUrl}/#keys`);
    await settle(page);
    const keys = page.getByRole("table", { name: "密钥列表", exact: true });
    await checkCells(keys);
    if (width === 1440) {
      for (const name of ["名称", "模型"]) expect((await keys.getByRole("columnheader", { name, exact: true }).boundingBox())!.width).toBeGreaterThanOrEqual(150);
      expect((await keys.getByRole("columnheader", { name: "状态", exact: true }).boundingBox())!.width).toBeLessThanOrEqual(140);
    }
    const menu = keys.getByRole("button", { name: /^更多操作：/ }).first();
    await menu.click();
    await expect(page.getByRole("menuitem", { name: "查看用量", exact: true })).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("menu")).toHaveCount(0);
    await expect(menu).toBeFocused();
    await capture(page, `tenant-keys-${width}`);
    expect(runtime, "uncaught application errors").toEqual([]);
  });
}
