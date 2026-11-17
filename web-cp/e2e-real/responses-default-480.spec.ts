import { expect, test } from "@playwright/test";
import { randomUUID } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { ApiClient } from "../e2e/stack";
import { chooseSelect, selectOptions } from "../e2e/select";

interface Resource {
  id: string;
  doc: Record<string, unknown>;
  rollout?: { state: string; online: number; applied: number };
}
interface State {
  httpUrl: string;
  previewUrl: string;
  operator: { email: string; password: string };
}
const output = resolve(process.env.RESPONSES_DEFAULT_QA_OUTPUT!);
const state = JSON.parse(readFileSync(process.env.RESPONSES_DEFAULT_QA_STATE!, "utf8")) as State;
for (const address of [state.httpUrl, state.previewUrl]) {
  if (new URL(address).hostname !== "127.0.0.1") throw new Error("This acceptance requires an isolated loopback control plane");
}

// 预期只来自用户确认的合同：默认 OpenAI 适配器可选，显式协议与平台边界继续生效。
test("480: default Responses accounts can be selected, saved and reloaded while explicit and platform exclusions remain", async ({ page }) => {
  mkdirSync(output, { recursive: true });
  const admin = new ApiClient(state.httpUrl);
  await admin.post("/admin/login", state.operator);
  const suffix = randomUUID().slice(0, 8);
  const create = async (name: string, doc: Record<string, unknown>): Promise<Resource> => admin.post("/admin/resources/provider_keys", {
    doc: { display_name: `480 ${name} ${suffix}`, api_key: `isolated-management-only-${suffix}`, ...doc },
  });
  const [custom, deepseek, openai, explicit, empty, messages, anthropic, bedrock, vertex, azure] = await Promise.all([
    create("Custom default", { provider: "custom480", adapter: "openai", api_base: "http://127.0.0.1:9/v1" }),
    create("DeepSeek default", { provider: "deepseek", adapter: "openai" }),
    create("OpenAI default", { provider: "openai" }),
    create("Explicit Responses", { provider: "custom480", adapter: "openai", api_base: "http://127.0.0.1:9/v1", apis: { responses: {} } }),
    create("Explicit empty", { provider: "openai", apis: {} }),
    create("Messages only", { provider: "custom480", adapter: "openai", api_base: "http://127.0.0.1:9/v1", apis: { messages: {} } }),
    create("Anthropic only", { provider: "anthropic" }),
    create("Bedrock", { provider: "bedrock", adapter: "bedrock" }),
    create("Vertex", { provider: "vertex", adapter: "vertex" }),
    create("Azure", { provider: "azure", adapter: "azure-openai" }),
  ]);
  const candidates = [custom, deepseek, openai, explicit];
  const excluded = [empty, messages, anthropic, bedrock, vertex, azure];
  const saved: object[] = [];
  const pageErrors: string[] = [];
  const consoleErrors: string[] = [];
  const consoles: object[] = [];
  const forbiddenRequests: string[] = [];
  page.on("pageerror", error => pageErrors.push(error.message));
  page.on("console", message => {
    if (["error", "warning"].includes(message.type())) consoles.push({ type: message.type(), text: message.text() });
    const bootstrap = message.text().includes("401") && /\/admin\/(me|session)$/.test(message.location().url);
    if (message.type() === "error" && !bootstrap) consoleErrors.push(message.text());
  });
  page.on("request", request => {
    const url = new URL(request.url());
    if (url.hostname !== "127.0.0.1" || /^\/v1\/|\/probe(?:\/|$)|\/upstream-models$|\/model-tests(?:\/|$)/.test(url.pathname)) {
      forbiddenRequests.push(`${request.method()} ${url.origin}${url.pathname}`);
    }
  });
  try {
    await page.goto(state.previewUrl);
    await page.getByLabel("邮箱", { exact: true }).fill(state.operator.email);
    await page.getByLabel("口令", { exact: true }).fill(state.operator.password);
    const loggedIn = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login" && response.request().method() === "POST");
    await page.getByRole("button", { name: "登录", exact: true }).click();
    expect((await loggedIn).status()).toBe(200);
    await expect(page.locator("[data-tab]").first()).toBeVisible();
    await page.goto(`${state.previewUrl}/#providers?section=token-counting`);
    await expect(page).toHaveURL(`${state.previewUrl}/#providers?section=token-counting`);
    const region = page.getByRole("region", { name: "Token计数", exact: true });
    await expect(region).toBeVisible();
    const control = region.getByRole("group", { name: "Responses · input_tokens", exact: true }).getByRole("combobox", { name: "计数账号", exact: true });
    const options = await selectOptions(control);
    for (const account of candidates) {
      await expect(options.and(page.locator(`[data-value="${account.id}"]`)), `${account.doc.display_name} is an enabled Responses candidate`).toBeVisible();
      await expect(options.and(page.locator(`[data-value="${account.id}"]`))).not.toHaveAttribute("aria-disabled", "true");
    }
    for (const account of excluded) {
      await expect(options.and(page.locator(`[data-value="${account.id}"]`)), `${account.doc.display_name} must not appear as a Responses candidate`).toHaveCount(0);
    }
    await page.screenshot({ path: join(output, "responses-candidates.png"), fullPage: false });
    await page.keyboard.press("Escape");
    for (const account of candidates) {
      await chooseSelect(control, { value: account.id });
      const saving = page.waitForResponse(response => new URL(response.url()).pathname.startsWith("/admin/resources/token_counting") && ["POST", "PUT"].includes(response.request().method()));
      await region.getByRole("button", { name: "保存Token计数", exact: true }).click();
      const response = await saving;
      const body: unknown = await response.json();
      saved.push({ account: account.id, status: response.status(), body });
      writeFileSync(join(output, "save-results.json"), JSON.stringify(saved, null, 2));
      expect([200, 201], `${account.doc.display_name}: ${JSON.stringify(body)}`).toContain(response.status());
      const read = async (): Promise<Resource> => {
        const listed = await admin.get("/admin/resources/token_counting") as { items: Resource[] };
        expect(listed.items).toHaveLength(1);
        return listed.items[0]!;
      };
      expect((await read()).doc.responses_input_tokens).toEqual({ provider_key_id: account.id, model_mappings: [] });
      await expect.poll(async () => (await read()).rollout?.state, { timeout: 45_000, message: "the saved account configuration is applied by a real managed gateway" }).toBe("applied");
      const applied = await read();
      expect(applied.rollout?.online).toBeGreaterThanOrEqual(1);
      expect(applied.rollout?.applied).toBe(applied.rollout?.online);
      await page.reload();
      await expect(control).toHaveAttribute("data-value", account.id);
      await expect(control).toContainText(String(account.doc.display_name));
    }
    await page.screenshot({ path: join(output, "responses-saved-reloaded.png"), fullPage: false });
    expect(await page.title()).not.toBe("");
    expect(await page.locator("vite-error-overlay").count()).toBe(0);
    expect(pageErrors).toEqual([]);
    expect(consoleErrors).toEqual([]);
    expect(forbiddenRequests, "account management sends no inference, probe, model-list or external requests").toEqual([]);
  } finally {
    writeFileSync(join(output, "browser-health.json"), JSON.stringify({ url: page.url(), title: await page.title(), pageErrors, consoleErrors, consoles, forbiddenRequests, browserPlugin: "Browser plugin not available; repository Playwright used", realBackend: true, officialProviderCalls: "NOT EXECUTED" }, null, 2));
  }
});
