import { randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { expect, test, type Locator, type Page } from "@playwright/test";
import { STACK_START_TIMEOUT_MS, startStack, type Json, type Stack } from "./stack";

/**
 * 价格 · 价格库 against the real control plane through the built `dist/`.
 *
 * The stack cannot reach the dataset's published address, so every test
 * fills the library the offline way an operator would: by uploading the
 * dataset file on 导入审核. The file is the committed snapshot of the real
 * dataset, and every price asserted here is read from that file, the input
 * the operator gave, never from the product's answers.
 */

let stack: Stack;

test.beforeAll(async () => {
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack = await startStack();
});

test.afterAll(async () => {
  await stack?.stop();
});

/** The committed snapshot: a real dataset file, not a hand-made fixture. */
const SNAPSHOT = join(process.cwd(), "..", "tests", "e2e", "fixtures", "pricing-import", "genai-prices-v2-7de757d.json");
/** The revision its file name carries. */
const REVISION = "genai-prices-v2-7de757d";

interface SnapshotProvider {
  id: string;
  pricing_urls?: string[];
  models: { id: string; prices: Record<string, number> }[];
}

/** What the snapshot says of `provider`'s `model`: its flat input and output prices, and the provider's first page. */
function snapshot(provider: string, model: string): { input: string; output: string; page: string } {
  const all = JSON.parse(readFileSync(SNAPSHOT, "utf8")) as SnapshotProvider[];
  const p = all.find((x) => x.id === provider);
  const m = p?.models.find((x) => x.id === model);
  if (!p || !m) throw new Error(`no ${provider}/${model} in the snapshot`);
  return { input: String(m.prices.input_mtok), output: String(m.prices.output_mtok), page: p.pricing_urls?.[0] ?? "" };
}

/** A USD rate per million as `$x / 百万 tokens`, the Dashboard's one way of writing it. */
function usd(rate: string): string {
  return `$${rate} / 百万 tokens`;
}

/** `usd × cny` per million as `¥x.xx`: exact, at least two decimals, as every ¥ figure is written. */
function cny(usd: string, rate: string): string {
  const micros = BigInt(Math.round(Number(usd) * 1_000_000));
  const e4 = BigInt(Math.round(Number(rate) * 10_000));
  const scale = 10n ** 10n;
  const value = micros * e4;
  const whole = (value / scale).toString().replace(/\B(?=(\d{3})+$)/g, ",");
  const frac = (value % scale).toString().padStart(10, "0").replace(/0+$/, "").padEnd(2, "0");
  return `¥${whole}.${frac}`;
}

async function login(page: Page): Promise<void> {
  await page.goto(stack.previewUrl);
  await page.getByLabel("邮箱").fill(stack.operator.email);
  await page.getByLabel("口令", { exact: true }).fill(stack.operator.password);
  await page.getByRole("button", { name: "登录" }).click();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
}

async function section(page: Page, name: string): Promise<void> {
  await page.getByRole("group", { name: "价格分区" }).getByRole("button", { name, exact: true }).click();
  await expect(page.locator("main").getByText("载入中…")).toHaveCount(0);
}

/** Fills the library the offline way: the dataset file, uploaded on 导入审核. */
async function uploadDataset(page: Page): Promise<void> {
  await page.locator('[data-tab="pricing"]').click();
  await section(page, "导入审核");
  await page.getByLabel("数据集文件").setInputFiles(SNAPSHOT);
  await page.getByLabel("数据集版本").fill(REVISION);
  await page.getByRole("button", { name: "上传", exact: true }).click();
  await expect(page.getByRole("table", { name: "提议" })).toBeVisible();
}

/** An upstream account of `provider` that serves `groups`. */
async function account(provider: string, groups: string[]): Promise<string> {
  const suffix = randomUUID().slice(0, 8);
  const key = await stack.admin.post("/admin/resources/provider_keys", {
    doc: {
      display_name: `acct-lib-${suffix}`,
      provider,
      api_key: `sk-acct-lib-${suffix}-upstream`,
      api_base: `${stack.mockUpstreamUrl}/v1`,
      groups,
    },
  });
  return key.id as string;
}

/** A public model `name` served by `accountId`, the live one of that name when one already is. */
async function publicModel(name: string, accountId: string): Promise<string> {
  const listed: Json = await stack.admin.get("/admin/public-models");
  const existing = (listed.items as Json[]).find((m) => m.name === name && !m.retired_at);
  const pub = existing ?? (await stack.admin.post("/admin/public-models", { name }));
  await stack.admin.put(`/admin/provider-keys/${accountId}/channel-models/${pub.id}`, { priority: 0, weight: 1 });
  return pub.id as string;
}

/** The preview's row of the model `name`, which keeps its identity however its cells read. */
function previewRow(page: Page, name: string): Locator {
  return page.getByRole("table", { name: "同步预览" }).locator(`tr[data-model="${name}"]`);
}

test("离线上传数据集后，价格库按名称或供应商搜索，显示标价与价格页面", async ({ page }) => {
  await login(page);
  await uploadDataset(page);
  await section(page, "价格库");

  await expect(page.getByLabel("价格库状态")).toContainText("文件上传");
  await expect(page.getByLabel("价格库状态")).toContainText(REVISION);

  const commandA = snapshot("cohere", "command-a");
  await page.getByLabel("搜索模型").fill("command-a");
  const row = page
    .getByRole("table", { name: "价格库条目" })
    .getByRole("row")
    .filter({ has: page.getByRole("cell", { name: "cohere", exact: true }) })
    .filter({ has: page.getByText("command-a", { exact: true }) });
  await expect(row).toContainText(usd(commandA.input));
  await expect(row).toContainText(usd(commandA.output));
  await expect(row.getByRole("link", { name: "价格页面" })).toHaveAttribute("href", commandA.page);

  // By provider: the provider's own entries are listed, and nothing listed
  // fails to name what was searched for.
  await page.getByLabel("搜索模型").fill("cohere");
  const rows = page.getByRole("table", { name: "价格库条目" }).getByRole("rowgroup").last().getByRole("row");
  await expect(rows.filter({ hasNotText: /cohere/i })).toHaveCount(0);
  await expect(rows.filter({ has: page.getByRole("cell", { name: "cohere", exact: true }) }).first()).toBeVisible();
});

test("预览同步后一次确认：基准价写入，分组售价按换算率出现", async ({ page }) => {
  const suffix = randomUUID().slice(0, 6);
  const group = await stack.admin.createGroup(`grp-lib-${suffix}`, `价格库分组 ${suffix}`, "2");
  // One provider prices `command-a`: matched, whoever serves it.
  const cohere = await account("cohere", [group.name as string]);
  await publicModel("command-a", cohere);
  // Two providers price `gpt-4o`, and this model's own provider is neither.
  const relay = await account(`relay-${suffix}`, [group.name as string]);
  await publicModel("gpt-4o", relay);
  // Nothing in the dataset is called this.
  const nothing = `lib-nomatch-${suffix}`;
  await publicModel(nothing, relay);

  await login(page);
  await uploadDataset(page);
  await section(page, "价格库");
  const sync = page.getByRole("region", { name: "同步基准价" });
  await expect(sync.getByRole("table", { name: "同步预览" })).toBeVisible();

  const commandA = snapshot("cohere", "command-a");
  const created = previewRow(page, "command-a");
  await expect(created).toContainText("将新增");
  await expect(created).toContainText("cohere/command-a");
  await expect(created).toContainText(`输入 $${commandA.input} · 输出 $${commandA.output}`);
  // Ambiguous: every candidate is offered as a price source, and none is
  // chosen for the operator.
  const ambiguous = previewRow(page, "gpt-4o");
  await expect(ambiguous).toContainText("多个匹配");
  const sources = ambiguous.getByLabel("选择价格来源 gpt-4o").locator("option");
  await expect(sources.filter({ hasText: "openai · gpt-4o" })).toHaveCount(1);
  await expect(sources.filter({ hasText: "github-copilot · gpt-4o" })).toHaveCount(1);
  await expect(sources.filter({ hasText: "openai · gpt-4o" })).toContainText("$2.5/10 每百万");
  await expect(previewRow(page, nothing)).toContainText("无匹配");

  // The one confirmation names how many models it writes: every row the
  // preview marks 将新增 or 将更新, and no other.
  const writes = await page
    .getByRole("table", { name: "同步预览" })
    .locator('tr[data-state="create"], tr[data-state="update"]')
    .count();
  expect(writes).toBeGreaterThan(0);
  const confirm = sync.getByRole("button", { name: `同步 ${writes} 个模型`, exact: true });
  await expect(confirm).toBeDisabled();
  await sync.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await confirm.click();
  await expect(page.locator(".toast").filter({ hasText: `已同步 ${writes} 个模型` }).first()).toBeVisible();

  // Read again: written, so nothing left to do; the others are as they were.
  await expect(previewRow(page, "command-a")).toContainText("一致");
  await expect(previewRow(page, "gpt-4o")).toContainText("多个匹配");
  await expect(previewRow(page, nothing)).toContainText("无匹配");

  // 基准价: priced from the third party, by this operator.
  await section(page, "基准价");
  const baseline = page
    .getByRole("table", { name: "基准价" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: "command-a", exact: true }) });
  await expect(baseline).toHaveAttribute("data-state", "priced");
  await expect(baseline).toContainText(usd(commandA.input));
  await expect(baseline).toContainText(usd(commandA.output));
  await expect(baseline).toContainText("第三方数据");
  await expect(baseline).toContainText("价格库同步");
  await expect(baseline).toContainText(stack.operator.email);
  // The ambiguous one was never written.
  await expect(
    page.getByRole("table", { name: "基准价" }).getByRole("row").filter({ has: page.getByRole("button", { name: "gpt-4o", exact: true }) }),
  ).toHaveAttribute("data-state", "unpriced");

  // The group's ¥ column: the synced baseline times the group's rate of 2.
  await section(page, "分组");
  const display = group.display_name as string;
  await page
    .getByRole("table", { name: "分组" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: display, exact: true }) })
    .getByRole("button", { name: display, exact: true })
    .click();
  await expect(page.getByRole("region", { name: `分组 ${display}` })).toBeVisible();
  const priced = page
    .getByRole("table", { name: "分组换算率" })
    .getByRole("row")
    .filter({ has: page.getByRole("cell", { name: "command-a", exact: true }) });
  await expect(priced.getByRole("cell").nth(5)).toHaveText(cny(commandA.input, "2"));
  await expect(priced.getByRole("cell").nth(6)).toHaveText(cny(commandA.output, "2"));
});

test("在基准价详情中为单个模型从价格库同步，需先勾选核对声明", async ({ page }) => {
  // Two providers price `command-r`; this model is served by one of them,
  // which is the one its baseline is read from.
  const cohere = await account("cohere", ["shared"]);
  await publicModel("command-r", cohere);
  const commandR = snapshot("cohere", "command-r");

  await login(page);
  await uploadDataset(page);
  await section(page, "基准价");
  await page
    .getByRole("table", { name: "基准价" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: "command-r", exact: true }) })
    .getByRole("button", { name: "command-r", exact: true })
    .click();
  const panel = page.getByRole("dialog", { name: "command-r 的基准价" });
  const one = panel.getByRole("region", { name: "从价格库同步" });
  await expect(one).toContainText("将新增");
  await expect(one).toContainText("cohere/command-r");
  await expect(one).toContainText(`输入 $${commandR.input} · 输出 $${commandR.output}`);

  const button = one.getByRole("button", { name: "从价格库同步", exact: true });
  await expect(button).toBeDisabled();
  await one.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await button.click();
  await expect(page.locator(".toast").filter({ hasText: "已从价格库同步" }).first()).toBeVisible();

  const now = panel.getByRole("region", { name: "现行基准价" });
  await expect(now).toContainText(usd(commandR.input));
  await expect(now).toContainText(usd(commandR.output));
  await expect(now).toContainText("价格库同步");
  await expect(one).toContainText("一致");
});

test("多个匹配的模型选好价格来源后同步：写入基准价、记住选择，刷新后仍显示已选", async ({ page }) => {
  const suffix = randomUUID().slice(0, 6);
  const group = await stack.admin.createGroup(`grp-choose-${suffix}`, `选择来源分组 ${suffix}`, "2");
  // Two providers price `claude-2` at different prices, and neither is the
  // provider the account is filed under.
  const relay = await account(`relay-${suffix}`, [group.name as string]);
  await publicModel("claude-2", relay);

  await login(page);
  await uploadDataset(page);
  await section(page, "价格库");
  const sync = page.getByRole("region", { name: "同步基准价" });
  await expect(sync.getByRole("table", { name: "同步预览" })).toBeVisible();

  const row = previewRow(page, "claude-2");
  await expect(row).toContainText("多个匹配");
  const pick = row.getByLabel("选择价格来源 claude-2");
  // Nothing is chosen for the operator.
  await expect(pick).toHaveValue("");
  // Every candidate is offered with its own prices.
  const anthropic = snapshot("anthropic", "claude-2");
  const openrouter = snapshot("openrouter", "claude-2");
  await expect(pick.locator("option", { hasText: "anthropic · claude-2" })).toContainText(
    `$${anthropic.input}/${anthropic.output} 每百万`,
  );
  await expect(pick.locator("option", { hasText: "openrouter · claude-2" })).toContainText(
    `$${openrouter.input}/${openrouter.output} 每百万`,
  );
  // The placeholder, and one line per candidate.
  await expect(pick.locator("option")).toHaveCount(3);
  // Nothing is written for it until one is chosen.
  const before = await page
    .getByRole("table", { name: "同步预览" })
    .locator('tr[data-shown="create"], tr[data-shown="update"]')
    .count();

  await pick.selectOption({ value: "anthropic/claude-2" });
  // The row now shows what syncing from the chosen entry would do.
  await expect(row).toHaveAttribute("data-shown", "create");
  await expect(row).toContainText("将新增");
  await expect(row).toContainText(`输入 $${anthropic.input} · 输出 $${anthropic.output}`);

  const after = await page
    .getByRole("table", { name: "同步预览" })
    .locator('tr[data-shown="create"], tr[data-shown="update"]')
    .count();
  expect(after).toBe(before + 1);
  const all = sync.getByRole("button", { name: `同步 ${after} 个模型`, exact: true });
  await sync.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await all.click();
  await expect(page.locator(".toast").filter({ hasText: `已同步 ${after} 个模型` }).first()).toBeVisible();

  // Read again: the choice is recorded and the row shows it, with nothing
  // left to do for it. The row's state is read first, so what follows is
  // judged against the preview the sync left, not the one before it.
  await expect(row).toHaveAttribute("data-shown", "same");
  await expect(row).toContainText("已选：anthropic/claude-2");
  await expect(row).toContainText("一致");
  await expect(row.getByRole("button", { name: "重新选择" })).toBeVisible();

  // 基准价: one baseline, written from the library under this operator's
  // attestation, at the prices the chosen entry showed. The choice itself is
  // pinned above and by the preview after the reload.
  await section(page, "基准价");
  const baseline = page
    .getByRole("table", { name: "基准价" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: "claude-2", exact: true }) });
  await expect(baseline).toHaveAttribute("data-state", "priced");
  await expect(baseline).toContainText(usd(anthropic.input));
  await expect(baseline).toContainText(usd(anthropic.output));
  await expect(baseline).toContainText("价格库同步");
  await expect(baseline).toContainText(REVISION);
  await expect(baseline).toContainText(stack.operator.email);

  // A reload keeps it: the choice is the control plane's, not the page's.
  await page.reload();
  await expect(page.locator("[data-tab]").first()).toBeVisible();
  await page.locator('[data-tab="pricing"]').click();
  await section(page, "价格库");
  const again = previewRow(page, "claude-2");
  await expect(again).toHaveAttribute("data-shown", "same");
  await expect(again).toContainText("已选：anthropic/claude-2");
  // 重新选择 forgets it, and the model needs a choice again.
  await again.getByRole("button", { name: "重新选择" }).click();
  await expect(again).toContainText("多个匹配");
  await expect(again.getByLabel("选择价格来源 claude-2")).toHaveValue("");
  await expect(again).not.toContainText("已选");
});

test("勾选核对后改选价格来源：核对声明随之取消，同步不可用，单个模型的同步也一样", async ({ page }) => {
  const suffix = randomUUID().slice(0, 6);
  const group = await stack.admin.createGroup(`grp-reattest-${suffix}`, `重新核对分组 ${suffix}`, "2");
  // Two providers price each of these, and neither is the account's own.
  const relay = await account(`relay-${suffix}`, [group.name as string]);
  const ids = [await publicModel("phi-4-reasoning-plus", relay), await publicModel("phi-4-multimodal-instruct", relay)];

  await login(page);
  await uploadDataset(page);
  await section(page, "价格库");
  const sync = page.getByRole("region", { name: "同步基准价" });
  const row = previewRow(page, "phi-4-reasoning-plus");
  await expect(row).toContainText("多个匹配");
  const pick = row.getByLabel("选择价格来源 phi-4-reasoning-plus");
  await pick.selectOption({ value: "azure/phi-4-reasoning-plus" });
  const azure = snapshot("azure", "phi-4-reasoning-plus");
  await expect(row).toContainText(`输入 $${azure.input} · 输出 $${azure.output}`);
  const attest = sync.getByRole("checkbox", { name: "已对照官方价格页核对" });
  const all = sync.getByRole("button", { name: /^同步 \d+ 个模型$/ });
  await attest.check();
  await expect(all).toBeEnabled();

  // The tick was given for the source then shown; another one is not checked.
  await pick.selectOption({ value: "openrouter/phi-4-reasoning-plus" });
  const openrouter = snapshot("openrouter", "phi-4-reasoning-plus");
  await expect(row).toContainText(`输入 $${openrouter.input} · 输出 $${openrouter.output}`);
  await expect(attest).not.toBeChecked();
  await expect(all).toBeDisabled();

  // The same in one model's own 从价格库同步.
  await section(page, "基准价");
  await page
    .getByRole("table", { name: "基准价" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: "phi-4-multimodal-instruct", exact: true }) })
    .getByRole("button", { name: "phi-4-multimodal-instruct", exact: true })
    .click();
  const one = page.getByRole("dialog", { name: "phi-4-multimodal-instruct 的基准价" }).getByRole("region", { name: "从价格库同步" });
  const pickOne = one.getByLabel("选择价格来源 phi-4-multimodal-instruct");
  await pickOne.selectOption({ value: "azure/phi-4-multimodal-instruct" });
  const attestOne = one.getByRole("checkbox", { name: "已对照官方价格页核对" });
  const button = one.getByRole("button", { name: "从价格库同步", exact: true });
  await attestOne.check();
  await expect(button).toBeEnabled();
  await pickOne.selectOption({ value: "openrouter/phi-4-multimodal-instruct" });
  const other = snapshot("openrouter", "phi-4-multimodal-instruct");
  await expect(one).toContainText(`输入 $${other.input} · 输出 $${other.output}`);
  await expect(attestOne).not.toBeChecked();
  await expect(button).toBeDisabled();

  // Nothing was written for either.
  const stored = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  for (const id of ids) expect((stored.items as Json[]).find((i) => i.model_id === id).current).toBeNull();
});

test("含长上下文价的条目在价格库与同步预览中标出，基准价缺少这一段时预览给出提示", async ({ page }) => {
  // One provider prices `claude-sonnet-4-5`: a base rate and a band above
  // 200K input tokens.
  const anthropic = await account("anthropic", ["shared"]);
  await publicModel("claude-sonnet-4-5", anthropic);

  await login(page);
  await uploadDataset(page);
  await section(page, "价格库");

  // The browse list marks the entry, with the band's own rates.
  await page.getByLabel("搜索模型").fill("claude-sonnet-4-5");
  const listed = page
    .getByRole("table", { name: "价格库条目" })
    .getByRole("row")
    .filter({ has: page.getByRole("cell", { name: "anthropic", exact: true }) })
    .filter({ has: page.getByText("claude-sonnet-4-5", { exact: true }) });
  await expect(listed).toContainText("含长上下文价");
  await expect(listed).toContainText(">200K 输入：$6 / $22.5 每百万");

  // The sync preview marks it too, and says the baseline lacks the band.
  const row = previewRow(page, "claude-sonnet-4-5");
  await expect(row).toContainText("将新增");
  await expect(row).toContainText("含长上下文价");
  await expect(row).toContainText(">200K 输入：$6 / $22.5 每百万");
  await expect(row).toContainText("当前基准价缺少长上下文档，同步后将补上");

  // Once synced, the baseline carries the band, and the warning is gone.
  const sync = page.getByRole("region", { name: "同步基准价" });
  const writes = await page
    .getByRole("table", { name: "同步预览" })
    .locator('tr[data-shown="create"], tr[data-shown="update"]')
    .count();
  await sync.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await sync.getByRole("button", { name: `同步 ${writes} 个模型`, exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: `已同步 ${writes} 个模型` }).first()).toBeVisible();
  const done = previewRow(page, "claude-sonnet-4-5");
  await expect(done).toContainText("一致");
  await expect(done).toContainText("含长上下文价");
  await expect(done).not.toContainText("当前基准价缺少长上下文档");
});

test("多个匹配的模型选好来源后价格库被替换：预览按新价格显示所选来源及其未计入项，再次确认写入新价格", async ({ page }) => {
  const suffix = randomUUID().slice(0, 6);
  const group = await stack.admin.createGroup(`grp-moved-${suffix}`, `换价分组 ${suffix}`, "2");
  // Two providers price `command-r-plus`, and neither is the account's own.
  const relay = await account(`relay-${suffix}`, [group.name as string]);
  const modelId = await publicModel("command-r-plus", relay);
  const cohere = snapshot("cohere", "command-r-plus");

  await login(page);
  await uploadDataset(page);
  await section(page, "价格库");
  const sync = page.getByRole("region", { name: "同步基准价" });
  const row = previewRow(page, "command-r-plus");
  await expect(row).toContainText("多个匹配");
  await row.getByLabel("选择价格来源 command-r-plus").selectOption({ value: "cohere/command-r-plus" });
  await expect(row).toContainText(`输入 $${cohere.input} · 输出 $${cohere.output}`);

  // Meanwhile, from another tab, the library is replaced. In the new file the
  // chosen entry's input price moved, it prices web searches ($10 per
  // 1,000), and it carries a price the gateway does not meter.
  const original = readFileSync(SNAPSHOT, "utf8");
  const before = `{"equals":"command-r-plus-08-2024"}]},"prices":{"input_mtok":${cohere.input},"output_mtok":${cohere.output}}}`;
  expect(original.split(before)).toHaveLength(2);
  const moved = original.replace(
    before,
    `{"equals":"command-r-plus-08-2024"}]},"prices":{"input_mtok":2.75,"output_mtok":${cohere.output},"web_searches_kcount":10,"storage_searches_kcount":2.5}}`,
  );
  const other = await page.context().newPage();
  await other.goto(stack.previewUrl);
  await expect(other.locator("[data-tab]").first()).toBeVisible();
  await other.locator('[data-tab="pricing"]').click();
  await section(other, "导入审核");
  await other.getByLabel("数据集文件").setInputFiles({ name: "moved.json", mimeType: "application/json", buffer: Buffer.from(moved) });
  await other.getByLabel("数据集版本").fill(`${REVISION}-moved`);
  await other.getByRole("button", { name: "上传", exact: true }).click();
  await expect(other.getByRole("table", { name: "提议" })).toBeVisible();
  await other.close();

  // The confirmation was given against the old preview: refused, and the
  // preview is read again.
  await sync.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await sync.getByRole("button", { name: /^同步 \d+ 个模型$/ }).click();
  await expect(sync.getByRole("alert")).toContainText("同步失败");
  // The source stays chosen, shown as the library now prices it: the new
  // input, the search per thousand, what it leaves off, and its page.
  await expect(row).toHaveAttribute("data-shown", "create");
  await expect(row).toContainText("输入 $2.75");
  await expect(row).toContainText("联网搜索 $10 / 千次");
  await expect(row).not.toContainText("$10000");
  await expect(row).toContainText("未计入：文件搜索存储（数据集字段 storage_searches_kcount，网关不计量）");
  await expect(row.getByRole("link", { name: "价格页面" })).toHaveAttribute("href", cohere.page);

  // Confirmed again, it writes what is shown.
  await expect(sync.getByRole("checkbox", { name: "已对照官方价格页核对" })).not.toBeChecked();
  const writes = await page
    .getByRole("table", { name: "同步预览" })
    .locator('tr[data-shown="create"], tr[data-shown="update"]')
    .count();
  await sync.getByRole("checkbox", { name: "已对照官方价格页核对" }).check();
  await sync.getByRole("button", { name: `同步 ${writes} 个模型`, exact: true }).click();
  await expect(page.locator(".toast").filter({ hasText: `已同步 ${writes} 个模型` }).first()).toBeVisible();
  await expect(row).toHaveAttribute("data-shown", "same");
  const stored = (await stack.admin.get("/admin/pricing/baselines")) as Json;
  const listed = stored.items.find((i: Json) => i.model_id === modelId);
  // $10 per 1,000 searches is 10000 per million, the unit every rate is stored in.
  expect(listed.current.card).toEqual({ default: { base: { input: "2.75", output: cohere.output, web_search: "10000" } } });

  // 基准价's own view of the model reads the search per thousand too.
  await section(page, "基准价");
  await page
    .getByRole("table", { name: "基准价" })
    .getByRole("row")
    .filter({ has: page.getByRole("button", { name: "command-r-plus", exact: true }) })
    .getByRole("button", { name: "command-r-plus", exact: true })
    .click();
  const panel = page.getByRole("dialog", { name: "command-r-plus 的基准价" });
  await expect(panel.getByRole("region", { name: "现行基准价" })).toContainText("联网搜索 $10 / 千次");
  const one = panel.getByRole("region", { name: "从价格库同步" });
  await expect(one).toContainText("一致");
  await expect(one).toContainText("联网搜索 $10 / 千次");
  await expect(panel).not.toContainText("$10000");
});
