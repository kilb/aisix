import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 仅登录统一准备的真实本地世界；不创建数据、不发推理、不替换任何响应。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (!state.ready || state.mockUpstream !== false) throw new Error("A ready, real, isolated world is required");
const output = resolve(process.env.REFINED_QA_OUTPUT!);
const viewports = [{ width: 1440, height: 1000 }, { width: 390, height: 844 }];

async function settle(page: Page): Promise<void> {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
  await expect(page.getByText("载入中…", { exact: true })).toHaveCount(0);
}

async function height(control: Locator, expected: number): Promise<void> {
  await expect(control).toBeVisible();
  const box = await control.boundingBox();
  expect(box).not.toBeNull();
  expect(box!.height).toBe(expected);
}

async function screenshot(page: Page, name: string): Promise<void> {
  mkdirSync(join(output, "controls-screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "controls-screenshots", name + ".png"), fullPage: false });
}

for (const app of ["admin", "tenant"] as const) {
  test.describe(app, () => {
    const target = state[app === "admin" ? "adminUrl" : "tenantUrl"] as string;
    const origin = new URL(target).origin;
    if (!["127.0.0.1", "localhost"].includes(new URL(target).hostname)) throw new Error("Only loopback origins are allowed");
    const prefix = app === "admin" ? "/admin" : "/api";
    const telemetry = new WeakMap<Page, { authenticated: boolean; errors: string[]; writes: string[] }>();

    test.beforeEach(async ({ page, context }) => {
      const health = { authenticated: false, errors: [] as string[], writes: [] as string[] };
      telemetry.set(page, health);
      page.on("pageerror", error => health.errors.push(error.message));
      page.on("console", message => {
        if (message.type() !== "error") return;
        const url = message.location().url;
        const bootstrap = !health.authenticated && /^https?:/.test(url) && new URL(url).origin === origin
          && new RegExp(`^${prefix}/(?:me|session)$`).test(new URL(url).pathname) && /401|Unauthorized/.test(message.text());
        if (!bootstrap) health.errors.push(message.text());
      });
      page.on("response", response => {
        const url = new URL(response.url());
        if (url.origin !== origin || !url.pathname.startsWith(prefix + "/") || response.status() < 400) return;
        if (!health.authenticated && response.status() === 401 && [prefix + "/me", prefix + "/session"].includes(url.pathname)) return;
        health.errors.push(`${response.status()} ${url.pathname}`);
      });
      page.on("request", request => {
        const url = new URL(request.url());
        if (url.origin !== origin || ["GET", "HEAD"].includes(request.method())) return;
        // 用户点击复制允许正式的按需明文读取；不允许保存或其它业务写入。
        if ([prefix + "/login", prefix + "/logout"].includes(url.pathname) || new RegExp(`^${prefix}/keys/[a-f0-9-]+/reveal$`).test(url.pathname)) return;
        health.writes.push(`${request.method()} ${url.pathname}`);
      });
      await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin });
      await page.goto(target);
      const who = app === "admin" ? auth.operator : auth.metricsOwner;
      await page.getByLabel("邮箱", { exact: true }).fill(who.email);
      await page.getByLabel("口令", { exact: true }).fill(who.password);
      const response = page.waitForResponse(item => new URL(item.url()).pathname === prefix + "/login" && item.request().method() === "POST");
      await page.getByRole("button", { name: "登录", exact: true }).click();
      expect((await response).status()).toBe(200);
      health.authenticated = true;
      await settle(page);
      expect(new URL(page.url()).origin).toBe(origin);
      await expect(page).toHaveTitle(/AISIX/i);
      await expect(page.locator("main")).toBeVisible();
      await expect(page.locator("vite-error-overlay")).toHaveCount(0);
    });

    test.afterEach(async ({ page }) => {
      const health = telemetry.get(page)!;
      expect.soft(health.errors, "unexpected runtime, console or API errors").toEqual([]);
      expect.soft(health.writes, "no unexpected business writes or model requests").toEqual([]);
      const status = await page.evaluate(async path => (await fetch(path, { method: "POST", headers: { "content-type": "application/json" }, body: "{}" })).status, prefix + "/logout");
      expect(status).toBe(204);
    });

    test("account-controls-share-height-and-visible-blue-keyboard-focus", async ({ page }) => {
      await page.goto(target + "/#account");
      await settle(page);
      const main = page.locator("main");
      await expect(main.getByRole("heading", { level: 1 })).toBeVisible();
      const fields = main.locator('input[type="password"]');
      expect(await fields.count()).toBeGreaterThanOrEqual(2);
      const form = main.locator("form").filter({ has: page.locator('input[type="password"]') });
      const save = form.locator('button[type="submit"]');
      await expect(save).toHaveCount(1);
      for (const viewport of viewports) {
        await page.setViewportSize(viewport);
        const expected = viewport.width < 720 ? 44 : 40;
        for (const field of await fields.all()) await height(field, expected);
        await height(save, expected);
        await fields.first().focus();
        await page.keyboard.press("Tab");
        await page.keyboard.press("Shift+Tab");
        await expect(fields.first()).toBeFocused();
        const focus = await fields.first().evaluate(element => {
          const css = getComputedStyle(element);
          return { outline: css.outlineStyle, width: css.outlineWidth, color: css.outlineColor, border: css.borderTopWidth };
        });
        expect(focus.outline).toBe("solid");
        expect(focus.width).toBe("2px");
        expect(focus.border).toBe("1px");
        const rgb = focus.color.match(/[\d.]+/g)!.map(Number);
        expect(rgb[2]! > rgb[0]! + 40, "keyboard focus is blue").toBe(true);
        const header = main.locator("header").filter({ has: page.getByRole("heading", { level: 1 }) });
        await expect(header).toHaveCSS("margin-bottom", "12px");
        await screenshot(page, `${app}-account-${viewport.width}`);
      }
    });

    test("call-filters-and-compact-page-size-keep-keyboard-and-viewport-contract", async ({ page }) => {
      await page.goto(target + "/#calls");
      await settle(page);
      const regular = page.getByRole("combobox", { name: "密钥", exact: true });
      const compact = page.getByRole("combobox", { name: /每页条数/ });
      for (const viewport of viewports) {
        await page.setViewportSize(viewport);
        await height(regular, viewport.width < 720 ? 44 : 40);
        await height(compact, viewport.width < 720 ? 44 : 32);
        await compact.focus();
        await page.keyboard.press("ArrowDown");
        const list = page.getByRole("listbox");
        await expect(list).toBeVisible();
        await page.keyboard.press("Home");
        await expect(page.getByRole("option", { name: "10", exact: true })).toBeFocused();
        const box = await list.boundingBox();
        expect(box).not.toBeNull();
        expect(box!.x).toBeGreaterThanOrEqual(0);
        expect(box!.y).toBeGreaterThanOrEqual(0);
        expect(box!.x + box!.width).toBeLessThanOrEqual(viewport.width);
        expect(box!.y + box!.height).toBeLessThanOrEqual(viewport.height);
        await screenshot(page, `${app}-select-${viewport.width}`);
        await page.keyboard.press("Enter");
        await expect(compact).toHaveText("10");
        await expect(compact).toBeFocused();
        await settle(page);
        await compact.click();
        await expect(list).toBeVisible();
        await page.keyboard.press("Escape");
        await expect(list).toHaveCount(0);
        await expect(compact).toBeFocused();
      }
    });

    test("copy-is-a-lightweight-icon-and-copies-a-real-key-on-demand", async ({ page }) => {
      await page.goto(target + "/#keys");
      await settle(page);
      const copy = page.getByRole("button", { name: "复制密钥", exact: true }).first();
      await expect(copy).toBeEnabled();
      for (const viewport of viewports) {
        await page.setViewportSize(viewport);
        await page.mouse.move(0, 0);
        await height(copy, viewport.width < 720 ? 44 : 32);
        await expect(copy).toHaveText("");
        await expect(copy).toHaveCSS("background-color", "rgba(0, 0, 0, 0)");
        await expect(copy).toHaveCSS("border-top-color", "rgba(0, 0, 0, 0)");
        await screenshot(page, `${app}-copy-${viewport.width}`);
      }
      const visiblePrefix = (await copy.locator("xpath=ancestor::tr[1]").getByLabel("密钥前缀", { exact: true }).innerText()).replace(/…$/, "");
      expect(visiblePrefix.length > 0).toBe(true);
      await page.evaluate(() => navigator.clipboard.writeText(""));
      await copy.click();
      await expect(page.getByRole("button", { name: "已复制", exact: true }).first()).toBeVisible();
      // 不把真实密钥写进断言、报告或截图。
      const matchesDisplayedKey = await page.evaluate(async prefix => {
        const value = await navigator.clipboard.readText();
        await navigator.clipboard.writeText("");
        return value.length > prefix.length && value.startsWith(prefix);
      }, visiblePrefix);
      expect(matchesDisplayedKey, "clipboard contains the key for the displayed prefix").toBe(true);
    });
  });
}
