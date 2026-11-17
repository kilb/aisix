import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { chooseSelect } from "../e2e/select";

const output = resolve(process.env.CALL_CARDS_QA_OUTPUT!);
const state = JSON.parse(readFileSync(process.env.CALL_CARDS_QA_STATE!, "utf8"));
for (const address of [state.httpUrl, state.previewUrl, state.tenantUrl]) {
  if (new URL(address).hostname !== "127.0.0.1") throw new Error("Isolated loopback services are required");
}

async function rect(area: Locator) {
  const box = await area.boundingBox();
  expect(box, "the measured surface must exist").not.toBeNull();
  return box!;
}
async function settle(page: Page) {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
}
async function scroll(page: Page, top: number) {
  await page.evaluate(async y => {
    window.scrollTo({ top: y, behavior: "instant" });
    await new Promise<void>(done => requestAnimationFrame(() => requestAnimationFrame(() => done())));
  }, top);
}
async function deepFilter(page: Page, filters: Locator, records: Locator, table: Locator, endpoint: string, name: string) {
  await scroll(page, 2500);
  const before = await page.evaluate(() => ({ scroll: scrollY, height: document.documentElement.scrollHeight, viewport: innerHeight }));
  expect(before.scroll, "the scenario starts beyond two viewports").toBeGreaterThan(before.viewport * 2);
  await page.evaluate(() => {
    const samples: { loading: string | null; scroll: number; height: number; rows: number }[] = [];
    const record = () => samples.push({ loading: document.querySelector(".call-record-layout")!.getAttribute("data-loading"), scroll: scrollY, height: document.documentElement.scrollHeight, rows: document.querySelectorAll(".call-table-card tbody > tr").length });
    const observer = new MutationObserver(record);
    observer.observe(document.querySelector(".call-record-layout")!, { attributes: true, attributeFilter: ["data-loading"] });
    window.addEventListener("scroll", record);
    Object.assign(window, { callHeightSamples: samples, stopCallHeightSamples: () => { observer.disconnect(); window.removeEventListener("scroll", record); } });
  });
  const read = page.waitForResponse(response => new URL(response.url()).pathname === endpoint && new URL(response.url()).searchParams.get("outcome") === "failed");
  await chooseSelect(filters.getByRole("combobox", { name: "调用结果", exact: true }), { value: "failed" });
  expect((await read).status()).toBe(200);
  await expect(records.locator("..")).toHaveAttribute("data-loading", "false");
  await expect(table.locator("tbody > tr")).toHaveCount(25);
  await settle(page);
  const after = await page.evaluate(() => {
    const audit = window as typeof window & { callHeightSamples: { loading: string | null; scroll: number; height: number; rows: number }[]; stopCallHeightSamples: () => void };
    audit.stopCallHeightSamples();
    return { scroll: scrollY, height: document.documentElement.scrollHeight, samples: audit.callHeightSamples };
  });
  await test.info().attach(name + "-deep-scroll", { body: JSON.stringify({ before, after }), contentType: "application/json" });
  const loading = after.samples.filter(sample => sample.loading === "true");
  expect(loading.length, "real loading was observed rather than skipped").toBeGreaterThan(0);
  for (const sample of loading) {
    expect(sample.rows, "loading never presents previous-filter rows").toBe(0);
    expect(Math.abs(sample.scroll - before.scroll), "loading preserves the deep reading position").toBeLessThanOrEqual(2);
    expect(Math.abs(sample.height - before.height), "loading preserves the previous results height").toBeLessThanOrEqual(2);
  }
  expect(Math.abs(after.scroll - before.scroll), "the completed 25-row result preserves the deep reading position").toBeLessThanOrEqual(2);
  await shot(page, name + "-deep-filter");
  const cleared = page.waitForResponse(response => new URL(response.url()).pathname === endpoint && !new URL(response.url()).searchParams.has("outcome"));
  await filters.getByRole("button", { name: "清除调用结果筛选", exact: true }).click();
  expect((await cleared).status()).toBe(200);
  await expect(table.locator("tbody > tr")).toHaveCount(25);
}
async function shot(page: Page, name: string) {
  mkdirSync(output, { recursive: true });
  await page.screenshot({ path: join(output, name + ".png"), fullPage: false });
}

// 断言来自用户要求的独立卡片、悬浮与可操作性；真实记录仅由网关连接失败产生。
for (const app of ["admin", "tenant"] as const) {
  test(`${app}: filters and records are separate cards, sticky in both directions, usable on narrow screens`, async ({ page }) => {
    const errors: string[] = [];
    const warnings: string[] = [];
    const requests: string[] = [];
    let authenticated = false;
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => {
      if (message.type() === "warning") warnings.push(message.text());
      if (message.type() !== "error") return;
      if (!authenticated && /401/.test(message.text()) && /\/(?:admin|api)\/(?:me|session)$/.test(message.location().url)) return;
      errors.push(message.text());
    });
    page.on("response", response => {
      const path = new URL(response.url()).pathname;
      if (/^\/(?:admin|api)\//.test(path)) requests.push(`${response.request().method()} ${path} ${response.status()}`);
      if (response.status() >= 400 && /^\/(?:admin|api)\//.test(path) && !(response.status() === 401 && !authenticated)) errors.push(`HTTP ${response.status()} ${path}`);
    });
    const base = app === "admin" ? state.previewUrl : state.tenantUrl;
    const who = app === "admin" ? state.operator : state.owner;
    await page.goto(base);
    await page.getByLabel("邮箱", { exact: true }).fill(who.email);
    await page.getByLabel("口令", { exact: true }).fill(who.password);
    const login = page.waitForResponse(response => new URL(response.url()).pathname === `/${app === "admin" ? "admin" : "api"}/login`);
    await page.getByRole("button", { name: "登录", exact: true }).click();
    expect((await login).status()).toBe(200);
    authenticated = true;
    await page.goto(`${base}/#calls`);
    const filters = page.getByRole("region", { name: app === "admin" ? "调用记录筛选" : "调用结果筛选", exact: true });
    const records = page.locator(`[role="region"][aria-label="${app === "admin" ? "调用计费记录" : "调用记录"}"]`);
    const table = records.getByRole("table", { name: app === "admin" ? "计费明细" : "调用记录", exact: true });
    await expect(filters).toBeVisible();
    await expect(table.locator("tbody > tr")).toHaveCount(25);
    await settle(page);
    await expect(page).toHaveTitle(app === "admin" ? "Routio 运维台" : "Routio 组织门户");
    expect(await page.locator("vite-error-overlay").count()).toBe(0);
    const containers = await filters.evaluate((filter, recordLabel) => {
      const record = document.querySelector(`[role="region"][aria-label="${recordLabel}"]`)!;
      return { siblings: filter.parentElement === record.parentElement, filterContainsRecord: filter.contains(record), recordContainsFilter: record.contains(filter) };
    }, app === "admin" ? "调用计费记录" : "调用记录");
    expect(containers).toEqual({ siblings: true, filterContainsRecord: false, recordContainsFilter: false });
    const initialFilter = await rect(filters), initialRecord = await rect(records);
    expect(Math.abs(initialFilter.x - initialRecord.x)).toBeLessThanOrEqual(1);
    expect(Math.abs(initialFilter.width - initialRecord.width)).toBeLessThanOrEqual(1);
    expect(initialRecord.y - initialFilter.y - initialFilter.height).toBeGreaterThanOrEqual(12);
    const style = await filters.evaluate(el => { const c = getComputedStyle(el); return { border: parseFloat(c.borderTopWidth), radius: parseFloat(c.borderTopLeftRadius), shadow: c.boxShadow }; });
    expect(style.border).toBeGreaterThan(0); expect(style.radius).toBeGreaterThan(0); expect(style.shadow).not.toBe("none");
    if (app === "admin") {
      const title = await rect(records.getByRole("heading", { name: "调用计费记录", exact: true }));
      const search = await rect(records.getByRole("searchbox"));
      expect(Math.abs(search.x - title.x), "table search aligns with its card heading without a repeated inset").toBeLessThanOrEqual(1);
    }
    await shot(page, `${app}-desktop`);

    // 位置突变会让下面滚动断言失败，防止仅检查 CSS 类名的假绿。
    if (process.env.CALL_CARDS_MUTANT === "static") await filters.evaluate(el => { (el as HTMLElement).style.position = "static"; });
    const header = page.locator(".shell-view-header");
    for (const distance of [700, 1100, 500]) {
      await scroll(page, distance);
      await expect.poll(async () => (await rect(filters)).y).toBeGreaterThanOrEqual(0);
      const f = await rect(filters), h = await rect(header);
      expect(f.y, "filters stay below the shell header while scrolling down and up").toBeGreaterThanOrEqual(h.y + h.height + 4);
      expect(f.y, "filters stay near the top of the viewport").toBeLessThanOrEqual(h.y + h.height + 16);
      expect(Math.abs(f.x - (await rect(records)).x)).toBeLessThanOrEqual(1);
    }
    await shot(page, `${app}-sticky`);
    await filters.getByRole("button", { name: "更多筛选", exact: true }).click();
    await expect(filters.getByRole("button", { name: "更多筛选", exact: true })).toHaveAttribute("aria-expanded", "true");
    await expect(filters.getByText("API 操作", { exact: true })).toBeVisible();
    await shot(page, `${app}-advanced-sticky`);
    await filters.getByRole("button", { name: "更多筛选", exact: true }).click();

    const endpoint = app === "admin" ? "/admin/pricing/charges" : "/api/usage/calls";
    await deepFilter(page, filters, records, table, endpoint, `${app}-1440`);
    const filtered = page.waitForResponse(response => new URL(response.url()).pathname === endpoint && new URL(response.url()).searchParams.get("outcome") === "succeeded");
    await chooseSelect(filters.getByRole("combobox", { name: "调用结果", exact: true }), { value: "succeeded" });
    expect((await filtered).status()).toBe(200);
    await expect(table).toHaveCount(0);
    await expect(records).toContainText(app === "admin" ? "没有符合条件的计费记录。" : "所选条件下没有调用记录");
    await filters.getByRole("button", { name: "清除调用结果筛选", exact: true }).click();
    await expect(table.locator("tbody > tr")).toHaveCount(25);
    await scroll(page, 0);
    const row = table.locator("tbody > tr").first();
    await expect(row).toContainText("同目标重试 2 次");
    await expect(row.getByLabel("响应性能", { exact: true }).getByText("输出 TPS", { exact: true }).locator("..").locator("dd")).toHaveText("—");
    const details = app === "admin" ? row.getByRole("link", { name: /^查看调用日志/ }) : row.getByRole("button", { name: "查看调用详情", exact: true });
    await details.click();
    if (app === "tenant") {
      const drawer = page.getByRole("dialog", { name: "调用详情", exact: true });
      await expect(drawer).toBeVisible(); await expect(drawer).toContainText("同目标重试 2 次");
      await page.keyboard.press("Escape"); await expect(drawer).toHaveCount(0);
    } else {
      await expect(page).toHaveURL(/detail=/);
      await expect(page.getByRole("heading", { name: "调用日志详情", exact: true })).toBeVisible();
      await page.goBack(); await expect(table).toBeVisible();
    }

    const grouped = await page.evaluate(async path => { const response = await fetch(path); return { status: response.status, body: await response.json() }; }, `${endpoint}?group_requests=true&limit=200`);
    expect(grouped.status).toBe(200);
    expect(grouped.body.items).toHaveLength(36);
    expect(new Set(grouped.body.items.map((item: { request_id: string }) => item.request_id)).size).toBe(36);
    for (const item of grouped.body.items) {
      for (const field of ["attempt_count", "retry_count", "failover_count"]) expect(Object.hasOwn(item.request_group, field), field).toBe(true);
      expect(item.request_group.attempt_count).toBe(3); expect(item.request_group.retry_count).toBe(2); expect(item.request_group.failover_count).toBe(0);
    }
    await records.getByRole("button", { name: "下一页", exact: true }).click();
    await expect(table.locator("tbody > tr")).toHaveCount(11);
    await records.getByRole("button", { name: "上一页", exact: true }).click();
    await expect(table.locator("tbody > tr")).toHaveCount(25);

    for (const viewport of [{ width: 1100, height: 900 }, { width: 390, height: 844 }, { width: 320, height: 740 }]) {
      await page.setViewportSize(viewport); await scroll(page, 0); await settle(page);
      const f = await rect(filters), r = await rect(records);
      expect(f.x).toBeGreaterThanOrEqual(0); expect(f.x + f.width).toBeLessThanOrEqual(viewport.width);
      expect(Math.abs(f.width - r.width)).toBeLessThanOrEqual(1);
      expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(viewport.width);
      const overflow = await filters.locator(".usage-record-filter-primary input, .usage-record-filter-primary button").evaluateAll(els => els.filter(el => { const b=el.getBoundingClientRect(); const p=el.closest("section")!.getBoundingClientRect(); return b.width>0 && (b.x<p.x-1 || b.right>p.right+1); }).map(el=>el.getAttribute("aria-label") || el.tagName));
      expect(overflow, "filter controls stay inside their own card").toEqual([]);
      await deepFilter(page, filters, records, table, endpoint, `${app}-${viewport.width}`);
      await scroll(page, 800);
      const stuck = await rect(filters), h = await rect(header);
      expect(stuck.y).toBeGreaterThanOrEqual(h.y+h.height+4);
      expect(stuck.y).toBeLessThanOrEqual(h.y+h.height+16);
      const failedRead = page.waitForResponse(response => new URL(response.url()).pathname === endpoint && new URL(response.url()).searchParams.get("outcome") === "failed");
      await chooseSelect(filters.getByRole("combobox", { name: "调用结果", exact: true }), { value: "failed" });
      expect((await failedRead).status()).toBe(200);
      await expect(table.locator("tbody > tr")).toHaveCount(25);
      await filters.getByRole("button", { name: "更多筛选", exact: true }).click();
      const expanded = await rect(filters);
      expect(expanded.y+expanded.height, "expanded filters leave room below for page interaction").toBeLessThanOrEqual(viewport.height-32);
      await shot(page, `${app}-${viewport.width}-expanded`);
      await filters.getByRole("button", { name: "更多筛选", exact: true }).click();
      await shot(page, `${app}-${viewport.width}-sticky`);
      await filters.getByRole("button", { name: "清除调用结果筛选", exact: true }).click();
    }
    await test.info().attach("api-requests", { body: JSON.stringify(requests), contentType: "application/json" });
    expect(requests.some(request => request === `GET ${endpoint} 200`)).toBe(true);
    expect(errors).toEqual([]);
    expect(warnings).toEqual([]);
  });
}
