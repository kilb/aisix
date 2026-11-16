import { expect, test } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 预期只来自真实记录、独立场景分类与公开的字节除四口径，不读取产品源码。
interface Scenario { kind: "positive" | "partial" | "zero" | "missing" | "multi"; event_id: string; request_id: string; tenant_auth?: string; from?: string; to?: string }
interface TextLength { utf8_bytes: number; complete: boolean }
interface Call {
  event_id: string; usage_input_tokens: number | null; usage: { completion_tokens?: number | null } | null;
  io_audit: { version: number; boundary: string; exchange_count?: number | null; input_text?: TextLength | null; output_text?: TextLength | null } | null;
  io_token_reference: { method: string; input_tokens?: number | null; output_tokens?: number | null } | null;
}
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const { cases } = JSON.parse(readFileSync(process.env.BYTE_TOKEN_QA_CASES!, "utf8")) as { cases: Scenario[] };
if (state.ready !== true || state.mockUpstream !== false || state.realBackend !== true) throw new Error("Byte-token E2E requires a ready real isolated stack");
if (!Array.isArray(cases) || cases.length === 0) throw new Error("Supply at least one classified real gateway event; no skip or generated fixture fallback");
for (const scenario of cases) {
  if (!["positive", "partial", "zero", "missing", "multi"].includes(scenario.kind) || !scenario.event_id || !scenario.request_id) throw new Error("Every byte-token scenario requires kind, event_id and request_id");
}
const output = resolve(process.env.REFINED_QA_OUTPUT!);
const count = (value: number) => new Intl.NumberFormat("en-US").format(value);
const supported = (call: Call) => call.io_audit != null && (call.io_audit.version === 1 && call.io_audit.boundary === "client_exchange" || call.io_audit.version === 2 && call.io_audit.boundary === "upstream_attempt");

function classify(call: Call, scenario: Scenario): void {
  const audit = call.io_audit;
  const reference = call.io_token_reference;
  if (scenario.kind === "missing") {
    expect(audit?.version === 2 && (audit.exchange_count ?? 1) > 1, "multiple exchanges must be classified separately from missing measurements").toBe(false);
    expect(audit == null || reference == null || reference.input_tokens == null || reference.output_tokens == null, "the real record must actually lack a reference").toBe(true);
    return;
  }
  expect(supported(call), "a classified measurement must have a supported version and boundary").toBe(true);
  if (scenario.kind === "multi") {
    expect(audit!.version).toBe(2);
    expect(audit!.exchange_count).toBeGreaterThan(1);
    expect(reference).toBeNull();
    return;
  }
  expect(reference?.method).toBe("utf8_bytes_div4_v1");
  const pairs = [[reference!.input_tokens, audit!.input_text], [reference!.output_tokens, audit!.output_text]] as const;
  for (const [tokens, text] of pairs) {
    if (tokens == null) continue;
    expect(text).not.toBeNull();
    expect(text).toBeDefined();
    expect(tokens, "the reference is a token count rather than the raw text byte count").toBe(Math.ceil(text!.utf8_bytes / 4));
  }
  if (scenario.kind === "positive") {
    expect(reference!.input_tokens).toBeGreaterThan(0);
    expect(reference!.output_tokens).toBeGreaterThan(0);
  } else if (scenario.kind === "zero") {
    expect(pairs.some(([tokens, text]) => tokens === 0 && text?.utf8_bytes === 0), "zero must be explicitly measured and reported").toBe(true);
  } else {
    expect(pairs.some(([tokens, text]) => tokens != null && text?.complete === false), "partial must come from an unfinished real text observation").toBe(true);
  }
}

function expectedSummary(call: Call): string {
  const audit = call.io_audit;
  if (!supported(call)) return `按字节估算 Token：${audit == null ? "未采集文本长度" : "统计口径暂不支持"}`;
  if (audit!.version === 2 && (audit!.exchange_count ?? 1) > 1) return "按字节估算 Token：多次上游交换，无法单次估算";
  const reference = call.io_token_reference;
  if (reference != null && reference.method !== "utf8_bytes_div4_v1") return "按字节估算 Token：估算方法暂不支持";
  const value = (tokens: number | null | undefined, text: TextLength | null | undefined) => text == null ? "不可估算（未采集文本）" : tokens == null ? "不可估算（参考未记录）" : `≈ ${count(tokens)} Token${text.complete ? "" : "（部分）"}`;
  return `按字节估算 Token：输入 ${value(reference?.input_tokens, audit!.input_text)} / 输出 ${value(reference?.output_tokens, audit!.output_text)}`;
}

for (const scenario of cases) for (const portal of ["admin", "tenant"] as const) {
  test(`${portal} ${scenario.kind}: real byte estimates show token counts in list and detail (${scenario.event_id})`, async ({ page }) => {
    const base = state[portal === "admin" ? "adminUrl" : "tenantUrl"] as string;
    if (!["127.0.0.1", "localhost"].includes(new URL(base).hostname)) throw new Error("Only isolated loopback URLs are allowed");
    const writes: string[] = [];
    const errors: string[] = [];
    const consoleMessages: object[] = [];
    page.on("request", request => { const path = new URL(request.url()).pathname; if (!['GET', 'HEAD'].includes(request.method()) && !/^\/(admin|api)\/login$/.test(path)) writes.push(`${request.method()} ${path}`); });
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => {
      if (!["warning", "error"].includes(message.type())) return;
      consoleMessages.push({ type: message.type(), text: message.text(), location: message.location() });
      const location = message.location().url;
      const path = /^https?:/.test(location) ? new URL(location).pathname : "";
      if (message.type() === "error" && !(message.text().includes("401") && ["/api/me", "/api/session", "/admin/me", "/admin/session"].includes(path))) errors.push(message.text());
    });
    page.on("response", response => {
      const path = new URL(response.url()).pathname;
      if (response.status() >= 400 && !["/api/me", "/api/session", "/admin/me", "/admin/session"].includes(path)) errors.push(`HTTP ${response.status()} ${path}`);
    });
    const who = auth[portal === "admin" ? "operator" : scenario.tenant_auth ?? "metricsOwner"];
    expect(typeof who?.email).toBe("string");
    await page.goto(base);
    await page.getByLabel("邮箱", { exact: true }).fill(who.email);
    await page.getByLabel("口令", { exact: true }).fill(who.password);
    const login = page.waitForResponse(response => new URL(response.url()).pathname === `/${portal === "admin" ? "admin" : "api"}/login` && response.request().method() === "POST");
    await page.getByRole("button", { name: "登录", exact: true }).click();
    expect((await login).status()).toBe(200);
    const path = portal === "admin" ? "/admin/pricing/charges" : "/api/usage/calls";
    const query = new URLSearchParams(portal === "admin" ? { event_id: scenario.event_id } : { request_id: scenario.request_id });
    for (const key of ["from", "to"] as const) if (scenario[key]) query.set(key, scenario[key]);
    const list = page.waitForResponse(response => new URL(response.url()).pathname === path && new URL(response.url()).searchParams.get(portal === "admin" ? "event_id" : "request_id") === (portal === "admin" ? scenario.event_id : scenario.request_id));
    await page.goto(`${base}/#calls?${query}`);
    const answer = await list;
    expect(answer.status()).toBe(200);
    const body = await answer.json() as { items: Call[] };
    const index = body.items.findIndex(call => call.event_id === scenario.event_id);
    expect(index, "the real API must return the exact manifest event").toBeGreaterThanOrEqual(0);
    const call = body.items[index]!;
    classify(call, scenario);
    const row = portal === "admin" ? page.getByRole("row").filter({ has: page.getByRole("link", { name: `查看调用日志 ${scenario.event_id}`, exact: true }) }) : page.getByRole("table", { name: "调用记录", exact: true }).locator("tbody > tr").nth(index);
    await expect(row.locator("[data-byte-token-estimate]")).toHaveText(expectedSummary(call));
    expect(await page.title()).toMatch(/AISIX/i);
    await expect(page.getByRole("heading", { name: "调用记录", exact: true }).first()).toBeVisible();
    await expect(page.locator("vite-error-overlay")).toHaveCount(0);
    mkdirSync(join(output, "screenshots"), { recursive: true });
    for (const width of [1440, 390]) {
      await page.setViewportSize({ width, height: 1000 });
      await row.locator("[data-byte-token-estimate]").scrollIntoViewIfNeeded();
      const bounds = await row.locator("[data-byte-token-estimate]").evaluate(element => ({
        page: document.documentElement.scrollWidth, viewport: document.documentElement.clientWidth,
        client: element.clientWidth, scroll: element.scrollWidth, whiteSpace: getComputedStyle(element).whiteSpace,
      }));
      expect(bounds.page, `page fits ${width}px viewport`).toBeLessThanOrEqual(bounds.viewport + 1);
      expect(bounds.scroll, `estimate wraps inside its cell at ${width}px`).toBeLessThanOrEqual(bounds.client + 1);
      expect(bounds.whiteSpace).toBe("normal");
      await page.screenshot({ path: join(output, "screenshots", `byte-token-${portal}-${scenario.kind}-${scenario.event_id}-${width}-list.png`) });
    }
    await page.setViewportSize({ width: 1440, height: 1000 });
    if (portal === "admin") {
      await row.getByRole("link", { name: `查看调用日志 ${scenario.event_id}`, exact: true }).click();
      await expect(page.getByRole("heading", { name: "调用日志详情", exact: true })).toBeVisible();
    } else await row.getByRole("button", { name: "展开明细", exact: true }).click();
    const details = page.locator('details[aria-label="输入输出与 Token 参考"]');
    await expect(details).toHaveCount(1);
    await details.locator("summary").click();
    for (const direction of ["input", "output"] as const) {
      const tokens = call.io_token_reference?.[`${direction}_tokens`];
      const text = call.io_audit?.[`${direction}_text`];
      const expected = supported(call) && call.io_token_reference?.method === "utf8_bytes_div4_v1" && tokens != null && text != null ? `≈ ${count(tokens)} Token（按字节估算） · ${text.complete ? "完整" : "部分"}` : "不可用";
      await expect(details.locator(`[data-io-field="${direction}-reference"] dd`)).toHaveText(expected);
    }
    await expect(details.locator('[data-io-field="recorded-input"] dd')).toHaveText(call.usage_input_tokens == null ? "未记录" : count(call.usage_input_tokens));
    await expect(details.locator('[data-io-field="recorded-output"] dd')).toHaveText(call.usage?.completion_tokens == null ? "未记录" : count(call.usage.completion_tokens));
    await expect(details).toContainText("不用于计费");
    await details.scrollIntoViewIfNeeded();
    await page.screenshot({ path: join(output, "screenshots", `byte-token-${portal}-${scenario.kind}-${scenario.event_id}-detail.png`) });
    await test.info().attach("byte-token-console", { body: JSON.stringify(consoleMessages), contentType: "application/json" });
    expect(errors, "real pages must not show application or unexpected HTTP errors").toEqual([]);
    expect(writes, "inspection must not generate inference, telemetry fixtures or business writes").toEqual([]);
  });
}
