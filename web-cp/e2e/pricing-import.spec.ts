import { randomUUID } from "node:crypto";
import { join } from "node:path";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Stack } from "./stack";

/**
 * 价格 · 导入审核 against the real control plane through the built `dist/`.
 * Every test uploads the committed snapshot of the public dataset through the
 * page itself, so the whole journey is the operator's: read the file, post it,
 * review what it proposes, and decide.
 */

let stack: Stack;

/** The committed snapshot: a real dataset file, not a hand-made fixture. */
const SNAPSHOT = join(process.cwd(), "..", "tests", "e2e", "fixtures", "pricing-import", "genai-prices-v2-7de757d.json");
/** The revision its file name carries. */
const REVISION = "genai-prices-v2-7de757d";
/** The credit the page owes the dataset's authors. */
const CREDIT = "价格数据来自公开数据集 pydantic/genai-prices（MIT 许可），属第三方数据；确认前请对照官方价格页核对。";

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

/** A live direct model of `provider`/`modelName`, which the upload then suggests. */
async function newModel(prefix: string, provider: string, modelName: string): Promise<{ id: string; name: string }> {
  const account = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: `acct-${prefix}-${randomUUID().slice(0, 4)}`,
      provider: "openai",
      api_key: `sk-acct-${prefix}-${randomUUID().slice(0, 8)}`,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups: ["shared"],
    },
  });
  const name = `${prefix}-${randomUUID().slice(0, 8)}`;
  const model = await stack.admin.post("/admin/resources/models", {
    doc: { display_name: name, provider, model_name: modelName, provider_key_id: account.id },
  });
  return { id: model.id, name };
}

async function login(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

async function openImports(page: Page): Promise<void> {
  await login(page);
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("button", { name: "导入审核", exact: true }).click();
  await expect(page.getByRole("heading", { name: "导入审核" })).toBeVisible();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
}

/** Uploads the snapshot through the page's own form and waits for its proposals. */
async function upload(page: Page): Promise<void> {
  await page.getByLabel("数据集文件").setInputFiles(SNAPSHOT);
  await page.getByLabel("数据集版本").fill(REVISION);
  await page.getByRole("button", { name: "上传", exact: true }).click();
  await expect(table(page)).toBeVisible();
  await expect(page.getByText(CREDIT)).toBeVisible();
}

function table(page: Page): Locator {
  return page.getByRole("table", { name: "提议" });
}

/** Narrows the list to the entries whose provider and model match `text`. */
async function only(page: Page, text: string): Promise<void> {
  await page.getByLabel("筛选模型").fill(text);
}

function row(page: Page, entry: string): Locator {
  return table(page).getByRole("row").filter({ has: page.getByRole("button", { name: entry, exact: true }) });
}

/** Opens one proposal's detail, which is where it is decided. */
async function openProposal(page: Page, entry: string): Promise<Locator> {
  await only(page, entry);
  await row(page, entry).getByRole("button", { name: entry, exact: true }).click();
  const panel = page.getByRole("dialog", { name: `${entry} 的提议` });
  await expect(panel).toBeVisible();
  return panel;
}

test("上传数据集快照后列出提议，标注第三方数据并致谢数据集与许可", async ({ page }) => {
  await openImports(page);
  await upload(page);

  // The dataset and its licence, worded as the authors ask.
  await expect(page.getByText(CREDIT)).toBeVisible();
  // The batch the upload made is the one on screen, by the revision typed.
  await expect(page.getByLabel("上传批次")).toContainText(REVISION);
  // Its proposals, marked third-party.
  await only(page, "anthropic/claude-opus-5-5");
  const priced = row(page, "anthropic/claude-opus-5-5");
  await expect(priced).toBeVisible();
  await expect(priced).toContainText("第三方数据");
  // The rates are the dataset's own text, exactly.
  await expect(priced).toContainText("$4 / 百万 tokens");
  await expect(priced).toContainText("$20 / 百万 tokens");
  // The dataset's own note about the entry is shown, so an operator can judge it.
  await expect(priced).toContainText("Flat pricing across full 1M context window");
});

test("未勾选核对声明时确认按钮不可用", async ({ page }) => {
  const model = await newModel("att", "anthropic", "claude-opus-5-5");
  await openImports(page);
  await upload(page);

  const panel = await openProposal(page, "anthropic/claude-opus-5-5");
  const confirm = panel.getByRole("button", { name: "确认", exact: true });
  // Preselected to the model the upload matched, and ready but for the attestation.
  await expect(panel.getByLabel("模型")).toHaveValue(model.id);
  await expect(confirm).toBeDisabled();

  await panel.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await expect(confirm).toBeEnabled();
});

test("确认后在基准价页显示第三方数据来源与确认人", async ({ page }) => {
  // Its own entry, so the upload suggests this model and no other test's.
  const model = await newModel("conf", "anthropic", "claude-haiku-5-5");
  await openImports(page);
  await upload(page);

  const panel = await openProposal(page, "anthropic/claude-haiku-5-5");
  await panel.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await panel.getByRole("button", { name: "确认", exact: true }).click();
  await expect(panel.getByText("已确认").first()).toBeVisible();

  // The baseline page shows the model priced from the third party, by whom.
  await page.getByRole("group", { name: "价格分区" }).getByRole("button", { name: "基准价", exact: true }).click();
  const baseline = page
    .getByRole("table", { name: "基准价" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: model.name, exact: true }) });
  await expect(baseline).toHaveAttribute("data-state", "priced");
  await expect(baseline).toContainText("第三方数据");
  await expect(baseline).toContainText(REVISION);
  await expect(baseline).toContainText(stack.operator.email);
});

test("带标记的提议不能确认，只提供手动录入入口", async ({ page }) => {
  await openImports(page);
  await upload(page);

  // Input-only: the dataset names no output price, so no card is made.
  const panel = await openProposal(page, "cloudflare/@cf/cloudflare/clef");
  await expect(panel).toContainText("价格不足以构成价目卡");
  // Its note says why: an operator reads it to price the entry by hand.
  await expect(panel).toContainText("Input only, $0.24 per million tokens");
  await expect(panel.getByRole("button", { name: "确认", exact: true })).toHaveCount(0);
  await expect(panel.getByRole("checkbox", { name: "已对照官方价格页核对" })).toHaveCount(0);

  // The way on is the baseline page's manual entry.
  await panel.getByRole("button", { name: "去「基准价」手动录入" }).click();
  await expect(page.getByRole("group", { name: "价格分区" }).getByRole("button", { name: "基准价", exact: true })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
});

test("驳回需要填写原因，驳回后不再待处理", async ({ page }) => {
  await openImports(page);
  await upload(page);
  const entry = "openrouter/alpindale/goliath-120b";

  const panel = await openProposal(page, entry);
  await panel.getByRole("button", { name: "驳回", exact: true }).click();
  await expect(panel.getByText("请填写拒绝原因")).toBeVisible();

  const reason = "数据集把批量价写成了标准价，按官方价格页驳回";
  await panel.getByLabel("拒绝原因").fill(reason);
  await panel.getByRole("button", { name: "驳回", exact: true }).click();
  await expect(panel.getByText("已驳回")).toBeVisible();

  // The row is decided, not pending, and carries the reason.
  await expect(row(page, entry)).toContainText("已驳回");
  await expect(row(page, entry)).not.toContainText("待处理");
  await expect(row(page, entry)).toContainText(reason);
});

test("从基准价页进入导入审核，落在本页", async ({ page }) => {
  // A model, so the baseline page shows its table rather than the empty state.
  await newModel("nav-imp", "openai", "gpt-5.4-nano");
  await login(page);
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("group", { name: "价格分区" }).getByRole("button", { name: "基准价", exact: true }).click();
  await expect(page.getByRole("table", { name: "基准价" })).toBeVisible();

  // The baseline page's own link, not the section switch.
  await page.getByRole("button", { name: "去「导入审核」确认第三方数据", exact: true }).click();

  const switchTo = (name: string) =>
    page.getByRole("group", { name: "价格分区" }).getByRole("button", { name, exact: true });
  await expect(switchTo("导入审核")).toHaveAttribute("aria-pressed", "true");
  await expect(switchTo("基准价")).toHaveAttribute("aria-pressed", "false");
  await expect(page.getByRole("heading", { name: "导入审核" })).toBeVisible();
  await expect(page.getByText(CREDIT)).toBeVisible();
});

test("金额按价目卡的原文显示，写成浮点的小数不构成价格", async ({ page }) => {
  await openImports(page);
  await upload(page);

  // Four decimal places in the dataset's own text: shown as written, never
  // through a float, which would round them.
  const exact = row(page, "openrouter/alpindale/goliath-120b");
  await only(page, "openrouter/alpindale/goliath-120b");
  await expect(exact).toContainText("$6.5625 / 百万 tokens");
  await expect(exact).toContainText("$9.375 / 百万 tokens");

  // The dataset's float noise is 17 digits long, which is no price at all:
  // it is flagged, and no rounded rate is invented for it.
  const noisy = "huggingface_together/deepseek-ai/DeepSeek-V4-Flash-0731";
  await only(page, noisy);
  await expect(row(page, noisy)).toContainText("价格数字无法读取");
  await expect(row(page, noisy)).not.toContainText("$0.14 / 百万 tokens");
});

test("只读运维看不到上传与决定入口", async ({ page }) => {
  const viewer = { email: `viewer-import-${randomUUID().slice(0, 6)}@e2e.local`, password: "viewer-import-pass-2d7" };
  await stack.admin.post("/admin/operators", { ...viewer, display_name: "只读", role: "viewer" });
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(viewer.email);
  await page.getByLabel("口令", { exact: true }).fill(viewer.password);
  await page.getByRole("button", { name: "登录" }).click();
  await page.locator('[data-tab="pricing"]').click();
  await page.getByRole("button", { name: "导入审核", exact: true }).click();

  await expect(page.getByRole("heading", { name: "导入审核" })).toBeVisible();
  await expect(page.getByLabel("数据集文件")).toHaveCount(0);
  await expect(page.getByRole("button", { name: "上传", exact: true })).toHaveCount(0);
});

/** The proposal of the open batch for `entry`, through the real `/admin`. */
async function proposalOf(entry: string): Promise<{ batch: string; id: string }> {
  const { items } = await stack.admin.get("/admin/pricing/imports");
  const batch = items.find((b: { status: string }) => b.status === "open").id as string;
  const detail = await stack.admin.get(`/admin/pricing/imports/${batch}?all=true`);
  const found = detail.proposals.find(
    (p: { provider: string; model: string; effective_from: string | null }) =>
      `${p.provider}/${p.model}` === entry && p.effective_from === null,
  );
  return { batch, id: found.id as string };
}

test("他人先处理了提议时，确认显示中文提示并刷新该行", async ({ page }) => {
  await newModel("race", "anthropic", "claude-haiku-4-5");
  await openImports(page);
  await upload(page);
  const entry = "anthropic/claude-haiku-4-5";

  const panel = await openProposal(page, entry);
  await panel.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  // Another operator rejects it first, through the real API.
  const { batch, id } = await proposalOf(entry);
  await stack.admin.post(`/admin/pricing/imports/${batch}/proposals/${id}/reject`, { reason: "另一位运维先处理" });

  await panel.getByRole("button", { name: "确认", exact: true }).click();

  await expect(panel.getByRole("alert")).toHaveText("该提议已被处理，请刷新");
  // Read again: the row and the panel now show the other operator's decision.
  await expect(row(page, entry)).toContainText("已驳回");
  await expect(row(page, entry)).toContainText("另一位运维先处理");
  await expect(panel.getByRole("button", { name: "确认", exact: true })).toHaveCount(0);
  // Nothing was written over it: still rejected, and no baseline.
  const after = await stack.admin.get(`/admin/pricing/imports/${batch}?all=true`);
  expect(after.proposals.find((p: { id: string }) => p.id === id).status).toBe("rejected");
});

test("批量确认需要核对声明，并逐条显示结果", async ({ page }) => {
  const a = await newModel("bulk-a", "cohere", "command-r7b");
  const b = await newModel("bulk-b", "deepseek", "deepseek-v3.2");
  await openImports(page);
  await upload(page);

  for (const entry of ["cohere/command-r7b", "deepseek/deepseek-v3.2"]) {
    await only(page, entry);
    // The entry's price in effect now; a dated one of the same entry starts later.
    await row(page, entry).filter({ hasText: "立即生效" }).getByRole("checkbox", { name: `选择 ${entry}` }).check();
  }
  const bulk = page.getByRole("group", { name: "批量确认" });
  const confirm = bulk.getByRole("button", { name: /^确认所选/ });
  await expect(confirm).toHaveText("确认所选（2）");
  await expect(confirm).toBeDisabled();

  await bulk.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await confirm.click();

  const results = page.getByRole("status").filter({ hasText: "已确认 cohere/command-r7b" });
  await expect(results).toContainText("已确认 cohere/command-r7b");
  await expect(results).toContainText("已确认 deepseek/deepseek-v3.2");
  // Each became its model's baseline, from the third party.
  const listed = await stack.admin.get("/admin/pricing/baselines");
  for (const model of [a, b]) {
    const current = listed.items.find((i: { model_id: string }) => i.model_id === model.id).current;
    expect(current?.source_kind).toBe("third_party");
  }
});

test("超过 8 MiB 的文件不上传；控制面拒收过大的请求时显示中文原因", async ({ page }) => {
  await openImports(page);
  const before = (await stack.admin.get("/admin/pricing/imports")).items.length;

  // One byte over: refused here, and nothing reaches the control plane.
  await page.getByLabel("数据集文件").setInputFiles({
    name: "too-big.json",
    mimeType: "application/json",
    buffer: Buffer.alloc((8 << 20) + 1, " "),
  });
  await page.getByLabel("数据集版本").fill("too-big");
  await page.getByRole("button", { name: "上传", exact: true }).click();
  await expect(page.getByText("数据集文件不能超过 8 MiB")).toBeVisible();
  expect((await stack.admin.get("/admin/pricing/imports")).items.length).toBe(before);

  // Exactly the limit: the file passes, but with the request around it the body
  // is over the control plane's limit, and its 413 is said in Chinese.
  await page.getByLabel("数据集文件").setInputFiles({
    name: "at-limit.json",
    mimeType: "application/json",
    buffer: Buffer.alloc(8 << 20, " "),
  });
  await page.getByRole("button", { name: "上传", exact: true }).click();
  await expect(page.getByRole("alert")).toHaveText("数据集文件过大，控制面拒绝接收（上限 8 MiB）");
  expect((await stack.admin.get("/admin/pricing/imports")).items.length).toBe(before);
});

test("与当前基准价相同的提议默认隐藏，勾选后列出", async ({ page }) => {
  const model = await newModel("same", "cohere", "command-r-plus");
  // The very card the dataset gives this entry, so the proposal has nothing to add.
  await stack.admin.post("/admin/pricing/baselines", {
    model_id: model.id,
    effective_from: new Date().toISOString(),
    card: { default: { base: { input: "2.5", output: "10" } } },
    note: "与数据集给出的价格相同",
  });
  await openImports(page);
  // One entry, so the answer under test is the filter's alone: the whole
  // snapshot's 2302 proposals take a read long enough to race a UI assertion.
  await page.getByLabel("数据集文件").setInputFiles({
    name: "one-entry.json",
    mimeType: "application/json",
    buffer: Buffer.from(
      JSON.stringify([{ id: "cohere", models: [{ id: "command-r-plus", prices: { input_mtok: 2.5, output_mtok: 10 } }] }]),
    ),
  });
  await page.getByLabel("数据集版本").fill("one-entry");
  await page.getByRole("button", { name: "上传", exact: true }).click();
  await expect(page.getByText(CREDIT)).toBeVisible();
  const entry = "cohere/command-r-plus";

  await only(page, entry);
  // Hidden by default: there is nothing to confirm.
  await expect(page.getByText("这一批没有待处理的提议")).toBeVisible();

  await page.getByRole("checkbox", { name: "显示与当前基准价相同的提议" }).check();
  await expect(row(page, entry)).toBeVisible();
  await expect(row(page, entry)).toContainText("$2.5 / 百万 tokens");
  await expect(row(page, entry)).toContainText("$10 / 百万 tokens");

  await page.getByRole("checkbox", { name: "显示与当前基准价相同的提议" }).uncheck();
  await expect(row(page, entry)).toHaveCount(0);
});
