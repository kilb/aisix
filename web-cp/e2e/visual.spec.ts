import { expect, test, type Browser, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

/**
 * The look of every main page, against the real control plane through the
 * built `dist/`: light by default even when the system asks for dark, dark
 * only through the toggle and kept across a reload, and at phone width no
 * sideways scroll with the section navigation as a bar along the bottom.
 *
 * Each step saves a full-page screenshot (`visual-<page>-<light|dark|360>.png`)
 * for review, with transitions run to their end. Nothing compares pixels:
 * the assertions are the computed background, the page width and where the
 * navigation sits. The platform is set up through the real `/admin` and
 * `/api` first, so the lists show upstream accounts, models, tenants in each
 * state, keys and a gateway credential as they would.
 */

let stack: Stack;

const PASSWORD = "visual-pass-3c8x";

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();

  const accounts: Record<string, string[]> = { "pool-a-01": ["shared", "apac-low"], "pool-e-01": ["eu-pool", "batch"] };
  const accountIds: string[] = [];
  for (const [name, groups] of Object.entries(accounts)) {
    const account = await stack.admin.post("/admin/resources/provider_keys", {
      doc: {
        display_name: name,
        provider: "openai",
        api_key: `sk-${name}-visual-upstream`,
        api_base: `${stack.mockUpstreamUrl}/v1`,
        groups,
      },
    });
    accountIds.push(account.id);
  }
  const modelIds: string[] = [];
  for (const [i, name] of ["chat-small", "chat-large", "embed-v3"].entries()) {
    const model = await stack.admin.post("/admin/resources/models", {
      doc: { display_name: name, provider: "openai", model_name: "gpt-4o-mini", provider_key_id: accountIds[i % 2] },
    });
    modelIds.push(model.id);
  }

  // A tenant at work, one waiting for review and one suspended.
  const signUp = async (org: string, email: string): Promise<string> =>
    (await stack.tenant().post("/api/signup", { org_name: org, email, password: PASSWORD })).tenant.id;
  const active = await signUp("北辰数据", "owner@beichen-3c8.test");
  await stack.admin.post(`/admin/tenants/${active}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${active}`);
  await stack.admin.patch(
    `/admin/tenants/${active}`,
    { allowed_model_ids: modelIds.slice(0, 2), allowed_provider_groups: ["shared", "apac-low"] },
    current.version,
  );
  await stack.admin.post(`/admin/tenants/${active}/keys`, { label: "prod-backend" });
  await stack.admin.post(`/admin/tenants/${active}/keys`, { label: "ci-runner" });
  await signUp("Kestrel Labs", "owner@kestrel-3c8.test");
  const closed = await signUp("Quarry Lane Research Collective", "owner@quarry-3c8.test");
  await stack.admin.post(`/admin/tenants/${closed}/approve`);
  await stack.admin.post(`/admin/tenants/${closed}/suspend`);

  await stack.admin.post("/admin/gateways/credentials", { name: "gw-edge-01" });
  await stack.admin.post("/admin/operators", { email: "viewer@visual-3c8.test", role: "viewer", password: PASSWORD });
});

test.afterAll(async () => {
  await stack?.stop();
});

/** Every main page, by its `data-tab` and its label. */
const PAGES: [string, string][] = [
  ["overview", "概览"],
  ["gateways", "网关"],
  ["providers", "上游账号"],
  ["models", "模型与路由"],
  ["pricing", "价格"],
  ["tenants", "租户"],
  ["billing", "计费"],
  ["keys", "密钥"],
  ["cache", "缓存策略"],
  ["resources", "全部资源"],
  ["operators", "运维账号"],
  ["audit", "审计日志"],
];

/** Concrete, the light page background, and the dark page background. */
const LIGHT_BG = "rgb(245, 247, 244)";
const DARK_BG = "rgb(18, 28, 39)";

const PHONE = { width: 360, height: 780 };

/** A browser whose system setting is dark: the apps must not follow it. */
async function systemDark(browser: Browser): Promise<Page> {
  const context = await browser.newContext({ colorScheme: "dark", viewport: { width: 1440, height: 900 } });
  return context.newPage();
}

async function login(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令").fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

/** Opens `tab` and waits until its page has read what it lists and its fonts are in. */
async function open(page: Page, tab: string): Promise<void> {
  await page.locator(`[data-tab="${tab}"]`).click();
  await expect(page.locator(`[data-tab="${tab}"]`)).toHaveAttribute("aria-selected", "true");
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
}

function background(page: Page): Promise<string> {
  return page.evaluate(() => getComputedStyle(document.body).backgroundColor);
}

async function shoot(page: Page, name: string): Promise<void> {
  const path = test.info().outputPath(name);
  await page.screenshot({ path, fullPage: true, animations: "disabled" });
  await test.info().attach(name, { path, contentType: "image/png" });
}

for (const [tab, label] of PAGES) {
  test(`${label}：系统为深色时仍默认浅色，深色只经切换并在刷新后保留，手机宽度不横向滚动`, async ({ browser }) => {
    const page = await systemDark(browser);
    await login(page);
    await open(page, tab);

    // Light first, whatever the system says.
    await expect.poll(() => background(page)).toBe(LIGHT_BG);
    await shoot(page, `visual-${tab}-light.png`);

    // Dark through the toggle, and kept across a reload.
    await page.getByRole("button", { name: "切换到深色" }).click();
    await expect.poll(() => background(page)).toBe(DARK_BG);
    await shoot(page, `visual-${tab}-dark.png`);
    await page.reload();
    await expect(page.locator("[data-tab]").first()).toBeVisible();
    await expect.poll(() => background(page)).toBe(DARK_BG);

    // Back to light, at phone width.
    await page.getByRole("button", { name: "切换到浅色" }).click();
    await expect.poll(() => background(page)).toBe(LIGHT_BG);
    await page.setViewportSize(PHONE);
    await open(page, tab);
    await shoot(page, `visual-${tab}-360.png`);
    expect(await page.evaluate(() => document.documentElement.scrollWidth), "no sideways scroll").toBeLessThanOrEqual(
      PHONE.width,
    );
    const bar = await page.getByRole("tablist").boundingBox();
    expect(bar, "the section navigation is on screen").not.toBeNull();
    expect(Math.abs(bar!.y + bar!.height - PHONE.height), "the navigation sits on the bottom edge").toBeLessThanOrEqual(1);

    await page.context().close();
  });
}

test("两步确认用键盘打开时焦点落在「取消」，再按回车只是收回，不会执行", async ({ browser }) => {
  const org = "Focus Co 2b7";
  const signedUp = await stack.tenant().post("/api/signup", {
    org_name: org,
    email: "owner@focus-2b7.test",
    password: "focus-pass-2b7x",
  });
  const id: string = signedUp.tenant.id;
  await stack.admin.post(`/admin/tenants/${id}/approve`);

  const page = await systemDark(browser);
  await login(page);
  await open(page, "tenants");
  const row = page.getByRole("row").filter({ hasText: org });
  const suspend = row.getByRole("button", { name: "停用", exact: true });
  const cancel = row.getByRole("button", { name: "取消" });

  await suspend.focus();
  await page.keyboard.press("Enter");
  await expect(row.getByRole("button", { name: "确认停用" })).toBeVisible();
  await expect(cancel).toBeFocused();
  // The second Enter lands on 取消: the step closes and the keyboard is back on the action.
  await page.keyboard.press("Enter");
  await expect(row.getByRole("button", { name: "确认停用" })).toHaveCount(0);
  await expect(suspend).toBeFocused();
  // Escape takes the step back as well.
  await page.keyboard.press("Enter");
  await expect(cancel).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(row.getByRole("button", { name: "确认停用" })).toHaveCount(0);
  await expect(suspend).toBeFocused();

  expect((await stack.admin.get(`/admin/tenants/${id}`)).status, "nothing was suspended").toBe("active");
  await page.context().close();
});

test("分区导航只用键盘也能走遍：方向键、Home、End 移动，回车或空格打开", async ({ browser }) => {
  const page = await systemDark(browser);
  await login(page);
  const tab = (name: string) => page.getByRole("tab", { name, exact: true });
  const role = () => page.evaluate(() => document.activeElement?.getAttribute("role") ?? null);

  // Shift+Tab from the header reaches the route at its current stop, and Tab
  // leaves the route again rather than walking its stops.
  await page.getByRole("button", { name: "切换到深色" }).focus();
  await page.keyboard.press("Shift+Tab");
  await expect(tab("概览")).toBeFocused();
  await page.keyboard.press("Tab");
  expect(await role()).not.toBe("tab");
  await page.keyboard.press("Shift+Tab");
  await expect(tab("概览")).toBeFocused();

  // Down and up along the line, wrapping at both ends; the other axis too.
  await page.keyboard.press("ArrowDown");
  await expect(tab("网关")).toBeFocused();
  await page.keyboard.press("ArrowUp");
  await expect(tab("概览")).toBeFocused();
  await page.keyboard.press("ArrowUp");
  await expect(tab("审计日志")).toBeFocused();
  await page.keyboard.press("ArrowDown");
  await expect(tab("概览")).toBeFocused();
  await page.keyboard.press("End");
  await expect(tab("审计日志")).toBeFocused();
  await page.keyboard.press("Home");
  await expect(tab("概览")).toBeFocused();
  await page.keyboard.press("ArrowRight");
  await expect(tab("网关")).toBeFocused();
  // Moving the focus opens nothing by itself.
  await expect(tab("概览")).toHaveAttribute("aria-selected", "true");

  await page.keyboard.press("Enter");
  await expect(tab("网关")).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("tabpanel", { name: "网关" })).toBeVisible();
  await expect(tab("网关")).toBeFocused();
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Space");
  await expect(tab("上游账号")).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("tabpanel", { name: "上游账号" })).toBeVisible();
  await page.context().close();
});

test("提交被拒时焦点落在第一个被拒的字段上，表单的提示照常播报", async ({ browser }) => {
  const page = await systemDark(browser);
  await login(page);
  await open(page, "gateways");
  await page.getByRole("button", { name: "签发网关凭据" }).click();
  const name = page.getByLabel("名称");
  const days = page.getByLabel("有效期（天）");
  await name.fill("gw-focus-3c8");
  await days.fill("100000");
  await page.getByRole("button", { name: "签发" }).click();

  // Only the validity is refused, and the focus goes to it, past the name
  // before it; the form's own note still speaks for the whole refusal.
  await expect(days).toHaveAttribute("aria-invalid", "true");
  await expect(name).not.toHaveAttribute("aria-invalid", "true");
  await expect(days).toBeFocused();
  await expect(page.locator("main form").getByRole("alert")).toBeVisible();
  const { items } = await stack.admin.get("/admin/gateways/credentials");
  expect((items as { name: string }[]).map((c) => c.name)).not.toContain("gw-focus-3c8");
  await page.context().close();
});

test("两步确认在请求途中显示载入、不能再次确认也不能取消，结束后只保存了一次", async ({ browser }) => {
  // A tenant granted two models; the operator withdraws one, which asks first.
  const { items } = await stack.admin.get("/admin/resources/models");
  const byName = (name: string) => (items as { id: string; doc: { display_name?: string } }[]).find((m) => m.doc.display_name === name)!.id;
  const org = "Busy Co 5d1";
  const signedUp = await stack.tenant().post("/api/signup", { org_name: org, email: "owner@busy-5d1.test", password: PASSWORD });
  const id: string = signedUp.tenant.id;
  await stack.admin.post(`/admin/tenants/${id}/approve`);
  const approved = await stack.admin.get(`/admin/tenants/${id}`);
  await stack.admin.patch(`/admin/tenants/${id}`, { allowed_model_ids: [byName("chat-small"), byName("chat-large")] }, approved.version);
  const before = await stack.admin.get(`/admin/tenants/${id}`);

  const page = await systemDark(browser);
  await login(page);
  await open(page, "tenants");
  await page.getByRole("button", { name: org }).click();
  const grants = page.getByRole("group", { name: "授权模型" });
  await grants.getByRole("checkbox", { name: "chat-large" }).uncheck();
  await grants.getByRole("button", { name: "保存授权" }).click();
  const confirm = grants.getByRole("button", { name: "确认保存授权" });
  await expect(confirm).toBeVisible();

  // A slow link keeps the save in flight long enough to look at it, and to try again.
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("Network.enable");
  const link = (latency: number) =>
    cdp.send("Network.emulateNetworkConditions", { offline: false, latency, downloadThroughput: -1, uploadThroughput: -1 });
  await link(2_000);
  await confirm.click();
  // In flight: the confirm shows it, and neither it nor 取消 can be pressed.
  await expect(confirm).toHaveAttribute("aria-busy", "true");
  await expect(confirm).toBeDisabled();
  await expect(grants.getByRole("button", { name: "取消" })).toBeDisabled();
  await confirm.evaluate((button) => (button as HTMLButtonElement).click());
  await link(0);

  // Done: the step closes, the save is reported, and it was made once.
  await expect(grants).toContainText("已保存授权");
  await expect(confirm).toHaveCount(0);
  await expect(grants.getByRole("button", { name: "保存授权" })).toBeEnabled();
  await expect(grants.getByRole("alert")).toHaveCount(0);
  const after = await stack.admin.get(`/admin/tenants/${id}`);
  expect(after.allowed_model_ids).toEqual([byName("chat-small")]);
  expect(after.version, "one save, not two").toBe(before.version + 1);
  await page.context().close();
});
