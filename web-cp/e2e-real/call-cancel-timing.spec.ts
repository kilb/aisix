import { expect, test } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join } from "node:path";

// 用真实网关在上游收到请求、未写响应头时断开调用方。相同场景在旧、新网关各跑一次。
// CALL_LOG_QA_CANCEL_CASE=historical|measured 与 CALL_LOG_QA_CANCEL_EVENT_ID 来自独立场景准备。
const session = process.env.REFINED_QA_SESSION!;
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const evidence = process.env.CALL_LOG_QA_CANCEL_CASE;
const eventId = process.env.CALL_LOG_QA_CANCEL_EVENT_ID;
if (!eventId || (evidence !== "historical" && evidence !== "measured")) throw new Error("需要真实断开事件编号及已知网关口径 historical 或 measured");
const base = new URL(state.adminUrl);
if (state.ready !== true || state.realBackend !== true || state.mockUpstream !== false || !["127.0.0.1", "localhost"].includes(base.hostname)) throw new Error("需要本机隔离真实后端");

test("等候上游响应头时断开，详情保留首内容缺失并按网关版本显示实测等待", async ({ page }) => {
  await page.goto(base.href);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const login = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login" && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await login).status()).toBe(200);
  try {
    const raw = await page.evaluate(async id => {
      const response = await fetch(`/admin/usage/events/${encodeURIComponent(id)}`);
      return { status: response.status, body: await response.json() };
    }, eventId!);
    expect(raw.status).toBe(200);
    const { event } = raw.body;
    expect(event).toMatchObject({ status_code: 499, error_class: "client_disconnected", upstream_dispatched: true });
    expect(event.upstream_timing.http_send_started_ms).toBeGreaterThanOrEqual(0);
    expect(event.upstream_timing.response_headers_ms).toBeUndefined();
    expect(event.first_token_latency_ms).toBeUndefined();
    expect(typeof event.upstream_latency_ms).toBe("number");
    if (evidence === "historical") expect(event.upstream_latency_ms).toBe(0);
    else expect(event.upstream_latency_ms).toBeGreaterThan(0);

    await page.goto(new URL(`/#calls?detail=${encodeURIComponent(eventId!)}`, base).href);
    const timing = page.getByRole("region", { name: "时间与性能", exact: true });
    const value = (label: string) => timing.locator("dt").getByText(label, { exact: true }).locator("xpath=following-sibling::dd[1]");
    await expect(value("请求首内容")).toHaveText("—");
    const waiting = value("尝试开始至断开");
    if (evidence === "historical") await expect(waiting).toHaveText("—");
    else {
      const ms = event.upstream_latency_ms;
      const unit = ms < 1000 ? "ms" : "s";
      const amount = new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 }).format(ms < 1000 ? ms : ms / 1000);
      await expect(waiting).toHaveText(`${amount} ${unit}`);
    }
  } finally {
    const status = await page.evaluate(async () => (await fetch("/admin/logout", { method: "POST", headers: { "content-type": "application/json" }, body: "{}" })).status);
    expect(status).toBe(204);
  }
});
