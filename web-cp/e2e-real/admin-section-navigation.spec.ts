import { expect, test, type Page } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import { join, resolve } from "node:path";

// 只在真实隔离后端读取页面；角色账号由正常管理 API 预先创建。
const state = JSON.parse(readFileSync(resolve(process.env.ADMIN_SECTION_QA_STATE!), "utf8"));
const base: string = state.baseUrl;
if (!state.ready || state.mockUpstream !== false || !["127.0.0.1", "localhost"].includes(new URL(base).hostname)) throw new Error("A ready real loopback stack is required");
const output = resolve(process.env.ADMIN_SECTION_QA_OUTPUT!);
const failures = new WeakMap<Page, string[]>();
const pages = [
  { tab: "pricing", label: "价格", sections: [["groups", "分组"], ["plans", "租户方案"], ["cost", "成本价"], ["catalogue", "价格目录"], ["status", "计价状态"], ["baselines", "基准价"], ["imports", "导入审核"], ["library", "价格库"]] },
  { tab: "billing", label: "计费", sections: [["accounts", "账户"], ["topups", "充值审批"], ["repricing", "重新计价"], ["reconciliation", "对账"], ["profit", "本利汇算"]] },
] as const;

async function settle(page: Page): Promise<void> {
  await page.waitForLoadState("networkidle");
  await page.evaluate(() => document.fonts.ready);
  await expect(page.locator("vite-error-overlay, nextjs-portal")).toHaveCount(0);
}

async function section(page: Page, tab: string, id: string, title: string): Promise<void> {
  await expect.poll(() => page.evaluate(() => new URLSearchParams(location.hash.split("?")[1]).get("section"))).toBe(id);
  expect(new URL(page.url()).hash.split("?")[0]).toBe(`#${tab}`);
  await expect(page.getByRole("heading", { name: title, exact: true, level: 1 })).toBeVisible();
  await settle(page);
}

for (const role of ["admin", "operator", "viewer"] as const) {
  test.describe(role, () => {
    test.beforeEach(async ({ page }) => {
      const errors: string[] = [];
      failures.set(page, errors);
      page.on("pageerror", error => errors.push(error.message));
      page.on("response", response => {
        const path = new URL(response.url()).pathname;
        if (path.startsWith("/admin/") && response.status() >= 400 && !(response.status() === 401 && /^\/admin\/(session|me)$/.test(path))) errors.push(`HTTP ${response.status()} ${path}`);
      });
      page.on("request", request => {
        const path = new URL(request.url()).pathname;
        if (/\/upstream-models$|\/probe(?:\/|$)|\/model-tests(?:\/|$)|^\/v1\//.test(path)) errors.push(`Unexpected upstream action: ${path}`);
      });
      await page.goto(base);
      await page.getByLabel("邮箱", { exact: true }).fill(state[role].email);
      await page.getByLabel("口令", { exact: true }).fill(state[role].password);
      await page.getByRole("button", { name: "登录", exact: true }).click();
      await page.locator("main").waitFor();
      await settle(page);
    });
    test.afterEach(async ({ page }) => { expect(failures.get(page), "no runtime errors or upstream actions").toEqual([]); });

    for (const family of pages) {
      test(`${family.tab}: every section preserves URL, filters, reload and history`, async ({ page }) => {
        await page.goto(`${base}/#${family.tab}?section=${family.sections[0][0]}&tenant_id=scope-481&model_id=model%2F481&filter=keep%20me`);
        await settle(page);
        const nav = page.getByRole("group", { name: `${family.label}分区`, exact: true });
        for (const [id, title] of family.sections) {
          await nav.getByRole("button", { name: title, exact: true }).click();
          await section(page, family.tab, id, title);
          expect(await page.evaluate(() => Object.fromEntries(new URLSearchParams(location.hash.split("?")[1])))).toMatchObject({ tenant_id: "scope-481", model_id: "model/481", filter: "keep me" });
          await page.reload();
          await section(page, family.tab, id, title);
        }
        const first = family.sections[0];
        const second = family.sections[1];
        await nav.getByRole("button", { name: first[1], exact: true }).click();
        await section(page, family.tab, first[0], first[1]);
        await nav.getByRole("button", { name: second[1], exact: true }).click();
        await section(page, family.tab, second[0], second[1]);
        await page.goBack();
        await section(page, family.tab, first[0], first[1]);
        await page.goForward();
        await section(page, family.tab, second[0], second[1]);
        const history = await page.evaluate(() => window.history.length);
        await nav.getByRole("button", { name: second[1], exact: true }).click();
        await section(page, family.tab, second[0], second[1]);
        expect(await page.evaluate(() => window.history.length), "reselecting the current section adds no history").toBe(history);
        mkdirSync(output, { recursive: true });
        await page.screenshot({ path: join(output, `${role}-${family.tab}-1440.png`) });
      });
    }

    test("same-tab deep links, missing/unknown section and role fallbacks", async ({ page }) => {
      await page.goto(`${base}/#pricing?section=cost`);
      await section(page, "pricing", "cost", "成本价");
      await page.goto(`${base}/#pricing`);
      await expect(page.getByRole("heading", { name: "分组", exact: true, level: 1 })).toBeVisible();
      await page.goto(`${base}/#pricing?section=not-a-real-section`);
      await expect(page.getByRole("heading", { name: "分组", exact: true, level: 1 })).toBeVisible();
      await expect(page.getByRole("group", { name: "价格分区", exact: true }).getByRole("button", { name: "分组", exact: true })).toHaveAttribute("aria-pressed", "true");
      if (role === "viewer") await expect(page.getByRole("button", { name: "新建分组", exact: true })).toHaveCount(0);
      await page.goto(`${base}/#operators`);
      await expect(page.getByRole("heading", { name: role === "admin" ? "运维账号" : "运行概况", exact: true, level: 1 })).toBeVisible();
    });

    test("tenant selection survives section clicks and legacy tenant links keep their destination", async ({ page }) => {
      for (const [tab, label, first, next, title] of [["pricing", "价格", "cost", "catalogue", "价格目录"], ["billing", "计费", "topups", "repricing", "重新计价"]]) {
        await page.goto(`${base}/#${tab}?section=${first}&tenant=${state.tenantId}&status=pending`);
        await settle(page);
        await page.getByRole("group", { name: `${label}分区`, exact: true }).getByRole("button", { name: title, exact: true }).click();
        await section(page, tab!, next!, title!);
        expect(await page.evaluate(() => Object.fromEntries(new URLSearchParams(location.hash.split("?")[1])))).toMatchObject({ tenant: state.tenantId, status: "pending" });
        await page.reload();
        await section(page, tab!, next!, title!);
      }
      await page.goto(`${base}/#billing?section=accounts&tenant=${state.tenantId}`);
      await expect(page.getByRole("heading", { name: state.tenantName, exact: true, level: 1 })).toBeVisible();
      await expect(page.locator('.tenant-hub-nav [aria-current="page"]')).toHaveText("余额与计费");
      await page.goto(`${base}/#pricing?section=plans&tenant=${state.tenantId}`);
      await expect(page.getByRole("heading", { name: state.tenantName, exact: true, level: 1 })).toBeVisible();
      await expect(page.locator('.tenant-hub-nav [aria-current="page"]')).toHaveText("折扣记录");
    });
  });
}
