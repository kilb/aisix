import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const OUTPUT_TPS_TITLE = /^输出 Token（含推理，不含输入）÷ 完整上游耗时(?:；.+)?$/;

interface Scenario { kind: "positive" | "missing"; event_id: string; request_id: string; tenant_auth: "metricsOwner" }
interface CallRecord {
  event_id: string; request_id: string; first_token_latency_ms: number | null; attempt_duration_ms: number | null; throughput_tps: number | null;
  latency_ms: number | null;
  usage_input_tokens: number | null; usage: Record<string, number | null> | null;
  readonly request_timing?: Readonly<Record<string, unknown>> | null;
  readonly upstream_timing?: Readonly<Record<string, unknown>> | null;
  charge: { status: string; calculation: { final_micro_cny: string | null } };
}
function readJson(path: string): any {
  try { return JSON.parse(readFileSync(path, "utf8")); }
  catch { throw new Error("Real drawer acceptance state could not be read"); }
}
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = readJson(join(session, "public.json"));
const auth = readJson(join(session, "local-auth.json"));
const output = resolve(process.env.REFINED_QA_OUTPUT!);
if (state.ready !== true || state.realBackend !== true || state.mockUpstream !== false) throw new Error("Real isolated backend is required");
for (const field of ["tenantUrl", "adminUrl", "httpUrl"]) {
  const url = new URL(state[field]);
  if (!["http:", "https:"].includes(url.protocol) || !["localhost", "127.0.0.1", "[::1]"].includes(url.hostname) || url.username || url.password) throw new Error("Only isolated loopback acceptance URLs are allowed");
}
// 候选前端通过独立预览端口连接同一个真实后端，原世界文件保持只读。
const baseUrl: string = process.env.CONSOLE_TEMPLATE_TENANT_URL ?? state.tenantUrl;
if (!["localhost", "127.0.0.1", "[::1]"].includes(new URL(baseUrl).hostname)) throw new Error("Only isolated preview URLs are allowed");
const scenarios = state.byteTokenEvidence?.cases as Scenario[] | undefined;
if (!Array.isArray(scenarios) || !scenarios.some(item => item.kind === "positive") || !scenarios.some(item => item.kind === "missing") || scenarios.some(item => item.tenant_auth !== "metricsOwner" || !item.event_id || !item.request_id)) throw new Error("Real positive and missing call evidence is required");
if (!state.tokenUiFixture?.apiKeyId || !state.tokenUiFixture?.tenantId) throw new Error("Real tenant and API key scope is required");
const positive = scenarios.find(item => item.kind === "positive")!;
const missing = scenarios.find(item => item.kind === "missing")!;
const runtimeErrors = new WeakMap<Page, string[]>();
const apiRequests = new WeakMap<Page, string[]>();
const authenticated = new WeakMap<Page, boolean>();

test.beforeEach(async ({ page }) => {
  const errors: string[] = [];
  const reads: string[] = [];
  runtimeErrors.set(page, errors); apiRequests.set(page, reads); authenticated.set(page, false);
  page.on("pageerror", () => errors.push("pageerror"));
  page.on("console", message => {
    if (message.type() !== "error") return;
    const bootstrap = !authenticated.get(page) && /401|Unauthorized/.test(message.text()) && /\/(?:api\/me|api\/session)(?:\?|$)/.test(message.location().url);
    if (!bootstrap) errors.push("console error");
  });
  page.on("response", response => {
    const path = new URL(response.url()).pathname;
    if (response.status() < 400 || !/^\/(?:api|admin)\//.test(path)) return;
    if (response.status() === 401 && !authenticated.get(page) && ["/api/me", "/api/session"].includes(path)) return;
    errors.push(`HTTP ${response.status()} ${path}`);
  });
  page.on("request", request => {
    const url = new URL(request.url());
    if (/^\/(?:api|admin)\//.test(url.pathname)) reads.push(`${request.method()} ${url.pathname}${url.search}`);
  });
  const who = auth.metricsOwner;
  if (typeof who?.email !== "string" || typeof who?.password !== "string") throw new Error("Isolated owner login is required");
  await page.goto(`${baseUrl.replace(/\/$/, "")}/login`);
  await expect(page.getByLabel("邮箱", { exact: true })).toBeVisible();
  try {
    await page.getByLabel("邮箱", { exact: true }).fill(who.email);
    await page.getByLabel("口令", { exact: true }).fill(who.password);
  } catch { throw new Error("Isolated owner credentials could not be entered"); }
  const response = page.waitForResponse(answer => new URL(answer.url()).pathname === "/api/login" && answer.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await response).status()).toBe(200);
  authenticated.set(page, true);
});

test.afterEach(async ({ page }) => {
  expect(runtimeErrors.get(page)).toEqual([]);
  expect(apiRequests.get(page)!.filter(request => !request.startsWith("GET ") && !/^POST \/api\/(?:login|logout)$/.test(request)), "only explicit local session writes are allowed").toEqual([]);
  const status = await page.evaluate(async () => (await fetch("/api/logout", { method: "POST", headers: { "content-type": "application/json" }, body: "{}" })).status);
  expect(status).toBe(204);
});

async function showCall(page: Page, scenario: Scenario): Promise<{ row: Locator; call: CallRecord }> {
  const query = new URLSearchParams({ api_key_id: state.tokenUiFixture.apiKeyId, request_id: scenario.request_id });
  await page.goto(`${baseUrl}/#calls?${query}`);
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
  expect(new URL(page.url()).hash).toContain("#calls?");
  await expect(page).toHaveTitle("Routio 组织门户");
  await expect(page.getByRole("heading", { name: "调用记录", exact: true }).first()).toBeVisible();
  const answer = await page.evaluate(async path => {
    const response = await fetch(path, { credentials: "include" });
    return { status: response.status, body: await response.json() };
  }, `/api/usage/calls?${query}`);
  expect(answer.status).toBe(200);
  const matches = answer.body.items.filter((item: CallRecord) => item.event_id === scenario.event_id);
  expect(matches).toHaveLength(1);
  const call = matches[0] as CallRecord;
  expect(call.request_id).toBe(scenario.request_id);
  const row = page.getByRole("table", { name: "调用记录", exact: true }).getByRole("row").filter({ hasText: scenario.request_id });
  await expect(row).toHaveCount(1);
  await expect(row).toBeVisible();
  return { row, call };
}
function definition(area: Locator, label: string): Locator {
  return area.getByText(label, { exact: true }).locator("..").locator("dd");
}

const UPSTREAM_TIMING_FIELDS = [
  "http_send_started_ms", "response_headers_ms", "first_response_byte_ms",
  "first_protocol_frame_ms", "first_canonical_chunk_ms", "upstream_first_content_ms", "first_content_ms",
] as const;

/** 只按真实 API 原值核对原始偏移，缺省和非法值不能从其他时钟补齐。 */
async function expectUpstreamTiming(scope: Locator, raw: unknown): Promise<void> {
  const region = scope.getByRole("region", { name: "上游 HTTP 阶段", exact: true });
  await expect(region).toHaveCount(1);
  await expect(region).toBeVisible();
  await expect(region.locator("[data-upstream-timing-field]")).toHaveCount(UPSTREAM_TIMING_FIELDS.length);
  await expect(region.locator('[data-upstream-timing-field="upstream_first_content_ms"] dt')).toHaveText("上游首个内容帧");
  await expect(region.locator('[data-upstream-timing-field="first_content_ms"] dt')).toHaveText("首次内容交付");
  const timing = raw !== null && typeof raw === "object" && !Array.isArray(raw) ? raw as Readonly<Record<string, unknown>> : null;
  for (const name of UPSTREAM_TIMING_FIELDS) {
    const slot = region.locator(`[data-upstream-timing-field="${name}"]`);
    await expect(slot).toHaveCount(1);
    const value = timing?.[name];
    const expected = typeof value === "number" && Number.isInteger(value) && value >= 0 && value <= 4_294_967_295
      ? `${new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 }).format(value)} ms` : "—";
    await expect(slot.locator("dd")).toHaveText(expected);
  }
  await expect(region).toContainText("各值均相对同一次上游尝试起点，按原始毫秒展示，不计算阶段差值");
  await expect(region).toContainText("不代表操作系统实际写包");
  await expect(region).toContainText("不代表客户端已收到或确认");
}

async function expectDuration(value: Locator, milliseconds: number | null): Promise<void> {
  if (milliseconds === null) { await expect(value).toHaveText("—"); return; }
  const shown = (await value.innerText()).match(/^([\d,.]+) (ms|s)$/);
  expect(shown, "recorded duration must retain its unit").not.toBeNull();
  const multiplier = shown![2] === "s" ? 1000 : 1;
  expect(Math.abs(Number(shown![1]!.replaceAll(",", "")) * multiplier - milliseconds)).toBeLessThanOrEqual(multiplier / 200 + 0.0001);
}
async function expectCharge(value: Locator, finalMicroCny: string): Promise<void> {
  const shown = (await value.innerText()).match(/(-)?¥([\d,]+)(?:\.(\d+))?/);
  expect(shown, "charged amount must show a precise currency value").not.toBeNull();
  expect((shown![3] ?? "").length).toBeLessThanOrEqual(6);
  const micro = (BigInt(shown![2]!.replaceAll(",", "")) * 1_000_000n + BigInt((shown![3] ?? "").padEnd(6, "0"))) * (shown![1] ? -1n : 1n);
  expect(micro.toString()).toBe(finalMicroCny);
}
async function screenshot(page: Page, name: string): Promise<void> {
  mkdirSync(join(output, "screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "screenshots", name), fullPage: false });
}

// 列表使用真实调用的API字段逐项核对；缺失不能被默认为零或由总用时推算。
test("real-call-list-preserves-measured-performance-token-usage-and-unknowns", async ({ page }) => {
  for (const scenario of [positive, missing]) {
    const { row, call } = await showCall(page, scenario);
    for (const field of ["first_token_latency_ms", "attempt_duration_ms", "throughput_tps", "usage_input_tokens", "usage"]) expect(Object.hasOwn(call, field), field).toBe(true);
    const tokens = row.getByLabel("记录 Token 消耗", { exact: true });
    for (const [label, value] of [["输入", call.usage_input_tokens], ["输出", call.usage?.completion_tokens]] as const) {
      await expect(definition(tokens, label)).toHaveText(value == null ? "—" : `${new Intl.NumberFormat("en-US").format(value)} Token`);
    }
    const performance = row.getByLabel("响应性能", { exact: true });
    await expect(definition(performance, "输出 TPS").locator("xpath=..")).toHaveAttribute("title", OUTPUT_TPS_TITLE);
    await expectDuration(definition(performance, "请求首内容"), call.first_token_latency_ms);
    await expectDuration(definition(performance, "本次上游耗时"), call.attempt_duration_ms);
    if (scenario.kind === "positive") {
      expect(call.first_token_latency_ms).not.toBeNull(); expect(call.first_token_latency_ms!).toBeGreaterThanOrEqual(0);
      expect(call.throughput_tps).not.toBeNull(); expect(call.throughput_tps!).toBeGreaterThan(0);
      expect(Number((await definition(performance, "输出 TPS").innerText()).replaceAll(",", ""))).toBeCloseTo(call.throughput_tps!, 2);
      expect(call.usage_input_tokens).toBeGreaterThan(0); expect(call.usage?.completion_tokens).toBeGreaterThan(0);
      expect(call.charge.status).toBe("charged"); expect(call.charge.calculation.final_micro_cny).not.toBeNull();
      await expectCharge(row.locator('td[data-label="费用"]'), call.charge.calculation.final_micro_cny!);
    } else {
      expect(call.first_token_latency_ms).toBeNull(); expect(call.throughput_tps).toBeNull();
      await expect(definition(performance, "输出 TPS")).toHaveText("—");
    }
    await expect(row).toContainText("按字节估算 Token");
    await screenshot(page, `tenant-call-list-1440-${scenario.kind}.png`);
  }
  await expect(page.getByText(/旧记录未采集/)).toBeVisible();
});

// 同一次列表浏览中重开详情复用已读尝试；键盘与遮罩关闭均保留触发按钮焦点。
test("real-call-drawer-keeps-fee-details-keyboard-focus-and-no-extra-requests", async ({ page }) => {
  const { row, call } = await showCall(page, positive);
  const trigger = row.getByRole("button", { name: "查看调用详情", exact: true });
  const before = apiRequests.get(page)!.length;
  const response = page.waitForResponse(reply => {
    const url = new URL(reply.url());
    return reply.ok() && url.pathname === "/api/usage/calls" && url.searchParams.get("event_id") === call.event_id && url.searchParams.get("include_attempts") === "true";
  });
  await trigger.click();
  const detail = await (await response).json() as { items: { attempts: { event_id: string }[] }[] };
  expect(detail.items).toHaveLength(1);
  const attempts = detail.items[0]!.attempts;
  expect(attempts.length).toBeGreaterThan(0);
  const dialog = page.getByRole("dialog", { name: "调用详情", exact: true });
  await expect(dialog).toBeVisible();
  const chain = dialog.getByRole("region", { name: "故障转移与重试链", exact: true });
  async function expectAttempts() {
    await expect(chain.locator("article[data-attempt-event]")).toHaveCount(attempts.length);
    for (const attempt of attempts) {
      const article = chain.locator(`article[data-attempt-event="${attempt.event_id}"]`);
      await expect(article.getByText(attempt.event_id, { exact: true })).toBeVisible();
      const log = article.locator("details");
      await log.locator("summary").click();
      await expect(log.locator("pre")).toHaveText(JSON.stringify(attempt, null, 2));
      await log.locator("summary").click();
    }
  }
  await expectAttempts();
  for (const label of ["请求信息", "响应性能", "Token 消耗", "费用计算", "计费明细"]) await expect(dialog.getByRole("heading", { name: label, exact: true })).toBeVisible();
  await expect(dialog.getByText(positive.request_id, { exact: true })).toBeVisible();
  await expectUpstreamTiming(dialog, call.upstream_timing);
  const ingress = dialog.getByRole("region", { name: "请求入口阶段", exact: true });
  await expect(ingress).toBeVisible();
  const timing = call.request_timing;
  expect(timing?.request_body_complete, "实测成功调用已读完整正文").toBe(true);
  expect(timing?.request_body_bytes).toBeGreaterThan(0);
  for (const key of ["body_read_ms", "json_decode_ms", "pre_handler_ms"]) {
    const ms = timing?.[key];
    expect(typeof ms, `实测调用必须有 ${key}`).toBe("number");
    await expectDuration(ingress.locator(`[data-request-timing-field="${key}"] dd`), ms as number);
  }
  await expect(ingress.locator('[data-request-timing-field="request_body_bytes"] dd')).toHaveText(`已读取 ${new Intl.NumberFormat("en-US").format(timing!.request_body_bytes as number)} 字节`);
  await expect(ingress.locator('[data-request-timing-field="request_body_complete"] dd')).toHaveText("已读至结束");
  const section = dialog.getByRole("region", { name: "响应性能", exact: true });
  expect(Object.hasOwn(call, "latency_ms")).toBe(true);
  await expectDuration(section.locator("p").filter({ hasText: "下游响应阶段耗时" }).locator("strong"), call.latency_ms);
  const performance = section.getByLabel("响应性能", { exact: true });
  await expect(definition(performance, "输出 TPS").locator("xpath=..")).toHaveAttribute("title", OUTPUT_TPS_TITLE);
  expect(call.throughput_tps).not.toBeNull(); expect(call.throughput_tps!).toBeGreaterThan(0);
  expect(Number((await definition(performance, "输出 TPS").innerText()).replaceAll(",", ""))).toBeCloseTo(call.throughput_tps!, 2);
  await expectCharge(definition(dialog.getByRole("region", { name: "费用计算", exact: true }), "最终费用"), call.charge.calculation.final_micro_cny!);
  for (let i = 0; i < 6; i++) {
    await page.keyboard.press("Tab");
    expect(await dialog.evaluate(element => element.contains(document.activeElement)), "Tab must stay inside the detail drawer").toBe(true);
  }
  await screenshot(page, "tenant-call-drawer-1440.png");
  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0); await expect(trigger).toBeFocused();
  await trigger.click(); await expect(dialog).toBeVisible();
  await expectAttempts();
  await page.mouse.click(8, 120);
  await expect(dialog).toHaveCount(0); await expect(trigger).toBeFocused();
  await page.waitForLoadState("networkidle");
  const reads = apiRequests.get(page)!.slice(before);
  expect(reads, "重复打开同一条调用只读取一次详情").toHaveLength(1);
  const [method, target] = reads[0]!.split(" ");
  const url = new URL(target!, baseUrl);
  expect(method).toBe("GET");
  expect(url.pathname).toBe("/api/usage/calls");
  expect(Object.fromEntries(url.searchParams)).toEqual({ event_id: call.event_id, group_requests: "true", include_attempts: "true", limit: "1" });
});

// 窄屏抽屉保留完整宽度、内部滚动与可达关闭按钮，页面本身不横向溢出。
test("real-call-drawer-mobile-fits-scrolls-and-restores-focus", async ({ page }) => {
  await page.setViewportSize({ width: 390, height: 667 });
  const { row, call } = await showCall(page, positive);
  const trigger = row.getByRole("button", { name: "查看调用详情", exact: true });
  await trigger.click();
  const dialog = page.getByRole("dialog", { name: "调用详情", exact: true });
  await expect(dialog).toBeVisible();
  // 只等已启动的入场动画结束，不以几何断言轮询到通过来掩盖越界。
  await dialog.evaluate(async element => {
    const animations = element.getAnimations().filter(animation => animation instanceof CSSAnimation && animation.animationName === "tenant-call-enter");
    for (const animation of animations) {
      if (!Number.isFinite(animation.effect?.getComputedTiming().endTime)) throw new Error("Drawer entry animation must finish in finite time");
    }
    let timeout: number | undefined;
    try {
      await Promise.race([
        Promise.all(animations.map(animation => animation.finished)),
        new Promise<never>((_, reject) => { timeout = window.setTimeout(() => reject(new Error("Drawer entry animation exceeded 1200 ms")), 1200); }),
      ]);
    } finally {
      if (timeout !== undefined) window.clearTimeout(timeout);
    }
    await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())));
  });
  const bounds = await dialog.boundingBox();
  expect(bounds).not.toBeNull(); expect(bounds!.width).toBeGreaterThanOrEqual(360); expect(bounds!.x).toBeGreaterThanOrEqual(0); expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(391);
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  expect(await dialog.evaluate(element => element.scrollWidth <= element.clientWidth)).toBe(true);
  await expectUpstreamTiming(dialog, call.upstream_timing);
  const body = dialog.locator(".tenant-call-drawer-body");
  expect(await body.evaluate(element => element.scrollHeight > element.clientHeight)).toBe(true);
  await body.hover(); await page.mouse.wheel(0, 1600);
  await expect.poll(() => body.evaluate(element => element.scrollTop)).toBeGreaterThan(0);
  await expect(dialog.getByRole("button", { name: "关闭调用详情", exact: true })).toBeInViewport();
  await screenshot(page, "tenant-call-drawer-390.png");
  await dialog.getByRole("button", { name: "关闭调用详情", exact: true }).click();
  await expect(dialog).toHaveCount(0); await expect(trigger).toBeFocused();
});
