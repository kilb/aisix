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
  const shown = new Date(Date.parse(stored.current.effective_from) + 8 * 3_600_000)
    .toISOString()
    .slice(0, 19)
    .replace("T", " ");
  await expect(priced).toContainText(`${shown} 北京时间`);
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
  const typed = new Date(start.getTime() + 8 * 3_600_000).toISOString().slice(0, 16).replace("T", " ");
  await panel.getByLabel("生效时间（北京时间）").fill(typed);
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  // Nothing is in force yet; the baseline waits under 待生效.
  const priced = row(page, model.name);
  await expect(priced).toHaveAttribute("data-state", "unpriced");
  await expect(priced).toContainText("未定价");
  await expect(priced.locator("td").nth(5)).toHaveText("1");
  await expect(priced).toContainText("待生效");
  await expect(priced).toContainText(`${typed}:00 北京时间`);

  const stored = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  const listed = stored.items.find((i: Json) => i.model_id === model.id);
  expect(listed.current).toBeNull();
  expect(listed.upcoming).toHaveLength(1);
  expect(new Date(listed.upcoming[0].effective_from).toISOString()).toBe(new Date(`${typed.replace(" ", "T")}:00+08:00`).toISOString());
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

test("面板列出每个分组的人民币售价：基准价乘以该分组现行的换算率", async ({ page }) => {
  // A group at 1.2 CNY per USD, beside the first-boot `shared` group at 1.
  const group = `grp-mx-${randomUUID().slice(0, 6)}`;
  await stack.admin.post("/admin/provider-groups", {
    name: group,
    display_name: `换算 1.2 分组 ${group.slice(-4)}`,
    description: "e2e 矩阵",
    default_cny_per_usd: "1.2",
  });
  const model = await newModel("mx-base", "gpt-5.4-nano");

  await openBaselines(page);
  const panel = await openModel(page, model.name);
  await panel.getByLabel("方式").selectOption("manual");
  await panel.getByLabel("输入", { exact: true }).fill("3");
  await panel.getByLabel("输出", { exact: true }).fill("6");
  await panel.getByLabel("说明", { exact: true }).fill("矩阵价");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  // The scenario's own arithmetic: 3 USD per million at 1.2 is ¥3.60, and 6 is
  // ¥7.20. The first-boot `shared` group (shown as 通用分组) has no rate in
  // force in this world, so its row says which one is missing rather than
  // showing a figure.
  const matrix = panel.getByRole("table", { name: "各分组售价" });
  const rated = matrix.getByRole("row").filter({ hasText: "换算 1.2 分组" });
  await expect(rated).toContainText("¥3.60");
  await expect(rated).toContainText("¥7.20");
  // By its whole cell, so no other row whose text contains it matches.
  const shared = matrix.getByRole("row").filter({ has: page.getByRole("cell", { name: "通用分组", exact: true }) });
  await expect(shared).toContainText("分组未设换算率");
});

/** Fills each `[label, value]` in `group`, by its exact label. */
async function fill(group: Locator, values: [string, string][]): Promise<void> {
  for (const [label, value] of values) await group.getByLabel(label, { exact: true }).fill(value);
}

test("手动录入可以填写多个档位与长上下文档，联网搜索按每千次填写，保存后从接口读回同一张卡", async ({ page }) => {
  const model = await newModel("tier-base", "gpt-5.4-nano");
  await openBaselines(page);
  const panel = await openModel(page, model.name);
  await panel.getByLabel("方式").selectOption("manual");

  // The standard tier: its base, and a band above 200,000 input tokens. A
  // web search is typed as the provider publishes it, $10 per 1,000.
  await fill(panel.getByRole("group", { name: "标准档 基础价", exact: true }), [
    ["输入", "3"],
    ["输出", "15"],
    ["缓存读", "0.3"],
    ["联网搜索（每千次）", "10"],
  ]);
  await panel.getByRole("group", { name: "标准档", exact: true }).getByRole("button", { name: "添加长上下文档" }).click();
  await fill(panel.getByRole("group", { name: "标准档 长上下文档 1", exact: true }), [
    ["输入超过（tokens）", "200000"],
    ["输入", "6"],
    ["输出", "22.5"],
    ["缓存读", "0.6"],
    ["联网搜索（每千次）", "10"],
  ]);

  // A second tier, batch, with its band at the standard tier's threshold.
  await panel.getByLabel("要添加的档位").selectOption("batch");
  await panel.getByRole("button", { name: "添加档位" }).click();
  const batchBand = panel.getByRole("group", { name: "批量档 长上下文档 1", exact: true });
  await expect(batchBand.getByLabel("输入超过（tokens）")).toHaveValue("200000");
  await fill(panel.getByRole("group", { name: "批量档 基础价", exact: true }), [
    ["输入", "1.5"],
    ["输出", "7.5"],
    ["缓存读", "0.15"],
    ["联网搜索（每千次）", "10"],
  ]);
  await fill(batchBand, [
    ["输入", "3"],
    ["输出", "11.25"],
    ["缓存读", "0.3"],
    ["联网搜索（每千次）", "10"],
  ]);
  await panel.getByLabel("说明", { exact: true }).fill("两档位与长上下文档");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();

  // What the control plane stores: every rate as typed, per million units,
  // so a search's $10 per 1,000 is 10000 per million searches.
  const search = "10000";
  const stored = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  const listed = stored.items.find((i: Json) => i.model_id === model.id);
  expect(listed.current.card).toEqual({
    default: {
      base: { input: "3", output: "15", cache_read: "0.3", web_search: search },
      long_context: [{ above_input_tokens: 200000, rates: { input: "6", output: "22.5", cache_read: "0.6", web_search: search } }],
    },
    batch: {
      base: { input: "1.5", output: "7.5", cache_read: "0.15", web_search: search },
      long_context: [{ above_input_tokens: 200000, rates: { input: "3", output: "11.25", cache_read: "0.3", web_search: search } }],
    },
  });

  // Shown back per thousand searches, never as 10000 a search.
  const now = panel.getByRole("region", { name: "现行基准价" });
  await expect(now).toContainText("联网搜索 $10 / 千次");
  await expect(now).not.toContainText("$10000");
  // Read again on a fresh form, the whole card is filled in, in the unit it was typed in.
  await page.reload();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("button", { name: "基准价", exact: true }).click();
  const again = await openModel(page, model.name);
  await again.getByLabel("方式").selectOption("manual");
  await expect(again.getByRole("group", { name: "批量档 长上下文档 1", exact: true }).getByLabel("输出", { exact: true })).toHaveValue("11.25");
  await expect(again.getByRole("group", { name: "标准档 基础价", exact: true }).getByLabel("联网搜索（每千次）")).toHaveValue("10");
});

test("档位的长上下文门槛与标准档不同：在该档位旁报错，不写入基准价", async ({ page }) => {
  const model = await newModel("tier-bad", "gpt-5.4-nano");
  await openBaselines(page);
  const panel = await openModel(page, model.name);
  await panel.getByLabel("方式").selectOption("manual");

  await fill(panel.getByRole("group", { name: "标准档 基础价", exact: true }), [
    ["输入", "1"],
    ["输出", "2"],
  ]);
  await panel.getByRole("group", { name: "标准档", exact: true }).getByRole("button", { name: "添加长上下文档" }).click();
  await fill(panel.getByRole("group", { name: "标准档 长上下文档 1", exact: true }), [
    ["输入超过（tokens）", "200000"],
    ["输入", "2"],
    ["输出", "4"],
  ]);
  await panel.getByLabel("要添加的档位").selectOption("batch");
  await panel.getByRole("button", { name: "添加档位" }).click();
  await fill(panel.getByRole("group", { name: "批量档 基础价", exact: true }), [
    ["输入", "0.5"],
    ["输出", "1"],
  ]);
  // The batch band at another threshold than the standard tier's.
  await fill(panel.getByRole("group", { name: "批量档 长上下文档 1", exact: true }), [
    ["输入超过（tokens）", "100000"],
    ["输入", "1"],
    ["输出", "2"],
  ]);
  await panel.getByLabel("说明", { exact: true }).fill("门槛不一致");
  await panel.getByRole("button", { name: "保存" }).click();

  // Refused beside the batch tier's bands, not above the whole card.
  const batch = panel.getByRole("group", { name: "批量档", exact: true });
  await expect(batch.getByRole("alert")).toContainText("长上下文门槛必须与标准档的完全相同");
  await expect(panel.getByRole("group", { name: "标准档", exact: true }).getByRole("alert")).toHaveCount(0);
  const stored = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  const listed = stored.items.find((i: Json) => i.model_id === model.id);
  expect(listed.current).toBeNull();
  expect(listed.upcoming).toHaveLength(0);

  // Without the batch tier, a threshold out of order is refused at its own field.
  await batch.getByRole("button", { name: "移除档位" }).click();
  await expect(panel.getByRole("group", { name: "批量档", exact: true })).toHaveCount(0);
  await panel.getByRole("group", { name: "标准档", exact: true }).getByRole("button", { name: "添加长上下文档" }).click();
  await fill(panel.getByRole("group", { name: "标准档 长上下文档 2", exact: true }), [
    ["输入超过（tokens）", "100000"],
    ["输入", "3"],
    ["输出", "5"],
  ]);
  await panel.getByRole("button", { name: "保存" }).click();
  const threshold = panel.getByRole("group", { name: "标准档 长上下文档 2", exact: true }).getByLabel("输入超过（tokens）");
  await expect(threshold).toHaveAttribute("aria-invalid", "true");
  await expect(panel.getByRole("group", { name: "标准档 长上下文档 2", exact: true })).toContainText("长上下文门槛必须逐个递增");
  const after = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  expect(after.items.find((i: Json) => i.model_id === model.id).current).toBeNull();
});

test("联网搜索的价格不是每千次的金额时，在字段旁提示，不提交", async ({ page }) => {
  const model = await newModel("search-bad", "gpt-5.4-nano");
  await openBaselines(page);
  const panel = await openModel(page, model.name);
  await panel.getByLabel("方式").selectOption("manual");
  await fill(panel.getByRole("group", { name: "标准档 基础价", exact: true }), [
    ["输入", "1"],
    ["输出", "2"],
    // Ten decimals per thousand: more than a card holds per million.
    ["联网搜索（每千次）", "0.0000000001"],
  ]);
  await panel.getByLabel("说明", { exact: true }).fill("x");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(panel.getByText("按每千次的美元数填写，最多 9 位小数")).toBeVisible();
  const stored = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  expect(stored.items.find((i: Json) => i.model_id === model.id).current).toBeNull();
});

test("JSON 视图里每个价格都按每百万单位：提示不说按每千次填写，按提示写的联网搜索价是每千次 10 美元", async ({ page }) => {
  const model = await newModel("search-json", "gpt-5.4-nano");
  await openBaselines(page);
  const panel = await openModel(page, model.name);
  await panel.getByLabel("方式").selectOption("manual");
  const card = panel.getByRole("group", { name: "价格卡（美元）" });
  // The fields take a search per thousand.
  await expect(card.getByText("联网搜索按每千次填写", { exact: false })).toBeVisible();

  // The JSON view holds the card as stored: the per-thousand hint is gone,
  // and the one shown gives the per-million figure for the same price.
  await card.getByRole("button", { name: "编辑完整价格卡（JSON）" }).click();
  await expect(card.getByText("联网搜索按每千次填写", { exact: false })).toHaveCount(0);
  await expect(card.getByText("JSON 里的每个价格都按每百万单位填写", { exact: false })).toContainText('"10000"');

  // Followed, the hint stores $10 per 1,000 searches, read back per thousand.
  await card.getByLabel("资源文档").fill(JSON.stringify({ default: { base: { input: "1", output: "2", web_search: "10000" } } }));
  await panel.getByLabel("说明", { exact: true }).fill("JSON 录入联网搜索");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();
  await expect(panel.getByRole("region", { name: "现行基准价" })).toContainText("联网搜索 $10 / 千次");
  const after = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  expect(after.items.find((i: Json) => i.model_id === model.id).current.card).toEqual({
    default: { base: { input: "1", output: "2", web_search: "10000" } },
  });
});

test("JSON 里写了字段视图读不了的价格卡：切回字段时留在 JSON 视图并指出位置，改正后可继续编辑与保存", async ({ page }) => {
  const model = await newModel("shape-base", "gpt-5.4-nano");
  await openBaselines(page);
  const panel = await openModel(page, model.name);
  await panel.getByLabel("方式").selectOption("manual");
  const card = panel.getByRole("group", { name: "价格卡（美元）" });
  const toggle = card.getByRole("button", { name: "编辑完整价格卡（JSON）" });

  // Top-level legal, inner band null: the document the review found blanked
  // the whole Dashboard when the field view read `band.rates` of it.
  const broken = JSON.stringify({ default: { base: { input: "1" }, long_context: [null] } });
  await toggle.click();
  await card.getByLabel("资源文档").fill(broken);
  await toggle.click();

  // The page is still here, the text is still there, and the problem is named
  // at the band that is not one.
  await expect(panel).toBeVisible();
  await expect(card.getByLabel("资源文档")).toHaveValue(broken);
  await expect(card.getByRole("alert")).toContainText("/default/long_context/0");
  // An unreadable card is not saved as it stands.
  await panel.getByLabel("说明", { exact: true }).fill("结构错误");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(card.getByRole("list", { name: "校验问题" })).toContainText("价格卡中有无法读取的内容，修改尚未生效；请先改正");
  const none = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  expect(none.items.find((i: Json) => i.model_id === model.id).current).toBeNull();

  // Corrected, the field view opens on the same card, and it saves.
  const fixed = {
    default: {
      base: { input: "1", output: "3" },
      // A band carries every optional rate its own tier's base carries.
      long_context: [{ above_input_tokens: 200_000, rates: { input: "2", output: "4" } }],
    },
  };
  await card.getByLabel("资源文档").fill(JSON.stringify(fixed));
  // The shape complaint went with the document it was about. (The save's own
  // refusal stays listed until the next save, as every refusal on this form
  // does.)
  await expect(card.getByText("/default/long_context/0")).toHaveCount(0);
  await toggle.click();
  await expect(card.getByLabel("资源文档")).toHaveCount(0);
  const band = card.getByRole("group", { name: "标准档 长上下文档 1", exact: true });
  await expect(band.getByLabel("输入超过（tokens）", { exact: true })).toHaveValue("200000");
  await expect(band.getByLabel("输入", { exact: true })).toHaveValue("2");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();
  const stored = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  expect(stored.items.find((i: Json) => i.model_id === model.id).current.card).toEqual(fixed);
});

for (const { label, broken, path } of [
  {
    label: "基础价",
    broken: { default: { base: { web_search: { toString: null } } } },
    path: "/default/base/web_search",
  },
  {
    label: "长上下文价格",
    broken: { default: { long_context: [{ above_input_tokens: 200_000, rates: { web_search: { toString: null } } }] } },
    path: "/default/long_context/0/rates/web_search",
  },
  {
    label: "长上下文阈值",
    broken: { default: { long_context: [{ above_input_tokens: { toString: null }, rates: { input: "2" } }] } },
    path: "/default/long_context/0/above_input_tokens",
  },
]) {
  test(`JSON 中${label}的对象值保留原文和局部错误，纠正后可编辑并保存`, async ({ page }, testInfo) => {
    const pageErrors: string[] = [];
    const consoleErrors: string[] = [];
    page.on("pageerror", (error) => pageErrors.push(error.message));
    page.on("console", (message) => {
      if (message.type() === "error") consoleErrors.push(message.text());
    });
    const model = await newModel("rate-shape-base", "gpt-5.4-nano");
    await openBaselines(page);
    const panel = await openModel(page, model.name);
    await panel.getByLabel("方式").selectOption("manual");
    const card = panel.getByRole("group", { name: "价格卡（美元）" });
    const toggle = card.getByRole("button", { name: "编辑完整价格卡（JSON）" });

    // 同一页面的合法价格先能回到字段，确保错误场景确实经过切视图。
    await toggle.click();
    await card.getByLabel("资源文档").fill(JSON.stringify({ default: { base: { input: "1", output: "3", web_search: "10000" } } }));
    await toggle.click();
    await expect(card.getByRole("group", { name: "标准档 基础价", exact: true }).getByLabel("联网搜索（每千次）", { exact: true })).toHaveValue("10");

    const original = JSON.stringify(broken);
    await toggle.click();
    await card.getByLabel("资源文档").fill(original);
    await toggle.click();
    await expect(panel, "结构错误不能卸载整个运维台").toBeVisible();
    await expect(page.getByRole("heading", { name: "基准价", exact: true })).toBeVisible();
    await expect(card.getByLabel("资源文档"), "原 JSON 应保留以便纠正").toHaveValue(original);
    await expect(card.getByRole("alert")).toContainText(path);
    await expect(page.locator("vite-error-overlay")).toHaveCount(0);
    expect(pageErrors, "错误应在编辑器局部显示，不能产生渲染异常").toEqual([]);
    expect(consoleErrors).toEqual([]);
    await testInfo.attach("保留 JSON 与局部错误", { body: await page.screenshot(), contentType: "image/png" });

    await panel.getByLabel("说明", { exact: true }).fill("纠正价格结构");
    await panel.getByRole("button", { name: "保存" }).click();
    await expect(card.getByRole("list", { name: "校验问题" })).toContainText("价格卡中有无法读取的内容，修改尚未生效；请先改正");
    const refused = (await stack.admin.get("/admin/pricing/baselines")) as Json;
    expect(refused.items.find((item: Json) => item.model_id === model.id).current).toBeNull();

    await card.getByLabel("资源文档").fill(JSON.stringify({
      default: {
        base: { input: "1", output: "3", web_search: "10000" },
        long_context: [{ above_input_tokens: 200_000, rates: { input: "2", output: "4", web_search: "20000" } }],
      },
    }));
    // 上次提交的拒绝仍列在表单中；当前 JSON 的结构错误已随更正消失。
    await expect(card.getByRole("alert").filter({ hasText: path })).toHaveCount(0);
    await toggle.click();
    await expect(card.getByLabel("资源文档")).toHaveCount(0);
    const base = card.getByRole("group", { name: "标准档 基础价", exact: true });
    await expect(base.getByLabel("输入", { exact: true })).toHaveValue("1");
    const band = card.getByRole("group", { name: "标准档 长上下文档 1", exact: true });
    await expect(band.getByLabel("输入超过（tokens）", { exact: true })).toHaveValue("200000");
    await expect(band.getByLabel("联网搜索（每千次）", { exact: true })).toHaveValue("20");
    await base.getByLabel("输出", { exact: true }).fill("5");
    await panel.getByRole("button", { name: "保存" }).click();
    await expect(page.getByText("已保存")).toBeVisible();
    const stored = (await stack.admin.get("/admin/pricing/baselines")) as Json;
    expect(stored.items.find((item: Json) => item.model_id === model.id).current.card).toEqual({
      default: {
        base: { input: "1", output: "5", web_search: "10000" },
        long_context: [{ above_input_tokens: 200_000, rates: { input: "2", output: "4", web_search: "20000" } }],
      },
    });
    expect(pageErrors).toEqual([]);
    expect(consoleErrors).toEqual([]);
    await testInfo.attach("纠正并保存后", { body: await page.screenshot(), contentType: "image/png" });
  });
}

test("生效时间是日历上不存在的日子时报错且不写入；闰年的 2 月 29 日照常写入", async ({ page }) => {
  const model = await newModel("cal-base", "gpt-5.4-nano");
  await openBaselines(page);
  const panel = await openModel(page, model.name);
  await panel.getByLabel("方式").selectOption("manual");
  await panel.getByLabel("输入", { exact: true }).fill("5");
  await panel.getByLabel("输出", { exact: true }).fill("10");
  await panel.getByLabel("说明", { exact: true }).fill("日历");
  await panel.getByLabel("立即生效").uncheck();

  // 2037 is no leap year, April has 30 days, and a day has no 24:00. Each was
  // once read as a valid instant of another day and scheduled there.
  for (const typed of ["2037-02-29 00:00", "2037-04-31 10:00", "2037-01-01 24:00"]) {
    await panel.getByLabel("生效时间（北京时间）").fill(typed);
    await panel.getByRole("button", { name: "保存" }).click();
    await expect(panel.getByLabel("生效时间（北京时间）")).toHaveAttribute("aria-invalid", "true");
    await expect(panel.getByText("生效时间应写成 年-月-日 时:分，按北京时间计")).toBeVisible();
  }
  const refused = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  const none = refused.items.find((i: Json) => i.model_id === model.id);
  expect(none.current).toBeNull();
  expect(none.upcoming).toHaveLength(0);

  await panel.getByLabel("生效时间（北京时间）").fill("2036-02-29 00:00");
  await panel.getByRole("button", { name: "保存" }).click();
  await expect(page.getByText("已保存")).toBeVisible();
  const stored = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  const listed = stored.items.find((i: Json) => i.model_id === model.id);
  expect(listed.upcoming).toHaveLength(1);
  expect(new Date(listed.upcoming[0].effective_from).toISOString()).toBe("2036-02-28T16:00:00.000Z");
});
