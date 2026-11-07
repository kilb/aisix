import { expect, test, type Browser, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

/**
 * The look of every main page, against the real control plane through the
 * built `dist/`: light by default even when the system asks for dark, dark
 * only through the toggle and kept across a reload, and at phone width no
 * sideways scroll with the section navigation as a bar along the bottom.
 *
 * Each step saves a full-page screenshot (`visual-<page>-<light|dark|360>.png`)
 * for review. Nothing compares pixels: the assertions are the computed
 * background, the page width and where the navigation sits.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
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
  ["tenants", "租户"],
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
  await page.screenshot({ path, fullPage: true });
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
