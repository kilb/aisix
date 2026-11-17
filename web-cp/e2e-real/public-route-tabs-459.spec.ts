import { expect, test } from "@playwright/test";
import { readFileSync, mkdirSync } from "node:fs";
import { join, resolve } from "node:path";

const session = resolve(process.env.FLOATING_FILTERS_QA_SESSION!);
const output = resolve(process.env.FLOATING_FILTERS_QA_OUTPUT!);
const world = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (!world.ready || !world.realBackend || world.mockUpstream !== false || new URL(world.adminUrl).hostname !== "127.0.0.1") throw new Error("A real isolated backend is required");

for (const width of [1440, 390]) {
  test(`public model tabs share a full-width page and preserve the draft at ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 844 });
    const errors: string[] = [];
    const reads: string[] = [];
    const writes: { method: string; path: string; body: unknown }[] = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
    await page.goto(world.adminUrl);
    await page.getByLabel("邮箱", { exact: true }).fill(auth.operator.email);
    await page.getByLabel("口令", { exact: true }).fill(auth.operator.password);
    const login = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/login");
    await page.getByRole("button", { name: "登录", exact: true }).click();
    expect((await login).status()).toBe(200);
    await expect(page.locator("main h1:visible")).toHaveCount(1);
    // 隔离环境使用 Secure 会话；浏览器真实同源读取才能带上本地会话。
    const response = await page.evaluate(async () => {
      const result = await fetch("/admin/public-models", { credentials: "same-origin" });
      return { status: result.status, body: await result.json() };
    });
    expect(response.status).toBe(200);
    const models = response.body.items;
    const model = models.find((row: { retired_at: string | null; served_by: { enabled: boolean }[] }) => row.retired_at === null && row.served_by.some(account => account.enabled));
    expect(model, "the real fixture contains a serving public model").toBeTruthy();
    page.on("request", request => {
      const path = new URL(request.url()).pathname;
      if (request.method() === "GET" && path.startsWith("/admin/")) reads.push(path);
      if (!["GET", "HEAD"].includes(request.method())) writes.push({ method: request.method(), path, body: request.postDataJSON() });
    });
    await page.goto(world.adminUrl + "/#public-routes?section=" + model.id);
    await expect(page.locator("main h1")).toHaveText(model.name);
    await page.waitForLoadState("networkidle");
    await page.evaluate(() => document.fonts.ready);
    const tabs = page.getByRole("tablist", { name: "公共模型设置", exact: true });
    await expect(tabs).toBeVisible();
    await expect(tabs.getByRole("tab", { name: "编辑模型", exact: true })).toHaveAttribute("aria-selected", "true");
    const edit = page.getByRole("tabpanel", { name: "编辑模型", exact: true });
    const editorWidth = (await edit.getByRole("form", { name: `编辑模型「${model.name}」`, exact: true }).boundingBox())!;
    const workspaceWidth = (await tabs.boundingBox())!;
    expect(Math.abs(editorWidth.x - workspaceWidth.x)).toBeLessThanOrEqual(1);
    expect(Math.abs(editorWidth.width - workspaceWidth.width)).toBeLessThanOrEqual(1);
    mkdirSync(join(output, "screenshots"), { recursive: true });
    await page.screenshot({ path: join(output, "screenshots", `public-edit-${width}.png`), animations: "disabled" });
    const draft = edit.getByRole("spinbutton").first();
    const original = await draft.inputValue();
    const changed = original === "17" ? "18" : "17";
    await draft.fill(changed);
    await tabs.getByRole("tab", { name: "路由明细", exact: true }).click();
    const routes = page.getByRole("tabpanel", { name: "路由明细", exact: true });
    await expect(routes).toBeVisible();
    const routesWidth = (await routes.locator(":scope > section").boundingBox())!;
    expect(Math.abs(routesWidth.x - editorWidth.x)).toBeLessThanOrEqual(1);
    expect(Math.abs(routesWidth.width - editorWidth.width)).toBeLessThanOrEqual(1);
    const regenerate = routes.getByRole("button", { name: "重新生成", exact: true });
    await expect(regenerate).toBeVisible();
    await expect(routes.getByRole("heading", { name: "重新生成", exact: true })).toHaveCount(0);
    const button = (await regenerate.boundingBox())!;
    const heading = (await routes.getByRole("heading", { name: /^路由明细/ }).boundingBox())!;
    expect(button.x).toBeGreaterThan(heading.x + heading.width);
    expect(Math.abs(button.y - heading.y)).toBeLessThanOrEqual(10);
    expect(reads.filter(path => path === "/admin/pricing/baselines")).toEqual([]);
    const pricesReadBefore = reads.filter(path => path === "/admin/pricing/models").length;
    const baselineReply = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/pricing/baselines");
    await tabs.getByRole("tab", { name: "基准价", exact: true }).click();
    const baselineResponse = await baselineReply;
    expect(baselineResponse.status()).toBe(200);
    const expectedBaseline = (await baselineResponse.json()).items.find((row: { model_id: string }) => row.model_id === model.id);
    expect(expectedBaseline, "the selected model has its own baseline entry").toBeTruthy();
    const baseline = page.getByRole("tabpanel", { name: "基准价", exact: true });
    await expect(baseline).toBeVisible();
    await expect(baseline.getByText("正在读取完整基准价…", { exact: true })).toHaveCount(0);
    await expect(baseline.getByRole("heading", { name: "基准价", exact: true })).toBeVisible();
    await expect(baseline.getByRole("heading", { name: "现行基准价", exact: true })).toBeVisible();
    await expect(baseline.getByRole("region", { name: `${model.name} 的基准价`, exact: true })).toBeVisible();
    if (expectedBaseline.current) {
      await expect(baseline.getByText(`版本 #${expectedBaseline.current.revision_id}`, { exact: true })).toBeVisible();
      if (expectedBaseline.current.source.note) await expect(baseline.getByText(expectedBaseline.current.source.note, { exact: true })).toBeVisible();
      const inputPrice = expectedBaseline.current.card.default?.base?.input;
      if (inputPrice !== undefined) await expect(baseline.getByRole("row", { name: /^输入 美元 \/ 百万 tokens/ }).getByText(`$${inputPrice}`, { exact: true })).toBeVisible();
    } else {
      await expect(baseline.getByText("未定价", { exact: true })).toBeVisible();
    }
    const baselineWidth = (await baseline.locator(":scope > section").boundingBox())!;
    expect(Math.abs(baselineWidth.x - editorWidth.x)).toBeLessThanOrEqual(1);
    expect(Math.abs(baselineWidth.width - editorWidth.width)).toBeLessThanOrEqual(1);
    if (width === 1440) {
      const baselineCard = baseline.locator(":scope > section");
      const originalStyle = await baselineCard.getAttribute("style");
      await baselineCard.evaluate(element => { (element as HTMLElement).style.width = "calc(100% - 24px)"; });
      const narrowed = (await baselineCard.boundingBox())!;
      expect(Math.abs(narrowed.width - editorWidth.width), "the same-width check rejects a genuinely narrower card").toBeGreaterThan(20);
      await baselineCard.evaluate((element, style) => {
        if (style === null) element.removeAttribute("style"); else element.setAttribute("style", style);
      }, originalStyle);
      expect(Math.abs((await baselineCard.boundingBox())!.width - editorWidth.width)).toBeLessThanOrEqual(1);
      await test.info().attach("width-negative-control", { body: JSON.stringify({ mutation: "baseline card width reduced by 24px in the live DOM only", editorWidth, narrowed, restored: await baselineCard.boundingBox() }), contentType: "application/json" });
    }
    await expect(page.getByRole("dialog", { name: `${model.name} 的基准价`, exact: true })).toHaveCount(0);
    await expect(baseline.getByRole("button", { name: "新基准价", exact: true })).toBeVisible();
    await baseline.getByRole("button", { name: "新基准价", exact: true }).click();
    await expect(page.getByRole("dialog", { name: "新基准价", exact: true })).toBeVisible();
    const dialog = (await page.getByRole("dialog", { name: "新基准价", exact: true }).boundingBox())!;
    expect(dialog.x).toBeGreaterThanOrEqual(0);
    expect(dialog.x + dialog.width).toBeLessThanOrEqual(width);
    await page.keyboard.press("Escape");
    await expect(page.getByRole("dialog", { name: "新基准价", exact: true })).toHaveCount(0);
    await tabs.getByRole("tab", { name: "编辑模型", exact: true }).click();
    await expect(draft).toHaveValue(changed);
    expect(writes, "switching tabs and cancelling a price dialog never writes").toEqual([]);
    expect(reads.filter(path => path === "/admin/pricing/models").length, "baseline reuses the current model's group prices").toBe(pricesReadBefore);
    await draft.fill(original);
    await tabs.getByRole("tab", { name: "路由明细", exact: true }).click();
    const before = model.served_by;
    await regenerate.click();
    await expect(page.getByText("已重新生成", { exact: true })).toBeVisible();
    const saved = await page.evaluate(async () => {
      const result = await fetch("/admin/public-models", { credentials: "same-origin" });
      return { status: result.status, body: await result.json() };
    });
    expect(saved.status).toBe(200);
    const after = saved.body.items.find((row: { id: string }) => row.id === model.id);
    expect(after.served_by).toEqual(before);
    expect(after.effective_routing).toEqual(model.effective_routing);
    expect(writes).toEqual(model.served_by.map((row: { provider_key_id: string; upstream_model: string; priority: number; weight: number; enabled: boolean }) => ({
      method: "PUT",
      path: `/admin/provider-keys/${row.provider_key_id}/channel-models/${model.id}`,
      body: { upstream_model: row.upstream_model, priority: row.priority, weight: row.weight, enabled: row.enabled },
    })));
    const documentWidth = await page.evaluate(() => ({ width: document.documentElement.clientWidth, scroll: document.documentElement.scrollWidth }));
    expect(documentWidth.scroll).toBeLessThanOrEqual(documentWidth.width + 1);
    await expect(page.locator("vite-error-overlay")).toHaveCount(0);
    expect(errors).toEqual([]);
    await page.evaluate(() => window.scrollTo(0, 0));
    await page.screenshot({ path: join(output, "screenshots", `public-routes-${width}.png`), animations: "disabled" });
    await tabs.getByRole("tab", { name: "基准价", exact: true }).click();
    await page.evaluate(() => window.scrollTo(0, 0));
    await page.screenshot({ path: join(output, "screenshots", `public-baseline-${width}.png`), animations: "disabled" });
    await test.info().attach("real-public-model-state", { body: JSON.stringify({ modelId: model.id, modelName: model.name, workspaceWidth, editorWidth, routesWidth, baselineWidth, button, heading, reads, writes, errors, documentWidth }), contentType: "application/json" });
  });
}
