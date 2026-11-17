import { expect, test, type Locator, type Page } from "@playwright/test";
import { existsSync, mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 预期来自请求链的用户合同和真实保留记录，不读取产品实现或生成遥测。
const session = resolve(process.env.REQUEST_GROUPS_QA_SESSION!);
const output = resolve(process.env.REQUEST_GROUPS_QA_OUTPUT!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (state.ready !== true || state.mockUpstream !== false || state.realBackend !== true) {
  throw new Error("Request grouping E2E requires a ready real isolated backend with retained gateway records");
}
for (const origin of [state.adminUrl, state.tenantUrl]) {
  if (!["127.0.0.1", "localhost"].includes(new URL(origin).hostname)) throw new Error("Only isolated loopback origins are allowed");
}
const tenantId: string = auth.metricsTenantId ?? auth.tenantId;
const tenantOwner = auth.metricsOwner ?? auth.owner;
if (!tenantId || !tenantOwner) throw new Error("The retained tenant and its real owner credentials are required");

type Surface = "admin" | "tenant";
interface GroupCase {
  kind: "succeeded" | "failed";
  requestId: string;
  eventId: string;
  tenantId: string;
  attemptCount: number;
  failoverCount: number;
  retryCount: number;
}
interface StoredRecord {
  event_id: string;
  seq: number;
  dp_id: string;
  api_key_id: string | null;
  request_id: string | null;
  occurred_at: string;
  event: Record<string, unknown>;
}
interface AttemptRecord {
  event_id: string;
  attempt_index: number;
  attempt_kind: string;
  is_final_attempt: boolean | null;
  error_message: string | null;
  stored_event?: StoredRecord;
}
interface GroupSummary {
  attempt_count: number;
  failover_count: number;
  retry_count: number;
  charge_micro_cny: string | null;
  adjusted_micro_cny: string;
  complete: boolean;
}
interface GroupRow extends AttemptRecord {
  tenant_id?: string;
  status_code?: number | null;
  error_class?: string | null;
  status: string;
  request_group: GroupSummary;
  attempts?: AttemptRecord[];
  next_attempt_after?: number | null;
}
interface ChargeRecord {
  event_id: string;
  charge_micro_cny: number | null;
  adjusted_micro_cny: number | null;
}
interface CaseOracle {
  scenario: GroupCase;
  records: StoredRecord[];
  representative: string;
  originalTotal: string | null;
  adjustedTotal: string;
  from: string;
  to: string;
}
interface Ledger { authenticated: boolean; errors: string[]; writes: string[]; calls: { method: string; path: string; query: string }[] }
const ledgers = new WeakMap<Page, Ledger>();
let oracles: CaseOracle[];

function queryPath(path: string, fields: Record<string, string | number | boolean>): string {
  return path + "?" + new URLSearchParams(Object.entries(fields).map(([key, value]) => [key, String(value)]));
}
async function jsonGet<T>(page: Page, path: string): Promise<T> {
  if (!path.startsWith("/admin/") && !path.startsWith("/api/")) throw new Error("The oracle uses same-origin application routes only");
  const response = await page.evaluate(async target => {
    const answer = await fetch(target, { credentials: "include" });
    return { status: answer.status, body: await answer.json() };
  }, path);
  expect(response.status, path).toBe(200);
  return response.body as T;
}
async function settle(page: Page): Promise<void> {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
}
async function login(page: Page, surface: Surface): Promise<void> {
  const base = surface === "admin" ? state.adminUrl : state.tenantUrl;
  const who = surface === "admin" ? auth.operator : tenantOwner;
  await page.goto(base);
  await page.getByLabel("邮箱", { exact: true }).fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  const response = page.waitForResponse(answer => new URL(answer.url()).pathname === (surface === "admin" ? "/admin/login" : "/api/login") && answer.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status()).toBe(200);
  const ledger = ledgers.get(page);
  if (ledger) ledger.authenticated = true;
  await settle(page);
}
function succeeded(event: Record<string, unknown>): boolean {
  return typeof event.status_code === "number" && event.status_code >= 200 && event.status_code < 300 && !(typeof event.error_class === "string" && event.error_class.length > 0);
}
function assertGroup(row: GroupRow, oracle: CaseOracle): void {
  expect(row.event_id).toBe(oracle.representative);
  for (const [field, value] of Object.entries({
    attempt_count: oracle.scenario.attemptCount,
    failover_count: oracle.scenario.failoverCount,
    retry_count: oracle.scenario.retryCount,
    charge_micro_cny: oracle.originalTotal,
    adjusted_micro_cny: oracle.adjustedTotal,
    complete: true,
  })) {
    expect(Object.hasOwn(row.request_group, field), `explicit ${field}`).toBe(true);
    expect(row.request_group[field as keyof GroupSummary], field).toBe(value);
  }
}
function assertTenantSafe(value: unknown): void {
  if (Array.isArray(value)) { value.forEach(assertTenantSafe); return; }
  if (value === null || typeof value !== "object") return;
  for (const [key, child] of Object.entries(value)) {
    expect([
      "cost_micro_cny", "cost_micro_usd", "provider_key_id", "upstream_account_id",
      "upstream_account_name", "upstream_model", "model_id", "stored_event",
    ], `tenant field ${key}`).not.toContain(key);
    assertTenantSafe(child);
  }
}

async function expectFee(row: Locator, surface: Surface, expected: string | null): Promise<void> {
  const amount = surface === "admin" ? row.locator('td[data-label="费用（元）"] .finance-amount-value') : row.locator('td[data-label="费用"]');
  await expect(amount).toBeVisible();
  if (expected === null) {
    await expect(amount).toContainText("未知");
    return;
  }
  const match = (await amount.innerText()).match(/[¥￥](-?[\d,]+)(?:\.(\d+))?/);
  expect(match, "the list displays the whole request's recorded CNY fee").not.toBeNull();
  const integer = match![1].replace(/,/g, "");
  const fraction = match![2] ?? "";
  expect(fraction.length, "stored micro-CNY remains exact").toBeLessThanOrEqual(6);
  const magnitude = BigInt(integer.replace(/^-/, "")) * 1_000_000n + BigInt(fraction.padEnd(6, "0"));
  expect((integer.startsWith("-") ? -magnitude : magnitude).toString()).toBe(expected);
}

async function readAll<T>(page: Page, path: string, fields: Record<string, string>): Promise<T[]> {
  const records: T[] = [];
  let before: number | null = null;
  for (let count = 0; count < 50; count += 1) {
    const pageOf: { items: T[]; next_before: number | null } = await jsonGet(page, queryPath(path, { ...fields, limit: 500, ...(before === null ? {} : { before }) }));
    records.push(...pageOf.items);
    if (pageOf.next_before === null) return records;
    expect(pageOf.next_before).not.toBe(before);
    before = pageOf.next_before;
  }
  throw new Error("Real retained request exceeds the bounded oracle scan; supply a narrower explicit case");
}
async function casesFromRetained(page: Page): Promise<GroupCase[]> {
  const configured = process.env.REQUEST_GROUPS_QA_CASES ?? state.requestGroupsCases;
  const file = configured ? resolve(session, configured) : join(session, "group-cases.json");
  if (existsSync(file)) {
    const supplied = JSON.parse(readFileSync(file, "utf8"));
    if (!Array.isArray(supplied)) throw new Error("The real group-cases file must contain a JSON array");
    return supplied as GroupCase[];
  }
  const rows = await readAll<GroupRow>(page, "/admin/pricing/charges", { tenant_id: tenantId, group_requests: "true" });
  return (["succeeded", "failed"] as const).map(kind => {
    const sample = rows.find(row => row.request_group.complete && row.request_group.attempt_count > 1 && row.request_group.failover_count > 0 && row.is_final_attempt === true && (kind === "succeeded" ? succeeded(row as unknown as Record<string, unknown>) : !succeeded(row as unknown as Record<string, unknown>)));
    if (!sample) throw new Error(`Real retained ${kind} multi-attempt failover request is required; telemetry fixtures are forbidden`);
    return { kind, requestId: "", eventId: sample.event_id, tenantId, attemptCount: sample.request_group.attempt_count, failoverCount: sample.request_group.failover_count, retryCount: sample.request_group.retry_count };
  });
}

test.beforeAll(async ({ browser }) => {
  mkdirSync(output, { recursive: true });
  const context = await browser.newContext();
  const page = await context.newPage();
  try {
    await login(page, "admin");
    const cases = await casesFromRetained(page);
    expect(cases.filter(scenario => scenario.kind === "succeeded").length).toBeGreaterThan(0);
    expect(cases.filter(scenario => scenario.kind === "failed").length).toBeGreaterThan(0);
    oracles = [];
    for (const scenario of cases) {
      expect(scenario.tenantId, "the real tenant owner must be able to inspect this case").toBe(tenantId);
      const anchor = await jsonGet<StoredRecord>(page, `/admin/usage/events/${encodeURIComponent(scenario.eventId)}`);
      expect(anchor.event.trace_id, "group cases need a recorded correlation identity").toEqual(expect.stringMatching(/\S/));
      const requestId = scenario.requestId || anchor.request_id;
      expect(requestId, "real recorded request identity").toBeTruthy();
      const all = await readAll<StoredRecord>(page, "/admin/usage/events", { request_id: requestId! });
      const records = all.filter(record => record.dp_id === anchor.dp_id && record.api_key_id === anchor.api_key_id && record.event.trace_id === anchor.event.trace_id).sort((left, right) => Number(left.event.attempt_index ?? 0) - Number(right.event.attempt_index ?? 0) || left.seq - right.seq);
      expect(records.length).toBe(scenario.attemptCount);
      expect(records.length).toBeGreaterThan(1);
      expect(records.filter(record => record.event.attempt_kind === "fallback").length).toBe(scenario.failoverCount);
      expect(scenario.failoverCount).toBeGreaterThan(0);
      expect(records.filter(record => record.event.attempt_kind === "retry").length).toBe(scenario.retryCount);
      const terminal = records.filter(record => record.event.is_final_attempt === true);
      expect(terminal.length, "real requests have one recorded terminal attempt").toBe(1);
      expect(succeeded(terminal[0].event)).toBe(scenario.kind === "succeeded");
      const rawCharges = await readAll<ChargeRecord>(page, "/admin/pricing/charges", { request_id: requestId! });
      const charges = rawCharges.filter(charge => records.some(record => record.event_id === charge.event_id));
      expect(charges.length).toBe(records.length);
      const adjusted = charges.reduce((sum, charge) => sum + BigInt(charge.adjusted_micro_cny ?? 0), 0n);
      const original = charges.some(charge => charge.charge_micro_cny === null) ? null : (charges.reduce((sum, charge) => sum + BigInt(charge.charge_micro_cny!), 0n) + adjusted).toString();
      const times = records.map(record => Date.parse(record.occurred_at));
      oracles.push({ scenario: { ...scenario, requestId: requestId! }, records, representative: terminal[0].event_id, originalTotal: original, adjustedTotal: adjusted.toString(), from: new Date(Math.min(...times) - 60_000).toISOString(), to: new Date(Math.max(...times) + 60_000).toISOString() });
    }
  } finally {
    await context.close();
  }
});

test.beforeEach(async ({ page }) => {
  const ledger: Ledger = { authenticated: false, errors: [], writes: [], calls: [] };
  ledgers.set(page, ledger);
  page.on("pageerror", cause => ledger.errors.push(cause.message));
  page.on("console", message => { if (ledger.authenticated && message.type() === "error") ledger.errors.push(message.text()); });
  page.on("request", request => {
    const url = new URL(request.url());
    if (!/^\/(?:api|admin)\//.test(url.pathname)) return;
    ledger.calls.push({ method: request.method(), path: url.pathname, query: url.search });
    if (!["GET", "HEAD"].includes(request.method()) && !/^\/(?:api|admin)\/(?:login|logout)$/.test(url.pathname)) ledger.writes.push(request.method() + " " + url.pathname);
  });
  page.on("response", response => {
    const url = new URL(response.url());
    if (ledger.authenticated && /^\/(?:api|admin)\//.test(url.pathname) && response.status() >= 400) ledger.errors.push(`HTTP ${response.status()} ${url.pathname}`);
  });
});
test.afterEach(async ({ page }) => {
  const ledger = ledgers.get(page)!;
  await test.info().attach("real-read-only-request-ledger", { body: JSON.stringify(ledger.calls), contentType: "application/json" });
  expect(ledger.writes, "log inspection never sends business, probe or inference writes").toEqual([]);
  expect(ledger.errors, "unexpected application/HTTP errors").toEqual([]);
});

async function expectFullChain(scope: Page | Locator, surface: Surface, oracle: CaseOracle, detail: GroupRow): Promise<void> {
  const chain = scope.getByRole("region", { name: "故障转移与重试链", exact: true });
  await expect(chain).toBeVisible();
  await expect(chain.locator("article[data-attempt-event]")).toHaveCount(oracle.records.length);
  const attempts = detail.attempts!;
  expect(attempts.map(attempt => attempt.event_id).sort()).toEqual(oracle.records.map(record => record.event_id).sort());
  for (const record of oracle.records) {
    const attempt = attempts.find(value => value.event_id === record.event_id)!;
    const article = chain.locator(`article[data-attempt-event=${JSON.stringify(record.event_id)}]`);
    await expect(article).toBeVisible();
    await expect(article).toContainText(record.event_id);
    const error = surface === "admin" ? record.event.error_message : attempt.error_message;
    if (typeof error === "string" && error.length > 0) {
      if (surface === "admin") {
        const errorLog = article.getByRole("region", { name: /错误信息$/ });
        await expect(errorLog).toHaveCount(1);
        await expect(errorLog.locator("pre")).toBeVisible();
        await expect(errorLog.locator("pre")).toHaveText(error);
      } else {
        await expect(article.getByText(error, { exact: true })).toBeVisible();
      }
    }
    const json = surface === "admin" ? article.getByRole("region", { name: /事件 JSON$/ }) : article.locator("details");
    await expect(json).toHaveCount(1);
    const pre = json.locator("pre");
    if (!(await pre.isVisible())) await json.locator("summary").click();
    await expect(pre).toHaveText(JSON.stringify(surface === "admin" ? record : attempt, null, 2));
  }
}

for (const surface of ["admin", "tenant"] as const) for (const width of [1440, 390]) {
  test(`${surface} grouped success and all-failed chains stay complete at ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    await login(page, surface);
    const base = surface === "admin" ? state.adminUrl : state.tenantUrl;
    const endpoint = surface === "admin" ? "/admin/pricing/charges" : "/api/usage/calls";
    for (const oracle of oracles) {
      const params = new URLSearchParams({ request_id: oracle.scenario.requestId, from: oracle.from, to: oracle.to });
      const response = page.waitForResponse(answer => {
        const url = new URL(answer.url());
        return url.pathname === endpoint && url.searchParams.get("request_id") === oracle.scenario.requestId && url.searchParams.get("group_requests") === "true" && url.searchParams.get("include_attempts") !== "true";
      });
      await page.goto(`${base}/#calls?${params}`);
      const listed = await response;
      expect(listed.status()).toBe(200);
      const body = await listed.json() as { items: GroupRow[]; next_before: number | null };
      expect(body.items.length, "all attempts of this real request occupy one list row").toBe(1);
      assertGroup(body.items[0], oracle);
      expect(body.next_before).toBeNull();
      await settle(page);
      const table = page.getByRole("table", { name: surface === "admin" ? "计费明细" : "调用记录", exact: true });
      const row = table.locator("tbody > tr").filter({ hasText: oracle.scenario.requestId });
      await expect(row).toHaveCount(1);
      await expect(row).toContainText(`故障转移 ${oracle.scenario.failoverCount} 次`);
      if (oracle.scenario.retryCount > 0) await expect(row).toContainText(`同目标重试 ${oracle.scenario.retryCount} 次`);
      if (surface === "admin") await expect(row.locator(".call-outcome")).toHaveText(oracle.scenario.kind === "succeeded" ? "成功" : "失败");
      else if (oracle.scenario.kind === "succeeded") await expect(row.getByText("成功", { exact: true })).toBeVisible();
      else await expect(row.getByText(/^失败（[^）]+）$|^失败$/)).toBeVisible();
      await expectFee(row, surface, oracle.originalTotal);

      const detail = await jsonGet<{ items: GroupRow[] }>(page, queryPath(endpoint, { group_requests: true, include_attempts: true, event_id: oracle.scenario.eventId }));
      expect(detail.items.length).toBe(1);
      assertGroup(detail.items[0], oracle);
      expect(detail.items[0].next_attempt_after).toBeNull();
      if (surface === "tenant") assertTenantSafe(detail);
      if (surface === "admin") {
        await row.getByRole("link", { name: `查看调用日志 ${oracle.representative}`, exact: true }).click();
        await expect(page.getByRole("heading", { name: "调用日志详情", exact: true })).toBeVisible();
        await expectFullChain(page, surface, oracle, detail.items[0]);
        const directUrl = page.url();
        expect(new URLSearchParams(directUrl.split("?")[1]).get("detail")).toBe(oracle.representative);
        await page.reload();
        await expectFullChain(page, surface, oracle, detail.items[0]);
        await page.screenshot({ path: join(output, `${surface}-${width}-${oracle.scenario.kind}-chain.png`), fullPage: false });
        await page.getByRole("link", { name: "返回调用记录", exact: true }).click();
        await expect(page.getByRole("table", { name: "计费明细", exact: true })).toBeVisible();
        expect(new URLSearchParams(page.url().split("?")[1]).get("request_id")).toBe(oracle.scenario.requestId);
      } else {
        await row.getByRole("button", { name: "查看调用详情", exact: true }).click();
        const dialog = page.getByRole("dialog", { name: "调用详情", exact: true });
        await expectFullChain(dialog, surface, oracle, detail.items[0]);
        for (const forbidden of ["上游账号", "上游账号 ID", "实际模型 ID", "成本（元）"]) await expect(dialog.getByText(forbidden, { exact: true })).toHaveCount(0);
        await page.screenshot({ path: join(output, `${surface}-${width}-${oracle.scenario.kind}-chain.png`), fullPage: false });
        await dialog.getByRole("button", { name: "关闭调用详情", exact: true }).click();
        await expect(dialog).toHaveCount(0);
      }

      const collected: string[] = [];
      let after: number | null = null;
      for (let number = 0; number <= oracle.records.length; number += 1) {
        const part: { items: GroupRow[] } = await jsonGet(page, queryPath(endpoint, { group_requests: true, include_attempts: true, event_id: oracle.scenario.eventId, attempt_limit: 1, ...(after === null ? {} : { attempts_after: after }) }));
        assertGroup(part.items[0], oracle);
        expect(part.items[0].attempts!.length).toBe(1);
        collected.push(part.items[0].attempts![0].event_id);
        const next = part.items[0].next_attempt_after;
        expect(next, "explicit pagination end or next cursor").not.toBeUndefined();
        if (next === null) break;
        expect(next).not.toBe(after);
        after = next!;
      }
      expect(collected.sort()).toEqual(oracle.records.map(record => record.event_id).sort());
      expect(new Set(collected).size).toBe(oracle.records.length);
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= document.documentElement.clientWidth + 1)).toBe(true);
    }
  });
}
