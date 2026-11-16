import { expect, test, type Locator, type Page } from "@playwright/test";
import { randomUUID } from "node:crypto";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { chooseSelect } from "../e2e/select";

// 只在统一启动的真实隔离环境内配置专属模型，不发送推理或上游探测。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const baseUrl: string = state.adminUrl;
if (!state.ready || state.mockUpstream !== false || !["127.0.0.1", "localhost"].includes(new URL(baseUrl).hostname)) {
  throw new Error("Model editor E2E requires a ready real loopback stack");
}
const accountId: string = state.tokenUiFixture?.providerAccountId;
const upstream: string = state.tokenUiFixture?.publicModelName;
if (!accountId || !upstream) throw new Error("The real provider account metadata is required");
const output = resolve(process.env.REFINED_QA_OUTPUT!);
type Document = { id: string; version: number; doc: Record<string, unknown> };
const failures = new WeakMap<Page, string[]>();
const forbidden = new WeakMap<Page, string[]>();

async function api(page: Page, path: string, method = "GET", body?: unknown, version?: number): Promise<any> {
  if (!path.startsWith("/admin/") || /upstream-models|probe|test/.test(path)) throw new Error("Only configuration API paths are allowed");
  const answer = await page.evaluate(async ({ path, method, body, version }) => {
    const response = await fetch(path, {
      method,
      credentials: "include",
      headers: { "content-type": "application/json", ...(version === undefined ? {} : { "if-match": String(version) }) },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    return { status: response.status, body: response.status === 204 ? null : await response.json() };
  }, { path, method, body, version });
  expect([200, 201, 204], `${method} ${path}`).toContain(answer.status);
  return answer.body;
}

async function settle(page: Page): Promise<void> {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
}

async function navigate(page: Page, route: "models" | "public-routes"): Promise<void> {
  await page.goto(`${baseUrl}/#${route}`);
  await settle(page);
  expect(new URL(page.url()).hash).toBe(`#${route}`);
  expect(await page.title()).not.toBe("");
  await expect(page.getByRole("heading", { name: route === "models" ? "模型目录" : "公共模型与路由", exact: true, level: 1 })).toBeVisible();
}

async function editModel(page: Page, name: string): Promise<Locator> {
  await navigate(page, "models");
  await page.getByRole("searchbox", { name: "搜索模型", exact: true }).fill(name);
  const row = page.getByRole("table", { name: "全部模型", exact: true }).getByRole("row").filter({ has: page.getByText(name, { exact: true }) });
  await row.getByRole("button", { name: "编辑", exact: true }).click();
  const form = page.getByRole("form", { name: `编辑模型「${name}」`, exact: true });
  await expect(form).toBeVisible();
  await expect(page.getByRole("heading", { name: "模型文档 · 当前配置", exact: true })).toHaveCount(0);
  return form;
}

async function createDirect(page: Page, name: string): Promise<Document> {
  const account = await api(page, `/admin/resources/provider_keys/${accountId}`);
  expect(typeof account.doc.provider).toBe("string");
  return api(page, "/admin/resources/models", "POST", { doc: {
    display_name: name, provider_key_id: accountId, provider: account.doc.provider, model_name: upstream,
  } });
}

async function checkLayout(page: Page, form: Locator, label: string): Promise<void> {
  const layout = await form.evaluate(element => {
    const box = element.getBoundingClientRect();
    const computed = getComputedStyle(element);
    const right = box.right - parseFloat(computed.paddingRight) - parseFloat(computed.borderRightWidth);
    const sections = [...element.querySelectorAll(":scope > section[aria-label]")].map(section => ({
      name: section.getAttribute("aria-label"), right: section.getBoundingClientRect().right,
    }));
    return { width: document.documentElement.clientWidth, pageWidth: document.documentElement.scrollWidth, right, sections };
  });
  expect(layout.sections.length, "named form groups must exist").toBeGreaterThan(0);
  expect(layout.pageWidth, "page has no horizontal overflow").toBeLessThanOrEqual(layout.width + 1);
  if (layout.width >= 1440) {
    // 将首个分组临时缩成半宽，确认同一宽度断言会拒绝原来的空白布局。
    const narrowed = await form.locator(":scope > section[aria-label]").first().evaluate(element => {
      const original = element.getAttribute("style");
      (element as HTMLElement).style.maxWidth = "50%";
      const right = element.getBoundingClientRect().right;
      if (original === null) element.removeAttribute("style");
      else element.setAttribute("style", original);
      return right;
    });
    expect(() => expect(narrowed).toBeGreaterThanOrEqual(layout.right - 2), "half-width regression must fail the width assertion").toThrow();
  }
  for (const section of layout.sections) expect(section.right, `${section.name} uses the available form width`).toBeGreaterThanOrEqual(layout.right - 2);
  await test.info().attach(label, { body: JSON.stringify(layout), contentType: "application/json" });
  await form.getByRole("heading").first().scrollIntoViewIfNeeded();
  mkdirSync(join(output, "screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "screenshots", `${label}.png`), fullPage: false });
}

test.beforeEach(async ({ page }) => {
  const errors: string[] = [];
  const probes: string[] = [];
  let authenticated = false;
  failures.set(page, errors); forbidden.set(page, probes);
  page.on("pageerror", error => errors.push(error.message));
  page.on("console", message => {
    const bootstrap = !authenticated && message.text().includes("401") && /\/admin\/(me|session)$/.test(message.location().url);
    if (message.type() === "error" && !bootstrap) errors.push(message.text());
  });
  page.on("response", response => {
    const path = new URL(response.url()).pathname;
    if (!path.startsWith("/admin/") || response.status() < 400) return;
    if (response.status() === 401 && !authenticated && /^\/admin\/(me|session)$/.test(path)) return;
    errors.push(`HTTP ${response.status()} ${path}`);
  });
  page.on("request", request => {
    const path = new URL(request.url()).pathname;
    if (/\/upstream-models$|\/probe(?:\/|$)|\/model-tests(?:\/|$)|^\/v1\//.test(path)) probes.push(path);
  });
  await page.goto(baseUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const login = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login" && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await login).status()).toBe(200);
  authenticated = true;
  await settle(page);
});

test.afterEach(async ({ page }) => {
  expect.soft(failures.get(page), "no application errors").toEqual([]);
  expect.soft(forbidden.get(page), "no inference or upstream probes").toEqual([]);
});

test("manual-direct-model-edit-uses-full-width-and-persists-every-field", async ({ page }) => {
  const name = `ui-direct-${randomUUID().slice(0, 8)}`;
  const model = await createDirect(page, name);
  const form = await editModel(page, name);
  await expect(form.getByLabel("对外名称", { exact: true })).toHaveValue(name);
  await expect(form.getByLabel("上游模型", { exact: true })).toHaveValue(upstream);
  await checkLayout(page, form, "models-direct-edit-1440");
  await page.setViewportSize({ width: 390, height: 1000 });
  await checkLayout(page, form, "models-direct-edit-390");
  await form.getByLabel("对外名称", { exact: true }).fill(`${name}-edited`);
  await form.getByLabel("上游模型", { exact: true }).fill(`${upstream}-ui-edited`);
  await form.getByRole("checkbox", { name: "故障冷却", exact: true }).check();
  await form.getByLabel("默认冷却秒数", { exact: true }).fill("45");
  await form.getByLabel("Retry-After 上限秒数", { exact: true }).fill("900");
  const saved = page.waitForResponse(response => new URL(response.url()).pathname === `/admin/resources/models/${model.id}` && response.request().method() === "PUT");
  await form.getByRole("button", { name: "保存", exact: true }).click();
  const response = await saved;
  expect(response.status()).toBe(200);
  expect(response.request().headers()["if-match"]).toMatch(new RegExp(`^(?:W/)?"?${model.version}"?$`));
  await expect(form).toHaveCount(0);
  await expect(page.getByRole("heading", { name: "模型文档 · 当前配置", exact: true })).toBeVisible();
  const stored = await api(page, `/admin/resources/models/${model.id}`);
  expect(stored.doc).toMatchObject({ ...model.doc, display_name: `${name}-edited`, model_name: `${upstream}-ui-edited`, cooldown: { enabled: true, default_seconds: 45, max_seconds: 900 } });
  expect(stored.version).toBeGreaterThan(model.version);
  const reopened = await editModel(page, `${name}-edited`);
  await expect(reopened.getByRole("checkbox", { name: "故障冷却", exact: true })).toBeChecked();
  await expect(reopened.getByLabel("默认冷却秒数", { exact: true })).toHaveValue("45");
  await expect(reopened.getByLabel("Retry-After 上限秒数", { exact: true })).toHaveValue("900");
});

test("public-model-route-edit-preserves-targets-settings-and-price-entry", async ({ page }) => {
  const name = `ui-public-${randomUUID().slice(0, 8)}`;
  const model = await api(page, "/admin/public-models", "POST", { name });
  await api(page, `/admin/provider-keys/${accountId}/channel-models/${model.id}`, "PUT", { upstream_model: upstream, priority: 10, weight: 2, enabled: true });
  await navigate(page, "public-routes");
  await page.getByRole("searchbox", { name: "搜索公共模型", exact: true }).fill(name);
  await page.getByRole("button", { name, exact: true }).click();
  await settle(page);
  const form = page.getByRole("form", { name: `编辑模型「${name}」`, exact: true });
  await checkLayout(page, form, "public-route-edit-1440");
  await expect(page.getByRole("heading", { name: "基准价联动", exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "重新生成", exact: true })).toBeEnabled();
  await chooseSelect(form.getByRole("combobox", { name: "同级分配方式", exact: true }), { label: "同级按请求加权轮询" });
  await form.getByLabel("重试次数", { exact: true }).fill("2");
  await form.getByLabel("最多切换几个目标", { exact: true }).fill("0");
  await form.getByLabel("额外重试的状态码", { exact: true }).fill("408");
  await form.getByRole("checkbox", { name: "429 也参与重试和切换", exact: true }).check();
  const target = form.getByRole("listitem", { name: /^目标 / });
  await expect(target).toHaveCount(1);
  await target.getByLabel("优先级", { exact: true }).fill("30");
  await target.getByLabel("权重", { exact: true }).fill("7");
  await expect(target.getByRole("combobox", { name: "上游模型名称", exact: true })).toHaveValue(upstream);
  await page.setViewportSize({ width: 390, height: 1000 });
  await checkLayout(page, form, "public-route-edit-390");
  const saved = page.waitForResponse(response => new URL(response.url()).pathname === `/admin/public-models/${model.id}` && response.request().method() === "PATCH");
  await form.getByRole("button", { name: "保存", exact: true }).click();
  expect((await saved).status()).toBe(200);
  await settle(page);
  const stored = await api(page, `/admin/public-models/${model.id}`);
  expect(stored.routing).toMatchObject({ strategy: "round_robin", retries: 2, max_fallbacks: 0, retry_on_429: true, fallback_on_statuses: [408] });
  expect(stored.served_by).toHaveLength(1);
  expect(stored.served_by[0]).toMatchObject({ provider_key_id: accountId, upstream_model: upstream, priority: 30, weight: 7, enabled: true });
  await expect(target.getByLabel("优先级", { exact: true })).toHaveValue("30");
  await expect(target.getByLabel("权重", { exact: true })).toHaveValue("7");
  await form.getByRole("button", { name: "关闭", exact: true }).click();
  await expect(page.getByRole("heading", { name: "公共模型与路由", exact: true, level: 1 })).toBeVisible();
  await page.getByRole("searchbox", { name: "搜索公共模型", exact: true }).fill(name);
  await page.getByRole("button", { name, exact: true }).click();
  await expect(form.getByLabel("重试次数", { exact: true })).toHaveValue("2");
});

test("manual-routing-model-adds-and-removes-targets-through-the-editor", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const [first, second] = await Promise.all([createDirect(page, `ui-target-a-${suffix}`), createDirect(page, `ui-target-b-${suffix}`)]);
  const name = `ui-routing-${suffix}`;
  const model = await api(page, "/admin/resources/models", "POST", { doc: { display_name: name, routing: { strategy: "round_robin", targets: [{ model_id: first.id, priority: 10, weight: 1 }] } } });
  const form = await editModel(page, name);
  await form.getByRole("button", { name: "添加目标", exact: true }).click();
  const rows = form.getByRole("group", { name: /^目标 \d+$/ });
  await expect(rows).toHaveCount(2);
  await chooseSelect(rows.nth(1).getByRole("combobox", { name: "目标模型", exact: true }), second.id);
  await rows.nth(1).getByLabel("权重", { exact: true }).fill("4");
  await rows.nth(1).getByLabel("优先级", { exact: true }).fill("0");
  await checkLayout(page, form, "manual-routing-edit-1440");
  let saved = page.waitForResponse(response => new URL(response.url()).pathname === `/admin/resources/models/${model.id}` && response.request().method() === "PUT");
  await form.getByRole("button", { name: "保存", exact: true }).click();
  expect((await saved).status()).toBe(200);
  const stored = await api(page, `/admin/resources/models/${model.id}`);
  expect(stored.doc.routing.targets).toEqual([{ model_id: first.id, priority: 10, weight: 1 }, { model_id: second.id, priority: 0, weight: 4 }]);
  const reopened = await editModel(page, name);
  await reopened.getByRole("group", { name: "目标 2", exact: true }).getByRole("button", { name: "移除", exact: true }).click();
  await expect(reopened.getByRole("group", { name: /^目标 \d+$/ })).toHaveCount(2);
  await reopened.getByRole("button", { name: "确认移除", exact: true }).click();
  await expect(reopened.getByRole("group", { name: /^目标 \d+$/ })).toHaveCount(1);
  await page.setViewportSize({ width: 390, height: 1000 });
  await checkLayout(page, reopened, "manual-routing-edit-390");
  saved = page.waitForResponse(response => new URL(response.url()).pathname === `/admin/resources/models/${model.id}` && response.request().method() === "PUT");
  await reopened.getByRole("button", { name: "保存", exact: true }).click();
  expect((await saved).status()).toBe(200);
  expect((await api(page, `/admin/resources/models/${model.id}`)).doc.routing.targets).toEqual([{ model_id: first.id, priority: 10, weight: 1 }]);
});
