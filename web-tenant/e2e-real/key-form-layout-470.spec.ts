import { expect, test, type Locator, type Page } from "@playwright/test";
import { mkdirSync, readFileSync } from "node:fs";
import { join } from "node:path";

// 仅用保留的真实授权模型和密钥观察布局；所有修改停留在未提交的表单。
const session = process.env.KEY_LAYOUT_QA_SESSION!;
const output = process.env.KEY_LAYOUT_QA_OUTPUT!;
const world = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
const tenantOrigin = process.env.KEY_LAYOUT_TENANT_ORIGIN!;
const adminOrigin = process.env.KEY_LAYOUT_ADMIN_ORIGIN!;
if (world.ready !== true || world.realBackend !== true || world.mockUpstream !== false) {
  throw new Error("Layout checks require the ready real isolated backend");
}
for (const origin of [tenantOrigin, adminOrigin]) {
  if (!["localhost", "127.0.0.1"].includes(new URL(origin).hostname)) throw new Error("Only isolated loopback origins are allowed");
}
mkdirSync(output, { recursive: true });

async function login(page: Page, admin = false): Promise<void> {
  const who = admin ? auth.operator : auth.metricsOwner ?? auth.owner;
  await page.goto(admin ? adminOrigin : tenantOrigin);
  await page.getByLabel("邮箱", { exact: true }).fill(who.email);
  await page.getByLabel("口令", { exact: true }).fill(who.password);
  const answer = page.waitForResponse(response => new URL(response.url()).pathname === (admin ? "/admin/login" : "/api/login") && response.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await answer).status()).toBe(200);
  await page.waitForLoadState("networkidle");
  await page.evaluate(() => document.fonts.ready);
}

function watch(page: Page): { errors: string[]; writes: string[] } {
  const observed = { errors: [] as string[], writes: [] as string[] };
  page.on("pageerror", error => observed.errors.push(error.message));
  page.on("request", request => {
    const path = new URL(request.url()).pathname;
    if (["PUT", "PATCH", "DELETE"].includes(request.method()) || (request.method() === "POST" && !path.endsWith("/login"))) observed.writes.push(`${request.method()} ${path}`);
  });
  return observed;
}

async function geometry(options: Locator): Promise<{ overlaps: number; outside: number }> {
  return options.evaluateAll(inputs => {
    const rectangles = inputs.map(input => input.closest("label")!.getBoundingClientRect());
    let overlaps = 0;
    let outside = 0;
    for (const [index, box] of rectangles.entries()) {
      if (box.x < 0 || box.right > window.innerWidth + 1) outside += 1;
      for (const next of rectangles.slice(index + 1)) {
        const x = Math.min(box.right, next.right) - Math.max(box.left, next.left);
        const y = Math.min(box.bottom, next.bottom) - Math.max(box.top, next.top);
        if (x > 1 && y > 1) overlaps += 1;
      }
    }
    return { overlaps, outside };
  });
}

for (const width of [1440, 390]) {
  for (const flow of ["create", "edit"] as const) {
    test(`portal key ${flow} at ${width}: readable model choices and usable full-width sections`, async ({ page }) => {
      await page.setViewportSize({ width, height: 1000 });
      const observed = watch(page);
      await login(page);
      await page.goto(`${tenantOrigin}/#keys`);
      if (flow === "create") {
        await page.getByRole("button", { name: "新建密钥", exact: true }).first().click();
        await expect(page.getByRole("dialog", { name: "新建密钥", exact: true })).toBeVisible();
      } else {
        await page.locator(".key-detail-link").first().click();
        await page.getByRole("button", { name: "修改密钥", exact: true }).click();
        await expect(page.getByRole("heading", { name: "修改密钥", exact: true })).toBeVisible();
      }
      const form = page.locator("form").filter({ has: page.getByRole("group", { name: "可用模型", exact: true }) });
      const models = form.getByRole("group", { name: "可用模型", exact: true });
      await models.getByRole("radio", { name: "指定模型", exact: true }).check();
      await models.getByRole("button", { name: "展开模型列表", exact: true }).click();
      const options = models.getByRole("checkbox");
      await expect(options.first()).toBeVisible();
      expect(await options.count(), "real model choices are present").toBeGreaterThan(1);
      await page.waitForLoadState("networkidle");
      const framing = await form.evaluate(element => {
        const bordered = (node: Element) => {
          const css = getComputedStyle(node);
          return [css.borderTopWidth, css.borderRightWidth, css.borderBottomWidth, css.borderLeftWidth].some(width => parseFloat(width) > 0);
        };
        return {
          optionFrames: [...element.querySelectorAll(".pick")].filter(bordered).length,
          fieldsetFrames: [...element.querySelectorAll("fieldset")].filter(bordered).length,
          textInputBorder: bordered(element.querySelector('input:not([type="checkbox"]):not([type="radio"])')!),
          nestedCardShadow: element.closest(".form-dialog") ? getComputedStyle(element.closest(".panel")!).boxShadow : null,
        };
      });
      expect(framing.optionFrames, "individual choices do not add a card frame to every model").toBe(0);
      expect(framing.fieldsetFrames, "fieldset semantics do not add nested group frames").toBe(0);
      expect(framing.textInputBorder, "editable inputs retain a visible boundary").toBe(true);
      if (flow === "create") expect(framing.nestedCardShadow, "the dialog does not contain another framed card").toBe("none");
      await test.info().attach("observed-form-framing", { body: JSON.stringify(framing), contentType: "application/json" });
      await page.screenshot({ path: join(output, `portal-${flow}-${width}.png`) });

      // 每个真实选项有独立命中区域，文字与相邻选项不能相互覆盖。
      expect(await geometry(options)).toEqual({ overlaps: 0, outside: 0 });
      const section = await models.boundingBox();
      const whole = await form.boundingBox();
      expect(section).not.toBeNull();
      expect(whole).not.toBeNull();
      expect(section!.width / whole!.width, "model scope uses the available form width").toBeGreaterThan(.95);
      const wanted = options.first();
      await options.last().uncheck();
      await wanted.check();
      await expect(wanted).toBeChecked();
      // 移开鼠标，避免悬停底色掩盖选中底色缺失的回归。
      await page.mouse.move(0, 0);
      await page.evaluate(() => new Promise(resolveFrame => requestAnimationFrame(() => requestAnimationFrame(resolveFrame))));
      const choiceBackground = await wanted.evaluate(input => getComputedStyle(input.closest("label")!).backgroundColor);
      const emptyBackground = await options.last().evaluate(input => getComputedStyle(input.closest("label")!).backgroundColor);
      expect(choiceBackground, "selected models retain a distinct soft fill").not.toBe(emptyBackground);
      await wanted.focus();
      await page.keyboard.press("Space");
      await expect(wanted).not.toBeChecked();
      const focus = await wanted.evaluate(input => {
        const css = getComputedStyle(input.closest("label")!);
        return { width: parseFloat(css.outlineWidth), style: css.outlineStyle };
      });
      expect(focus.width, "keyboard navigation retains a visible focus ring").toBeGreaterThanOrEqual(2);
      expect(focus.style).not.toBe("none");
      await page.keyboard.press("Space");
      await expect(wanted).toBeChecked();
      const search = models.getByRole("searchbox", { name: "搜索模型", exact: true });
      const modelName = await wanted.getAttribute("aria-label") ?? (await wanted.locator("..").innerText()).trim();
      await search.fill(modelName);
      await expect(models.getByRole("checkbox", { name: modelName, exact: true })).toBeVisible();
      await search.clear();
      const submit = form.getByRole("button", { name: flow === "create" ? "新建密钥" : "保存修改", exact: true });
      await submit.scrollIntoViewIfNeeded();
      await expect(submit).toBeVisible();
      expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth + 1)).toBe(true);
      expect(observed.errors).toEqual([]);
      expect(observed.writes).toEqual([]);
    });
  }

  test(`admin tenant group choices at ${width}: corner options remain clickable`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    const observed = watch(page);
    await login(page, true);
    await page.goto(`${adminOrigin}/#tenants?tenant=${encodeURIComponent(auth.metricsTenantId ?? auth.tenantId)}&view=access`);
    const groups = page.getByRole("group", { name: "可用分组", exact: true });
    const options = groups.getByRole("checkbox");
    await expect(options.first()).toBeVisible();
    await expect.poll(() => options.count(), { message: "the real group directory has finished loading" }).toBeGreaterThan(1);
    await options.first().scrollIntoViewIfNeeded();
    await page.screenshot({ path: join(output, `admin-group-${width}.png`) });
    for (const option of [options.first(), options.last()]) {
      await option.scrollIntoViewIfNeeded();
      const reachable = await option.evaluate(input => {
        const box = input.getBoundingClientRect();
        const hit = document.elementFromPoint(box.x + box.width / 2, box.y + box.height / 2);
        return hit === input || hit?.closest("label") === input.closest("label");
      });
      expect(reachable, "the visible corner checkbox is not clipped from hit testing").toBe(true);
      const checked = await option.isChecked();
      await option.setChecked(!checked);
      await expect(option).toBeChecked({ checked: !checked });
    }
    expect(observed.errors).toEqual([]);
    expect(observed.writes).toEqual([]);
  });
}
