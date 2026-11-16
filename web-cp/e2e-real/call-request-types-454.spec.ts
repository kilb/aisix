import { expect, test, type Page } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 场景由真实网关请求及后台摄取产生；历史未知记录来自保留快照。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
interface Scenario { kind: "stream" | "nonstream" | "unknown"; eventId: string; requestId: string }
const fixture = JSON.parse(readFileSync(process.env.CALL_TYPES_QA_FIXTURE!, "utf8")) as { cases: Scenario[] };
if (state.ready !== true || state.mockUpstream !== false || state.realBackend !== true) throw new Error("A ready real isolated backend is required");
for (const origin of [state.adminUrl, state.tenantUrl]) if (!["127.0.0.1", "localhost"].includes(new URL(origin).hostname)) throw new Error("Only isolated local URLs are allowed");
for (const kind of ["stream", "nonstream", "unknown"]) if (fixture.cases.filter(item => item.kind === kind).length !== 1) throw new Error("Real stream/nonstream/legacy cases are required");

async function login(page: Page, surface: "admin" | "tenant"): Promise<void> {
  const origin = surface === "admin" ? state.adminUrl : state.tenantUrl;
  const who = surface === "admin" ? auth.operator : auth.metricsOwner;
  await page.goto(origin);
  await page.getByLabel("邮箱", { exact: true }).fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  const response = page.waitForResponse(reply => new URL(reply.url()).pathname === (surface === "admin" ? "/admin/login" : "/api/login") && reply.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status()).toBe(200);
  await page.waitForLoadState("networkidle");
}

for (const surface of ["admin", "tenant"] as const) for (const width of [1440, 390]) {
  test(`#93 ${surface} time leads and all three actual request types remain exact at ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    const errors: string[] = [];
    const writes: string[] = [];
    let authenticated = false;
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => {
      if (authenticated && message.type() === "error") errors.push("console: " + message.text());
    });
    page.on("response", response => {
      const path = new URL(response.url()).pathname;
      if (authenticated && /^\/(?:api|admin)\//.test(path) && response.status() >= 400) errors.push(`HTTP ${response.status()} ${path}`);
    });
    page.on("request", request => {
      const path = new URL(request.url()).pathname;
      if (!["GET", "HEAD"].includes(request.method()) && !/^\/(?:api|admin)\/(?:login|logout)$/.test(path)) writes.push(request.method() + " " + path);
    });
    await login(page, surface);
    authenticated = true;
    const base = surface === "admin" ? state.adminUrl : state.tenantUrl;
    for (const scenario of fixture.cases) {
      const query = new URLSearchParams(surface === "admin" ? { event_id: scenario.eventId } : { request_id: scenario.requestId });
      const path = surface === "admin" ? "/admin/pricing/charges" : "/api/usage/calls";
      const response = page.waitForResponse(reply => new URL(reply.url()).pathname === path && new URL(reply.url()).searchParams.get(surface === "admin" ? "event_id" : "request_id") === (surface === "admin" ? scenario.eventId : scenario.requestId));
      await page.goto(`${base}/#calls?${query}`);
      const reply = await response;
      expect(reply.status()).toBe(200);
      const body = await reply.json();
      const record = body.items.find((item: { event_id: string }) => item.event_id === scenario.eventId);
      expect(record, "同一真实事件必须出现在实际列表响应").toBeDefined();
      if (scenario.kind === "unknown") expect(record.stream == null).toBe(true);
      else {
        expect(Object.hasOwn(record, "stream"), "缺失字段不能冒充false").toBe(true);
        expect(record.stream).toBe(scenario.kind === "stream");
      }
      const table = page.getByRole("table", { name: surface === "admin" ? "计费明细" : "调用记录", exact: true });
      await expect(table.getByRole("columnheader").first()).toHaveText("时间");
      const typeIndex = await table.getByRole("columnheader", { name: "请求类型", exact: true }).evaluate(element => (element as HTMLTableCellElement).cellIndex);
      const row = table.locator("tbody > tr").filter({ has: page.locator("td[data-label=请求类型]") });
      await expect(row).toHaveCount(1);
      await expect(row.getByRole("cell").nth(typeIndex)).toHaveText(scenario.kind === "stream" ? "流" : scenario.kind === "nonstream" ? "非流" : "未知");
      const time = row.getByRole("cell").first().locator("time");
      await expect(time).toHaveAttribute("datetime", record.occurred_at);
      await expect(time.locator("span")).toHaveCount(2);
      for (const line of await time.locator("span").all()) await expect(line).toHaveCSS("display", "block");
      await page.evaluate(() => document.fonts.ready);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1)).toBe(true);
      await expect(page.locator("vite-error-overlay")).toHaveCount(0);
      await page.screenshot({ path: join(process.env.REFINED_QA_OUTPUT!, `call-types-${surface}-${width}-${scenario.kind}.png`), fullPage: false });
    }
    expect(writes).toEqual([]);
    expect(errors).toEqual([]);
  });
}
