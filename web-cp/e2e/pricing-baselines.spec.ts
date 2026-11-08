import { randomUUID } from "node:crypto";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";

/**
 * 价格 · 基准价 against the real control plane through the built `dist/`.
 * Two models are created through the real `/admin` and neither is priced:
 * every baseline in these tests is set through the page.
 */

let stack: Stack;
let account: string;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
  const key = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: "acct-baseline-6e2",
      provider: "openai",
      api_key: "sk-acct-baseline-6e2-upstream",
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  account = key.id;
});

test.afterAll(async () => {
  await stack?.stop();
});

/**
 * A model with no baseline. `model_name` is the name the catalogue knows, so
 * the catalogue path preselects that model's entry by itself.
 */
async function newModel(prefix: string, modelName: string): Promise<{ id: string; name: string }> {
  const name = `${prefix}-${randomUUID().slice(0, 8)}`;
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: name, provider: "openai", model_name: modelName, provider_key_id: account },
  });
  return { id: model.id, name };
}

async function login(page: Page, email: string, password: string): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(email);
  await page.getByLabel("口令", { exact: true }).fill(password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

async function openBaselines(page: Page, who = stack.operator): Promise<void> {
  await login(page, who.email, who.password);
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("button", { name: "基准价", exact: true }).click();
  // The heading is up at once; the list under it is still being read.
  await expect(page.getByRole("heading", { name: "基准价" })).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
}

function table(page: Page): Locator {
  return page.getByRole("table", { name: "基准价" });
}

function row(page: Page, name: string): Locator {
  return table(page)
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name, exact: true }) });
}

async function openModel(page: Page, name: string): Promise<Locator> {
  await row(page, name).getByRole("button", { name, exact: true }).click();
  const panel = page.getByRole("dialog", { name: `${name} 的基准价` });
  await expect(panel).toBeVisible();
  return panel;
}

test("从基准价打开「导入审核」会切换当前分区", async ({ page }) => {
  // Named so it sorts after the order test's fixture: it leaves an unpriced
  // model behind, and that test names the row it expects first.
  await newModel("zz-nav-base", "gpt-5.4-nano");
  await openBaselines(page);
  const switchTo = (name: string) =>
    page.getByRole("group", { name: "价格分区" }).getByRole("button", { name, exact: true });
  await expect(switchTo("基准价")).toHaveAttribute("aria-pressed", "true");

  // The baseline page's own link; 「导入审核」 is also the section switch's label now.
  await page.getByRole("button", { name: "去「导入审核」确认第三方数据", exact: true }).click();

  // The section is registered, so the link lands on it, not on the first section.
  await expect(switchTo("基准价")).toHaveAttribute("aria-pressed", "false");
  await expect(switchTo("导入审核")).toHaveAttribute("aria-pressed", "true");
  await expect(page.getByRole("table", { name: "基准价" })).toHaveCount(0);
});

test("未定价的模型排在最前，并标为未定价", async ({ page }) => {
  // Names chosen so that the alphabet says the opposite of the order under test.
  const later = await newModel("a0-base", "gpt-5.4-mini");
  const first = await newModel("z9-base", "gpt-5.4-nano");
  await openBaselines(page);

  const states = await table(page)
    .locator("tbody tr")
    .evaluateAll((trs) => trs.map((tr) => tr.getAttribute("data-state")));
  expect(states[0]).toBe("unpriced");
  expect(states).toEqual([...states].sort((a, b) => (a === b ? 0 : a === "unpriced" ? -1 : 1)));
  await expect(table(page).locator("tbody tr").first()).toContainText("未定价");
  await expect(row(page, later.name)).toHaveAttribute("data-state", "unpriced");
  await expect(row(page, later.name)).toContainText("未定价");

  // Price the one the alphabet puts first, through the page: it moves after.
  const panel = await openModel(page, later.name);
  await panel.getByLabel("方式").selectOption("catalogue");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  await expect(row(page, later.name)).toHaveAttribute("data-state", "priced");
  await expect(row(page, later.name)).not.toContainText("未定价");
  await expect(table(page).locator("tbody tr").first()).toHaveAttribute("data-state", "unpriced");
  await expect(table(page).locator("tbody tr").first()).toContainText(first.name);
});

test("从官方目录设置基准价，显示官方目录来源、来源页链接与生效时间", async ({ page }) => {
  const model = await newModel("cat-base", "gpt-5.4");
  // The expected rates come from the catalogue the control plane serves.
  const catalogue = (await stack.admin.get("/admin/pricing/catalogue")) as {
    items: { key: string; source_url: string | null; as_of: string | null; card: Record<string, { base: Record<string, string> }> }[];
  };
  const entry = catalogue.items.find((e) => e.key === "openai/gpt-5.4");
  expect(entry, "the built-in catalogue prices openai/gpt-5.4").toBeTruthy();
  const input = entry!.card.default!.base.input!;
  const output = entry!.card.default!.base.output!;

  await openBaselines(page);
  const started = Date.now();
  const panel = await openModel(page, model.name);
  await panel.getByLabel("方式").selectOption("catalogue");
  // The model's own entry, preselected from its provider and model name.
  await expect(panel.getByLabel("目录条目")).toHaveValue("openai/gpt-5.4");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  const priced = row(page, model.name);
  await expect(priced).toHaveAttribute("data-state", "priced");
  await expect(priced).toContainText(`$${input} / 百万 tokens`);
  await expect(priced).toContainText(`$${output} / 百万 tokens`);

  // Provenance: the catalogue, the page it was read from, and the reading date.
  await expect(priced).toContainText("官方目录");
  const link = priced.getByRole("link", { name: "来源页", exact: true });
  await expect(link).toHaveAttribute("href", entry!.source_url!);
  await expect(priced).toContainText(entry!.as_of!);
  // 立即生效: the stored baseline's start, to the second.
  const listed = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  const stored = listed.items.find((i: Json) => i.model_id === model.id);
  const shown = new Date(stored.current.effective_from)
    .toISOString()
    .slice(0, 19)
    .replace("T", " ");
  await expect(priced).toContainText(`${shown} UTC`);
  expect(Date.parse(stored.current.effective_from)).toBeGreaterThanOrEqual(started - 1_000);
});

test("手动录入必须填写说明；保存后显示手动录入与填写人", async ({ page }) => {
  const model = await newModel("man-base", "gpt-5.4-nano");
  const note = "按官方价格页抄录 2026-11-08";
  await openBaselines(page);
  const panel = await openModel(page, model.name);

  await panel.getByLabel("方式").selectOption("manual");
  await panel.getByLabel("输入", { exact: true }).fill("3");
  await panel.getByLabel("输出", { exact: true }).fill("6");
  await panel.getByRole("button", { name: "保存" }).click();

  // Refused without a note, and nothing was written.
  await expect(panel.getByText("手动录入需要填写说明")).toBeVisible();
  const fresh = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  expect(fresh.items.find((i: Json) => i.model_id === model.id).current).toBeNull();

  await panel.getByLabel("说明", { exact: true }).fill(note);
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  const priced = row(page, model.name);
  await expect(priced).toHaveAttribute("data-state", "priced");
  await expect(priced).toContainText("$3 / 百万 tokens");
  await expect(priced).toContainText("$6 / 百万 tokens");
  await expect(priced).toContainText("手动录入");
  await expect(priced).toContainText(note);
  await expect(priced).toContainText(stack.operator.email);
});

test("预约的基准价显示在待生效中", async ({ page }) => {
  const model = await newModel("sch-base", "gpt-5.4-nano");
  const note = "预约的价";
  await openBaselines(page);
  const panel = await openModel(page, model.name);

  await panel.getByLabel("方式").selectOption("manual");
  await panel.getByLabel("输入", { exact: true }).fill("5");
  await panel.getByLabel("输出", { exact: true }).fill("10");
  await panel.getByLabel("说明", { exact: true }).fill(note);
  await panel.getByLabel("立即生效").uncheck();
  // Three days ahead: far enough that no clock skew can make it "now".
  const start = new Date(Date.now() + 3 * 86_400_000);
  const typed = start.toISOString().slice(0, 16).replace("T", " ");
  await panel.getByLabel("生效时间（UTC）").fill(typed);
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  // Nothing is in force yet; the baseline waits under 待生效.
  const priced = row(page, model.name);
  await expect(priced).toHaveAttribute("data-state", "unpriced");
  await expect(priced).toContainText("未定价");
  await expect(priced.locator("td").nth(5)).toHaveText("1");
  await expect(priced).toContainText("待生效");
  await expect(priced).toContainText(`${typed}:00 UTC`);

  const stored = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  const listed = stored.items.find((i: Json) => i.model_id === model.id);
  expect(listed.current).toBeNull();
  expect(listed.upcoming).toHaveLength(1);
  expect(listed.upcoming[0].source.note).toBe(note);
});

test("只读运维看不到编辑入口", async ({ page }) => {
  const model = await newModel("ro-base", "gpt-5.4-nano");
  const viewer = { email: `viewer-base-${randomUUID().slice(0, 6)}@e2e.local`, password: "viewer-base-pass-7d2" };
  await stack.admin.post("/admin/operators", { ...viewer, display_name: "只读", role: "viewer" });
  await openBaselines(page, viewer);

  // The page still lists what it holds, the model is there, and its detail
  // opens: read-only means no editor inside it, not no detail.
  await expect(row(page, model.name)).toBeVisible();
  const panel = await openModel(page, model.name);
  await expect(panel.getByRole("heading", { name: "现行与待生效" })).toBeVisible();
  await expect(panel.getByLabel(/^方式/).and(page.locator("select"))).toHaveCount(0);
  await expect(panel.getByRole("button", { name: "保存" })).toHaveCount(0);
  await expect(panel.getByRole("form", { name: "新基准价" })).toHaveCount(0);
});
