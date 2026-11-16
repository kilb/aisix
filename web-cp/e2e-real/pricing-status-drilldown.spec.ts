import { expect, test, type Page } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

interface Scenario {
  name: string;
  code: string;
  modelId: string | null;
  modelLabel: string;
  reasonLabel: string;
  scopeLabel: string;
  pricingTier: string | null;
  groupId: string | null;
  eventIds: string[];
}

// 只读现成的真实事件；没有 seed、拦截、改时间、模型调用或探测。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (!state.ready || state.mockUpstream !== false) throw new Error("A ready isolated real stack is required");
const baseUrl: string = state.adminUrl;
if (!["localhost", "127.0.0.1"].includes(new URL(baseUrl).hostname)) throw new Error("Only isolated loopback origins are allowed");
const fixture = JSON.parse(readFileSync(resolve(process.env.PRICING_DRILLDOWN_FIXTURE!), "utf8")) as { cases: Scenario[] };
if (!fixture.cases.length || fixture.cases.some(c => c.eventIds.length === 0 || c.eventIds.length > 25)) {
  throw new Error("Each scenario needs 1–25 retained real event IDs currently unpriced in the last 24 hours");
}
const output = resolve(process.env.REFINED_QA_OUTPUT!);
const errors = new WeakMap<Page, string[]>();
const writes = new WeakMap<Page, string[]>();

test.beforeEach(async ({ page }) => {
  errors.set(page, []);
  writes.set(page, []);
  let signedIn = false;
  page.on("pageerror", e => errors.get(page)!.push(e.message));
  page.on("console", m => { if (signedIn && m.type() === "error") errors.get(page)!.push(m.text()); });
  page.on("request", r => {
    const path = new URL(r.url()).pathname;
    if (!['GET', 'HEAD'].includes(r.method()) && /^\/(admin|api)\//.test(path) && !['/admin/login', '/admin/logout'].includes(path)) {
      writes.get(page)!.push(`${r.method()} ${path}`);
    }
  });
  await page.goto(baseUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const login = page.waitForResponse(r => new URL(r.url()).pathname === "/admin/login" && r.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await login).status()).toBe(200);
  signedIn = true;
  await expect(page.getByRole("link", { name: "价格", exact: true })).toBeVisible();
  // 登录后的初始页面请求先结束，避免把上一页的动态窗口当成本页响应。
  await page.waitForLoadState("networkidle");
});

test.afterEach(async ({ page }, info) => {
  mkdirSync(output, { recursive: true });
  await page.screenshot({ path: join(output, `pricing-drilldown-${info.testId}.png`), fullPage: false });
  expect(errors.get(page)).toEqual([]);
  expect(writes.get(page), "No business, model or probe writes").toEqual([]);
});

for (const scenario of fixture.cases) {
  test(scenario.name, async ({ page }) => {
    const statusResponse = page.waitForResponse(r => new URL(r.url()).pathname === "/admin/pricing/status" && r.request().method() === "GET");
    await page.goto(`${baseUrl}/#pricing?section=status`);
    const response = await statusResponse;
    expect(response.status()).toBe(200);
    const status = await response.json();
    expect(status.window?.since, "Explicit pricing window start").toBeTruthy();
    expect(status.window?.until, "Explicit pricing window end").toBeTruthy();
    const matching = status.unpriced_details.items.filter((d: any) => d.code === scenario.code && d.model.id === scenario.modelId && d.service_tier === scenario.pricingTier && (d.group?.id ?? null) === scenario.groupId);
    expect(matching, "The retained real scenario must still exist; never replace it with synthetic traffic").toHaveLength(1);
    expect(matching[0].count).toBe(scenario.eventIds.length);

    const table = page.getByRole("table", { name: "未计价的调用", exact: true });
    const row = table.getByRole("row")
      .filter({ has: page.getByRole("cell", { name: scenario.reasonLabel, exact: true }) })
      .filter({ has: page.getByRole("cell", { name: scenario.modelLabel, exact: true }) })
      .filter({ has: page.getByRole("cell", { name: scenario.scopeLabel, exact: true }) });
    await expect(row).toHaveCount(1);
    const chargesResponse = page.waitForResponse(r => new URL(r.url()).pathname === "/admin/pricing/charges" && r.request().method() === "GET");
    await row.getByRole("button", { name: "查看调用", exact: true }).click();
    const charges = await chargesResponse;
    expect(charges.status()).toBe(200);
    const query = new URL(charges.url()).searchParams;
    expect(query.get("status")).toBe("unpriced");
    expect(query.get("error_code")).toBe(scenario.code);
    expect(query.get("model_id")).toBe(scenario.modelId ?? "");
    expect(query.get("priced_since")).toBe(status.window.since);
    expect(query.get("priced_before")).toBe(status.window.until);
    expect(query.get("exclude_corrected")).toBe("true");
    if (scenario.code === "missing_tier_price") expect(query.get("pricing_tier")).toBe(scenario.pricingTier ?? "");
    if (scenario.code === "missing_rate") expect(query.get("group_id")).toBe(scenario.groupId ?? "");
    const body = await charges.json();
    expect(body.items.map((r: any) => r.event_id).sort()).toEqual([...scenario.eventIds].sort());
    expect(body.next_before).toBeNull();
    await expect(page.getByRole("heading", { name: "调用记录", exact: true })).toBeVisible();
    const links = page.getByRole("table", { name: "计费明细", exact: true }).getByRole("link", { name: /^查看调用日志 / });
    await expect(links).toHaveCount(scenario.eventIds.length);
    expect((await links.evaluateAll(nodes => nodes.map(n => n.getAttribute("aria-label")!.replace("查看调用日志 ", "")))).sort()).toEqual([...scenario.eventIds].sort());
    await expect(page.getByRole("status").filter({ hasText: "只看" })).toContainText("排除已更正");
    await page.getByRole("status").filter({ hasText: "只看" }).getByRole("button", { name: "清除", exact: true }).click();
    await expect(page.getByRole("status").filter({ hasText: "只看" })).toHaveCount(0);
    const remaining = new URLSearchParams(page.url().split("?")[1] ?? "");
    for (const field of ["group_id", "pricing_tier", "priced_since", "priced_before", "exclude_corrected", "model_id", "error_code"]) expect(remaining.has(field), `Clear ${field}`).toBe(false);
  });
}
