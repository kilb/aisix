import { expect, test, type Browser, type Page, type TestInfo } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

/**
 * The portal's look, page by page: light whatever the system says, dark
 * only through the theme toggle and still dark after a reload, and a phone
 * width that neither scrolls sideways nor loses the route nav, which sits
 * at the bottom edge there. Each step saves a screenshot for the lane
 * review (`visual-<page>-{light,dark}.png` full-page; on the phone
 * `visual-<page>-360.png` as the screen shows it, with the route bar,
 * `visual-<page>-360-full.png` for the rest of the page and
 * `visual-<page>-360-dark.png` in dark); nothing is
 * compared pixel by pixel.
 *
 * Every context runs with the system set to dark, the setting the portal
 * must ignore. The organisation is set up through the real `/admin` and
 * `/api`, so its pages show keys, members and an invitation as they would.
 */

let stack: Stack;

const PASSWORD = "visual-pass-7e4x";
const OWNER = "owner@visual-7e4.test";
const ORG = "Visual Org 7e4";

/** The body's background in each theme: Concrete, and the dark page. */
const LIGHT_BG = "rgb(245, 247, 244)";
const DARK_BG = "rgb(18, 28, 39)";
const PHONE = { width: 360, height: 780 };

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();

  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: "acct-visual-7e4",
      provider: "openai",
      api_key: "sk-acct-visual-7e4-upstream",
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  const modelIds: string[] = [];
  for (const name of ["chat-small", "chat-large", "embed-v3"]) {
    const model = await stack.admin.post("/admin/resources/models", {
      doc: { display_name: name, provider: "openai", model_name: "gpt-4o-mini", provider_key_id: account.id },
    });
    modelIds.push(model.id);
  }

  const signedUp = await stack.tenant().post("/api/signup", {
    org_name: ORG,
    email: OWNER,
    password: PASSWORD,
    display_name: "Zhou Lan",
  });
  const id: string = signedUp.tenant.id;
  await stack.admin.post(`/admin/tenants/${id}/approve`);
  const current = await stack.admin.get(`/admin/tenants/${id}`);
  await stack.admin.patch(
    `/admin/tenants/${id}`,
    { allowed_model_ids: modelIds, limits: { key_concurrency_max: 20, key_rpm_max: 600 } },
    current.version,
  );

  for (const [email, role, name] of [
    ["chen@visual-7e4.test", "admin", "Chen Yizhou"],
    ["lin@visual-7e4.test", "member", "Lin Qi"],
  ]) {
    const invite = await stack.admin.post(`/admin/tenants/${id}/invites`, { email, role });
    await stack.tenant().post("/api/invites/accept", { token: invite.invite_token, password: PASSWORD, display_name: name });
  }

  const owner = stack.tenant();
  await owner.post("/api/login", { email: OWNER, password: PASSWORD });
  const prod = await owner.post("/api/keys", { label: "prod-backend", rate_limit: { concurrency: 20, rpm: 600 } });
  await owner.post("/api/keys", {
    label: "ci-runner",
    model_ids: modelIds.slice(0, 2),
    rate_limit: { concurrency: 4 },
    expires_at: "2099-12-31T00:00:00Z",
  });
  const staging = await owner.post("/api/keys", { label: "staging" });
  await owner.patch(`/api/keys/${staging.key.id}`, { disabled: true }, staging.key.version);
  const legacy = await owner.post("/api/keys", { label: "legacy-import", model_ids: modelIds.slice(1, 2) });
  const listed = await stack.admin.get(`/admin/keys/${legacy.key.id}`);
  await stack.admin.patch(`/admin/keys/${legacy.key.id}`, { disabled: true }, listed.version);
  await owner.post("/api/members/invites", { email: "new@visual-7e4.test", role: "member" });

  const member = stack.tenant();
  await member.post("/api/login", { email: "lin@visual-7e4.test", password: PASSWORD });
  await member.post("/api/keys", { label: "nightly-eval", model_ids: modelIds.slice(1) });

  // 账户, 用量 and 预算 need money and a call: a seven-figure balance (the widest
  // figure the readout may carry), one priced call so the figures have sub-cent
  // digits, and a key quota.
  await stack.admin.post(`/admin/tenants/${id}/credits`, { amount_usd: "1000000.00", reason: "visual funding" });
  await stack.admin.post(`/admin/tenants/${id}/credits`, { amount_usd: "234567.89", reason: "visual funding" });
  const startsAt = Date.now() + 3_000;
  await stack.admin.post("/admin/pricing/sell", {
    model_id: modelIds[0],
    effective_from: new Date(startsAt).toISOString(),
    card: { default: { base: { input: "1000", output: "2000" } } },
  });
  await new Promise((r) => setTimeout(r, Math.max(0, startsAt - Date.now()) + 250));
  await expect
    .poll(
      async () => {
        const res = await fetch(`${stack.proxyUrl}/v1/chat/completions`, {
          method: "POST",
          headers: { authorization: `Bearer ${prod.plaintext}`, "content-type": "application/json" },
          body: JSON.stringify({ model: "chat-small", messages: [{ role: "user", content: "hi" }] }),
        });
        await res.arrayBuffer();
        return res.status;
      },
      { message: "the gateway admits the priced call", timeout: 60_000 },
    )
    .toBe(200);
  await owner.post("/api/budgets", { scope: "api_key", scope_ref: prod.key.id, kind: "periodic", period: "month", limit_usd: "50.00" });
});

test.afterAll(async () => {
  await stack?.stop();
});

async function background(page: Page): Promise<string> {
  return page.evaluate(() => getComputedStyle(document.body).backgroundColor);
}

async function signIn(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(OWNER);
  await page.getByLabel("口令", { exact: true }).fill(PASSWORD);
  await page.getByRole("button", { name: "登录", exact: true }).click();
  await expect(page.getByRole("button", { name: "登出" })).toBeVisible();
}

/** Opens tab `name` and waits until its page shows what it holds. */
async function openTab(page: Page, name: string, shows: string): Promise<void> {
  await page.getByRole("tab", { name }).click();
  await expect(page.getByRole("main").getByText(shows, { exact: true }).first()).toBeVisible();
}

interface Shown {
  /** The file-name slug. */
  slug: string;
  /** The page's name in the test titles. */
  name: string;
  /** Whether the page is behind the sign-in, and so has the route nav. */
  signedIn: boolean;
  /** Brings the page up from the portal's address, signed in already when `signedIn`. */
  open: (page: Page) => Promise<void>;
}

const PAGES: Shown[] = [
  {
    slug: "login",
    name: "登录",
    signedIn: false,
    open: async (page) => {
      await page.goto(stack.previewUrl);
      await expect(page.getByLabel("邮箱")).toBeVisible();
    },
  },
  {
    slug: "signup",
    name: "注册组织",
    signedIn: false,
    open: async (page) => {
      await page.goto(stack.previewUrl);
      await page.getByRole("tab", { name: "注册组织" }).click();
      await expect(page.getByLabel("组织名称")).toBeVisible();
    },
  },
  {
    slug: "org",
    name: "组织",
    signedIn: true,
    open: async (page) => {
      await page.goto(stack.previewUrl);
      await expect(page.getByRole("main")).toContainText(ORG);
    },
  },
  {
    slug: "keys",
    name: "密钥",
    signedIn: true,
    open: async (page) => {
      await page.goto(stack.previewUrl);
      await openTab(page, "密钥", "legacy-import");
    },
  },
  {
    slug: "usage",
    name: "用量",
    signedIn: true,
    open: async (page) => {
      await page.goto(stack.previewUrl);
      await openTab(page, "用量", "调用记录");
      await expect(page.getByRole("table", { name: "调用记录" }).locator("tbody tr").first()).toBeVisible();
    },
  },
  {
    slug: "budgets",
    name: "预算",
    signedIn: true,
    open: async (page) => {
      await page.goto(stack.previewUrl);
      await openTab(page, "预算", "密钥额度");
      await expect(page.getByRole("table", { name: "密钥实际可用" }).locator("tbody tr").first()).toBeVisible();
      await expect(page.getByRole("table", { name: "密钥额度" }).locator("tbody tr").first()).toBeVisible();
    },
  },
  {
    slug: "billing",
    name: "账户",
    signedIn: true,
    open: async (page) => {
      await page.goto(stack.previewUrl);
      await openTab(page, "账户", "余额流水");
      // The seven-figure balance and the readout are in before the page is judged.
      await expect(page.getByTestId("readout-amount")).toContainText("$");
      await expect(page.getByRole("region", { name: "余额", exact: true })).toContainText("1,2");
    },
  },
  {
    slug: "members",
    name: "成员",
    signedIn: true,
    open: async (page) => {
      await page.goto(stack.previewUrl);
      await openTab(page, "成员", "new@visual-7e4.test");
    },
  },
];

/** A screenshot with transitions run to their end, so it shows the settled page. */
async function shot(page: Page, testInfo: TestInfo, file: string, fullPage = true): Promise<void> {
  const path = testInfo.outputPath(file);
  await page.screenshot({ path, fullPage, animations: "disabled" });
  await testInfo.attach(file, { path, contentType: "image/png" });
}

async function systemDarkPage(browser: Browser): Promise<Page> {
  const context = await browser.newContext({ colorScheme: "dark", viewport: { width: 1440, height: 900 } });
  return context.newPage();
}

for (const shown of PAGES) {
  test(`${shown.name}：系统为深色时仍默认浅色，深色只经切换并在刷新后保留，手机宽度不横向滚动`, async ({ browser }, testInfo) => {
    const page = await systemDarkPage(browser);
    if (shown.signedIn) await signIn(page);
    await shown.open(page);

    // Light by default, whatever the system says.
    await expect.poll(() => background(page)).toBe(LIGHT_BG);
    await shot(page, testInfo, `visual-${shown.slug}-light.png`);

    // Dark through the toggle, and still dark after a reload.
    await page.getByRole("button", { name: "切换到深色" }).click();
    await expect.poll(() => background(page)).toBe(DARK_BG);
    await shot(page, testInfo, `visual-${shown.slug}-dark.png`);
    await page.reload();
    await shown.open(page);
    await expect.poll(() => background(page)).toBe(DARK_BG);

    // Back to light, then a phone: nothing scrolls sideways, and the route
    // nav is a bar along the bottom edge.
    await page.getByRole("button", { name: "切换到浅色" }).click();
    await expect.poll(() => background(page)).toBe(LIGHT_BG);
    await page.setViewportSize(PHONE);
    await shown.open(page);
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(PHONE.width);
    if (shown.signedIn) {
      const box = await page.getByRole("tablist", { name: "组织门户分区" }).boundingBox();
      expect(box).not.toBeNull();
      expect(Math.abs(box!.y + box!.height - PHONE.height)).toBeLessThanOrEqual(1);
    }
    await shot(page, testInfo, `visual-${shown.slug}-360.png`, false);
    await shot(page, testInfo, `visual-${shown.slug}-360-full.png`);

    // Dark on the phone: no sideways scroll there either.
    await page.getByRole("button", { name: "切换到深色" }).click();
    await expect.poll(() => background(page)).toBe(DARK_BG);
    await shown.open(page);
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(PHONE.width);
    await shot(page, testInfo, `visual-${shown.slug}-360-dark.png`, false);
    await page.context().close();
  });
}

test("分区导航只用键盘也能走遍：方向键、Home、End 移动，回车打开", async ({ browser }) => {
  const page = await systemDarkPage(browser);
  await signIn(page);
  const tab = (name: string) => page.getByRole("tab", { name });
  const focused = () => page.evaluate(() => document.activeElement?.textContent ?? "");

  // Tab reaches the route at its current stop, and the next Tab leaves the
  // route rather than walking its stops.
  await page.getByRole("button", { name: "登出" }).focus();
  await page.keyboard.press("Tab");
  await expect(tab("组织")).toBeFocused();
  await page.keyboard.press("Tab");
  expect(await page.evaluate(() => document.activeElement?.getAttribute("role") ?? null)).not.toBe("tab");
  await page.keyboard.press("Shift+Tab");
  await expect(tab("组织")).toBeFocused();

  await page.keyboard.press("ArrowRight");
  await expect(tab("密钥")).toBeFocused();
  await page.keyboard.press("End");
  await expect(tab("账户")).toBeFocused();
  await page.keyboard.press("ArrowRight");
  await expect(tab("组织")).toBeFocused();
  await page.keyboard.press("ArrowLeft");
  await expect(tab("账户")).toBeFocused();
  await page.keyboard.press("Home");
  await expect(tab("组织")).toBeFocused();
  // Moving the focus opens nothing by itself.
  await expect(tab("组织")).toHaveAttribute("aria-selected", "true");

  await page.keyboard.press("ArrowRight");
  await page.keyboard.press("Enter");
  await expect(tab("密钥")).toHaveAttribute("aria-selected", "true");
  await expect(page.getByRole("main").getByText("legacy-import", { exact: true })).toBeVisible();
  // The open section is the tab's panel, named by it.
  await expect(page.getByRole("tabpanel", { name: "密钥" })).toContainText("legacy-import");
  expect(await focused()).toBe("密钥");
  await page.context().close();
});

test("事实行里的中点有自己的宽度，不压在后面的空格上", async ({ browser }) => {
  const page = await systemDarkPage(browser);
  await signIn(page);
  await openTab(page, "密钥", "prod-backend");
  // The key's limits read "并发 20 · 每分钟 600": both limits, joined by a dot.
  const limits = page.getByRole("row").filter({ hasText: "prod-backend" }).getByRole("cell").filter({ hasText: "·" });
  await expect(limits).toHaveCount(1);
  const { width, size } = await limits.evaluate(async (cell) => {
    const style = getComputedStyle(cell);
    await document.fonts.load(`${style.fontSize} ${style.fontFamily}`, "·");
    const walker = document.createTreeWalker(cell, NodeFilter.SHOW_TEXT);
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const at = (node as Text).data.indexOf("·");
      if (at < 0) continue;
      const range = document.createRange();
      range.setStart(node, at);
      range.setEnd(node, at + 1);
      return { width: range.getBoundingClientRect().width, size: parseFloat(style.fontSize) };
    }
    return { width: null, size: 0 };
  });
  // A Latin dot, spaced: neither drawn over the space after it (no advance)
  // nor a full-width CJK dot that splits the line into two phrases.
  expect(width, "the dot's own advance").toBeGreaterThan(2);
  expect(width, "a half-width dot, not a full-width one").toBeLessThan(size * 0.75);
  await page.context().close();
});

test("提交被拒时焦点落在第一个被拒的字段上，读屏能读到原因", async ({ browser }) => {
  const page = await systemDarkPage(browser);
  await signIn(page);
  await openTab(page, "密钥", "legacy-import");
  const label = page.getByLabel("名称", { exact: true });
  const concurrency = page.getByLabel("并发", { exact: true });
  const rpm = page.getByLabel("每分钟请求", { exact: true });
  await label.fill("focus-key-7e4");
  await concurrency.fill("3");
  // Over the organisation's cap of 600 a minute; the concurrency is within its cap.
  await rpm.fill("9000");
  await page.getByRole("button", { name: "新建密钥" }).click();

  // Only the per-minute limit is refused, and the focus goes to it, past the
  // fields before it, so a screen reader reads the field and its refusal.
  await expect(rpm).toHaveAttribute("aria-invalid", "true");
  await expect(concurrency).not.toHaveAttribute("aria-invalid", "true");
  await expect(rpm).toBeFocused();
  await expect(page.getByRole("main").getByText("focus-key-7e4", { exact: true })).toHaveCount(0);
  await page.context().close();
});
