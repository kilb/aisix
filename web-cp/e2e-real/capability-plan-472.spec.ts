import { expect, test, type Page } from "@playwright/test";
import { readFileSync, mkdirSync, appendFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { chooseSelect } from "../e2e/select";

const session = resolve(process.env.CAPABILITY_PLAN_QA_SESSION!);
const output = resolve(process.env.CAPABILITY_PLAN_QA_OUTPUT!);
const origin = process.env.CAPABILITY_PLAN_QA_URL!;
const world = JSON.parse(readFileSync(join(session, "public.json"), "utf8"));
const auth = JSON.parse(readFileSync(join(session, "local-auth.json"), "utf8"));
if (!world.ready || !world.realBackend || world.mockUpstream !== false || new URL(origin).hostname !== "127.0.0.1") throw new Error("A real isolated backend is required");

async function openPlan(page: Page) {
  const reads: string[] = [];
  page.on("request", request => { if (request.method() === "GET" && new URL(request.url()).pathname.startsWith("/admin/")) reads.push(new URL(request.url()).pathname); });
  // 真实 API 建立同源会话后直达目标页，避免共享 QA 的概览统计查询。
  const login = await page.request.post(origin + "/admin/login", { data: auth.operator });
  expect(login.status()).toBe(200);
  await page.goto(origin + "/#model-tests?section=assessment");
  await expect(page.locator("main h1")).toHaveText("模型测试");
  await page.getByRole("button", { name: "新建能力对照", exact: true }).click();
  const dialog = page.getByRole("dialog", { name: "新建能力对照", exact: true });
  await expect(dialog).toBeVisible();
  await expect(dialog.locator("fieldset")).toBeEnabled();
  const choices = await dialog.locator("select").first().locator("option").evaluateAll(options => options.map(option => ({ value: (option as HTMLOptionElement).value, label: option.textContent ?? "" })).filter(option => option.value));
  expect(choices.length).toBeGreaterThan(2);
  return { dialog, choices, reads };
}

for (const width of [1440, 390]) {
  test(`incomplete plan explains required fields and a corrected plan persists without starting at ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 844 });
    const errors: string[] = [], writes: string[] = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => { if (message.type() === "error") errors.push(message.text()); });
    const { dialog, choices, reads } = await openPlan(page);
    page.on("request", request => { if (!["GET", "HEAD"].includes(request.method())) writes.push(new URL(request.url()).pathname); });
    const generate = dialog.getByRole("button", { name: "生成计划（不调用模型）", exact: true });
    // 禁用提交或丢掉字段错误时，这个实际用户路径必须变红。
    await expect(generate).toBeEnabled();
    await generate.click();
    await expect(dialog.getByText("请选择已保存的参考模型。", { exact: true })).toBeVisible();
    await expect(dialog.getByText("请填写来源核实时间（北京时间）。", { exact: true })).toBeVisible();
    await expect(dialog.getByText("请填写来源核实说明。", { exact: true })).toBeVisible();
    await expect(dialog.getByText("请审定参考来源并确认供应渠道独立性。", { exact: true })).toBeVisible();
    await expect(dialog.getByRole("checkbox")).not.toBeChecked();
    await expect(dialog.getByRole("combobox").first()).toBeFocused();
    expect(writes).toEqual([]);
    const reference = choices[0]!;
    const candidate = choices.find(choice => choice.label.split(" · ").at(-1) !== reference.label.split(" · ").at(-1))!;
    expect(candidate).toBeTruthy();
    await chooseSelect(dialog.getByRole("combobox").nth(0), { value: reference.value });
    await chooseSelect(dialog.getByRole("combobox").nth(1), { value: candidate.value });
    // 两个模型已选、独立性已确认时，缺说明必须给出可见原因。
    await dialog.getByRole("checkbox").check();
    await generate.click();
    await expect(dialog.getByText("请填写来源核实说明。", { exact: true })).toBeVisible();
    expect(writes).toEqual([]);
    mkdirSync(join(output, "screenshots"), { recursive: true });
    await page.screenshot({ path: join(output, "screenshots", `plan-missing-fields-${width}.png`), fullPage: true });
    const note = `472 real capability plan ${width} ${Date.now()}; only plan, no model calls`;
    await dialog.getByLabel("来源核实说明", { exact: true }).fill(note);
    await generate.click();
    await expect(dialog.locator("input[type=datetime-local]")).toBeFocused();
    expect(writes).toEqual([]);
    await dialog.locator("input[type=datetime-local]").fill("2099-01-01T12:00");
    await generate.click();
    await expect(dialog.getByText("核实时间须为有效的北京时间，且不能晚于当前时间。", { exact: true })).toBeVisible();
    expect(writes).toEqual([]);
    const beijing = new Date(Date.now() - 60_000 + 8 * 60 * 60_000).toISOString().slice(0, 16);
    await dialog.locator("input[type=datetime-local]").fill(beijing);
    await dialog.getByRole("checkbox").uncheck();
    await generate.click();
    await expect(dialog.getByText("请审定参考来源并确认供应渠道独立性。", { exact: true })).toBeVisible();
    await expect(dialog.getByRole("checkbox")).toBeFocused();
    expect(writes).toEqual([]);
    await dialog.getByRole("checkbox").check();
    await page.evaluate(() => document.fonts.ready);
    const geometry = await dialog.evaluate(element => ({ width: element.clientWidth, scroll: element.scrollWidth, left: element.getBoundingClientRect().left, right: element.getBoundingClientRect().right }));
    expect(geometry.scroll).toBeLessThanOrEqual(geometry.width + 1);
    expect(geometry.left).toBeGreaterThanOrEqual(0);
    expect(geometry.right).toBeLessThanOrEqual(width);
    await page.screenshot({ path: join(output, "screenshots", `plan-corrected-${width}.png`), fullPage: true });
    const planned = page.waitForResponse(response => new URL(response.url()).pathname === "/admin/model-assessments/plan");
    await generate.click();
    const response = await planned;
    expect(response.status()).toBe(201);
    const run = await response.json();
    expect(run.id).toMatch(/^[0-9a-f-]{36}$/);
    expect(run.state).toBe("planned");
    expect(run.reference.model_id).toBe(reference.value);
    expect(run.candidate.model_id).toBe(candidate.value);
    expect(run.reference_source.note).toBe(note);
    expect(run.reference_source.independence_confirmed).toBe(true);
    expect(run.planned_requests).toBe(20);
    for (const key of ["reserved_requests", "sent_requests", "unknown_requests", "finished_requests"]) {
      expect(Object.hasOwn(run, key)).toBe(true);
      expect(run[key]).toBe(0);
    }
    appendFileSync(join(output, "created-plans.jsonl"), JSON.stringify({ id: run.id, state: run.state, created_at: run.created_at, note }) + "\n");
    await expect(dialog).toHaveCount(0);
    await expect(page.locator(".assessment-run-id")).toHaveText(run.id);
    await page.reload();
    await expect(page.locator(".assessment-run-id")).toHaveText(run.id);
    await expect(page.getByRole("button", { name: "开始对照（费用未知）", exact: true })).toBeDisabled();
    await page.screenshot({ path: join(output, "screenshots", `plan-persisted-${width}.png`), fullPage: true });
    expect(writes).toEqual(["/admin/model-assessments/plan"]);
    appendFileSync(join(output, "http-paths.jsonl"), JSON.stringify({ width, reads, writes }) + "\n");
    expect(reads.filter(path => /\/(?:usage|metrics|profit)(?:\/|$)|\/billing\/summary(?:\/|$)|\/pricing\/status(?:\/|$)/.test(path))).toEqual([]);
    expect(errors).toEqual([]);
  });
}

test("same-account comparison explains refusal without creating or starting a plan", async ({ page }) => {
  const { dialog, choices, reads } = await openPlan(page);
  const writes: string[] = [];
  page.on("request", request => { if (!["GET", "HEAD"].includes(request.method())) writes.push(new URL(request.url()).pathname); });
  const reference = choices[0]!;
  const candidate = choices.find(choice => choice.value !== reference.value && choice.label.split(" · ").at(-1) === reference.label.split(" · ").at(-1))!;
  expect(candidate).toBeTruthy();
  await chooseSelect(dialog.getByRole("combobox").nth(0), { value: reference.value });
  await chooseSelect(dialog.getByRole("combobox").nth(1), { value: candidate.value });
  await dialog.getByRole("button", { name: "生成计划（不调用模型）", exact: true }).click();
  await expect(dialog.getByText("参考与被测必须使用不同账号。", { exact: true })).toBeVisible();
  await expect(dialog.getByRole("combobox").nth(1)).toBeFocused();
  expect(writes).toEqual([]);
  expect(reads.filter(path => /\/(?:usage|metrics|profit)(?:\/|$)|\/billing\/summary(?:\/|$)|\/pricing\/status(?:\/|$)/.test(path))).toEqual([]);
});
