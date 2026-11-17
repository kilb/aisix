import { expect, test } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

// 创建与编辑只切换未保存的表单，不改现存真实密钥或费用额度。
const session = process.env.KEY_LAYOUT_QA_SESSION!;
const output = process.env.KEY_LAYOUT_QA_OUTPUT!;
const origin = process.env.KEY_LAYOUT_ADMIN_ORIGIN!;
const world = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (world.ready !== true || world.realBackend !== true || world.mockUpstream !== false || !["127.0.0.1", "localhost"].includes(new URL(origin).hostname)) {
  throw new Error("Admin layout checks require a ready real isolated backend");
}
mkdirSync(output, { recursive: true });

for (const width of [1440, 390]) for (const flow of ["create", "edit"] as const) {
  test(`admin key ${flow} at ${width}: readable model grid and quota fields`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    const errors: string[] = [];
    const writes: string[] = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("request", request => {
      const path = new URL(request.url()).pathname;
      if (["PUT", "PATCH", "DELETE"].includes(request.method()) || (request.method() === "POST" && !path.endsWith("/login"))) writes.push(`${request.method()} ${path}`);
    });
    await page.goto(origin);
    await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
    await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
    const login = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login" && response.request().method() === "POST");
    await page.getByRole("button", { name: "登录", exact: true }).click();
    expect((await login).status()).toBe(200);
    await page.goto(`${origin}/#keys?tenant=${encodeURIComponent(auth.metricsTenantId ?? auth.tenantId)}`);
    if (flow === "create") await page.getByRole("button", { name: "新建密钥", exact: true }).click();
    else {
      await page.getByRole("button", { name: "编辑密钥", exact: true }).first().click();
      await expect(page.getByRole("heading", { name: "编辑密钥", exact: true })).toBeVisible();
    }
    await page.getByRole("combobox", { name: "模型范围", exact: true }).click();
    await page.getByRole("option", { name: "指定模型", exact: true }).click();
    const list = page.locator(".key-model-options").first();
    const choices = list.getByRole("checkbox");
    await expect.poll(() => choices.count()).toBeGreaterThan(1);
    await page.waitForLoadState("networkidle");
    await page.evaluate(() => document.fonts.ready);
    await page.screenshot({ path: join(output, `admin-key-${flow}-${width}.png`) });
    const layout = await list.evaluate(element => {
      const boxes = [...element.querySelectorAll("label")].map(label => label.getBoundingClientRect());
      let overlaps = 0;
      for (const [index, box] of boxes.entries()) for (const next of boxes.slice(index + 1)) {
        if (Math.min(box.right, next.right) - Math.max(box.left, next.left) > 1 && Math.min(box.bottom, next.bottom) - Math.max(box.top, next.top) > 1) overlaps += 1;
      }
      return {
        columns: getComputedStyle(element).gridTemplateColumns.split(" ").length,
        inputWidths: [...element.querySelectorAll("input")].map(input => input.getBoundingClientRect().width),
        overlaps,
        horizontalOverflow: element.scrollWidth > element.clientWidth + 1,
        height: element.getBoundingClientRect().height,
        heightLimit: 20 * parseFloat(getComputedStyle(document.documentElement).fontSize),
        scrollHeight: element.scrollHeight,
        clientHeight: element.clientHeight,
      };
    });
    expect(layout.columns).toBe(width > 639 ? 2 : 1);
    expect(layout.overlaps).toBe(0);
    expect(layout.horizontalOverflow).toBe(false);
    expect(layout.inputWidths.every(value => value >= 17)).toBe(true);
    expect(layout.height, "the full model directory leaves space for the remaining form fields").toBeLessThanOrEqual(layout.heightLimit + 1);
    if (layout.scrollHeight > layout.heightLimit + 1) expect(layout.scrollHeight).toBeGreaterThan(layout.clientHeight);
    const name = page.getByLabel("名称", { exact: true });
    const formWidth = await page.locator(".key-create-form").evaluate(element => element.getBoundingClientRect().width);
    expect((await name.boundingBox())!.width / formWidth).toBeGreaterThan(.95);
    const checkbox = choices.first();
    await checkbox.check();
    await expect(checkbox).toBeChecked();
    await choices.last().check();
    await expect(choices.last()).toBeChecked();
    const amount = flow === "create" ? page.getByLabel("费用额度（元）", { exact: true }) : page.getByRole("form", { name: "添加密钥费用额度", exact: true }).getByLabel("费用额度（元）", { exact: true });
    await amount.fill("3.000001");
    await expect(amount).toHaveValue("3.000001");
    expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
    expect(errors).toEqual([]);
    expect(writes).toEqual([]);
  });
}
