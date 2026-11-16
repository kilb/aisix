import { expect, test, type Locator, type Page } from "@playwright/test";
import { randomUUID } from "node:crypto";
import { mkdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";

// 只使用统一 QA 环境的真实配置 API，不触发探测或模型调用。
const session = resolve(process.env.REFINED_QA_SESSION!);
const state = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const baseUrl: string = state.adminUrl;
const accountId: string = state.tokenUiFixture?.providerAccountId;
const upstream: string = state.tokenUiFixture?.publicModelName;
const output = resolve(process.env.REFINED_QA_OUTPUT!);
if (!state.ready || state.mockUpstream !== false || !["127.0.0.1", "localhost"].includes(new URL(baseUrl).hostname) || !accountId || !upstream) {
  throw new Error("Catalog layout E2E requires the ready real loopback stack and provider metadata");
}
const failures = new WeakMap<Page, string[]>();
const forbidden = new WeakMap<Page, string[]>();

async function api(page: Page, path: string, method = "GET", body?: unknown): Promise<any> {
  if (!path.startsWith("/admin/") || /upstream-models|probe|test/.test(path)) throw new Error("Only configuration API paths are allowed");
  const answer = await page.evaluate(async ({ path, method, body }) => {
    const response = await fetch(path, {
      method, credentials: "include", headers: { "content-type": "application/json" },
      ...(body === undefined ? {} : { body: JSON.stringify(body) }),
    });
    return { status: response.status, body: await response.json() };
  }, { path, method, body });
  expect([200, 201], `${method} ${path}`).toContain(answer.status);
  return answer.body;
}

async function settle(page: Page): Promise<void> {
  await page.evaluate(() => document.fonts.ready);
  await page.waitForLoadState("networkidle");
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
  expect(await page.title()).not.toBe("");
  const width = await page.evaluate(() => ({ viewport: document.documentElement.clientWidth, page: document.documentElement.scrollWidth }));
  expect(width.page, "page has no horizontal overflow").toBeLessThanOrEqual(width.viewport + 1);
}

async function navigate(page: Page, route: "models" | "providers"): Promise<void> {
  await page.goto(`${baseUrl}/#${route}`);
  await settle(page);
  expect(new URL(page.url()).hash).toBe(`#${route}`);
  await expect(page.getByRole("heading", { level: 1, name: route === "models" ? "模型目录" : "上游账号", exact: true })).toBeVisible();
}

async function screenshot(page: Page, name: string): Promise<void> {
  mkdirSync(join(output, "screenshots"), { recursive: true });
  await page.screenshot({ path: join(output, "screenshots", `${name}.png`), fullPage: false });
}

async function box(locator: Locator): Promise<{ x: number; y: number; width: number; height: number }> {
  const result = await locator.boundingBox();
  expect(result).not.toBeNull();
  return result!;
}

test.beforeEach(async ({ page }) => {
  const errors: string[] = [], probes: string[] = [];
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
  expect(forbidden.get(page), "layout never calls paid models or upstream discovery").toEqual([]);
  expect(failures.get(page), "no console, page or API errors").toEqual([]);
});

test("routing-catalog-stays-compact-and-all-targets-remain-readable-and-editable", async ({ page }) => {
  const suffix = randomUUID().slice(0, 8);
  const account = await api(page, `/admin/resources/provider_keys/${accountId}`);
  const targets = await Promise.all(Array.from({ length: 5 }, (_, i) => api(page, "/admin/resources/models", "POST", {
    doc: { display_name: `route-${i + 1}-${suffix}${i === 4 ? `-${"long-target-".repeat(8)}` : ""}`, provider_key_id: accountId, provider: account.doc.provider, model_name: upstream },
  })));
  const name = `catalog-${suffix}`;
  const config = targets.map((target, i) => ({ model_id: target.id, priority: 10 + i, weight: 1 + i }));
  const route = await api(page, "/admin/resources/models", "POST", { doc: { display_name: name, routing: { strategy: "round_robin", targets: config } } });
  for (const width of [1440, 1280]) {
    await page.setViewportSize({ width, height: 1000 });
    await navigate(page, "models");
    await page.getByRole("searchbox", { name: "搜索模型", exact: true }).fill(name);
    const row = page.getByRole("table", { name: "全部模型", exact: true }).getByRole("row").filter({ has: page.getByText(name, { exact: true }) });
    const targetCell = row.getByRole("cell").nth(3);
    const targetBox = await box(targetCell), nameBox = await box(row.getByRole("cell").first());
    expect(targetBox.width, "routing targets receive more space than the name").toBeGreaterThan(nameBox.width);
    expect((await box(row)).height, "five short targets do not produce an oversized row").toBeLessThanOrEqual(160);
    const first = targetCell.getByRole("listitem").filter({ has: page.getByText(targets[0].doc.display_name, { exact: true }) });
    const firstName = await box(first.getByText(targets[0].doc.display_name, { exact: true }));
    const groupLabel = await box(first.getByText("可用分组", { exact: true }));
    expect(groupLabel.y, "target and available-group label share a line").toBeLessThan(firstName.y + firstName.height);
    await expect(targetCell.getByText(targets[4].doc.display_name, { exact: true })).not.toBeVisible();
    const expand = targetCell.getByText("展开其余 3 个目标（共 5 个）", { exact: true });
    await expect(expand).toBeVisible();
    await screenshot(page, `catalog-compact-${width}`);
    await expand.click();
    for (const target of targets) {
      const line = targetCell.getByRole("listitem").filter({ has: page.getByText(target.doc.display_name, { exact: true }) });
      await expect(line.getByText(target.doc.display_name, { exact: true })).toBeVisible();
      for (const group of account.doc.groups ?? []) await expect(line.getByText(group, { exact: true })).toBeVisible();
    }
    await expand.focus();
    await expand.press("Enter");
    await expect(targetCell.getByText(targets[4].doc.display_name, { exact: true })).not.toBeVisible();
    await expand.press("Space");
    await expect(targetCell.getByText(targets[4].doc.display_name, { exact: true })).toBeVisible();
    await screenshot(page, `catalog-expanded-${width}`);
    await row.getByRole("button", { name: "编辑", exact: true }).click();
    const form = page.getByRole("form", { name: `编辑模型「${name}」`, exact: true });
    await expect(form.getByRole("group", { name: /^目标 \d+$/ })).toHaveCount(5);
    await form.getByRole("button", { name: "取消", exact: true }).click();
  }
  await page.setViewportSize({ width: 390, height: 1000 });
  await navigate(page, "models");
  await page.getByRole("searchbox", { name: "搜索模型", exact: true }).fill(name);
  await page.getByText("展开其余 3 个目标（共 5 个）", { exact: true }).click();
  for (const target of targets) await expect(page.getByText(target.doc.display_name, { exact: true })).toBeVisible();
  await settle(page);
  await screenshot(page, "catalog-expanded-390");
  expect((await api(page, `/admin/resources/models/${route.id}`)).doc.routing.targets).toEqual(config);
});

test("upstream-account-connection-and-groups-use-the-editor-width", async ({ page }) => {
  const account = await api(page, `/admin/resources/provider_keys/${accountId}`);
  const name: string = account.doc.display_name || account.id;
  for (const width of [1440, 1280, 390]) {
    await page.setViewportSize({ width, height: 1000 });
    await navigate(page, "providers");
    await page.getByRole("searchbox", { name: "搜索上游账号", exact: true }).fill(name);
    const row = page.getByRole("table", { name: "上游账号", exact: true }).getByRole("row").filter({ has: page.getByText(name, { exact: true }) });
    await row.getByRole("button", { name: "编辑", exact: true }).click();
    const form = page.getByRole("form", { name: `编辑上游账号「${name}」`, exact: true });
    await expect(form).toBeVisible();
    const available = await form.evaluate(element => {
      const rect = element.getBoundingClientRect(), style = getComputedStyle(element);
      return { left: rect.left + parseFloat(style.paddingLeft) + parseFloat(style.borderLeftWidth), right: rect.right - parseFloat(style.paddingRight) - parseFloat(style.borderRightWidth) };
    });
    for (const section of [form.getByRole("region", { name: "上游连接", exact: true }), form.getByRole("group", { name: "分组", exact: true })]) {
      const rect = await box(section);
      expect(rect.x).toBeCloseTo(available.left, 0);
      expect(rect.x + rect.width).toBeGreaterThanOrEqual(available.right - 2);
    }
    if (width === 1440) {
      const connection = form.getByRole("region", { name: "上游连接", exact: true });
      const narrowed = await connection.evaluate(element => {
        const original = element.getAttribute("style");
        (element as HTMLElement).style.maxWidth = "50%";
        const right = element.getBoundingClientRect().right;
        if (original === null) element.removeAttribute("style"); else element.setAttribute("style", original);
        return right;
      });
      expect(() => expect(narrowed).toBeGreaterThanOrEqual(available.right - 2), "width assertion rejects half-width regression").toThrow();
    }
    await settle(page);
    await screenshot(page, `provider-editor-${width}`);
    await form.getByRole("button", { name: "取消", exact: true }).click();
  }
});

test("public-catalog-row-keeps-a-visible-working-route-management-entry", async ({ page }) => {
  const name = `public-layout-${randomUUID().slice(0, 8)}`;
  const model = await api(page, "/admin/public-models", "POST", { name });
  await api(page, `/admin/provider-keys/${accountId}/channel-models/${model.id}`, "PUT", { upstream_model: upstream, priority: 10, weight: 2, enabled: true });
  const documents = await api(page, "/admin/resources/models");
  const projected = documents.items.find((item: any) => item.id === model.id);
  expect(projected.doc.routing.targets).toHaveLength(1);
  const targetId: string = projected.doc.routing.targets[0].model_id;
  expect(typeof targetId).toBe("string");
  const internal = await api(page, `/admin/resources/models/${targetId}`);
  await navigate(page, "models");
  await page.getByRole("checkbox", { name: /^显示内部直连模型/ }).check();
  await page.getByRole("searchbox", { name: "搜索模型", exact: true }).fill(targetId);
  const internalRow = page.getByRole("table", { name: "全部模型", exact: true }).getByRole("row").filter({ has: page.getByRole("button", { name: "查看", exact: true }) });
  await expect(internalRow).toHaveCount(1);
  await internalRow.getByRole("button", { name: "查看", exact: true }).click();
  const internalForm = page.getByRole("form");
  await expect(internalForm).toBeVisible();
  expect(await internalForm.getAttribute("aria-label")).toContain(internal.doc.display_name);
  await expect(internalForm.getByRole("button", { name: "保存", exact: true })).toHaveCount(0);
  await internalForm.getByRole("button", { name: "关闭", exact: true }).click();
  await navigate(page, "models");
  await page.getByRole("checkbox", { name: /^显示内部直连模型/ }).uncheck();
  await page.getByRole("searchbox", { name: "搜索模型", exact: true }).fill(name);
  const row = page.getByRole("table", { name: "全部模型", exact: true }).getByRole("row").filter({ has: page.locator("td:first-child").getByText(name, { exact: true }) });
  const manage = row.getByRole("button", { name: "公共路由", exact: true });
  await expect(manage).toBeVisible();
  await expect(manage).toHaveText("公共路由");
  await manage.click();
  await expect(page.getByRole("form", { name: `编辑模型「${name}」`, exact: true })).toBeVisible();
  await settle(page);
  await screenshot(page, "catalog-public-route-management");
  expect((await api(page, `/admin/public-models/${model.id}`)).served_by).toEqual(expect.arrayContaining([
    expect.objectContaining({ provider_key_id: accountId, upstream_model: upstream, priority: 10, weight: 2, enabled: true }),
  ]));
});
