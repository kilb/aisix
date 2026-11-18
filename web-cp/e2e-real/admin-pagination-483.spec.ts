import { readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, test } from "@playwright/test";

const output = process.env.ADMIN_PAGINATION_QA_OUTPUT!;
const state = JSON.parse(readFileSync(join(output, "session/public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(output, "session/local-auth.json"), "utf8"));
if (state.ready !== true || state.realBackend !== true || state.mockUpstream !== false || !["127.0.0.1", "localhost"].includes(new URL(state.httpUrl).hostname)) {
  throw new Error("分页验收需要独立的真实本地控制面");
}

test("tenant and resource pages retain rows beyond the first API page", async ({ page }) => {
  const errors: string[] = [];
  const responses: { path: string; before: boolean; rows: number; cursor: boolean; status: number }[] = [];
  const pending: Promise<void>[] = [];
  page.on("pageerror", error => errors.push(error.message));
  page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
  page.on("response", response => {
    const url = new URL(response.url());
    if (response.request().method() !== "GET" || !["/admin/tenants", "/admin/resources/rate_limit_policies"].includes(url.pathname)) return;
    pending.push((async () => {
      const body = await response.json();
      responses.push({ path: url.pathname, before: url.searchParams.has("before"), rows: body.items.length, cursor: typeof body.next_before === "string", status: response.status() });
    })());
  });
  await page.goto(state.adminUrl);
  await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
  const login = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login" && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await login).status()).toBe(200);
  await page.goto(`${state.adminUrl}/#tenants`);
  await expect(page).toHaveURL(/#tenants$/);
  await expect(page.getByRole("heading", { name: "租户", exact: true })).toBeVisible();
  const tenantTable = page.getByRole("table", { name: "租户列表", exact: true });
  await expect(tenantTable.getByRole("row")).toHaveCount(26);
  await expect(page.getByRole("navigation", { name: "租户列表", exact: true })).toContainText("共 202 条");
  await page.getByRole("navigation", { name: "租户列表", exact: true }).getByRole("button", { name: "下一页", exact: true }).click();
  await expect(tenantTable).toContainText("paging-tenant-025");
  await page.getByRole("searchbox", { name: "租户列表搜索", exact: true }).fill("paging-tenant-200");
  await expect(tenantTable.getByRole("row")).toHaveCount(2);
  await expect(tenantTable).toContainText("paging-tenant-200");
  await page.screenshot({ path: join(output, "tenant-second-api-page.png") });

  await page.goto(`${state.adminUrl}/#resources?section=rate_limit_policies`);
  await expect(page).toHaveURL(/#resources\?section=rate_limit_policies$/);
  await expect(page.getByRole("heading", { name: "全部资源", exact: true })).toBeVisible();
  const resourceTable = page.getByRole("table", { name: "资源文档", exact: true });
  await expect(resourceTable.getByRole("row")).toHaveCount(26);
  await expect(page.getByRole("navigation", { name: "资源文档", exact: true })).toContainText("共 201 条");
  await page.getByRole("searchbox", { name: "资源文档搜索", exact: true }).fill("paging-policy-200");
  await expect(resourceTable.getByRole("row")).toHaveCount(2);
  await expect(resourceTable).toContainText("paging-policy-200");
  await page.screenshot({ path: join(output, "resource-second-api-page.png") });
  await Promise.all(pending);
  for (const path of ["/admin/tenants", "/admin/resources/rate_limit_policies"]) {
    const pages = responses.filter(response => response.path === path);
    expect(pages.some(response => response.rows === 200 && response.cursor && !response.before)).toBe(true);
    expect(pages.some(response => response.rows === (path === "/admin/tenants" ? 2 : 1) && !response.cursor && response.before)).toBe(true);
    expect(pages.every(response => response.status === 200 && response.rows <= 200)).toBe(true);
  }
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
  expect(await page.title()).not.toBe("");
  expect(errors).toEqual([]);
});
