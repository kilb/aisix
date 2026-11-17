import { expect, test, type Locator, type Page } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import { join, resolve } from "node:path";

// 只读取保留的真实隔离环境；不生成业务数据、不替换网络响应。
const session = resolve(process.env.FLOATING_FILTERS_QA_SESSION!);
const output = resolve(process.env.FLOATING_FILTERS_QA_OUTPUT!);
const world = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (!world.ready || !world.realBackend || world.mockUpstream !== false) throw new Error("A real isolated backend is required");
for (const base of [world.adminUrl, world.tenantUrl]) {
  if (new URL(base).hostname !== "127.0.0.1") throw new Error("Only isolated loopback UI origins are accepted");
}

async function settle(page: Page): Promise<void> {
  await expect(page.locator("main h1:visible")).toHaveCount(1);
  await page.waitForLoadState("networkidle");
  await page.evaluate(() => document.fonts.ready);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
}

async function geometry(bar: Locator): Promise<{ left: number; right: number; width: number }> {
  return bar.evaluate(element => {
    const box = element.getBoundingClientRect();
    return { left: box.left, right: box.right, width: box.width };
  });
}

async function aligned(bar: Locator, table?: Locator): Promise<void> {
  const current = await geometry(bar);
  const reference = await bar.evaluate(element => {
    const card = element.closest(".bento-card")!;
    const box = card.getBoundingClientRect();
    const css = getComputedStyle(card);
    return { left: box.left + parseFloat(css.borderLeftWidth), right: box.right - parseFloat(css.borderRightWidth) };
  });
  expect(Math.abs(current.left - reference.left), "filter surface aligns with its content edge").toBeLessThanOrEqual(1);
  expect(Math.abs(current.right - reference.right), "filter surface fills its content width").toBeLessThanOrEqual(1);
  if (table) {
    const viewport = await table.evaluate(element => {
      const box = element.parentElement!.getBoundingClientRect();
      return { left: box.left, right: box.right };
    });
    expect(Math.abs(current.left - viewport.left), "filter and table viewport share their left edge").toBeLessThanOrEqual(1);
    expect(Math.abs(current.right - viewport.right), "filter and table viewport share their right edge").toBeLessThanOrEqual(1);
  }
}

for (const width of [1440, 390]) {
  for (const surface of ["admin-calls", "tenant-calls", "tenant-usage"] as const) {
    test(`${surface} aligns before and during scroll with readable translucent controls at ${width}`, async ({ page }) => {
      await page.setViewportSize({ width, height: 500 });
      const admin = surface.startsWith("admin");
      const base = admin ? world.adminUrl : world.tenantUrl;
      const prefix = admin ? "/admin" : "/api";
      const who = admin ? auth.operator : auth.owner;
      const errors: string[] = [];
      const writes: string[] = [];
      page.on("pageerror", error => errors.push(error.message));
      await page.goto(base);
      await page.getByLabel("邮箱", { exact: true }).fill(who.email);
      await page.getByLabel("口令", { exact: true }).fill(who.password);
      const login = page.waitForResponse(r => new URL(r.url()).pathname === prefix + "/login");
      await page.getByRole("button", { name: "登录", exact: true }).click();
      expect((await login).status()).toBe(200);
      await settle(page);
      page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
      page.on("request", request => { if (!["GET", "HEAD"].includes(request.method())) writes.push(request.method() + " " + new URL(request.url()).pathname); });
      const usage = surface.endsWith("usage");
      await page.goto(base + "/#" + (usage ? "usage" : "calls") + "?from=2026-11-12T00%3A00%3A00.000Z&to=2026-11-18T00%3A00%3A00.000Z");
      await settle(page);
      const bar = usage ? page.getByRole("region", { name: "用量分析筛选", exact: true }) : admin ? page.locator(".charge-filter-controls") : page.getByRole("region", { name: "调用结果筛选", exact: true });
      const table = usage ? undefined : page.getByRole("table", { name: admin ? "计费明细" : "调用记录", exact: true });
      await expect(bar).toBeVisible();
      if (table) await expect(table).toBeVisible();
      await aligned(bar, table);
      const style = await bar.evaluate(element => {
        const css = getComputedStyle(element);
        const canvas = document.createElement("canvas");
        canvas.width = canvas.height = 1;
        const context = canvas.getContext("2d")!;
        context.fillStyle = css.backgroundColor;
        context.fillRect(0, 0, 1, 1);
        return {
          alpha: context.getImageData(0, 0, 1, 1).data[3], blur: css.backdropFilter,
          radii: [css.borderTopLeftRadius, css.borderTopRightRadius, css.borderBottomRightRadius, css.borderBottomLeftRadius].map(parseFloat),
          borders: [css.borderTopWidth, css.borderRightWidth, css.borderBottomWidth, css.borderLeftWidth].map(parseFloat),
          borderColor: css.borderTopColor, shadow: css.boxShadow,
        };
      });
      expect(style.alpha, "the filter surface has an actual translucent fill").toBeGreaterThan(0);
      expect(style.alpha, "the filter surface is not an opaque rectangle").toBeLessThan(255);
      expect(style.blur, "underlying table content is softened").toMatch(/blur\(/);
      expect(style.radii.every(radius => radius >= 8), "the floating surface has readable rounded corners").toBe(true);
      expect(style.borders.every(border => border >= 1), "the floating surface has a complete visible border").toBe(true);
      expect(style.borderColor).not.toBe("rgba(0, 0, 0, 0)");
      expect(style.shadow, "the floating surface is distinct from the table underneath").not.toBe("none");
      mkdirSync(join(output, "screenshots"), { recursive: true });
      await page.screenshot({ path: join(output, "screenshots", `${surface}-${width}-before-scroll.png`), animations: "disabled" });
      const before = await geometry(bar);
      const scroll = await bar.evaluate(element => {
        let root = element.parentElement;
        while (root && !(root.scrollHeight > root.clientHeight + 5 && ["auto", "scroll"].includes(getComputedStyle(root).overflowY))) root = root.parentElement;
        const scrolling = root ?? document.scrollingElement!;
        const previous = scrolling.scrollTop;
        scrolling.scrollTop += element.getBoundingClientRect().top + 80;
        return { before: previous, after: scrolling.scrollTop };
      });
      expect(scroll.after, "the page actually scrolls").toBeGreaterThan(scroll.before + 30);
      await page.evaluate(() => new Promise(resolveFrame => requestAnimationFrame(() => requestAnimationFrame(resolveFrame))));
      await aligned(bar, table);
      const after = await geometry(bar);
      expect(Math.abs(after.width - before.width), "scroll does not stretch or shrink the filter bar").toBeLessThanOrEqual(1);
      const bounds = await bar.boundingBox();
      expect(bounds!.y, "filters stay below the sticky page header").toBeGreaterThanOrEqual(59);
      expect(bounds!.y, "the filter surface reaches its sticky position").toBeLessThanOrEqual(61);
      const select = bar.getByRole("combobox", { name: usage ? "模型" : "调用结果", exact: true });
      await select.scrollIntoViewIfNeeded();
      await select.click();
      const list = page.getByRole("listbox");
      await expect(list).toBeVisible();
      const menu = await list.boundingBox();
      expect(menu!.x).toBeGreaterThanOrEqual(0);
      expect(menu!.x + menu!.width).toBeLessThanOrEqual(width);
      expect(menu!.y).toBeGreaterThanOrEqual(0);
      expect(menu!.y + menu!.height).toBeLessThanOrEqual(500);
      await page.keyboard.press("Escape");
      await expect(list).toHaveCount(0);
      const dimensions = await page.evaluate(() => ({ width: document.documentElement.clientWidth, scroll: document.documentElement.scrollWidth }));
      expect(dimensions.scroll).toBeLessThanOrEqual(dimensions.width + 1);
      mkdirSync(join(output, "screenshots"), { recursive: true });
      await page.screenshot({ path: join(output, "screenshots", `${surface}-${width}.png`), animations: "disabled" });
      await select.click();
      const options = page.getByRole("listbox");
      const option = usage ? options.getByRole("option").and(page.locator('[data-value]:not([data-value=""])')).first() : options.getByRole("option", { name: "失败", exact: true });
      const selected = usage ? await option.getAttribute("data-value") : "failed";
      expect(selected, "the real dropdown offers a non-default filter").toBeTruthy();
      const filtered = page.waitForResponse(response => {
        const url = new URL(response.url());
        return response.request().method() === "GET" && url.pathname === (usage ? "/api/usage/metrics" : admin ? "/admin/pricing/charges" : "/api/usage/calls") && url.searchParams.get(usage ? "model_id" : "outcome") === selected;
      });
      await option.click();
      expect((await filtered).status(), "selecting a floating control reaches the real backend").toBe(200);
      await expect(select).toHaveAttribute("data-value", selected!);
      await test.info().attach("observed-filter-state", { body: JSON.stringify({ style, before, after, scroll, errors, writes }), contentType: "application/json" });
      expect(errors).toEqual([]);
      expect(writes).toEqual([]);
    });
  }
}
