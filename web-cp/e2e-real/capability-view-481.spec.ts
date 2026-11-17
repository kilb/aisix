import { expect, test } from "@playwright/test";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const session = resolve(process.env.CAPABILITY_UI_QA_SESSION!);
const output = resolve(process.env.CAPABILITY_UI_QA_OUTPUT!);
const state = JSON.parse(readFileSync(join(session, "stack-private.json"), "utf8"));
const seed = JSON.parse(readFileSync(join(session, "seed.json"), "utf8")) as { plans: { id: string; state: string; sent_requests: number }[] };
if (new URL(state.previewUrl).hostname !== "127.0.0.1" || seed.plans.length < 2 || seed.plans.some(plan => plan.state !== "planned" || plan.sent_requests !== 0)) throw new Error("Two real unstarted local plans are required");

for (const width of [1440, 390]) {
  test(`viewing an already selected comparison brings its details into view, and another record opens at ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 960 });
    const errors: string[] = [], writes: string[] = [], reads: string[] = [], forbidden: string[] = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
    const login = await page.request.post(state.previewUrl + "/admin/login", { data: state.operator });
    expect(login.status()).toBe(200);
    page.on("request", request => {
      const url = new URL(request.url());
      if (url.pathname.startsWith("/admin/") && request.method() === "GET") reads.push(url.pathname);
      if (!["GET", "HEAD"].includes(request.method())) writes.push(`${request.method()} ${url.pathname}`);
      if (url.hostname !== "127.0.0.1" || /^\/v1\/|\/start$|\/probe(?:\/|$)/.test(url.pathname)) forbidden.push(url.pathname);
    });
    const current = seed.plans[1]!, other = seed.plans[0]!;
    await page.goto(state.previewUrl + `/#model-tests?section=assessment&assessment_run=${current.id}`);
    await expect(page.locator("main h1")).toHaveText("模型测试");
    await expect(page.locator(".assessment-run-id")).toHaveText(current.id);
    const viewing = page.getByRole("button", { name: `查看能力对照 ${current.id}`, exact: true });
    await expect(viewing).toBeEnabled();
    await viewing.click();
    // 同一记录的按钮失去导航行为时，详情仍留在屏幕之外，此检查必须失败。
    await expect(page.getByRole("heading", { name: "计划与运行", exact: true })).toBeInViewport();
    await expect(page.getByRole("region", { name: "能力对照详情", exact: true })).toBeFocused();
    await expect(page.locator(".assessment-run-id")).toHaveText(current.id);
    await page.getByRole("button", { name: `查看能力对照 ${other.id}`, exact: true }).click();
    await expect(page.locator(".assessment-run-id")).toHaveText(other.id);
    await expect(page.getByRole("heading", { name: "计划与运行", exact: true })).toBeInViewport();
    await expect(page).toHaveURL(new RegExp(`assessment_run=${other.id}`));
    await expect(page.getByRole("button", { name: "开始对照（费用未知）", exact: true })).toBeDisabled();
    await page.reload();
    await expect(page.locator(".assessment-run-id")).toHaveText(other.id);
    await expect(page.getByRole("table", { name: "能力对照样本", exact: true })).toBeHidden();
    const evidence = page.locator("summary").filter({ hasText: "样本与证据" });
    await evidence.click();
    const samples = page.getByRole("table", { name: "能力对照样本", exact: true });
    await expect(samples).toBeVisible();
    await expect(samples.getByRole("row")).toHaveCount(21);
    await expect(samples.getByRole("row").nth(1)).toContainText("未执行");
    await samples.locator("summary").first().click();
    await expect(samples.getByText("无回答预览", { exact: true }).first()).toBeVisible();
    await evidence.click();
    await expect(samples).toBeHidden();
    await page.getByRole("button", { name: `查看能力对照 ${other.id}`, exact: true }).click();
    await expect(page.getByRole("heading", { name: "计划与运行", exact: true })).toBeInViewport();
    await page.evaluate(() => document.fonts.ready);
    mkdirSync(output, { recursive: true });
    await page.screenshot({ path: join(output, `details-${width}.png`), fullPage: false });
    const geometry = await page.locator("main").evaluate(element => ({ width: element.clientWidth, scrollWidth: element.scrollWidth }));
    expect(geometry.scrollWidth).toBeLessThanOrEqual(geometry.width + 1);
    await page.getByRole("tab", { name: "当前测试", exact: true }).click();
    await expect(page.getByRole("tab", { name: "当前测试", exact: true })).toHaveAttribute("aria-selected", "true");
    await page.getByRole("tab", { name: "能力对照", exact: true }).click();
    await expect(page.getByRole("table", { name: "能力对照历史", exact: true })).toBeVisible();
    await expect(page).toHaveURL(state.previewUrl + "/#model-tests?section=assessment");
    await page.screenshot({ path: join(output, `history-${width}.png`), fullPage: false });
    expect(await page.title()).not.toBe("");
    expect(await page.locator("vite-error-overlay").count()).toBe(0);
    expect(reads).toContain(`/admin/model-assessments/${current.id}`);
    expect(reads).toContain(`/admin/model-assessments/${other.id}`);
    expect(writes).toEqual([]);
    expect(forbidden).toEqual([]);
    expect(errors).toEqual([]);
    writeFileSync(join(output, `health-${width}.json`), JSON.stringify({ url: page.url(), title: await page.title(), width, reads, writes, forbidden, errors, realBackend: true, inference: "not executed", browser: "Browser plugin not available; repository Playwright used" }, null, 2));
  });
}

for (const role of ["viewer", "operator"] as const) {
  test(`${role} keeps the administrator permission boundary for comparison reads and navigation`, async ({ page }) => {
    const auth = JSON.parse(readFileSync(join(session, "role-auth.json"), "utf8"));
    const login = await page.request.post(state.previewUrl + "/admin/login", { data: auth[role] });
    expect(login.status()).toBe(200);
    const cookie = login.headers()["set-cookie"]!.split(";")[0]!;
    const denied = await page.request.get(state.previewUrl + "/admin/model-assessments", { headers: { cookie } });
    expect(denied.status()).toBe(403);
    const assessmentReads: string[] = [];
    page.on("request", request => { if (new URL(request.url()).pathname.startsWith("/admin/model-assessments")) assessmentReads.push(request.method()); });
    await page.goto(state.previewUrl + "/#model-tests?section=assessment");
    await expect(page.getByRole("tab", { name: "能力对照", exact: true })).toBeDisabled();
    await expect(page.getByText("能力对照的计划、运行、历史与导出仅管理员可用。", { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "新建能力对照", exact: true })).toHaveCount(0);
    await page.getByRole("tab", { name: "测试历史", exact: true }).click();
    await expect(page.getByRole("tab", { name: "测试历史", exact: true })).toHaveAttribute("aria-selected", "true");
    expect(assessmentReads).toEqual([]);
  });
}
