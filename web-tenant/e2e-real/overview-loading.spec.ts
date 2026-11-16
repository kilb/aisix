import { expect, test, type Response } from "@playwright/test";
import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const baseUrl: string = process.env.TENANT_OVERVIEW_URL ?? state.tenantUrl;
const base = new URL(baseUrl);
if (state.ready !== true || state.realBackend !== true || state.mockUpstream !== false) throw new Error("Real isolated backend is required");
if (!["http:", "https:"].includes(base.protocol) || !["localhost", "127.0.0.1", "[::1]"].includes(base.hostname) || base.username || base.password) throw new Error("Only an isolated loopback overview URL is allowed");
if (typeof auth.metricsOwner?.email !== "string" || typeof auth.metricsOwner?.password !== "string") throw new Error("Existing isolated owner credentials are required");

test("已认证概览不等组织资料返回才开始读取统计", async ({ context, page }, info) => {
  const errors: string[] = [];
  const writes: string[] = [];
  page.on("pageerror", () => errors.push("pageerror"));
  page.on("console", message => { if (message.type() === "error") errors.push("console error"); });
  page.on("request", request => {
    const url = new URL(request.url());
    if (url.origin === base.origin && url.pathname.startsWith("/api/") && request.method() !== "GET") writes.push(`${request.method()} ${url.pathname}`);
  });
  const login = await context.request.post(new URL("api/login", base).href, { data: auth.metricsOwner });
  expect(login.status(), "reuse the retained local owner without seeding or inference").toBe(200);
  try {
    const paths = ["/api/session", "/api/org", "/api/usage/metrics", "/api/usage/summary", "/api/budgets", "/api/billing/balance"];
    const answers = paths.map(path => page.waitForResponse(response => new URL(response.url()).pathname === path && response.request().method() === "GET"));
    await page.goto(new URL("#org", base).href, { waitUntil: "domcontentloaded" });
    const responses = await Promise.all(answers);
    const byPath = new Map<string, Response>();
    for (const response of responses) {
      expect(response.status(), new URL(response.url()).pathname).toBe(200);
      await response.finished();
      byPath.set(new URL(response.url()).pathname, response);
    }
    const bootstrap = await byPath.get("/api/session")!.json();
    expect(bootstrap).toMatchObject({ authed: true, tenant: { status: "active" }, member: { role: "owner" } });
    const timings = Object.fromEntries([...byPath].map(([path, response]) => [path, response.request().timing()]));
    await info.attach("overview-request-timings", { body: JSON.stringify(timings, null, 2), contentType: "application/json" });
    const org = timings["/api/org"]!;
    expect(org.requestStart, "organization profile request was actually sent").toBeGreaterThanOrEqual(0);
    expect(org.responseEnd, "organization profile response actually completed").toBeGreaterThanOrEqual(org.requestStart);
    const orgCompletedAt = org.startTime + org.responseEnd;
    for (const path of ["/api/usage/metrics", "/api/usage/summary", "/api/budgets", "/api/billing/balance"]) {
      const timing = timings[path]!;
      expect(timing.requestStart, `${path} was actually sent`).toBeGreaterThanOrEqual(0);
      expect(timing.startTime, `${path} must start before the independent organization profile finishes`).toBeLessThan(orgCompletedAt);
    }
    await expect(page).toHaveTitle("Routio 组织门户");
    await expect(page.getByRole("heading", { name: "概览", exact: true })).toBeVisible();
    await expect(page.getByRole("group", { name: "API 指标概览", exact: true })).toHaveAttribute("aria-busy", "false");
    await expect(page.getByRole("alert")).toHaveCount(0);
    expect(errors).toEqual([]);
    expect(writes, "cold overview performs reads only").toEqual([]);
    await page.screenshot({ path: info.outputPath("overview-1440.png"), fullPage: false });
  } finally {
    expect((await context.request.post(new URL("api/logout", base).href, { data: {} })).status()).toBe(204);
  }
});
